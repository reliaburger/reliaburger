//! Node directory built from gossip directory extensions (Phase 12b.2).
//!
//! Every node advertises its control-plane endpoints (API, reporting) and
//! relays the highest-term leader hint it has seen on each gossip datagram.
//! The [`MustardNode`](super::protocol::MustardNode) folds those extensions
//! into a `NodeDirectory` and publishes it on a `watch` channel, giving
//! every node — voter or not — a route to the current leader. This is what
//! lets a cluster grow past the Raft council: Raft metrics only tell voters
//! who leads; the gossip directory tells everyone.

use std::collections::HashMap;
use std::net::SocketAddr;

use crate::meat::NodeId;

use super::message::{DirectoryExtension, LeaderHint};

/// A node's advertised control-plane endpoints, learned via gossip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeEndpoints {
    /// The node's HTTP API endpoint.
    pub api_address: SocketAddr,
    /// The node's reporting-tree endpoint.
    pub reporting_address: SocketAddr,
}

/// The gossip-learned control-plane directory: per-node endpoints plus the
/// best (highest-term) leader hint observed so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeDirectory {
    /// Advertised endpoints per node.
    pub endpoints: HashMap<NodeId, NodeEndpoints>,
    /// Best known leader; terms only grow, so highest term wins.
    pub leader: Option<LeaderHint>,
}

impl NodeDirectory {
    /// Fold one received extension in. Returns `true` if the directory
    /// changed (callers republish the watch only on change).
    pub fn observe(&mut self, extension: &DirectoryExtension) -> bool {
        let endpoints = NodeEndpoints {
            api_address: extension.api_address,
            reporting_address: extension.reporting_address,
        };
        let mut changed = match self.endpoints.get(&extension.node_id) {
            Some(existing) if *existing == endpoints => false,
            _ => {
                self.endpoints.insert(extension.node_id.clone(), endpoints);
                true
            }
        };
        if let Some(hint) = &extension.leader {
            changed |= self.observe_hint(hint);
        }
        changed
    }

    /// Adopt `hint` if it is newer (higher term) than the current one, or
    /// same-term with different content (a leader re-advertising moved
    /// endpoints). Returns `true` if adopted.
    pub fn observe_hint(&mut self, hint: &LeaderHint) -> bool {
        let newer = match &self.leader {
            Some(current) => {
                hint.term > current.term || (hint.term == current.term && hint != current)
            }
            None => true,
        };
        if newer {
            self.leader = Some(hint.clone());
        }
        newer
    }

    /// Drop endpoint entries for reaped members. Returns `true` if any were
    /// removed. The leader hint is kept even if the leader was reaped — a
    /// higher-term hint from the next leader replaces it.
    pub fn prune(&mut self, reaped: &[NodeId]) -> bool {
        let before = self.endpoints.len();
        for node_id in reaped {
            self.endpoints.remove(node_id);
        }
        self.endpoints.len() != before
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    fn extension(node: &str, api: u16, leader: Option<LeaderHint>) -> DirectoryExtension {
        DirectoryExtension {
            node_id: NodeId::new(node),
            api_address: addr(api),
            reporting_address: addr(api + 1),
            leader,
            hmac: [0u8; 32],
        }
    }

    fn hint(node: &str, term: u64) -> LeaderHint {
        LeaderHint {
            node_id: NodeId::new(node),
            term,
            api_address: addr(9117),
            reporting_address: addr(9445),
        }
    }

    #[test]
    fn observe_records_endpoints_and_reports_change() {
        let mut dir = NodeDirectory::default();
        assert!(dir.observe(&extension("n1", 9117, None)));
        assert_eq!(
            dir.endpoints.get(&NodeId::new("n1")).unwrap().api_address,
            addr(9117)
        );
        // Same content again: no change.
        assert!(!dir.observe(&extension("n1", 9117, None)));
        // Moved endpoint: change.
        assert!(dir.observe(&extension("n1", 9200, None)));
    }

    #[test]
    fn higher_term_hint_replaces_lower() {
        let mut dir = NodeDirectory::default();
        assert!(dir.observe_hint(&hint("a", 3)));
        assert!(dir.observe_hint(&hint("b", 5)));
        assert_eq!(dir.leader.as_ref().unwrap().node_id, NodeId::new("b"));
    }

    #[test]
    fn stale_hint_is_ignored() {
        let mut dir = NodeDirectory::default();
        dir.observe_hint(&hint("b", 5));
        // A deposed leader's replayed hint loses on term.
        assert!(!dir.observe_hint(&hint("a", 3)));
        assert_eq!(dir.leader.as_ref().unwrap().node_id, NodeId::new("b"));
    }

    #[test]
    fn same_term_different_content_is_adopted() {
        let mut dir = NodeDirectory::default();
        dir.observe_hint(&hint("a", 3));
        let mut moved = hint("a", 3);
        moved.api_address = addr(9300);
        assert!(dir.observe_hint(&moved));
        assert_eq!(dir.leader.as_ref().unwrap().api_address, addr(9300));
        // Identical hint: no change (no watch churn).
        assert!(!dir.observe_hint(&moved));
    }

    #[test]
    fn prune_removes_reaped_members_but_keeps_the_hint() {
        let mut dir = NodeDirectory::default();
        dir.observe(&extension("n1", 9117, Some(hint("n1", 2))));
        dir.observe(&extension("n2", 9127, None));
        assert!(dir.prune(&[NodeId::new("n1")]));
        assert!(!dir.endpoints.contains_key(&NodeId::new("n1")));
        assert!(dir.endpoints.contains_key(&NodeId::new("n2")));
        // The hint survives until a higher-term one arrives.
        assert!(dir.leader.is_some());
        assert!(!dir.prune(&[NodeId::new("gone")]));
    }
}
