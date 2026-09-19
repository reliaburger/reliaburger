//! Node leaf renewal authorised by an existing mutually authenticated identity.

use super::{
    ca, cert, join,
    types::{CaRole, SecurityState, SerialNumber},
};
use crate::council::{CouncilNode, CouncilResponse, RaftRequest};

/// The leaf presented on this TLS connection, inserted only by the TLS listener.
/// Request headers and bodies never populate this extension. Renewal revalidates
/// it against current council state because the connection may predate revocation.
#[derive(Debug, Clone)]
pub struct TlsPeerCertificate(pub rustls::pki_types::CertificateDer<'static>);

/// A node asks to replace its leaf while retaining its authenticated node identity.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewalRequest {
    /// Required cluster protocol and state generations.
    pub compatibility: crate::compatibility::Compatibility,
    /// Base64 DER CSR; the private key remains on the requesting node.
    pub csr_b64: String,
}

/// Refusals are distinguished from retryable council/signing failures.
#[derive(Debug, thiserror::Error)]
pub enum RenewalError {
    /// The existing TLS identity is no longer authorised to renew.
    #[error("node renewal identity refused: {0}")]
    Identity(String),
    /// The CSR does not describe the authenticated node.
    #[error("invalid node renewal request: {0}")]
    Request(String),
    /// This member cannot currently prove authority or issue a certificate.
    #[error("node renewal unavailable: {0}")]
    Unavailable(String),
}

/// Issue a replacement after fresh quorum-backed identity validation and serial
/// allocation. A follower never forwards a peer identity through its own TLS
/// connection; the requesting node must retry directly against the leader.
pub async fn issue_renewal(
    council: &CouncilNode,
    peer: &TlsPeerCertificate,
    request: &RenewalRequest,
) -> Result<join::JoinBundle, RenewalError> {
    use base64::Engine as _;
    request
        .compatibility
        .require_current()
        .map_err(|error| RenewalError::Request(error.to_string()))?;
    if request.csr_b64.len() > 16 * 1024 {
        return Err(RenewalError::Request("CSR exceeds 16 KiB".into()));
    }
    let csr = base64::engine::general_purpose::STANDARD
        .decode(&request.csr_b64)
        .map_err(|error| RenewalError::Request(error.to_string()))?;
    let state = council
        .security_state_linearizable()
        .await
        .map_err(|error| RenewalError::Unavailable(error.to_string()))?;
    let node_id = validate_peer(peer, &state)?;
    let csr_der = rustls::pki_types::CertificateSigningRequestDer::from(csr.clone());
    let parsed = rcgen::CertificateSigningRequestParams::from_der(&csr_der)
        .map_err(|error| RenewalError::Request(error.to_string()))?;
    let expected_uri = ca::node_spiffe_uri(&node_id);
    if !parsed
        .params
        .subject_alt_names
        .iter()
        .any(|name| matches!(name, rcgen::SanType::URI(uri) if uri.as_str() == expected_uri))
    {
        return Err(RenewalError::Request(
            "CSR does not name the authenticated node".into(),
        ));
    }
    let wrapping_ikm = *council
        .wrapping_ikm()
        .ok_or_else(|| RenewalError::Unavailable("no wrapping key available".into()))?;
    let serial = match council
        .write(RaftRequest::AllocateNodeSerial {
            node_id: node_id.clone(),
        })
        .await
        .map_err(|error| RenewalError::Unavailable(error.to_string()))?
    {
        CouncilResponse::SerialAllocated { serial } => SerialNumber(serial),
        _ => {
            return Err(RenewalError::Unavailable(
                "serial allocation refused".into(),
            ));
        }
    };
    let state = council
        .security_state_linearizable()
        .await
        .map_err(|error| RenewalError::Unavailable(error.to_string()))?;
    validate_peer(peer, &state)?;
    let result = tokio::task::spawn_blocking(move || {
        join::sign_join_csr(&csr, &node_id, serial, &state, &wrapping_ikm)
    })
    .await
    .map_err(|error| RenewalError::Unavailable(error.to_string()))?
    .map_err(|error| RenewalError::Unavailable(error.to_string()))?;
    Ok(join::JoinBundle::from_result(&result))
}

fn validate_peer(peer: &TlsPeerCertificate, state: &SecurityState) -> Result<String, RenewalError> {
    let node_ca = state
        .get_ca(CaRole::Node)
        .ok_or_else(|| RenewalError::Unavailable("Node CA is unavailable".into()))?;
    let root_ca = state
        .get_ca(CaRole::Root)
        .ok_or_else(|| RenewalError::Unavailable("Root CA is unavailable".into()))?;
    cert::validate_chain(&peer.0, &node_ca.certificate_der, &root_ca.certificate_der)
        .map_err(|error| RenewalError::Identity(error.to_string()))?;
    cert::check_issuer_binding(&peer.0, &node_ca.certificate_der)
        .and_then(|()| {
            cert::check_issuer_binding(&node_ca.certificate_der, &root_ca.certificate_der)
        })
        .map_err(|error| RenewalError::Identity(error.to_string()))?;
    for certificate in [
        peer.0.as_ref(),
        &node_ca.certificate_der,
        &root_ca.certificate_der,
    ] {
        let (_, parsed) = x509_parser::parse_x509_certificate(certificate)
            .map_err(|error| RenewalError::Identity(error.to_string()))?;
        if parsed.serial.to_bytes_be().len() > 8 {
            return Err(RenewalError::Identity(
                "serial exceeds the node revocation format".into(),
            ));
        }
        let serial = cert::serial_from_der(certificate)
            .map_err(|error| RenewalError::Identity(error.to_string()))?;
        cert::check_crl(serial, &state.crl)
            .map_err(|error| RenewalError::Identity(error.to_string()))?;
    }
    let uris = cert::subject_uri_sans(&peer.0)
        .map_err(|error| RenewalError::Identity(error.to_string()))?;
    let [uri] = uris.as_slice() else {
        return Err(RenewalError::Identity(
            "expected exactly one node URI".into(),
        ));
    };
    let node_id = ca::node_id_from_spiffe_uri(uri)
        .filter(|node| !node.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| RenewalError::Identity("certificate does not identify a node".into()))?;
    if state.crl.retired_nodes.contains_key(&node_id) {
        return Err(RenewalError::Identity("node identity is retired".into()));
    }
    Ok(node_id)
}
