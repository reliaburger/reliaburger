//! `relish ca backup` and `relish ca verify`: the root CA backup the operator
//! holds (F04 R3), and `relish ca rotate`, which signs a new intermediate
//! with it (F04 R4).
//!
//! `backup` and `verify` run offline. `backup` runs where the master key is, on the node
//! `relish init` ran on, and reads the three files init wrote there: the
//! master key, the security bootstrap state and the sealed root key. The
//! root key goes from there straight into a file sealed to the operator's
//! passphrase or age key, and never onto the cluster.

use std::fs;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use age::secrecy::SecretString;

use super::RelishError;
use crate::sesame::bootstrap;
use crate::sesame::ca_rotation::{PreparedRotation, SignedIntermediate};
use crate::sesame::root_backup::{self, BackupOpener, BackupSeal, RootBackup, SealKind};
use crate::sesame::types::CaRole;

/// The directory `relish ca backup` looks in when `--dir` isn't given, where
/// the Linux server guide runs `relish init`.
pub const DEFAULT_INIT_DIR: &str = "/etc/reliaburger";

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

/// The files `relish init` wrote for one cluster.
#[derive(Debug, Clone)]
struct InitFiles {
    cluster: String,
    master_key: PathBuf,
    security_state: PathBuf,
    sealed_root: PathBuf,
}

impl InitFiles {
    /// Find the cluster's files in `dir`. Without a cluster name, the
    /// directory must hold exactly one `*-security-bootstrap.json`.
    fn find(dir: &Path, cluster: Option<&str>) -> Result<Self, RelishError> {
        let cluster = match cluster {
            Some(name) => name.to_string(),
            None => only_cluster_in(dir)?,
        };
        Ok(Self {
            master_key: dir.join(format!("{cluster}-master.key")),
            security_state: dir.join(format!("{cluster}-security-bootstrap.json")),
            sealed_root: dir.join(format!("{cluster}-root-ca.age")),
            cluster,
        })
    }
}

fn only_cluster_in(dir: &Path) -> Result<String, RelishError> {
    let mut clusters: Vec<String> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix("-security-bootstrap.json"))
                .map(str::to_string)
        })
        .collect();
    clusters.sort();
    match clusters.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(RelishError::InvalidFlag {
            flag: "dir".to_string(),
            reason: format!(
                "no *-security-bootstrap.json in {}: run this where `relish init` ran",
                dir.display()
            ),
        }),
        many => Err(RelishError::InvalidFlag {
            flag: "cluster-name".to_string(),
            reason: format!(
                "{} holds several clusters ({}): pass --cluster-name",
                dir.display(),
                many.join(", ")
            ),
        }),
    }
}

/// `relish ca backup`: write the root CA backup to `out`.
pub fn ca_backup(
    out: &Path,
    dir: &Path,
    cluster: Option<&str>,
    target: &BackupTarget,
) -> Result<(), RelishError> {
    // Resolve the seal first, so a mistyped passphrase or recipient fails
    // before the root key is ever unsealed.
    let seal = match target {
        BackupTarget::Passphrase(source) => BackupSeal::Passphrase(new_passphrase(source)?),
        BackupTarget::Recipient(public_key) => BackupSeal::Recipient(public_key.clone()),
    };
    let backup = write_backup(out, dir, cluster, &seal, SystemTime::now())?;

    println!("wrote the root CA backup to {}", out.display());
    print_summary(&backup);
    println!();
    println!("Keep it off the cluster, with the passphrase or age identity stored apart from it.");
    println!(
        "Check it at any time with `relish ca verify {} --fingerprint {}`.",
        out.display(),
        backup.fingerprint
    );
    Ok(())
}

/// Build, seal and write the backup. Split from [`ca_backup`] so tests can
/// pass the seal and the clock.
fn write_backup(
    out: &Path,
    dir: &Path,
    cluster: Option<&str>,
    seal: &BackupSeal,
    now: SystemTime,
) -> Result<RootBackup, RelishError> {
    let files = InitFiles::find(dir, cluster)?;
    let master_key = bootstrap::load_master_key(&files.master_key)?;
    let state = bootstrap::load_bootstrap_state(&files.security_state)?;
    let sealed_root = fs::read(&files.sealed_root).map_err(|e| {
        RelishError::Io(std::io::Error::new(
            e.kind(),
            format!("failed to read {}: {e}", files.sealed_root.display()),
        ))
    })?;

    let backup =
        root_backup::backup_from_init(&files.cluster, &state, &sealed_root, &master_key, now)?;
    let sealed = backup.seal(seal)?;
    super::commands::write_private_key(out, &sealed)?;
    Ok(backup)
}

