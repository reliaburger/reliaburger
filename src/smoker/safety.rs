/// Safety rail evaluation for fault injection.
///
/// Every fault must pass these checks before the leader approves it.
/// Two rails are non-overridable (QuorumRisk, ReplicaMinimum) and two
/// can be overridden with explicit CLI flags (LeaderTargeted,
/// NodePercentageExceeded).
use super::types::{FaultRequest, FaultType, SafetyCheck, SafetyContext, SafetyViolation};

/// Evaluate all safety rails for a proposed fault.
///
/// Returns a `SafetyCheck` with `approved = true` if the fault is safe
/// to inject, or `approved = false` with the first violation found.
pub fn evaluate_safety(request: &FaultRequest, context: &SafetyContext) -> SafetyCheck {
    // 1. Quorum protection — cannot be overridden
    if let Some(violation) = check_quorum_risk(request, context) {
        return SafetyCheck {
            approved: false,
            violation: Some(violation),
            context: context.clone(),
        };
    }

    // 2. Replica minimum — cannot be overridden
    if let Some(violation) = check_replica_minimum(request, context) {
        return SafetyCheck {
            approved: false,
            violation: Some(violation),
            context: context.clone(),
        };
    }

    // 3. Leader protection — overridable with --include-leader
    if !request.include_leader
        && let Some(violation) = check_leader_targeted(request, context)
    {
        return SafetyCheck {
            approved: false,
            violation: Some(violation),
            context: context.clone(),
        };
    }

    // 4. Node percentage — overridable with --override-safety
    if !request.override_safety
        && let Some(violation) = check_node_percentage(request, context)
    {
        return SafetyCheck {
            approved: false,
            violation: Some(violation),
            context: context.clone(),
        };
    }

    SafetyCheck {
        approved: true,
        violation: None,
        context: context.clone(),
    }
}

/// Check whether the fault would risk Raft quorum.
///
/// A fault that targets a council node is only allowed if the total
/// number of affected council nodes stays at or below
/// `(council_size - 1) / 2`. This preserves a strict majority.
fn check_quorum_risk(request: &FaultRequest, context: &SafetyContext) -> Option<SafetyViolation> {
    // Only node-level and partition faults affect council membership
    let targets_node = matches!(
        request.fault_type,
        FaultType::NodeKill { .. } | FaultType::NodeDrain | FaultType::Partition { .. }
    );
    if !targets_node {
        return None;
    }

    if context.council_size == 0 {
        return None;
    }

    let max_allowed = (context.council_size - 1) / 2;
    let would_be_affected = context.council_nodes_with_active_faults + 1;

    if would_be_affected > max_allowed {
        Some(SafetyViolation::QuorumRisk {
            current_affected: context.council_nodes_with_active_faults,
            max_allowed,
        })
    } else {
        None
    }
}

/// Check whether the fault would kill all replicas of the target service.
///
/// At least one replica must survive. This rail applies to Kill, Pause,
/// and resource faults that could make instances unavailable.
fn check_replica_minimum(
    request: &FaultRequest,
    context: &SafetyContext,
) -> Option<SafetyViolation> {
    let kills_instances = matches!(
        request.fault_type,
        FaultType::Kill { .. } | FaultType::Pause | FaultType::MemoryPressure { oom: true, .. }
    );
    if !kills_instances {
        return None;
    }

    if context.target_service_replicas == 0 {
        return None;
    }

    let kill_count = match &request.fault_type {
        FaultType::Kill { count } => {
            if *count == 0 {
                context.target_service_replicas
            } else {
                *count
            }
        }
        _ => context.target_service_replicas, // Pause/OOM affects all targeted
    };

    let already_faulted = context.target_service_faulted_replicas;
    let total_affected = already_faulted + kill_count;
    let surviving = context
        .target_service_replicas
        .saturating_sub(total_affected);

    if surviving == 0 {
        Some(SafetyViolation::ReplicaMinimum {
            service: request.target_service.clone(),
            current_replicas: context.target_service_replicas,
            surviving,
        })
    } else {
        None
    }
}

