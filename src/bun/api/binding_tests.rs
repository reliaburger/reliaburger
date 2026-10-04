//! Route-level tests for binding images to digests at apply (F03 U1, #361).
//!
//! The binder is attached as a layer, as `bun` attaches it when its own
//! runtime pulls images. The registry is a fixed answer, so every test runs
//! without a network.

use std::time::{Duration, SystemTime};

use super::tests::seeded_council;
use super::*;
use crate::bun::agent::BunAgent;
use crate::grill::mock::MockGrill;
use crate::grill::port::PortAllocator;
use crate::meat::deploy_types::{DeployId, DeployResult};
use crate::meat::types::AppId;
use crate::pickle::binding::{FixedUpstream, ImageBinder};
use crate::pickle::types::Digest;
use axum::body::Body;
use http_body_util::BodyExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

fn digest(i: u64) -> Digest {
    Digest::new(&format!("sha256:{i:064x}")).unwrap()
}

/// A binder whose registry answers every tag with `digest(i)`, or is down.
fn binder(answer: Option<u64>) -> ImageBinder {
    ImageBinder::with_upstream(Arc::new(FixedUpstream(answer.map(digest))))
}

/// A router over a live standalone agent (a `MockGrill`), with `council`
/// and `history` when given and the binder layered on when given.
fn router_with(
    council: Option<Arc<crate::council::CouncilNode>>,
    history: Option<Arc<RwLock<Vec<DeployHistoryEntry>>>>,
    binder: Option<ImageBinder>,
) -> (Router, CancellationToken) {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        MockGrill::new(),
        PortAllocator::new(30000, 31000),
        cmd_rx,
        shutdown.clone(),
    );
    tokio::spawn(async move { agent.run().await });
    let app = router(
        cmd_tx, None, None, history, None, None, council, None, None, None, None, None, 9117, None,
    );
    let app = match binder {
        Some(binder) => app.layer(axum::Extension(binder)),
        None => app,
    };
    (app, shutdown)
}

async fn post(app: Router, uri: &str, body: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            axum::http::Request::post(uri)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn desired_image(council: &crate::council::CouncilNode, app: &str) -> Option<String> {
    council
        .desired_state()
        .await
        .apps
        .get(&AppId::new(app, "default"))
        .and_then(|spec| spec.image.clone())
}

#[tokio::test]
async fn a_cluster_apply_stores_the_bound_reference_and_prints_the_binding() {
    let council = seeded_council("bind-cluster-apply").await;
    let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder(Some(7))));

    let (status, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("web: nginx:1.27 → sha256:"), "{body}");
    assert_eq!(
        desired_image(&council, "web").await,
        Some(format!("nginx:1.27@{}", digest(7).as_str()))
    );
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// Decision 2: with the registry down and nothing cached, the apply fails
/// and Raft keeps no unbound tag.
#[tokio::test]
async fn a_cluster_apply_fails_and_writes_nothing_when_the_registry_is_down() {
    let council = seeded_council("bind-registry-down").await;
    let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder(None)));

    let (_, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert!(body.contains("\"Error\""), "{body}");
    assert!(body.contains("nginx:1.27"), "{body}");
    assert_eq!(desired_image(&council, "web").await, None);
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// Decision 5: a node whose runtime pulls nothing (ProcessGrill) has no
/// binder, and stores the image as written.
#[tokio::test]
async fn without_a_binder_the_image_is_stored_as_written() {
    let council = seeded_council("bind-no-binder").await;
    let (app, shutdown) = router_with(Some(council.clone()), None, None);

    let (status, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        desired_image(&council, "web").await,
        Some("nginx:1.27".to_string())
    );
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// A standalone node binds before its agent deploys, so the instance runs
/// (and reports) the bound reference.
#[tokio::test]
async fn a_standalone_apply_deploys_the_bound_reference() {
    let (app, shutdown) = router_with(None, None, Some(binder(Some(9))));

    let (status, body) = post(
        app.clone(),
        "/v1/apply",
        "[app.web]\nimage = \"nginx:1.27\"\n",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("web: nginx:1.27 → sha256:"), "{body}");
    let response = app
        .oneshot(
            axum::http::Request::get("/v1/apps")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let apps = response.into_body().collect().await.unwrap().to_bytes();
    let apps = String::from_utf8_lossy(&apps);
    assert!(
        apps.contains(&format!("nginx:1.27@{}", digest(9).as_str())),
        "{apps}"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn a_standalone_apply_fails_before_deploying_when_the_registry_is_down() {
    let (app, shutdown) = router_with(None, None, Some(binder(None)));

    let (status, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("nginx:1.27"), "{body}");
    shutdown.cancel();
}

fn completed(id: u64, created_secs: u64, image: &str) -> DeployHistoryEntry {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(created_secs);
    let spec: crate::config::app::AppSpec =
        toml::from_str(&format!("image = \"{image}\"")).unwrap();
    DeployHistoryEntry {
        id: DeployId(id),
        app_id: AppId::new("web", "default"),
        image: image.to_string(),
        result: DeployResult::Completed,
        created_at: at,
        completed_at: at,
        steps_completed: 1,
        steps_total: 1,
        spec: Some(Box::new(spec)),
    }
}

/// A rollback restores the reference that ran before, digest and all. The
/// registry is down, so a re-resolution of the tag would fail the rollback.
#[tokio::test]
async fn a_rollback_restores_the_bound_digest_without_asking_the_registry() {
    let council = seeded_council("bind-rollback").await;
    let old = format!("nginx:1.27@{}", digest(1).as_str());
    let new = format!("nginx:1.27@{}", digest(2).as_str());
    let spec: crate::config::app::AppSpec = toml::from_str(&format!("image = \"{new}\"")).unwrap();
    council
        .write(crate::council::RaftRequest::AppSpec {
            app_id: AppId::new("web", "default"),
            spec: Box::new(spec),
        })
        .await
        .unwrap();
    let history = Arc::new(RwLock::new(vec![
        completed(1, 10, &old),
        completed(2, 20, &new),
    ]));
    let (app, shutdown) = router_with(Some(council.clone()), Some(history), Some(binder(None)));

    let (status, body) = post(app, "/v1/rollback/web/default", "").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("\"Error\""), "{body}");
    assert_eq!(desired_image(&council, "web").await, Some(old));
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}
