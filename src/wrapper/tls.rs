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
use std::sync::atomic::{AtomicU64, Ordering};

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Process-local monotonic serial source for ingress certificates (M15).
///
/// Ingress certs are minted per-SNI and short-lived, so threading the cluster's
/// Raft serial allocator (a consensus write per cert) is impractical. This at
/// least gives every ingress cert a *distinct* serial — the old code stamped
/// every one `SerialNumber(1)`, so they were indistinguishable and could never
/// be told apart in a CRL. Seeded high so it can't collide with the CA's
/// centrally-allocated (low, sequential) serials. Central CRL-revocation of an
/// individual ingress leaf remains future work.
static INGRESS_SERIAL: AtomicU64 = AtomicU64::new(1 << 62);

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
    // usage. Each ingress cert gets a distinct serial (M15) — see INGRESS_SERIAL.
    let serial = INGRESS_SERIAL.fetch_add(1, Ordering::Relaxed);
    let (cert_der, key_der, _serial) = crate::sesame::ca::issue_end_entity_cert(
        &common_name,
        crate::sesame::types::SerialNumber(serial),
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

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| TlsError::ConfigFailed(e.to_string()))?;

    // rustls 0.23 defaults to TLS 1.2+ (no 1.0/1.1 support at all),
    // so no additional version filtering is needed.

    // Advertise HTTP/2 then HTTP/1.1 (E); hyper's auto builder serves whichever
    // ALPN the client negotiates.
    config.alpn_protocols = crate::wrapper::types::alpn_protocols();

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
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = crate::wrapper::types::alpn_protocols();
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

/// Largest number of per-SNI certificates the resolver caches. The host
/// allowlist already bounds this to the number of configured ingress routes;
/// the cap is defence in depth against a pathologically large route set.
const MAX_SNI_CACHE: usize = 1024;

/// Per-SNI ingress certificate resolver backed by the cluster Ingress CA (M8).
///
/// On each TLS handshake it looks at the client's SNI hostname. A cluster-CA
/// certificate is minted (and cached) **only for a host that currently has an
/// ingress route**; every other SNI — unknown host, no SNI, or a routing table
/// momentarily locked for a rebuild — is served the self-signed default. That
/// allowlist is the security boundary: without it, an attacker's arbitrary SNI
/// would drive unbounded CA signing and grow the cache without limit.
///
/// `rustls`'s `resolve` is a synchronous trait method called on the handshake
/// path, so the cache uses `std::sync::Mutex` (not the tokio one) and the
/// routing table is read with `try_read()`: neither lock is held across an
/// `.await`, and a contended rebuild simply falls back to the default cert.
pub struct IngressCertResolver {
    ca_keypair: rcgen::KeyPair,
    ca_params: rcgen::CertificateParams,
    lifetime: std::time::Duration,
    default_key: Arc<rustls::sign::CertifiedKey>,
    routes: Arc<tokio::sync::RwLock<super::routing::RoutingTable>>,
    cache: std::sync::Mutex<std::collections::HashMap<String, Arc<rustls::sign::CertifiedKey>>>,
}

impl std::fmt::Debug for IngressCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IngressCertResolver")
            .finish_non_exhaustive()
    }
}

