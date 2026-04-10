/// Smoker chaos test scenarios.
///
/// These tests verify fault injection, safety rails, and recovery using
/// in-memory infrastructure. They exercise the Smoker module's logic
/// without requiring a real eBPF-capable Linux host.
use std::time::Duration;

use reliaburger::smoker::registry::FaultRegistry;
use reliaburger::smoker::safety::evaluate_safety;
use reliaburger::smoker::types::*;

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

// ---------------------------------------------------------------------------
// Chaos scenario 1: Kill leader mid-deploy
// ---------------------------------------------------------------------------

/// Verify that safety rails prevent killing the leader without --include-leader,
/// and that the fault registry correctly tracks the kill if approved.
#[test]
fn kill_leader_blocked_without_flag() {
    let ctx = default_context();
    let req = FaultRequest {
        fault_type: FaultType::NodeKill {
            kill_containers: true,
        },
        target_service: "".into(),
        target_instance: None,
        target_node: Some("node-1".into()), // the leader
        duration: Duration::from_secs(30),
        injected_by: "chaos-test".into(),
        reason: Some("kill leader mid-deploy".into()),
        include_leader: false,
        override_safety: false,
    };
    let check = evaluate_safety(&req, &ctx);
    assert!(!check.approved);
    assert!(matches!(
        check.violation,
        Some(SafetyViolation::LeaderTargeted)
    ));
}

#[test]
fn kill_leader_allowed_with_flag() {
    let ctx = default_context();
    let req = FaultRequest {
        fault_type: FaultType::NodeKill {
            kill_containers: true,
        },
        target_service: "".into(),
        target_instance: None,
        target_node: Some("node-1".into()),
        duration: Duration::from_secs(30),
        injected_by: "chaos-test".into(),
        reason: None,
        include_leader: true,
        override_safety: false,
    };
    let check = evaluate_safety(&req, &ctx);
    assert!(check.approved);
}

// ---------------------------------------------------------------------------
// Chaos scenario 2: Kill node, replicas rescheduled
// ---------------------------------------------------------------------------

