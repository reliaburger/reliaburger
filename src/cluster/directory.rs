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

/// How far a gossip leader hint's term may legitimately lead a Raft
/// member's own term before the member treats it as bogus.
///
/// A real election bumps the term by one, and a voter or learner learns the
/// current term from the leader's AppendEntries, so a hint should sit at or
/// just above its local term — the small lead absorbs gossip out-running Raft
/// replication to a lagging member. A hint claiming a term far beyond it
/// (e.g. `u64::MAX`) is rejected, so it can't carry a member's reporting
/// epoch past every later real election.
///
/// The bound applies only to Raft members (#1996), and only to a hint from
/// the recovery epoch the member's own term belongs to: a recovery starts a
/// fresh term line, so a fenced voter's old term says nothing about the new
/// council's. A node outside the Raft
/// membership never hears AppendEntries, so its term stays at 0 for good:
/// bounding hints by it would lose the leader once the cluster had seen more
/// than ten elections. Such a node follows the newest hint gossip has shown
/// it. Gossip datagrams are HMAC-authenticated with the cluster key, which
/// every node holds, so a hint proves nothing about the node it names: the
/// reporting epoch only gates anything on the leader, which is a member,
/// and a seatless node fetches its security state only from a voter of the
/// council it knows, over TLS bound to that voter (#2406).
pub const MAX_LEADER_HINT_TERM_LEAD: u64 = 10;

/// Where a [`LeaderView`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderSource {
    /// This node's own Raft metrics, authoritative on a Raft member.
    Raft,
    /// The gossip leader hint.
    Gossip,
}

/// The resolved route to the current leader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderView {
    /// The leader's gossip node id.
    pub node_id: NodeId,
    /// The Raft term this view was resolved under. Doubles as the
    /// reporting epoch: a new leader means a new term means a new epoch.
    pub term: u64,
    /// The recovery epoch `term` belongs to (#1997). Each `council recover`
    /// starts a fresh term line, so terms only compare within one epoch.
    pub recovery_epoch: u64,
    /// Whether Raft metrics or a gossip hint named this leader.
    pub source: LeaderSource,
    /// The leader's HTTP API endpoint, if resolvable.
    pub api_address: Option<SocketAddr>,
    /// The leader's reporting-tree endpoint, if resolvable.
    pub reporting_address: Option<SocketAddr>,
}

/// Openraft's metrics channel, as [`crate::council::CouncilNode::metrics`]
/// hands it out.
pub type MetricsReceiver = watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>;

/// The Raft view of the leader, `(leader_id, raft_address, term)`, and this
/// node's own term, copied out of the metrics channel. The term is `None`
/// on a node outside the Raft membership, which never learns one.
type RaftLeader = (Option<(NodeId, SocketAddr, u64)>, Option<u64>);