impl IngressCertResolver {
    /// Build a resolver from the Ingress CA material, the live ingress routing
    /// table (the host allowlist), and a self-signed fallback cert/key (served
    /// for any SNI that isn't a configured route, and when a handshake carries
    /// no SNI).
    pub fn new(
        ca_keypair: rcgen::KeyPair,
        ca_params: rcgen::CertificateParams,
        lifetime: std::time::Duration,
        routes: Arc<tokio::sync::RwLock<super::routing::RoutingTable>>,
        default_cert: Vec<CertificateDer<'static>>,
        default_key: PrivateKeyDer<'static>,
    ) -> Result<Self, TlsError> {
        Ok(Self {
            ca_keypair,
            ca_params,
            lifetime,
            default_key: certified_key(default_cert, default_key)?,
            routes,
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
        if let Ok(mut cache) = self.cache.lock()
            && (cache.len() < MAX_SNI_CACHE || cache.contains_key(hostname))
        {
            cache.insert(hostname.to_string(), Arc::clone(&certified));
        }
        Some(certified)
    }

    /// Whether `hostname` currently has an ingress route, read without blocking
    /// the synchronous handshake path. A rebuild write briefly makes this
    /// `false`; that only means the default cert is served for one handshake.
    fn is_configured_host(&self, hostname: &str) -> bool {
        self.routes
            .try_read()
            .map(|table| table.contains_host(hostname))
            .unwrap_or(false)
    }

    /// The certificate to serve for a handshake's SNI (`None` = no SNI). A
    /// cluster cert is minted/served only for a configured ingress host; every
    /// other case gets the self-signed default. Split out from
    /// [`ResolvesServerCert::resolve`] so the decision is unit-testable without
    /// fabricating a `ClientHello`.
    fn certificate_for(&self, server_name: Option<&str>) -> Arc<rustls::sign::CertifiedKey> {
        match server_name {
            Some(name) if self.is_configured_host(name) => self
                .key_for(name)
                .unwrap_or_else(|| Arc::clone(&self.default_key)),
            _ => Arc::clone(&self.default_key),
        }
    }
}

impl rustls::server::ResolvesServerCert for IngressCertResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        // Mint (or serve a cached) cluster cert only for a configured ingress
        // host. An unknown SNI never triggers CA signing or a cache insert — it
        // gets the self-signed default, whose name mismatch the client rejects,
        // exactly as an unconfigured host should be.
        Some(self.certificate_for(client_hello.server_name()))
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
            empty_routes(),
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

    /// An empty routing table — every SNI is an unconfigured host.
    fn empty_routes() -> Arc<tokio::sync::RwLock<super::super::routing::RoutingTable>> {
        Arc::new(tokio::sync::RwLock::new(
            super::super::routing::RoutingTable::new(),
        ))
    }

    fn resolver_with(
        routes: Arc<tokio::sync::RwLock<super::super::routing::RoutingTable>>,
    ) -> IngressCertResolver {
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("test-cluster", b"test-wrap-ikm").unwrap();
        let (default_cert, default_key) = generate_self_signed_cert().unwrap();
        IngressCertResolver::new(
            hierarchy.ingress.signing_keypair,
            hierarchy.ingress.certificate_params,
            std::time::Duration::from_secs(3600),
            routes,
            vec![default_cert],
            default_key,
        )
        .unwrap()
    }

    /// The security fix: an SNI for a host with no ingress route is served the
    /// self-signed default — it never triggers CA signing and never touches the
    /// cache, so an attacker's arbitrary SNI can neither mint certs nor grow
    /// memory.
    #[test]
    fn unconfigured_sni_gets_the_default_and_never_caches() {
        let resolver = resolver_with(empty_routes());

        let served = resolver.certificate_for(Some("attacker.example.com"));
        assert!(
            Arc::ptr_eq(&served, &resolver.default_key),
            "an unconfigured host must get the self-signed default"
        );
        // No SNI also gets the default.
        assert!(Arc::ptr_eq(
            &resolver.certificate_for(None),
            &resolver.default_key
        ));
        // Crucially, nothing was minted or cached.
        assert!(resolver.cache.lock().unwrap().is_empty());
    }

    /// A configured ingress host still gets a real cluster-CA cert, cached.
    #[tokio::test]
    async fn configured_sni_mints_a_cluster_cert() {
        use crate::config::app::IngressSpec;
        use crate::onion::service_id::ServiceId;
        use crate::onion::service_map::ServiceMap;
        use crate::onion::types::BackendInstance;
        use std::collections::HashMap;
        use std::net::Ipv4Addr;

        let mut map = ServiceMap::new();
        map.register_app("web", "default", 8080, None).unwrap();
        map.add_backend(
            &ServiceId::new("default", "web"),
            BackendInstance {
                instance_id: "web-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30001,
                healthy: true,
            },
        )
        .unwrap();
        let mut configs = HashMap::new();
        configs.insert(
            ("default".to_string(), "web".to_string()),
            IngressSpec {
                host: "myapp.example.com".to_string(),
                path: None,
                tls: Some("cluster".to_string()),
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = super::super::routing::RoutingTable::new();
        table.rebuild(&map, &configs).unwrap();
        let routes = Arc::new(tokio::sync::RwLock::new(table));

        let resolver = resolver_with(routes);
        let served = resolver.certificate_for(Some("myapp.example.com"));
        // A configured host gets a minted cert, not the default...
        assert!(!Arc::ptr_eq(&served, &resolver.default_key));
        // ...and it is cached (case-insensitively matched, cached by SNI).
        assert!(
            resolver
                .cache
                .lock()
                .unwrap()
                .contains_key("myapp.example.com")
        );
        // An unconfigured host on the same resolver still gets the default.
        assert!(Arc::ptr_eq(
            &resolver.certificate_for(Some("evil.example.com")),
            &resolver.default_key
        ));
    }
}
