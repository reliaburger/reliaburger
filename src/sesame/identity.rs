//! Workload identity — CSR generation, validation, signing, and rotation.
//!
//! This module implements the SPIFFE-compatible workload identity lifecycle:
//! workers generate keypairs and CSRs, council nodes validate and sign them,
//! and identity bundles are delivered to workloads via tmpfs.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber as RcgenSerial,
};
use serde::{Deserialize, Serialize};

use super::cert;
use super::types::{SerialNumber, SpiffeUri, WorkloadIdentity};

/// Workload certificate lifetime: 1 hour.
pub const WORKLOAD_CERT_LIFETIME: Duration = Duration::from_secs(3600);

/// Rotation interval: 30 minutes (half of certificate lifetime).
pub const ROTATION_INTERVAL: Duration = Duration::from_secs(1800);

/// Maximum grace period extension when council is unreachable: 4 hours.
pub const GRACE_PERIOD_EXTENSION: Duration = Duration::from_secs(4 * 3600);

/// How far a workload certificate's `not_before` is backdated, so a signer
/// and verifier whose clocks disagree by a few minutes still accept a
/// freshly issued certificate.
pub const CLOCK_SKEW_BACKDATE: Duration = Duration::from_secs(300);

/// Errors from workload identity operations.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("failed to generate workload keypair: {0}")]
    KeyGenFailed(String),
    #[error("failed to create CSR: {0}")]
    CsrFailed(String),
    #[error("CSR validation failed: {0}")]
    CsrValidationFailed(String),
    #[error("failed to sign workload certificate: {0}")]
    SignFailed(String),
    #[error("failed to write identity files: {0}")]
    WriteFailed(#[from] std::io::Error),
    #[error("invalid identity file {file}: {reason}")]
    InvalidFile { file: String, reason: String },
    #[error("OIDC error: {0}")]
    Oidc(#[from] super::oidc::OidcError),
    #[error("CA error: {0}")]
    Ca(#[from] super::ca::CaError),
}

// ---------------------------------------------------------------------------
// Worker-side: CSR generation
// ---------------------------------------------------------------------------

/// Generate a keypair and CSR for a workload identity.
///
/// Returns `(csr_der, private_key_der)`. The private key never leaves
/// the worker node — only the CSR is sent to the council for signing.
pub fn create_workload_csr(spiffe_uri: &SpiffeUri) -> Result<(Vec<u8>, Vec<u8>), IdentityError> {
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| IdentityError::KeyGenFailed(e.to_string()))?;
    let private_key_der = key_pair.serialize_der();

    let uri_string = spiffe_uri.to_uri();

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, &uri_string);
    params.distinguished_name = dn;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];

    // SPIFFE URI SAN — rcgen's SanType::URI produces the correct ASN.1 tag (6)
    let uri_san: SanType = SanType::URI(
        uri_string
            .try_into()
            .map_err(|e: rcgen::Error| IdentityError::CsrFailed(e.to_string()))?,
    );
    params.subject_alt_names = vec![uri_san];

    let csr = params
        .serialize_request(&key_pair)
        .map_err(|e| IdentityError::CsrFailed(e.to_string()))?;

    Ok((csr.der().to_vec(), private_key_der))
}

// ---------------------------------------------------------------------------
// Council-side: CSR validation and signing
// ---------------------------------------------------------------------------

/// What a signed workload certificate is authorised to do.
///
/// The EKU set the council stamps onto the leaf is server-owned: a build
/// signer gets code-signing, an mTLS workload gets server/client auth, and
/// never the two shall mix. Image verification refuses a signature whose
/// leaf lacks code-signing (IMG3), so a workload's mTLS cert can't be
/// repurposed to vouch for an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertUsage {
    /// Server + client auth, for the internal mTLS mesh (the default).
    Mtls,
    /// Code signing, for the build-signer identity.
    CodeSigning,
}

impl CertUsage {
    fn extended_key_usages(self) -> Vec<ExtendedKeyUsagePurpose> {
        match self {
            CertUsage::Mtls => vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
            CertUsage::CodeSigning => vec![ExtendedKeyUsagePurpose::CodeSigning],
        }
    }
}

