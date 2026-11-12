//! The root CA backup the operator holds (F04 R3, #709): where `relish init`
//! writes it, `relish ca backup`, which re-seals it, `relish ca verify`, and
//! `relish ca rotate`, which signs a new intermediate with it (F04 R4).
//!
//! `backup` and `verify` run offline and read only the backup itself. No
//! node keeps the root key, so there's nothing on a node to read it from.

use std::fs;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use age::secrecy::SecretString;

use super::RelishError;
use super::output::{OutputFormat, print_output};
use crate::sesame::ca_rotation::{PreparedRotation, SignedIntermediate};
use crate::sesame::root_backup::{self, BackupOpener, BackupSeal, RootBackup, SealKind};
use crate::sesame::types::CaRole;

/// Where a passphrase comes from.
#[derive(Debug, Clone)]
pub enum PassphraseSource {
    /// The first line of a file the operator owns.
    File(PathBuf),
    /// Typed at the terminal, with echo off.
    Prompt,
}

/// What `relish ca backup` seals to.
#[derive(Debug, Clone)]
pub enum BackupTarget {
    /// A passphrase (the default).
    Passphrase(PassphraseSource),
    /// An operator's age public key.
    Recipient(String),
}

/// Where `relish init` writes the root CA backup, and what it seals it to.
#[derive(Debug, Clone)]
pub struct InitRootBackup {
    /// The backup file, which mustn't exist yet.
    pub path: PathBuf,
    /// A passphrase or the operator's age recipient.
    pub target: BackupTarget,
}

impl InitRootBackup {
    /// Turn `relish init`'s flags into a destination (#709). With no
    /// `--recipient` or `--passphrase-file`, a terminal is asked for a
    /// passphrase; with no `--root-backup`, it's asked for a path. Without a
    /// terminal, both are refused with the flags to pass: init never writes
    /// the root key to a default file on the node.
    pub fn from_flags(
        path: Option<PathBuf>,
        recipient: Option<String>,
        passphrase_file: Option<PathBuf>,
        terminal: bool,
        ask_path: impl FnOnce() -> std::io::Result<String>,
    ) -> Result<Self, RelishError> {
        let target = match (recipient, passphrase_file) {
            (Some(recipient), _) => BackupTarget::Recipient(recipient),
            (None, Some(file)) => BackupTarget::Passphrase(PassphraseSource::File(file)),
            (None, None) if terminal => BackupTarget::Passphrase(PassphraseSource::Prompt),
            (None, None) => return Err(no_destination()),
        };
        let path = match path {
            Some(path) => path,
            None if terminal => {
                let answer = ask_path()?;
                let answer = answer.trim();
                if answer.is_empty() {
                    return Err(no_destination());
                }
                PathBuf::from(answer)
            }
            None => return Err(no_destination()),
        };
        Ok(Self { path, target })
    }

    /// The seal, asking for the passphrase (twice) if it comes from the
    /// terminal. Call it before generating anything, so a mistyped
    /// passphrase costs nothing.
    pub fn seal(&self) -> Result<BackupSeal, RelishError> {
        target_seal(&self.target)
    }
}

fn no_destination() -> RelishError {
    RelishError::InvalidFlag {
        flag: "root-backup".to_string(),
        reason: "the root CA key goes only into a backup you hold: pass --root-backup PATH \
                 with --passphrase-file FILE or --recipient age1..., or run `relish init` at \
                 a terminal to be asked"
            .to_string(),
    }
}

fn target_seal(target: &BackupTarget) -> Result<BackupSeal, RelishError> {
    Ok(match target {
        BackupTarget::Passphrase(source) => BackupSeal::Passphrase(new_passphrase(source)?),
        BackupTarget::Recipient(public_key) => BackupSeal::Recipient(public_key.clone()),
    })
}

/// How to open an existing backup: its passphrase, or an age identity file.
#[derive(Debug, Clone)]
pub struct BackupKey {
    /// Where the passphrase comes from, for a passphrase-sealed backup.
    pub passphrase: PassphraseSource,
    /// The identity file, for a backup sealed to a recipient.
    pub identity: Option<PathBuf>,
}

