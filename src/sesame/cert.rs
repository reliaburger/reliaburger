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
    #[error("certificate is missing the code-signing extended key usage")]
    NotCodeSigning,
    #[error(
        "leaf certificate asserts the CA basic constraint; a CA must not be used as an end-entity"
    )]
    LeafIsCa,
    #[error("issuer/subject mismatch: {0}")]
    IssuerMismatch(String),
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
    check_validity_at(der, SystemTime::now())
}

/// Check that a certificate is valid at the given instant. The injected
/// clock exists so tests can assert exact validity windows (PKI6).
pub fn check_validity_at(der: &[u8], at: SystemTime) -> Result<(), CertError> {
    let (_, cert) =
        X509Certificate::from_der(der).map_err(|e| CertError::ParseFailed(format!("{e}")))?;

    let now = at
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
    // Defence in depth (O16): the leaf must not itself be a CA. CA-pinning and
    // the path-length constraint on our intermediates already prevent a CA cert
    // being accepted as an end-entity, but asserting `CA:FALSE` on the leaf
    // closes the gap explicitly rather than relying on the issuer's constraints.
    assert_leaf_not_ca(leaf_der)?;
    Ok(())
}

/// Reject a leaf that asserts the CA basic constraint (O16).
///
/// A certificate with no BasicConstraints extension, or `CA:FALSE`, is a valid
/// end-entity. Only an explicit `CA:TRUE` is refused.
fn assert_leaf_not_ca(der: &[u8]) -> Result<(), CertError> {
    let (_, cert) =
        X509Certificate::from_der(der).map_err(|e| CertError::ParseFailed(format!("{e}")))?;
    let is_ca = cert
        .basic_constraints()
        .map_err(|e| CertError::ParseFailed(format!("failed to read basic constraints: {e}")))?
        .map(|ext| ext.value.ca)
        .unwrap_or(false);
    if is_ca {
        return Err(CertError::LeafIsCa);
    }
    Ok(())
}

/// Check that a certificate carries the code-signing extended key usage.
///
/// A signing certificate that omits code-signing EKU is one the CA never
/// authorised to sign artefacts. An mTLS-only leaf (ServerAuth/ClientAuth)
/// must not double as a code signer, so image verification rejects it (IMG3).
pub fn check_code_signing_eku(der: &[u8]) -> Result<(), CertError> {
    let (_, cert) =
        X509Certificate::from_der(der).map_err(|e| CertError::ParseFailed(format!("{e}")))?;

    // `?` inside `.map(...)` won't work here: `extended_key_usage()` returns a
    // `Result<Option<...>>`, so we flatten both layers before inspecting.
    let eku = cert
        .extended_key_usage()
        .map_err(|e| CertError::ParseFailed(format!("failed to read EKU: {e}")))?
        .map(|ext| ext.value.code_signing)
        .unwrap_or(false);
    if !eku {
        return Err(CertError::NotCodeSigning);
    }
    Ok(())
}

