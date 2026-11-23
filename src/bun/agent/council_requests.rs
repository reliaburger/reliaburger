//! Council writes a command needs before it can answer, run off the agent
//! loop (#351, stage 3).
//!
//! A join consumes its token through a Raft write, and signing an image
//! attaches the signature through one. With quorum that's tens of
//! milliseconds. Without it, openraft's `client_write` waits until the leader
//! steps down, or longer, and on the loop that wait held every caller on the
//! node. Now the loop only checks that the node has a council, which is in
//! memory, and a task makes the request under [`COUNCIL_ANSWER_TIMEOUT`].

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use super::{BunAgent, BunError, CouncilNode, Grill};

/// How long a join or a signature waits for the council before the caller
/// hears an error. A council without quorum shouldn't leave the request
/// hanging, and nothing on the loop waits for it any more.
pub(super) const COUNCIL_ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything a join needs from the agent, copied out of it on the loop.
struct JoinAuthority {
    council: Arc<CouncilNode>,
    wrapping_ikm: [u8; 32],
    node_leaf_lifetime: Duration,
}

fn security_error(reason: impl Into<String>) -> BunError {
    BunError::SecurityError {
        reason: reason.into(),
    }
}

/// Validate a join token, consume it through the council, and sign the
/// joiner's CSR. The consume is the one Raft write.
///
/// The leader proves it leads before the consume, not after: a join that
/// failed once the single-use token was spent would leave the joiner with
/// nothing to retry with (#1937).
async fn issue_join_bundle(
    authority: JoinAuthority,
    token: String,
    node_id: String,
    csr_der: Vec<u8>,
    address: Option<std::net::IpAddr>,
) -> Result<crate::sesame::join::JoinBundle, BunError> {
    let council = &authority.council;
    // A leader that can't prove it leads refuses here, with the token
    // unspent. The check is current, so a token consumed or a node retired
    // on another leader is refused too. The authoritative consume is below.
    let security_state = council
        .security_state_confirmed(crate::council::LEADER_CONFIRMATION)
        .await
        .map_err(|error| security_error(error.to_string()))?;
    let token_hash = crate::sesame::join::check_join_token(&token, &node_id, &security_state)
        .map_err(|e| security_error(format!("join validation failed: {e}")))?;

    // Atomically consume the token and allocate a serial in one committed
    // Raft entry (PKI5). Two racing joiners with the same token: exactly one
    // gets a serial here; the loser is refused, so a token issues one cert.
    let serial = match council
        .write(crate::council::RaftRequest::ConsumeJoinTokenForIssue { token_hash })
        .await
        .map_err(|e| security_error(format!("failed to consume join token: {e}")))?
    {
        crate::council::CouncilResponse::JoinTokenConsumed { serial } => {
            crate::sesame::types::SerialNumber(serial)
        }
        crate::council::CouncilResponse::Refused { reason } => {
            return Err(security_error(format!("join refused: {reason}")));
        }
        other => {
            return Err(security_error(format!(
                "unexpected council response to join: {other:?}"
            )));
        }
    };

    // The write returns once this leader has applied it, so its own state is
    // at least as new as the consume: a node retired before it is refused
    // below, and no second leadership round can fail with the token spent.
    let security_state = council.security_state().await;
    let join_result = crate::sesame::join::sign_join_csr(
        &csr_der,
        &node_id,
        serial,
        authority.node_leaf_lifetime,
        &security_state,
        &authority.wrapping_ikm,
        address,
    )
    .map_err(|e| security_error(format!("join signing failed: {e}")))?;

    Ok(crate::sesame::join::JoinBundle::from_result(&join_result))
}

/// Verify an operator's detached signature and attach it to the manifest
/// through the council.
async fn attach_image_signature(
    council: Arc<CouncilNode>,
    trusted_keys: Vec<String>,
    submission: crate::pickle::signing::SignatureSubmission,
) -> Result<String, BunError> {
    let public_key = submission.public_key.clone();
    let (digest, signature) = submission
        .into_verified()
        .map_err(|e| security_error(format!("signature rejected: {e}")))?;
    let fingerprint = match &signature.method {
        crate::pickle::types::SigningMethod::ExternalKey { key_id } => key_id.clone(),
        crate::pickle::types::SigningMethod::Keyless { identity, .. } => identity.clone(),
    };

    let attach = crate::pickle::types::AttachSignature {
        manifest_digest: digest.clone(),
        signature,
    };
    let response = council
        .write(crate::council::RaftRequest::AttachSignature(attach))
        .await
        .map_err(|e| security_error(format!("failed to attach signature: {e}")))?;
    // An unknown digest comes back as a refusal, not an error; reporting
    // success there would claim a signature that attached to nothing.
    if let crate::council::types::CouncilResponse::Refused { reason } = response {
        return Err(security_error(format!(
            "signature attach refused: {reason}"
        )));
    }

    let mut message = format!("signed {} with key {fingerprint}", digest.as_str());
    if !trusted_keys.contains(&public_key) {
        message.push_str(
            "\nwarning: this node's [images.trust_policy] keys does not list this key, so deploys here will refuse the image until it does",
        );
    }
    Ok(message)
}