/// `relish ca backup`: re-seal the backup in `from` to a new passphrase or
/// recipient, into `out`. It never reads anything from a node.
pub fn ca_backup(
    from: &Path,
    key: &BackupKey,
    out: &Path,
    target: &BackupTarget,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let backup = open_backup(from, key)?;
    let seal = target_seal(target)?;
    let backup = rewrap(&backup, out, &seal, SystemTime::now())?;
    print_output(
        &BackupSummary::new(BackupCheck::Written, out, &backup),
        output,
    )
}

/// A root CA backup `relish ca backup` wrote or `ca verify` opened: what it
/// holds, never the key itself.
#[derive(Debug, serde::Serialize)]
pub struct BackupSummary<'a> {
    pub result: BackupCheck,
    pub path: &'a Path,
    pub cluster: &'a str,
    pub trust_domain: &'a str,
    /// The root certificate's fingerprint, to check the backup against.
    pub fingerprint: &'a str,
    /// When the root certificate expires, Unix seconds.
    pub expires_at: i64,
}

/// What became of the backup in a [`BackupSummary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupCheck {
    /// `ca backup` wrote it.
    Written,
    /// `ca verify` opened it and it matches the fingerprint.
    Ok,
}

impl<'a> BackupSummary<'a> {
    fn new(result: BackupCheck, path: &'a Path, backup: &'a RootBackup) -> Self {
        Self {
            result,
            path,
            cluster: &backup.cluster,
            trust_domain: &backup.trust_domain,
            fingerprint: &backup.fingerprint,
            expires_at: backup.not_after,
        }
    }
}

impl std::fmt::Display for BackupSummary<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.result {
            BackupCheck::Written => {
                writeln!(f, "wrote the root CA backup to {}", self.path.display())?
            }
            BackupCheck::Ok => writeln!(f, "{}: OK", self.path.display())?,
        }
        writeln!(f, "  cluster:      {}", self.cluster)?;
        writeln!(f, "  trust domain: {}", self.trust_domain)?;
        writeln!(f, "  fingerprint:  {}", self.fingerprint)?;
        writeln!(f, "  expires:      {}", format_expiry(self.expires_at))?;
        if self.result == BackupCheck::Written {
            writeln!(f)?;
            writeln!(
                f,
                "Keep it off the cluster, with the passphrase or age identity stored apart from it."
            )?;
            writeln!(
                f,
                "Check it at any time with `relish ca verify {} --fingerprint {}`.",
                self.path.display(),
                self.fingerprint
            )?;
        }
        Ok(())
    }
}

/// Check `backup` against its own fingerprint, then write it to `out` under
/// `seal`, checking what landed. Split from [`ca_backup`] so tests can pass
/// the seal and the clock.
fn rewrap(
    backup: &RootBackup,
    out: &Path,
    seal: &BackupSeal,
    now: SystemTime,
) -> Result<RootBackup, RelishError> {
    backup.verify(&backup.fingerprint, now)?;
    if out.exists() {
        return Err(RelishError::FileExists {
            path: out.display().to_string(),
        });
    }
    root_backup::write_backup(
        backup,
        &root_backup::BackupDestination::Sealed { path: out, seal },
        now,
    )?;
    Ok(backup.clone())
}

/// `relish ca verify`: open a backup and check it offline.
pub fn ca_verify(
    file: &Path,
    fingerprint: &str,
    passphrase: &PassphraseSource,
    identity: Option<&Path>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let backup = open_and_verify(file, fingerprint, passphrase, identity, SystemTime::now())?;
    print_output(&BackupSummary::new(BackupCheck::Ok, file, &backup), output)
}

fn open_and_verify(
    file: &Path,
    fingerprint: &str,
    passphrase: &PassphraseSource,
    identity: Option<&Path>,
    now: SystemTime,
) -> Result<RootBackup, RelishError> {
    let backup = open_backup(
        file,
        &BackupKey {
            passphrase: passphrase.clone(),
            identity: identity.map(Path::to_path_buf),
        },
    )?;
    backup.verify(fingerprint, now)?;
    Ok(backup)
}

/// Open a backup without checking it against a cluster.
fn open_backup(file: &Path, key: &BackupKey) -> Result<RootBackup, RelishError> {
    let sealed = fs::read(file)?;
    let opener = match (root_backup::seal_kind(&sealed)?, &key.identity) {
        (SealKind::Recipient, Some(path)) => BackupOpener::Identity(read_age_identity(path)?),
        (SealKind::Recipient, None) => {
            return Err(root_backup::RootBackupError::NeedsIdentity.into());
        }
        (SealKind::Passphrase, _) => {
            BackupOpener::Passphrase(existing_passphrase(&key.passphrase)?)
        }
    };
    Ok(RootBackup::open(&sealed, &opener)?)
}

