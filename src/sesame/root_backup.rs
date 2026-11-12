//! The root CA backup the operator holds (F04 R3, #709).
//!
//! The root CA is offline: no node ever keeps its private key. `relish init`
//! and the setups that build a cluster from the operator's machine write it
//! only into this backup: the root key and certificate plus the metadata
//! needed to check them (cluster, trust domain, fingerprint, expiry), sealed
//! to a passphrase or to an age recipient the operator holds. It's never
//! sealed to a cluster key, because cluster keys live on the nodes and rotate,
//! and the backup has to outlive them.
//!
//! The sealed file is an ordinary ASCII-armoured age file. The `age` tool
//! opens it too, and inside is JSON with PEM-encoded certificate and key.

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use age::secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};

/// The format tag written into every backup, checked on open.
pub const BACKUP_FORMAT: &str = "reliaburger-root-ca-backup/v1";

/// The shortest passphrase a backup accepts. The passphrase is all that
/// stands between whoever finds the file and the cluster's root of trust.
pub const MIN_PASSPHRASE_CHARS: usize = 12;

/// Errors from building, sealing, opening or checking a root CA backup.
#[derive(Debug, thiserror::Error)]
pub enum RootBackupError {
    #[error("the passphrase must be at least {MIN_PASSPHRASE_CHARS} characters")]
    PassphraseTooShort,

    #[error("invalid age recipient: {0}")]
    InvalidRecipient(String),

    #[error("failed to seal the backup: {0}")]
    SealFailed(String),

    #[error("the backup is sealed to a passphrase, not an age identity")]
    NeedsPassphrase,

    #[error("the backup is sealed to an age recipient: pass the matching identity")]
    NeedsIdentity,

    #[error("could not open the backup (wrong passphrase or identity?): {0}")]
    OpenFailed(String),

    #[error("the backup's contents are not a root CA backup: {0}")]
    Malformed(String),

    #[error("unsupported backup format {0:?} (expected {BACKUP_FORMAT:?})")]
    UnsupportedFormat(String),

    #[error("the private key does not match the root certificate")]
    KeyMismatch,

    #[error("the certificate is not a self-signed root: {0}")]
    NotSelfSigned(String),

    #[error("the backup records fingerprint {recorded}, but its certificate is {actual}")]
    FingerprintRecordMismatch { recorded: String, actual: String },

    #[error("the root certificate expired at {not_after} (unix seconds)")]
    Expired { not_after: i64 },

    #[error("the backup holds root {actual}, not this cluster's {expected}")]
    ForeignRoot { expected: String, actual: String },

    #[error("could not write the backup to {path}: {reason}")]
    WriteFailed { path: String, reason: String },

    #[error("the backup written to {0} doesn't check out")]
    WrittenBackupUnreadable(String),
}

/// What a backup is sealed to.
pub enum BackupSeal {
    /// A passphrase, through age's scrypt recipient (the default).
    Passphrase(SecretString),
    /// An operator's age public key (`age1...`).
    Recipient(String),
}

/// What opens a sealed backup: the passphrase, or the age identity that
/// matches the recipient it was sealed to.
pub enum BackupOpener {
    /// The passphrase the backup was sealed with.
    Passphrase(SecretString),
    /// The operator's age identity.
    Identity(age::x25519::Identity),
}

/// How a sealed backup was sealed, read from its age header without opening it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealKind {
    /// Sealed to a passphrase.
    Passphrase,
    /// Sealed to one or more age recipients.
    Recipient,
}

/// The plaintext inside a sealed backup.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct RootBackup {
    /// Always [`BACKUP_FORMAT`].
    pub format: String,
    /// The cluster name `relish init` was given.
    pub cluster: String,
    /// The SPIFFE trust domain, which is the cluster name.
    pub trust_domain: String,
    /// The root certificate's `sha256:HEX` fingerprint, as `relish init`
    /// and `relish join` print it.
    pub fingerprint: String,
    /// When the root certificate expires, in unix seconds.
    pub not_after: i64,
    /// The root certificate, PEM.
    pub certificate_pem: String,
    /// The root's private key, PKCS#8 PEM.
    pub private_key_pem: String,
}