/// Check whether the fault targets the cluster leader.
fn check_leader_targeted(
    request: &FaultRequest,
    context: &SafetyContext,
) -> Option<SafetyViolation> {
    let targets_node = matches!(
        request.fault_type,
        FaultType::NodeKill { .. } | FaultType::NodeDrain
    );
    if !targets_node {
        return None;
    }

    if let Some(target_node) = &request.target_node
        && *target_node == context.leader_node_id
    {
        return Some(SafetyViolation::LeaderTargeted);
    }

    None
}

/// Check whether the fault would affect more than 50% of nodes.
fn check_node_percentage(
    request: &FaultRequest,
    context: &SafetyContext,
) -> Option<SafetyViolation> {
    let targets_node = matches!(
        request.fault_type,
        FaultType::NodeKill { .. } | FaultType::NodeDrain
    );
    if !targets_node {
        return None;
    }

    if context.total_nodes == 0 {
        return None;
    }

    let would_be_affected = context.nodes_with_active_faults + 1;
    let threshold = context.total_nodes.div_ceil(2); // >50%, i.e. majority

    if would_be_affected > threshold {
        Some(SafetyViolation::NodePercentageExceeded {
            affected_nodes: would_be_affected,
            total_nodes: context.total_nodes,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn default_context() -> SafetyContext {
        SafetyContext {
            council_size: 5,
            council_nodes_with_active_faults: 0,
            leader_node_id: "node-1".into(),
            total_nodes: 6,
            nodes_with_active_faults: 0,
            target_service_replicas: 3,
            target_service_faulted_replicas: 0,
        }
    }

    fn node_kill_request(target_node: &str) -> FaultRequest {
        FaultRequest {
            fault_type: FaultType::NodeKill {
                kill_containers: false,
            },
            target_service: "".into(),
            target_instance: None,
            target_node: Some(target_node.into()),
            duration: Duration::from_secs(30),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        }
    }

    fn kill_request(service: &str, count: u32) -> FaultRequest {
        FaultRequest {
            fault_type: FaultType::Kill { count },
            target_service: service.into(),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(30),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        }
    }

    // -----------------------------------------------------------------------
    // Quorum protection
    // -----------------------------------------------------------------------

    #[test]
    fn quorum_risk_rejects_majority_partition() {
        let mut ctx = default_context();
        // 5-member council, 2 already affected → max_allowed = 2, would_be = 3
        ctx.council_nodes_with_active_faults = 2;
        let req = node_kill_request("node-3");
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
        assert!(matches!(
            check.violation,
            Some(SafetyViolation::QuorumRisk { .. })
        ));
    }

    #[test]
    fn quorum_risk_allows_minority_partition() {
        let mut ctx = default_context();
        // 5-member council, 1 already affected → max_allowed = 2, would_be = 2
        ctx.council_nodes_with_active_faults = 1;
        let req = node_kill_request("node-3");
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    #[test]
    fn quorum_risk_allows_first_fault() {
        let ctx = default_context();
        let req = node_kill_request("node-3");
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    #[test]
    fn quorum_risk_three_member_council() {
        let mut ctx = default_context();
        ctx.council_size = 3;
        // max_allowed = 1, already 1 → would_be = 2 → rejected
        ctx.council_nodes_with_active_faults = 1;
        let req = node_kill_request("node-3");
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
        assert!(matches!(
            check.violation,
            Some(SafetyViolation::QuorumRisk { max_allowed: 1, .. })
        ));
    }

    #[test]
    fn quorum_risk_does_not_apply_to_process_faults() {
        let mut ctx = default_context();
        ctx.council_nodes_with_active_faults = 10; // would fail if checked
        let req = kill_request("redis", 1);
        let check = evaluate_safety(&req, &ctx);
        // Kill doesn't target nodes, so quorum check doesn't apply
        assert!(check.approved);
    }

    // -----------------------------------------------------------------------
    // Replica minimum
    // -----------------------------------------------------------------------

    #[test]
    fn replica_minimum_rejects_last_replica() {
        let ctx = default_context(); // 3 replicas, 0 faulted
        let req = kill_request("web", 0); // count=0 means kill all
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
        assert!(matches!(
            check.violation,
            Some(SafetyViolation::ReplicaMinimum { surviving: 0, .. })
        ));
    }

    #[test]
    fn replica_minimum_allows_n_minus_1() {
        let ctx = default_context(); // 3 replicas
        let req = kill_request("web", 2); // kill 2, 1 survives
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    #[test]
    fn replica_minimum_rejects_when_already_faulted() {
        let mut ctx = default_context(); // 3 replicas
        ctx.target_service_faulted_replicas = 2; // 2 already faulted
        let req = kill_request("web", 1); // kill 1 more → 0 survive
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
        assert!(matches!(
            check.violation,
            Some(SafetyViolation::ReplicaMinimum { surviving: 0, .. })
        ));
    }

    #[test]
    fn replica_minimum_allows_single_kill_with_headroom() {
        let mut ctx = default_context();
        ctx.target_service_replicas = 5;
        let req = kill_request("web", 1);
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    #[test]
    fn replica_minimum_applies_to_pause() {
        let ctx = default_context(); // 3 replicas
        let req = FaultRequest {
            fault_type: FaultType::Pause,
            target_service: "web".into(),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(30),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        };
        // Pause affects all replicas → 0 survive
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
    }

    #[test]
    fn replica_minimum_applies_to_oom() {
        let ctx = default_context();
        let req = FaultRequest {
            fault_type: FaultType::MemoryPressure {
                percentage: 0,
                oom: true,
            },
            target_service: "web".into(),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(30),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        };
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
    }

    #[test]
    fn replica_minimum_does_not_apply_to_delay() {
        let ctx = default_context();
        let req = FaultRequest {
            fault_type: FaultType::Delay {
                delay_ns: 200_000_000,
                jitter_ns: 0,
            },
            target_service: "web".into(),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(30),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        };
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    // -----------------------------------------------------------------------
    // Leader protection
    // -----------------------------------------------------------------------

    #[test]
    fn leader_targeted_without_flag_rejected() {
        let ctx = default_context(); // leader is node-1
        let req = node_kill_request("node-1"); // targeting the leader
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
        assert!(matches!(
            check.violation,
            Some(SafetyViolation::LeaderTargeted)
        ));
    }

    #[test]
    fn leader_targeted_with_flag_allowed() {
        let ctx = default_context();
        let mut req = node_kill_request("node-1");
        req.include_leader = true;
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    #[test]
    fn non_leader_node_not_flagged() {
        let ctx = default_context();
        let req = node_kill_request("node-3"); // not the leader
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    // -----------------------------------------------------------------------
    // Node percentage
    // -----------------------------------------------------------------------

    #[test]
    fn node_percentage_exceeded_without_override_rejected() {
        let mut ctx = default_context(); // 6 nodes
        ctx.nodes_with_active_faults = 3; // 3 already, adding 1 = 4/6 = 67%
        let req = node_kill_request("node-5");
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
        assert!(matches!(
            check.violation,
            Some(SafetyViolation::NodePercentageExceeded { .. })
        ));
    }

    #[test]
    fn node_percentage_exceeded_with_override_allowed() {
        let mut ctx = default_context();
        ctx.nodes_with_active_faults = 3;
        let mut req = node_kill_request("node-5");
        req.override_safety = true;
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    #[test]
    fn node_percentage_at_half_is_allowed() {
        let mut ctx = default_context(); // 6 nodes
        // threshold = (6+1)/2 = 3, would_be = 3, 3 <= 3 → allowed
        ctx.nodes_with_active_faults = 2;
        let req = node_kill_request("node-5");
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
    }

    // -----------------------------------------------------------------------
    // Combined
    // -----------------------------------------------------------------------

    #[test]
    fn quorum_checked_before_leader() {
        // If both quorum and leader are violated, quorum should fire first
        let mut ctx = default_context();
        ctx.council_nodes_with_active_faults = 2;
        let req = node_kill_request("node-1"); // leader + quorum risk
        let check = evaluate_safety(&req, &ctx);
        assert!(!check.approved);
        assert!(matches!(
            check.violation,
            Some(SafetyViolation::QuorumRisk { .. })
        ));
    }

    #[test]
    fn all_rails_pass_for_safe_fault() {
        let ctx = default_context();
        let req = FaultRequest {
            fault_type: FaultType::Delay {
                delay_ns: 200_000_000,
                jitter_ns: 0,
            },
            target_service: "redis".into(),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(300),
            injected_by: "alice".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        };
        let check = evaluate_safety(&req, &ctx);
        assert!(check.approved);
        assert!(check.violation.is_none());
    }
}
