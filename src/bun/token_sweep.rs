//! The expiry sweep for API tokens (F05 I2).
//!
//! An expired token already gets 401, so the sweep isn't what stops it
//! working. It keeps the store from filling with dead credentials that
//! `relish token list` would show forever. The leader proposes
//! [`RaftRequest::SweepExpiredApiTokens`] on a slow timer; the state machine
//! decides what goes (see [`crate::sesame::token::tokens_to_sweep`]), never
//! the last Admin and never the whole store, and the leader records one
//! audit event per token it removed.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::bun::events::{AuditEvent, EventKind, EventSeverity, EventStore};
use crate::council::{CouncilError, CouncilNode, CouncilResponse, RaftRequest};

/// How often the leader looks for expired tokens. Tokens stay a day past
/// their expiry anyway, so an hour's lag is noise.
pub const TOKEN_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The principal audit events name for what the cluster does by itself.
pub const SWEEP_PRINCIPAL: &str = "system";

/// Sweep once, as of `now`, if this node leads; returns the removed names.
///
/// A follower does nothing, and so does a leader with nothing due: the
/// check runs on the leader's own copy of the store first, so a quiet
/// cluster writes no Raft entry per hour.
pub async fn sweep_expired_tokens_once(
    council: &CouncilNode,
    events: Option<&Arc<RwLock<EventStore>>>,
    node: &str,
    now: SystemTime,
) -> Result<Vec<String>, CouncilError> {
    if !council.is_leader().await {
        return Ok(Vec::new());
    }
    let now_unix_ms = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let tokens = council.security_state().await.api_tokens;
    if crate::sesame::token::tokens_to_sweep(&tokens, now_unix_ms).is_empty() {
        return Ok(Vec::new());
    }
    let removed = match council
        .write(RaftRequest::SweepExpiredApiTokens { now_unix_ms })
        .await?
    {
        CouncilResponse::ApiTokensSwept { removed } => removed,
        _ => Vec::new(),
    };
    if let Some(events) = events {
        let timestamp = now_unix_ms / 1000;
        let mut store = events.write().await;
        for name in &removed {
            store.record_audit(AuditEvent {
                timestamp,
                kind: EventKind::Token,
                severity: EventSeverity::Info,
                action: "token.expired_swept".to_string(),
                principal: SWEEP_PRINCIPAL.to_string(),
                app: None,
                namespace: None,
                node: Some(node.to_string()),
                details: std::collections::BTreeMap::from([("token".to_string(), name.clone())]),
                message: format!("expired API token {name} removed"),
            });
        }
    }
    Ok(removed)
}

/// Sweep every [`TOKEN_SWEEP_INTERVAL`] until `shutdown`.
pub async fn run_token_sweep_loop(
    council: Arc<CouncilNode>,
    events: Option<Arc<RwLock<EventStore>>>,
    node: String,
    shutdown: CancellationToken,
) {
    let mut ticker = tokio::time::interval(TOKEN_SWEEP_INTERVAL);
    // The first tick fires at once; a node that just started has better
    // things to do than sweep, so wait a full interval.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = ticker.tick() => {}
        }
        if let Err(error) =
            sweep_expired_tokens_once(&council, events.as_ref(), &node, SystemTime::now()).await
        {
            eprintln!("bun: token expiry sweep failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::types::{ApiRole, ApiToken, TokenScope};

    async fn leader_council() -> Arc<CouncilNode> {
        use std::collections::BTreeMap;

        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::{CouncilConfig, CouncilNodeInfo};

        let raft_router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, raft_router.clone());
        let node = CouncilNode::new(
            1,
            CouncilConfig::default(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        raft_router.register(1, node.raft().clone()).await;
        let members = BTreeMap::from([(
            1,
            CouncilNodeInfo {
                addr: "127.0.0.1:9444".parse().unwrap(),
                name: "node-1".into(),
            },
        )]);
        node.initialize(members).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !node.is_leader().await {
            assert!(tokio::time::Instant::now() < deadline, "no leader");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Arc::new(node)
    }

    fn token(name: &str, role: ApiRole, expires_at: Option<SystemTime>) -> ApiToken {
        ApiToken {
            name: name.to_string(),
            token_hash: name.as_bytes().to_vec(),
            token_salt: Vec::new(),
            role,
            scope: TokenScope::default(),
            expires_at,
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[tokio::test]
    async fn the_leader_sweeps_an_expired_token_and_audits_it_as_the_system() {
        let council = leader_council().await;
        let now = SystemTime::now();
        let long_ago = now - Duration::from_secs(3 * 86_400);
        for token in [
            token("admin", ApiRole::Admin, Some(long_ago)),
            token("ci", ApiRole::Deployer, Some(long_ago)),
            token(
                "fresh",
                ApiRole::ReadOnly,
                Some(now - Duration::from_secs(60)),
            ),
        ] {
            council
                .write(RaftRequest::CreateApiToken(token))
                .await
                .unwrap();
        }
        let events = Arc::new(RwLock::new(EventStore::new()));

        let removed = sweep_expired_tokens_once(&council, Some(&events), "node-1", now)
            .await
            .unwrap();

        assert_eq!(removed, ["ci"], "the last Admin stays, however expired");
        let left: Vec<String> = council
            .security_state()
            .await
            .api_tokens
            .into_iter()
            .map(|token| token.name)
            .collect();
        assert_eq!(left, ["admin", "fresh"]);
        let audit = events.read().await.recent(10, None, None);
        assert_eq!(audit.len(), 1, "{audit:?}");
        assert_eq!(audit[0].action.as_deref(), Some("token.expired_swept"));
        assert_eq!(audit[0].principal.as_deref(), Some(SWEEP_PRINCIPAL));
        assert_eq!(
            audit[0].details.get("token").map(String::as_str),
            Some("ci")
        );
    }

    #[tokio::test]
    async fn a_sweep_with_nothing_due_writes_nothing() {
        let council = leader_council().await;
        council
            .write(RaftRequest::CreateApiToken(token(
                "admin",
                ApiRole::Admin,
                None,
            )))
            .await
            .unwrap();
        let before = council.metrics().borrow().last_applied;

        let removed = sweep_expired_tokens_once(&council, None, "node-1", SystemTime::now())
            .await
            .unwrap();

        assert!(removed.is_empty());
        assert_eq!(council.metrics().borrow().last_applied, before);
    }

    #[test]
    fn the_sweep_runs_hourly() {
        assert_eq!(TOKEN_SWEEP_INTERVAL, Duration::from_secs(3_600));
    }
}
