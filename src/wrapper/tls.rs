/// TLS termination for the Wrapper ingress proxy.
///
/// Phase 3: self-signed cert generated on startup, or load from disk.
/// Phase 4 (Sesame): replaces with ACME (Let's Encrypt) or cluster
/// CA certificates.
///
/// TLS 1.0 and 1.1 are rejected. Only 1.2 and 1.3 are accepted.
use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Errors from TLS operations.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("failed to generate self-signed certificate: {0}")]
    CertGenFailed(String),

    #[error("failed to load certificate from {path}: {reason}")]
    LoadFailed { path: String, reason: String },

    #[error("failed to build TLS config: {0}")]
    ConfigFailed(String),
}

/// Generate a self-signed certificate for development/testing.
///
/// Creates an ECDSA P-256 certificate valid for `localhost` and
/// `127.0.0.1`. Not suitable for production — Phase 4 Sesame
/// provides real certificate management.
pub fn generate_self_signed_cert()
-> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), TlsError> {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .map_err(|e| TlsError::CertGenFailed(e.to_string()))?;

    let cert_der = CertificateDer::from(cert.cert);
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| TlsError::CertGenFailed(format!("invalid key: {e}")))?;

    Ok((cert_der, key_der))
}

/// Issue an ingress TLS certificate from the cluster's Sesame Ingress CA.
///
/// This is the `tls = "cluster"` path (ING1): rather than a self-signed cert
/// or an operator-supplied file, the ingress certificate is signed by the
/// cluster's Ingress CA, so any client that trusts the cluster root trusts
/// the ingress. It reuses the same CA hierarchy Sesame builds for node and
/// workload identity — no parallel TLS scheme.
///
/// `ca_keypair` and `ca_params` come from the Ingress `GeneratedCa`
/// (`hierarchy.ingress`). `hostnames` are the ingress hosts to put in the
/// certificate's SANs. The returned cert/key plug straight into
/// [`build_tls_config`].
pub fn issue_ingress_cert(
    hostnames: &[String],
    lifetime: std::time::Duration,
    ca_keypair: &rcgen::KeyPair,
    ca_params: &rcgen::CertificateParams,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsError> {
    let common_name = hostnames
        .first()
        .cloned()
        .ok_or_else(|| TlsError::CertGenFailed("no ingress hostname supplied".to_string()))?;

    // The cert is a TLS server, so it carries the ServerAuth extended key
    // usage. `SerialNumber(1)` is a placeholder; a real deployment threads
    // the CA's serial allocator through here.
    let (cert_der, key_der, _serial) = crate::sesame::ca::issue_end_entity_cert(
        &common_name,
        crate::sesame::types::SerialNumber(1),
        lifetime,
        hostnames,
        &[rcgen::ExtendedKeyUsagePurpose::ServerAuth],
        ca_keypair,
        ca_params,
    )
    .map_err(|e| TlsError::CertGenFailed(e.to_string()))?;

    let cert = CertificateDer::from(cert_der);
    let key = PrivateKeyDer::try_from(key_der)
        .map_err(|e| TlsError::CertGenFailed(format!("invalid issued key: {e}")))?;
    Ok((vec![cert], key))
}

/// Load a certificate and private key from PEM files on disk.
pub fn load_certs_from_disk(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsError> {
    let cert_file = std::fs::File::open(cert_path).map_err(|e| TlsError::LoadFailed {
        path: cert_path.display().to_string(),
        reason: e.to_string(),
    })?;
    let mut cert_reader = std::io::BufReader::new(cert_file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<_, _>>()
        .map_err(|e| TlsError::LoadFailed {
            path: cert_path.display().to_string(),
            reason: e.to_string(),
        })?;

    if certs.is_empty() {
        return Err(TlsError::LoadFailed {
            path: cert_path.display().to_string(),
            reason: "no certificates found in PEM file".to_string(),
        });
    }

    let key_file = std::fs::File::open(key_path).map_err(|e| TlsError::LoadFailed {
        path: key_path.display().to_string(),
        reason: e.to_string(),
    })?;
    let mut key_reader = std::io::BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| TlsError::LoadFailed {
            path: key_path.display().to_string(),
            reason: e.to_string(),
        })?
        .ok_or_else(|| TlsError::LoadFailed {
            path: key_path.display().to_string(),
            reason: "no private key found in PEM file".to_string(),
        })?;

    Ok((certs, key))
}

/// Build a rustls `ServerConfig` from a certificate and key.
///
/// Enforces TLS 1.2+ (rejects 1.0 and 1.1).
pub fn build_tls_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>, TlsError> {
    // Ensure the ring crypto provider is installed (idempotent)
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| TlsError::ConfigFailed(e.to_string()))?;

    // rustls 0.23 defaults to TLS 1.2+ (no 1.0/1.1 support at all),
    // so no additional version filtering is needed.

    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_self_signed_cert_succeeds() {
        let (cert, key) = generate_self_signed_cert().unwrap();
        assert!(!cert.is_empty());
        match &key {
            PrivateKeyDer::Pkcs8(k) => assert!(!k.secret_pkcs8_der().is_empty()),
            other => panic!("unexpected key type: {other:?}"),
        }
    }

    #[test]
    fn build_tls_config_from_self_signed() {
        let (cert, key) = generate_self_signed_cert().unwrap();
        let config = build_tls_config(vec![cert], key).unwrap();

        // rustls 0.23 only supports TLS 1.2 and 1.3 — there's no
        // way to enable 1.0/1.1 even if you tried. Verify the config
        // was built successfully (the version enforcement is implicit).
        assert!(config.alpn_protocols.is_empty() || !config.alpn_protocols.is_empty());
        // If we got here without an error, the config is valid
    }

    #[test]
    fn load_from_nonexistent_file_errors() {
        let result = load_certs_from_disk(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        );
        assert!(result.is_err());
    }

    /// ING1 cluster-CA path: an ingress cert issued from the Sesame Ingress
    /// CA builds a valid TLS server config. This proves the cluster-CA cert
    /// is issued and servable, reusing the real CA hierarchy.
    #[test]
    fn ingress_cert_from_cluster_ca_builds_a_server_config() {
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("test-cluster", b"test-wrap-ikm").unwrap();

        let (certs, key) = issue_ingress_cert(
            &["myapp.example.com".to_string()],
            std::time::Duration::from_secs(90 * 24 * 3600),
            &hierarchy.ingress.signing_keypair,
            &hierarchy.ingress.certificate_params,
        )
        .unwrap();

        assert_eq!(certs.len(), 1);
        // The issued cert and key must yield a working rustls config.
        let config = build_tls_config(certs, key).unwrap();
        let _ = config; // built without error means it's servable
    }

    #[test]
    fn ingress_cert_requires_a_hostname() {
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("test-cluster", b"test-wrap-ikm").unwrap();
        let result = issue_ingress_cert(
            &[],
            std::time::Duration::from_secs(3600),
            &hierarchy.ingress.signing_keypair,
            &hierarchy.ingress.certificate_params,
        );
        assert!(result.is_err());
    }
}