// A derived `Debug` would print the private key into any log line that
// formats a backup, so the key is redacted.
impl std::fmt::Debug for RootBackup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RootBackup")
            .field("format", &self.format)
            .field("cluster", &self.cluster)
            .field("trust_domain", &self.trust_domain)
            .field("fingerprint", &self.fingerprint)
            .field("not_after", &self.not_after)
            .field("private_key_pem", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl RootBackup {
    /// Build a backup from the root's certificate and private key (both DER).
    ///
    /// Refuses a key that doesn't match the certificate, a certificate that
    /// isn't a self-signed root, and an expired root: a backup that couldn't
    /// pass [`RootBackup::verify`] is never written.
    pub fn new(
        cluster: &str,
        certificate_der: &[u8],
        private_key_der: &[u8],
        now: SystemTime,
    ) -> Result<Self, RootBackupError> {
        let fingerprint = super::identity_store::root_ca_fingerprint(certificate_der);
        let not_after = certificate_not_after(certificate_der)?;
        let backup = Self {
            format: BACKUP_FORMAT.to_string(),
            cluster: cluster.to_string(),
            trust_domain: cluster.to_string(),
            fingerprint: fingerprint.clone(),
            not_after,
            certificate_pem: pem::encode(&pem::Pem::new("CERTIFICATE", certificate_der)),
            private_key_pem: pem::encode(&pem::Pem::new("PRIVATE KEY", private_key_der)),
        };
        backup.verify(&fingerprint, now)?;
        Ok(backup)
    }

    /// Check the backup offline, with no cluster running.
    ///
    /// The key must match the certificate, the certificate must be a
    /// self-signed root that hasn't expired at `now`, and its fingerprint
    /// must be `expected_fingerprint`, the one the cluster's nodes pin.
    pub fn verify(
        &self,
        expected_fingerprint: &str,
        now: SystemTime,
    ) -> Result<(), RootBackupError> {
        if self.format != BACKUP_FORMAT {
            return Err(RootBackupError::UnsupportedFormat(self.format.clone()));
        }
        let certificate_der = self.certificate_der()?;
        let private_key_der = self.private_key_der()?;

        check_key_matches(&certificate_der, &private_key_der)?;

        let actual = super::identity_store::root_ca_fingerprint(&certificate_der);
        if actual != self.fingerprint {
            return Err(RootBackupError::FingerprintRecordMismatch {
                recorded: self.fingerprint.clone(),
                actual,
            });
        }
        if actual != expected_fingerprint.trim() {
            return Err(RootBackupError::ForeignRoot {
                expected: expected_fingerprint.trim().to_string(),
                actual,
            });
        }

        let not_after = certificate_not_after(&certificate_der)?;
        if unix_seconds(now) >= not_after {
            return Err(RootBackupError::Expired { not_after });
        }
        Ok(())
    }

    /// The root certificate, DER.
    pub fn certificate_der(&self) -> Result<Vec<u8>, RootBackupError> {
        parse_pem(&self.certificate_pem, "CERTIFICATE")
    }

    /// The root's private key, PKCS#8 DER.
    pub fn private_key_der(&self) -> Result<Vec<u8>, RootBackupError> {
        parse_pem(&self.private_key_pem, "PRIVATE KEY")
    }

    /// Seal the backup as an ASCII-armoured age file.
    pub fn seal(&self, to: &BackupSeal) -> Result<Vec<u8>, RootBackupError> {
        let encryptor = match to {
            BackupSeal::Passphrase(passphrase) => {
                if passphrase.expose_secret().chars().count() < MIN_PASSPHRASE_CHARS {
                    return Err(RootBackupError::PassphraseTooShort);
                }
                age::Encryptor::with_user_passphrase(passphrase.clone())
            }
            BackupSeal::Recipient(public_key) => {
                let recipient: age::x25519::Recipient = public_key
                    .trim()
                    .parse()
                    .map_err(|e: &str| RootBackupError::InvalidRecipient(e.to_string()))?;
                age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                    .map_err(|e| RootBackupError::SealFailed(e.to_string()))?
            }
        };

        let plaintext = serde_json::to_vec_pretty(self)
            .map_err(|e| RootBackupError::SealFailed(e.to_string()))?;
        let seal_failed = |e: std::io::Error| RootBackupError::SealFailed(e.to_string());

        let mut sealed = vec![];
        let armour =
            age::armor::ArmoredWriter::wrap_output(&mut sealed, age::armor::Format::AsciiArmor)
                .map_err(seal_failed)?;
        let mut writer = encryptor
            .wrap_output(armour)
            .map_err(|e| RootBackupError::SealFailed(e.to_string()))?;
        writer.write_all(&plaintext).map_err(seal_failed)?;
        writer
            .finish()
            .and_then(|armour| armour.finish())
            .map_err(seal_failed)?;
        Ok(sealed)
    }

    /// Open a sealed backup. Doesn't check it: call [`RootBackup::verify`].
    pub fn open(sealed: &[u8], with: &BackupOpener) -> Result<Self, RootBackupError> {
        let open_failed = |e: &dyn std::fmt::Display| RootBackupError::OpenFailed(e.to_string());
        let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(sealed))
            .map_err(|e| open_failed(&e))?;

        // Since age 0.11 the decryptor is one type for both seals, and a
        // passphrase is an identity like any other (`age::scrypt::Identity`).
        // Checking the seal first keeps the error that says which one to pass.
        let mut reader = match (decryptor.is_scrypt(), with) {
            (true, BackupOpener::Passphrase(passphrase)) => {
                let identity = age::scrypt::Identity::new(passphrase.clone());
                decryptor
                    .decrypt(std::iter::once(&identity as &dyn age::Identity))
                    .map_err(|e| open_failed(&e))?
            }
            (false, BackupOpener::Identity(identity)) => decryptor
                .decrypt(std::iter::once(identity as &dyn age::Identity))
                .map_err(|e| open_failed(&e))?,
            (true, BackupOpener::Identity(_)) => return Err(RootBackupError::NeedsPassphrase),
            (false, BackupOpener::Passphrase(_)) => return Err(RootBackupError::NeedsIdentity),
        };

        let mut plaintext = vec![];
        reader
            .read_to_end(&mut plaintext)
            .map_err(|e| open_failed(&e))?;
        serde_json::from_slice(&plaintext).map_err(|e| RootBackupError::Malformed(e.to_string()))
    }
}

