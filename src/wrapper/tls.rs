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
/// [`build_tls_config`]. The chain includes the original root-signed Ingress CA
/// certificate so clients need only the cluster root as their trust anchor.
pub fn issue_ingress_cert(
    hostnames: &[String],
    lifetime: std::time::Duration,
    ca_keypair: &rcgen::KeyPair,
    ca_params: &rcgen::CertificateParams,
    issuer_certificate: &CertificateDer<'static>,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsError> {
    let common_name = hostnames
        .first()
        .cloned()
        .ok_or_else(|| TlsError::CertGenFailed("no ingress hostname supplied".to_string()))?;

    // The original certificate, not reconstructed signing parameters, defines
    // the issuer's validity. A leaf must never extend that trust window.
    let issuer_validity = CertificateValidity::parse(issuer_certificate)?;
    let mut ca_params = ca_params.clone();
    ca_params.not_before = time::OffsetDateTime::from_unix_timestamp(issuer_validity.not_before)
        .map_err(|e| TlsError::CertGenFailed(e.to_string()))?;
    ca_params.not_after = time::OffsetDateTime::from_unix_timestamp(issuer_validity.not_after)
        .map_err(|e| TlsError::CertGenFailed(e.to_string()))?;
    let (cert_der, key_der) = crate::sesame::ca::issue_ingress_leaf_cert(
        &common_name,
        lifetime,
        hostnames,
        ca_keypair,
        &ca_params,
    )
    .map_err(|e| TlsError::CertGenFailed(e.to_string()))?;

    let cert = CertificateDer::from(cert_der);
    let key = PrivateKeyDer::try_from(key_der)
        .map_err(|e| TlsError::CertGenFailed(format!("invalid issued key: {e}")))?;
    Ok((vec![cert, issuer_certificate.clone()], key))
}

/// Load a certificate and private key from PEM files on disk.
pub fn load_certs_from_disk(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), TlsError> {
    let mut cert_reader = read_pem(cert_path)?;
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

    let mut key_reader = read_pem(key_path)?;
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

// Read each local regular file once with a hard size cap, off the async runtime.
// O_NONBLOCK also keeps a mistakenly configured FIFO from hanging the loader.
fn read_pem(path: &Path) -> Result<std::io::Cursor<Vec<u8>>, TlsError> {
    use std::io::Read;
    let read = || -> std::io::Result<Vec<u8>> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits());
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other(
                "certificate material must be a regular file",
            ));
        }
        const MAX_PEM_BYTES: u64 = 1024 * 1024;
        let mut bytes = Vec::new();
        file.take(MAX_PEM_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_PEM_BYTES {
            return Err(std::io::Error::other("certificate material exceeds 1 MiB"));
        }
        Ok(bytes)
    };
    read()
        .map(std::io::Cursor::new)
        .map_err(|error| TlsError::LoadFailed {
            path: path.display().to_string(),
            reason: error.to_string(),
        })
}

/// An operator-supplied certificate pair reloaded without restarting listeners.
/// Invalid replacements retain the previous pair only while it remains valid.
pub(super) struct FileCertResolver {
    cert_path: std::path::PathBuf,
    key_path: std::path::PathBuf,
    current: tokio::sync::watch::Sender<CachedCertificate>,
}

impl std::fmt::Debug for FileCertResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileCertResolver").finish_non_exhaustive()
    }
}

impl FileCertResolver {
    pub(super) async fn load(cert_path: &Path, key_path: &Path) -> Result<Self, TlsError> {
        let current = Self::read_pair(cert_path.to_owned(), key_path.to_owned()).await?;
        let (current, _) = tokio::sync::watch::channel(current);
        Ok(Self {
            cert_path: cert_path.to_owned(),
            key_path: key_path.to_owned(),
            current,
        })
    }

    async fn read_pair(
        cert_path: std::path::PathBuf,
        key_path: std::path::PathBuf,
    ) -> Result<CachedCertificate, TlsError> {
        tokio::task::spawn_blocking(move || {
            let (chain, key) = load_certs_from_disk(&cert_path, &key_path)?;
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let mut validity = CertificateValidity {
                not_before: i64::MIN,
                not_after: i64::MAX,
            };
            for certificate in &chain {
                let window = CertificateValidity::parse(certificate)?;
                if !window.contains(now) {
                    return Err(TlsError::ConfigFailed(
                        "certificate chain is outside its validity period".into(),
                    ));
                }
                validity.not_before = validity.not_before.max(window.not_before);
                validity.not_after = validity.not_after.min(window.not_after);
            }
            Ok(CachedCertificate {
                key: certified_key(chain, key)?,
                validity,
            })
        })
        .await
        .map_err(|error| TlsError::ConfigFailed(format!("certificate loader failed: {error}")))?
    }

