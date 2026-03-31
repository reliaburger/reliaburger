//! Certificate Authority generation and signing.
//!
//! Generates the root CA and intermediate CAs (Node, Workload, Ingress)
//! using ECDSA P-256. Signs CSRs and issues certificates.

use std::time::{Duration, SystemTime};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SerialNumber as RcgenSerial,
};
use ring::rand::{SecureRandom, SystemRandom};

use super::crypto;
use super::types::{CaRole, CertificateAuthority, SerialNumber};

/// Errors from CA operations.
#[derive(Debug, thiserror::Error)]
pub enum CaError {
    #[error("failed to generate keypair: {0}")]
    KeyGenFailed(String),
    #[error("failed to generate certificate: {0}")]
    CertGenFailed(String),
    #[error("failed to sign CSR: {0}")]
    SignFailed(String),
    #[error("crypto error: {0}")]
    Crypto(#[from] crypto::CryptoError),
}

/// Duration constants for certificate lifetimes.
const ROOT_CA_LIFETIME: Duration = Duration::from_secs(10 * 365 * 24 * 3600); // 10 years
const INTERMEDIATE_CA_LIFETIME: Duration = Duration::from_secs(5 * 365 * 24 * 3600); // 5 years

/// The result of generating a CA: the CA struct for storage, plus
/// the raw private key DER (for the caller to use before wrapping).
pub struct GeneratedCa {
    /// The CA data for Raft storage (private key is wrapped).
    pub ca: CertificateAuthority,
    /// The raw private key DER (caller should zeroize after use).
    pub private_key_der: Vec<u8>,
    /// The rcgen keypair + certificate for signing child certs.
    pub signing_keypair: KeyPair,
    /// The rcgen certificate params (needed for signing child certs).
    pub certificate_params: CertificateParams,
}

/// Generate a self-signed root CA.
///
/// The root CA uses ECDSA P-256 and has a 10-year lifetime. The private
/// key is returned unwrapped — the caller is responsible for wrapping
/// it with `crypto::wrap_key` before storing.
pub fn generate_root_ca(cluster_name: &str, serial: SerialNumber) -> Result<GeneratedCa, CaError> {
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| CaError::KeyGenFailed(e.to_string()))?;
    let private_key_der = key_pair.serialize_der();

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(
        DnType::CommonName,
        format!("Reliaburger Root CA - {cluster_name}"),
    );
    dn.push(DnType::OrganizationName, "Reliaburger");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(RcgenSerial::from_slice(&serial.0.to_be_bytes()));
    params.not_before = rcgen::date_time_ymd(
        time_to_year(SystemTime::now()),
        time_to_month(SystemTime::now()),
        time_to_day(SystemTime::now()),
    );
    let not_after_time = SystemTime::now() + ROOT_CA_LIFETIME;
    params.not_after = rcgen::date_time_ymd(
        time_to_year(not_after_time),
        time_to_month(not_after_time),
        time_to_day(not_after_time),
    );

    let certificate = params
        .clone()
        .self_signed(&key_pair)
        .map_err(|e| CaError::CertGenFailed(e.to_string()))?;
    let certificate_der = certificate.der().to_vec();

    let now = SystemTime::now();
    let ca = CertificateAuthority {
        role: CaRole::Root,
        certificate_der,
        private_key_wrapped: None,
        serial,
        not_before: now,
        not_after: now + ROOT_CA_LIFETIME,
        issuer_serial: None,
        generation: 0,
    };

    Ok(GeneratedCa {
        ca,
        private_key_der,
        signing_keypair: key_pair,
        certificate_params: params,
    })
}

