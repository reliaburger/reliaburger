/// Cluster membership table.
///
/// Every node maintains a local copy of the membership table, updated
/// via piggybacked gossip updates. The table tracks each node's state,
/// incarnation number, address, labels, and role (council/leader).
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::meat::NodeId;

use super::message::MembershipUpdate;
use super::state::{self, NodeState};

/// Per-node resource summary piggybacked on gossip.
/// Approximately 64 bytes serialised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSummary {
    pub cpu_capacity_millicores: u32,
    pub cpu_used_millicores: u32,
    pub memory_capacity_mb: u32,
    pub memory_used_mb: u32,
    pub gpu_count: u8,
    pub gpu_used: u8,
    pub running_app_count: u16,
    pub running_job_count: u16,
}

/// Full membership record for a single node.
#[derive(Debug, Clone)]
pub struct NodeMembership {
    pub node_id: NodeId,
    pub address: SocketAddr,
    pub state: NodeState,
    pub incarnation: u64,
    /// When the node was first seen alive.
    pub first_seen: Instant,
    /// Last time we received a direct or indirect ACK from this node.
    pub last_ack: Instant,
    /// When the node last changed state (used for reap timing).
    pub state_changed: Instant,
    /// Resource summary, updated via gossip piggyback.
    pub resources: Option<ResourceSummary>,
    /// Node labels (zone, region, etc.), set at join time.
    pub labels: BTreeMap<String, String>,
    /// Whether this node is a council member.
    pub is_council: bool,
    /// Whether this node is the current Raft leader.
    pub is_leader: bool,
}

/// The local membership table maintained by each Mustard node.
///
/// Stores all known nodes and their states. Provides methods for
/// applying updates, querying alive members, and selecting probe
/// targets.
pub struct MembershipTable {
    members: HashMap<NodeId, NodeMembership>,
}

impl MembershipTable {
    /// Create an empty membership table.
    pub fn new() -> Self {
        Self {
            members: HashMap::new(),
        }
    }

    /// Add or update a node in the membership table.
    ///
    /// If the node already exists, the update is applied using SWIM's
    /// incarnation-based conflict resolution. Returns `true` if the
    /// membership state actually changed (useful for deciding whether
    /// to re-disseminate).
    pub fn apply_update(&mut self, update: &MembershipUpdate, now: Instant) -> bool {
        if let Some(existing) = self.members.get_mut(&update.node_id) {
            let old_state = existing.state;
            let old_incarnation = existing.incarnation;

            let (new_state, new_incarnation) = state::resolve_conflict(
                existing.state,
                existing.incarnation,
                update.state,
                update.incarnation,
            );

            if new_state != old_state || new_incarnation != old_incarnation {
                if new_state != old_state {
                    existing.state_changed = now;
                }
                existing.state = new_state;
                existing.incarnation = new_incarnation;
                if new_state == NodeState::Alive {
                    existing.last_ack = now;
                }
                true
            } else {
                false
            }
        } else {
            // New node — only add if it's alive (don't add dead nodes we've never seen)
            if update.state == NodeState::Alive {
                self.members.insert(
                    update.node_id.clone(),
                    NodeMembership {
                        node_id: update.node_id.clone(),
                        address: update.address,
                        state: NodeState::Alive,
                        incarnation: update.incarnation,
                        first_seen: now,
                        last_ack: now,
                        state_changed: now,
                        resources: None,
                        labels: BTreeMap::new(),
                        is_council: false,
                        is_leader: false,
                    },
                );
                true
            } else {
                false
            }
        }
    }

    /// Register a node with a known address.
    ///
    /// Used when a node joins via a direct connection (seed node, or
    /// PING received from a new peer). If the node already exists,
    /// updates its address only. Returns `true` if this was a
    /// previously unknown node.
    pub fn add_node(
        &mut self,
        node_id: NodeId,
        address: SocketAddr,
        incarnation: u64,
        labels: BTreeMap<String, String>,
        now: Instant,
    ) -> bool {
        use std::collections::hash_map::Entry;
        match self.members.entry(node_id.clone()) {
            Entry::Occupied(mut entry) => {
                let m = entry.get_mut();
                // Left is terminal — a returning node must wait for the
                // cleanup timeout to reap the old entry before rejoining.
                if m.state == NodeState::Left {
                    return false;
                }
                m.address = address;
                if incarnation > m.incarnation {
                    m.incarnation = incarnation;
                    if m.state != NodeState::Alive {
                        m.state_changed = now;
                    }
                    m.state = NodeState::Alive;
                }
                m.last_ack = now;
                false
            }
            Entry::Vacant(entry) => {
                entry.insert(NodeMembership {
                    node_id,
                    address,
                    state: NodeState::Alive,
                    incarnation,
                    first_seen: now,
                    last_ack: now,
                    state_changed: now,
                    resources: None,
                    labels,
                    is_council: false,
                    is_leader: false,
                });
                true
            }
        }
    }