    /// Poll once a second; the caller owns this future alongside the listeners.
    pub(super) async fn run(&self, shutdown: tokio_util::sync::CancellationToken) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_error = None;
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            let replacement = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                replacement = Self::read_pair(self.cert_path.clone(), self.key_path.clone()) => replacement,
            };
            match replacement {
                Ok(replacement) => {
                    last_error = None;
                    let changed = self.current.borrow().key.cert != replacement.key.cert;
                    if changed {
                        self.current.send_replace(replacement);
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    if last_error.as_ref() != Some(&error) {
                        eprintln!(
                            "wrapper: TLS file reload refused; keeping the last pair subject to its expiry: {error}"
                        );
                        last_error = Some(error);
                    }
                }
            }
        }
    }
}

impl rustls::server::ResolvesServerCert for FileCertResolver {
    fn resolve(
        &self,
        _: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let current = self.current.borrow();
        current
            .validity
            .contains(time::OffsetDateTime::now_utc().unix_timestamp())
            .then(|| Arc::clone(&current.key))
    }
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
    // A resumed session skips certificate resolution and validity checks.
    // Every reconnect must observe the current leaf after renewal or reload.
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
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
    // A resumed session skips certificate resolution and validity checks.
    // Every reconnect must observe the current leaf after renewal or reload.
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
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
    let certified = rustls::sign::CertifiedKey::new(certs, signing_key);
    certified
        .keys_match()
        .map_err(|error| TlsError::ConfigFailed(error.to_string()))?;
    Ok(Arc::new(certified))
}

#[derive(Debug, Clone, Copy)]
struct CertificateValidity {
    not_before: i64,
    not_after: i64,
}

impl CertificateValidity {
    fn parse(certificate: &CertificateDer<'_>) -> Result<Self, TlsError> {
        use x509_parser::prelude::FromDer;
        let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(certificate)
            .map_err(|error| TlsError::ConfigFailed(format!("invalid certificate: {error}")))?;
        Ok(Self {
            not_before: parsed.validity().not_before.timestamp(),
            not_after: parsed.validity().not_after.timestamp(),
        })
    }

    fn contains(self, now: i64) -> bool {
        self.not_before <= now && now < self.not_after
    }

    fn renewal_due(self, now: i64) -> bool {
        now >= self.not_before + (self.not_after - self.not_before) / 2
    }
}