/// Validate a CSR and sign it with the Workload CA.
///
/// The council calls this after verifying that the requesting node is
/// actually scheduled to run the workload. Returns the signed
/// certificate DER.
///
/// The only thing taken from the CSR is its public key. Every other
/// certificate field — the SAN list and the EKU set in particular — is
/// rebuilt server-side from what the council *expects*, so a CSR smuggling
/// extra SANs (another workload's SPIFFE URI, a DNS name) or a code-signing
/// EKU it wasn't granted never gets them signed (PKI6). Validity is an exact
/// timestamped window around `now` (injected for testability).
pub fn validate_and_sign_csr(
    csr_der: &[u8],
    expected_spiffe_uri: &SpiffeUri,
    serial: SerialNumber,
    usage: CertUsage,
    workload_ca_keypair: &KeyPair,
    workload_ca_params: &CertificateParams,
    now: SystemTime,
) -> Result<Vec<u8>, IdentityError> {
    let csr_der_owned: Vec<u8> = csr_der.to_vec();
    let csr_der_ref = rustls::pki_types::CertificateSigningRequestDer::from(csr_der_owned);
    let csr_params = CertificateSigningRequestParams::from_der(&csr_der_ref)
        .map_err(|e| IdentityError::CsrValidationFailed(format!("failed to parse CSR: {e}")))?;

    // The CSR must claim the identity the council expects. This is a
    // request check only — the claimed SAN list is never signed as-is.
    let expected_uri = expected_spiffe_uri.to_uri();
    let has_matching_uri = csr_params
        .params
        .subject_alt_names
        .iter()
        .any(|san| matches!(san, SanType::URI(uri) if uri.as_str() == expected_uri));
    if !has_matching_uri {
        return Err(IdentityError::CsrValidationFailed(format!(
            "CSR does not contain expected URI SAN: {expected_uri}"
        )));
    }

    // Rebuild the certificate contents server-side.
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, &expected_uri);
    params.distinguished_name = dn;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = usage.extended_key_usages();
    params.subject_alt_names =
        vec![SanType::URI(expected_uri.try_into().map_err(
            |e: rcgen::Error| IdentityError::SignFailed(e.to_string()),
        )?)];
    params.serial_number = Some(RcgenSerial::from_slice(&serial.0.to_be_bytes()));

    // Exact validity window: a one-hour certificate is valid for one hour
    // (plus the skew backdate), not until midnight.
    params.not_before = time::OffsetDateTime::from(now - CLOCK_SKEW_BACKDATE);
    params.not_after = time::OffsetDateTime::from(now + WORKLOAD_CERT_LIFETIME);

    // Reconstruct the CA certificate object for signing
    let ca_cert = workload_ca_params
        .clone()
        .self_signed(workload_ca_keypair)
        .map_err(|e| IdentityError::SignFailed(format!("failed to reconstruct CA cert: {e}")))?;

    let signed = params
        .signed_by(&csr_params.public_key, &ca_cert, workload_ca_keypair)
        .map_err(|e| IdentityError::SignFailed(e.to_string()))?;

    Ok(signed.der().to_vec())
}

// ---------------------------------------------------------------------------
// Identity bundle assembly
// ---------------------------------------------------------------------------

/// Build a complete workload identity bundle from a signed certificate.
pub fn build_identity_bundle(
    spiffe_uri: SpiffeUri,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
    workload_ca_cert_der: &[u8],
    root_ca_cert_der: &[u8],
    jwt_token: String,
) -> WorkloadIdentity {
    let workload_ca_pem = cert::der_to_pem(workload_ca_cert_der, "CERTIFICATE");
    let root_ca_pem = cert::der_to_pem(root_ca_cert_der, "CERTIFICATE");
    let ca_chain_pem = format!("{workload_ca_pem}{root_ca_pem}");

    let now = SystemTime::now();
    WorkloadIdentity {
        spiffe_uri,
        certificate_der,
        private_key_der,
        ca_chain_pem,
        jwt_token,
        issued_at: now,
        expires_at: now + WORKLOAD_CERT_LIFETIME,
        next_rotation: now + ROTATION_INTERVAL,
        grace_extended: false,
    }
}

// ---------------------------------------------------------------------------
// Per-instance identity directories
// ---------------------------------------------------------------------------

/// Metadata sidecar file name inside an instance identity directory.
const IDENTITY_META_FILE: &str = "meta.json";

/// Size cap for the tmpfs backing an instance identity directory. The
/// bundle is a few kilobytes; 1 MiB leaves generous headroom.
#[cfg(target_os = "linux")]
const IDENTITY_TMPFS_SIZE: &str = "1m";

/// Rotation metadata persisted next to the identity files, so a restarted
/// Bun can rebuild the rotation schedule from disk instead of re-CSRing
/// from scratch (D9). Contains no secrets.
#[derive(Debug, Serialize, Deserialize)]
struct WorkloadIdentityMeta {
    schema: u32,
    spiffe_uri: SpiffeUri,
    issued_at: SystemTime,
    expires_at: SystemTime,
    next_rotation: SystemTime,
    grace_extended: bool,
}

/// The identity directory for one workload instance:
/// `{volumes_dir}/.identity/{instance_id}` (PKI7). Keying by instance —
/// not by app — is what stops replicas of the same app overwriting one
/// another's private key.
pub fn instance_identity_dir(volumes_dir: &Path, instance_id: &str) -> PathBuf {
    volumes_dir.join(".identity").join(instance_id)
}