/// `relish ca verify`: open a backup and check it offline.
pub fn ca_verify(
    file: &Path,
    fingerprint: &str,
    passphrase: &PassphraseSource,
    identity: Option<&Path>,
) -> Result<(), RelishError> {
    let backup = open_and_verify(file, fingerprint, passphrase, identity, SystemTime::now())?;
    println!("{}: OK", file.display());
    print_summary(&backup);
    Ok(())
}

fn open_and_verify(
    file: &Path,
    fingerprint: &str,
    passphrase: &PassphraseSource,
    identity: Option<&Path>,
    now: SystemTime,
) -> Result<RootBackup, RelishError> {
    let sealed = fs::read(file)?;
    let opener = match (root_backup::seal_kind(&sealed)?, identity) {
        (SealKind::Recipient, Some(path)) => BackupOpener::Identity(read_age_identity(path)?),
        (SealKind::Recipient, None) => {
            return Err(root_backup::RootBackupError::NeedsIdentity.into());
        }
        (SealKind::Passphrase, _) => BackupOpener::Passphrase(existing_passphrase(passphrase)?),
    };
    let backup = RootBackup::open(&sealed, &opener)?;
    backup.verify(fingerprint, now)?;
    Ok(backup)
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
    println!("{message}");
    println!();
    match role {
        CaRole::Node => println!(
            "Every node now acknowledges the new trust set, then renews onto the new CA one at \
             a time."
        ),
        CaRole::Workload => {
            println!("Workload certificates move to the new CA as they renew, within the hour.")
        }
        CaRole::Ingress | CaRole::Root => println!(
            "Ingress certificates now come from the new CA; each node re-mints its routes' \
             certificates as it picks the new CA up."
        ),
    }
    println!(
        "Retire the old CA with `relish ca rotate --role {} --finalize`; it says what is still \
         waiting if it's too early.",
        role_flag(role)
    );
    Ok(())
}

