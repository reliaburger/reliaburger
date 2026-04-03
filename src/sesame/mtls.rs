//! Mutual TLS configuration for inter-node communication.
//!
//! Builds rustls `ClientConfig` and `ServerConfig` that require both
//! sides to present valid node certificates signed by the Node CA.

use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

/// Errors from mTLS operations.
#[derive(Debug, thiserror::Error)]
pub enum MtlsError {
    #[error("failed to build TLS config: {0}")]
    ConfigFailed(String),
    #[error("invalid certificate: {0}")]
    InvalidCert(String),
    #[error("invalid private key: {0}")]
    InvalidKey(String),
}

/// Build a rustls `ServerConfig` that requires client certificates.
///
/// The server presents its own node certificate and verifies that
/// connecting clients present a certificate signed by the Root CA.
pub fn build_mtls_server_config(
    node_cert_der: &[u8],
    node_key_der: &[u8],
    root_ca_der: &[u8],
) -> Result<Arc<ServerConfig>, MtlsError> {
    // Install the ring crypto provider (idempotent)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build the trust store with the Root CA
    let mut root_store = RootCertStore::empty();
    root_store
        .add(CertificateDer::from(root_ca_der.to_vec()))
        .map_err(|e| MtlsError::InvalidCert(format!("root CA: {e}")))?;

    // Build the client verifier (requires client certs signed by root CA)
    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|e| MtlsError::ConfigFailed(format!("client verifier: {e}")))?;

    let cert = CertificateDer::from(node_cert_der.to_vec());
    let key = PrivateKeyDer::try_from(node_key_der.to_vec())
        .map_err(|e| MtlsError::InvalidKey(e.to_string()))?;

    let config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![cert], key)
        .map_err(|e| MtlsError::ConfigFailed(e.to_string()))?;

    Ok(Arc::new(config))
}

/// Build a rustls `ClientConfig` for connecting to cluster peers.
///
/// The client presents its own node certificate and verifies that
/// the server presents a certificate signed by the Root CA.
pub fn build_mtls_client_config(
    node_cert_der: &[u8],
    node_key_der: &[u8],
    root_ca_der: &[u8],
) -> Result<Arc<ClientConfig>, MtlsError> {
    // Install the ring crypto provider (idempotent)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Build the trust store with the Root CA
    let mut root_store = RootCertStore::empty();
    root_store
        .add(CertificateDer::from(root_ca_der.to_vec()))
        .map_err(|e| MtlsError::InvalidCert(format!("root CA: {e}")))?;

    let cert = CertificateDer::from(node_cert_der.to_vec());
    let key = PrivateKeyDer::try_from(node_key_der.to_vec())
        .map_err(|e| MtlsError::InvalidKey(e.to_string()))?;

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| MtlsError::ConfigFailed(e.to_string()))?;

    Ok(Arc::new(config))
}

/// Build a server-auth-only TLS config (for join flow before mTLS is established).
///
/// The connecting node verifies the server's certificate against the Root CA
/// but does not present a client certificate (it doesn't have one yet).
pub fn build_server_auth_only_client_config(
    root_ca_der: &[u8],
) -> Result<Arc<ClientConfig>, MtlsError> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut root_store = RootCertStore::empty();
    root_store
        .add(CertificateDer::from(root_ca_der.to_vec()))
        .map_err(|e| MtlsError::InvalidCert(format!("root CA: {e}")))?;

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    Ok(Arc::new(config))
}

/// HMAC-SHA256 authentication for gossip UDP messages.
///
/// Since UDP can't use TLS, we authenticate gossip messages with a
/// shared HMAC key derived from the cluster's root CA certificate.
pub mod gossip_hmac {
    use ring::hmac;

    /// Derive an HMAC key from the root CA certificate.
    ///
    /// All cluster nodes share the root CA cert, so they all derive
    /// the same HMAC key. This provides authentication (proving the
    /// sender is a cluster member) but not confidentiality.
    pub fn derive_gossip_key(root_ca_der: &[u8]) -> hmac::Key {
        // Use the root CA cert bytes as input to HKDF, then use the
        // output as the HMAC key. This is a simplified approach —
        // a production system would use a separate shared secret.
        let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, b"reliaburger-gossip-hmac-v1");
        let prk = salt.extract(root_ca_der);

