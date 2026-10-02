use reliaburger::config::Replicas;
use reliaburger::config::app::AppSpec;
use reliaburger::council::types::{DesiredState, RaftRequest};
use reliaburger::meat::cluster_state::{ClusterStateCache, SchedulerNodeState};
use reliaburger::meat::quota::QuotaLedger;
use reliaburger::meat::types::{AppId, NodeId, Placement, Resources};
use reliaburger::mustard::membership::MembershipSnapshot;
use reliaburger::mustard::state::NodeState;
use reliaburger::reporting::aggregator::AggregatedState;
use reliaburger::reporting::types::*;
use reliaburger::{bun, cluster, config, council, meat, mustard};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};
fn app_spec(cpu_request: u64, replicas: u32) -> AppSpec {
    let mut spec: AppSpec = toml::from_str(r#"image = "x:1""#).unwrap();
    spec.replicas = Replicas::Fixed(replicas);
    spec.cpu = Some(crate::config::types::ResourceRange {
        request: cpu_request,
        limit: cpu_request,
    });
    spec
}

fn fast_raft_config() -> council::types::CouncilConfig {
    council::types::CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 200,
        election_timeout_max_ms: 400,
        snapshot_threshold: 100,
        max_in_snapshot_log_to_keep: 50,
    }
}

async fn memory_council(
    id: u64,
    router: &council::network::InMemoryRaftRouter,
) -> std::sync::Arc<council::CouncilNode> {
    let node = std::sync::Arc::new(
        council::CouncilNode::new(
            id,
            fast_raft_config(),
            council::network::InMemoryRaftNetworkFactory::new(id, router.clone()),
            council::log_store::MemLogStore::new(),
            council::state_machine::CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap(),
    );
    router.register(id, node.raft().clone()).await;
    node
}

async fn api_for(
    node: std::sync::Arc<council::CouncilNode>,
    directory: Option<tokio::sync::watch::Receiver<mustard::directory::NodeDirectory>>,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Receiver<bun::agent::AgentCommand>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut router = bun::api::router(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(node),
        None,
        None,
        None,
        None,
        None,
        addr.port(),
        None,
    );
    if let Some(directory) = directory {
        router = router.layer(axum::Extension(bun::api::LeaderDirectory(directory)));
    }
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), server, rx)
}

async fn initialized_leader(
    router: &council::network::InMemoryRaftRouter,
) -> std::sync::Arc<council::CouncilNode> {
    let leader = memory_council(1, router).await;
    leader
        .initialize(BTreeMap::from([(
            1,
            council::types::CouncilNodeInfo::new("127.0.0.1:9000".parse().unwrap(), "leader"),
        )]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !leader.is_leader().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    leader
        .write(RaftRequest::AppSpec {
            app_id: AppId::new("web", "default"),
            spec: Box::new(app_spec(100, 1)),
        })
        .await
        .unwrap();
    leader
}
#[tokio::test]
async fn worker_desired_apps_must_not_report_an_empty_cluster() {
    let network = council::network::InMemoryRaftRouter::new();
    let leader = initialized_leader(&network).await;
    let worker = memory_council(2, &network).await;
    let (leader_url, leader_server, _) = api_for(leader.clone(), None).await;
    let leader_address = leader_url.trim_start_matches("http://").parse().unwrap();
    let (_send, directory) = tokio::sync::watch::channel(mustard::directory::NodeDirectory {
        leader: Some(mustard::message::LeaderHint {
            node_id: NodeId::new("leader"),
            term: 1,
            api_address: leader_address,
            reporting_address: leader_address,
        }),
        ..Default::default()
    });
    let (worker_url, worker_server, _) = api_for(worker.clone(), Some(directory)).await;
    let client = reqwest::Client::new();
    let expected: serde_json::Value = client
        .get(format!("{leader_url}/v1/diagnostics/apps"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = client
        .get(format!("{worker_url}/v1/diagnostics/apps"))
        .send()
        .await
        .unwrap();
    let code = response.status();
    let actual: serde_json::Value = response.json().await.unwrap();
    leader_server.abort();
    worker_server.abort();
    leader.shutdown().await.unwrap();
    worker.shutdown().await.unwrap();
    eprintln!("leader={expected}; worker={code} {actual}");
    assert!(code == reqwest::StatusCode::SERVICE_UNAVAILABLE || (code.is_success() && actual == expected), "worker must return authoritative state or explicit unavailability; got {code} {actual}");
}