/// `relish ca rotate --role ROLE --root-backup FILE`: rotate an intermediate
/// CA (F04 R4).
///
/// The council makes the new key and sends a CSR. The backup is opened here
/// and checked against the root the cluster trusts, the CSR is signed with
/// the root key, and only the certificate goes back. The root key never
/// leaves this machine.
pub async fn ca_rotate(
    role: CaRole,
    root_backup: &Path,
    passphrase: &PassphraseSource,
    identity: Option<&Path>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = super::client::BunClient::default_local();
    let prepared = client.ca_rotation_prepare(role).await?;
    let backup = open_and_verify(
        root_backup,
        &prepared.root_fingerprint,
        passphrase,
        identity,
        SystemTime::now(),
    )?;
    let signed = sign_prepared_rotation(&backup, &prepared)?;
    let message = client.ca_rotation_begin(&signed).await?;
    print_output(
        &CaRotation {
            role: role_flag(role),
            stage: RotationStage::Begun,
            message,
        },
        output,
    )
}

/// `relish ca rotate --role ROLE --finalize`: retire the old CA, refused
/// while anything could still depend on it.
pub async fn ca_rotate_finalize(role: CaRole, output: OutputFormat) -> Result<(), RelishError> {
    let client = super::client::BunClient::default_local();
    let message = client.ca_rotation_finalize(role).await?;
    print_output(
        &CaRotation {
            role: role_flag(role),
            stage: RotationStage::Finalised,
            message,
        },
        output,
    )
}

/// What `relish ca rotate` did to an intermediate CA.
#[derive(Debug, serde::Serialize)]
pub struct CaRotation {
    /// The role as `--role` spells it.
    pub role: &'static str,
    pub stage: RotationStage,
    /// What the council said.
    pub message: String,
}

/// How far a [`CaRotation`] has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationStage {
    /// The new CA is signed and rolling out.
    Begun,
    /// The old CA is retired.
    Finalised,
}

impl std::fmt::Display for CaRotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.message)?;
        if self.stage == RotationStage::Finalised {
            return Ok(());
        }
        writeln!(f)?;
        match self.role {
            "node" => writeln!(
                f,
                "Every node now acknowledges the new trust set, then renews onto the new CA one at \
                 a time."
            )?,
            "workload" => writeln!(
                f,
                "Workload certificates move to the new CA as they renew, within the hour."
            )?,
            _ => writeln!(
                f,
                "Ingress certificates now come from the new CA; each node re-mints its routes' \
                 certificates as it picks the new CA up."
            )?,
        }
        writeln!(
            f,
            "Retire the old CA with `relish ca rotate --role {} --finalize`; it says what is still \
             waiting if it's too early.",
            self.role
        )
    }
}

/// `relish identity rotate-signing-key`: sign workload JWTs with a new key,
/// and say which key keeps verifying until when (#709).
pub async fn identity_rotate_signing_key() -> Result<(), RelishError> {
    let client = super::client::BunClient::default_local();
    let rotated = client.identity_rotate_signing_key().await?;
    println!("{}", rotation_message(&rotated));
    Ok(())
}

/// What `relish identity rotate-signing-key` prints for `rotated`.
fn rotation_message(rotated: &crate::sesame::oidc::RotatedSigningKey) -> String {
    format!(
        "Workload identity tokens are now signed with key {}.\n\
         Key {} keeps verifying until {} UTC; workloads pick up new tokens on their next refresh.",
        rotated.key_id,
        rotated.retiring_key_id,
        super::audit_cmd::format_time(rotated.retire_after)
    )
}

