//! `POST /v1/scale/{app}/{namespace}` through the real router (#2176): the
//! count goes into the app's spec in the council, the way an apply's does,
//! so the next apply of the file puts back the file's count. It's refused
//! for what has no count to set, for an app GitOps manages unless forced,
//! and for a token that may not scale the app, and it's audited.

use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::tests::{named_user_token, seeded_council, setup_with_auth_leases_events_and_council};
use super::*;
use crate::bun::audit::{AuditFilter, AuditLimits, AuditLog, AuditRecord};
use crate::bun::events::EventStore;
use crate::config::Replicas;
use crate::meat::AppId;
use crate::sesame::types::{ApiRole, TokenScope};

const WEB: &str = "[app.web]\nimage = \"web:1\"\nreplicas = 1\nnamespace = \"shop\"\n";

struct Cluster {
    app: Router,
    council: Arc<crate::council::CouncilNode>,
    log: AuditLog,
    admin: String,
    deployer: String,
    reader: String,
    other_namespace: String,
    _directory: tempfile::TempDir,
    _shutdown: tokio_util::sync::CancellationToken,
}

async fn cluster(tag: &str) -> Cluster {
    let council = seeded_council(tag).await;
    let (admin_token, admin) = named_user_token("operator", ApiRole::Admin);
    let (deployer_token, deployer) = named_user_token("alice", ApiRole::Deployer);
    let (reader_token, reader) = named_user_token("reader", ApiRole::ReadOnly);
    let scoped = crate::sesame::token::create_token(
        "billing-bot",
        ApiRole::Deployer,
        TokenScope {
            apps: None,
            namespaces: Some(vec!["billing".to_string()]),
        },
        None,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let log = AuditLog::open(
        directory.path().join("audit"),
        AuditLimits::from_total(1 << 20),
    )
    .unwrap();
    let mut store = EventStore::new();
    store.set_audit_log(log.clone());
    let (app, shutdown) = setup_with_auth_leases_events_and_council(
        vec![admin_token, deployer_token, reader_token, scoped.token],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::new(RwLock::new(store))),
        Some(Arc::clone(&council)),
    )
    .await;
    let cluster = Cluster {
        app,
        council,
        log,
        admin,
        deployer,
        reader,
        other_namespace: scoped.plaintext,
        _directory: directory,
        _shutdown: shutdown,
    };
    let (status, body) = cluster
        .post("/v1/apply", &cluster.admin, "[namespace.shop]\n")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    cluster
}

