//! Mutual TLS configuration for inter-node communication.
//!
//! Builds rustls `ClientConfig` and `ServerConfig` that require both
//! sides to present valid node certificates signed by the Node CA.
//!
//! Two things distinguish these from stock rustls configs:
//!
//! - Node certificates are signed by the **Node CA intermediate**, so both
//!   sides present `[leaf, node-ca]` — WebPKI needs the intermediate to
//!   build a path to the Root CA trust anchor.
//! - Revocation comes from the Raft-replicated [`Crl`], shared through a
//!   [`CrlHandle`], not from X.509 CRL files. Every handshake re-reads the
//!   handle, so a `RevokeCertificate` write takes effect on the next
//!   connection without rebuilding any config.
//!
//! The client side uses [`PinnedChainServerVerifier`], which checks the chain
//! against the pinned cluster CAs and skips hostname verification (peers are
//! dialled by gossip IP, not by DNS name). When the caller knows which node it
//! is dialling — the Raft transport does — it builds the verifier in *bound*
//! mode, which also asserts the peer leaf carries the matching node-id URI SAN
//! (`spiffe://reliaburger/node/<id>`, PKI3). That lifts the guarantee from "a
//! valid, unrevoked Node-CA certificate" to "the specific node I meant to
//! reach", so one compromised node can't impersonate another. Callers that
//! dial by address only (reporting, node-to-node API) stay on the CA-pin +
//! CRL guarantee.

use std::sync::{Arc, RwLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};

use super::cert::{self, CertError};
use super::identity_store::NodeIdentity;
use super::types::Crl;

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

/// The peer's leaf certificate did not carry the node-id URI SAN we required
/// (PKI3). Carried inside a rustls `CertificateError::Other` so the handshake
/// fails with a diagnosable reason.
#[derive(Debug, thiserror::Error)]
#[error("peer node-id mismatch: expected {expected}, presented {presented:?}")]
struct NodeIdMismatch {
    expected: String,
    presented: Option<String>,
}

/// A shared, updatable view of the cluster CRL.
///
/// Verifiers hold a clone and re-read it on every handshake; the owner
/// (bun's security refresh ticker) replaces the contents whenever the
/// Raft-replicated `SecurityState.crl` changes.
#[derive(Debug, Clone, Default)]
pub struct CrlHandle {
    inner: Arc<RwLock<Crl>>,
}

impl CrlHandle {
    /// Create a handle seeded with the given CRL.
    pub fn new(crl: Crl) -> Self {
        Self {
            inner: Arc::new(RwLock::new(crl)),
        }
    }

    /// Replace the CRL contents (called from the security refresh ticker).
    pub fn update(&self, crl: Crl) {
        // A poisoned lock only means a writer panicked mid-update; the data
        // is a plain Vec swap, so recover rather than propagate the panic.
        let mut guard = self.inner.write().unwrap_or_else(|p| p.into_inner());
        *guard = crl;
    }

    /// Check a certificate serial against the CRL.
    pub fn check(&self, serial: super::types::SerialNumber) -> Result<(), CertError> {
        let guard = self.inner.read().unwrap_or_else(|p| p.into_inner());
        cert::check_crl(serial, &guard)
    }

    /// Check every certificate in a presented chain against the CRL.
    fn check_chain(&self, chain: &[&CertificateDer<'_>]) -> Result<(), rustls::Error> {
        for der in chain {
            let serial = cert::serial_from_der(der).map_err(cert_error_to_rustls)?;
            self.check(serial).map_err(cert_error_to_rustls)?;
        }
        Ok(())
    }
}

/// Map our certificate errors onto rustls's error vocabulary.
fn cert_error_to_rustls(err: CertError) -> rustls::Error {
    match err {
        CertError::Revoked { .. } => {
            rustls::Error::InvalidCertificate(rustls::CertificateError::Revoked)
        }
        CertError::Expired => rustls::Error::InvalidCertificate(rustls::CertificateError::Expired),
        CertError::NotYetValid => {
            rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidYet)
        }
        CertError::ParseFailed(_) => {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        }
        CertError::ChainInvalid(_) | CertError::IssuerMismatch(_) => {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadSignature)
        }
        CertError::NotCodeSigning => {
            rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        }
    }
}

