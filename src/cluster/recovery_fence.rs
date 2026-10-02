//! Wiring the recovery fence (#424) into a running node.
//!
//! [`crate::council::fence`] holds the state machine. This module feeds it
//! from gossip and keeps it on disk: a restarted voter waits briefly for its
//! peers' advertised epochs before it serves Raft, any node that sees a newer
//! epoch in gossip fences itself, and every change is persisted so a fence
//! survives restarts. It also keeps fenced nodes out of the reconciler's
//! candidates, since a node that refuses every Raft RPC can never catch up.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::council::fence::{FenceSnapshot, FenceState, RecoveryFence};
use crate::council::node::CouncilNode;
use crate::meat::types::NodeId;
use crate::mustard::directory::NodeDirectory;
use crate::mustard::membership::MembershipSnapshot;

/// How long a restarted voter waits to hear its peers' recovery epochs over
/// gossip before it serves Raft anyway. Gossip probes every few hundred
/// milliseconds, so a reachable peer answers well inside this.
pub const STARTUP_EPOCH_PROBE: Duration = Duration::from_secs(5);

/// Whether gossip has shown an advertised epoch (or the lack of one) for
/// every peer in `peers`. An empty peer set (a single-voter council, or a
/// membership not loaded yet) is not complete: the deadline decides then.
pub fn probe_complete(directory: &NodeDirectory, peers: &BTreeSet<NodeId>) -> bool {
    !peers.is_empty()
        && peers
            .iter()
            .all(|peer| directory.recovery_epochs.contains_key(peer))
}

/// Nodes advertising an epoch older than `own`: fenced voters of a replaced
/// council. A node advertising no epoch is fresh and may join.
pub fn stale_epoch_members(directory: &NodeDirectory, own: Option<u64>) -> HashSet<NodeId> {
    let Some(own) = own else {
        return HashSet::new();
    };
    directory
        .recovery_epochs
        .iter()
        .filter(|(_, epoch)| epoch.is_some_and(|epoch| epoch < own))
        .map(|(node_id, _)| node_id.clone())
        .collect()
}

/// The voters this node's persisted membership names, other than itself.
fn persisted_peers(council: &CouncilNode, self_name: &str) -> BTreeSet<NodeId> {
    council
        .metrics()
        .borrow()
        .membership_config
        .membership()
        .nodes()
        .filter(|(_, info)| info.name != self_name)
        .map(|(_, info)| NodeId::new(&info.name))
        .collect()
}

fn log_transition(before: FenceSnapshot, after: FenceSnapshot) {
    match after.state {
        FenceState::Fenced { newer_epoch } => eprintln!(
            "council: FENCED: a recovery replaced this node's council (epoch {}) with epoch \
             {newer_epoch}; this node no longer serves Raft or accepts writes. Re-enrol it with \
             a fresh data directory",
            after.epoch
        ),
        FenceState::Serving if before.state == FenceState::Probing => eprintln!(
            "council: no peer advertises a newer recovery epoch than {}; serving raft",
            after.epoch
        ),
        FenceState::Serving if before.state == FenceState::Unclaimed => eprintln!(
            "council: joined a council at recovery epoch {}",
            after.epoch
        ),
        _ => {}
    }
}

/// Feed `fence` from gossip, end the startup probe, and persist every change.
pub fn spawn_recovery_fence_supervisor(
    fence: RecoveryFence,
    council: Arc<CouncilNode>,
    mut directory_rx: watch::Receiver<NodeDirectory>,
    self_name: String,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + STARTUP_EPOCH_PROBE;
        let mut fence_rx = fence.subscribe();
        let mut recorded = fence.snapshot();
        loop {
            {
                let directory = directory_rx.borrow_and_update();
                for epoch in directory.recovery_epochs.values().flatten() {
                    fence.observe_peer_epoch(*epoch);
                }
                if fence.snapshot().state == FenceState::Probing
                    && (tokio::time::Instant::now() >= deadline
                        || probe_complete(&directory, &persisted_peers(&council, &self_name)))
                {
                    fence.finish_probe();
                }
            }
            let current = *fence_rx.borrow_and_update();
            if current != recorded {
                log_transition(recorded, current);
                if let Err(error) = fence.persist().await {
                    eprintln!("council: could not persist the recovery fence: {error}");
                }
                recorded = current;
            }
            let probing = current.state == FenceState::Probing;
            tokio::select! {
                _ = shutdown.cancelled() => break,
                changed = directory_rx.changed() => if changed.is_err() { break },
                changed = fence_rx.changed() => if changed.is_err() { break },
                _ = tokio::time::sleep_until(deadline), if probing => {}
            }
        }
    });
}

/// The membership the council reconciler plans from: gossip's live members
/// minus fenced voters of a replaced council.
pub fn spawn_reconciler_membership_filter(
    mut membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    mut directory_rx: watch::Receiver<NodeDirectory>,
    fence: RecoveryFence,
    shutdown: CancellationToken,
) -> watch::Receiver<Vec<MembershipSnapshot>> {
    let filter = move |membership: &[MembershipSnapshot], directory: &NodeDirectory| {
        let stale = stale_epoch_members(directory, fence.snapshot().claimed_epoch());
        membership
            .iter()
            .filter(|member| !stale.contains(&member.node_id))
            .cloned()
            .collect::<Vec<_>>()
    };
    let (tx, rx) = watch::channel(filter(&membership_rx.borrow(), &directory_rx.borrow()));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                changed = membership_rx.changed() => if changed.is_err() { break },
                changed = directory_rx.changed() => if changed.is_err() { break },
            }
            let filtered = filter(&membership_rx.borrow(), &directory_rx.borrow());
            if tx.send(filtered).is_err() {
                break;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(epochs: &[(&str, Option<u64>)]) -> NodeDirectory {
        NodeDirectory {
            recovery_epochs: epochs
                .iter()
                .map(|(name, epoch)| (NodeId::new(*name), *epoch))
                .collect(),
            ..NodeDirectory::default()
        }
    }

    fn peers(names: &[&str]) -> BTreeSet<NodeId> {
        names.iter().map(|name| NodeId::new(*name)).collect()
    }

    #[test]
    fn probe_completes_once_every_persisted_peer_has_advertised() {
        let partial = directory(&[("n2", Some(0))]);
        assert!(!probe_complete(&partial, &peers(&["n2", "n3"])));
        // A wiped peer advertises no epoch; that still answers the question.
        let full = directory(&[("n2", Some(0)), ("n3", None)]);
        assert!(probe_complete(&full, &peers(&["n2", "n3"])));
    }

    #[test]
    fn probe_without_known_peers_waits_for_the_deadline() {
        assert!(!probe_complete(&directory(&[("n2", Some(0))]), &peers(&[])));
    }

    #[test]
    fn older_epoch_members_are_stale_and_fresh_ones_are_not() {
        let dir = directory(&[("old", Some(0)), ("current", Some(1)), ("fresh", None)]);
        assert_eq!(
            stale_epoch_members(&dir, Some(1)),
            HashSet::from([NodeId::new("old")])
        );
        // A node with no epoch of its own judges no one.
        assert!(stale_epoch_members(&dir, None).is_empty());
    }
}
