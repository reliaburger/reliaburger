//! Coordinator election for Lettuce GitOps.
//!
//! The GitOps coordinator is a council member role, separate from
//! the Raft leader. The leader elects a coordinator (preferring
//! non-leader members to distribute load). If the coordinator fails,
//! a new one is elected within seconds.

use std::time::{SystemTime, UNIX_EPOCH};

use super::types::{CoordinatorElection, CoordinatorElectionReason};

/// Select a coordinator from a list of council member node IDs.
///
/// Prefers non-leader members. If all members are the leader (single
/// node), the leader is selected. Uses deterministic selection
/// (first non-leader, alphabetically) for consistency.
pub fn select_coordinator(
    council_members: &[String],
    leader_id: &str,
    reason: CoordinatorElectionReason,
) -> Option<CoordinatorElection> {
    if council_members.is_empty() {
        return None;
    }

    // Prefer non-leader members
    let non_leaders: Vec<&String> = council_members
        .iter()
        .filter(|id| id.as_str() != leader_id)
        .collect();

    let selected = if non_leaders.is_empty() {
        // Single-node cluster: leader is the coordinator
        leader_id.to_string()
    } else {
        // First non-leader alphabetically (deterministic)
        let mut sorted = non_leaders;
        sorted.sort();
        sorted[0].clone()
    };

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Some(CoordinatorElection {
        node_id: selected,
        reason,
        timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_non_leader() {
        let members = vec![
            "node-01".to_string(),
            "node-02".to_string(),
            "node-03".to_string(),
        ];
        let election =
            select_coordinator(&members, "node-01", CoordinatorElectionReason::Initial).unwrap();
        assert_ne!(election.node_id, "node-01", "should prefer non-leader");
        assert_eq!(
            election.node_id, "node-02",
            "should pick first non-leader alphabetically"
        );
    }

    #[test]
    fn single_node_selects_leader() {
        let members = vec!["node-01".to_string()];
        let election =
            select_coordinator(&members, "node-01", CoordinatorElectionReason::Initial).unwrap();
        assert_eq!(election.node_id, "node-01");
    }

    #[test]
    fn empty_council_returns_none() {
        let result = select_coordinator(&[], "node-01", CoordinatorElectionReason::Initial);
        assert!(result.is_none());
    }

    #[test]
    fn failover_selects_different_node() {
        let members = vec![
            "node-01".to_string(),
            "node-02".to_string(),
            "node-03".to_string(),
        ];
        // node-02 was coordinator and failed, node-01 is leader
        let election =
            select_coordinator(&members, "node-01", CoordinatorElectionReason::Failover).unwrap();
        assert_eq!(election.reason, CoordinatorElectionReason::Failover);
        // Still picks first non-leader
        assert_eq!(election.node_id, "node-02");
    }

    #[test]
    fn deterministic_selection() {
        let members = vec![
            "node-03".to_string(),
            "node-01".to_string(),
            "node-02".to_string(),
        ];
        let e1 =
            select_coordinator(&members, "node-01", CoordinatorElectionReason::Initial).unwrap();
        let e2 =
            select_coordinator(&members, "node-01", CoordinatorElectionReason::Initial).unwrap();
        assert_eq!(e1.node_id, e2.node_id, "should be deterministic");
    }
}
