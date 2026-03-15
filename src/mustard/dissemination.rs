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
}
