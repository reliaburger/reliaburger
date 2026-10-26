//! Re-applying an app whose rollout failed (#929).
//!
//! The leader marks a spec `RolloutFailed` when a node's rollout of it
//! aborts (#692, D11-B). An identical re-apply keeps the mark and says so;
//! any change to the spec, a scale change included, clears it.
//!
//! On a credential-free plaintext cluster a node has no service token, so
//! its report arrives unauthenticated. The leader still takes it, but only
//! for a node the app is placed on (#1067).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, CouncilResponse, RaftRequest};
use reliaburger::meat::deploy_types::{RolloutFailure, rollout_fingerprint};
use reliaburger::meat::types::{AppId, NodeId, Placement, Resources, SchedulingDecision};
use tokio::sync::mpsc;

use crate::admission_fixture::write_admission_fixture;

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

/// The council's API, with no agent behind it: a plaintext cluster with
/// no credentials at all.
fn api_router(council: &Arc<CouncilNode>) -> axum::Router {
    api_router_with_service_token(council, None)
}

/// The council's API, holding `service_token` when the cluster has one.
fn api_router_with_service_token(
    council: &Arc<CouncilNode>,
    service_token: Option<String>,
) -> axum::Router {
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
        service_token,
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

/// Place `web` on `node`, as the scheduler does after an apply.
async fn place_web_on(council: &CouncilNode, node: &str) {
    let response = write_admission_fixture(
        council,
        RaftRequest::SchedulingDecision(SchedulingDecision {
            app_id: AppId::new("web", "default"),
            placements: vec![Placement {
                node_id: NodeId::new(node),
                resources: Resources::new(100, 64 * 1024 * 1024, 0),
                ordinal: 0,
            }],
        }),
    )
    .await
    .unwrap();
    assert!(
        !matches!(response, CouncilResponse::Refused { .. }),
        "{response:?}"
    );
}

/// POST a rollout-failure report for the current `web` spec from `node`,
/// with no credentials, as a node without a service token sends it.
async fn report_without_credentials(
    council: &CouncilNode,
    router: &axum::Router,
    node: &str,
) -> axum::http::StatusCode {
    use tower::ServiceExt;
    let web = AppId::new("web", "default");
    let spec = council.desired_state().await.apps[&web].clone();
    let report = serde_json::json!({
        "app_name": "web",
        "namespace": "default",
        "node": node,
        "fingerprint": rollout_fingerprint(&spec),
        "reason": "web-0 failed its health check",
    });
    let request = axum::http::Request::post("/v1/rollouts/failed")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(report.to_string()))
        .unwrap();
    router.clone().oneshot(request).await.unwrap().status()
}

/// Whether `web` carries no `RolloutFailed` mark.
async fn web_is_unmarked(council: &CouncilNode) -> bool {
    council
        .desired_state()
        .await
        .rollouts
        .get(&AppId::new("web", "default"))
        .is_none_or(|rollouts| rollouts.failed.is_none())
}

/// #1067: a plaintext cluster with no credentials has no service token to
/// sign a node's report with. The leader takes the report anyway for a
/// node the app is placed on, so the bad version reaches `RolloutFailed`
/// and the node stops retrying it.
#[tokio::test]
async fn credential_free_cluster_takes_a_placed_nodes_rollout_failure() {
    let council = single_node_leader().await;
    let router = api_router(&council);
    apply(&router, WEB_V2).await;
    place_web_on(&council, "node-2").await;

    let status = report_without_credentials(&council, &router, "node-2").await;
    let rollouts = council.desired_state().await.rollouts;
    council.shutdown().await.ok();
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
    let failed = rollouts[&AppId::new("web", "default")].failed.clone();
    assert_eq!(failed.map(|failure| failure.node), Some("node-2".into()));
}

/// #1067: without credentials a caller can report only a node the app is
/// placed on. A report naming any other node is refused and marks nothing.
#[tokio::test]
async fn credential_free_cluster_refuses_a_report_for_a_node_without_the_app() {
    let council = single_node_leader().await;
    let router = api_router(&council);
    apply(&router, WEB_V2).await;
    place_web_on(&council, "node-2").await;

    let status = report_without_credentials(&council, &router, "node-9").await;
    let unmarked = web_is_unmarked(&council).await;
    council.shutdown().await.ok();
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert!(unmarked, "nothing is marked");
}

/// #1067: a cluster with a service token keeps requiring it. An
/// unauthenticated report is refused even for a node the app is placed on.
#[tokio::test]
async fn cluster_with_a_service_token_refuses_an_unauthenticated_rollout_failure() {
    let council = single_node_leader().await;
    let router = api_router_with_service_token(&council, Some("rbrg_service".into()));
    apply(&router, WEB_V2).await;
    place_web_on(&council, "node-2").await;

    let status = report_without_credentials(&council, &router, "node-2").await;
    let unmarked = web_is_unmarked(&council).await;
    council.shutdown().await.ok();
    assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
    assert!(unmarked, "nothing is marked");
}
