//! Certificate parsing and validation.
//!
//! Uses `x509-parser` to parse DER certificates and verify chains.
//! Wraps the parsed output in Reliaburger types.

use std::time::SystemTime;

use x509_parser::prelude::*;

use super::types::{NodeCertificate, SerialNumber};

/// Errors from certificate operations.
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("failed to parse certificate: {0}")]
    ParseFailed(String),
    #[error("certificate chain validation failed: {0}")]
    ChainInvalid(String),
    #[error("certificate has expired")]
    Expired,
    #[error("certificate is not yet valid")]
    NotYetValid,
    #[error("certificate serial {serial} revoked: {reason}")]
    Revoked {
        serial: SerialNumber,
        reason: String,
    },
}

/// Parse a DER-encoded X.509 certificate and extract basic fields.
pub struct ParsedCert {
    /// The common name (CN) from the subject.
    pub common_name: String,
    /// Whether this is a CA certificate.
    pub is_ca: bool,
    /// The serial number from the certificate.
    pub serial: u64,
    /// The issuer CN.
    pub issuer_cn: String,
}

/// Parse a DER-encoded certificate and extract key fields.
pub fn parse_certificate(der: &[u8]) -> Result<ParsedCert, CertError> {
    let (_, cert) =
        X509Certificate::from_der(der).map_err(|e| CertError::ParseFailed(format!("{e}")))?;

    let common_name = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or("")
        .to_string();

    let issuer_cn = cert
        .issuer()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok())
        .unwrap_or("")
        .to_string();

    let is_ca = cert.is_ca();

    // Extract serial as u64 (safe for our serials which are small)
    let serial_bytes = cert.serial.to_bytes_be();
    let mut serial = 0u64;
    for &b in &serial_bytes {
        serial = serial.wrapping_shl(8) | b as u64;
    }

    Ok(ParsedCert {
        common_name,
        is_ca,
        serial,
        issuer_cn,
    })
}

/// Verify that a child certificate was signed by a parent certificate.
///
/// This checks the cryptographic signature. It does not check expiry
/// or revocation — those are separate checks.
pub fn verify_signature(child_der: &[u8], parent_der: &[u8]) -> Result<(), CertError> {
    let (_, child) = X509Certificate::from_der(child_der)
        .map_err(|e| CertError::ParseFailed(format!("child: {e}")))?;
    let (_, parent) = X509Certificate::from_der(parent_der)
        .map_err(|e| CertError::ParseFailed(format!("parent: {e}")))?;

    child
        .verify_signature(Some(&parent.tbs_certificate.subject_pki))
        .map_err(|e| CertError::ChainInvalid(format!("signature verification failed: {e}")))?;

    Ok(())
}

/// Check that a certificate is currently valid (not expired, not before start).
pub fn check_validity(der: &[u8]) -> Result<(), CertError> {
    let (_, cert) =
        X509Certificate::from_der(der).map_err(|e| CertError::ParseFailed(format!("{e}")))?;

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let now_secs = now.as_secs() as i64;

    let validity = &cert.validity();
    // x509-parser uses ASN1Time which wraps a chrono offset
    if now_secs < validity.not_before.timestamp() {
        return Err(CertError::NotYetValid);
    }
    if now_secs > validity.not_after.timestamp() {
        return Err(CertError::Expired);
    }

    Ok(())
}

/// Validate a full certificate chain: child → intermediate → root.
///
/// Checks both signatures and validity.
pub fn validate_chain(
    leaf_der: &[u8],
    intermediate_der: &[u8],
    root_der: &[u8],
) -> Result<(), CertError> {
    // Check leaf signed by intermediate
    verify_signature(leaf_der, intermediate_der)?;
    // Check intermediate signed by root
    verify_signature(intermediate_der, root_der)?;
    // Check all are valid
    check_validity(leaf_der)?;
    check_validity(intermediate_der)?;
    check_validity(root_der)?;
    Ok(())
}

