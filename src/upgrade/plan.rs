//! Server-side derivation of the rolling-upgrade node plan.
//!
//! A cluster upgrade names a set of nodes, each with an API address and a
//! role (worker / council / leader) that decides its position in the
//! rolling order. The obvious-but-wrong design lets the *client* (relish)
//! supply those fields and trusts them. That means anyone who can reach the
//! admin endpoint can claim a worker is the leader, or point the leader's
//! address at some other node, and steer the swap under a false identity.
//!
//! Instead the leader derives the authoritative identity of every node from
//! its own state — the API address from gossip membership, the role from the
//! Raft voter set and current leader — and validates the client's claims
//! against it. A request that names an unknown node, or that disagrees with
//! the authoritative address or role, is rejected. The client picks *which*
//! nodes to upgrade and how many workers at once; it does not get to invent
//! *what those nodes are*.

use super::types::{NodeRole, NodeUpgradePhase, NodeUpgradeRecord};

/// The leader's authoritative view of one node: what relish must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeNode {
    /// API address (`host:port`) the orchestrator will POST directives to.
    pub address: String,
    pub role: NodeRole,
}

/// Derive a node's authoritative role from Raft: the current leader is
/// [`NodeRole::Leader`], any other voter is [`NodeRole::Council`], and a
/// non-voter is [`NodeRole::Worker`]. `raft_id` is the node's stable Raft
/// id (from `raft_id_from_name`); `voters` is the current voter set.
pub fn role_from_raft(
    raft_id: u64,
    leader_id: Option<u64>,
    voters: &std::collections::BTreeSet<u64>,
) -> NodeRole {
    if Some(raft_id) == leader_id {
        NodeRole::Leader
    } else if voters.contains(&raft_id) {
        NodeRole::Council
    } else {
        NodeRole::Worker
    }
}

/// Why a requested node was rejected.
///
/// The two rejections both defend an invariant a false identity could
/// subvert:
///
/// - [`AddressMismatch`](PlanError::AddressMismatch): the address is where
///   the leader POSTs directives, so a wrong one aims the swap at another
///   host.
/// - [`LeaderMismatch`](PlanError::LeaderMismatch): the leader upgrades
///   LAST, in place; claiming the real leader is a worker (or a worker is
///   the leader) would break that ordering and could disrupt quorum.
///
/// A worker↔council relabel among non-leaders is not rejected — both go
/// before the leader — but the built record still carries the authoritative
/// role, so the plan the orchestrator walks never depends on the claim.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("node {node_id:?} is not a live cluster member")]
    UnknownNode { node_id: String },
    #[error(
        "role for node {node_id:?} claims {claimed:?} but the cluster's leader status disagrees ({authoritative:?})"
    )]
    LeaderMismatch {
        node_id: String,
        claimed: NodeRole,
        authoritative: NodeRole,
    },
    #[error("address for node {node_id:?} is {claimed:?} but the cluster sees {authoritative:?}")]
    AddressMismatch {
        node_id: String,
        claimed: String,
        authoritative: String,
    },
}

/// One node as named in the client's start request.
#[derive(Debug, Clone)]
pub struct RequestedNode {
    pub node_id: String,
    pub address: String,
    pub role: NodeRole,
}