impl Cluster {
    async fn post(&self, uri: &str, bearer: &str, body: &str) -> (StatusCode, String) {
        let request = axum::http::Request::post(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .extension(axum::extract::ConnectInfo(
                "203.0.113.9:40000".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Apply `manifest` as the deployer and check it committed.
    async fn apply(&self, manifest: &str) {
        let (status, body) = self.post("/v1/apply", &self.deployer, manifest).await;
        assert!(body.contains("committed"), "{status}: {body}");
    }

    async fn scale(&self, bearer: &str, uri: &str, replicas: u32) -> (StatusCode, String) {
        self.post(uri, bearer, &format!(r#"{{"replicas":{replicas}}}"#))
            .await
    }

    async fn web(&self) -> crate::config::AppSpec {
        self.council.desired_state().await.apps[&AppId::new("web", "shop")].clone()
    }

    async fn records(&self, action: &str) -> Vec<AuditRecord> {
        self.log
            .query(AuditFilter {
                action: Some(action.to_string()),
                ..AuditFilter::default()
            })
            .await
            .unwrap()
    }
}

/// A scale writes the new count into the app's spec in the council and
/// keeps the rest of it. What a plan compares the file with moves too, so
/// it shows the difference, and the next apply of the file puts back the
/// file's count.
#[tokio::test]
async fn a_scale_changes_the_stored_spec_until_the_file_is_applied_again() {
    let cluster = cluster("scale-spec").await;
    cluster.apply(WEB).await;

    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/web/shop", 3)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("committed"), "{body}");
    let web = cluster.web().await;
    assert_eq!(web.replicas, Replicas::Fixed(3));
    assert_eq!(web.image.as_deref(), Some("web:1"));
    // What a plan compares the file with: the declared spec, or the stored
    // one when the declaration needed nothing filled in.
    let desired = cluster.council.desired_state().await;
    let web_id = AppId::new("web", "shop");
    let declared = desired
        .declared_apps
        .get(&web_id)
        .unwrap_or(&desired.apps[&web_id]);
    assert_eq!(declared.replicas, Replicas::Fixed(3));

    cluster.apply(WEB).await;
    assert_eq!(cluster.web().await.replicas, Replicas::Fixed(1));
}

/// The scale is in the audit log under its own action, naming the caller.
#[tokio::test]
async fn a_scale_is_audited() {
    let cluster = cluster("scale-audit").await;
    cluster.apply(WEB).await;
    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/web/shop", 2)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let records = cluster.records("app.scaled").await;
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].app.as_deref(), Some("web"));
    assert_eq!(records[0].namespace.as_deref(), Some("shop"));
    assert_eq!(records[0].details["replicas_from"], "1");
    assert_eq!(records[0].details["replicas_to"], "2");
}

/// A stopped app scaled to a count starts again with that count, as an
/// apply starts it.
#[tokio::test]
async fn scaling_a_stopped_app_starts_it_again() {
    let cluster = cluster("scale-stopped").await;
    cluster.apply(WEB).await;
    let (status, body) = cluster
        .post("/v1/stop/web/shop", &cluster.deployer, "")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");

    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/web/shop", 2)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let desired = cluster.council.desired_state().await;
    assert!(!desired.stopped_apps.contains(&AppId::new("web", "shop")));
    assert_eq!(cluster.web().await.replicas, Replicas::Fixed(2));
}

/// Zero is `stop`, an autoscaled app's count is the autoscaler's, and a
/// daemon set has no count: each is refused, naming why, and nothing moves.
#[tokio::test]
async fn what_has_no_count_to_set_is_refused() {
    let cluster = cluster("scale-refused").await;
    cluster.apply(WEB).await;
    cluster
        .apply(
            "[app.api]\nimage = \"api:1\"\nnamespace = \"shop\"\n\
             [app.api.autoscale]\nmetric = \"cpu\"\ntarget = \"70%\"\nmin = 2\nmax = 8\n\
             [app.agent]\nimage = \"agent:1\"\nreplicas = \"*\"\nnamespace = \"shop\"\n",
        )
        .await;

    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/web/shop", 0)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("relish stop"), "{body}");
    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/api/shop", 4)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("autoscale"), "{body}");
    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/agent/shop", 4)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("every node"), "{body}");
    assert_eq!(cluster.web().await.replicas, Replicas::Fixed(1));
}

/// An app that isn't there is a 404, and a body that isn't a count is a
/// 400; neither writes anything.
#[tokio::test]
async fn an_unknown_app_or_a_bad_body_is_refused() {
    let cluster = cluster("scale-unknown").await;
    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/ghost/shop", 2)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    cluster.apply(WEB).await;
    for bad in ["", "not json", r#"{"replicas":-1}"#, r#"{"count":2}"#] {
        let (status, body) = cluster
            .post("/v1/scale/web/shop", &cluster.deployer, bad)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    assert_eq!(cluster.web().await.replicas, Replicas::Fixed(1));
}

/// `current_replicas` is a precondition, as `kubectl scale
/// --current-replicas` is: when the count moved since the caller looked
/// (someone else scaled it), the scale is refused instead of overwriting
/// theirs.
#[tokio::test]
async fn a_stale_current_count_is_refused() {
    let cluster = cluster("scale-precondition").await;
    cluster.apply(WEB).await;
    let (status, body) = cluster
        .post(
            "/v1/scale/web/shop",
            &cluster.deployer,
            r#"{"replicas":4,"current_replicas":2}"#,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("has 1"), "{body}");
    let (status, body) = cluster
        .post(
            "/v1/scale/web/shop",
            &cluster.deployer,
            r#"{"replicas":4,"current_replicas":1}"#,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(cluster.web().await.replicas, Replicas::Fixed(4));
}

/// A read-only token can't scale, and nor can a token scoped to another
/// namespace.
#[tokio::test]
async fn a_token_that_may_not_scale_the_app_is_refused() {
    let cluster = cluster("scale-auth").await;
    cluster.apply(WEB).await;
    for bearer in [&cluster.reader, &cluster.other_namespace] {
        let (status, body) = cluster.scale(bearer, "/v1/scale/web/shop", 3).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    assert_eq!(cluster.web().await.replicas, Replicas::Fixed(1));
}

/// An app GitOps manages is refused without `?force=true`: the next sync
/// would undo the scale. With it the scale lands, and the declared spec
/// moves, so the next sync sees the difference and puts back git's count.
#[tokio::test]
async fn an_app_gitops_manages_needs_force() {
    let cluster = cluster("scale-gitops").await;
    let spec: crate::config::AppSpec =
        toml::from_str("image = \"web:1\"\nreplicas = 1\nnamespace = \"shop\"").unwrap();
    cluster
        .council
        .write(crate::council::types::RaftRequest::GitOpsWrite {
            term: cluster.council.current_term(),
            change: Box::new(crate::council::types::RaftRequest::AppSpec {
                app_id: AppId::new("web", "shop"),
                spec: Box::new(spec),
            }),
        })
        .await
        .unwrap();
    assert!(
        cluster
            .council
            .desired_state()
            .await
            .gitops_owned
            .apps
            .contains(&AppId::new("web", "shop"))
    );

    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/web/shop", 3)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("managed by GitOps"), "{body}");
    assert!(body.contains("--force"), "{body}");
    assert_eq!(cluster.web().await.replicas, Replicas::Fixed(1));

    let (status, body) = cluster
        .scale(&cluster.deployer, "/v1/scale/web/shop?force=true", 3)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("reverted on the next sync"), "{body}");
    assert_eq!(cluster.web().await.replicas, Replicas::Fixed(3));
}

/// A standalone node keeps no desired state to change, so it says how to
/// scale there instead.
#[tokio::test]
async fn a_standalone_node_says_to_change_the_file() {
    let (deployer_token, deployer) = named_user_token("alice", ApiRole::Deployer);
    let (app, shutdown) = setup_with_auth_leases_events_and_council(
        vec![deployer_token],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        None,
        None,
    )
    .await;
    let request = axum::http::Request::post("/v1/scale/web/default")
        .header("authorization", format!("Bearer {deployer}"))
        .body(Body::from(r#"{"replicas":2}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        String::from_utf8_lossy(&bytes).contains("relish apply"),
        "{bytes:?}"
    );
    shutdown.cancel();
}
