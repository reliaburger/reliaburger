/// Integration tests for the Meat scheduler.
///
/// Exercises the scheduler with realistic multi-node cluster states.
/// Each test builds a ClusterStateCache, creates an AppSpec, and
/// verifies the resulting SchedulingDecision.
use std::collections::{BTreeMap, HashSet};

use reliaburger::config::app::{AppSpec, PlacementSpec};
use reliaburger::config::types::{Replicas, ResourceRange};
use reliaburger::meat::cluster_state::{ClusterStateCache, SchedulerNodeState};
use reliaburger::meat::quota::{NamespaceQuota, NamespaceUsage, QuotaError, check_quota};
use reliaburger::meat::scheduler::Scheduler;
use reliaburger::meat::types::{AppId, NodeId, Resources};

fn default_spec() -> AppSpec {
    toml::from_str(r#"image = "test:v1""#).unwrap()
}

fn node(name: &str, cpu: u64, mem: u64, labels: BTreeMap<String, String>) -> SchedulerNodeState {
    SchedulerNodeState {
        node_id: NodeId::new(name),
        allocatable: Resources::new(cpu, mem, 0),
        allocated: Resources::default(),
        labels,
        ready: true,
        running_apps: HashSet::new(),
    }
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn cluster_5_nodes() -> ClusterStateCache {
    let mut cluster = ClusterStateCache::new();
    for i in 1..=5 {
        let mut n = node(
            &format!("node-{i}"),
            4000,
            8 * 1024 * 1024 * 1024,
            BTreeMap::new(),
        );
        // Nodes near capacity — bin-packing can't differentiate, spread kicks in
        n.allocated = Resources::new(800, 0, 0);
        cluster.set_node(n);
    }
    cluster
}

/// Deploy 3 replicas across a 5-node cluster where nodes are near capacity.
/// When bin-packing can't differentiate (all ~80% full), spread scoring
/// forces replicas onto distinct nodes.
#[test]
fn scheduling_deploy_3_replicas_on_distinct_nodes() {
    let mut scheduler = Scheduler::new(cluster_5_nodes());

    let mut spec = default_spec();
    spec.replicas = Replicas::Fixed(3);
    spec.cpu = Some(ResourceRange {
        request: 100,
        limit: 200,
    });

    let app = AppId::new("web", "prod");
    let decision = scheduler.schedule_app(&app, &spec).unwrap();

    assert_eq!(decision.placements.len(), 3);

    let nodes: HashSet<_> = decision.placements.iter().map(|p| &p.node_id).collect();
    assert_eq!(nodes.len(), 3, "3 replicas should land on 3 distinct nodes");
}

/// Deploy a daemon app on a 5-node cluster. Verify placement on all 5.
#[test]
fn scheduling_deploy_daemon_on_all_nodes() {
    let mut scheduler = Scheduler::new(cluster_5_nodes());

    let mut spec = default_spec();
    spec.replicas = Replicas::DaemonSet;
    spec.cpu = Some(ResourceRange {
        request: 100,
        limit: 200,
    });

    let app = AppId::new("monitor", "system");
    let decision = scheduler.schedule_app(&app, &spec).unwrap();

    assert_eq!(
        decision.placements.len(),
        5,
        "daemon should place on all 5 nodes"
    );

    let nodes: HashSet<_> = decision.placements.iter().map(|p| &p.node_id).collect();
    assert_eq!(nodes.len(), 5);
}

/// Deploy with required labels. Only labelled nodes should receive placements.
#[test]
fn scheduling_deploy_with_required_labels() {
    let mut cluster = ClusterStateCache::new();
    cluster.set_node(node(
        "gpu-1",
        4000,
        8_000_000_000,
        labels(&[("gpu", "a100")]),
    ));
    cluster.set_node(node(
        "gpu-2",
        4000,
        8_000_000_000,
        labels(&[("gpu", "a100")]),
    ));
    cluster.set_node(node("cpu-1", 4000, 8_000_000_000, BTreeMap::new()));
    cluster.set_node(node("cpu-2", 4000, 8_000_000_000, BTreeMap::new()));
    cluster.set_node(node("cpu-3", 4000, 8_000_000_000, BTreeMap::new()));

    let mut scheduler = Scheduler::new(cluster);

    let mut spec = default_spec();
    spec.replicas = Replicas::Fixed(2);
    spec.cpu = Some(ResourceRange {
        request: 100,
        limit: 200,
    });
    spec.placement = Some(PlacementSpec {
        required: vec!["gpu=a100".to_string()],
        preferred: vec![],
    });

    let app = AppId::new("ml-train", "prod");
    let decision = scheduler.schedule_app(&app, &spec).unwrap();

    assert_eq!(decision.placements.len(), 2);
    for p in &decision.placements {
        assert!(
            p.node_id == NodeId::new("gpu-1") || p.node_id == NodeId::new("gpu-2"),
            "should only place on gpu nodes, got {:?}",
            p.node_id
        );
    }
}

/// Deploy with preferred labels. First placement prefers matching node,
/// second falls back to non-matching. Nodes are near capacity so
/// bin-packing scores are similar and the preferred label bonus decides.
#[test]
fn scheduling_deploy_with_preferred_labels_fallback() {
    let mut cluster = ClusterStateCache::new();
    // east-1 has just enough room for one replica (150m free)
    let mut east = node(
        "east-1",
        1000,
        8_000_000_000,
        labels(&[("zone", "us-east")]),
    );
    east.allocated = Resources::new(850, 0, 0);
    cluster.set_node(east);
    // west nodes have room for many replicas
    let mut west1 = node(
        "west-1",
        1000,
        8_000_000_000,
        labels(&[("zone", "us-west")]),
    );
    west1.allocated = Resources::new(500, 0, 0);
    cluster.set_node(west1);
    let mut west2 = node(
        "west-2",
        1000,
        8_000_000_000,
        labels(&[("zone", "us-west")]),
    );
    west2.allocated = Resources::new(500, 0, 0);
    cluster.set_node(west2);

    let mut scheduler = Scheduler::new(cluster);

    let mut spec = default_spec();
    spec.replicas = Replicas::Fixed(2);
    spec.cpu = Some(ResourceRange {
        request: 100,
        limit: 200,
    });
    spec.placement = Some(PlacementSpec {
        required: vec![],
        preferred: vec!["zone=us-east".to_string()],
    });

    let app = AppId::new("web", "prod");
    let decision = scheduler.schedule_app(&app, &spec).unwrap();

    assert_eq!(decision.placements.len(), 2);

    // First placement should prefer east-1 (has the preferred label)
    assert_eq!(
        decision.placements[0].node_id,
        NodeId::new("east-1"),
        "first replica should prefer the node with the preferred label"
    );

    // Second should fall back to a west node (soft constraint)
    assert!(
        decision.placements[1].node_id == NodeId::new("west-1")
            || decision.placements[1].node_id == NodeId::new("west-2"),
        "second replica should fall back to a non-preferred node"
    );
}

/// Namespace quota rejection: deploying beyond CPU quota fails.
#[test]
fn scheduling_namespace_quota_rejection() {
    let quota = NamespaceQuota {
        namespace: "prod".to_string(),
        max_cpu_millicores: Some(1000),
        max_memory_bytes: None,
        max_gpus: None,
        max_apps: Some(3),
        max_replicas: Some(5),
    };

    let usage = NamespaceUsage {
        cpu_millicores: 800,
        memory_bytes: 0,
        gpus: 0,
        app_count: 2,
        replica_count: 4,
    };

    // Requesting 300m more CPU (800 + 300 = 1100 > 1000)
    let requested = Resources::new(300, 1024, 0);
    let result = check_quota(&quota, &usage, &requested, 1, false);
    assert!(
        matches!(result, Err(QuotaError::CpuExceeded { .. })),
        "should reject: {result:?}"
    );

    // Requesting 100m (800 + 100 = 900 < 1000) — should pass
    let requested_ok = Resources::new(100, 1024, 0);
    let result_ok = check_quota(&quota, &usage, &requested_ok, 1, false);
    assert!(result_ok.is_ok(), "should allow: {result_ok:?}");

    // Adding a 4th app when max_apps=3 and already at 3
    let usage_at_limit = NamespaceUsage {
        app_count: 3,
        ..Default::default()
    };
    let result_apps = check_quota(&quota, &usage_at_limit, &Resources::default(), 1, true);
    assert!(
        matches!(result_apps, Err(QuotaError::MaxAppsExceeded { .. })),
        "should reject new app: {result_apps:?}"
    );
}