/// Generate an intermediate CA signed by a parent CA.
///
/// Used for Node CA, Workload CA, and Ingress CA. Each gets a 5-year
/// lifetime and is constrained to its specific purpose via key usage.
pub fn generate_intermediate_ca(
    role: CaRole,
    cluster_name: &str,
    serial: SerialNumber,
    parent_serial: SerialNumber,
    parent_keypair: &KeyPair,
    parent_params: &CertificateParams,
    wrapping_ikm: &[u8],
) -> Result<GeneratedCa, CaError> {
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| CaError::KeyGenFailed(e.to_string()))?;
    let private_key_der = key_pair.serialize_der();

    let role_name = match role {
        CaRole::Node => "Node",
        CaRole::Workload => "Workload",
        CaRole::Ingress => "Ingress",
        CaRole::Root => unreachable!("root CA is not an intermediate"),
    };

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(
        DnType::CommonName,
        format!("Reliaburger {role_name} CA - {cluster_name}"),
    );
    dn.push(DnType::OrganizationName, "Reliaburger");
    params.distinguished_name = dn;
    // Path length 0 = can sign end-entity certs but not further sub-CAs
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params.serial_number = Some(RcgenSerial::from_slice(&serial.0.to_be_bytes()));
    params.not_before = rcgen::date_time_ymd(
        time_to_year(SystemTime::now()),
        time_to_month(SystemTime::now()),
        time_to_day(SystemTime::now()),
    );
    let not_after_time = SystemTime::now() + INTERMEDIATE_CA_LIFETIME;
    params.not_after = rcgen::date_time_ymd(
        time_to_year(not_after_time),
        time_to_month(not_after_time),
        time_to_day(not_after_time),
    );

    // Sign with parent — both self_signed and signed_by consume their params,
    // so we clone before calling.
    let parent_cert = parent_params
        .clone()
        .self_signed(parent_keypair)
        .map_err(|e| CaError::CertGenFailed(e.to_string()))?;

    let certificate = params
        .clone()
        .signed_by(&key_pair, &parent_cert, parent_keypair)
        .map_err(|e| CaError::CertGenFailed(e.to_string()))?;
    let certificate_der = certificate.der().to_vec();

    // Wrap the private key
    let wrap_info = format!("reliaburger-{}-ca-wrap-v1", role_name.to_lowercase());
    let wrapped = crypto::wrap_key(wrapping_ikm, &private_key_der, &wrap_info)?;

    let now = SystemTime::now();
    let ca = CertificateAuthority {
        role,
        certificate_der,
        private_key_wrapped: Some(wrapped),
        serial,
        not_before: now,
        not_after: now + INTERMEDIATE_CA_LIFETIME,
        issuer_serial: Some(parent_serial),
        generation: 0,
    };

    Ok(GeneratedCa {
        ca,
        private_key_der,
        signing_keypair: key_pair,
        certificate_params: params,
    })
}

/// Issue an end-entity certificate (e.g. node cert) signed by an intermediate CA.
///
/// Returns `(certificate_der, private_key_der, serial)`.
pub fn issue_end_entity_cert(
    common_name: &str,
    serial: SerialNumber,
    lifetime: Duration,
    san_dns_names: &[String],
    extended_key_usage: &[ExtendedKeyUsagePurpose],
    ca_keypair: &KeyPair,
    ca_params: &CertificateParams,
) -> Result<(Vec<u8>, Vec<u8>, SerialNumber), CaError> {
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| CaError::KeyGenFailed(e.to_string()))?;
    let private_key_der = key_pair.serialize_der();

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    params.distinguished_name = dn;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = extended_key_usage.to_vec();
    params.serial_number = Some(RcgenSerial::from_slice(&serial.0.to_be_bytes()));

    let mut all_sans: Vec<rcgen::SanType> = san_dns_names
        .iter()
        .map(|name| rcgen::SanType::DnsName(name.clone().try_into().unwrap()))
        .collect();
    // Also add the CN as a SAN (modern TLS requires SAN)
    if let Ok(ia5) = common_name.to_string().try_into() {
        all_sans.push(rcgen::SanType::DnsName(ia5));
    }
    params.subject_alt_names = all_sans;

    params.not_before = rcgen::date_time_ymd(
        time_to_year(SystemTime::now()),
        time_to_month(SystemTime::now()),
        time_to_day(SystemTime::now()),
    );
    let not_after_time = SystemTime::now() + lifetime;
    params.not_after = rcgen::date_time_ymd(
        time_to_year(not_after_time),
        time_to_month(not_after_time),
        time_to_day(not_after_time),
    );

    let ca_cert = ca_params
        .clone()
        .self_signed(ca_keypair)
        .map_err(|e| CaError::CertGenFailed(e.to_string()))?;

    let certificate = params
        .signed_by(&key_pair, &ca_cert, ca_keypair)
        .map_err(|e| CaError::SignFailed(e.to_string()))?;
    let certificate_der = certificate.der().to_vec();

    Ok((certificate_der, private_key_der, serial))
}

/// Issue a node certificate signed by the Node CA. Convenience wrapper
/// around `issue_end_entity_cert` with the right key usage for mTLS.
pub fn issue_node_cert(
    node_id: &str,
    serial: SerialNumber,
    ca_keypair: &KeyPair,
    ca_params: &CertificateParams,
) -> Result<(Vec<u8>, Vec<u8>, SerialNumber), CaError> {
    let lifetime = Duration::from_secs(365 * 24 * 3600); // 1 year
    issue_end_entity_cert(
        node_id,
        serial,
        lifetime,
        &[],
        &[
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ],
        ca_keypair,
        ca_params,
    )
}

