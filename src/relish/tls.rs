//! Shared cluster-CA policy for HTTP and WebSocket clients.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};

/// Build a client that verifies certificate chains against only the supplied
/// cluster CAs, and accepts only a cluster node's certificate. Cluster nodes
/// are reached through forwarded IP addresses, so their node-name
/// certificates cannot use ordinary DNS-name verification; the node-id URI
/// SAN stands in for the name (#2396).
pub(super) fn cluster_config(roots: RootCertStore) -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = ClusterCaVerifier {
        roots,
        algorithms: provider.signature_verification_algorithms,
    };
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("failed to configure cluster TLS: {error}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The server's certificate chains to the cluster CA but isn't a node's: a
/// workload's or an ingress certificate, say.
#[derive(Debug, thiserror::Error)]
#[error("the server's certificate is not a cluster node's")]
struct NotANodeCertificate;

#[derive(Debug)]
struct ClusterCaVerifier {
    roots: RootCertStore,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for ClusterCaVerifier {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let certificate = rustls::server::ParsedCertificate::try_from(leaf)?;
        rustls::client::verify_server_cert_signed_by_trust_anchor(
            &certificate,
            &self.roots,
            intermediates,
            now,
            self.algorithms.all,
        )?;
        // The root also signs the Workload and Ingress CAs, whose leaves
        // carry server auth and whose keys live in workloads. Only a node
        // certificate carries a node-id URI SAN, and only the Node CA signs
        // one, so this is what makes the server a node and not merely
        // "something under the root". The bearer token goes out only after.
        if crate::sesame::mtls::node_id_from_leaf(leaf).is_none() {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::Other(rustls::OtherError(Arc::new(NotANodeCertificate))),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, certificate, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, certificate, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
