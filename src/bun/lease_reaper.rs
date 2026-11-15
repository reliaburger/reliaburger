//! The leader's reaper for agent leases (#1686).
//!
//! A lapsed lease already stops counting the moment its time passes: every
//! rule in [`crate::proposal::lease`] reads the expiry against the clock in
//! the request. The reaper removes it from the council, so `relish agent
//! context` and `GET /v1/leases` stop listing it, the lease generation
//! moves, and the audit log gets its `lease.expired` record.
//!
//! Only the leader writes, and it checks its own copy first, so a quiet
//! cluster writes no Raft entry per tick. A new leader picks up where the
//! old one stopped, because the leases live in the council.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::bun::events::{AuditEvent, EventKind, EventSeverity, EventStore};
use crate::council::{CouncilError, CouncilNode, CouncilResponse, RaftRequest};
use crate::proposal::lease::{AgentLease, LeaseOutcome};

/// How often the leader looks for lapsed leases. The shortest lease is 30
/// seconds, so a second's lag is noise.
pub const LEASE_REAPER_INTERVAL: Duration = Duration::from_secs(1);

/// The principal expiry records name: the cluster itself.
pub const REAPER_PRINCIPAL: &str = "system";

/// Expire every lease lapsed by `now_unix_ms`, if this node leads, and
/// record each in the audit log. Returns the leases it removed.
pub async fn expire_leases_once(
    council: &CouncilNode,
    events: Option<&Arc<RwLock<EventStore>>>,
    node: &str,
    now_unix_ms: u64,
) -> Result<Vec<AgentLease>, CouncilError> {
    // One round: the loop asks again next tick (#1393).
    if !council.is_leader().await {
        return Ok(Vec::new());
    }
    if !council
        .read_desired(|desired| desired.agent_leases.any_lapsed(now_unix_ms))
        .await
    {
        return Ok(Vec::new());
    }
    let expired = match council
        .write(RaftRequest::LeaseExpire { now_unix_ms })
        .await?
    {
        CouncilResponse::AgentLease(outcome) => match *outcome {
            LeaseOutcome::Expired { leases } => leases,
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    if let Some(events) = events {
        // Release the event ring's lock before waiting for the disk.
        let receipts: Vec<_> = {
            let mut store = events.write().await;
            expired
                .iter()
                .map(|lease| store.record_audit(expiry_event(lease, node, now_unix_ms)))
                .collect()
        };
        for receipt in receipts {
            if !receipt.written().await {
                eprintln!("bun: a lease.expired audit record is not on disk");
            }
        }
    }
    Ok(expired)
}

/// The `lease.expired` audit record for `lease`.
pub fn expiry_event(lease: &AgentLease, node: &str, now_unix_ms: u64) -> AuditEvent {
    AuditEvent {
        timestamp: now_unix_ms / 1000,
        kind: EventKind::Operation,
        severity: EventSeverity::Info,
        action: "lease.expired".to_string(),
        principal: REAPER_PRINCIPAL.to_string(),
        principal_kind: crate::sesame::types::PrincipalKind::System,
        app: lease.target.app_name().map(str::to_string),
        namespace: Some(lease.target.namespace_name().to_string()),
        node: Some(node.to_string()),
        source_address: None,
        details: lease.audit_details(),
        message: format!("{}'s lease on {} expired", lease.holder.name, lease.target),
    }
}

/// Expire lapsed leases every [`LEASE_REAPER_INTERVAL`] until `shutdown`.
/// Followers tick too, and do nothing.
pub fn spawn_lease_reaper(
    council: Arc<CouncilNode>,
    events: Option<Arc<RwLock<EventStore>>>,
    node: String,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(LEASE_REAPER_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => {}
            }
            let now = crate::testkit::lease::now_unix_millis();
            if let Err(error) = expire_leases_once(&council, events.as_ref(), &node, now).await {
                eprintln!("bun: agent lease expiry deferred: {error}");
            }
        }
    })
}