    /// Mark a node as Suspect.
    ///
    /// Returns `true` if the state changed (was Alive before).
    pub fn suspect(&mut self, node_id: &NodeId) -> bool {
        if let Some(member) = self.members.get_mut(node_id)
            && member.state == NodeState::Alive
        {
            member.state = NodeState::Suspect;
            member.state_changed = Instant::now();
            return true;
        }
        false
    }

    /// Mark a node as Dead.
    ///
    /// Returns `true` if the state changed.
    pub fn declare_dead(&mut self, node_id: &NodeId) -> bool {
        if let Some(member) = self.members.get_mut(node_id)
            && !member.state.is_down()
        {
            member.state = NodeState::Dead;
            member.state_changed = Instant::now();
            return true;
        }
        false
    }

    /// Remove all Dead and Left nodes from the table unconditionally.
    pub fn reap_dead(&mut self) {
        self.members.retain(|_, m| !m.state.is_down());
    }

    /// Remove Dead and Left nodes that have been down for longer than `timeout`.
    ///
    /// Called each probe cycle to prevent the table from growing without
    /// bound. Only removes nodes that have been in a terminal state long
    /// enough for all cluster members to have learned about the departure
    /// via gossip dissemination.
    pub fn reap_expired_dead(&mut self, timeout: std::time::Duration, now: Instant) -> Vec<NodeId> {
        let mut reaped = Vec::new();
        self.members.retain(|id, m| {
            if m.state.is_down() && now.duration_since(m.state_changed) > timeout {
                reaped.push(id.clone());
                false
            } else {
                true
            }
        });
        reaped
    }

    /// Get a node's membership record.
    pub fn get(&self, node_id: &NodeId) -> Option<&NodeMembership> {
        self.members.get(node_id)
    }

    /// Get a mutable reference to a node's membership record.
    pub fn get_mut(&mut self, node_id: &NodeId) -> Option<&mut NodeMembership> {
        self.members.get_mut(node_id)
    }

    /// All nodes currently considered alive.
    pub fn alive_members(&self) -> Vec<&NodeMembership> {
        self.members
            .values()
            .filter(|m| m.state == NodeState::Alive)
            .collect()
    }

    /// All nodes that are alive or suspect (i.e. not confirmed down).
    pub fn active_members(&self) -> Vec<&NodeMembership> {
        self.members
            .values()
            .filter(|m| !m.state.is_down())
            .collect()
    }

    /// All nodes currently marked as council members.
    pub fn council_members(&self) -> Vec<&NodeMembership> {
        self.members
            .values()
            .filter(|m| m.is_council && !m.state.is_down())
            .collect()
    }

    /// The current leader node, if known.
    pub fn leader(&self) -> Option<&NodeMembership> {
        self.members
            .values()
            .find(|m| m.is_leader && !m.state.is_down())
    }

    /// Total number of entries in the table (all states).
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Returns `true` if the table has no entries.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Iterator over all membership records.
    pub fn iter(&self) -> impl Iterator<Item = &NodeMembership> {
        self.members.values()
    }
}

impl Default for MembershipTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// MembershipSnapshot
// ---------------------------------------------------------------------------

/// A serialisable snapshot of a single node's membership state.
///
/// Used to publish membership data via a `watch` channel without
/// sharing the `MembershipTable` across task boundaries.
#[derive(Debug, Clone)]
pub struct MembershipSnapshot {
    pub node_id: NodeId,
    pub address: SocketAddr,
    pub state: NodeState,
    pub incarnation: u64,
    pub is_council: bool,
    pub is_leader: bool,
    pub labels: BTreeMap<String, String>,
}