/// `relish ca rotate --role ROLE --finalize`: retire the old CA, refused
/// while anything could still depend on it.
pub async fn ca_rotate_finalize(role: CaRole) -> Result<(), RelishError> {
    let client = super::client::BunClient::default_local();
    println!("{}", client.ca_rotation_finalize(role).await?);
    Ok(())
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

fn print_summary(backup: &RootBackup) {
    println!("  cluster:      {}", backup.cluster);
    println!("  trust domain: {}", backup.trust_domain);
    println!("  fingerprint:  {}", backup.fingerprint);
    println!("  expires:      {}", format_expiry(backup.not_after));
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
    Ok(SecretString::new(line))
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
    Ok(SecretString::new(
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
    use crate::sesame::root_backup::RootBackupError;

    const PASSPHRASE: &str = "correct horse battery staple";

    /// Run `relish init` into a temporary directory and return it with the
    /// root fingerprint the cluster's nodes pin.
    fn init_cluster(cluster: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        super::super::commands::init_with_security(
            dir.path(),
            cluster,
            "node-01",
            super::super::commands::InitSecurityMode::MutualTls,
        )
        .unwrap();
        let identity = crate::sesame::identity_store::load(&dir.path().join("identity"))
            .unwrap()
            .unwrap();
        let fingerprint = crate::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der);
        (dir, fingerprint)
    }

    fn passphrase_file(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("passphrase");
        fs::write(&path, format!("{text}\n")).unwrap();
        path
    }

    #[test]
    fn backup_then_verify_round_trips_from_the_init_directory() {
        let (dir, fingerprint) = init_cluster("prod");
        let out = dir.path().join("prod-root-backup.age");
        let path = passphrase_file(dir.path(), PASSPHRASE);
        let seal = BackupSeal::Passphrase(read_passphrase_file(&path).unwrap());
        let source = PassphraseSource::File(path);

        let backup = write_backup(&out, dir.path(), None, &seal, SystemTime::now()).unwrap();
        assert_eq!(backup.cluster, "prod");
        assert_eq!(backup.fingerprint, fingerprint);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let verified =
            open_and_verify(&out, &fingerprint, &source, None, SystemTime::now()).unwrap();
        assert_eq!(verified, backup);
    }

    #[test]
    fn backup_refuses_to_overwrite_an_existing_file() {
        let (dir, _) = init_cluster("prod");
        let out = dir.path().join("exists.age");
        fs::write(&out, "keep me").unwrap();
        let seal = BackupSeal::Recipient(age::x25519::Identity::generate().to_public().to_string());

        let err = write_backup(&out, dir.path(), None, &seal, SystemTime::now()).unwrap_err();
        assert!(matches!(err, RelishError::FileExists { .. }), "{err}");
        assert_eq!(fs::read_to_string(&out).unwrap(), "keep me");
    }

    #[test]
    fn verify_opens_a_recipient_sealed_backup_with_an_identity_file() {
        let (dir, fingerprint) = init_cluster("prod");
        let identity = age::x25519::Identity::generate();
        let identity_path = dir.path().join("operator.key");
        {
            use age::secrecy::ExposeSecret as _;
            fs::write(
                &identity_path,
                format!(
                    "# created: today\n{}\n",
                    identity.to_string().expose_secret()
                ),
            )
            .unwrap();
        }
        let out = dir.path().join("backup.age");
        let seal = BackupSeal::Recipient(identity.to_public().to_string());
        write_backup(&out, dir.path(), Some("prod"), &seal, SystemTime::now()).unwrap();

        open_and_verify(
            &out,
            &fingerprint,
            &PassphraseSource::Prompt,
            Some(&identity_path),
            SystemTime::now(),
        )
        .unwrap();

        let err = open_and_verify(
            &out,
            &fingerprint,
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
    fn verify_refuses_another_clusters_backup() {
        let (ours, our_fingerprint) = init_cluster("prod");
        let (theirs, _) = init_cluster("prod");
        let out = ours.path().join("theirs.age");
        let identity = age::x25519::Identity::generate();
        let identity_path = ours.path().join("operator.key");
        {
            use age::secrecy::ExposeSecret as _;
            fs::write(&identity_path, identity.to_string().expose_secret()).unwrap();
        }
        let seal = BackupSeal::Recipient(identity.to_public().to_string());
        write_backup(&out, theirs.path(), None, &seal, SystemTime::now()).unwrap();

        let err = open_and_verify(
            &out,
            &our_fingerprint,
            &PassphraseSource::Prompt,
            Some(&identity_path),
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
    fn backup_needs_a_cluster_name_when_the_directory_holds_several() {
        let (dir, _) = init_cluster("prod");
        fs::write(dir.path().join("staging-security-bootstrap.json"), "{}").unwrap();
        let seal = BackupSeal::Recipient(age::x25519::Identity::generate().to_public().to_string());

        let err = write_backup(
            &dir.path().join("out.age"),
            dir.path(),
            None,
            &seal,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--cluster-name"), "{err}");
    }

    #[test]
    fn backup_refuses_a_directory_without_init_files() {
        let dir = tempfile::tempdir().unwrap();
        let seal = BackupSeal::Recipient(age::x25519::Identity::generate().to_public().to_string());
        let err = write_backup(
            &dir.path().join("out.age"),
            dir.path(),
            None,
            &seal,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("relish init"), "{err}");
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
        let (dir, fingerprint) = init_cluster("prod");
        let out = dir.path().join("root.age");
        let path = passphrase_file(dir.path(), PASSPHRASE);
        let seal = BackupSeal::Passphrase(read_passphrase_file(&path).unwrap());
        write_backup(&out, dir.path(), None, &seal, SystemTime::now()).unwrap();
        let backup = open_and_verify(
            &out,
            &fingerprint,
            &PassphraseSource::File(path),
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
        let (ours, our_fingerprint) = init_cluster("prod");
        let (theirs, _) = init_cluster("prod");
        let out = ours.path().join("theirs.age");
        let path = passphrase_file(ours.path(), PASSPHRASE);
        let seal = BackupSeal::Passphrase(read_passphrase_file(&path).unwrap());
        write_backup(&out, theirs.path(), None, &seal, SystemTime::now()).unwrap();

        // `ca_rotate` opens the backup against the fingerprint the council
        // sent with the CSR, so another cluster's root never signs.
        let council = prepared(CaRole::Node, &our_fingerprint);
        let err = open_and_verify(
            &out,
            &council.root_fingerprint,
            &PassphraseSource::File(path),
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
        let (dir, fingerprint) = init_cluster("prod");
        let out = dir.path().join("root.age");
        let path = passphrase_file(dir.path(), PASSPHRASE);
        let seal = BackupSeal::Passphrase(read_passphrase_file(&path).unwrap());
        let backup = write_backup(&out, dir.path(), None, &seal, SystemTime::now()).unwrap();
        let mut garbled = prepared(CaRole::Workload, &fingerprint);
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
}