/// Read how a sealed backup was sealed, so the CLI knows whether to ask for
/// a passphrase or an identity.
pub fn seal_kind(sealed: &[u8]) -> Result<SealKind, RootBackupError> {
    let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(sealed))
        .map_err(|e| RootBackupError::OpenFailed(e.to_string()))?;
    if decryptor.is_scrypt() {
        Ok(SealKind::Passphrase)
    } else {
        Ok(SealKind::Recipient)
    }
}

/// Where `write_backup` writes a backup, and what it seals it to.
pub enum BackupDestination<'a> {
    /// Sealed to a passphrase or an age recipient the operator chose.
    Sealed {
        /// Where the backup goes.
        path: &'a Path,
        /// What it's sealed to.
        seal: &'a BackupSeal,
    },
    /// Sealed to a fresh age identity, written to its own file. For the
    /// setups that run on the operator's machine and never ask anything.
    FreshIdentity {
        /// Where the backup goes.
        path: &'a Path,
        /// Where the identity that opens it goes.
        identity: &'a Path,
    },
}

/// Seal `backup`, write it where `destination` says, and check what landed
/// on disk before returning. Neither file may exist yet, and both are
/// written owner-only.
///
/// A passphrase backup and one sealed to a fresh identity are opened again
/// and verified against the backup's fingerprint. A backup sealed to an
/// operator's recipient can't be opened here, so it's checked as an age file
/// sealed to a recipient.
pub fn write_backup(
    backup: &RootBackup,
    destination: &BackupDestination<'_>,
    now: SystemTime,
) -> Result<(), RootBackupError> {
    match destination {
        BackupDestination::Sealed { path, seal } => {
            let sealed = backup.seal(seal)?;
            write_new_private(path, &sealed)?;
            let written = read_back(path)?;
            match seal {
                BackupSeal::Passphrase(passphrase) => check_opens(
                    &written,
                    &BackupOpener::Passphrase(passphrase.clone()),
                    backup,
                    path,
                    now,
                ),
                BackupSeal::Recipient(_) => match seal_kind(&written) {
                    Ok(SealKind::Recipient) => Ok(()),
                    _ => Err(RootBackupError::WrittenBackupUnreadable(
                        path.display().to_string(),
                    )),
                },
            }
        }
        BackupDestination::FreshIdentity { path, identity } => {
            let key = age::x25519::Identity::generate();
            let recipient = key.to_public().to_string();
            let identity_file = format!(
                "# The age identity that opens the root CA backup {}\n\
                 # for cluster {} (root {}). Keep it offline, apart from the backup.\n{}\n",
                path.display(),
                backup.cluster,
                backup.fingerprint,
                key.to_string().expose_secret()
            );
            write_new_private(identity, identity_file.as_bytes())?;
            let sealed = backup.seal(&BackupSeal::Recipient(recipient))?;
            write_new_private(path, &sealed)?;
            let written = read_back(path)?;
            check_opens(&written, &BackupOpener::Identity(key), backup, path, now)
        }
    }
}