fn raft_leader(metrics: &openraft::RaftMetrics<u64, CouncilNodeInfo>) -> RaftLeader {
    let membership = metrics.membership_config.membership();
    let leader = metrics.current_leader.and_then(|leader_id| {
        membership
            .get_node(&leader_id)
            .map(|info| (NodeId::new(&info.name), info.addr, metrics.current_term))
    });
    // Voters and learners both sit in the membership's node map.
    let member = membership.get_node(&metrics.id).is_some();
    (leader, member.then_some(metrics.current_term))
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
///
/// `local_term` is this node's own Raft term, or `None` when it isn't a Raft
/// member (#1996). The directory's leader hint is already from this node's
/// recovery epoch or a newer one (#1997): see
/// [`NodeDirectory::raise_recovery_floor`].
pub fn resolve_leader_view(
    raft_leader: Option<(NodeId, SocketAddr, u64)>,
    directory: &NodeDirectory,
    local_term: Option<u64>,
    raft_to_api_offset: i32,
    raft_to_reporting_offset: i32,
) -> Option<LeaderView> {
    // A Raft member trusts a hint from its own recovery epoch only when the
    // hint's term is plausibly close to its own (see
    // [`MAX_LEADER_HINT_TERM_LEAD`]). A non-member has no term of its own to
    // judge by, and a newer epoch's terms are a different line, so either
    // takes the hint.
    let local_epoch = directory.local_recovery_epoch;
    let hint_plausible = |hint: &crate::mustard::message::LeaderHint| {
        hint.recovery_epoch != local_epoch
            || local_term
                .is_none_or(|local| hint.term <= local.saturating_add(MAX_LEADER_HINT_TERM_LEAD))
    };
    if let Some((node_id, raft_address, term)) = raft_leader {
        // A strictly newer gossip hint outranks local Raft metrics: a
        // partitioned ex-leader can keep reporting itself as leader until
        // it steps down, while the rest of the cluster has moved on. Newer
        // means a newer recovery epoch, or a higher term in this one; a
        // hint far beyond the local term can never outrank real metrics.
        if let Some(hint) = &directory.leader
            && (hint.recovery_epoch, hint.term) > (local_epoch, term)
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
            recovery_epoch: local_epoch,
            source: LeaderSource::Raft,
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
    leader_api_target(metrics, directory).map(|(_, address)| address)
}

/// [`leader_api_address`], with the node id of the leader the address
/// belongs to, so a caller can bind its TLS connection to that node (#2406).
pub fn leader_api_target(
    metrics: Option<&MetricsReceiver>,
    directory: &watch::Receiver<NodeDirectory>,
) -> Option<(NodeId, SocketAddr)> {
    // The metrics borrow is a temporary, dropped at the end of this
    // statement, before the directory borrow below.
    let raft = metrics.map(|metrics| raft_leader(&metrics.borrow()));
    let directory = directory.borrow();
    let hinted = || {
        directory
            .leader
            .as_ref()
            .map(|leader| (leader.node_id.clone(), leader.api_address))
    };
    let Some((leader, local_term)) = raft else {
        return hinted();
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
                .map(|address| (leader.node_id, address))
        })
        .or_else(hinted)
}

/// Whether the leader-target maintainer should publish `view.term` as this
/// node's reporting epoch, given the `(recovery_epoch, term)` it published
/// last (#1997). Returns the pair to remember when it should.
///
/// The leader's own Raft metrics are authoritative, so a Raft view always
/// sets the epoch, even downwards: that is what lets a recovered leader, or
/// one whose epoch an old council's hint once raised, schedule again. A
/// gossip view only moves the epoch forward, ordered by recovery epoch and
/// then term, so a lagging hint can't roll it back.
pub fn next_reporting_epoch(last: Option<(u64, u64)>, view: &LeaderView) -> Option<(u64, u64)> {
    let key = (view.recovery_epoch, view.term);
    let publish = match view.source {
        LeaderSource::Raft => last != Some(key),
        LeaderSource::Gossip => last.is_none_or(|last| key > last),
    };
    publish.then_some(key)
}

fn hint_view(hint: &crate::mustard::message::LeaderHint) -> LeaderView {
    LeaderView {
        node_id: hint.node_id.clone(),
        term: hint.term,
        recovery_epoch: hint.recovery_epoch,
        source: LeaderSource::Gossip,
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
        let view = resolve_leader_view(None, &directory, None, 2, 1).unwrap();
        assert_eq!(view.node_id, NodeId::new("leader"));
        assert_eq!(view.term, 5);
        assert_eq!(view.source, LeaderSource::Gossip);
        assert_eq!(view.api_address, Some(addr(9117)));
        assert_eq!(view.reporting_address, Some(addr(9445)));
    }

    #[test]
    fn no_leader_anywhere_resolves_to_none() {
        assert!(resolve_leader_view(None, &NodeDirectory::default(), None, 2, 1).is_none());
        assert!(resolve_leader_view(None, &NodeDirectory::default(), Some(0), 2, 1).is_none());
    }

    // #1996: beyond the council cap a node never joins Raft. Its metrics
    // stay at term 0 with no membership, however many elections the
    // cluster has had, so bounding the hint by that term lost the leader
    // after the tenth. This drives the real metrics path.
    #[test]
    fn a_node_outside_raft_follows_the_leader_after_many_elections() {
        let (_metrics_tx, metrics) = watch::channel(openraft::RaftMetrics::new_initial(9));
        let (_directory_tx, directory) = watch::channel(NodeDirectory {
            leader: Some(hint("leader", 50, 9117, 9445)),
            ..Default::default()
        });
        let view = resolve_leader(&metrics, &directory, 2, 1)
            .expect("a node outside Raft lost the leader at term 50");
        assert_eq!(view.node_id, NodeId::new("leader"));
        assert_eq!(view.term, 50);
        assert_eq!(view.api_address, Some(addr(9117)));
    }

    // #1997: after `council recover` the recovered leader starts a fresh
    // term line at a new recovery epoch. The replaced council's hint, still
    // relayed by survivors, has a higher term from the old line: it must not
    // outrank the recovered leader's own metrics.
    #[test]
    fn an_old_councils_hint_cannot_outrank_a_recovered_leader() {
        let mut directory = NodeDirectory::default();
        let mut stale = hint("old-leader", 9, 9217, 9545);
        stale.recovery_epoch = 0;
        directory.observe_hint(&stale);
        directory.raise_recovery_floor(1);
        directory.set_local_recovery_epoch(1);

        let view = resolve_leader_view(raft_leader_at(3), &directory, Some(3), 2, 1).unwrap();
        assert_eq!(view.node_id, NodeId::new("survivor"));
        assert_eq!((view.recovery_epoch, view.term), (1, 3));
        assert_eq!(view.source, LeaderSource::Raft);

        // A hint from the new council, at a higher term, still outranks.
        let mut newer = hint("successor", 4, 9217, 9545);
        newer.recovery_epoch = 1;
        directory.observe_hint(&newer);
        let view = resolve_leader_view(raft_leader_at(3), &directory, Some(3), 2, 1).unwrap();
        assert_eq!(view.node_id, NodeId::new("successor"));
    }

    // A fenced voter of the replaced council keeps its old term (5 here),
    // but the recovered council's terms start a fresh line, so comparing
    // them is meaningless: the term bound holds only within the node's own
    // recovery epoch. A fenced node names no Raft leader, so it resolves
    // through the hint alone.
    #[test]
    fn a_fenced_voter_follows_the_new_council_whatever_its_old_term() {
        let mut directory = NodeDirectory::default();
        directory.raise_recovery_floor(1);
        let mut recovered = hint("survivor", 30, 9217, 9545);
        recovered.recovery_epoch = 1;
        directory.observe_hint(&recovered);

        let view = resolve_leader_view(None, &directory, Some(5), 2, 1)
            .expect("a fenced voter refused the new council's leader");
        assert_eq!(view.node_id, NodeId::new("survivor"));
        assert_eq!((view.recovery_epoch, view.term), (1, 30));

        // Within the node's own epoch the bound still holds.
        let mut absurd = hint("bogus", 5 + MAX_LEADER_HINT_TERM_LEAD + 1, 9217, 9545);
        absurd.recovery_epoch = 0;
        let mut same_epoch = NodeDirectory::default();
        same_epoch.observe_hint(&absurd);
        assert!(resolve_leader_view(None, &same_epoch, Some(5), 2, 1).is_none());
    }

    fn raft_leader_at(term: u64) -> Option<(NodeId, SocketAddr, u64)> {
        Some((NodeId::new("survivor"), addr(9444), term))
    }

    fn view(source: LeaderSource, recovery_epoch: u64, term: u64) -> LeaderView {
        LeaderView {
            node_id: NodeId::new("leader"),
            term,
            recovery_epoch,
            source,
            api_address: None,
            reporting_address: None,
        }
    }

    // #1997: an old council's term once raised the reporting epoch, and the
    // epoch only grew, so the recovered leader's lower term never matched
    // it and every orchestration pass was skipped.
    #[test]
    fn the_leaders_own_term_sets_the_reporting_epoch_even_downwards() {
        let raised = Some((0, 9));
        assert_eq!(
            next_reporting_epoch(raised, &view(LeaderSource::Raft, 1, 3)),
            Some((1, 3))
        );
        assert_eq!(
            next_reporting_epoch(raised, &view(LeaderSource::Raft, 0, 7)),
            Some((0, 7)),
            "Raft metrics are authoritative, even below a hint's term"
        );
        assert_eq!(
            next_reporting_epoch(Some((1, 3)), &view(LeaderSource::Raft, 1, 3)),
            None
        );
    }

    #[test]
    fn a_gossip_hint_only_moves_the_reporting_epoch_forward() {
        let last = Some((1, 5));
        // A lagging hint can't roll the epoch back.
        assert_eq!(
            next_reporting_epoch(last, &view(LeaderSource::Gossip, 1, 4)),
            None
        );
        assert_eq!(
            next_reporting_epoch(last, &view(LeaderSource::Gossip, 0, 9)),
            None
        );
        assert_eq!(
            next_reporting_epoch(last, &view(LeaderSource::Gossip, 1, 5)),
            None
        );
        // A newer term, or a newer recovery epoch with a lower term, moves it.
        assert_eq!(
            next_reporting_epoch(last, &view(LeaderSource::Gossip, 1, 6)),
            Some((1, 6))
        );
        assert_eq!(
            next_reporting_epoch(last, &view(LeaderSource::Gossip, 2, 1)),
            Some((2, 1))
        );
        assert_eq!(
            next_reporting_epoch(None, &view(LeaderSource::Gossip, 0, 1)),
            Some((0, 1))
        );
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
        let view = resolve_leader_view(raft, &directory, Some(7), 2, 1).unwrap();
        assert_eq!(view.term, 7);
        assert_eq!(view.api_address, Some(addr(19117)));
        assert_eq!(view.reporting_address, Some(addr(19445)));
    }

    #[test]
    fn voter_falls_back_to_offset_derivation_without_a_directory_entry() {
        let raft = Some((NodeId::new("leader"), addr(9444), 7));
        let view = resolve_leader_view(raft, &NodeDirectory::default(), Some(7), 2, 1).unwrap();
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
        let view = resolve_leader_view(raft, &directory, Some(6), 2, 1).unwrap();
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
        let view = resolve_leader_view(raft, &directory, Some(7), 2, 1).unwrap();
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
        let view = resolve_leader_view(raft, &directory, Some(7), 2, 1).unwrap();
        assert_eq!(
            view.node_id,
            NodeId::new("real-leader"),
            "an implausible hint term must not outrank real Raft metrics"
        );
        assert_eq!(view.term, 7);

        // A member between leaders rejects the absurd hint outright rather
        // than adopting it (which would fix its epoch at MAX).
        assert!(resolve_leader_view(None, &directory, Some(7), 2, 1).is_none());

        // A hint just within the window is still honoured.
        let near = NodeDirectory {
            leader: Some(hint("near", 7 + MAX_LEADER_HINT_TERM_LEAD, 9217, 9545)),
            ..Default::default()
        };
        assert!(resolve_leader_view(None, &near, Some(7), 2, 1).is_some());
    }

    #[test]
    fn offset_derivation_rejects_out_of_range_ports() {
        let raft = Some((NodeId::new("leader"), addr(65535), 1));
        let view = resolve_leader_view(raft, &NodeDirectory::default(), Some(1), 2, -1).unwrap();
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