/// Extract the URI SANs from a certificate (SPIFFE identities live here).
///
/// Returns an empty vector when the certificate has no URI SANs. The
/// caller decides whether an empty set is acceptable.
pub fn subject_uri_sans(der: &[u8]) -> Result<Vec<String>, CertError> {
    let (_, cert) =
        X509Certificate::from_der(der).map_err(|e| CertError::ParseFailed(format!("{e}")))?;
    let uris = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|ext| {
            ext.value
                .general_names
                .iter()
                .filter_map(|gn| match gn {
                    x509_parser::extensions::GeneralName::URI(u) => Some(u.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(uris)
}

/// Check that `child`'s issuer distinguished name matches `parent`'s subject
/// distinguished name. This is path-building: a valid signature over the
/// wrong parent (a colliding key, a swapped intermediate) is caught here
/// even though the cryptographic check passed.
pub fn check_issuer_binding(child_der: &[u8], parent_der: &[u8]) -> Result<(), CertError> {
    let (_, child) = X509Certificate::from_der(child_der)
        .map_err(|e| CertError::ParseFailed(format!("child: {e}")))?;
    let (_, parent) = X509Certificate::from_der(parent_der)
        .map_err(|e| CertError::ParseFailed(format!("parent: {e}")))?;

    if child.issuer() != parent.subject() {
        return Err(CertError::IssuerMismatch(format!(
            "child issuer {} does not match parent subject {}",
            child.issuer(),
            parent.subject()
        )));
    }
    Ok(())
}

/// Validate a full certificate chain `[leaf, intermediate…, root]` at a
/// given instant: every adjacent link's signature *and* issuer binding, and
/// every certificate's validity window.
///
/// Unlike [`validate_chain`] (fixed three-cert shape, `now` implicit), this
/// walks a chain of any length and injects the clock so tests can assert
/// exact validity windows (#87's pattern). The `root_der` is the trust
/// anchor: the last chain cert must chain to it.
pub fn validate_chain_at(
    chain: &[Vec<u8>],
    root_der: &[u8],
    at: SystemTime,
) -> Result<(), CertError> {
    if chain.is_empty() {
        return Err(CertError::ChainInvalid(
            "empty certificate chain".to_string(),
        ));
    }

    // Verify every adjacent link inside the presented chain.
    for pair in chain.windows(2) {
        verify_signature(&pair[0], &pair[1])?;
        check_issuer_binding(&pair[0], &pair[1])?;
    }

    // The last presented cert must chain to the trust anchor. When the caller
    // includes the root as the final element it self-chains, so only check
    // the link to the anchor when they differ.
    let last = chain.last().expect("chain is non-empty");
    if last.as_slice() != root_der {
        verify_signature(last, root_der)?;
        check_issuer_binding(last, root_der)?;
    }

    // Every certificate, including the anchor, must be within its validity
    // window at `at`.
    for cert in chain {
        check_validity_at(cert, at)?;
    }
    check_validity_at(root_der, at)?;

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

/// Extract the serial number from a DER certificate.
///
/// Reliaburger serials come from `SecurityState.next_serial`, a monotonic
/// `u64` allocator, so the big-endian fold in `parse_certificate` is lossless.
pub fn serial_from_der(der: &[u8]) -> Result<SerialNumber, CertError> {
    let parsed = parse_certificate(der)?;
    Ok(SerialNumber(parsed.serial))
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

    /// O16: a CA certificate presented as the leaf is refused, even though its
    /// signatures and validity check out.
    #[test]
    fn validate_chain_rejects_a_ca_as_the_leaf() {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        // The Node CA is a CA cert, signed by the root — a valid chain link,
        // but not a valid end-entity.
        let err = validate_chain(
            &hierarchy.node.ca.certificate_der,
            &hierarchy.root.ca.certificate_der,
            &hierarchy.root.ca.certificate_der,
        )
        .unwrap_err();
        assert!(matches!(err, CertError::LeafIsCa), "got: {err:?}");

        // A genuine leaf still passes `assert_leaf_not_ca`.
        assert!(
            assert_leaf_not_ca(
                &ca::issue_node_cert(
                    "node-01",
                    SerialNumber(10),
                    &hierarchy.node.signing_keypair,
                    &hierarchy.node.certificate_params,
                )
                .unwrap()
                .0
            )
            .is_ok()
        );
    }

    #[test]
    fn serial_from_der_reads_the_serial_rcgen_wrote() {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (cert_der, _key_der, serial) = ca::issue_node_cert(
            "node-01",
            SerialNumber(42),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();

        assert_eq!(serial, SerialNumber(42));
        assert_eq!(serial_from_der(&cert_der).unwrap(), SerialNumber(42));
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

    /// Issue a leaf under the workload CA with the given EKU set and URI SANs.
    fn issue_leaf(
        hierarchy: &ca::CaHierarchy,
        eku: &[rcgen::ExtendedKeyUsagePurpose],
        _uri: &str,
        serial: u64,
    ) -> Vec<u8> {
        // `issue_end_entity_cert` takes the CN as a DNS SAN; the code-signing
        // helper we test lives on the DER, so a DNS SAN is enough here.
        let (der, _key, _serial) = ca::issue_end_entity_cert(
            "build-signer",
            SerialNumber(serial),
            std::time::Duration::from_secs(3600),
            &[],
            eku,
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
        )
        .unwrap();
        der
    }

    #[test]
    fn code_signing_eku_present_passes() {
        use rcgen::ExtendedKeyUsagePurpose;
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let der = issue_leaf(
            &hierarchy,
            &[ExtendedKeyUsagePurpose::CodeSigning],
            "spiffe://test/ns/ci/job/build",
            201,
        );
        check_code_signing_eku(&der).unwrap();
    }

    #[test]
    fn code_signing_eku_absent_is_rejected() {
        use rcgen::ExtendedKeyUsagePurpose;
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        // A leaf with only server/client auth is not a code signer.
        let der = issue_leaf(
            &hierarchy,
            &[
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ],
            "spiffe://test/ns/ci/job/build",
            202,
        );
        assert!(matches!(
            check_code_signing_eku(&der),
            Err(CertError::NotCodeSigning)
        ));
    }

    #[test]
    fn issuer_binding_matches_for_a_real_chain() {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (leaf, _, _) = ca::issue_node_cert(
            "node-01",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        check_issuer_binding(&leaf, &hierarchy.node.ca.certificate_der).unwrap();
    }

    #[test]
    fn issuer_binding_rejects_an_unrelated_parent() {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (leaf, _, _) = ca::issue_node_cert(
            "node-01",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        // The workload CA never issued this leaf, so its subject DN is wrong.
        assert!(matches!(
            check_issuer_binding(&leaf, &hierarchy.workload.ca.certificate_der),
            Err(CertError::IssuerMismatch(_))
        ));
    }

    #[test]
    fn validate_chain_at_accepts_a_well_formed_chain() {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (leaf, _, _) = ca::issue_node_cert(
            "node-01",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        let chain = vec![leaf, hierarchy.node.ca.certificate_der.clone()];
        validate_chain_at(
            &chain,
            &hierarchy.root.ca.certificate_der,
            SystemTime::now(),
        )
        .unwrap();
    }

    #[test]
    fn validate_chain_at_rejects_a_broken_intermediate_link() {
        // Two independent hierarchies: splice the wrong intermediate in.
        let a = ca::generate_ca_hierarchy("a", b"ikm").unwrap();
        let b = ca::generate_ca_hierarchy("b", b"ikm").unwrap();
        let (leaf, _, _) = ca::issue_node_cert(
            "node-01",
            SerialNumber(10),
            &a.node.signing_keypair,
            &a.node.certificate_params,
        )
        .unwrap();
        // Leaf issued by a's node CA, but we present b's node CA as its parent.
        let chain = vec![leaf, b.node.ca.certificate_der.clone()];
        assert!(validate_chain_at(&chain, &a.root.ca.certificate_der, SystemTime::now()).is_err());
    }

    #[test]
    fn validate_chain_at_rejects_an_expired_cert() {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (leaf, _, _) = ca::issue_node_cert(
            "node-01",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        let chain = vec![leaf, hierarchy.node.ca.certificate_der.clone()];
        // Ten years hence, everything in the chain has expired.
        let future = SystemTime::now() + std::time::Duration::from_secs(10 * 365 * 24 * 3600);
        assert!(matches!(
            validate_chain_at(&chain, &hierarchy.root.ca.certificate_der, future),
            Err(CertError::Expired)
        ));
    }

    #[test]
    fn subject_uri_sans_reads_the_spiffe_uri() {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (leaf, _, _) = ca::issue_node_cert(
            "node-07",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        let uris = subject_uri_sans(&leaf).unwrap();
        assert!(
            uris.contains(&ca::node_spiffe_uri("node-07")),
            "got {uris:?}"
        );
    }
}