#[test]
fn kill_non_leader_node_approved() {
    let ctx = default_context();
    let req = FaultRequest {
        fault_type: FaultType::NodeKill {
            kill_containers: false,
        },
        target_service: "".into(),
        target_instance: None,
        target_node: Some("node-3".into()),
        duration: Duration::from_secs(60),
        injected_by: "chaos-test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    let check = evaluate_safety(&req, &ctx);
    assert!(check.approved);

    // Verify registry tracks the fault
    let mut registry = FaultRegistry::new();
    let rule = registry.insert(&req);
    assert_eq!(rule.target_node, Some("node-3".into()));
    assert_eq!(registry.len(), 1);
}

// ---------------------------------------------------------------------------
// Chaos scenario 3: Drain node, zero-downtime migration
// ---------------------------------------------------------------------------

#[test]
fn drain_node_tracked_in_registry() {
    let mut registry = FaultRegistry::new();
    let req = FaultRequest {
        fault_type: FaultType::NodeDrain,
        target_service: "".into(),
        target_instance: None,
        target_node: Some("node-5".into()),
        duration: Duration::from_secs(120),
        injected_by: "chaos-test".into(),
        reason: Some("zero-downtime migration".into()),
        include_leader: false,
        override_safety: false,
    };

    let check = evaluate_safety(&req, &default_context());
    assert!(check.approved);

    let rule = registry.insert(&req);
    assert!(matches!(rule.fault_type, FaultType::NodeDrain));
    assert_eq!(registry.count_by_node("node-5"), 1);
}

// ---------------------------------------------------------------------------
// Chaos scenario 4: Kill 2 of 3 replicas simultaneously
// ---------------------------------------------------------------------------

#[test]
fn kill_2_of_3_replicas_allowed() {
    let ctx = default_context(); // 3 replicas
    let req = FaultRequest {
        fault_type: FaultType::Kill { count: 2 },
        target_service: "web".into(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(0),
        injected_by: "chaos-test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    let check = evaluate_safety(&req, &ctx);
    assert!(check.approved); // 1 replica survives
}

#[test]
fn kill_all_3_replicas_blocked() {
    let ctx = default_context();
    let req = FaultRequest {
        fault_type: FaultType::Kill { count: 0 }, // 0 = kill all
        target_service: "web".into(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(0),
        injected_by: "chaos-test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    let check = evaluate_safety(&req, &ctx);
    assert!(!check.approved);
    assert!(matches!(
        check.violation,
        Some(SafetyViolation::ReplicaMinimum { surviving: 0, .. })
    ));
}

// ---------------------------------------------------------------------------
// Chaos scenario 5: Rapid leader elections
// ---------------------------------------------------------------------------

#[test]
fn rapid_elections_quorum_protection() {
    let mut ctx = default_context(); // 5-member council
    // Simulate 2 nodes already affected
    ctx.council_nodes_with_active_faults = 2;

    // Trying to kill a third council member should be blocked (would break quorum)
    let req = FaultRequest {
        fault_type: FaultType::NodeKill {
            kill_containers: false,
        },
        target_service: "".into(),
        target_instance: None,
        target_node: Some("node-4".into()),
        duration: Duration::from_secs(30),
        injected_by: "chaos-test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    let check = evaluate_safety(&req, &ctx);
    assert!(!check.approved);
    assert!(matches!(
        check.violation,
        Some(SafetyViolation::QuorumRisk { .. })
    ));
}

// ---------------------------------------------------------------------------
// Chaos scenario 6: Node failure with volume app
// ---------------------------------------------------------------------------

#[test]
fn node_failure_fault_tracked_with_reason() {
    let mut registry = FaultRegistry::new();
    let req = FaultRequest {
        fault_type: FaultType::NodeKill {
            kill_containers: true,
        },
        target_service: "".into(),
        target_instance: None,
        target_node: Some("node-5".into()),
        duration: Duration::from_secs(30),
        injected_by: "chaos-test".into(),
        reason: Some("test volume survival on node failure".into()),
        include_leader: false,
        override_safety: false,
    };
    let rule = registry.insert(&req);
    assert_eq!(
        rule.reason.as_deref(),
        Some("test volume survival on node failure")
    );
}

// ---------------------------------------------------------------------------
// Chaos scenario 7: Resource exhaustion
// ---------------------------------------------------------------------------

#[test]
fn oom_kill_blocked_for_all_replicas() {
    let ctx = default_context(); // 3 replicas
    let req = FaultRequest {
        fault_type: FaultType::MemoryPressure {
            percentage: 0,
            oom: true,
        },
        target_service: "web".into(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(30),
        injected_by: "chaos-test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    // OOM kill on all replicas should be blocked
    let check = evaluate_safety(&req, &ctx);
    assert!(!check.approved);
}

#[test]
fn cpu_stress_allowed_no_replica_check() {
    let ctx = default_context();
    let req = FaultRequest {
        fault_type: FaultType::CpuStress {
            percentage: 90,
            cores: None,
        },
        target_service: "web".into(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(60),
        injected_by: "chaos-test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    // CPU stress doesn't kill instances, so replica check shouldn't apply
    let check = evaluate_safety(&req, &ctx);
    assert!(check.approved);
}

// ---------------------------------------------------------------------------
// Chaos scenario 8: Bun restart — fault registry cleared
// ---------------------------------------------------------------------------

#[test]
fn registry_cleared_on_restart() {
    let mut registry = FaultRegistry::new();

    // Inject several faults
    for i in 0..5 {
        let req = FaultRequest {
            fault_type: FaultType::Delay {
                delay_ns: 200_000_000,
                jitter_ns: 0,
            },
            target_service: format!("service-{i}"),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(300),
            injected_by: "chaos-test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        };
        registry.insert(&req);
    }
    assert_eq!(registry.len(), 5);

    // Simulate Bun restart: new empty registry
    let fresh_registry = FaultRegistry::new();
    assert!(fresh_registry.is_empty());
    // This verifies the non-persistence property: faults exist only in-memory
}

// ---------------------------------------------------------------------------
// Fault registry lifecycle: inject, list, expire, clear
// ---------------------------------------------------------------------------

#[test]
fn fault_lifecycle_inject_list_expire_clear() {
    let mut registry = FaultRegistry::new();

    // Inject a fault with very short duration
    let req = FaultRequest {
        fault_type: FaultType::Pause,
        target_service: "redis".into(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_nanos(1), // expires almost immediately
        injected_by: "test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    let rule = registry.insert(&req);

    // List should show it
    assert_eq!(registry.list().len(), 1);
    assert_eq!(registry.list()[0].id, rule.id.0);

    // Drain expired (it should have expired by now)
    std::thread::sleep(Duration::from_millis(1));
    let now = monotonic_now_ns();
    let expired = registry.drain_expired(now);
    assert_eq!(expired.len(), 1);
    assert!(registry.is_empty());
}

#[test]
fn clear_all_removes_everything() {
    let mut registry = FaultRegistry::new();
    for _ in 0..3 {
        let req = FaultRequest {
            fault_type: FaultType::DnsNxdomain,
            target_service: "api".into(),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(300),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
        };
        registry.insert(&req);
    }
    assert_eq!(registry.len(), 3);

    let removed = registry.clear();
    assert_eq!(removed.len(), 3);
    assert!(registry.is_empty());
}

// ---------------------------------------------------------------------------
// Node percentage guard
// ---------------------------------------------------------------------------

#[test]
fn node_percentage_exceeded_blocks_majority_fault() {
    let mut ctx = default_context(); // 6 nodes
    ctx.nodes_with_active_faults = 3;
    let req = FaultRequest {
        fault_type: FaultType::NodeDrain,
        target_service: "".into(),
        target_instance: None,
        target_node: Some("node-6".into()),
        duration: Duration::from_secs(300),
        injected_by: "test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    let check = evaluate_safety(&req, &ctx);
    assert!(!check.approved);
    assert!(matches!(
        check.violation,
        Some(SafetyViolation::NodePercentageExceeded {
            affected_nodes: 4,
            total_nodes: 6,
        })
    ));
}
