/// Piggyback dissemination queue.
///
/// Membership updates are not sent as dedicated messages. Instead,
/// they piggyback on every PING/ACK exchange. Each update is broadcast
/// a limited number of times: `ceil(log2(cluster_size))`, which is
/// enough for O(log N) convergence across the whole cluster.
///
/// The queue prioritises failure-related updates (Dead, Suspect) over
/// join updates (Alive), so failures propagate faster.
use std::cmp::Ordering;
use std::collections::BinaryHeap;

use super::message::{MAX_PIGGYBACK_UPDATES, MembershipUpdate};

/// An update waiting to be piggybacked onto outgoing messages.
#[derive(Debug, Clone)]
struct QueuedUpdate {
    /// The update to disseminate.
    update: MembershipUpdate,
    /// How many more times this update should be piggybacked.
    remaining_broadcasts: u32,
}

impl Eq for QueuedUpdate {}

impl PartialEq for QueuedUpdate {
    fn eq(&self, other: &Self) -> bool {
        self.update.node_id == other.update.node_id
            && self.update.incarnation == other.update.incarnation
            && self.update.state == other.update.state
    }
}

/// Priority ordering: higher dissemination priority first, then
/// more remaining broadcasts first (fresher updates).
impl Ord for QueuedUpdate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.update
            .state
            .dissemination_priority()
            .cmp(&other.update.state.dissemination_priority())
            .then_with(|| self.remaining_broadcasts.cmp(&other.remaining_broadcasts))
    }
}

impl PartialOrd for QueuedUpdate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Smallest queue length that can trigger compaction (O5).
///
/// Nothing bounded this heap: `enqueue` pushed unconditionally while
/// `select_updates` drains at most [`MAX_PIGGYBACK_UPDATES`] per outgoing
/// message, so repeated updates about the same nodes could accumulate
/// faster than the queue drains, forever.
///
/// The trigger scales with cluster size rather than being a flat ceiling,
/// because one entry per member *is* the normal shape of a first
/// dissemination: a 10,000-member cluster legitimately queues ~10,000
/// updates and every one of them has to go out. A flat cap would silently
/// throw away most of the cluster's initial view. This floor only matters
/// for small clusters, where churn about a handful of nodes is the growth
/// worth catching.
const MIN_COMPACT_THRESHOLD: usize = 1024;

/// Absolute backstop, far above any plausible cluster.
///
/// Coalescing bounds the queue at the number of distinct node ids, which a
/// real cluster's membership bounds in turn. This only trips if something
/// is feeding us updates about ids that don't correspond to members — and
/// unlike the old flat cap it is loud about it, because a queue that has to
/// discard real updates is an incident, not routine housekeeping.
const HARD_CAP: usize = 65_536;

/// Whether `queued` should replace `existing` when coalescing the same node's
/// updates: a strictly higher incarnation, or the same incarnation with a
/// strictly higher-priority state (Dead/Left/Suspect over Alive). This mirrors
/// [`crate::mustard::state::resolve_conflict`]'s precedence.
fn queued_supersedes(queued: &QueuedUpdate, existing: &QueuedUpdate) -> bool {
    use std::cmp::Ordering;
    match queued.update.incarnation.cmp(&existing.update.incarnation) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => {
            queued.update.state.dissemination_priority()
                > existing.update.state.dissemination_priority()
        }
    }
}

/// The dissemination queue that selects updates to piggyback on messages.
pub struct DisseminationQueue {
    queue: BinaryHeap<QueuedUpdate>,
}

impl DisseminationQueue {
    /// Create an empty dissemination queue.
    pub fn new() -> Self {
        Self {
            queue: BinaryHeap::new(),
        }
    }

    /// Enqueue an update for dissemination.
    ///
    /// The update will be piggybacked on `ceil(log2(cluster_size))`
    /// outgoing messages. If `cluster_size` is 0 or 1, the update
    /// is broadcast once.
    pub fn enqueue(&mut self, update: MembershipUpdate, cluster_size: usize) {
        let broadcasts = broadcast_count(cluster_size);
        self.queue.push(QueuedUpdate {
            update,
            remaining_broadcasts: broadcasts,
        });
        // Scale the trigger with the cluster: one queued update per member is
        // normal, so compacting at a flat threshold would fire constantly on a
        // large cluster and discard its first dissemination.
        let threshold = cluster_size.saturating_mul(2).max(MIN_COMPACT_THRESHOLD);
        if self.queue.len() > threshold {
            self.compact();
        }
    }