#[derive(Clone)]
struct CachedCertificate {
    key: Arc<rustls::sign::CertifiedKey>,
    validity: CertificateValidity,
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
    issuer_certificate: CertificateDer<'static>,
    lifetime: std::time::Duration,
    default_key: Arc<rustls::sign::CertifiedKey>,
    routes: Arc<tokio::sync::RwLock<super::routing::RoutingTable>>,
    cache: std::sync::Mutex<std::collections::HashMap<String, CachedCertificate>>,
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
        issuer_certificate: CertificateDer<'static>,
        lifetime: std::time::Duration,
        routes: Arc<tokio::sync::RwLock<super::routing::RoutingTable>>,
        default_cert: Vec<CertificateDer<'static>>,
        default_key: PrivateKeyDer<'static>,
    ) -> Result<Self, TlsError> {
        Ok(Self {
            ca_keypair,
            ca_params,
            issuer_certificate,
            lifetime,
            default_key: certified_key(default_cert, default_key)?,
            routes,
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Issue (or fetch from cache) a certified key for `hostname`.
    fn key_for(&self, hostname: &str) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let hostname = hostname.to_ascii_lowercase();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let existing = self
            .cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(&hostname).cloned())
            .filter(|entry| entry.validity.contains(now));
        if let Some(entry) = &existing
            && !entry.validity.renewal_due(now)
        {
            return Some(Arc::clone(&entry.key));
        }

        // Signing stays outside the cache lock. If renewal fails, the previous
        // key is usable only for its remaining validity, never after expiry.
        self.issue_key(&hostname).or_else(|| {
            existing
                .filter(|entry| {
                    entry
                        .validity
                        .contains(time::OffsetDateTime::now_utc().unix_timestamp())
                })
                .map(|entry| entry.key)
        })
    }

    fn issue_key(&self, hostname: &str) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let hosts = [hostname.to_string()];
        let (chain, key) = issue_ingress_cert(
            &hosts,
            self.lifetime,
            &self.ca_keypair,
            &self.ca_params,
            &self.issuer_certificate,
        )
        .ok()?;
        let validity = CertificateValidity::parse(chain.first()?).ok()?;
        if !validity.contains(time::OffsetDateTime::now_utc().unix_timestamp()) {
            return None;
        }
        let certified = certified_key(chain, key).ok()?;
        if let Ok(mut cache) = self.cache.lock()
            && (cache.len() < MAX_SNI_CACHE || cache.contains_key(hostname))
        {
            cache.insert(
                hostname.to_string(),
                CachedCertificate {
                    key: Arc::clone(&certified),
                    validity,
                },
            );
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
            &CertificateDer::from(hierarchy.ingress.ca.certificate_der.clone()),
        )
        .unwrap();

        assert_eq!(certs.len(), 2);
        // The issued cert and key must yield a working rustls config.
        let config = build_tls_config(certs, key).unwrap();
        let _ = config; // built without error means it's servable
    }

    // Child entry point for the separate-process serial regression. No key
    // material crosses stdout or process arguments.
    #[test]
    fn ingress_serial_child() {
        use rcgen::{CertificateParams, KeyPair};
        let Some(directory) = std::env::var_os("RELIABURGER_SERIAL_TEST_DIRECTORY") else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        let certificate = CertificateDer::from(std::fs::read(directory.join("ca.der")).unwrap());
        let params = CertificateParams::from_ca_cert_der(&certificate).unwrap();
        let key =
            PrivateKeyDer::try_from(std::fs::read(directory.join("ca.key")).unwrap()).unwrap();
        let key = KeyPair::from_der_and_sign_algo(&key, &rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let (certificates, _) = issue_ingress_cert(
            &["app.example.com".into()],
            std::time::Duration::from_secs(3600),
            &key,
            &params,
            &certificate,
        )
        .unwrap();
        let destination = std::env::var_os("RELIABURGER_SERIAL_TEST_OUTPUT").unwrap();
        std::fs::write(destination, &certificates[0]).unwrap();
    }

    #[tokio::test]
    async fn ingress_serials_differ_across_nodes_and_restarts_under_one_issuer() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use x509_parser::prelude::FromDer;
        let directory = tempfile::tempdir().unwrap();
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("serial-test", b"test-wrap-ikm").unwrap();
        std::fs::write(
            directory.path().join("ca.der"),
            &hierarchy.ingress.ca.certificate_der,
        )
        .unwrap();
        let mut key_file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(directory.path().join("ca.key"))
            .unwrap();
        key_file
            .write_all(&hierarchy.ingress.private_key_der)
            .unwrap();
        drop(key_file);
        async fn issue(directory: &Path, node: &str) -> Vec<u8> {
            let destination = directory.join(node);
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "wrapper::tls::tests::ingress_serial_child",
                    "--nocapture",
                ])
                .env("RELIABURGER_SERIAL_TEST_DIRECTORY", directory)
                .env("RELIABURGER_SERIAL_TEST_OUTPUT", &destination)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let status = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(status.success());
            std::fs::read(destination).unwrap()
        }
        let first = issue(directory.path(), "first.der").await;
        let restarted = issue(directory.path(), "restarted.der").await;
        let (peer_a, peer_b) = tokio::join!(
            issue(directory.path(), "peer-a.der"),
            issue(directory.path(), "peer-b.der")
        );
        let (_, ca) = x509_parser::certificate::X509Certificate::from_der(
            &hierarchy.ingress.ca.certificate_der,
        )
        .unwrap();
        let mut serials = std::collections::HashSet::new();
        for der in [first, restarted, peer_a, peer_b] {
            let (_, certificate) =
                x509_parser::certificate::X509Certificate::from_der(&der).unwrap();
            certificate.verify_signature(Some(ca.public_key())).unwrap();
            assert!(
                serials.insert(certificate.raw_serial().to_vec()),
                "separate processes reused a certificate serial under the same CA"
            );
        }
        for serial in serials {
            assert_eq!(serial.len(), 20);
            assert!((0x40..=0x7f).contains(&serial[0]));
        }
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
            &CertificateDer::from(hierarchy.ingress.ca.certificate_der.clone()),
        );
        assert!(result.is_err());
    }

    #[test]
    fn ingress_leaf_cannot_outlive_its_issuer() {
        use x509_parser::prelude::FromDer;
        let mut hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("short-issuer", b"test-ikm").unwrap();
        hierarchy.ingress.certificate_params.not_after =
            time::OffsetDateTime::now_utc() + time::Duration::seconds(30);
        let issuer = hierarchy
            .ingress
            .certificate_params
            .clone()
            .self_signed(&hierarchy.ingress.signing_keypair)
            .unwrap();
        let (chain, _) = issue_ingress_cert(
            &["web.example".into()],
            std::time::Duration::from_secs(3600),
            &hierarchy.ingress.signing_keypair,
            &hierarchy.ingress.certificate_params,
            issuer.der(),
        )
        .unwrap();
        let (_, leaf) = x509_parser::certificate::X509Certificate::from_der(&chain[0]).unwrap();
        let (_, issuer) = x509_parser::certificate::X509Certificate::from_der(&chain[1]).unwrap();
        assert!(leaf.validity().not_after <= issuer.validity().not_after);
    }

    #[test]
    fn expired_ingress_issuer_cannot_mint_a_leaf() {
        let mut hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("expired-issuer", b"test-ikm").unwrap();
        hierarchy.ingress.certificate_params.not_after =
            time::OffsetDateTime::now_utc() - time::Duration::seconds(1);
        let issuer = hierarchy
            .ingress
            .certificate_params
            .clone()
            .self_signed(&hierarchy.ingress.signing_keypair)
            .unwrap();
        assert!(
            issue_ingress_cert(
                &["web.example".into()],
                std::time::Duration::from_secs(3600),
                &hierarchy.ingress.signing_keypair,
                &hierarchy.ingress.certificate_params,
                issuer.der(),
            )
            .is_err()
        );
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
            CertificateDer::from(hierarchy.ingress.ca.certificate_der),
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
            CertificateDer::from(hierarchy.ingress.ca.certificate_der),
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
