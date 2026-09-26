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

use super::error::UpgradeError;
use super::types::{NodeRole, NodeUpgradePhase, NodeUpgradeRecord};
use super::version::BinaryVersion;

/// The leader's authoritative view of one node: what relish must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeNode {
    /// API address (`host:port`) the orchestrator will POST directives to,
    /// as the node advertised it over gossip. `None` while the leader only
    /// knows the node from another member's membership sync and has yet to
    /// hear its advertisement: a derived guess is not an identity.
    pub address: Option<String>,
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
///
/// [`AddressNotAdvertised`](PlanError::AddressNotAdvertised) is different in
/// kind: the leader can't check the claim yet, so it refuses rather than
/// compare against a guess (see [`PlanError::is_transient`]).
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
    #[error("node {node_id:?} has not advertised its API address to the leader yet; retry shortly")]
    AddressNotAdvertised { node_id: String },
}

impl PlanError {
    /// `true` when the refusal reflects gossip still converging (the same
    /// request can succeed moments later), not a claim the cluster
    /// contradicts.
    pub fn is_transient(&self) -> bool {
        matches!(self, PlanError::AddressNotAdvertised { .. })
    }
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
/// - the node hasn't advertised its address to the leader yet →
///   [`PlanError::AddressNotAdvertised`];
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
        let Some(address) = authoritative.address else {
            return Err(PlanError::AddressNotAdvertised {
                node_id: node.node_id.clone(),
            });
        };
        if node.address != address {
            return Err(PlanError::AddressMismatch {
                node_id: node.node_id.clone(),
                claimed: node.address.clone(),
                authoritative: address,
            });
        }

        records.push(NodeUpgradeRecord {
            node_id: node.node_id.clone(),
            // Authoritative, not the client's copy.
            address,
            role: authoritative.role,
            from_version: None,
            phase: NodeUpgradePhase::Pending,
            directive_retry: None,
            since: None,
        });
    }
    Ok(records)
}

/// What one node reports running, as input to [`check_target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningBinary {
    /// How to name the node in an error: `node n1`, or `this node`.
    pub node: String,
    pub version: BinaryVersion,
    /// Hex SHA-256 of the running executable. `None` when the node could
    /// not (or does not) report it.
    pub sha256: Option<String>,
}

/// Verdict of [`check_target`] when the upgrade may go ahead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetCheck {
    /// At least one node needs the candidate.
    Proceed,
    /// Every node already runs exactly these bytes: nothing to do.
    AlreadyRunning,
}

/// Gate an upgrade on what the nodes run *now*.
///
/// The rolling walk and the binary store are both keyed by version, so a
/// candidate that shares the running version can't swap anything: the
/// orchestrator would see the target version and call the node healthy.
/// This makes that explicit:
///
/// - a node on the target version with different (or unknown) bytes:
///   [`UpgradeError::SameVersionDifferentBinary`];
/// - a node on a *newer* version, unless `allow_downgrade`:
///   [`UpgradeError::DowngradeRefused`];
/// - every node already on the target with identical bytes:
///   [`TargetCheck::AlreadyRunning`].
///
/// Rollbacks don't come through here: they return to a binary that's
/// already on disk and verified.
pub fn check_target(
    target: &BinaryVersion,
    candidate_sha256: &str,
    allow_downgrade: bool,
    running: &[RunningBinary],
) -> Result<TargetCheck, UpgradeError> {
    let mut already_running = 0;
    for node in running {
        if node.version == *target {
            let same_bytes = node
                .sha256
                .as_deref()
                .is_some_and(|sha| sha.eq_ignore_ascii_case(candidate_sha256));
            if !same_bytes {
                return Err(UpgradeError::SameVersionDifferentBinary {
                    node: node.node.clone(),
                    version: target.clone(),
                    running: node.sha256.clone().unwrap_or_else(|| "unknown".to_string()),
                    candidate: candidate_sha256.to_string(),
                });
            }
            already_running += 1;
        } else if node.version > *target && !allow_downgrade {
            return Err(UpgradeError::DowngradeRefused {
                node: node.node.clone(),
                running: node.version.clone(),
                target: target.clone(),
            });
        }
    }
    if !running.is_empty() && already_running == running.len() {
        Ok(TargetCheck::AlreadyRunning)
    } else {
        Ok(TargetCheck::Proceed)
    }
}