/// Prepare an instance's identity directory before its container is
/// created (the bind-mount source must exist).
///
/// On Linux, running as root, the directory is backed by a small
/// dedicated tmpfs so private keys never touch persistent storage.
/// Everywhere else (macOS, rootless) it is a plain `0700` directory —
/// a documented gap, not silent parity. Idempotent: re-driving a
/// restart never stacks mounts.
pub fn prepare_identity_dir(dir: &Path) -> Result<(), IdentityError> {
    std::fs::create_dir_all(dir)?;
    restrict_dir_permissions(dir)?;

    #[cfg(target_os = "linux")]
    if nix::unistd::geteuid().is_root() && !is_mount_point(dir) {
        let status = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "-o"])
            .arg(format!("size={IDENTITY_TMPFS_SIZE},mode=0700"))
            .arg("reliaburger-identity")
            .arg(dir)
            .status();
        match status {
            Ok(s) if s.success() => {}
            other => {
                // Fall back to the plain directory rather than failing the
                // deploy: the key material is still 0700, just disk-backed.
                eprintln!(
                    "sesame: warning: tmpfs mount for {} failed ({other:?}); \
                     identity files will be disk-backed",
                    dir.display()
                );
            }
        }
    }

    Ok(())
}

/// Remove an instance's identity directory, unmounting its tmpfs backing
/// first when present. Key material must not outlive the instance (PKI7).
pub fn cleanup_identity_dir(dir: &Path) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    if is_mount_point(dir) {
        let status = std::process::Command::new("umount").arg(dir).status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "umount {} failed",
                dir.display()
            )));
        }
    }

    std::fs::remove_dir_all(dir)
}

