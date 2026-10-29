//! Leader resolution from Raft metrics plus the gossip node directory
//! (Phase 12b.2, H1/D1/CP1).
//!
//! Before this module, the reporting worker and the placement reconciler
//! found the leader through local Raft metrics alone — which non-voters
//! don't have, so any node beyond the council cap silently stopped
//! converging. Resolution now works on every node: Raft metrics stay
//! authoritative for voters, the gossip [`NodeDirectory`] carries the
//! leader's identity and advertised endpoints to everyone else, and the
//! old "derive the port by fixed offset" trick becomes a fallback used
//! only while the directory is still propagating.

use std::net::SocketAddr;

use tokio::sync::watch;

use crate::council::types::CouncilNodeInfo;
use crate::meat::types::NodeId;
use crate::mustard::directory::NodeDirectory;

/// How far a gossip leader hint's term may legitimately lead this node's own
/// Raft term before it is treated as bogus.
///
/// A real election bumps the term by one, and a follower/learner learns the
/// current term from the leader's AppendEntries, so a hint should sit at or
/// just above the local term — the small lead absorbs gossip out-running Raft
/// replication to a lagging node. A hint claiming a term far beyond the local
/// term (e.g. `u64::MAX`) is rejected: because the reporting epoch only ever
/// grows, adopting one would wedge reporting cluster-wide past every
/// subsequent real election.
pub const MAX_LEADER_HINT_TERM_LEAD: u64 = 10;

/// The resolved route to the current leader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderView {
    /// The leader's gossip node id.
    pub node_id: NodeId,
    /// The Raft term this view was resolved under. Doubles as the
    /// reporting epoch: a new leader means a new term means a new epoch.
    pub term: u64,
    /// The leader's HTTP API endpoint, if resolvable.
    pub api_address: Option<SocketAddr>,
    /// The leader's reporting-tree endpoint, if resolvable.
    pub reporting_address: Option<SocketAddr>,
}

/// Openraft's metrics channel, as [`crate::council::CouncilNode::metrics`]
/// hands it out.
pub type MetricsReceiver = watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>;

/// The Raft view of the leader, `(leader_id, raft_address, term)`, and this
/// node's own term, copied out of the metrics channel.
type RaftLeader = (Option<(NodeId, SocketAddr, u64)>, u64);

fn raft_leader(metrics: &openraft::RaftMetrics<u64, CouncilNodeInfo>) -> RaftLeader {
    let leader = metrics.current_leader.and_then(|leader_id| {
        metrics
            .membership_config
            .membership()
            .get_node(&leader_id)
            .map(|info| (NodeId::new(&info.name), info.addr, metrics.current_term))
    });
    (leader, metrics.current_term)
}

/// Resolve the current leader from local Raft metrics and the gossip
/// directory. Works on voters (metrics) and non-voters (directory hint)
/// alike; returns `None` only when neither source knows a leader.
///
/// It never holds both channels' borrows at once (#1273, #1433): see
/// [`leader_api_address`].
pub fn resolve_leader(
    metrics: &MetricsReceiver,
    directory: &watch::Receiver<NodeDirectory>,
    raft_to_api_offset: i32,
    raft_to_reporting_offset: i32,
) -> Option<LeaderView> {
    // Copy the metrics out and drop that borrow in this one statement.
    let (leader, local_term) = raft_leader(&metrics.borrow());
    resolve_leader_view(
        leader,
        &directory.borrow(),
        local_term,
        raft_to_api_offset,
        raft_to_reporting_offset,
    )
}