/// A client-certificate verifier that delegates chain building to
/// `WebPkiClientVerifier` and then rejects revoked certificates.
#[derive(Debug)]
pub struct RevocationCheckingClientVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    crl: CrlHandle,
}

impl ClientCertVerifier for RevocationCheckingClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        self.inner.root_hint_subjects()
    }

    // Delegate the auth-required policy to the inner verifier: the Raft/
    // reporting configs require a client cert; the API config allows an
    // unauthenticated client (relish/browsers use a bearer token or cookie).
    // Without these the trait defaults to mandatory, ignoring the inner
    // builder's `allow_unauthenticated`.
    fn offer_client_auth(&self) -> bool {
        self.inner.offer_client_auth()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.inner.client_auth_mandatory()
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self
            .inner
            .verify_client_cert(end_entity, intermediates, now)?;
        let mut chain: Vec<&CertificateDer<'_>> = vec![end_entity];
        chain.extend(intermediates.iter());
        self.crl.check_chain(&chain)?;
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// A server-certificate verifier for cluster peers.
///
/// Validates the presented certificate against the **pinned** Node CA and
/// Root CA from this node's identity, checks the CRL, and skips hostname
/// verification (peers are dialled by gossip IP, not by a DNS name).
///
/// When `expected_node_id` is set (PKI3), the verifier additionally asserts
/// that the peer leaf carries the matching node-id URI SAN
/// (`spiffe://reliaburger/node/<id>`). That turns the guarantee from "some
/// valid, unrevoked Node-CA certificate" into "the specific node I meant to
/// reach", so one compromised node cannot present its own valid cert to
/// impersonate another. When it is `None` the check is CA-pin + CRL only,
/// which is what the join-time bootstrap and node-to-node API calls use.
#[derive(Debug)]
pub struct PinnedChainServerVerifier {
    node_ca_der: Vec<u8>,
    root_ca_der: Vec<u8>,
    crl: CrlHandle,
    expected_node_id: Option<String>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl PinnedChainServerVerifier {
    /// Build a verifier pinned to the given CA certificates, without node-id
    /// binding (CA-pin + CRL only).
    pub fn new(node_ca_der: Vec<u8>, root_ca_der: Vec<u8>, crl: CrlHandle) -> Self {
        Self {
            node_ca_der,
            root_ca_der,
            crl,
            expected_node_id: None,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    /// Build a verifier that also binds the connection to `expected_node_id`
    /// (PKI3): the peer leaf must carry the matching node-id URI SAN.
    pub fn new_bound(
        node_ca_der: Vec<u8>,
        root_ca_der: Vec<u8>,
        crl: CrlHandle,
        expected_node_id: String,
    ) -> Self {
        Self {
            node_ca_der,
            root_ca_der,
            crl,
            expected_node_id: Some(expected_node_id),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }
}

/// Extract the node id from a leaf certificate's node-id URI SAN, if present.
fn node_id_from_leaf(der: &CertificateDer<'_>) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let san = cert.subject_alternative_name().ok().flatten()?;
    for name in &san.value.general_names {
        if let x509_parser::extensions::GeneralName::URI(uri) = name
            && let Some(id) = super::ca::node_id_from_spiffe_uri(uri)
        {
            return Some(id.to_string());
        }
    }
    None
}

impl ServerCertVerifier for PinnedChainServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Chain checks run against the pinned CAs, not whatever
        // intermediates the peer chose to present.
        cert::validate_chain(end_entity, &self.node_ca_der, &self.root_ca_der)
            .map_err(cert_error_to_rustls)?;
        self.crl.check_chain(&[end_entity])?;
        let node_ca_serial =
            cert::serial_from_der(&self.node_ca_der).map_err(cert_error_to_rustls)?;
        self.crl
            .check(node_ca_serial)
            .map_err(cert_error_to_rustls)?;

        // PKI3: bind to the expected node id when one was threaded through.
        if let Some(expected) = &self.expected_node_id {
            let presented = node_id_from_leaf(end_entity).ok_or_else(|| {
                rustls::Error::InvalidCertificate(rustls::CertificateError::Other(
                    rustls::OtherError(Arc::new(NodeIdMismatch {
                        expected: expected.clone(),
                        presented: None,
                    })),
                ))
            })?;
            if &presented != expected {
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::Other(rustls::OtherError(Arc::new(NodeIdMismatch {
                        expected: expected.clone(),
                        presented: Some(presented),
                    }))),
                ));
            }
        }

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a rustls `ServerConfig` that requires client certificates.
///
/// The server presents `[node cert, node CA]` and verifies that connecting
/// clients present an unrevoked certificate chaining to the Root CA.
pub fn build_mtls_server_config(
    identity: &NodeIdentity,
    crl: CrlHandle,
) -> Result<Arc<ServerConfig>, MtlsError> {
    // Install the ring crypto provider (idempotent)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Pin the trust anchor to the **Node CA**, not the Root CA. Node certs
    // and workload certs are both signed under the Root, and node certs carry
    // ClientAuth EKU — so trusting the Root would let a workload certificate
    // authenticate as a node. Anchoring on the Node CA admits only certs it
    // issued directly (PKI2). The peer presents [leaf, node-ca]; webpki finds
    // the node-ca anchor and verifies the leaf against it.
    let mut root_store = RootCertStore::empty();
    root_store
        .add(CertificateDer::from(identity.node_ca_der.clone()))
        .map_err(|e| MtlsError::InvalidCert(format!("node CA: {e}")))?;

    // Chain building goes through WebPKI; revocation through the CRL handle.
    let webpki = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .map_err(|e| MtlsError::ConfigFailed(format!("client verifier: {e}")))?;
    let client_verifier = Arc::new(RevocationCheckingClientVerifier { inner: webpki, crl });

    let (chain, key) = identity_chain_and_key(identity)?;
    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(chain, key)
        .map_err(|e| MtlsError::ConfigFailed(e.to_string()))?;

    // Disable session resumption: a resumed session skips client-cert
    // verification, which would let a freshly revoked node reconnect with
    // a ticket from before its revocation. Cluster connections are few and
    // long-lived, so the extra full handshakes are cheap.
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;

    Ok(Arc::new(config))
}

/// Build a rustls `ServerConfig` for the agent API listener: it presents the
/// node certificate but treats client certs as **optional**.
///
/// The API is reached by the `relish` CLI and browsers, which have no node
/// certificate — they authenticate with a bearer token or a session cookie
/// over TLS. Node-to-node API calls that do present a node cert are verified
/// (and CRL-checked) exactly as on the Raft transport; a client with no cert
/// simply gets an unauthenticated TLS connection, which the bearer/cookie
/// layer then handles.
pub fn build_api_server_config(
    identity: &NodeIdentity,
    crl: CrlHandle,
) -> Result<Arc<ServerConfig>, MtlsError> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut root_store = RootCertStore::empty();
    root_store
        .add(CertificateDer::from(identity.node_ca_der.clone()))
        .map_err(|e| MtlsError::InvalidCert(format!("node CA: {e}")))?;
    let webpki = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .allow_unauthenticated()
        .build()
        .map_err(|e| MtlsError::ConfigFailed(format!("client verifier: {e}")))?;
    let client_verifier = Arc::new(RevocationCheckingClientVerifier { inner: webpki, crl });

    let (chain, key) = identity_chain_and_key(identity)?;
    let config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(chain, key)
        .map_err(|e| MtlsError::ConfigFailed(e.to_string()))?;
    Ok(Arc::new(config))
}

/// Build a reqwest client that trusts the cluster root CA for verifying the
/// server it connects to, using the same pinned verifier as the internal
/// transports (no hostname check). Used for node-to-node API calls when the
/// cluster runs mTLS. The client presents no certificate: node-to-node calls
/// still authenticate with the service token, now inside TLS.
pub fn build_cluster_http_client(identity: &NodeIdentity) -> Result<reqwest::Client, MtlsError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let verifier = Arc::new(PinnedChainServerVerifier::new(
        identity.node_ca_der.clone(),
        identity.root_ca_der.clone(),
        // Peer API certs are node certs; revocation is enforced on the Raft
        // and reporting transports. A fresh handle keeps this client simple.
        CrlHandle::default(),
    ));
    let tls = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .build()
        .map_err(|e| MtlsError::ConfigFailed(format!("cluster http client: {e}")))
}

/// Build a rustls `ClientConfig` for connecting to cluster peers.
///
/// The client presents `[node cert, node CA]` and verifies the server
/// against the pinned cluster CAs and the CRL (no hostname check — see
/// [`PinnedChainServerVerifier`]).
pub fn build_mtls_client_config(
    identity: &NodeIdentity,
    crl: CrlHandle,
) -> Result<Arc<ClientConfig>, MtlsError> {
    // Install the ring crypto provider (idempotent)
    let _ = rustls::crypto::ring::default_provider().install_default();

    let verifier = Arc::new(PinnedChainServerVerifier::new(
        identity.node_ca_der.clone(),
        identity.root_ca_der.clone(),
        crl,
    ));

    let (chain, key) = identity_chain_and_key(identity)?;
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(chain, key)
        .map_err(|e| MtlsError::ConfigFailed(e.to_string()))?;

    Ok(Arc::new(config))
}

/// Build a `ClientConfig` that binds the connection to `expected_node_id`
/// (PKI3): in addition to the CA-pin and CRL checks, the peer's leaf must
/// carry the matching node-id URI SAN. Used by the Raft transport, which
/// knows which node it is dialling.
pub fn build_mtls_client_config_bound(
    identity: &NodeIdentity,
    crl: CrlHandle,
    expected_node_id: &str,
) -> Result<Arc<ClientConfig>, MtlsError> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let verifier = Arc::new(PinnedChainServerVerifier::new_bound(
        identity.node_ca_der.clone(),
        identity.root_ca_der.clone(),
        crl,
        expected_node_id.to_string(),
    ));

    let (chain, key) = identity_chain_and_key(identity)?;
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(chain, key)
        .map_err(|e| MtlsError::ConfigFailed(e.to_string()))?;

    Ok(Arc::new(config))
}