/// Validate the client's node list against the leader's authoritative view
/// and build the `NodeUpgradeRecord`s the orchestrator walks.
///
/// `lookup` maps a node id to its authoritative identity (built server-side
/// from gossip + Raft). For each requested node:
///
/// - unknown to the cluster → [`PlanError::UnknownNode`];
/// - a claim that crosses the leader boundary (claims Leader but isn't, or
///   is the leader but claims otherwise) → [`PlanError::LeaderMismatch`];
/// - claimed address disagrees with the authoritative address →
///   [`PlanError::AddressMismatch`].
///
/// The built record always carries the *authoritative* address and role,
/// never the client's copy: even a worker↔council relabel (accepted, since
/// both precede the leader) is corrected to the real role, so the plan the
/// orchestrator walks never depends on the client's claim.
pub fn derive_upgrade_nodes<F>(
    requested: &[RequestedNode],
    lookup: F,
) -> Result<Vec<NodeUpgradeRecord>, PlanError>
where
    F: Fn(&str) -> Option<AuthoritativeNode>,
{
    let mut records = Vec::with_capacity(requested.len());
    for node in requested {
        let authoritative = lookup(&node.node_id).ok_or_else(|| PlanError::UnknownNode {
            node_id: node.node_id.clone(),
        })?;

        // Leadership can't be claimed or denied: it decides the leader-last
        // ordering and quorum handling.
        let claims_leader = node.role == NodeRole::Leader;
        let is_leader = authoritative.role == NodeRole::Leader;
        if claims_leader != is_leader {
            return Err(PlanError::LeaderMismatch {
                node_id: node.node_id.clone(),
                claimed: node.role,
                authoritative: authoritative.role,
            });
        }
        if node.address != authoritative.address {
            return Err(PlanError::AddressMismatch {
                node_id: node.node_id.clone(),
                claimed: node.address.clone(),
                authoritative: authoritative.address.clone(),
            });
        }

        records.push(NodeUpgradeRecord {
            node_id: node.node_id.clone(),
            // Authoritative, not the client's copy.
            address: authoritative.address,
            role: authoritative.role,
            from_version: None,
            phase: NodeUpgradePhase::Pending,
            since: None,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn requested(id: &str, address: &str, role: NodeRole) -> RequestedNode {
        RequestedNode {
            node_id: id.to_string(),
            address: address.to_string(),
            role,
        }
    }

    fn view() -> HashMap<String, AuthoritativeNode> {
        HashMap::from([
            (
                "leader".to_string(),
                AuthoritativeNode {
                    address: "10.0.0.1:9117".to_string(),
                    role: NodeRole::Leader,
                },
            ),
            (
                "c1".to_string(),
                AuthoritativeNode {
                    address: "10.0.0.2:9117".to_string(),
                    role: NodeRole::Council,
                },
            ),
            (
                "w1".to_string(),
                AuthoritativeNode {
                    address: "10.0.0.3:9117".to_string(),
                    role: NodeRole::Worker,
                },
            ),
        ])
    }

    #[test]
    fn matching_request_builds_authoritative_records() {
        let view = view();
        let requested = vec![
            requested("w1", "10.0.0.3:9117", NodeRole::Worker),
            requested("c1", "10.0.0.2:9117", NodeRole::Council),
            requested("leader", "10.0.0.1:9117", NodeRole::Leader),
        ];
        let records = derive_upgrade_nodes(&requested, |id| view.get(id).cloned()).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].address, "10.0.0.3:9117");
        assert_eq!(records[2].role, NodeRole::Leader);
    }

    #[test]
    fn claiming_a_non_leader_is_the_leader_is_rejected() {
        let view = view();
        // Claim a worker is the leader — that would put it last in the
        // rolling order and let a caller steer the leader-last invariant.
        let requested = vec![requested("w1", "10.0.0.3:9117", NodeRole::Leader)];
        let err = derive_upgrade_nodes(&requested, |id| view.get(id).cloned()).unwrap_err();
        assert!(matches!(
            err,
            PlanError::LeaderMismatch {
                authoritative: NodeRole::Worker,
                ..
            }
        ));
    }

    #[test]
    fn denying_the_real_leader_is_rejected() {
        let view = view();
        // Claim the real leader is a mere worker — it must still go LAST.
        let requested = vec![requested("leader", "10.0.0.1:9117", NodeRole::Worker)];
        let err = derive_upgrade_nodes(&requested, |id| view.get(id).cloned()).unwrap_err();
        assert!(matches!(
            err,
            PlanError::LeaderMismatch {
                authoritative: NodeRole::Leader,
                ..
            }
        ));
    }

    #[test]
    fn worker_council_relabel_is_corrected_not_rejected() {
        let view = view();
        // A council voter labelled Worker: both precede the leader, so this
        // is accepted, but the record carries the authoritative Council role
        // (this is exactly the 4-voter cluster case the gated tests hit).
        let requested = vec![requested("c1", "10.0.0.2:9117", NodeRole::Worker)];
        let records = derive_upgrade_nodes(&requested, |id| view.get(id).cloned()).unwrap();
        assert_eq!(records[0].role, NodeRole::Council);
    }

    #[test]
    fn spoofed_address_is_rejected() {
        let view = view();
        // Point the leader's address at another host.
        let requested = vec![requested("leader", "10.0.0.9:9117", NodeRole::Leader)];
        let err = derive_upgrade_nodes(&requested, |id| view.get(id).cloned()).unwrap_err();
        assert!(matches!(err, PlanError::AddressMismatch { .. }));
    }

    #[test]
    fn role_from_raft_maps_leader_council_worker() {
        let voters = std::collections::BTreeSet::from([1u64, 2, 3]);
        // The current leader.
        assert_eq!(role_from_raft(1, Some(1), &voters), NodeRole::Leader);
        // Another voter.
        assert_eq!(role_from_raft(2, Some(1), &voters), NodeRole::Council);
        // A non-voter.
        assert_eq!(role_from_raft(9, Some(1), &voters), NodeRole::Worker);
        // No leader known yet: even a voter is Council, not Leader.
        assert_eq!(role_from_raft(1, None, &voters), NodeRole::Council);
    }

    #[test]
    fn unknown_node_is_rejected() {
        let view = view();
        let requested = vec![requested("ghost", "10.0.0.9:9117", NodeRole::Worker)];
        let err = derive_upgrade_nodes(&requested, |id| view.get(id).cloned()).unwrap_err();
        assert_eq!(
            err,
            PlanError::UnknownNode {
                node_id: "ghost".to_string()
            }
        );
    }
}