/// The pure core of [`resolve_leader`], taking the Raft view already
/// extracted as `(leader_id, raft_address, term)` so it can be unit-tested
/// without fabricating `openraft` metrics.
pub fn resolve_leader_view(
    raft_leader: Option<(NodeId, SocketAddr, u64)>,
    directory: &NodeDirectory,
    local_term: u64,
    raft_to_api_offset: i32,
    raft_to_reporting_offset: i32,
) -> Option<LeaderView> {
    // A gossip leader hint is only trusted when its term is plausibly close to
    // this node's own Raft term (see [`MAX_LEADER_HINT_TERM_LEAD`]). An
    // implausibly high term is bogus and, because the reporting epoch only
    // grows, would otherwise wedge reporting forever.
    let hint_plausible = |hint: &crate::mustard::message::LeaderHint| {
        hint.term <= local_term.saturating_add(MAX_LEADER_HINT_TERM_LEAD)
    };
    if let Some((node_id, raft_address, term)) = raft_leader {
        // A strictly newer gossip hint outranks local Raft metrics: a
        // partitioned ex-leader can keep reporting itself as leader until
        // it steps down, while the rest of the cluster has moved on. But a
        // hint far beyond the local term can never outrank real metrics.
        if let Some(hint) = &directory.leader
            && hint.term > term
            && hint_plausible(hint)
        {
            return Some(hint_view(hint));
        }
        // Raft says who leads; the directory says where its endpoints are.
        // The port-offset derivation survives only as a fallback for peers
        // whose extension hasn't arrived yet (and for mixed-version
        // clusters where old nodes never advertise).
        let advertised = directory.endpoints.get(&node_id);
        let api_address = advertised
            .map(|e| e.api_address)
            .or_else(|| offset_address(raft_address, raft_to_api_offset));
        let reporting_address = advertised
            .map(|e| e.reporting_address)
            .or_else(|| offset_address(raft_address, raft_to_reporting_offset));
        return Some(LeaderView {
            node_id,
            term,
            api_address,
            reporting_address,
        });
    }
    directory
        .leader
        .as_ref()
        .filter(|hint| hint_plausible(hint))
        .map(hint_view)
}

/// Where to send a request that only the leader can serve: the current
/// leader's advertised API endpoint.
///
/// Raft metrics say who leads when this node is a Raft member, and the
/// directory says where that node's API is. A worker outside the council, or
/// a node that hasn't learned a Raft term yet, has only the gossip hint. This
/// is a route, not a proof of leadership: the destination must still pass its
/// own quorum check before it acts.
///
/// Pass `None` for `metrics` on a node without a council handle: it has only
/// the gossip hint.
///
/// A `watch` borrow holds its channel's read lock, and that lock makes a new
/// reader wait behind a queued writer. Openraft and gossip both publish
/// often, so a task holding one borrow while it takes the other can wedge
/// against a task taking them the other way round, parking a worker thread
/// each. This copies what it needs from the metrics and drops that borrow
/// before it reads the directory.
pub fn leader_api_address(
    metrics: Option<&MetricsReceiver>,
    directory: &watch::Receiver<NodeDirectory>,
) -> Option<SocketAddr> {
    // The metrics borrow is a temporary, dropped at the end of this
    // statement, before the directory borrow below.
    let raft = metrics.map(|metrics| raft_leader(&metrics.borrow()));
    let directory = directory.borrow();
    let Some((leader, local_term)) = raft else {
        return directory.leader.as_ref().map(|leader| leader.api_address);
    };
    resolve_leader_view(leader, &directory, local_term, 0, 0)
        .and_then(|leader| {
            directory
                .endpoints
                .get(&leader.node_id)
                .map(|node| node.api_address)
                .or_else(|| {
                    directory
                        .leader
                        .as_ref()
                        .filter(|hint| hint.node_id == leader.node_id && hint.term == leader.term)
                        .map(|hint| hint.api_address)
                })
        })
        .or_else(|| directory.leader.as_ref().map(|leader| leader.api_address))
}

fn hint_view(hint: &crate::mustard::message::LeaderHint) -> LeaderView {
    LeaderView {
        node_id: hint.node_id.clone(),
        term: hint.term,
        api_address: Some(hint.api_address),
        reporting_address: Some(hint.reporting_address),
    }
}

