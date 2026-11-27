//! The security state for a node with no Raft seat (#1477).
//!
//! A node outside the council has nothing applied: no API tokens, no CRL, no
//! trust set, no age keys. Its listener stayed closed waiting for tokens that
//! never came, and a revocation or a rotated CA never reached it. So such a
//! node asks the leader for [`SecurityState::node_view`] over mTLS, every
//! time Bun's security refresh ticks, and the council handle serves that view
//! to every reader until the node's own replica holds the state.
//!
//! The leader's address comes from gossip, and gossip is authenticated only
//! by a key every node holds, so a hint proves nothing (#2406). A node takes
//! a view only from a node that is a voter of the council it knows (its own
//! membership, or the council it expects while it has none, see
//! [`crate::council::admission::ExpectedCouncil`]), over TLS bound to that
//! voter's node id. Anything else is refused before the service token
//! leaves the node.

use std::net::SocketAddr;
use std::time::Duration;

use super::credentials::LiveNodeIdentity;
use super::mtls::{self, CrlHandle, MtlsError};
use super::renewal::{RenewalError, TlsPeerCertificate, validate_peer};
use super::types::SecurityState;
use crate::council::CouncilNode;
use crate::council::admission::ExpectedCouncil;

/// The leader's answer: the view, with the generations it was read under.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityView {
    /// Required cluster protocol and state generations.
    pub compatibility: crate::compatibility::Compatibility,
    /// [`SecurityState::node_view`] of the leader's committed state.
    pub security: SecurityState,
}

/// The leader's side: read the state linearizably, so a deposed leader can't
/// serve a stale revocation list, check the caller's node certificate
/// against it, and answer with the node's view.
pub async fn issue_view(
    council: &CouncilNode,
    peer: &TlsPeerCertificate,
) -> Result<SecurityView, RenewalError> {
    let state = council
        .security_state_confirmed(crate::council::LEADER_CONFIRMATION)
        .await
        .map_err(|error| RenewalError::Unavailable(error.to_string()))?;
    validate_peer(peer, &state)?;
    Ok(SecurityView {
        compatibility: crate::compatibility::CURRENT,
        security: state.node_view(),
    })
}

/// The node's side: fetches the view from the leader with this node's live
/// mTLS identity and the cluster's service token.
pub struct SecurityViewClient {
    identity: LiveNodeIdentity,
    crl: CrlHandle,
    headers: reqwest::header::HeaderMap,
    expected: ExpectedCouncil,
}

impl SecurityViewClient {
    /// A client presenting `identity`'s current leaf, refusing redirects,
    /// that takes a view only from a voter of this node's council or of
    /// `expected`, the council it expects while it has none.
    pub fn new(
        identity: &LiveNodeIdentity,
        crl: CrlHandle,
        service_token: &str,
        expected: ExpectedCouncil,
    ) -> Result<Self, MtlsError> {
        let mut headers = reqwest::header::HeaderMap::new();
        let mut bearer = reqwest::header::HeaderValue::from_str(&format!("Bearer {service_token}"))
            .map_err(|error| {
                MtlsError::ConfigFailed(format!("security view service token: {error}"))
            })?;
        bearer.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, bearer);
        Ok(Self {
            identity: identity.clone(),
            crl,
            headers,
            expected,
        })
    }

    /// Fetch the view from `leader`, the node gossip or Raft names as
    /// leader, at its API `address`. Refused before any connection when
    /// `leader` isn't a voter of `council` (or, while `council` has no
    /// membership, of the expected council); the TLS connection must then
    /// present `leader`'s own certificate.
    pub async fn fetch(
        &self,
        council: &CouncilNode,
        leader: &str,
        address: SocketAddr,
    ) -> Result<SecurityState, String> {
        let voters = match crate::council::admission::council_voters(council.raft()).await {
            Some(voters) => voters,
            None => crate::council::admission::roster_names(&self.expected.roster().await),
        };
        if !voters.values().any(|name| name == leader) {
            return Err(format!(
                "{leader} is named as leader, but is not a council voter this node knows \
                 ({:?}); not asking it for the security view",
                voters.values().collect::<Vec<_>>()
            ));
        }
        let tls =
            (*mtls::build_live_mtls_client_config(&self.identity, self.crl.clone(), Some(leader))
                .map_err(|error| error.to_string())?)
            .clone();
        let http = crate::http_client::builder()
            .use_preconfigured_tls(tls)
            .default_headers(self.headers.clone())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| format!("security view HTTP client: {error}"))?;
        let response = http
            .get(format!("https://{address}/v1/cluster/security-view"))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "security view endpoint returned {}",
                response.status()
            ));
        }
        let view: SecurityView = response
            .json()
            .await
            .map_err(|error| format!("invalid security view: {error}"))?;
        view.compatibility
            .require_current()
            .map_err(|error| error.to_string())?;
        Ok(view.security)
    }
}