fn check_opens(
    written: &[u8],
    opener: &BackupOpener,
    backup: &RootBackup,
    path: &Path,
    now: SystemTime,
) -> Result<(), RootBackupError> {
    let unreadable = || RootBackupError::WrittenBackupUnreadable(path.display().to_string());
    let opened = RootBackup::open(written, opener).map_err(|_| unreadable())?;
    opened
        .verify(&backup.fingerprint, now)
        .map_err(|_| unreadable())?;
    if opened != *backup {
        return Err(unreadable());
    }
    Ok(())
}

fn write_new_private(path: &Path, bytes: &[u8]) -> Result<(), RootBackupError> {
    let failed = |e: std::io::Error| RootBackupError::WriteFailed {
        path: path.display().to_string(),
        reason: e.to_string(),
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(failed)?;
    file.write_all(bytes).map_err(failed)?;
    file.sync_all().map_err(failed)
}

fn read_back(path: &Path) -> Result<Vec<u8>, RootBackupError> {
    std::fs::read(path).map_err(|e| RootBackupError::WriteFailed {
        path: path.display().to_string(),
        reason: e.to_string(),
    })
}

fn parse_pem(text: &str, tag: &str) -> Result<Vec<u8>, RootBackupError> {
    let parsed = pem::parse(text).map_err(|e| RootBackupError::Malformed(e.to_string()))?;
    if parsed.tag() != tag {
        return Err(RootBackupError::Malformed(format!(
            "expected a {tag} PEM block, found {}",
            parsed.tag()
        )));
    }
    Ok(parsed.into_contents())
}

fn parse_certificate(
    der: &[u8],
) -> Result<x509_parser::certificate::X509Certificate<'_>, RootBackupError> {
    x509_parser::parse_x509_certificate(der)
        .map(|(_, certificate)| certificate)
        .map_err(|e| RootBackupError::Malformed(format!("root certificate: {e}")))
}

fn certificate_not_after(der: &[u8]) -> Result<i64, RootBackupError> {
    Ok(parse_certificate(der)?.validity().not_after.timestamp())
}

/// The key matches when its public half is the certificate's subject key,
/// and the certificate is a root when that same key verifies its signature.
fn check_key_matches(
    certificate_der: &[u8],
    private_key_der: &[u8],
) -> Result<(), RootBackupError> {
    let certificate = parse_certificate(certificate_der)?;
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(private_key_der.to_vec())
        .map_err(|e| RootBackupError::Malformed(format!("root private key: {e}")))?;
    let keypair = rcgen::KeyPair::from_der_and_sign_algo(&key_der, &rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| RootBackupError::Malformed(format!("root private key: {e}")))?;

    if keypair.public_key_raw() != certificate.public_key().subject_public_key.data.as_ref() {
        return Err(RootBackupError::KeyMismatch);
    }
    certificate
        .verify_signature(None)
        .map_err(|e| RootBackupError::NotSelfSigned(e.to_string()))
}

fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::sesame::ca;
    use crate::sesame::types::SerialNumber;

    const PASSPHRASE: &str = "correct horse battery staple";

    fn passphrase(text: &str) -> SecretString {
        SecretString::from(text.to_string())
    }

    fn root_backup(cluster: &str) -> RootBackup {
        let root = ca::generate_root_ca(cluster, SerialNumber(1)).unwrap();
        RootBackup::new(
            cluster,
            &root.ca.certificate_der,
            &root.private_key_der,
            SystemTime::now(),
        )
        .unwrap()
    }

    // Backups sealed by an age 0.10 build (Reliaburger 0.2.1 and earlier),
    // checked in as fixtures. The operator holds these files for years, so
    // every later age release has to keep opening them (#1026).
    const AGE_0_10_PASSPHRASE_BACKUP: &[u8] =
        include_bytes!("../../tests/fixtures/age/root-backup-passphrase.age");
    const AGE_0_10_RECIPIENT_BACKUP: &[u8] =
        include_bytes!("../../tests/fixtures/age/root-backup-recipient.age");
    const AGE_0_10_IDENTITY: &str = include_str!("../../tests/fixtures/age/identity.txt");
    const AGE_0_10_FINGERPRINT: &str = include_str!("../../tests/fixtures/age/fingerprint.txt");

    fn verify_fixture_backup(opened: &RootBackup) {
        assert_eq!(opened.cluster, "fixture");
        assert_eq!(opened.fingerprint, AGE_0_10_FINGERPRINT.trim());
        // The fixture's root expires one day, so check it the day before.
        let before_expiry = UNIX_EPOCH + Duration::from_secs(opened.not_after as u64 - 86_400);
        opened.verify(AGE_0_10_FINGERPRINT, before_expiry).unwrap();
    }

    #[test]
    fn a_passphrase_backup_sealed_by_age_0_10_still_opens() {
        assert_eq!(
            seal_kind(AGE_0_10_PASSPHRASE_BACKUP).unwrap(),
            SealKind::Passphrase
        );
        let opened = RootBackup::open(
            AGE_0_10_PASSPHRASE_BACKUP,
            &BackupOpener::Passphrase(passphrase(PASSPHRASE)),
        )
        .unwrap();
        verify_fixture_backup(&opened);
    }

    #[test]
    fn a_recipient_backup_sealed_by_age_0_10_still_opens() {
        assert_eq!(
            seal_kind(AGE_0_10_RECIPIENT_BACKUP).unwrap(),
            SealKind::Recipient
        );
        let identity: age::x25519::Identity = AGE_0_10_IDENTITY.trim().parse().unwrap();
        let opened =
            RootBackup::open(AGE_0_10_RECIPIENT_BACKUP, &BackupOpener::Identity(identity)).unwrap();
        verify_fixture_backup(&opened);
    }

    #[test]
    fn a_passphrase_sealed_backup_round_trips_and_verifies() {
        let backup = root_backup("prod");
        let sealed = backup
            .seal(&BackupSeal::Passphrase(passphrase(PASSPHRASE)))
            .unwrap();

        assert!(sealed.starts_with(b"-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(
            !String::from_utf8_lossy(&sealed).contains("PRIVATE KEY"),
            "the key must not appear in the clear"
        );
        assert_eq!(seal_kind(&sealed).unwrap(), SealKind::Passphrase);

        let opened =
            RootBackup::open(&sealed, &BackupOpener::Passphrase(passphrase(PASSPHRASE))).unwrap();
        assert_eq!(opened, backup);
        assert_eq!(opened.cluster, "prod");
        assert_eq!(opened.trust_domain, "prod");
        opened
            .verify(&backup.fingerprint, SystemTime::now())
            .unwrap();
    }

    #[test]
    fn a_recipient_sealed_backup_opens_only_with_its_identity() {
        let backup = root_backup("prod");
        let identity = age::x25519::Identity::generate();
        let sealed = backup
            .seal(&BackupSeal::Recipient(identity.to_public().to_string()))
            .unwrap();
        assert_eq!(seal_kind(&sealed).unwrap(), SealKind::Recipient);

        let opened = RootBackup::open(&sealed, &BackupOpener::Identity(identity)).unwrap();
        assert_eq!(opened, backup);

        let stranger = age::x25519::Identity::generate();
        let err = RootBackup::open(&sealed, &BackupOpener::Identity(stranger)).unwrap_err();
        assert!(matches!(err, RootBackupError::OpenFailed(_)), "{err}");
        let err = RootBackup::open(&sealed, &BackupOpener::Passphrase(passphrase(PASSPHRASE)))
            .unwrap_err();
        assert!(matches!(err, RootBackupError::NeedsIdentity), "{err}");
    }

    #[test]
    fn a_wrong_passphrase_does_not_open_the_backup() {
        let sealed = root_backup("prod")
            .seal(&BackupSeal::Passphrase(passphrase(PASSPHRASE)))
            .unwrap();
        let err = RootBackup::open(
            &sealed,
            &BackupOpener::Passphrase(passphrase("not the passphrase at all")),
        )
        .unwrap_err();
        assert!(matches!(err, RootBackupError::OpenFailed(_)), "{err}");
    }

    #[test]
    fn a_short_passphrase_is_refused() {
        let err = root_backup("prod")
            .seal(&BackupSeal::Passphrase(passphrase("short")))
            .unwrap_err();
        assert!(matches!(err, RootBackupError::PassphraseTooShort), "{err}");
    }

    #[test]
    fn a_key_that_does_not_match_its_certificate_is_refused() {
        let mut backup = root_backup("prod");
        backup.private_key_pem = root_backup("prod").private_key_pem;

        let err = backup
            .verify(&backup.fingerprint, SystemTime::now())
            .unwrap_err();
        assert!(matches!(err, RootBackupError::KeyMismatch), "{err}");

        let other = ca::generate_root_ca("prod", SerialNumber(1)).unwrap();
        let root = ca::generate_root_ca("prod", SerialNumber(1)).unwrap();
        let err = RootBackup::new(
            "prod",
            &root.ca.certificate_der,
            &other.private_key_der,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(matches!(err, RootBackupError::KeyMismatch), "{err}");
    }

    #[test]
    fn an_expired_root_is_refused() {
        let backup = root_backup("prod");
        let after_expiry = UNIX_EPOCH + Duration::from_secs(backup.not_after as u64 + 1);

        let err = backup
            .verify(&backup.fingerprint, after_expiry)
            .unwrap_err();
        assert!(matches!(err, RootBackupError::Expired { .. }), "{err}");

        let root = ca::generate_root_ca("prod", SerialNumber(1)).unwrap();
        let err = RootBackup::new(
            "prod",
            &root.ca.certificate_der,
            &root.private_key_der,
            after_expiry,
        )
        .unwrap_err();
        assert!(matches!(err, RootBackupError::Expired { .. }), "{err}");
    }

    #[test]
    fn another_clusters_backup_is_refused_by_fingerprint() {
        let ours = root_backup("prod");
        let theirs = root_backup("prod");

        let err = theirs
            .verify(&ours.fingerprint, SystemTime::now())
            .unwrap_err();
        match err {
            RootBackupError::ForeignRoot { expected, actual } => {
                assert_eq!(expected, ours.fingerprint);
                assert_eq!(actual, theirs.fingerprint);
            }
            other => panic!("expected ForeignRoot, got {other}"),
        }
    }

    #[test]
    fn a_tampered_fingerprint_record_is_refused() {
        let mut backup = root_backup("prod");
        let claimed = root_backup("prod").fingerprint;
        backup.fingerprint = claimed.clone();

        let err = backup.verify(&claimed, SystemTime::now()).unwrap_err();
        assert!(
            matches!(err, RootBackupError::FingerprintRecordMismatch { .. }),
            "{err}"
        );
    }

    #[test]
    fn an_intermediate_is_not_a_root() {
        let hierarchy = ca::generate_ca_hierarchy("prod", b"ikm").unwrap();
        let key_der = hierarchy.node.signing_keypair.serialize_der();
        let err = RootBackup::new(
            "prod",
            &hierarchy.node.ca.certificate_der,
            &key_der,
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(matches!(err, RootBackupError::NotSelfSigned(_)), "{err}");
    }

    #[test]
    fn debug_output_redacts_the_private_key() {
        let backup = root_backup("prod");
        let printed = format!("{backup:?}");
        assert!(printed.contains("<redacted>"));
        assert!(!printed.contains("PRIVATE KEY"));
    }

    #[test]
    fn a_passphrase_backup_is_written_owner_only_and_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.age");
        let backup = root_backup("prod");
        let seal = BackupSeal::Passphrase(passphrase(PASSPHRASE));
        write_backup(
            &backup,
            &BackupDestination::Sealed {
                path: &path,
                seal: &seal,
            },
            SystemTime::now(),
        )
        .unwrap();
        let opened = RootBackup::open(
            &std::fs::read(&path).unwrap(),
            &BackupOpener::Passphrase(passphrase(PASSPHRASE)),
        )
        .unwrap();
        assert_eq!(opened, backup);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_recipient_backup_is_written_as_an_age_file_for_that_recipient() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.age");
        let operator = age::x25519::Identity::generate();
        let seal = BackupSeal::Recipient(operator.to_public().to_string());
        let backup = root_backup("prod");
        write_backup(
            &backup,
            &BackupDestination::Sealed {
                path: &path,
                seal: &seal,
            },
            SystemTime::now(),
        )
        .unwrap();
        let opened = RootBackup::open(
            &std::fs::read(&path).unwrap(),
            &BackupOpener::Identity(operator),
        )
        .unwrap();
        assert_eq!(opened, backup);
    }

    #[test]
    fn a_backup_sealed_to_a_fresh_identity_opens_only_with_that_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.age");
        let identity_path = dir.path().join("keys").join("root.key");
        std::fs::create_dir(dir.path().join("keys")).unwrap();
        let backup = root_backup("prod");
        write_backup(
            &backup,
            &BackupDestination::FreshIdentity {
                path: &path,
                identity: &identity_path,
            },
            SystemTime::now(),
        )
        .unwrap();
        let identity_text = std::fs::read_to_string(&identity_path).unwrap();
        let key: age::x25519::Identity = identity_text
            .lines()
            .find(|line| line.starts_with("AGE-SECRET-KEY-"))
            .unwrap()
            .parse()
            .unwrap();
        let sealed = std::fs::read(&path).unwrap();
        assert_eq!(
            RootBackup::open(&sealed, &BackupOpener::Identity(key)).unwrap(),
            backup
        );
        let other = age::x25519::Identity::generate();
        assert!(RootBackup::open(&sealed, &BackupOpener::Identity(other)).is_err());
    }

    #[test]
    fn a_backup_never_overwrites_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.age");
        std::fs::write(&path, b"an older backup").unwrap();
        let seal = BackupSeal::Passphrase(passphrase(PASSPHRASE));
        let err = write_backup(
            &root_backup("prod"),
            &BackupDestination::Sealed {
                path: &path,
                seal: &seal,
            },
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(matches!(err, RootBackupError::WriteFailed { .. }), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"an older backup");
    }

    #[test]
    fn a_backup_that_cannot_be_written_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("root.age");
        let seal = BackupSeal::Passphrase(passphrase(PASSPHRASE));
        let err = write_backup(
            &root_backup("prod"),
            &BackupDestination::Sealed {
                path: &path,
                seal: &seal,
            },
            SystemTime::now(),
        )
        .unwrap_err();
        assert!(matches!(err, RootBackupError::WriteFailed { .. }), "{err}");
    }
}
