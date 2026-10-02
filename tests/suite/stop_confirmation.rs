//! A clustered stop or delete answers as an accepted request (#435): the
//! council has recorded it, but no worker has confirmed the cleanup yet.

use reliaburger::config::{Replicas, app::AppSpec};
use reliaburger::council::types::RaftRequest;
use reliaburger::meat::types::{AppId, NodeId, Placement, Resources};
use reliaburger::{bun, config, council, meat, mustard};
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
#[tokio::test]
async fn stop_must_not_claim_cleanup_before_any_worker_has_seen_it() {
    let network = council::network::InMemoryRaftRouter::new();
    let leader = initialized_leader(&network).await;
    leader
        .write(RaftRequest::SchedulingDecision(
            meat::types::SchedulingDecision {
                app_id: AppId::new("web", "default"),
                placements: vec![Placement {
                    node_id: NodeId::new("unreachable-worker"),
                    resources: Resources::default(),
                }],
            },
        ))
        .await
        .unwrap();
    let (url, server, mut commands) = api_for(leader.clone(), None).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/stop/web/default"))
        .send()
        .await
        .unwrap();
    let code = response.status();
    let body = response.text().await.unwrap();
    let scheduling = leader.desired_state().await.scheduling;
    let dispatched = commands.try_recv().is_ok();
    server.abort();
    leader.shutdown().await.unwrap();
    eprintln!(
        "stop response={code} {body}; placements={scheduling:?}; command_dispatched={dispatched}"
    );
    assert_eq!(code, reqwest::StatusCode::ACCEPTED);
    assert!(
        !body.contains("\"stopped\""),
        "the operation reports stopped while the only worker has received no stop instruction"
    );
}

#[tokio::test]
async fn delete_reports_an_accepted_request_not_completed_cleanup() {
    let network = council::network::InMemoryRaftRouter::new();
    let leader = initialized_leader(&network).await;
    let (url, server, _commands) = api_for(leader.clone(), None).await;
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/delete/web/default"))
        .send()
        .await
        .unwrap();
    let code = response.status();
    let body: serde_json::Value = response.json().await.unwrap();
    server.abort();
    leader.shutdown().await.unwrap();
    assert_eq!(code, reqwest::StatusCode::ACCEPTED);
    assert_eq!(body["status"], "deleting");
}