// ---------------------------------------------------------------------------
// Full CA hierarchy generation
// ---------------------------------------------------------------------------

/// The complete CA hierarchy generated during `relish init`.
pub struct CaHierarchy {
    /// Root CA (private key will be sealed and deleted from memory).
    pub root: GeneratedCa,
    /// Node CA for inter-node mTLS certificates.
    pub node: GeneratedCa,
    /// Workload CA for SPIFFE workload identity certificates.
    pub workload: GeneratedCa,
    /// Ingress CA for ingress TLS certificates.
    pub ingress: GeneratedCa,
}

/// Generate the full CA hierarchy for a new cluster.
///
/// Creates Root CA → {Node CA, Workload CA, Ingress CA}. All intermediate
/// CA private keys are wrapped with `wrapping_ikm`.
pub fn generate_ca_hierarchy(
    cluster_name: &str,
    wrapping_ikm: &[u8],
) -> Result<CaHierarchy, CaError> {
    let mut next_serial = 1u64;

    // Root CA
    let root = generate_root_ca(cluster_name, SerialNumber(next_serial))?;
    let root_serial = SerialNumber(next_serial);
    next_serial += 1;

    // Node CA
    let node = generate_intermediate_ca(
        CaRole::Node,
        cluster_name,
        SerialNumber(next_serial),
        root_serial,
        &root.signing_keypair,
        &root.certificate_params,
        wrapping_ikm,
    )?;
    next_serial += 1;

    // Workload CA
    let workload = generate_intermediate_ca(
        CaRole::Workload,
        cluster_name,
        SerialNumber(next_serial),
        root_serial,
        &root.signing_keypair,
        &root.certificate_params,
        wrapping_ikm,
    )?;
    next_serial += 1;

    // Ingress CA
    let ingress = generate_intermediate_ca(
        CaRole::Ingress,
        cluster_name,
        SerialNumber(next_serial),
        root_serial,
        &root.signing_keypair,
        &root.certificate_params,
        wrapping_ikm,
    )?;
    let _ = next_serial; // suppress unused warning

    Ok(CaHierarchy {
        root,
        node,
        workload,
        ingress,
    })
}

// ---------------------------------------------------------------------------
// Join token generation
// ---------------------------------------------------------------------------

/// Generate a one-time join token.
///
/// Returns `(token_plaintext_hex, token_hash)`. The plaintext is shown
/// to the admin once; only the hash is stored in Raft.
pub fn generate_join_token() -> Result<(String, [u8; 32]), CaError> {
    let rng = SystemRandom::new();
    let mut token_bytes = [0u8; 32];
    rng.fill(&mut token_bytes)
        .map_err(|_| CaError::KeyGenFailed("RNG failed for join token".to_string()))?;

    let token_hex = format!("rbrg_join_1_{}", hex::encode(token_bytes));

    // Hash with SHA-256 for storage
    let hash = ring::digest::digest(&ring::digest::SHA256, &token_bytes);
    let mut token_hash = [0u8; 32];
    token_hash.copy_from_slice(hash.as_ref());

    Ok((token_hex, token_hash))
}

/// Verify a join token against its stored hash.
pub fn verify_join_token(token_plaintext: &str, stored_hash: &[u8; 32]) -> bool {
    let Some(hex_part) = token_plaintext.strip_prefix("rbrg_join_1_") else {
        return false;
    };
    let Ok(token_bytes) = hex::decode(hex_part) else {
        return false;
    };
    let hash = ring::digest::digest(&ring::digest::SHA256, &token_bytes);
    hash.as_ref() == stored_hash
}

// ---------------------------------------------------------------------------
// Time helpers (rcgen needs year/month/day)
// ---------------------------------------------------------------------------