/// Sign the council's CSR with the backup's root. Split from [`ca_rotate`]
/// so tests can run it without a cluster.
fn sign_prepared_rotation(
    backup: &RootBackup,
    prepared: &PreparedRotation,
) -> Result<SignedIntermediate, RelishError> {
    use base64::Engine as _;
    let csr_der = base64::engine::general_purpose::STANDARD
        .decode(&prepared.csr_b64)
        .map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("the council sent an unreadable CSR: {e}"),
        })?;
    let certificate_der = crate::sesame::ca::sign_intermediate_csr(
        &csr_der,
        prepared.role,
        &backup.cluster,
        crate::sesame::types::SerialNumber(prepared.serial),
        &backup.private_key_der()?,
        &backup.certificate_der()?,
    )?;
    Ok(SignedIntermediate {
        role: prepared.role,
        certificate_b64: base64::engine::general_purpose::STANDARD.encode(certificate_der),
    })
}

/// How `--role` spells a role.
fn role_flag(role: CaRole) -> &'static str {
    match role {
        CaRole::Node => "node",
        CaRole::Workload => "workload",
        CaRole::Ingress => "ingress",
        CaRole::Root => "root",
    }
}

fn format_expiry(unix_seconds: i64) -> String {
    let Ok(seconds) = u64::try_from(unix_seconds) else {
        return format!("{unix_seconds} (unix seconds)");
    };
    let when = UNIX_EPOCH + std::time::Duration::from_secs(seconds);
    let date = time::OffsetDateTime::from(when).date();
    format!("{date} ({unix_seconds} unix seconds)")
}

/// Read an age identity file (`AGE-SECRET-KEY-1...`, as `age-keygen` writes).
fn read_age_identity(path: &Path) -> Result<age::x25519::Identity, RelishError> {
    let contents = fs::read_to_string(path)?;
    contents
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("AGE-SECRET-KEY-"))
        .and_then(|line| line.parse().ok())
        .ok_or_else(|| RelishError::InvalidFlag {
            flag: "identity".to_string(),
            reason: format!("no AGE-SECRET-KEY-1... line in {}", path.display()),
        })
}

/// The passphrase for a new backup. At the terminal it's asked twice.
fn new_passphrase(source: &PassphraseSource) -> Result<SecretString, RelishError> {
    match source {
        PassphraseSource::File(path) => read_passphrase_file(path),
        PassphraseSource::Prompt => {
            let first = prompt_secret("Passphrase for the root CA backup: ")?;
            let second = prompt_secret("Again: ")?;
            use age::secrecy::ExposeSecret as _;
            if first.expose_secret() != second.expose_secret() {
                return Err(RelishError::InvalidFlag {
                    flag: "passphrase".to_string(),
                    reason: "the two passphrases differ".to_string(),
                });
            }
            Ok(first)
        }
    }
}

/// The passphrase that opens an existing backup.
fn existing_passphrase(source: &PassphraseSource) -> Result<SecretString, RelishError> {
    match source {
        PassphraseSource::File(path) => read_passphrase_file(path),
        PassphraseSource::Prompt => prompt_secret("Passphrase for the root CA backup: "),
    }
}

fn read_passphrase_file(path: &Path) -> Result<SecretString, RelishError> {
    let file = fs::File::open(path)?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line)?;
    let line = line.trim_end_matches(['\n', '\r']).to_string();
    Ok(SecretString::from(line))
}

/// Ask for a secret on the controlling terminal with echo off.
#[cfg(unix)]
fn prompt_secret(prompt: &str) -> Result<SecretString, RelishError> {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};

    let no_terminal = |e: std::io::Error| {
        RelishError::Io(std::io::Error::new(
            e.kind(),
            format!("no terminal to ask for the passphrase on ({e}): pass --passphrase-file"),
        ))
    };
    let tty = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(no_terminal)?;
    let saved = tcgetattr(&tty).map_err(|e| no_terminal(e.into()))?;
    let mut silent = saved.clone();
    silent.local_flags.remove(LocalFlags::ECHO);
    silent.local_flags.insert(LocalFlags::ECHONL);
    tcsetattr(&tty, SetArg::TCSANOW, &silent).map_err(|e| no_terminal(e.into()))?;

    let read = ask_on(&tty, prompt);
    // Restore echo whether or not the read worked.
    tcsetattr(&tty, SetArg::TCSANOW, &saved).map_err(|e| no_terminal(e.into()))?;

    let line = read?;
    Ok(SecretString::from(
        line.trim_end_matches(['\n', '\r']).to_string(),
    ))
}