/// Run `request` in a task and answer `response` with its result, or with
/// `late` once [`COUNCIL_ANSWER_TIMEOUT`] has passed.
fn answer_from_task<T: Send + 'static>(
    request: impl std::future::Future<Output = Result<T, BunError>> + Send + 'static,
    late: &'static str,
    response: oneshot::Sender<Result<T, BunError>>,
) {
    tokio::spawn(async move {
        let result = tokio::time::timeout(COUNCIL_ANSWER_TIMEOUT, request)
            .await
            .unwrap_or_else(|_| Err(security_error(late)));
        let _ = response.send(result);
    });
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// The council this node can write through, if it has one.
    fn writable_council(&self) -> Option<Arc<CouncilNode>> {
        self.cluster.as_ref()?.council.clone()
    }

    /// Issue a certificate bundle for a joining node.
    ///
    /// Runs on an existing cluster member. Validates the token against the
    /// replicated security state, consumes it via Raft, and answers with the
    /// bundle (certificate, private key, CA chain) for the joiner to persist.
    /// The joiner supplies its own `node_id`.
    pub(super) fn spawn_join_issue(
        &self,
        token: String,
        node_id: String,
        csr_der: Vec<u8>,
        address: Option<std::net::IpAddr>,
        response: oneshot::Sender<Result<crate::sesame::join::JoinBundle, BunError>>,
    ) {
        let authority = match self.join_authority() {
            Ok(authority) => authority,
            Err(error) => {
                let _ = response.send(Err(error));
                return;
            }
        };
        answer_from_task(
            issue_join_bundle(authority, token, node_id, csr_der, address),
            // The write may still commit after this, consuming the token.
            "the council did not answer the join in time; if a retry is refused, create a new join token",
            response,
        );
    }

    fn join_authority(&self) -> Result<JoinAuthority, BunError> {
        let cluster = self
            .cluster
            .as_ref()
            .ok_or_else(|| security_error("no cluster available for join validation"))?;
        let council = cluster
            .council
            .clone()
            .ok_or_else(|| security_error("no council available for join validation"))?;
        let wrapping_ikm = cluster
            .wrapping_ikm
            .ok_or_else(|| security_error("no wrapping IKM available"))?;
        Ok(JoinAuthority {
            council,
            wrapping_ikm,
            node_leaf_lifetime: self.node_leaf_lifetime,
        })
    }

    /// Answer a `SignImage` command: verify an operator's detached signature
    /// and attach it to the manifest via Raft.
    ///
    /// The node never holds the signing key, so it can't mint trust: it only
    /// checks that the signature verifies under the public key it came with.
    /// Whether that key is trusted is decided at deploy time against
    /// `[images.trust_policy] keys`. The reply warns when this node's policy
    /// doesn't list the key, because deploys here would still refuse it.
    pub(super) fn spawn_sign_image(
        &self,
        submission: crate::pickle::signing::SignatureSubmission,
        response: oneshot::Sender<Result<String, BunError>>,
    ) {
        let Some(council) = self.writable_council() else {
            let _ = response.send(Err(security_error(
                "image signatures live in the cluster catalogue; this node has no council",
            )));
            return;
        };
        answer_from_task(
            attach_image_signature(council, self.trust_policy.keys.clone(), submission),
            "the council did not attach the signature in time; it may still attach",
            response,
        );
    }
}

