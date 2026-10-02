//! Desired-app diagnostics come from the current leader on every node (#436).

use reliaburger::config::{Replicas, app::AppSpec};
use reliaburger::council::types::RaftRequest;
use reliaburger::meat::types::{AppId, NodeId};
use reliaburger::{bun, config, council, mustard};
use std::collections::BTreeMap;
use std::time::Duration;

fn app_spec(cpu_request: u64, replicas: u32) -> AppSpec {
    let mut spec: AppSpec = toml::from_str(r#"image = "x:1""#).unwrap();
    spec.replicas = Replicas::Fixed(replicas);
    spec.cpu = Some(config::types::ResourceRange {
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

fn directory_naming(
    leader_url: &str,
) -> tokio::sync::watch::Receiver<mustard::directory::NodeDirectory> {
    let leader_address = leader_url.trim_start_matches("http://").parse().unwrap();
    let (send, directory) = tokio::sync::watch::channel(mustard::directory::NodeDirectory {
        leader: Some(mustard::message::LeaderHint {
            node_id: NodeId::new("leader"),
            term: 1,
            api_address: leader_address,
            reporting_address: leader_address,
        }),
        ..Default::default()
    });
    // The receiver only reads; keeping the sender alive isn't needed.
    drop(send);
    directory
}

/// A worker outside Raft has an empty state machine. It must answer with the
/// leader's desired apps, not with its own empty view (#436).
#[tokio::test]
async fn worker_desired_apps_come_from_the_leader() {
    let network = council::network::InMemoryRaftRouter::new();
    let leader = initialized_leader(&network).await;
    let worker = memory_council(2, &network).await;
    let (leader_url, leader_server, _) = api_for(leader.clone(), None).await;
    let (worker_url, worker_server, _) =
        api_for(worker.clone(), Some(directory_naming(&leader_url))).await;
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
    assert_eq!(
        expected.as_array().unwrap().len(),
        1,
        "leader control must expose the committed app"
    );
    assert_eq!(code, reqwest::StatusCode::OK, "worker answered {actual}");
    assert_eq!(actual, expected);
}

/// With no leader to ask, a worker says so instead of claiming the cluster
/// runs nothing.
#[tokio::test]
async fn worker_without_a_known_leader_reports_unavailable() {
    let network = council::network::InMemoryRaftRouter::new();
    let worker = memory_council(2, &network).await;
    let (worker_url, worker_server, _) = api_for(worker.clone(), None).await;
    let response = reqwest::Client::new()
        .get(format!("{worker_url}/v1/diagnostics/apps"))
        .send()
        .await
        .unwrap();
    let code = response.status();
    worker_server.abort();
    worker.shutdown().await.unwrap();
    assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE);
}

/// A read another node already forwarded is never forwarded again, so two
/// nodes that each think the other leads can't bounce it between them.
#[tokio::test]
async fn forwarded_desired_apps_read_is_not_forwarded_twice() {
    let network = council::network::InMemoryRaftRouter::new();
    let leader = initialized_leader(&network).await;
    let worker = memory_council(2, &network).await;
    let (leader_url, leader_server, _) = api_for(leader.clone(), None).await;
    let (worker_url, worker_server, _) =
        api_for(worker.clone(), Some(directory_naming(&leader_url))).await;
    let response = reqwest::Client::new()
        .get(format!("{worker_url}/v1/diagnostics/apps"))
        .header("x-reliaburger-desired-apps-forwarded", "1")
        .send()
        .await
        .unwrap();
    let code = response.status();
    leader_server.abort();
    worker_server.abort();
    leader.shutdown().await.unwrap();
    worker.shutdown().await.unwrap();
    assert_eq!(code, reqwest::StatusCode::SERVICE_UNAVAILABLE);
}