/// Shift a socket address's port by a signed offset, or `None` if the
/// result falls outside the valid port range.
fn offset_address(address: SocketAddr, offset: i32) -> Option<SocketAddr> {
    u16::try_from(i32::from(address.port()) + offset)
        .ok()
        .map(|port| SocketAddr::new(address.ip(), port))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Whether a gossip directory write had to wait while `read` was stuck
/// reading the Raft metrics (#1273, #1433).
///
/// The test holds a metrics borrow, as the leader-target maintainer does,
/// and queues an openraft write behind it. `read` then runs on its own
/// thread with both channels and blocks on the metrics, because the lock
/// makes a new reader wait behind a queued writer. If `read` still holds the
/// directory at that point, gossip's next write waits too: half of the
/// four-way deadlock. Every wait here is bounded, so a regression fails
/// instead of hanging.
#[cfg(test)]
pub(crate) fn directory_write_waits_for_metrics_read(
    read: impl FnOnce(MetricsReceiver, watch::Receiver<NodeDirectory>) + Send + 'static,
) -> bool {
    use std::time::Duration;

    let (metrics_tx, metrics_rx) = watch::channel(openraft::RaftMetrics::new_initial(1));
    let (directory_tx, directory_rx) = watch::channel(NodeDirectory::default());
    let reader_metrics = metrics_rx.clone();
    let held = metrics_rx.borrow();
    let metrics_writer = std::thread::spawn(move || {
        metrics_tx.send_replace(openraft::RaftMetrics::new_initial(1));
    });
    std::thread::sleep(Duration::from_millis(100));
    let reader = std::thread::spawn(move || read(reader_metrics, directory_rx));
    std::thread::sleep(Duration::from_millis(100));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let directory_writer = std::thread::spawn(move || {
        directory_tx.send_replace(NodeDirectory::default());
        let _ = done_tx.send(());
    });
    let waited = done_rx.recv_timeout(Duration::from_millis(500)).is_err();
    drop(held);
    metrics_writer.join().unwrap();
    reader.join().unwrap();
    directory_writer.join().unwrap();
    waited
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mustard::directory::NodeEndpoints;
    use crate::mustard::message::LeaderHint;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn hint(name: &str, term: u64, api: u16, reporting: u16) -> LeaderHint {
        LeaderHint {
            node_id: NodeId::new(name),
            term,
            recovery_epoch: 0,
            api_address: addr(api),
            reporting_address: addr(reporting),
        }
    }

    #[test]
    fn non_voter_resolves_the_leader_from_the_gossip_hint() {
        // No Raft view at all — the CP1 case: a worker outside the council.
        let directory = NodeDirectory {
            leader: Some(hint("leader", 5, 9117, 9445)),
            ..Default::default()
        };
        let view = resolve_leader_view(None, &directory, 5, 2, 1).unwrap();
        assert_eq!(view.node_id, NodeId::new("leader"));
        assert_eq!(view.term, 5);
        assert_eq!(view.api_address, Some(addr(9117)));
        assert_eq!(view.reporting_address, Some(addr(9445)));
    }

    #[test]
    fn no_leader_anywhere_resolves_to_none() {
        assert!(resolve_leader_view(None, &NodeDirectory::default(), 0, 2, 1).is_none());
    }

    #[test]
    fn voter_prefers_advertised_endpoints_over_offset_derivation() {
        let mut directory = NodeDirectory::default();
        directory.endpoints.insert(
            NodeId::new("leader"),
            NodeEndpoints {
                api_address: addr(19117),
                reporting_address: addr(19445),
            },
        );
        // Raft knows the leader at raft port 9444; the directory's
        // advertised addresses win over 9444+offset.
        let raft = Some((NodeId::new("leader"), addr(9444), 7));
        let view = resolve_leader_view(raft, &directory, 7, 2, 1).unwrap();
        assert_eq!(view.term, 7);
        assert_eq!(view.api_address, Some(addr(19117)));
        assert_eq!(view.reporting_address, Some(addr(19445)));
    }

    #[test]
    fn voter_falls_back_to_offset_derivation_without_a_directory_entry() {
        let raft = Some((NodeId::new("leader"), addr(9444), 7));
        let view = resolve_leader_view(raft, &NodeDirectory::default(), 7, 2, 1).unwrap();
        assert_eq!(view.api_address, Some(addr(9446)));
        assert_eq!(view.reporting_address, Some(addr(9445)));
    }

    #[test]
    fn newer_gossip_hint_outranks_stale_raft_metrics() {
        // A deposed leader still believes in itself at term 6; gossip
        // already carries the term-7 successor.
        let directory = NodeDirectory {
            leader: Some(hint("new-leader", 7, 9217, 9545)),
            ..Default::default()
        };
        let raft = Some((NodeId::new("old-leader"), addr(9444), 6));
        let view = resolve_leader_view(raft, &directory, 6, 2, 1).unwrap();
        assert_eq!(view.node_id, NodeId::new("new-leader"));
        assert_eq!(view.term, 7);
    }

    #[test]
    fn same_term_keeps_raft_metrics_authoritative() {
        let directory = NodeDirectory {
            leader: Some(hint("gossip-leader", 7, 9217, 9545)),
            ..Default::default()
        };
        let raft = Some((NodeId::new("raft-leader"), addr(9444), 7));
        let view = resolve_leader_view(raft, &directory, 7, 2, 1).unwrap();
        assert_eq!(view.node_id, NodeId::new("raft-leader"));
    }

    #[test]
    fn implausible_gossip_hint_term_cannot_wedge_reporting() {
        // A member gossips a leader hint with an absurd term to ratchet the
        // (monotonic) reporting epoch to u64::MAX forever.
        let directory = NodeDirectory {
            leader: Some(hint("bogus", u64::MAX, 9217, 9545)),
            ..Default::default()
        };

        // Voter path: the absurd hint cannot outrank the node's own metrics.
        let raft = Some((NodeId::new("real-leader"), addr(9444), 7));
        let view = resolve_leader_view(raft, &directory, 7, 2, 1).unwrap();
        assert_eq!(
            view.node_id,
            NodeId::new("real-leader"),
            "an implausible hint term must not outrank real Raft metrics"
        );
        assert_eq!(view.term, 7);

        // Non-voter path: with no Raft leader, the absurd hint is rejected
        // outright rather than adopted (which would fix the epoch at MAX).
        assert!(resolve_leader_view(None, &directory, 7, 2, 1).is_none());

        // A hint just within the window is still honoured.
        let near = NodeDirectory {
            leader: Some(hint("near", 7 + MAX_LEADER_HINT_TERM_LEAD, 9217, 9545)),
            ..Default::default()
        };
        assert!(resolve_leader_view(None, &near, 7, 2, 1).is_some());
    }

    #[test]
    fn offset_derivation_rejects_out_of_range_ports() {
        let raft = Some((NodeId::new("leader"), addr(65535), 1));
        let view = resolve_leader_view(raft, &NodeDirectory::default(), 1, 2, -1).unwrap();
        assert_eq!(view.api_address, None);
        assert_eq!(view.reporting_address, Some(addr(65534)));
    }

    // #1433: a reader that held the gossip directory while it waited for the
    // metrics stalled gossip's next write, half of #1273's deadlock.
    #[test]
    fn resolving_the_leader_reads_the_metrics_without_holding_the_directory() {
        let waited = directory_write_waits_for_metrics_read(|metrics, directory| {
            resolve_leader(&metrics, &directory, 0, 0);
        });
        assert!(
            !waited,
            "a gossip directory write waited for the metrics read"
        );
    }

    #[test]
    fn the_leader_api_address_reads_the_metrics_without_holding_the_directory() {
        let waited = directory_write_waits_for_metrics_read(|metrics, directory| {
            leader_api_address(Some(&metrics), &directory);
        });
        assert!(
            !waited,
            "a gossip directory write waited for the metrics read"
        );
    }
}