/// The certificate chain this node presents, plus its private key.
fn identity_chain_and_key(
    identity: &NodeIdentity,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), MtlsError> {
    let chain = vec![
        CertificateDer::from(identity.certificate_der.clone()),
        CertificateDer::from(identity.node_ca_der.clone()),
    ];
    let key = PrivateKeyDer::try_from(identity.private_key_der.clone())
        .map_err(|e| MtlsError::InvalidKey(e.to_string()))?;
    Ok((chain, key))
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
/// shared HMAC key derived from the cluster's master secret.
pub mod gossip_hmac {
    use ring::hmac;

    /// Derive the shared gossip HMAC key from the cluster master secret.
    ///
    /// Every node loads the same 32-byte master key (`master.key`, Stage 3a),
    /// so they all derive the same HMAC key deterministically. This
    /// authenticates gossip datagrams (proving the sender holds the cluster
    /// secret) but provides no confidentiality. The master key is secret, so
    /// this is a genuine shared secret — unlike the earlier root-CA derivation,
    /// where the CA cert is public.
    pub fn derive_gossip_key(ikm: &[u8; 32]) -> hmac::Key {
        let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, b"reliaburger-gossip-hmac-v1");
        let prk = salt.extract(ikm);

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
    use crate::sesame::ca::{self, CaHierarchy};
    use crate::sesame::types::{CrlEntry, SerialNumber};
    use std::time::SystemTime;

    fn test_hierarchy(cluster: &str) -> CaHierarchy {
        ca::generate_ca_hierarchy(cluster, b"ikm").unwrap()
    }

    fn identity_from(hierarchy: &CaHierarchy, node_id: &str, serial: u64) -> NodeIdentity {
        let (cert_der, key_der, serial) = ca::issue_node_cert(
            node_id,
            SerialNumber(serial),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        let now = SystemTime::now();
        NodeIdentity {
            node_id: node_id.to_string(),
            certificate_der: cert_der,
            private_key_der: key_der,
            serial,
            ca_generation: 0,
            node_ca_der: hierarchy.node.ca.certificate_der.clone(),
            root_ca_der: hierarchy.root.ca.certificate_der.clone(),
            not_before: now,
            not_after: now + std::time::Duration::from_secs(365 * 24 * 3600),
        }
    }

    /// A client identity whose leaf is a **workload** certificate (issued by
    /// the Workload CA, with ClientAuth EKU) presenting `[wl-leaf, workload-ca]`.
    /// It chains to the same Root as node certs — the trap PKI2 guards against.
    fn workload_client_identity(hierarchy: &CaHierarchy) -> NodeIdentity {
        let (cert_der, key_der, serial) = ca::issue_end_entity_cert(
            "spiffe-workload",
            SerialNumber(99),
            std::time::Duration::from_secs(3600),
            &[],
            &[
                rcgen::ExtendedKeyUsagePurpose::ClientAuth,
                rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            ],
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
        )
        .unwrap();
        let now = SystemTime::now();
        NodeIdentity {
            node_id: "spiffe-workload".to_string(),
            certificate_der: cert_der,
            private_key_der: key_der,
            serial,
            ca_generation: 0,
            // Presents the Workload CA as its chain intermediate.
            node_ca_der: hierarchy.workload.ca.certificate_der.clone(),
            root_ca_der: hierarchy.root.ca.certificate_der.clone(),
            not_before: now,
            not_after: now + std::time::Duration::from_secs(3600),
        }
    }

    fn crl_with(serial: u64) -> Crl {
        Crl {
            entries: vec![CrlEntry {
                serial: SerialNumber(serial),
                issuer: crate::sesame::types::CaRole::Node,
                revoked_at: SystemTime::now(),
                reason: "test revocation".to_string(),
            }],
            version: 1,
            updated_at: SystemTime::now(),
        }
    }

    /// Run a full TLS handshake plus one round trip over an in-memory
    /// duplex pipe. Returns Ok(()) only if both sides complete.
    async fn try_handshake(
        server_config: Arc<ServerConfig>,
        client_config: Arc<ClientConfig>,
    ) -> Result<(), String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client_io, server_io) = tokio::io::duplex(16 * 1024);
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
        let connector = tokio_rustls::TlsConnector::from(client_config);
        // The pinned verifier ignores the name; WebPki-based configs in
        // these tests never get far enough to check it.
        let name = ServerName::try_from("peer.internal").unwrap();

        let server = async {
            let mut tls = acceptor
                .accept(server_io)
                .await
                .map_err(|e| e.to_string())?;
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
            tls.write_all(b"pong").await.map_err(|e| e.to_string())?;
            tls.shutdown().await.ok();
            Ok::<(), String>(())
        };
        let client = async {
            let mut tls = connector
                .connect(name, client_io)
                .await
                .map_err(|e| e.to_string())?;
            tls.write_all(b"ping").await.map_err(|e| e.to_string())?;
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await.map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        };

        let (s, c) = tokio::join!(server, client);
        s?;
        c?;
        Ok(())
    }

    #[tokio::test]
    async fn mtls_handshake_succeeds_between_two_issued_node_certs() {
        let hierarchy = test_hierarchy("test");
        let server_id = identity_from(&hierarchy, "node-01", 10);
        let client_id = identity_from(&hierarchy, "node-02", 11);

        let server = build_mtls_server_config(&server_id, CrlHandle::default()).unwrap();
        let client = build_mtls_client_config(&client_id, CrlHandle::default()).unwrap();

        try_handshake(server, client).await.unwrap();
    }

    #[tokio::test]
    async fn mtls_server_refuses_a_client_without_a_certificate() {
        let hierarchy = test_hierarchy("test");
        let server_id = identity_from(&hierarchy, "node-01", 10);
        let server = build_mtls_server_config(&server_id, CrlHandle::default()).unwrap();

        // A client that trusts the cluster CAs but presents no certificate.
        let verifier = Arc::new(PinnedChainServerVerifier::new(
            server_id.node_ca_der.clone(),
            server_id.root_ca_der.clone(),
            CrlHandle::default(),
        ));
        let client = Arc::new(
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth(),
        );

        let err = try_handshake(server, client).await.unwrap_err();
        assert!(
            err.to_lowercase().contains("certificate"),
            "expected a certificate-required failure, got: {err}"
        );
    }

    #[tokio::test]
    async fn mtls_server_refuses_a_client_cert_from_a_foreign_ca() {
        let hierarchy = test_hierarchy("test");
        let foreign = test_hierarchy("intruder");
        let server_id = identity_from(&hierarchy, "node-01", 10);
        let mut client_id = identity_from(&foreign, "node-99", 10);
        // The intruder knows the real cluster's CA certs (they're public)
        // but its own cert is signed by a different Node CA.
        client_id.node_ca_der = foreign.node.ca.certificate_der.clone();
        client_id.root_ca_der = hierarchy.root.ca.certificate_der.clone();

        let server = build_mtls_server_config(&server_id, CrlHandle::default()).unwrap();
        let client = build_mtls_client_config(&client_id, CrlHandle::default()).unwrap();

        try_handshake(server, client).await.unwrap_err();
    }

    #[tokio::test]
    async fn mtls_server_refuses_a_workload_cert_presented_as_a_node() {
        // PKI2: a workload certificate chains to the same Root and carries
        // ClientAuth EKU. Pinning the client verifier to the Node CA (not the
        // Root) must reject it — a workload identity is not a node identity.
        let hierarchy = test_hierarchy("test");
        let server_id = identity_from(&hierarchy, "node-01", 10);
        let workload_id = workload_client_identity(&hierarchy);

        let server = build_mtls_server_config(&server_id, CrlHandle::default()).unwrap();
        let client = build_mtls_client_config(&workload_id, CrlHandle::default()).unwrap();

        try_handshake(server, client)
            .await
            .expect_err("a workload cert must not authenticate as a node");
    }

    #[tokio::test]
    async fn mtls_server_refuses_a_revoked_client_cert() {
        let hierarchy = test_hierarchy("test");
        let server_id = identity_from(&hierarchy, "node-01", 10);
        let client_id = identity_from(&hierarchy, "node-02", 11);

        let server = build_mtls_server_config(&server_id, CrlHandle::new(crl_with(11))).unwrap();
        let client = build_mtls_client_config(&client_id, CrlHandle::default()).unwrap();

        let err = try_handshake(server, client).await.unwrap_err();
        assert!(
            err.to_lowercase().contains("revoked") || err.to_lowercase().contains("certificate"),
            "expected a revocation failure, got: {err}"
        );
    }

    #[tokio::test]
    async fn client_refuses_a_server_whose_cert_is_revoked() {
        let hierarchy = test_hierarchy("test");
        let server_id = identity_from(&hierarchy, "node-01", 10);
        let client_id = identity_from(&hierarchy, "node-02", 11);

        let server = build_mtls_server_config(&server_id, CrlHandle::default()).unwrap();
        let client = build_mtls_client_config(&client_id, CrlHandle::new(crl_with(10))).unwrap();

        try_handshake(server, client).await.unwrap_err();
    }

    #[tokio::test]
    async fn crl_updates_apply_to_new_handshakes_without_rebuilding_configs() {
        let hierarchy = test_hierarchy("test");
        let server_id = identity_from(&hierarchy, "node-01", 10);
        let client_id = identity_from(&hierarchy, "node-02", 11);

        let server_crl = CrlHandle::default();
        let server = build_mtls_server_config(&server_id, server_crl.clone()).unwrap();
        let client = build_mtls_client_config(&client_id, CrlHandle::default()).unwrap();

        // First handshake succeeds with an empty CRL.
        try_handshake(server.clone(), client.clone()).await.unwrap();

        // Revoke the client's serial through the shared handle — the same
        // ServerConfig must now refuse the same client.
        server_crl.update(crl_with(11));
        try_handshake(server, client).await.unwrap_err();
    }

    /// Handshake helper that binds the client to an expected node id (PKI3).
    async fn try_handshake_bound(
        server_id: &NodeIdentity,
        client_id: &NodeIdentity,
        expected_node_id: &str,
    ) -> Result<(), String> {
        let server = build_mtls_server_config(server_id, CrlHandle::default()).unwrap();
        let client =
            build_mtls_client_config_bound(client_id, CrlHandle::default(), expected_node_id)
                .unwrap();
        try_handshake(server, client).await
    }

    #[tokio::test]
    async fn bound_verifier_accepts_the_expected_node_id() {
        // PKI3: the server is node-01; the client dials expecting node-01.
        let hierarchy = test_hierarchy("test");
        let server_id = identity_from(&hierarchy, "node-01", 10);
        let client_id = identity_from(&hierarchy, "node-02", 11);
        try_handshake_bound(&server_id, &client_id, "node-01")
            .await
            .expect("matching node id should pass");
    }

    #[tokio::test]
    async fn bound_verifier_rejects_a_valid_cert_for_the_wrong_node() {
        // PKI3: node-99 holds a perfectly valid Node-CA cert, but a client that
        // meant to reach node-01 must refuse it — one compromised node cannot
        // impersonate another just by presenting its own valid cert.
        let hierarchy = test_hierarchy("test");
        let wrong_server = identity_from(&hierarchy, "node-99", 12);
        let client_id = identity_from(&hierarchy, "node-02", 11);
        let err = try_handshake_bound(&wrong_server, &client_id, "node-01")
            .await
            .expect_err("a valid cert for the wrong node must be rejected");
        assert!(
            err.to_lowercase().contains("certificate") || err.to_lowercase().contains("node-id"),
            "expected a node-id binding failure, got: {err}"
        );
    }

    #[tokio::test]
    async fn bound_verifier_rejects_a_leaf_without_a_node_id_san() {
        // A node cert issued the legacy way (no node-id URI SAN) fails the
        // binding check: absence is treated as mismatch under the bound mode.
        let hierarchy = test_hierarchy("test");
        let mut server_id = identity_from(&hierarchy, "node-01", 10);
        // Re-issue the server leaf without the node-id URI SAN.
        let (cert_der, key_der, serial) = ca::issue_end_entity_cert(
            "node-01",
            SerialNumber(20),
            std::time::Duration::from_secs(3600),
            &[],
            &[
                rcgen::ExtendedKeyUsagePurpose::ServerAuth,
                rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            ],
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        server_id.certificate_der = cert_der;
        server_id.private_key_der = key_der;
        server_id.serial = serial;
        let client_id = identity_from(&hierarchy, "node-02", 11);

        try_handshake_bound(&server_id, &client_id, "node-01")
            .await
            .expect_err("a leaf without a node-id SAN must be rejected under binding");
    }

    #[test]
    fn build_server_auth_only_config_succeeds() {
        let hierarchy = test_hierarchy("test");
        let _config =
            build_server_auth_only_client_config(&hierarchy.root.ca.certificate_der).unwrap();
    }

    #[test]
    fn gossip_hmac_verifies_a_tag_it_signed() {
        let key = gossip_hmac::derive_gossip_key(&[7u8; 32]);
        let payload = b"gossip ping message";
        let tag = gossip_hmac::sign(&key, payload);
        assert!(gossip_hmac::verify(&key, payload, &tag));
    }

    #[test]
    fn gossip_hmac_rejects_a_tampered_payload() {
        let key = gossip_hmac::derive_gossip_key(&[7u8; 32]);
        let tag = gossip_hmac::sign(&key, b"original message");
        assert!(!gossip_hmac::verify(&key, b"tampered message", &tag));
    }

    #[test]
    fn derive_gossip_key_yields_different_keys_for_different_ikm() {
        let key_a = gossip_hmac::derive_gossip_key(&[1u8; 32]);
        let key_b = gossip_hmac::derive_gossip_key(&[2u8; 32]);
        let payload = b"message from cluster A";
        let tag = gossip_hmac::sign(&key_a, payload);
        // A key derived from a different master secret must not verify A's tag.
        assert!(!gossip_hmac::verify(&key_b, payload, &tag));
    }

    #[test]
    fn derive_gossip_key_yields_same_key_for_same_ikm() {
        let key1 = gossip_hmac::derive_gossip_key(&[9u8; 32]);
        let key2 = gossip_hmac::derive_gossip_key(&[9u8; 32]);
        let tag = gossip_hmac::sign(&key1, b"test");
        // Same master secret → same key → cross-verifies.
        assert!(gossip_hmac::verify(&key2, b"test", &tag));
    }
}