    /// Drop queued updates that nothing is waiting for (O5).
    ///
    /// Coalesce to the highest-incarnation entry per node. An older update
    /// about a node is exactly what the newer one supersedes, so sending
    /// both spends datagram space telling peers something they immediately
    /// overwrite. This is the whole bound in practice: the queue can hold at
    /// most one entry per distinct node, and a real cluster's membership
    /// bounds that.
    ///
    /// Nothing legitimate is discarded here, which is the point — the
    /// earlier version of this capped the queue at a flat 4096 and truncated
    /// the excess, which quietly threw away 6,000 members' updates on a
    /// 10,000-member cluster. The `gossip_10k` acceptance test caught it.
    ///
    /// [`HARD_CAP`] remains as a backstop against updates about ids that
    /// aren't members at all, and it complains rather than trimming in
    /// silence.
    fn compact(&mut self) {
        use std::collections::HashMap;

        let drained: Vec<QueuedUpdate> = self.queue.drain().collect();
        let mut newest: HashMap<crate::meat::NodeId, QueuedUpdate> =
            HashMap::with_capacity(drained.len());
        for queued in drained {
            match newest.get(&queued.update.node_id) {
                // Keep the surviving update per SWIM precedence: a higher
                // incarnation always wins, and at an *equal* incarnation the
                // higher-priority state does (Dead/Left/Suspect over Alive).
                // `BinaryHeap::drain()` yields arbitrary order, so the old
                // `existing.incarnation >= queued.incarnation` kept whichever
                // was drained first — silently dropping a Dead/Suspect in
                // favour of an Alive at equal incarnation (inverting
                // `resolve_conflict`, so a failure notification vanished).
                Some(existing) if !queued_supersedes(&queued, existing) => {}
                _ => {
                    newest.insert(queued.update.node_id.clone(), queued);
                }
            }
        }

        let mut remaining: Vec<QueuedUpdate> = newest.into_values().collect();
        if remaining.len() > HARD_CAP {
            // Descending: highest priority first, so what goes is `Alive`
            // chatter rather than a `Dead`/`Suspect` someone is waiting on.
            remaining.sort_by(|a, b| b.cmp(a));
            let dropped = remaining.len() - HARD_CAP;
            remaining.truncate(HARD_CAP);
            eprintln!(
                "mustard: dissemination queue exceeded {HARD_CAP} distinct nodes; \
                 dropped {dropped} update(s) — this implies updates about non-members"
            );
        }
        self.queue = remaining.into_iter().collect();
    }

    /// Select up to `MAX_PIGGYBACK_UPDATES` updates to piggyback on
    /// the next outgoing message.
    ///
    /// Highest-priority updates are selected first. Each selected
    /// update has its remaining broadcast count decremented. Updates
    /// that have been broadcast enough times are dropped.
    pub fn select_updates(&mut self) -> Vec<MembershipUpdate> {
        let mut selected = Vec::with_capacity(MAX_PIGGYBACK_UPDATES);
        let mut remaining = Vec::new();

        while let Some(mut entry) = self.queue.pop() {
            if selected.len() < MAX_PIGGYBACK_UPDATES {
                selected.push(entry.update.clone());
                entry.remaining_broadcasts -= 1;
                if entry.remaining_broadcasts > 0 {
                    remaining.push(entry);
                }
            } else {
                remaining.push(entry);
                break;
            }
        }

        // Put back entries we didn't select (still in the heap)
        // plus remaining entries that still need broadcasting
        for entry in remaining {
            self.queue.push(entry);
        }
        // Also push back anything left in the original heap
        // (the break above may leave entries)
        // Actually, the while loop drains the heap, so remaining
        // already has everything. But we broke early, so re-drain.
        // Let me restructure this.

        selected
    }