        let mut key_bytes = [0u8; 32];
        let info = [b"gossip-hmac" as &[u8]];
        let okm = prk.expand(&info, HkdfLen).expect("HKDF expand");
        okm.fill(&mut key_bytes).expect("HKDF fill");

        hmac::Key::new(hmac::HMAC_SHA256, &key_bytes)
    }

    struct HkdfLen;
    impl ring::hkdf::KeyType for HkdfLen {
        fn len(&self) -> usize {
            32
        }
    }

    /// Sign a gossip message payload.
    pub fn sign(key: &hmac::Key, payload: &[u8]) -> Vec<u8> {
        let tag = hmac::sign(key, payload);
        tag.as_ref().to_vec()
    }

    /// Verify a gossip message's HMAC tag.
    pub fn verify(key: &hmac::Key, payload: &[u8], tag: &[u8]) -> bool {
        hmac::verify(key, payload, tag).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::ca;
    use crate::sesame::types::SerialNumber;

    fn test_hierarchy() -> (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>) {
        let hierarchy = ca::generate_ca_hierarchy("test", b"ikm").unwrap();
        let (cert_der, key_der, _) = ca::issue_node_cert(
            "node-01",
            SerialNumber(10),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        (
            cert_der,
            key_der,
            hierarchy.root.ca.certificate_der.clone(),
            hierarchy.node.ca.certificate_der.clone(),
        )
    }

    #[test]
    fn build_mtls_server_config_succeeds() {
        let (cert, key, root_ca, _) = test_hierarchy();
        let _config = build_mtls_server_config(&cert, &key, &root_ca).unwrap();
        // If we get here without error, the mTLS config was built successfully
        // with a client cert verifier that requires node certs signed by the root CA.
    }

    #[test]
    fn build_mtls_client_config_succeeds() {
        let (cert, key, root_ca, _) = test_hierarchy();
        let _config = build_mtls_client_config(&cert, &key, &root_ca).unwrap();
    }

    #[test]
    fn build_server_auth_only_config_succeeds() {
        let (_, _, root_ca, _) = test_hierarchy();
        let _config = build_server_auth_only_client_config(&root_ca).unwrap();
    }

    #[test]
    fn mtls_config_rejects_empty_cert() {
        let result = build_mtls_server_config(&[], &[], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn gossip_hmac_sign_verify_round_trip() {
        let (_, _, root_ca, _) = test_hierarchy();
        let key = gossip_hmac::derive_gossip_key(&root_ca);

        let payload = b"gossip ping message";
        let tag = gossip_hmac::sign(&key, payload);
        assert!(gossip_hmac::verify(&key, payload, &tag));
    }

    #[test]
    fn gossip_hmac_wrong_payload_fails() {
        let (_, _, root_ca, _) = test_hierarchy();
        let key = gossip_hmac::derive_gossip_key(&root_ca);

        let payload = b"original message";
        let tag = gossip_hmac::sign(&key, payload);
        assert!(!gossip_hmac::verify(&key, b"tampered message", &tag));
    }

    #[test]
    fn gossip_hmac_wrong_key_fails() {
        let hierarchy1 = ca::generate_ca_hierarchy("cluster-a", b"ikm-a").unwrap();
        let hierarchy2 = ca::generate_ca_hierarchy("cluster-b", b"ikm-b").unwrap();

        let key1 = gossip_hmac::derive_gossip_key(&hierarchy1.root.ca.certificate_der);
        let key2 = gossip_hmac::derive_gossip_key(&hierarchy2.root.ca.certificate_der);

        let payload = b"message from cluster A";
        let tag = gossip_hmac::sign(&key1, payload);
        // Cluster B's key should not verify cluster A's tag
        assert!(!gossip_hmac::verify(&key2, payload, &tag));
    }

    #[test]
    fn gossip_hmac_deterministic_key_derivation() {
        let (_, _, root_ca, _) = test_hierarchy();
        let key1 = gossip_hmac::derive_gossip_key(&root_ca);
        let key2 = gossip_hmac::derive_gossip_key(&root_ca);

        let payload = b"test";
        let tag1 = gossip_hmac::sign(&key1, payload);
        // Same root CA produces same key, so both should verify
        assert!(gossip_hmac::verify(&key2, payload, &tag1));
    }
}
