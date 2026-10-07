//! Node leaf renewal authorised by an existing mutually authenticated identity.

use super::{
    ca, cert, join,
    trust::TrustSet,
    types::{SecurityState, SerialNumber},
};
use crate::council::{CouncilNode, CouncilResponse, RaftRequest};

/// The leaf presented on this TLS connection, inserted only by the TLS listener.
/// Request headers and bodies never populate this extension. Renewal revalidates
/// it against current council state because the connection may predate revocation.
#[derive(Debug, Clone)]
pub struct TlsPeerCertificate(pub rustls::pki_types::CertificateDer<'static>);

/// The node leaf lifetime this member signs renewals with, installed by Bun as
/// a router extension from `[security] leaf_lifetime_override_secs`. Absent,
/// the member signs with [`ca::NODE_LEAF_LIFETIME`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeLeafLifetime(pub std::time::Duration);

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
/// The leaf lives for `lifetime`, the signing leader's configured value.
pub async fn issue_renewal(
    council: &CouncilNode,
    peer: &TlsPeerCertificate,
    request: &RenewalRequest,
    lifetime: std::time::Duration,
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
        join::sign_join_csr(&csr, &node_id, serial, lifetime, &state, &wrapping_ikm)
    })
    .await
    .map_err(|error| RenewalError::Unavailable(error.to_string()))?
    .map_err(|error| RenewalError::Unavailable(error.to_string()))?;
    Ok(join::JoinBundle::from_result(&result))
}

/// A node tells the leader it has installed the council's trust set (F04 R4),
/// so a Node CA rotation can move on: nodes renew onto the new CA only once
/// every node trusts it, and finalise waits for it too.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustAcknowledgement {
    /// Required cluster protocol and state generations.
    pub compatibility: crate::compatibility::Compatibility,
    /// The `sha256:HEX` fingerprint of every Node CA the node trusts.
    pub node_ca_fingerprints: Vec<String>,
}

/// The most Node CAs an acknowledgement may list. A rotation trusts two.
const MAX_ACKNOWLEDGED_NODE_CAS: usize = 8;

/// Record that the authenticated node trusts the active Node CA. Refused
/// unless the node lists every Node CA the council trusts, so the record
/// can't run ahead of what the node really installed. Returns the
/// acknowledged generation.
pub async fn acknowledge_trust(
    council: &CouncilNode,
    peer: &TlsPeerCertificate,
    request: &TrustAcknowledgement,
) -> Result<u64, RenewalError> {
    request
        .compatibility
        .require_current()
        .map_err(|error| RenewalError::Request(error.to_string()))?;
    if request.node_ca_fingerprints.len() > MAX_ACKNOWLEDGED_NODE_CAS {
        return Err(RenewalError::Request(format!(
            "an acknowledgement lists at most {MAX_ACKNOWLEDGED_NODE_CAS} Node CAs"
        )));
    }
    let state = council
        .security_state_linearizable()
        .await
        .map_err(|error| RenewalError::Unavailable(error.to_string()))?;
    let node_id = validate_peer(peer, &state)?;
    let trusted = state.trusted_cas(super::types::CaRole::Node);
    let generation = trusted
        .first()
        .map(|ca| ca.generation)
        .ok_or_else(|| RenewalError::Unavailable("no Node CA is available".into()))?;
    let installed = trusted.iter().all(|ca| {
        let fingerprint = super::identity_store::root_ca_fingerprint(&ca.certificate_der);
        request.node_ca_fingerprints.contains(&fingerprint)
    });
    if !installed {
        return Err(RenewalError::Request(
            "the node has not installed the council's trust set yet".into(),
        ));
    }
    match council
        .write(RaftRequest::AcknowledgeNodeTrust {
            node_id,
            generation,
        })
        .await
        .map_err(|error| RenewalError::Unavailable(error.to_string()))?
    {
        CouncilResponse::Refused { reason } => Err(RenewalError::Request(reason)),
        _ => Ok(generation),
    }
}

/// Validate the current node identity, chain, validity and retirement state
/// before authorising a control request on a potentially long-lived connection.
pub(crate) fn validate_peer(
    peer: &TlsPeerCertificate,
    state: &SecurityState,
) -> Result<String, RenewalError> {
    // Every Node CA the council trusts, so a node holding a leaf from a
    // retiring CA can still renew onto the new one (F04 R2).
    let trust = TrustSet::from_state(state)
        .ok_or_else(|| RenewalError::Unavailable("Node CA or root is unavailable".into()))?;
    let chain = trust
        .validate_node_leaf(&peer.0)
        .map_err(|error| RenewalError::Identity(error.to_string()))?;
    for certificate in [peer.0.as_ref(), chain.node_ca, chain.root] {
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