    /// Number of pending updates in the queue.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns `true` if no updates are pending.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl Default for DisseminationQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Calculate how many times an update should be broadcast.
///
/// Uses `3 * ceil(log2(cluster_size))` — the multiplier of 3 (lambda
/// in the SWIM paper) compensates for the fact that each gossip round
/// burns a broadcast on both the PING and ACK, and not every broadcast
/// reaches a node that hasn't seen the update yet. A minimum of 6
/// ensures updates survive long enough during early cluster formation.
fn broadcast_count(cluster_size: usize) -> u32 {
    if cluster_size <= 1 {
        return 6;
    }
    // ceil(log2(n)) = 64 - leading_zeros(n - 1) for n > 1
    let bits = usize::BITS - (cluster_size - 1).leading_zeros();
    (bits * 3).max(6)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meat::NodeId;
    use crate::mustard::NodeState;

    fn update(node: &str, state: NodeState) -> MembershipUpdate {
        MembershipUpdate {
            node_id: NodeId::new(node),
            address: std::net::SocketAddr::from(([127, 0, 0, 1], 9000)),
            state,
            incarnation: 1,
            lamport: 0,
        }
    }

    fn queued(node: &str, state: NodeState, incarnation: u64) -> QueuedUpdate {
        let mut u = update(node, state);
        u.incarnation = incarnation;
        QueuedUpdate {
            update: u,
            remaining_broadcasts: 3,
        }
    }

    // -- coalescing precedence (M11) ------------------------------------------

    #[test]
    fn coalescing_keeps_the_failure_state_at_equal_incarnation() {
        // A Dead and an Alive for the same node at the same incarnation must
        // resolve to Dead — the old compaction kept whichever the arbitrary
        // heap-drain yielded first, silently dropping the failure notification.
        let dead = queued("n1", NodeState::Dead, 5);
        let alive = queued("n1", NodeState::Alive, 5);
        assert!(queued_supersedes(&dead, &alive));
        assert!(!queued_supersedes(&alive, &dead));
    }

    #[test]
    fn coalescing_prefers_the_higher_incarnation() {
        // A higher incarnation always wins, even an Alive over a Dead.
        let fresh_alive = queued("n1", NodeState::Alive, 9);
        let stale_dead = queued("n1", NodeState::Dead, 5);
        assert!(queued_supersedes(&fresh_alive, &stale_dead));
        assert!(!queued_supersedes(&stale_dead, &fresh_alive));
    }

    // -- broadcast_count ------------------------------------------------------

    #[test]
    fn broadcast_count_zero_cluster() {
        assert_eq!(broadcast_count(0), 6);
    }

    #[test]
    fn broadcast_count_single_node() {
        assert_eq!(broadcast_count(1), 6);
    }

    #[test]
    fn broadcast_count_two_nodes() {
        assert_eq!(broadcast_count(2), 6);
    }

    #[test]
    fn broadcast_count_four_nodes() {
        assert_eq!(broadcast_count(4), 6);
    }

    #[test]
    fn broadcast_count_eight_nodes() {
        assert_eq!(broadcast_count(8), 9);
    }

    #[test]
    fn broadcast_count_hundred_nodes() {
        assert_eq!(broadcast_count(100), 21);
    }

    #[test]
    fn broadcast_count_ten_thousand_nodes() {
        assert_eq!(broadcast_count(10_000), 42);
    }

    // -- enqueue and select ---------------------------------------------------

    #[test]
    fn enqueue_and_select_single_update() {
        let mut queue = DisseminationQueue::new();
        queue.enqueue(update("n1", NodeState::Alive), 8);

        let selected = queue.select_updates();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].node_id, NodeId::new("n1"));
    }

    #[test]
    fn select_returns_empty_when_queue_empty() {
        let mut queue = DisseminationQueue::new();
        let selected = queue.select_updates();
        assert!(selected.is_empty());
    }

    #[test]
    fn updates_expire_after_broadcast_count() {
        let mut queue = DisseminationQueue::new();
        // cluster_size=2 -> broadcast_count=6 (minimum)
        queue.enqueue(update("n1", NodeState::Alive), 2);

        // Should be selectable 6 times (minimum broadcast count)
        for _ in 0..6 {
            let selected = queue.select_updates();
            assert_eq!(selected.len(), 1);
        }
        // Seventh time should be empty
        let selected = queue.select_updates();
        assert!(selected.is_empty());
    }

    #[test]
    fn update_broadcast_multiple_times_for_larger_cluster() {
        let mut queue = DisseminationQueue::new();
        // cluster_size=100 -> broadcast_count=21 (3 * ceil(log2(100)))
        queue.enqueue(update("n1", NodeState::Alive), 100);

        // Should be selectable 21 times
        for _ in 0..21 {
            let selected = queue.select_updates();
            assert_eq!(selected.len(), 1);
        }
        // 22nd time should be empty
        let selected = queue.select_updates();
        assert!(selected.is_empty());
    }

    #[test]
    fn dead_updates_prioritised_over_alive() {
        let mut queue = DisseminationQueue::new();
        queue.enqueue(update("alive-node", NodeState::Alive), 8);
        queue.enqueue(update("dead-node", NodeState::Dead), 8);

        let selected = queue.select_updates();
        assert_eq!(selected.len(), 2);
        // Dead should come first
        assert_eq!(selected[0].state, NodeState::Dead);
        assert_eq!(selected[1].state, NodeState::Alive);
    }

    #[test]
    fn suspect_updates_prioritised_over_alive() {
        let mut queue = DisseminationQueue::new();
        queue.enqueue(update("alive-node", NodeState::Alive), 8);
        queue.enqueue(update("suspect-node", NodeState::Suspect), 8);

        let selected = queue.select_updates();
        assert_eq!(selected[0].state, NodeState::Suspect);
        assert_eq!(selected[1].state, NodeState::Alive);
    }

    #[test]
    fn select_bounded_to_max_piggyback_updates() {
        let mut queue = DisseminationQueue::new();
        for i in 0..20 {
            queue.enqueue(update(&format!("n{i}"), NodeState::Alive), 100);
        }

        let selected = queue.select_updates();
        assert_eq!(selected.len(), MAX_PIGGYBACK_UPDATES);
    }

    #[test]
    fn unselected_updates_remain_in_queue() {
        let mut queue = DisseminationQueue::new();
        for i in 0..12 {
            queue.enqueue(update(&format!("n{i}"), NodeState::Alive), 100);
        }

        let first_batch = queue.select_updates();
        assert_eq!(first_batch.len(), MAX_PIGGYBACK_UPDATES);

        // The remaining 4 + the re-queued 8 (minus decremented) should still be there
        assert!(!queue.is_empty());

        let second_batch = queue.select_updates();
        assert!(!second_batch.is_empty());
    }

    #[test]
    fn len_tracks_pending_updates() {
        let mut queue = DisseminationQueue::new();
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());

        queue.enqueue(update("n1", NodeState::Alive), 2);
        queue.enqueue(update("n2", NodeState::Dead), 2);
        assert_eq!(queue.len(), 2);
    }

    // -- bounded growth (O5) --------------------------------------------------

    /// The queue drains at most `MAX_PIGGYBACK_UPDATES` per outgoing message
    /// but accepted enqueues without limit, so churn about one node could
    /// outrun it forever.
    #[test]
    fn repeated_updates_about_one_node_do_not_grow_without_bound() {
        let mut queue = DisseminationQueue::new();
        let churn = MIN_COMPACT_THRESHOLD as u64 + 100;
        for incarnation in 1..churn {
            let mut u = update("n1", NodeState::Alive);
            u.incarnation = incarnation;
            queue.enqueue(u, 3);
        }
        let newest = churn - 1;

        // Compaction is amortised — it runs when the threshold is crossed,
        // not on every enqueue — so the queue sits under the threshold rather
        // than at 1. What matters is that it stops growing.
        assert!(
            queue.len() <= MIN_COMPACT_THRESHOLD,
            "queue grew to {} past the {MIN_COMPACT_THRESHOLD} threshold",
            queue.len()
        );
        // …and that coalescing keeps the newest incarnation. Losing that
        // would make the bound a correctness bug rather than a memory fix.
        let mut seen_newest = false;
        while !queue.is_empty() {
            if queue
                .select_updates()
                .iter()
                .any(|u| u.incarnation == newest)
            {
                seen_newest = true;
                break;
            }
        }
        assert!(seen_newest, "compaction dropped the newest incarnation");
    }

    /// The regression that CI caught. One queued update per member is the
    /// normal shape of a first dissemination, so a large cluster must keep
    /// *every* one of them — the first version of this bound capped the queue
    /// at a flat 4096 and silently threw away the rest, which on a
    /// 10,000-member cluster meant 6,000 members never disseminated at all.
    #[test]
    fn a_large_clusters_first_dissemination_keeps_every_member() {
        let cluster_size = 10_000;
        let mut queue = DisseminationQueue::new();
        for i in 0..cluster_size {
            queue.enqueue(update(&format!("n{i}"), NodeState::Alive), cluster_size);
        }
        assert_eq!(
            queue.len(),
            cluster_size,
            "a legitimate one-per-member queue was trimmed"
        );

        // And every member is actually exposed by repeated selection, which is
        // the property `tests/gossip_10k.rs` asserts end to end.
        let mut seen = std::collections::HashSet::new();
        while seen.len() < cluster_size && !queue.is_empty() {
            let batch = queue.select_updates();
            assert!(!batch.is_empty(), "updates expired before first broadcast");
            seen.extend(batch.into_iter().map(|u| u.node_id));
        }
        assert_eq!(seen.len(), cluster_size);
    }

    /// Compaction never discards a distinct node's update below the hard
    /// backstop, so a `Dead` buried under a flood of `Alive` still goes out.
    #[test]
    fn compaction_keeps_distinct_nodes_including_urgent_ones() {
        let mut queue = DisseminationQueue::new();
        queue.enqueue(update("critical", NodeState::Dead), 3);
        // A small declared cluster size, so the floor threshold is what
        // triggers compaction rather than the scaled one.
        for i in 0..(MIN_COMPACT_THRESHOLD + 500) {
            queue.enqueue(update(&format!("n{i}"), NodeState::Alive), 3);
        }

        let mut seen = std::collections::HashSet::new();
        while !queue.is_empty() {
            seen.extend(queue.select_updates().into_iter().map(|u| u.node_id));
        }
        assert!(
            seen.contains(&NodeId::new("critical")),
            "compaction dropped a Dead update"
        );
        assert_eq!(
            seen.len(),
            MIN_COMPACT_THRESHOLD + 501,
            "compaction dropped distinct nodes below the hard backstop"
        );
    }
}