/// Whether one node can accept a cluster (network) upgrade directive, as
/// input to [`check_network_prerequisites`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkReadiness {
    /// How to name the node in an error: `node n1`.
    pub node: String,
    /// False when the node has no `upgrades.external_signing_key`, has no
    /// upgrade manager, or doesn't report the field at all: each of those
    /// refuses every network directive.
    pub accepts_network_upgrades: bool,
}

/// Refuse a cluster upgrade that every node would refuse anyway.
///
/// Cluster directives always fetch the binary from Pickle, so every node
/// demands the operator's external signature and a key to check it with.
/// Recording a run that the first node rejects leaves a paused upgrade
/// behind, and that paused upgrade blocks every later start until an
/// operator clears it. Checking up front turns that into one clear 409.
pub fn check_network_prerequisites(
    external_signature: Option<&str>,
    nodes: &[NetworkReadiness],
) -> Result<(), UpgradeError> {
    if external_signature.is_none_or(str::is_empty) {
        return Err(UpgradeError::ExternalSignatureRequired);
    }
    let unready: Vec<&str> = nodes
        .iter()
        .filter(|node| !node.accepts_network_upgrades)
        .map(|node| node.node.as_str())
        .collect();
    if unready.is_empty() {
        Ok(())
    } else {
        Err(UpgradeError::NodesLackExternalKey {
            nodes: unready.join(", "),
        })
    }
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
                    address: Some("10.0.0.1:9117".to_string()),
                    role: NodeRole::Leader,
                },
            ),
            (
                "c1".to_string(),
                AuthoritativeNode {
                    address: Some("10.0.0.2:9117".to_string()),
                    role: NodeRole::Council,
                },
            ),
            (
                "w1".to_string(),
                AuthoritativeNode {
                    address: Some("10.0.0.3:9117".to_string()),
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
    fn unadvertised_address_is_refused_as_not_yet_known() {
        // A restarted leader learns a member from a peer's membership sync
        // before that member's own gossip tells it the API endpoint. Until
        // then it only has a port-offset guess, which is wrong whenever nodes
        // pick their ports independently. Comparing the client's (correct)
        // address against that guess used to report a spurious mismatch.
        let mut view = view();
        view.insert(
            "fresh".to_string(),
            AuthoritativeNode {
                address: None,
                role: NodeRole::Council,
            },
        );
        let requested = vec![requested("fresh", "10.0.0.4:9117", NodeRole::Council)];
        let err = derive_upgrade_nodes(&requested, |id| view.get(id).cloned()).unwrap_err();
        assert_eq!(
            err,
            PlanError::AddressNotAdvertised {
                node_id: "fresh".to_string()
            }
        );
        assert!(err.is_transient());
    }

    #[test]
    fn identity_rejections_are_not_transient() {
        let view = view();
        let requested = vec![requested("leader", "10.0.0.9:9117", NodeRole::Leader)];
        let err = derive_upgrade_nodes(&requested, |id| view.get(id).cloned()).unwrap_err();
        assert!(!err.is_transient());
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

    fn running(node: &str, version: &str, sha256: Option<&str>) -> RunningBinary {
        RunningBinary {
            node: format!("node {node}"),
            version: version.parse().unwrap(),
            sha256: sha256.map(String::from),
        }
    }

    #[test]
    fn target_newer_than_every_node_proceeds() {
        let nodes = [
            running("a", "v0.1.0", Some("aa")),
            running("b", "v0.1.0", None),
        ];
        let verdict = check_target(&"v0.2.0".parse().unwrap(), "bb", false, &nodes).unwrap();
        assert_eq!(verdict, TargetCheck::Proceed);
    }

    #[test]
    fn same_version_with_different_bytes_is_refused() {
        let nodes = [
            running("a", "v0.1.0", Some("aa")),
            running("b", "v0.1.0", Some("aa")),
        ];
        let err = check_target(&"v0.1.0".parse().unwrap(), "bb", false, &nodes).unwrap_err();
        assert!(
            matches!(err, UpgradeError::SameVersionDifferentBinary { ref node, .. } if node == "node a"),
            "{err}"
        );
        assert!(err.to_string().contains("give the candidate a new version"));
    }

    #[test]
    fn same_version_with_unknown_bytes_is_refused() {
        let nodes = [running("a", "v0.1.0", None)];
        let err = check_target(&"v0.1.0".parse().unwrap(), "bb", false, &nodes).unwrap_err();
        assert!(matches!(
            err,
            UpgradeError::SameVersionDifferentBinary { .. }
        ));
    }

    #[test]
    fn same_version_with_identical_bytes_everywhere_is_already_running() {
        let nodes = [
            running("a", "v0.1.0", Some("AB")),
            running("b", "v0.1.0", Some("ab")),
        ];
        let verdict = check_target(&"v0.1.0".parse().unwrap(), "ab", false, &nodes).unwrap();
        assert_eq!(verdict, TargetCheck::AlreadyRunning);
    }

    #[test]
    fn a_partly_upgraded_cluster_proceeds_for_the_rest() {
        // Starting again after a partial walk: finished nodes are fine.
        let nodes = [
            running("a", "v0.2.0", Some("bb")),
            running("b", "v0.1.0", Some("aa")),
        ];
        let verdict = check_target(&"v0.2.0".parse().unwrap(), "bb", false, &nodes).unwrap();
        assert_eq!(verdict, TargetCheck::Proceed);
    }

    #[test]
    fn downgrade_is_refused_without_the_flag() {
        // 0.1.0-soak.1 sorts BEFORE 0.1.0 (semver pre-release rules).
        let nodes = [running("a", "v0.1.0", Some("aa"))];
        let target = "v0.1.0-soak.1".parse().unwrap();
        let err = check_target(&target, "bb", false, &nodes).unwrap_err();
        assert!(
            matches!(err, UpgradeError::DowngradeRefused { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("--allow-downgrade"));
    }

    #[test]
    fn downgrade_proceeds_with_the_flag() {
        let nodes = [running("a", "v0.1.0", Some("aa"))];
        let target = "v0.1.0-soak.1".parse().unwrap();
        let verdict = check_target(&target, "bb", true, &nodes).unwrap();
        assert_eq!(verdict, TargetCheck::Proceed);
    }

    #[test]
    fn the_flag_does_not_excuse_same_version_different_bytes() {
        let nodes = [running("a", "v0.1.0", Some("aa"))];
        let err = check_target(&"v0.1.0".parse().unwrap(), "bb", true, &nodes).unwrap_err();
        assert!(matches!(
            err,
            UpgradeError::SameVersionDifferentBinary { .. }
        ));
    }

    #[test]
    fn no_reachable_nodes_proceeds() {
        let verdict = check_target(&"v0.2.0".parse().unwrap(), "bb", false, &[]).unwrap();
        assert_eq!(verdict, TargetCheck::Proceed);
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

    fn readiness(node: &str, accepts: bool) -> NetworkReadiness {
        NetworkReadiness {
            node: format!("node {node}"),
            accepts_network_upgrades: accepts,
        }
    }

    #[test]
    fn cluster_upgrade_without_an_external_signature_is_refused() {
        let nodes = [readiness("a", true)];
        let err = check_network_prerequisites(None, &nodes).unwrap_err();
        assert!(
            matches!(err, UpgradeError::ExternalSignatureRequired),
            "{err}"
        );
        let err = check_network_prerequisites(Some(""), &nodes).unwrap_err();
        assert!(matches!(err, UpgradeError::ExternalSignatureRequired));
    }

    #[test]
    fn cluster_upgrade_is_refused_when_a_node_has_no_external_key() {
        let nodes = [
            readiness("a", true),
            readiness("b", false),
            readiness("c", false),
        ];
        let err = check_network_prerequisites(Some("sig"), &nodes).unwrap_err();
        assert!(
            matches!(err, UpgradeError::NodesLackExternalKey { ref nodes } if nodes == "node b, node c"),
            "{err}"
        );
        assert!(err.to_string().contains("upgrades.external_signing_key"));
    }

    #[test]
    fn cluster_upgrade_proceeds_when_every_node_can_verify() {
        let nodes = [readiness("a", true), readiness("b", true)];
        check_network_prerequisites(Some("sig"), &nodes).unwrap();
        check_network_prerequisites(Some("sig"), &[]).unwrap();
    }
}