#[cfg(test)]
impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// `SignImage` as a caller sees it, for tests that drive the agent
    /// without its loop.
    pub(super) async fn handle_sign_image(
        &self,
        submission: crate::pickle::signing::SignatureSubmission,
    ) -> Result<String, BunError> {
        let (response, answer) = oneshot::channel();
        self.spawn_sign_image(submission, response);
        answer
            .await
            .unwrap_or_else(|_| Err(security_error("signing task dropped its answer")))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::council::network::InMemoryRaftRouter;
    use crate::sesame::types::{CertificateAuthority, SecurityState};

    const IKM: [u8; 32] = [9; 32];

    /// Three in-memory voters led by node 1, which holds [`IKM`], seeded
    /// with a CA hierarchy and a join token for `node-9`. Returns the
    /// voters, the network between them and the token's plaintext.
    async fn council_with_a_join_token() -> (Vec<Arc<CouncilNode>>, InMemoryRaftRouter, String) {
        use crate::council::log_store::MemLogStore;
        use crate::council::network::InMemoryRaftNetworkFactory;
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::{CouncilConfig, CouncilNodeInfo};

        let network = InMemoryRaftRouter::new();
        let mut nodes = Vec::new();
        let mut members = std::collections::BTreeMap::new();
        for id in 1..=3u64 {
            members.insert(
                id,
                CouncilNodeInfo {
                    addr: std::net::SocketAddr::from(([127, 0, 0, 1], 7100 + id as u16)),
                    name: format!("node-{id}"),
                },
            );
            let node = Arc::new(
                CouncilNode::new(
                    id,
                    CouncilConfig::default(),
                    InMemoryRaftNetworkFactory::new(id, network.clone()),
                    MemLogStore::new(),
                    CouncilStateMachine::new(),
                    (id == 1).then_some(IKM),
                )
                .await
                .unwrap(),
            );
            network.register(id, &node).await;
            nodes.push(node);
        }
        nodes[0].initialize(members).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !nodes[0].is_leader().await {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        let hierarchy = crate::sesame::ca::generate_ca_hierarchy("join", &IKM).unwrap();
        let (token, join_token) =
            crate::sesame::join::create_join_token(Duration::from_secs(600), "node-9").unwrap();
        let security = SecurityState {
            certificate_authorities: vec![
                CertificateAuthority {
                    private_key_wrapped: None,
                    ..hierarchy.root.ca.clone()
                },
                hierarchy.node.ca.clone(),
                hierarchy.workload.ca.clone(),
                hierarchy.ingress.ca.clone(),
            ],
            join_tokens: vec![join_token],
            next_serial: 10,
            ..SecurityState::default()
        };
        nodes[0]
            .write(crate::council::RaftRequest::SecurityStateInit(Box::new(
                security,
            )))
            .await
            .unwrap();
        (nodes, network, token)
    }

    fn authority(council: &Arc<CouncilNode>) -> JoinAuthority {
        JoinAuthority {
            council: council.clone(),
            wrapping_ikm: IKM,
            node_leaf_lifetime: Duration::from_secs(3600),
        }
    }

    async fn join(
        council: &Arc<CouncilNode>,
        token: &str,
    ) -> Result<crate::sesame::join::JoinBundle, BunError> {
        let (csr_der, _key) = crate::sesame::ca::create_node_csr("node-9").unwrap();
        issue_join_bundle(
            authority(council),
            token.into(),
            "node-9".into(),
            csr_der,
            None,
        )
        .await
    }

    // #1937: a join consumed its single-use token first and proved
    // leadership after, so a leader that couldn't prove it answered the
    // joiner with an error after spending the token. The token must still
    // be there for the retry.
    #[tokio::test]
    async fn a_leader_that_cannot_confirm_it_leads_leaves_the_join_token_for_a_retry() {
        let (nodes, network, token) = council_with_a_join_token().await;
        for node in &nodes {
            node.hold_elections_until(Instant::now() + Duration::from_secs(60));
        }
        network.partition(1, 2).await;
        network.partition(1, 3).await;

        let refused = tokio::time::timeout(Duration::from_secs(8), join(&nodes[0], &token)).await;
        assert!(
            matches!(refused, Ok(Err(_))),
            "the join should fail once the leader can't confirm it leads"
        );

        network.heal().await;
        let security = nodes[0]
            .security_state_confirmed(crate::council::LEADER_CONFIRMATION)
            .await
            .unwrap();
        crate::sesame::join::check_join_token(&token, "node-9", &security)
            .expect("the failed join spent the token");

        join(&nodes[0], &token)
            .await
            .expect("the retry should join");
        let again = join(&nodes[0], &token).await.unwrap_err();
        assert!(again.to_string().contains("consumed"), "{again}");
        for node in nodes {
            node.shutdown().await.unwrap();
        }
    }
}
