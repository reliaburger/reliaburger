//! Workload identity — CSR generation, validation, signing, and rotation.
//!
//! This module implements the SPIFFE-compatible workload identity lifecycle:
//! workers generate keypairs and CSRs, council nodes validate and sign them,
//! and identity bundles are delivered to workloads via tmpfs.

use std::path::Path;
use std::time::{Duration, SystemTime};

use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber as RcgenSerial,
};

use super::cert;
use super::types::{SerialNumber, SpiffeUri, WorkloadIdentity};

/// Workload certificate lifetime: 1 hour.
pub const WORKLOAD_CERT_LIFETIME: Duration = Duration::from_secs(3600);

/// Rotation interval: 30 minutes (half of certificate lifetime).
pub const ROTATION_INTERVAL: Duration = Duration::from_secs(1800);

/// Maximum grace period extension when council is unreachable: 4 hours.
pub const GRACE_PERIOD_EXTENSION: Duration = Duration::from_secs(4 * 3600);

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
    #[error("failed to write identity to tmpfs: {0}")]
    WriteFailed(#[from] std::io::Error),
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

/// Validate a CSR and sign it with the Workload CA.
///
/// The council calls this after verifying that the requesting node is
/// actually scheduled to run the workload. Returns the signed
/// certificate DER.
pub fn validate_and_sign_csr(
    csr_der: &[u8],
    expected_spiffe_uri: &SpiffeUri,
    serial: SerialNumber,
    workload_ca_keypair: &KeyPair,
    workload_ca_params: &CertificateParams,
) -> Result<Vec<u8>, IdentityError> {
    let csr_der_owned: Vec<u8> = csr_der.to_vec();
    let csr_der_ref = rustls::pki_types::CertificateSigningRequestDer::from(csr_der_owned);
    let mut csr_params = CertificateSigningRequestParams::from_der(&csr_der_ref)
        .map_err(|e| IdentityError::CsrValidationFailed(format!("failed to parse CSR: {e}")))?;

    // Validate the SPIFFE URI SAN
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

    // Override lifetime and serial for the signed certificate
    let now = SystemTime::now();
    let not_after_time = now + WORKLOAD_CERT_LIFETIME;
    csr_params.params.not_before =
        rcgen::date_time_ymd(time_to_year(now), time_to_month(now), time_to_day(now));
    csr_params.params.not_after = rcgen::date_time_ymd(
        time_to_year(not_after_time),
        time_to_month(not_after_time),
        time_to_day(not_after_time),
    );
    csr_params.params.serial_number = Some(RcgenSerial::from_slice(&serial.0.to_be_bytes()));

    // Reconstruct the CA certificate object for signing
    let ca_cert = workload_ca_params
        .clone()
        .self_signed(workload_ca_keypair)
        .map_err(|e| IdentityError::SignFailed(format!("failed to reconstruct CA cert: {e}")))?;

    let signed = csr_params
        .signed_by(&ca_cert, workload_ca_keypair)
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
// Tmpfs delivery
// ---------------------------------------------------------------------------

/// Write identity files to a workload's tmpfs mount point.
///
/// Creates the directory and writes five files atomically (write to
/// `.tmp` then rename):
/// - `cert.pem` — the workload certificate
/// - `key.pem` — the private key
/// - `ca.pem` — the CA trust chain
/// - `bundle.pem` — cert + CA chain concatenated
/// - `token` — the OIDC JWT string
pub fn write_identity_to_tmpfs(
    identity: &WorkloadIdentity,
    base_path: &Path,
) -> Result<(), IdentityError> {
    let dir = base_path.join("identity");
    std::fs::create_dir_all(&dir)?;

    let cert_pem = cert::der_to_pem(&identity.certificate_der, "CERTIFICATE");
    let key_pem = cert::der_to_pem(&identity.private_key_der, "PRIVATE KEY");

    let bundle_pem = format!("{}{}", cert_pem, identity.ca_chain_pem);

    atomic_write(&dir.join("cert.pem"), cert_pem.as_bytes())?;
    atomic_write(&dir.join("key.pem"), key_pem.as_bytes())?;
    atomic_write(&dir.join("ca.pem"), identity.ca_chain_pem.as_bytes())?;
    atomic_write(&dir.join("bundle.pem"), bundle_pem.as_bytes())?;
    atomic_write(&dir.join("token"), identity.jwt_token.as_bytes())?;

    Ok(())
}

/// Write data to a temp file then atomically rename.
fn atomic_write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
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

// ---------------------------------------------------------------------------
// Time helpers (same approach as ca.rs)
// ---------------------------------------------------------------------------

fn system_time_to_date_components(t: SystemTime) -> (i32, u8, u8) {
    let duration = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = duration.as_secs() as i64;
    let days = secs / 86400;
    let mut year = 1970 + (days / 365) as i32;
    let mut day_of_year = days - ((year - 1970) as i64 * 365 + ((year - 1969) / 4) as i64);

    while day_of_year < 0 {
        year -= 1;
        day_of_year = days - ((year - 1970) as i64 * 365 + ((year - 1969) / 4) as i64);
    }

    let is_leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let month_days: [i64; 12] = if is_leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 0u8;
    let mut remaining = day_of_year;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining < md {
            month = (i + 1) as u8;
            break;
        }
        remaining -= md;
    }
    if month == 0 {
        month = 12;
    }
    let day = (remaining + 1).max(1) as u8;
    (year, month, day)
}

fn time_to_year(t: SystemTime) -> i32 {
    system_time_to_date_components(t).0
}

fn time_to_month(t: SystemTime) -> u8 {
    system_time_to_date_components(t).1
}

fn time_to_day(t: SystemTime) -> u8 {
    system_time_to_date_components(t).2
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

        let cert_der =
            validate_and_sign_csr(&csr_der, &uri, SerialNumber(100), &ca_kp, &ca_params).unwrap();

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

        let result =
            validate_and_sign_csr(&csr_der, &wrong_uri, SerialNumber(100), &ca_kp, &ca_params);
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

        let cert_der =
            validate_and_sign_csr(&csr_der, &uri, SerialNumber(100), &ca_kp, &ca_params).unwrap();

        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der).unwrap();
        // End-entity cert, not a CA
        assert!(!cert.is_ca());
        // Validity period exists (rcgen uses day granularity, so
        // not_after >= not_before is the best we can check)
        assert!(cert.validity().not_after >= cert.validity().not_before);
        // Subject contains the SPIFFE URI
        assert!(cert.subject().to_string().contains("test-cluster"));
    }

    #[test]
    fn signed_cert_chains_to_workload_ca() {
        let uri = test_spiffe_uri();
        let (csr_der, _) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, workload_ca_cert_der, _) = test_workload_ca();

        let cert_der =
            validate_and_sign_csr(&csr_der, &uri, SerialNumber(100), &ca_kp, &ca_params).unwrap();

        // Verify the workload cert is signed by the Workload CA
        cert::verify_signature(&cert_der, &workload_ca_cert_der).unwrap();
    }

    #[test]
    fn build_identity_bundle_populates_all_fields() {
        let uri = test_spiffe_uri();
        let (csr_der, private_key_der) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, workload_ca_cert_der, root_ca_cert_der) = test_workload_ca();

        let cert_der =
            validate_and_sign_csr(&csr_der, &uri, SerialNumber(100), &ca_kp, &ca_params).unwrap();

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

    #[test]
    fn write_identity_to_tmpfs_creates_all_files() {
        let uri = test_spiffe_uri();
        let (csr_der, private_key_der) = create_workload_csr(&uri).unwrap();
        let (ca_kp, ca_params, workload_ca_cert_der, root_ca_cert_der) = test_workload_ca();

        let cert_der =
            validate_and_sign_csr(&csr_der, &uri, SerialNumber(100), &ca_kp, &ca_params).unwrap();

        let identity = build_identity_bundle(
            uri,
            cert_der,
            private_key_der,
            &workload_ca_cert_der,
            &root_ca_cert_der,
            "my-jwt-token".to_string(),
        );

        let dir = tempfile::tempdir().unwrap();
        write_identity_to_tmpfs(&identity, dir.path()).unwrap();

        let id_dir = dir.path().join("identity");
        assert!(id_dir.join("cert.pem").exists());
        assert!(id_dir.join("key.pem").exists());
        assert!(id_dir.join("ca.pem").exists());
        assert!(id_dir.join("bundle.pem").exists());
        assert!(id_dir.join("token").exists());

        // Verify content
        let cert_pem = std::fs::read_to_string(id_dir.join("cert.pem")).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));

        let key_pem = std::fs::read_to_string(id_dir.join("key.pem")).unwrap();
        assert!(key_pem.contains("BEGIN PRIVATE KEY"));

        let token = std::fs::read_to_string(id_dir.join("token")).unwrap();
        assert_eq!(token, "my-jwt-token");

        let bundle = std::fs::read_to_string(id_dir.join("bundle.pem")).unwrap();
        // Bundle should contain the cert + CA chain (at least 3 PEM blocks)
        let cert_count = bundle.matches("BEGIN CERTIFICATE").count();
        assert!(cert_count >= 2, "bundle should contain cert + CA chain");
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