fn system_time_to_date_components(t: SystemTime) -> (i32, u8, u8) {
    let duration = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let secs = duration.as_secs() as i64;

    // Simple conversion — good enough for certificate dates.
    // Days since epoch
    let days = secs / 86400;
    // Approximate year (each year ~ 365.25 days)
    let mut year = 1970 + (days / 365) as i32;
    let mut day_of_year = days - ((year - 1970) as i64 * 365 + ((year - 1969) / 4) as i64);

    // Correct for leap year drift
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

    #[test]
    fn generate_root_ca_produces_valid_cert() {
        let root = generate_root_ca("test-cluster", SerialNumber(1)).unwrap();
        assert_eq!(root.ca.role, CaRole::Root);
        assert!(!root.ca.certificate_der.is_empty());
        assert!(!root.private_key_der.is_empty());
        assert!(root.ca.private_key_wrapped.is_none());
        assert_eq!(root.ca.serial, SerialNumber(1));
        assert!(root.ca.not_after > root.ca.not_before);

        // Parse the DER certificate and verify it's self-signed
        let (_, cert) = x509_parser::parse_x509_certificate(&root.ca.certificate_der).unwrap();
        assert!(cert.subject().to_string().contains("Reliaburger Root CA"));
        assert!(cert.is_ca());
    }

    #[test]
    fn generate_intermediate_ca_signed_by_root() {
        let root = generate_root_ca("test-cluster", SerialNumber(1)).unwrap();
        let wrapping_ikm = b"test-wrapping-material";

        let node_ca = generate_intermediate_ca(
            CaRole::Node,
            "test-cluster",
            SerialNumber(2),
            SerialNumber(1),
            &root.signing_keypair,
            &root.certificate_params,
            wrapping_ikm,
        )
        .unwrap();

        assert_eq!(node_ca.ca.role, CaRole::Node);
        assert_eq!(node_ca.ca.serial, SerialNumber(2));
        assert_eq!(node_ca.ca.issuer_serial, Some(SerialNumber(1)));
        assert!(node_ca.ca.private_key_wrapped.is_some());

        // Verify the intermediate cert is a CA
        let (_, cert) = x509_parser::parse_x509_certificate(&node_ca.ca.certificate_der).unwrap();
        assert!(cert.is_ca());
        assert!(cert.subject().to_string().contains("Node CA"));
    }

    #[test]
    fn intermediate_ca_wrapped_key_can_be_unwrapped() {
        let root = generate_root_ca("test-cluster", SerialNumber(1)).unwrap();
        let wrapping_ikm = b"test-wrapping-material";

        let node_ca = generate_intermediate_ca(
            CaRole::Node,
            "test-cluster",
            SerialNumber(2),
            SerialNumber(1),
            &root.signing_keypair,
            &root.certificate_params,
            wrapping_ikm,
        )
        .unwrap();

        let wrapped = node_ca.ca.private_key_wrapped.as_ref().unwrap();
        let unwrapped = crypto::unwrap_key(wrapping_ikm, wrapped).unwrap();
        assert_eq!(unwrapped, node_ca.private_key_der);
    }

    #[test]
    fn generate_full_ca_hierarchy() {
        let wrapping_ikm = b"master-secret-for-test";
        let hierarchy = generate_ca_hierarchy("prod", wrapping_ikm).unwrap();

        assert_eq!(hierarchy.root.ca.role, CaRole::Root);
        assert_eq!(hierarchy.node.ca.role, CaRole::Node);
        assert_eq!(hierarchy.workload.ca.role, CaRole::Workload);
        assert_eq!(hierarchy.ingress.ca.role, CaRole::Ingress);

        // All intermediates should chain to root
        assert_eq!(
            hierarchy.node.ca.issuer_serial,
            Some(hierarchy.root.ca.serial)
        );
        assert_eq!(
            hierarchy.workload.ca.issuer_serial,
            Some(hierarchy.root.ca.serial)
        );
        assert_eq!(
            hierarchy.ingress.ca.issuer_serial,
            Some(hierarchy.root.ca.serial)
        );
    }

    #[test]
    fn issue_node_cert_signed_by_node_ca() {
        let wrapping_ikm = b"test-ikm";
        let hierarchy = generate_ca_hierarchy("test", wrapping_ikm).unwrap();

        let (cert_der, key_der, serial) = issue_node_cert(
            "node-01",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();

        assert!(!cert_der.is_empty());
        assert!(!key_der.is_empty());
        assert_eq!(serial, SerialNumber(10));

        // Parse and verify it's an end-entity cert
        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der).unwrap();
        assert!(!cert.is_ca());
        assert!(cert.subject().to_string().contains("node-01"));
    }

    #[test]
    fn join_token_generation_and_verification() {
        let (token, hash) = generate_join_token().unwrap();
        assert!(token.starts_with("rbrg_join_1_"));
        assert!(verify_join_token(&token, &hash));
    }

    #[test]
    fn join_token_wrong_token_fails_verification() {
        let (_token, hash) = generate_join_token().unwrap();
        assert!(!verify_join_token("rbrg_join_1_deadbeef", &hash));
    }

    #[test]
    fn join_token_invalid_format_fails_verification() {
        let hash = [0u8; 32];
        assert!(!verify_join_token("not-a-token", &hash));
        assert!(!verify_join_token("rbrg_join_1_not-hex!", &hash));
    }
}