impl MembershipTable {
    /// Produce a snapshot of all active (non-down) members.
    pub fn snapshot(&self) -> Vec<MembershipSnapshot> {
        self.active_members()
            .into_iter()
            .map(|m| MembershipSnapshot {
                node_id: m.node_id.clone(),
                address: m.address,
                state: m.state,
                incarnation: m.incarnation,
                is_council: m.is_council,
                is_leader: m.is_leader,
                labels: m.labels.clone(),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn alive_update(node: &str, incarnation: u64) -> MembershipUpdate {
        MembershipUpdate {
            node_id: NodeId::new(node),
            address: addr(9000),
            state: NodeState::Alive,
            incarnation,
            lamport: 0,
        }
    }

    fn suspect_update(node: &str, incarnation: u64) -> MembershipUpdate {
        MembershipUpdate {
            node_id: NodeId::new(node),
            address: addr(9000),
            state: NodeState::Suspect,
            incarnation,
            lamport: 0,
        }
    }

    fn dead_update(node: &str, incarnation: u64) -> MembershipUpdate {
        MembershipUpdate {
            node_id: NodeId::new(node),
            address: addr(9000),
            state: NodeState::Dead,
            incarnation,
            lamport: 0,
        }
    }

    // -- add_node and basic queries ------------------------------------------

    #[test]
    fn add_node_creates_alive_member() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("n1"), addr(9001), 1, BTreeMap::new(), now());
        assert_eq!(table.len(), 1);
        let m = table.get(&NodeId::new("n1")).unwrap();
        assert_eq!(m.state, NodeState::Alive);
        assert_eq!(m.incarnation, 1);
    }

    #[test]
    fn add_node_updates_address_on_duplicate() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("n1"), addr(9001), 1, BTreeMap::new(), now());
        table.add_node(NodeId::new("n1"), addr(9002), 1, BTreeMap::new(), now());
        assert_eq!(table.len(), 1);
        assert_eq!(table.get(&NodeId::new("n1")).unwrap().address, addr(9002));
    }

    #[test]
    fn alive_members_excludes_suspect_and_dead() {
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("n2"), addr(2), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("n3"), addr(3), 1, BTreeMap::new(), t);

        table.suspect(&NodeId::new("n2"));
        table.declare_dead(&NodeId::new("n3"));

        let alive: Vec<_> = table
            .alive_members()
            .iter()
            .map(|m| m.node_id.clone())
            .collect();
        assert_eq!(alive, vec![NodeId::new("n1")]);
    }

    #[test]
    fn active_members_includes_suspect_excludes_dead() {
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("n2"), addr(2), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("n3"), addr(3), 1, BTreeMap::new(), t);

        table.suspect(&NodeId::new("n2"));
        table.declare_dead(&NodeId::new("n3"));

        let active: Vec<_> = table
            .active_members()
            .iter()
            .map(|m| m.node_id.clone())
            .collect();
        assert!(active.contains(&NodeId::new("n1")));
        assert!(active.contains(&NodeId::new("n2")));
        assert!(!active.contains(&NodeId::new("n3")));
    }

    // -- apply_update (conflict resolution) ----------------------------------

    #[test]
    fn apply_update_adds_new_alive_node() {
        let mut table = MembershipTable::new();
        let changed = table.apply_update(&alive_update("n1", 1), now());
        assert!(changed);
        assert_eq!(table.len(), 1);
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Alive
        );
    }

    #[test]
    fn apply_update_ignores_dead_for_unknown_node() {
        let mut table = MembershipTable::new();
        let changed = table.apply_update(&dead_update("n1", 1), now());
        assert!(!changed);
        assert!(table.is_empty());
    }

    #[test]
    fn apply_update_suspect_overrides_alive_at_same_incarnation() {
        let mut table = MembershipTable::new();
        let t = now();
        table.apply_update(&alive_update("n1", 1), t);
        let changed = table.apply_update(&suspect_update("n1", 1), t);
        assert!(changed);
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Suspect
        );
    }

    #[test]
    fn apply_update_alive_does_not_override_suspect_at_same_incarnation() {
        let mut table = MembershipTable::new();
        let t = now();
        table.apply_update(&alive_update("n1", 1), t);
        table.apply_update(&suspect_update("n1", 1), t);
        let changed = table.apply_update(&alive_update("n1", 1), t);
        assert!(!changed);
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Suspect
        );
    }

    #[test]
    fn apply_update_higher_incarnation_alive_refutes_suspect() {
        let mut table = MembershipTable::new();
        let t = now();
        table.apply_update(&alive_update("n1", 1), t);
        table.apply_update(&suspect_update("n1", 1), t);
        let changed = table.apply_update(&alive_update("n1", 2), t);
        assert!(changed);
        let m = table.get(&NodeId::new("n1")).unwrap();
        assert_eq!(m.state, NodeState::Alive);
        assert_eq!(m.incarnation, 2);
    }

    #[test]
    fn apply_update_lower_incarnation_is_ignored() {
        let mut table = MembershipTable::new();
        let t = now();
        table.apply_update(&alive_update("n1", 5), t);
        let changed = table.apply_update(&suspect_update("n1", 3), t);
        assert!(!changed);
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Alive
        );
    }

    #[test]
    fn apply_update_dead_at_same_incarnation_overrides_suspect() {
        let mut table = MembershipTable::new();
        let t = now();
        table.apply_update(&alive_update("n1", 1), t);
        table.apply_update(&suspect_update("n1", 1), t);
        let changed = table.apply_update(&dead_update("n1", 1), t);
        assert!(changed);
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Dead
        );
    }

    // -- suspect / declare_dead -----------------------------------------------

    #[test]
    fn suspect_changes_alive_to_suspect() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), now());
        assert!(table.suspect(&NodeId::new("n1")));
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Suspect
        );
    }

    #[test]
    fn suspect_noop_if_already_suspect() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), now());
        table.suspect(&NodeId::new("n1"));
        assert!(!table.suspect(&NodeId::new("n1")));
    }

    #[test]
    fn declare_dead_changes_suspect_to_dead() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), now());
        table.suspect(&NodeId::new("n1"));
        assert!(table.declare_dead(&NodeId::new("n1")));
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Dead
        );
    }

    #[test]
    fn declare_dead_noop_if_already_dead() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), now());
        table.declare_dead(&NodeId::new("n1"));
        assert!(!table.declare_dead(&NodeId::new("n1")));
    }

    // -- reap_dead -----------------------------------------------------------

    #[test]
    fn reap_dead_removes_dead_and_left_nodes() {
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("alive"), addr(1), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("suspect"), addr(2), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("dead"), addr(3), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("left"), addr(4), 1, BTreeMap::new(), t);

        table.suspect(&NodeId::new("suspect"));
        table.declare_dead(&NodeId::new("dead"));
        // Simulate a graceful leave
        table.apply_update(
            &MembershipUpdate {
                node_id: NodeId::new("left"),
                address: addr(4),
                state: NodeState::Left,
                incarnation: 1,
                lamport: 0,
            },
            t,
        );

        table.reap_dead();

        assert_eq!(table.len(), 2);
        assert!(table.get(&NodeId::new("alive")).is_some());
        assert!(table.get(&NodeId::new("suspect")).is_some());
        assert!(table.get(&NodeId::new("dead")).is_none());
        assert!(table.get(&NodeId::new("left")).is_none());
    }

    // -- council and leader ---------------------------------------------------

    #[test]
    fn council_members_returns_flagged_nodes() {
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("c1"), addr(1), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("c2"), addr(2), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("w1"), addr(3), 1, BTreeMap::new(), t);

        table.get_mut(&NodeId::new("c1")).unwrap().is_council = true;
        table.get_mut(&NodeId::new("c2")).unwrap().is_council = true;

        let council: Vec<_> = table
            .council_members()
            .iter()
            .map(|m| m.node_id.clone())
            .collect();
        assert_eq!(council.len(), 2);
        assert!(council.contains(&NodeId::new("c1")));
        assert!(council.contains(&NodeId::new("c2")));
    }

    #[test]
    fn leader_returns_flagged_node() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("leader"), addr(1), 1, BTreeMap::new(), now());
        table.get_mut(&NodeId::new("leader")).unwrap().is_leader = true;

        let leader = table.leader().unwrap();
        assert_eq!(leader.node_id, NodeId::new("leader"));
    }

    #[test]
    fn leader_returns_none_when_no_leader() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), now());
        assert!(table.leader().is_none());
    }

    #[test]
    fn dead_council_member_excluded_from_council_list() {
        let mut table = MembershipTable::new();
        table.add_node(NodeId::new("c1"), addr(1), 1, BTreeMap::new(), now());
        table.get_mut(&NodeId::new("c1")).unwrap().is_council = true;
        table.declare_dead(&NodeId::new("c1"));
        assert!(table.council_members().is_empty());
    }

    // -- reap_expired_dead ---------------------------------------------------

    #[test]
    fn dead_node_not_reaped_before_cleanup_timeout() {
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), t);
        table.declare_dead(&NodeId::new("n1"));

        // Reap immediately — timeout hasn't elapsed
        let reaped = table.reap_expired_dead(Duration::from_secs(60), Instant::now());
        assert!(reaped.is_empty());
        assert!(table.get(&NodeId::new("n1")).is_some());
    }

    #[test]
    fn dead_node_reaped_after_cleanup_timeout() {
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), t);
        table.declare_dead(&NodeId::new("n1"));

        // Backdate state_changed so the timeout has elapsed
        table.get_mut(&NodeId::new("n1")).unwrap().state_changed =
            Instant::now() - Duration::from_secs(61);

        let reaped = table.reap_expired_dead(Duration::from_secs(60), Instant::now());
        assert_eq!(reaped, vec![NodeId::new("n1")]);
        assert!(table.get(&NodeId::new("n1")).is_none());
    }

    #[test]
    fn left_node_reaped_after_cleanup_timeout() {
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), t);
        table.apply_update(
            &MembershipUpdate {
                node_id: NodeId::new("n1"),
                address: addr(1),
                state: NodeState::Left,
                incarnation: 1,
                lamport: 0,
            },
            t,
        );

        // Backdate state_changed
        table.get_mut(&NodeId::new("n1")).unwrap().state_changed =
            Instant::now() - Duration::from_secs(61);

        let reaped = table.reap_expired_dead(Duration::from_secs(60), Instant::now());
        assert_eq!(reaped, vec![NodeId::new("n1")]);
    }

    #[test]
    fn alive_and_suspect_nodes_never_reaped() {
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("alive"), addr(1), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("suspect"), addr(2), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("dead"), addr(3), 1, BTreeMap::new(), t);
        table.add_node(NodeId::new("left"), addr(4), 1, BTreeMap::new(), t);

        table.suspect(&NodeId::new("suspect"));
        table.declare_dead(&NodeId::new("dead"));
        table.apply_update(
            &MembershipUpdate {
                node_id: NodeId::new("left"),
                address: addr(4),
                state: NodeState::Left,
                incarnation: 1,
                lamport: 0,
            },
            t,
        );

        // Backdate all state_changed to long ago
        let old = Instant::now() - Duration::from_secs(120);
        for m in table.members.values_mut() {
            m.state_changed = old;
        }

        let reaped = table.reap_expired_dead(Duration::from_secs(60), Instant::now());
        assert_eq!(reaped.len(), 2);
        assert!(table.get(&NodeId::new("alive")).is_some());
        assert!(table.get(&NodeId::new("suspect")).is_some());
        assert!(table.get(&NodeId::new("dead")).is_none());
        assert!(table.get(&NodeId::new("left")).is_none());
    }

    // -- left node rejoin ----------------------------------------------------

    #[test]
    fn add_node_does_not_override_left() {
        // A node that gracefully left cannot rejoin until its entry is
        // reaped. Even a higher incarnation from add_node (direct PING)
        // must not override Left.
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), t);
        table.apply_update(
            &MembershipUpdate {
                node_id: NodeId::new("n1"),
                address: addr(1),
                state: NodeState::Left,
                incarnation: 1,
                lamport: 0,
            },
            t,
        );

        // Node comes back with higher incarnation — still blocked
        table.add_node(NodeId::new("n1"), addr(1), 5, BTreeMap::new(), now());
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Left,
        );
    }

    #[test]
    fn left_node_rejoins_after_reap() {
        // After the cleanup timeout reaps the Left entry, the returning
        // node is treated as a fresh join.
        let mut table = MembershipTable::new();
        let t = now();
        table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), t);
        table.apply_update(
            &MembershipUpdate {
                node_id: NodeId::new("n1"),
                address: addr(1),
                state: NodeState::Left,
                incarnation: 1,
                lamport: 0,
            },
            t,
        );

        // Backdate and reap
        table.get_mut(&NodeId::new("n1")).unwrap().state_changed =
            Instant::now() - Duration::from_secs(61);
        table.reap_expired_dead(Duration::from_secs(60), Instant::now());
        assert!(table.get(&NodeId::new("n1")).is_none());

        // Node rejoins — accepted as a new member
        let is_new = table.add_node(NodeId::new("n1"), addr(1), 1, BTreeMap::new(), now());
        assert!(is_new);
        assert_eq!(
            table.get(&NodeId::new("n1")).unwrap().state,
            NodeState::Alive,
        );
    }
}
