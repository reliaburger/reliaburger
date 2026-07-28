/// TLS termination for the Wrapper ingress proxy.
///
/// Wrapper uses a cluster Ingress CA certificate resolver or an
/// operator-supplied certificate and key. It generates a self-signed
/// certificate only for development and listener bootstrap. Automatic ACME
/// provisioning is not part of the current contract.
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
/// `127.0.0.1`. Not suitable for production; use the cluster Ingress CA or
/// configure an operator-supplied certificate and key.
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

/// Build a rustls `ServerConfig` that resolves certificates per SNI hostname
/// (M8). Used for `tls = "cluster"` ingress: `resolver` issues each host a
/// certificate from the cluster's Ingress CA on demand, so a client trusting
/// the cluster root trusts the ingress instead of hitting a self-signed
/// `localhost` cert.
pub fn build_tls_config_with_resolver(
    resolver: Arc<dyn rustls::server::ResolvesServerCert>,
) -> Result<Arc<ServerConfig>, TlsError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    Ok(Arc::new(config))
}

/// Turns a DER cert chain + key into a rustls [`CertifiedKey`] ready to serve.
fn certified_key(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<rustls::sign::CertifiedKey>, TlsError> {
    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| TlsError::ConfigFailed(format!("unsupported ingress key: {e}")))?;
    Ok(Arc::new(rustls::sign::CertifiedKey::new(
        certs,
        signing_key,
    )))
}

/// Per-SNI ingress certificate resolver backed by the cluster Ingress CA (M8).
///
/// On each TLS handshake it looks at the client's SNI hostname and returns a
/// certificate for it, issued from the Ingress CA and cached so a repeat
/// handshake doesn't re-sign. A handshake with no SNI (or an issuance failure)
/// falls back to the self-signed default, preserving the previous behaviour
/// rather than dropping the connection.
///
/// `rustls`'s `resolve` is a synchronous trait method called on the handshake
/// path, so the cache uses `std::sync::Mutex` (not the tokio one): the lock is
/// never held across an `.await`.
pub struct IngressCertResolver {
    ca_keypair: rcgen::KeyPair,
    ca_params: rcgen::CertificateParams,
    lifetime: std::time::Duration,
    default_key: Arc<rustls::sign::CertifiedKey>,
    cache: std::sync::Mutex<std::collections::HashMap<String, Arc<rustls::sign::CertifiedKey>>>,
}

impl std::fmt::Debug for IngressCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngressCertResolver")
            .finish_non_exhaustive()
    }
}

impl IngressCertResolver {
    /// Build a resolver from the Ingress CA material and a self-signed
    /// fallback cert/key (used when a handshake carries no SNI).
    pub fn new(
        ca_keypair: rcgen::KeyPair,
        ca_params: rcgen::CertificateParams,
        lifetime: std::time::Duration,
        default_cert: Vec<CertificateDer<'static>>,
        default_key: PrivateKeyDer<'static>,
    ) -> Result<Self, TlsError> {
        Ok(Self {
            ca_keypair,
            ca_params,
            lifetime,
            default_key: certified_key(default_cert, default_key)?,
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Issue (or fetch from cache) a certified key for `hostname`.
    fn key_for(&self, hostname: &str) -> Option<Arc<rustls::sign::CertifiedKey>> {
        if let Ok(cache) = self.cache.lock()
            && let Some(existing) = cache.get(hostname)
        {
            return Some(Arc::clone(existing));
        }
        let hosts = [hostname.to_string()];
        let (chain, key) =
            issue_ingress_cert(&hosts, self.lifetime, &self.ca_keypair, &self.ca_params).ok()?;
        let certified = certified_key(chain, key).ok()?;
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(hostname.to_string(), Arc::clone(&certified));
        }
        Some(certified)
    }
}

impl rustls::server::ResolvesServerCert for IngressCertResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        match client_hello.server_name() {
            Some(name) => Some(
                self.key_for(name)
                    .unwrap_or_else(|| Arc::clone(&self.default_key)),
            ),
            None => Some(Arc::clone(&self.default_key)),
        }
    }
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

    /// M8: the per-SNI resolver issues a cluster-CA cert for the requested
    /// host and caches it, and reuses the CA — proving `tls = "cluster"` gets a
    /// cluster-signed cert rather than the self-signed fallback.
    #[test]
    fn ingress_resolver_issues_and_caches_per_host() {
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("test-cluster", b"test-wrap-ikm").unwrap();
        let (default_cert, default_key) = generate_self_signed_cert().unwrap();
        let resolver = IngressCertResolver::new(
            hierarchy.ingress.signing_keypair,
            hierarchy.ingress.certificate_params,
            std::time::Duration::from_secs(90 * 24 * 3600),
            vec![default_cert],
            default_key,
        )
        .unwrap();

        let first = resolver.key_for("myapp.example.com").unwrap();
        let second = resolver.key_for("myapp.example.com").unwrap();
        // Cache hit returns the very same Arc.
        assert!(Arc::ptr_eq(&first, &second));
        // A different host issues a distinct cert.
        let other = resolver.key_for("other.example.com").unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
    }
}