/// Write the prompt to the terminal and read one line back. `Read` and
/// `Write` are implemented for `&File`, so a shared borrow is enough.
#[cfg(unix)]
fn ask_on(tty: &fs::File, prompt: &str) -> std::io::Result<String> {
    let mut writer = tty;
    writer.write_all(prompt.as_bytes())?;
    writer.flush()?;
    let mut line = String::new();
    std::io::BufReader::new(tty).read_line(&mut line)?;
    Ok(line)
}

#[cfg(not(unix))]
fn prompt_secret(_prompt: &str) -> Result<SecretString, RelishError> {
    Err(RelishError::InvalidFlag {
        flag: "passphrase-file".to_string(),
        reason: "prompting for a passphrase needs a Unix terminal".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rotation_says_which_key_signs_and_until_when_the_old_one_verifies() {
        let message = rotation_message(&crate::sesame::oidc::RotatedSigningKey {
            key_id: "new0123456789abc".into(),
            retiring_key_id: "old0123456789abc".into(),
            retire_after: 1_793_966_400,
        });
        assert_eq!(
            message,
            "Workload identity tokens are now signed with key new0123456789abc.\n\
             Key old0123456789abc keeps verifying until 2026-11-06 12:00:00 UTC; \
             workloads pick up new tokens on their next refresh."
        );
    }
    use crate::sesame::root_backup::RootBackupError;

    const PASSPHRASE: &str = "correct horse battery staple";

    /// A cluster `relish init` made, with the backup it wrote off the node.
    struct Initialised {
        node: tempfile::TempDir,
        operator: tempfile::TempDir,
        backup: PathBuf,
        passphrase: PathBuf,
        fingerprint: String,
    }

    /// Run `relish init` into a temporary directory, writing the root backup
    /// to another, sealed to [`PASSPHRASE`].
    fn init_cluster(cluster: &str) -> Initialised {
        let node = tempfile::tempdir().unwrap();
        let operator = tempfile::tempdir().unwrap();
        let passphrase = passphrase_file(operator.path(), PASSPHRASE);
        let backup = operator.path().join(format!("{cluster}-root-backup.age"));
        super::super::commands::init_with_security(
            node.path(),
            cluster,
            "node-01",
            super::super::commands::InitSecurityMode::MutualTls,
            &InitRootBackup {
                path: backup.clone(),
                target: BackupTarget::Passphrase(PassphraseSource::File(passphrase.clone())),
            },
            OutputFormat::Human,
        )
        .unwrap();
        let identity = crate::sesame::identity_store::load(&node.path().join("identity"))
            .unwrap()
            .unwrap();
        let fingerprint = crate::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der);
        Initialised {
            node,
            operator,
            backup,
            passphrase,
            fingerprint,
        }
    }

    fn passphrase_file(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("passphrase");
        fs::write(&path, format!("{text}\n")).unwrap();
        path
    }

    fn by_passphrase(path: &Path) -> BackupKey {
        BackupKey {
            passphrase: PassphraseSource::File(path.to_path_buf()),
            identity: None,
        }
    }

    fn identity_file(dir: &Path) -> (age::x25519::Identity, PathBuf) {
        use age::secrecy::ExposeSecret as _;
        let identity = age::x25519::Identity::generate();
        let path = dir.join("operator.key");
        fs::write(
            &path,
            format!(
                "# created: today\n{}\n",
                identity.to_string().expose_secret()
            ),
        )
        .unwrap();
        (identity, path)
    }

    #[test]
    fn init_without_a_terminal_refuses_without_a_root_backup_destination() {
        let err =
            InitRootBackup::from_flags(None, None, None, false, || unreachable!()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("--root-backup"), "{text}");
        assert!(text.contains("--passphrase-file"), "{text}");
        let err = InitRootBackup::from_flags(
            Some(PathBuf::from("/media/usb/root.age")),
            None,
            None,
            false,
            || unreachable!(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--recipient"), "{err}");
    }

    #[test]
    fn init_at_a_terminal_asks_where_the_backup_goes_and_for_a_passphrase() {
        let destination = InitRootBackup::from_flags(None, None, None, true, || {
            Ok("/media/usb/root.age\n".to_string())
        })
        .unwrap();
        assert_eq!(destination.path, PathBuf::from("/media/usb/root.age"));
        assert!(matches!(
            destination.target,
            BackupTarget::Passphrase(PassphraseSource::Prompt)
        ));
        let err = InitRootBackup::from_flags(None, None, None, true, || Ok("\n".to_string()))
            .unwrap_err();
        assert!(err.to_string().contains("--root-backup"), "{err}");
    }

    #[test]
    fn init_flags_choose_the_seal() {
        let recipient = InitRootBackup::from_flags(
            Some(PathBuf::from("root.age")),
            Some("age1example".to_string()),
            None,
            false,
            || unreachable!(),
        )
        .unwrap();
        assert!(matches!(recipient.target, BackupTarget::Recipient(ref r) if r == "age1example"));
        let file = InitRootBackup::from_flags(
            Some(PathBuf::from("root.age")),
            None,
            Some(PathBuf::from("pass")),
            false,
            || unreachable!(),
        )
        .unwrap();
        assert!(matches!(
            file.target,
            BackupTarget::Passphrase(PassphraseSource::File(_))
        ));
    }

    #[test]
    fn verify_opens_the_backup_init_wrote() {
        let cluster = init_cluster("prod");
        let backup = open_and_verify(
            &cluster.backup,
            &cluster.fingerprint,
            &PassphraseSource::File(cluster.passphrase.clone()),
            None,
            SystemTime::now(),
        )
        .unwrap();
        assert_eq!(backup.cluster, "prod");
        assert!(!cluster.node.path().join("prod-root-ca.age").exists());
    }

    #[test]
    fn backup_rewraps_an_operator_backup_to_a_new_passphrase() {
        let cluster = init_cluster("prod");
        let new_passphrase = cluster.operator.path().join("new-passphrase");
        fs::write(&new_passphrase, "a different long passphrase\n").unwrap();
        let original = open_backup(&cluster.backup, &by_passphrase(&cluster.passphrase)).unwrap();
        let out = cluster.operator.path().join("rewrapped.age");
        let seal = BackupSeal::Passphrase(read_passphrase_file(&new_passphrase).unwrap());
        rewrap(&original, &out, &seal, SystemTime::now()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let reopened = open_and_verify(
            &out,
            &cluster.fingerprint,
            &PassphraseSource::File(new_passphrase),
            None,
            SystemTime::now(),
        )
        .unwrap();
        assert_eq!(reopened, original);
    }

    #[test]
    fn backup_rewraps_to_a_recipient_that_only_its_identity_opens() {
        let cluster = init_cluster("prod");
        let (identity, identity_path) = identity_file(cluster.operator.path());
        let original = open_backup(&cluster.backup, &by_passphrase(&cluster.passphrase)).unwrap();
        let out = cluster.operator.path().join("backup.age");
        let seal = BackupSeal::Recipient(identity.to_public().to_string());
        rewrap(&original, &out, &seal, SystemTime::now()).unwrap();

        open_and_verify(
            &out,
            &cluster.fingerprint,
            &PassphraseSource::Prompt,
            Some(&identity_path),
            SystemTime::now(),
        )
        .unwrap();
        let err = open_and_verify(
            &out,
            &cluster.fingerprint,
            &PassphraseSource::Prompt,
            None,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(
            matches!(err, RelishError::RootBackup(RootBackupError::NeedsIdentity)),
            "{err}"
        );
    }

    #[test]
    fn backup_refuses_to_overwrite_an_existing_file() {
        let cluster = init_cluster("prod");
        let original = open_backup(&cluster.backup, &by_passphrase(&cluster.passphrase)).unwrap();
        let out = cluster.operator.path().join("exists.age");
        fs::write(&out, "keep me").unwrap();
        let seal = BackupSeal::Recipient(age::x25519::Identity::generate().to_public().to_string());

        let err = rewrap(&original, &out, &seal, SystemTime::now()).unwrap_err();
        assert!(matches!(err, RelishError::FileExists { .. }), "{err}");
        assert_eq!(fs::read_to_string(&out).unwrap(), "keep me");
    }

    #[test]
    fn backup_needs_the_existing_backups_passphrase() {
        let cluster = init_cluster("prod");
        let wrong = cluster.operator.path().join("wrong");
        fs::write(&wrong, "not the passphrase at all").unwrap();
        let err = open_backup(&cluster.backup, &by_passphrase(&wrong)).unwrap_err();
        assert!(
            matches!(err, RelishError::RootBackup(RootBackupError::OpenFailed(_))),
            "{err}"
        );
    }

    #[test]
    fn verify_refuses_another_clusters_backup() {
        let ours = init_cluster("prod");
        let theirs = init_cluster("prod");
        let err = open_and_verify(
            &theirs.backup,
            &ours.fingerprint,
            &PassphraseSource::File(theirs.passphrase.clone()),
            None,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                RelishError::RootBackup(RootBackupError::ForeignRoot { .. })
            ),
            "{err}"
        );
    }

    /// A CSR the way the council's prepare step answers it.
    fn prepared(role: CaRole, fingerprint: &str) -> PreparedRotation {
        use base64::Engine as _;
        let (csr, _) = crate::sesame::ca::create_intermediate_csr(role, b"ikm").unwrap();
        PreparedRotation {
            role,
            generation: 1,
            serial: 77,
            csr_b64: base64::engine::general_purpose::STANDARD.encode(csr),
            root_fingerprint: fingerprint.to_string(),
        }
    }

    #[test]
    fn rotate_signs_the_councils_csr_with_the_backup_root() {
        use base64::Engine as _;
        let cluster = init_cluster("prod");
        let fingerprint = cluster.fingerprint.clone();
        let backup = open_and_verify(
            &cluster.backup,
            &fingerprint,
            &PassphraseSource::File(cluster.passphrase.clone()),
            None,
            SystemTime::now(),
        )
        .unwrap();

        let signed =
            sign_prepared_rotation(&backup, &prepared(CaRole::Node, &fingerprint)).unwrap();
        assert_eq!(signed.role, CaRole::Node);
        let certificate = base64::engine::general_purpose::STANDARD
            .decode(&signed.certificate_b64)
            .unwrap();
        let root = backup.certificate_der().unwrap();
        crate::sesame::cert::verify_signature(&certificate, &root).unwrap();
        assert_eq!(
            crate::sesame::cert::serial_from_der(&certificate).unwrap(),
            crate::sesame::types::SerialNumber(77)
        );
    }

    #[test]
    fn rotate_refuses_a_backup_of_another_root_before_signing() {
        let ours = init_cluster("prod");
        let theirs = init_cluster("prod");

        // `ca_rotate` opens the backup against the fingerprint the council
        // sent with the CSR, so another cluster's root never signs.
        let council = prepared(CaRole::Node, &ours.fingerprint);
        let err = open_and_verify(
            &theirs.backup,
            &council.root_fingerprint,
            &PassphraseSource::File(theirs.passphrase.clone()),
            None,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                RelishError::RootBackup(RootBackupError::ForeignRoot { .. })
            ),
            "{err}"
        );
    }

    #[test]
    fn rotate_refuses_an_unreadable_csr() {
        let cluster = init_cluster("prod");
        let backup = open_backup(&cluster.backup, &by_passphrase(&cluster.passphrase)).unwrap();
        let mut garbled = prepared(CaRole::Workload, &cluster.fingerprint);
        garbled.csr_b64 = "bm90IGEgY3Ny".into();
        assert!(matches!(
            sign_prepared_rotation(&backup, &garbled),
            Err(RelishError::CertificateAuthority(_))
        ));
    }

    #[test]
    fn a_passphrase_file_loses_only_its_line_ending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p");
        fs::write(&path, "  spaced passphrase  \r\nsecond line\n").unwrap();
        use age::secrecy::ExposeSecret as _;
        assert_eq!(
            read_passphrase_file(&path).unwrap().expose_secret(),
            "  spaced passphrase  "
        );
    }

    /// #1566: a backup's JSON says what it holds, never the key.
    #[test]
    fn ca_reports_as_json() {
        let summary = BackupSummary {
            result: BackupCheck::Ok,
            path: Path::new("root-ca.backup"),
            cluster: "prod",
            trust_domain: "prod.reliaburger",
            fingerprint: "sha256:ab12",
            expires_at: 2_000_000_000,
        };
        insta::assert_snapshot!(
            "backup",
            crate::relish::output::format_output(&summary, OutputFormat::Json).unwrap()
        );
        insta::assert_snapshot!(
            "rotate",
            crate::relish::output::format_output(
                &CaRotation {
                    role: "node",
                    stage: RotationStage::Begun,
                    message: "node CA rotation begun".to_string(),
                },
                OutputFormat::Json
            )
            .unwrap()
        );
    }
}