/// Owner-only directory permissions (no-op off Unix).
fn restrict_dir_permissions(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Whether `dir` is currently a mount point (Linux: scan
/// `/proc/self/mounts` for its canonical path).
#[cfg(target_os = "linux")]
fn is_mount_point(dir: &Path) -> bool {
    let Ok(canonical) = dir.canonicalize() else {
        return false;
    };
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
        return false;
    };
    let needle = canonical.to_string_lossy();
    mounts
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .any(|mount_point| mount_point == needle)
}

/// Write a workload identity's files into its per-instance directory.
///
/// Writes six files atomically (write to `.tmp` then rename):
/// - `cert.pem` — the workload certificate
/// - `key.pem` — the private key (owner-only)
/// - `ca.pem` — the CA trust chain
/// - `bundle.pem` — cert + CA chain concatenated
/// - `token` — the OIDC JWT string (owner-only)
/// - `meta.json` — rotation metadata for adoption after a Bun restart
///
/// When `owner` is set (Linux root mode), every file and the directory
/// itself are chowned to the workload's runtime UID/GID so the container
/// process — not just root — can read its own owner-only key.
pub fn write_identity_files(
    identity: &WorkloadIdentity,
    dir: &Path,
    owner: Option<(u32, u32)>,
) -> Result<(), IdentityError> {
    std::fs::create_dir_all(dir)?;

    let cert_pem = cert::der_to_pem(&identity.certificate_der, "CERTIFICATE");
    let key_pem = cert::der_to_pem(&identity.private_key_der, "PRIVATE KEY");

    let bundle_pem = format!("{}{}", cert_pem, identity.ca_chain_pem);

    let meta = WorkloadIdentityMeta {
        schema: 1,
        spiffe_uri: identity.spiffe_uri.clone(),
        issued_at: identity.issued_at,
        expires_at: identity.expires_at,
        next_rotation: identity.next_rotation,
        grace_extended: identity.grace_extended,
    };
    let meta_json =
        serde_json::to_string_pretty(&meta).map_err(|e| IdentityError::InvalidFile {
            file: IDENTITY_META_FILE.to_string(),
            reason: e.to_string(),
        })?;

    atomic_write(&dir.join("cert.pem"), cert_pem.as_bytes())?;
    // The private key and the JWT are secrets — owner-only (M25). The certs
    // are public material and stay world-readable.
    atomic_write_mode(&dir.join("key.pem"), key_pem.as_bytes(), Some(0o600))?;
    atomic_write(&dir.join("ca.pem"), identity.ca_chain_pem.as_bytes())?;
    atomic_write(&dir.join("bundle.pem"), bundle_pem.as_bytes())?;
    atomic_write_mode(
        &dir.join("token"),
        identity.jwt_token.as_bytes(),
        Some(0o600),
    )?;
    atomic_write(&dir.join(IDENTITY_META_FILE), meta_json.as_bytes())?;

    if let Some((uid, gid)) = owner {
        chown_identity_dir(dir, uid, gid)?;
    }

    Ok(())
}

/// Load a workload identity back from its per-instance directory.
///
/// Returns `Ok(None)` when no identity has been written there (no
/// `meta.json`) — a legacy app-scoped directory or a not-yet-provisioned
/// instance. Used at adoption so a restarted Bun keeps each workload's
/// identity and rotation schedule instead of `identity: None` (D9).
pub fn load_identity(dir: &Path) -> Result<Option<WorkloadIdentity>, IdentityError> {
    let meta_path = dir.join(IDENTITY_META_FILE);
    if !meta_path.exists() {
        return Ok(None);
    }

    let meta_json = std::fs::read_to_string(&meta_path)?;
    let meta: WorkloadIdentityMeta =
        serde_json::from_str(&meta_json).map_err(|e| IdentityError::InvalidFile {
            file: IDENTITY_META_FILE.to_string(),
            reason: e.to_string(),
        })?;

    let certificate_der = read_pem_der(dir, "cert.pem")?;
    let private_key_der = read_pem_der(dir, "key.pem")?;
    let ca_chain_pem = std::fs::read_to_string(dir.join("ca.pem"))?;
    let jwt_token = std::fs::read_to_string(dir.join("token"))?;

    Ok(Some(WorkloadIdentity {
        spiffe_uri: meta.spiffe_uri,
        certificate_der,
        private_key_der,
        ca_chain_pem,
        jwt_token,
        issued_at: meta.issued_at,
        expires_at: meta.expires_at,
        next_rotation: meta.next_rotation,
        grace_extended: meta.grace_extended,
    }))
}

/// Parse a PEM file back to its DER contents.
fn read_pem_der(dir: &Path, file: &str) -> Result<Vec<u8>, IdentityError> {
    let content = std::fs::read_to_string(dir.join(file))?;
    let parsed = ::pem::parse(&content).map_err(|e| IdentityError::InvalidFile {
        file: file.to_string(),
        reason: e.to_string(),
    })?;
    Ok(parsed.contents().to_vec())
}

/// Chown the identity directory and every file in it (Unix only).
#[cfg(unix)]
fn chown_identity_dir(dir: &Path, uid: u32, gid: u32) -> Result<(), IdentityError> {
    let uid = nix::unistd::Uid::from_raw(uid);
    let gid = nix::unistd::Gid::from_raw(gid);
    let chown = |path: &Path| -> std::io::Result<()> {
        nix::unistd::chown(path, Some(uid), Some(gid))
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
    };
    chown(dir)?;
    for entry in std::fs::read_dir(dir)? {
        chown(&entry?.path())?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn chown_identity_dir(_dir: &Path, _uid: u32, _gid: u32) -> Result<(), IdentityError> {
    Ok(())
}

/// Write data to a temp file then atomically rename.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    atomic_write_mode(path, data, None)
}

/// Write + atomic rename, optionally restricting the final file mode.
///
/// The mode is applied to the temp file before the rename so the file is
/// never briefly world-readable at its final path (M25).
pub(crate) fn atomic_write_mode(
    path: &Path,
    data: &[u8],
    mode: Option<u32>,
) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Rotation state machine
// ---------------------------------------------------------------------------

/// The state of a workload identity's rotation cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationState {
    /// Identity is fresh, no rotation needed.
    Valid,
    /// Rotation interval reached, should re-CSR.
    NeedsRotation,
    /// CSR failed, operating under grace period.
    GracePeriod,
    /// Certificate has hard-expired.
    Expired,
}

/// Determine the rotation state of a workload identity.
pub fn rotation_state(identity: &WorkloadIdentity, now: SystemTime) -> RotationState {
    if now >= identity.expires_at {
        RotationState::Expired
    } else if identity.grace_extended {
        RotationState::GracePeriod
    } else if now >= identity.next_rotation {
        RotationState::NeedsRotation
    } else {
        RotationState::Valid
    }
}

/// Extend the grace period when the council is unreachable.
///
/// Returns `true` if the grace period was extended, `false` if
/// already at the maximum (issued_at + 5 hours).
pub fn extend_grace_period(identity: &mut WorkloadIdentity) -> bool {
    let max_expiry = identity.issued_at + WORKLOAD_CERT_LIFETIME + GRACE_PERIOD_EXTENSION;
    if identity.expires_at >= max_expiry {
        return false;
    }
    identity.expires_at = max_expiry;
    identity.grace_extended = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::ca;
    use crate::sesame::types::WorkloadType;

    fn test_spiffe_uri() -> SpiffeUri {
        SpiffeUri {
            trust_domain: "test-cluster".to_string(),
            namespace: "default".to_string(),
            workload_type: WorkloadType::App,
            name: "api".to_string(),
        }
    }

    fn test_workload_ca() -> (KeyPair, CertificateParams, Vec<u8>, Vec<u8>) {
        let wrapping_ikm = b"test-wrapping-material-32bytes!";
        let hierarchy = ca::generate_ca_hierarchy("test-cluster", wrapping_ikm).unwrap();
        let workload_ca_cert_der = hierarchy.workload.ca.certificate_der.clone();
        let root_ca_cert_der = hierarchy.root.ca.certificate_der.clone();
        (
            hierarchy.workload.signing_keypair,
            hierarchy.workload.certificate_params,
            workload_ca_cert_der,
            root_ca_cert_der,
        )
    }

    /// M25: the private key is written owner-only; public certs are not
    /// forced to 0600.
    #[cfg(unix)]
    #[test]
    fn atomic_write_mode_restricts_the_private_key() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("key.pem");
        let public = dir.path().join("cert.pem");

        atomic_write_mode(&secret, b"PRIVATE KEY", Some(0o600)).unwrap();
        atomic_write(&public, b"CERTIFICATE").unwrap();

        let secret_mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
        assert_eq!(secret_mode, 0o600, "private key must be owner-only");
        let public_mode = std::fs::metadata(&public).unwrap().permissions().mode() & 0o777;
        assert_ne!(public_mode, 0o600, "public cert need not be restricted");
    }

    #[test]
    fn create_workload_csr_has_spiffe_uri_san() {
        let uri = test_spiffe_uri();
        let (csr_der, private_key_der) = create_workload_csr(&uri).unwrap();

        assert!(!csr_der.is_empty());
        assert!(!private_key_der.is_empty());

        // Parse the CSR back and check the SAN
        let csr_ref: rustls::pki_types::CertificateSigningRequestDer<'_> =
            csr_der.as_slice().into();
        let parsed = CertificateSigningRequestParams::from_der(&csr_ref).unwrap();
        let has_uri = parsed.params.subject_alt_names.iter().any(|san| {
            matches!(san, SanType::URI(u) if u.as_str() == "spiffe://test-cluster/ns/default/app/api")
        });
        assert!(has_uri, "CSR should contain SPIFFE URI SAN");
    }

    #[test]
    fn create_workload_csr_app_and_job_variants() {
        let app_uri = SpiffeUri {
            trust_domain: "prod".to_string(),
            namespace: "web".to_string(),
            workload_type: WorkloadType::App,
            name: "frontend".to_string(),
        };
        let job_uri = SpiffeUri {
            trust_domain: "prod".to_string(),
            namespace: "batch".to_string(),
            workload_type: WorkloadType::Job,
            name: "migrate".to_string(),
        };

        let (app_csr, _) = create_workload_csr(&app_uri).unwrap();
        let (job_csr, _) = create_workload_csr(&job_uri).unwrap();

        // Both should parse successfully
        let app_ref: rustls::pki_types::CertificateSigningRequestDer<'_> =
            app_csr.as_slice().into();
        let job_ref: rustls::pki_types::CertificateSigningRequestDer<'_> =
            job_csr.as_slice().into();
        CertificateSigningRequestParams::from_der(&app_ref).unwrap();
        CertificateSigningRequestParams::from_der(&job_ref).unwrap();
    }

    #[test]
    fn validate_and_sign_csr_success() {
        let uri = test_spiffe_uri();
        let (csr_der, _private_key_der) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, _, _) = test_workload_ca();

        let cert_der = validate_and_sign_csr(
            &csr_der,
            &uri,
            SerialNumber(100),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            SystemTime::now(),
        )
        .unwrap();

        assert!(!cert_der.is_empty());

        // Parse and verify it's an end-entity cert
        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der).unwrap();
        assert!(!cert.is_ca());
    }

    #[test]
    fn validate_and_sign_csr_wrong_uri_rejected() {
        let uri = test_spiffe_uri();
        let wrong_uri = SpiffeUri {
            trust_domain: "test-cluster".to_string(),
            namespace: "default".to_string(),
            workload_type: WorkloadType::App,
            name: "wrong-app".to_string(),
        };
        let (csr_der, _) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, _, _) = test_workload_ca();

        let result = validate_and_sign_csr(
            &csr_der,
            &wrong_uri,
            SerialNumber(100),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            SystemTime::now(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("URI SAN"),
            "error should mention URI SAN: {err}"
        );
    }

    #[test]
    fn signed_cert_is_valid_end_entity() {
        let uri = test_spiffe_uri();
        let (csr_der, _) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, _, _) = test_workload_ca();

        let cert_der = validate_and_sign_csr(
            &csr_der,
            &uri,
            SerialNumber(100),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            SystemTime::now(),
        )
        .unwrap();

        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der).unwrap();
        // End-entity cert, not a CA
        assert!(!cert.is_ca());
        // A real validity window: strictly increasing, exact-second precision
        assert!(cert.validity().not_after > cert.validity().not_before);
        // Subject contains the SPIFFE URI
        assert!(cert.subject().to_string().contains("test-cluster"));
    }

    /// PKI6: a one-hour certificate's validity window is one hour (plus the
    /// clock-skew backdate), not equal midnight calendar dates.
    #[test]
    fn one_hour_certificate_window_is_exact_not_calendar_dates() {
        let uri = test_spiffe_uri();
        let (csr_der, _) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, _, _) = test_workload_ca();

        // Injected clock: a fixed instant with no sub-second noise.
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_752_000_000);
        let cert_der = validate_and_sign_csr(
            &csr_der,
            &uri,
            SerialNumber(9),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            now,
        )
        .unwrap();

        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der).unwrap();
        let not_before = cert.validity().not_before.timestamp();
        let not_after = cert.validity().not_after.timestamp();

        let now_secs = 1_752_000_000i64;
        assert_eq!(
            not_before,
            now_secs - CLOCK_SKEW_BACKDATE.as_secs() as i64,
            "not_before is now minus the skew backdate"
        );
        assert_eq!(
            not_after,
            now_secs + WORKLOAD_CERT_LIFETIME.as_secs() as i64,
            "not_after is now plus the configured lifetime"
        );
    }

    /// PKI6: `check_validity` accepts a fresh certificate and rejects the
    /// same certificate once its exact window has passed (injected clock).
    #[test]
    fn check_validity_accepts_fresh_and_rejects_expired_workload_cert() {
        let uri = test_spiffe_uri();
        let (csr_der, _) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, _, _) = test_workload_ca();

        let issued_at = SystemTime::now();
        let cert_der = validate_and_sign_csr(
            &csr_der,
            &uri,
            SerialNumber(9),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            issued_at,
        )
        .unwrap();

        // Valid right now.
        cert::check_validity_at(&cert_der, issued_at).unwrap();
        // Expired one second past the window.
        let past_expiry = issued_at + WORKLOAD_CERT_LIFETIME + Duration::from_secs(1);
        assert!(matches!(
            cert::check_validity_at(&cert_der, past_expiry),
            Err(cert::CertError::Expired)
        ));
        // Not yet valid before the backdated start.
        let before_start = issued_at - CLOCK_SKEW_BACKDATE - Duration::from_secs(1);
        assert!(matches!(
            cert::check_validity_at(&cert_der, before_start),
            Err(cert::CertError::NotYetValid)
        ));
    }

    /// PKI6: the signer rebuilds the SAN list server-side. A CSR smuggling
    /// another workload's SPIFFE URI and a DNS name still yields a
    /// certificate containing exactly the expected URI SAN.
    #[test]
    fn smuggled_csr_sans_are_not_signed() {
        let uri = test_spiffe_uri();
        let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();

        // A hand-built CSR: the legitimate SAN plus two smuggled ones.
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![
            SanType::URI(uri.to_uri().try_into().unwrap()),
            SanType::URI(
                "spiffe://test-cluster/ns/default/app/victim"
                    .to_string()
                    .try_into()
                    .unwrap(),
            ),
            SanType::DnsName(
                "payments.internal.example.com"
                    .to_string()
                    .try_into()
                    .unwrap(),
            ),
        ];
        let csr = params.serialize_request(&key_pair).unwrap();

        let (ca_kp, ca_params, _, _) = test_workload_ca();
        let cert_der = validate_and_sign_csr(
            csr.der(),
            &uri,
            SerialNumber(7),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            SystemTime::now(),
        )
        .unwrap();

        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der).unwrap();
        let san = cert
            .subject_alternative_name()
            .unwrap()
            .expect("issued certificate carries a SAN extension");

        let mut uris = Vec::new();
        let mut other_names = 0usize;
        for name in &san.value.general_names {
            match name {
                x509_parser::extensions::GeneralName::URI(u) => uris.push(u.to_string()),
                _ => other_names += 1,
            }
        }
        assert_eq!(uris, vec![uri.to_uri()], "exactly the expected URI SAN");
        assert_eq!(other_names, 0, "no DNS or other smuggled SANs are signed");
    }

    #[test]
    fn signed_cert_chains_to_workload_ca() {
        let uri = test_spiffe_uri();
        let (csr_der, _) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, workload_ca_cert_der, _) = test_workload_ca();

        let cert_der = validate_and_sign_csr(
            &csr_der,
            &uri,
            SerialNumber(100),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            SystemTime::now(),
        )
        .unwrap();

        // Verify the workload cert is signed by the Workload CA
        cert::verify_signature(&cert_der, &workload_ca_cert_der).unwrap();
    }

    #[test]
    fn build_identity_bundle_populates_all_fields() {
        let uri = test_spiffe_uri();
        let (csr_der, private_key_der) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, workload_ca_cert_der, root_ca_cert_der) = test_workload_ca();

        let cert_der = validate_and_sign_csr(
            &csr_der,
            &uri,
            SerialNumber(100),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            SystemTime::now(),
        )
        .unwrap();

        let identity = build_identity_bundle(
            uri.clone(),
            cert_der,
            private_key_der,
            &workload_ca_cert_der,
            &root_ca_cert_der,
            "test-jwt-token".to_string(),
        );

        assert_eq!(identity.spiffe_uri, uri);
        assert!(!identity.certificate_der.is_empty());
        assert!(!identity.private_key_der.is_empty());
        assert!(identity.ca_chain_pem.contains("BEGIN CERTIFICATE"));
        assert_eq!(identity.jwt_token, "test-jwt-token");
        assert!(!identity.grace_extended);
        assert!(identity.expires_at > identity.issued_at);
        assert!(identity.next_rotation > identity.issued_at);
        assert!(identity.next_rotation < identity.expires_at);
    }

    /// Build a full identity bundle for tests (fresh keypair each call).
    fn test_identity_bundle(jwt: &str) -> WorkloadIdentity {
        let uri = test_spiffe_uri();
        let (csr_der, private_key_der) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, workload_ca_cert_der, root_ca_cert_der) = test_workload_ca();
        let cert_der = validate_and_sign_csr(
            &csr_der,
            &uri,
            SerialNumber(100),
            CertUsage::Mtls,
            &ca_kp,
            &ca_params,
            SystemTime::now(),
        )
        .unwrap();
        build_identity_bundle(
            uri,
            cert_der,
            private_key_der,
            &workload_ca_cert_der,
            &root_ca_cert_der,
            jwt.to_string(),
        )
    }

    #[test]
    fn write_identity_files_creates_all_files_in_the_instance_dir() {
        let identity = test_identity_bundle("my-jwt-token");

        let volumes = tempfile::tempdir().unwrap();
        let dir = instance_identity_dir(volumes.path(), "web-0");
        prepare_identity_dir(&dir).unwrap();
        write_identity_files(&identity, &dir, None).unwrap();

        assert!(dir.join("cert.pem").exists());
        assert!(dir.join("key.pem").exists());
        assert!(dir.join("ca.pem").exists());
        assert!(dir.join("bundle.pem").exists());
        assert!(dir.join("token").exists());
        assert!(dir.join("meta.json").exists());

        // Verify content
        let cert_pem = std::fs::read_to_string(dir.join("cert.pem")).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));

        let key_pem = std::fs::read_to_string(dir.join("key.pem")).unwrap();
        assert!(key_pem.contains("BEGIN PRIVATE KEY"));

        let token = std::fs::read_to_string(dir.join("token")).unwrap();
        assert_eq!(token, "my-jwt-token");

        let bundle = std::fs::read_to_string(dir.join("bundle.pem")).unwrap();
        // Bundle should contain the cert + CA chain (at least 3 PEM blocks)
        let cert_count = bundle.matches("BEGIN CERTIFICATE").count();
        assert!(cert_count >= 2, "bundle should contain cert + CA chain");
    }

    /// PKI7: two replicas of the same app land in distinct per-instance
    /// directories with distinct private keys — the overwrite regression.
    #[test]
    fn two_replicas_of_one_app_get_distinct_identity_dirs_and_keys() {
        let volumes = tempfile::tempdir().unwrap();
        let dir_0 = instance_identity_dir(volumes.path(), "web-0");
        let dir_1 = instance_identity_dir(volumes.path(), "web-1");
        assert_ne!(dir_0, dir_1, "instance dirs must not collide by app");

        let identity_0 = test_identity_bundle("jwt-0");
        let identity_1 = test_identity_bundle("jwt-1");
        write_identity_files(&identity_0, &dir_0, None).unwrap();
        write_identity_files(&identity_1, &dir_1, None).unwrap();

        let key_0 = std::fs::read(dir_0.join("key.pem")).unwrap();
        let key_1 = std::fs::read(dir_1.join("key.pem")).unwrap();
        assert_ne!(key_0, key_1, "replicas must not share a private key");
        // Both survive: neither replica clobbered the other.
        assert!(dir_0.join("cert.pem").exists());
        assert!(dir_1.join("cert.pem").exists());
    }

    /// D9: an identity written to disk loads back byte-for-byte, including
    /// the rotation schedule — what adoption relies on after a Bun restart.
    #[test]
    fn identity_round_trips_through_the_instance_dir() {
        let identity = test_identity_bundle("adopt-me");
        let volumes = tempfile::tempdir().unwrap();
        let dir = instance_identity_dir(volumes.path(), "web-0");
        write_identity_files(&identity, &dir, None).unwrap();

        let loaded = load_identity(&dir).unwrap().expect("identity loads back");
        assert_eq!(loaded.spiffe_uri, identity.spiffe_uri);
        assert_eq!(loaded.certificate_der, identity.certificate_der);
        assert_eq!(loaded.private_key_der, identity.private_key_der);
        assert_eq!(loaded.ca_chain_pem, identity.ca_chain_pem);
        assert_eq!(loaded.jwt_token, identity.jwt_token);
        assert_eq!(loaded.next_rotation, identity.next_rotation);
        assert_eq!(loaded.expires_at, identity.expires_at);
        assert!(!loaded.grace_extended);
    }

    /// A directory without `meta.json` (legacy app-scoped layout, or a
    /// not-yet-provisioned instance) loads as `None`, never an error.
    #[test]
    fn load_identity_returns_none_without_metadata() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_identity(dir.path()).unwrap().is_none());
        // A legacy-style dir with stray files but no sidecar: still None.
        std::fs::write(dir.path().join("cert.pem"), b"stale").unwrap();
        assert!(load_identity(dir.path()).unwrap().is_none());
    }

    /// PKI7: cleanup removes the key material with the instance.
    #[test]
    fn cleanup_identity_dir_removes_key_material() {
        let identity = test_identity_bundle("bye");
        let volumes = tempfile::tempdir().unwrap();
        let dir = instance_identity_dir(volumes.path(), "web-0");
        prepare_identity_dir(&dir).unwrap();
        write_identity_files(&identity, &dir, None).unwrap();
        assert!(dir.join("key.pem").exists());

        cleanup_identity_dir(&dir).unwrap();
        assert!(!dir.exists(), "the whole instance dir is gone");
        // Idempotent: a second cleanup is a no-op.
        cleanup_identity_dir(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn prepare_identity_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let volumes = tempfile::tempdir().unwrap();
        let dir = instance_identity_dir(volumes.path(), "web-0");
        prepare_identity_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "identity dir must be owner-only");
        // Idempotent: preparing again never fails or stacks mounts.
        prepare_identity_dir(&dir).unwrap();
    }

    /// Linux + root only (run in the Lima rig): the identity directory is
    /// backed by a real size-bounded tmpfs, and cleanup unmounts it.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Linux root; run by make test-linux"]
    fn identity_dir_is_tmpfs_backed_under_root() {
        assert!(
            nix::unistd::geteuid().is_root(),
            "the tmpfs identity test requires root"
        );

        let volumes = tempfile::tempdir().unwrap();
        let dir = instance_identity_dir(volumes.path(), "web-0");
        prepare_identity_dir(&dir).unwrap();
        assert!(is_mount_point(&dir), "identity dir should be a tmpfs mount");

        // Preparing again must not stack a second mount.
        prepare_identity_dir(&dir).unwrap();
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
        let canonical = dir.canonicalize().unwrap();
        let count = mounts
            .lines()
            .filter(|l| l.split_whitespace().nth(1) == Some(&canonical.to_string_lossy()))
            .count();
        assert_eq!(count, 1, "prepare is idempotent, no stacked mounts");

        let identity = test_identity_bundle("tmpfs");
        write_identity_files(&identity, &dir, Some((65534, 65534))).unwrap();
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(dir.join("key.pem")).unwrap();
        assert_eq!(meta.uid(), 65534, "key owned by the workload uid");

        cleanup_identity_dir(&dir).unwrap();
        assert!(!dir.exists());
        assert!(!is_mount_point(&dir));
    }

    #[test]
    fn rotation_state_valid() {
        let identity = WorkloadIdentity {
            spiffe_uri: test_spiffe_uri(),
            certificate_der: vec![],
            private_key_der: vec![],
            ca_chain_pem: String::new(),
            jwt_token: String::new(),
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + Duration::from_secs(3600),
            next_rotation: SystemTime::now() + Duration::from_secs(1800),
            grace_extended: false,
        };
        assert_eq!(
            rotation_state(&identity, SystemTime::now()),
            RotationState::Valid
        );
    }

    #[test]
    fn rotation_state_needs_rotation() {
        let now = SystemTime::now();
        let identity = WorkloadIdentity {
            spiffe_uri: test_spiffe_uri(),
            certificate_der: vec![],
            private_key_der: vec![],
            ca_chain_pem: String::new(),
            jwt_token: String::new(),
            issued_at: now - Duration::from_secs(2000),
            expires_at: now + Duration::from_secs(1600),
            next_rotation: now - Duration::from_secs(200), // past rotation time
            grace_extended: false,
        };
        assert_eq!(rotation_state(&identity, now), RotationState::NeedsRotation);
    }

    #[test]
    fn rotation_state_grace_period() {
        let now = SystemTime::now();
        let identity = WorkloadIdentity {
            spiffe_uri: test_spiffe_uri(),
            certificate_der: vec![],
            private_key_der: vec![],
            ca_chain_pem: String::new(),
            jwt_token: String::new(),
            issued_at: now - Duration::from_secs(4000),
            expires_at: now + Duration::from_secs(14400), // extended
            next_rotation: now - Duration::from_secs(2200),
            grace_extended: true,
        };
        assert_eq!(rotation_state(&identity, now), RotationState::GracePeriod);
    }

    #[test]
    fn rotation_state_expired() {
        let now = SystemTime::now();
        let identity = WorkloadIdentity {
            spiffe_uri: test_spiffe_uri(),
            certificate_der: vec![],
            private_key_der: vec![],
            ca_chain_pem: String::new(),
            jwt_token: String::new(),
            issued_at: now - Duration::from_secs(7200),
            expires_at: now - Duration::from_secs(100), // expired
            next_rotation: now - Duration::from_secs(5400),
            grace_extended: false,
        };
        assert_eq!(rotation_state(&identity, now), RotationState::Expired);
    }

    #[test]
    fn extend_grace_period_success() {
        let now = SystemTime::now();
        let mut identity = WorkloadIdentity {
            spiffe_uri: test_spiffe_uri(),
            certificate_der: vec![],
            private_key_der: vec![],
            ca_chain_pem: String::new(),
            jwt_token: String::new(),
            issued_at: now,
            expires_at: now + Duration::from_secs(3600),
            next_rotation: now + Duration::from_secs(1800),
            grace_extended: false,
        };

        assert!(extend_grace_period(&mut identity));
        assert!(identity.grace_extended);
        // expires_at should now be issued_at + 5 hours
        let expected = now + WORKLOAD_CERT_LIFETIME + GRACE_PERIOD_EXTENSION;
        assert_eq!(identity.expires_at, expected);
    }

    #[test]
    fn extend_grace_period_capped_at_five_hours() {
        let now = SystemTime::now();
        let max_expiry = now + WORKLOAD_CERT_LIFETIME + GRACE_PERIOD_EXTENSION;
        let mut identity = WorkloadIdentity {
            spiffe_uri: test_spiffe_uri(),
            certificate_der: vec![],
            private_key_der: vec![],
            ca_chain_pem: String::new(),
            jwt_token: String::new(),
            issued_at: now,
            expires_at: max_expiry, // already at max
            next_rotation: now + Duration::from_secs(1800),
            grace_extended: true,
        };

        // Should return false — already at max
        assert!(!extend_grace_period(&mut identity));
    }
}
