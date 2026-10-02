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
async fn issue_join_bundle(
    authority: JoinAuthority,
    token: String,
    node_id: String,
    csr_der: Vec<u8>,
) -> Result<crate::sesame::join::JoinBundle, BunError> {
    let council = &authority.council;
    // Fast-fail check against the replicated state (unknown, expired or
    // already consumed token). The authoritative consume happens below.
    let security_state = council.security_state().await;
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

    // Confirm the identity still has authority after consuming the token.
    let security_state = council
        .security_state_linearizable()
        .await
        .map_err(|error| security_error(error.to_string()))?;
    let join_result = crate::sesame::join::sign_join_csr(
        &csr_der,
        &node_id,
        serial,
        authority.node_leaf_lifetime,
        &security_state,
        &authority.wrapping_ikm,
    )
    .map_err(|e| security_error(format!("join signing failed: {e}")))?;

    let mut bundle = crate::sesame::join::JoinBundle::from_result(&join_result);
    bundle.recovery_epoch = council.desired_state().await.recovery_epoch;
    Ok(bundle)
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
            issue_join_bundle(authority, token, node_id, csr_der),
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
