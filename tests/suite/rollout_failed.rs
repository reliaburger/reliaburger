//! Re-applying an app whose rollout failed (#929).
//!
//! The leader marks a spec `RolloutFailed` when a node's rollout of it
//! aborts (#692, D11-B). An identical re-apply keeps the mark and says so;
//! any change to the spec, a scale change included, clears it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, CouncilResponse, RaftRequest};
use reliaburger::meat::deploy_types::{RolloutFailure, rollout_fingerprint};
use reliaburger::meat::types::AppId;
use tokio::sync::mpsc;

const WEB_V2: &str = "[app.web]\nimage = 'web:v2'\nreplicas = 3\n";
const WEB_V2_SCALED: &str = "[app.web]\nimage = 'web:v2'\nreplicas = 5\n";

/// A single-node council, initialised so it becomes leader.
async fn single_node_leader() -> Arc<CouncilNode> {
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let node = CouncilNode::new(
        1,
        CouncilConfig {
            heartbeat_interval_ms: 50,
            election_timeout_min_ms: 150,
            election_timeout_max_ms: 400,
            snapshot_threshold: 100,
            max_in_snapshot_log_to_keep: 50,
        },
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    let mut members = BTreeMap::new();
    members.insert(
        1u64,
        CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1".to_string()),
    );
    node.initialize(members).await.unwrap();
    let node = Arc::new(node);
    for _ in 0..40 {
        if node.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node
}

/// The council's API, with no agent behind it.
fn api_router(council: &Arc<CouncilNode>) -> axum::Router {
    let (tx, _rx) = mpsc::channel(8);
    reliaburger::bun::api::router(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(Arc::clone(council)),
        None,
        None,
        None,
        None,
        None,
        0,
        None,
    )
}

async fn apply(router: &axum::Router, manifest: &str) -> String {
    use tower::ServiceExt;
    let request = axum::http::Request::post("/v1/apply")
        .body(axum::body::Body::from(manifest.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    assert!(status.is_success(), "{status}: {body}");
    assert!(body.contains("committed"), "the apply commits: {body}");
    body
}

/// Apply `web:v2` and mark its rollout failed, as the leader does when a
/// node reports an aborted rollout.
async fn failed_web(council: &CouncilNode, router: &axum::Router) -> (AppId, RolloutFailure) {
    apply(router, WEB_V2).await;
    let web = AppId::new("web", "default");
    let spec = council.desired_state().await.apps[&web].clone();
    let failure = RolloutFailure {
        fingerprint: rollout_fingerprint(&spec),
        node: "node-2".into(),
        reason: "web-g1-0 failed its health check".into(),
        at_unix_ms: 1,
    };
    let response = council
        .write(RaftRequest::RolloutFailed {
            app_id: web.clone(),
            failure: failure.clone(),
        })
        .await
        .unwrap();
    assert!(
        !matches!(response, CouncilResponse::Refused { .. }),
        "{response:?}"
    );
    (web, failure)
}

/// #929: an identical re-apply doesn't retry a failed rollout (D11-B), so
/// `relish apply` says so instead of reporting a plain commit.
#[tokio::test]
async fn identical_reapply_of_a_failed_rollout_says_it_is_not_retried() {
    let council = single_node_leader().await;
    let router = api_router(&council);
    let (web, failure) = failed_web(&council, &router).await;

    let body = apply(&router, WEB_V2).await;
    let rollouts = council.desired_state().await.rollouts[&web].clone();
    council.shutdown().await.ok();
    assert_eq!(rollouts.failed, Some(failure), "the mark stays");
    assert!(
        body.contains(
            "app 'web' in namespace 'default' is RolloutFailed on node-2 \
             (web-g1-0 failed its health check); the same spec again isn't retried"
        ),
        "{body}"
    );
}

/// #929: a scale change is a different spec. It clears the mark, so the
/// leader releases the spec node by node again, and `relish apply` has
/// nothing to warn about.
#[tokio::test]
async fn scale_change_reapply_clears_rollout_failed() {
    let council = single_node_leader().await;
    let router = api_router(&council);
    let (web, _) = failed_web(&council, &router).await;

    let body = apply(&router, WEB_V2_SCALED).await;
    let rollouts = council.desired_state().await.rollouts[&web].clone();
    council.shutdown().await.ok();
    assert_eq!(rollouts.failed, None, "a scale change clears the mark");
    assert!(!body.contains("RolloutFailed"), "{body}");
}