/// Build a `NodeCertificate` from DER cert and private key.
pub fn build_node_certificate(
    node_id: &str,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
    serial: SerialNumber,
    ca_generation: u64,
) -> NodeCertificate {
    let now = SystemTime::now();
    let one_year = std::time::Duration::from_secs(365 * 24 * 3600);
    NodeCertificate {
        node_id: node_id.to_string(),
        certificate_der,
        private_key_der,
        serial,
        not_before: now,
        not_after: now + one_year,
        ca_generation,
    }
}

/// Check whether a certificate serial number has been revoked.
///
/// Returns `Ok(())` if the serial is not in the CRL, or `Err` if revoked.
pub fn check_crl(
    serial: super::types::SerialNumber,
    crl: &super::types::Crl,
) -> Result<(), CertError> {
    if let Some(entry) = crl.entries.iter().find(|e| e.serial == serial) {
        return Err(CertError::Revoked {
            serial,
            reason: entry.reason.clone(),
        });
    }
    Ok(())
}

/// Encode a DER certificate to PEM format.
pub fn der_to_pem(der: &[u8], label: &str) -> String {
    let p = ::pem::Pem::new(label, der.to_vec());
    ::pem::encode(&p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::ca;

    #[test]
    fn parse_root_ca_certificate() {
        let root = ca::generate_root_ca("test", SerialNumber(1)).unwrap();
        let parsed = parse_certificate(&root.ca.certificate_der).unwrap();
        assert!(parsed.common_name.contains("Root CA"));
        assert!(parsed.is_ca);
        assert_eq!(parsed.serial, 1);
    }

    #[test]
    fn parse_intermediate_ca_certificate() {
        let root = ca::generate_root_ca("test", SerialNumber(1)).unwrap();
        let node_ca = ca::generate_intermediate_ca(
            crate::sesame::types::CaRole::Node,
            "test",
            SerialNumber(2),
            SerialNumber(1),
            &root.signing_keypair,
            &root.certificate_params,
            b"ikm",
        )
        .unwrap();

        let parsed = parse_certificate(&node_ca.ca.certificate_der).unwrap();
        assert!(parsed.common_name.contains("Node CA"));
        assert!(parsed.is_ca);
    }

    #[test]
    fn verify_intermediate_signed_by_root() {
        let root = ca::generate_root_ca("test", SerialNumber(1)).unwrap();
        let node_ca = ca::generate_intermediate_ca(
            crate::sesame::types::CaRole::Node,
            "test",
            SerialNumber(2),
            SerialNumber(1),
            &root.signing_keypair,
            &root.certificate_params,
            b"ikm",
        )
        .unwrap();

        verify_signature(&node_ca.ca.certificate_der, &root.ca.certificate_der).unwrap();
    }

    #[test]
    fn verify_node_cert_chain() {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (cert_der, _key_der, _serial) = ca::issue_node_cert(
            "node-01",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();

        validate_chain(
            &cert_der,
            &hierarchy.node.ca.certificate_der,
            &hierarchy.root.ca.certificate_der,
        )
        .unwrap();
    }

    #[test]
    fn check_validity_on_fresh_cert() {
        let root = ca::generate_root_ca("test", SerialNumber(1)).unwrap();
        check_validity(&root.ca.certificate_der).unwrap();
    }

    #[test]
    fn der_to_pem_round_trip() {
        let root = ca::generate_root_ca("test", SerialNumber(1)).unwrap();
        let pem_str = der_to_pem(&root.ca.certificate_der, "CERTIFICATE");
        assert!(pem_str.contains("-----BEGIN CERTIFICATE-----"));
        assert!(pem_str.contains("-----END CERTIFICATE-----"));

        // Parse back
        let parsed = ::pem::parse(pem_str).unwrap();
        assert_eq!(parsed.contents(), root.ca.certificate_der);
    }
}
