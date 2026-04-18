/// Integration tests for state reconstruction.
///
/// Verifies the end-to-end flow: after a leader election, the
/// reconstruction controller collects reports, diffs desired vs actual
/// state, and produces correct corrections.
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, SystemTime};

use reliaburger::config::node::ReconstructionSection;
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};
use reliaburger::meat::types::{AppId, NodeId, Placement, Resources, SchedulingDecision};
use reliaburger::reconstruction::controller::ReconstructionController;
use reliaburger::reconstruction::types::{Correction, LearningOutcome, ReconstructionPhase};
use reliaburger::reporting::aggregator::AggregatedState;
use reliaburger::reporting::types::{
    AppResourceUsage, ReportHealthStatus, ResourceUsage, RunningApp, StateReport,
};

fn fast_council_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 200,
        election_timeout_max_ms: 400,
        snapshot_threshold: 100,
        max_in_snapshot_log_to_keep: 50,
    }
}

fn node_info(id: u64) -> CouncilNodeInfo {
    CouncilNodeInfo::new(
        format!("127.0.0.1:{}", 9000 + id).parse().unwrap(),
        format!("node-{id}"),
    )
}

fn recon_config() -> ReconstructionSection {
    ReconstructionSection {
        report_threshold_percent: 95,
        learning_period_timeout_secs: 15,
        ..Default::default()
    }
}

async fn create_cluster(n: u64) -> (Vec<CouncilNode>, InMemoryRaftRouter) {
    let router = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();

    for id in 1..=n {
        let network = InMemoryRaftNetworkFactory::new(id, router.clone());
        let node = CouncilNode::new(
            id,
            fast_council_config(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        router.register(id, node.raft().clone()).await;
        nodes.push(node);
    }
    (nodes, router)
}

async fn init_cluster(nodes: &[CouncilNode]) {
    let mut members = BTreeMap::new();
    for (i, _) in nodes.iter().enumerate() {
        let id = (i + 1) as u64;
        members.insert(id, node_info(id));
    }
    nodes[0].initialize(members).await.unwrap();
}

async fn wait_for_leader(nodes: &[CouncilNode], timeout: Duration) -> Option<u64> {
    let start = tokio::time::Instant::now();
    loop {
        for node in nodes {
            if let Some(leader) = node.current_leader().await {
                return Some(leader);
            }
        }
        if start.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn build_aggregated(entries: Vec<(NodeId, Vec<(&str, &str)>)>) -> AggregatedState {
    let mut reports = HashMap::new();
    for (node_id, apps) in entries {
        let running_apps = apps
            .into_iter()
            .map(|(name, ns)| RunningApp {
                app_name: name.to_string(),
                namespace: ns.to_string(),
                instance_id: 0,
                image: String::new(),
                port: None,
                health_status: ReportHealthStatus::Healthy,
                uptime: Duration::from_secs(60),
                resource_usage: AppResourceUsage::default(),
            })
            .collect();
        reports.insert(
            node_id.clone(),
            StateReport {
                node_id,
                timestamp: SystemTime::now(),
                running_apps,
                cached_specs: vec![],
                resource_usage: ResourceUsage::default(),
                event_log: vec![],
            },
        );
    }
    AggregatedState {
        reports,
        stale_nodes: vec![],
    }
}

/// Matching state: desired == actual, no corrections.
#[tokio::test]
async fn reconstruction_matching_state_no_corrections() {
    let (nodes, _router) = create_cluster(3).await;
    init_cluster(&nodes).await;

    let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
        .await
        .expect("leader should be elected");
    let leader = &nodes[(leader_id - 1) as usize];

    // Write scheduling decisions to Raft.
    leader
        .write(RaftRequest::SchedulingDecision(SchedulingDecision {
            app_id: AppId::new("web", "prod"),
            placements: vec![Placement {
                node_id: NodeId::new("worker-1"),
                resources: Resources::new(100, 128 * 1024 * 1024, 0),
            }],
        }))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Read desired state from the new leader.
    let desired = leader.desired_state().await;
    assert!(desired.scheduling.contains_key(&AppId::new("web", "prod")));

    // Build aggregated state that matches.
    let aggregated = build_aggregated(vec![
        (NodeId::new("worker-1"), vec![("web", "prod")]),
        (NodeId::new("worker-2"), vec![]),
    ]);

    // Run reconstruction.
    let alive = [NodeId::new("worker-1"), NodeId::new("worker-2")];
    let mut ctrl = ReconstructionController::new(recon_config());
    ctrl.on_leader_elected(2);

    let result = ctrl
        .on_report_received(&aggregated, &desired, &alive)
        .expect("100% coverage should trigger threshold");

    assert_eq!(ctrl.phase(), ReconstructionPhase::Active);
    assert!(matches!(
        result.outcome,
        LearningOutcome::ThresholdMet { .. }
    ));

    // No corrections because desired matches actual.
    let non_unknown: Vec<_> = result
        .corrections
        .iter()
        .filter(|c| !matches!(c, Correction::UnknownNode { .. }))
        .collect();
    assert!(
        non_unknown.is_empty(),
        "no missing/extra corrections expected, got: {non_unknown:?}"
    );
}

/// Mismatched state: desired has app but it's not running.
#[tokio::test]
async fn reconstruction_missing_app_detected() {
    let (nodes, _router) = create_cluster(3).await;
    init_cluster(&nodes).await;

    let leader_id = wait_for_leader(&nodes, Duration::from_secs(5))
        .await
        .expect("leader should be elected");
    let leader = &nodes[(leader_id - 1) as usize];

    // Desired: web/prod on worker-1, api/prod on worker-2.
    leader
        .write(RaftRequest::SchedulingDecision(SchedulingDecision {
            app_id: AppId::new("web", "prod"),
            placements: vec![Placement {
                node_id: NodeId::new("worker-1"),
                resources: Resources::new(100, 128 * 1024 * 1024, 0),
            }],
        }))
        .await
        .unwrap();
    leader
        .write(RaftRequest::SchedulingDecision(SchedulingDecision {
            app_id: AppId::new("api", "prod"),
            placements: vec![Placement {
                node_id: NodeId::new("worker-2"),
                resources: Resources::new(200, 256 * 1024 * 1024, 0),
            }],
        }))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;
    let desired = leader.desired_state().await;

    // Actual: web/prod running on worker-1, but api/prod NOT running on worker-2.
    // Worker-3 didn't report at all.
    let aggregated = build_aggregated(vec![
        (NodeId::new("worker-1"), vec![("web", "prod")]),
        (NodeId::new("worker-2"), vec![]), // api/prod missing
    ]);

    let alive = [
        NodeId::new("worker-1"),
        NodeId::new("worker-2"),
        NodeId::new("worker-3"),
    ];

    let mut ctrl = ReconstructionController::new(ReconstructionSection {
        report_threshold_percent: 60, // 2/3 = 66% > 60%
        ..recon_config()
    });
    ctrl.on_leader_elected(3);

    let result = ctrl
        .on_report_received(&aggregated, &desired, &alive)
        .expect("66% coverage should trigger 60% threshold");

    // Should have MissingApp for api/prod on worker-2.
    let missing: Vec<_> = result
        .corrections
        .iter()
        .filter(|c| matches!(c, Correction::MissingApp { .. }))
        .collect();
    assert_eq!(missing.len(), 1, "expected 1 MissingApp correction");
    assert!(matches!(
        &missing[0],
        Correction::MissingApp { app_id, node_id }
        if app_id.name == "api" && node_id.0 == "worker-2"
    ));

    // Should have UnknownNode for worker-3.
    assert!(
        result.unknown_nodes.iter().any(|n| n.0 == "worker-3"),
        "worker-3 should be unknown"
    );
}
