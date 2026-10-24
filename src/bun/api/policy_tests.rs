//! Policies through the real router (#1117): who may write and delete
//! them, what a denial answers, and what the audit log keeps.

use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::tests::{named_user_token, seeded_council, setup_with_auth_leases_events_and_council};
use super::*;
use crate::bun::audit::{AuditFilter, AuditLimits, AuditLog, AuditRecord};
use crate::bun::events::EventStore;
use crate::sesame::types::ApiRole;

const NO_LATEST: &str = r#"
[policy.no-latest]
deny = "app.image.endsWith(':latest')"
message = "pin a version"
"#;

const LATEST_APP: &str = "[app.web]\nimage = 'web:latest'\n";

struct Cluster {
    app: Router,
    council: Arc<crate::council::CouncilNode>,
    log: AuditLog,
    admin: String,
    deployer: String,
    _events: Arc<RwLock<EventStore>>,
    _directory: tempfile::TempDir,
    _shutdown: tokio_util::sync::CancellationToken,
}

async fn cluster(tag: &str) -> Cluster {
    let council = seeded_council(tag).await;
    let (admin_token, admin) = named_user_token("operator", ApiRole::Admin);
    let (deployer_token, deployer) = named_user_token("ci", ApiRole::Deployer);
    let directory = tempfile::tempdir().unwrap();
    let log = AuditLog::open(
        directory.path().join("audit"),
        AuditLimits::from_total(1 << 20),
    )
    .unwrap();
    let mut store = EventStore::new();
    store.set_audit_log(log.clone());
    let events = Arc::new(RwLock::new(store));
    let (app, shutdown) = setup_with_auth_leases_events_and_council(
        vec![admin_token, deployer_token],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::clone(&events)),
        Some(Arc::clone(&council)),
    )
    .await;
    Cluster {
        app,
        council,
        log,
        admin,
        deployer,
        _events: events,
        _directory: directory,
        _shutdown: shutdown,
    }
}

impl Cluster {
    async fn call(&self, uri: &str, bearer: &str, body: &str) -> (StatusCode, String) {
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

    async fn records(&self, action: &str) -> Vec<AuditRecord> {
        self.log
            .query(AuditFilter {
                action: Some(action.to_string()),
                ..AuditFilter::default()
            })
            .await
            .unwrap()
    }

    async fn apply_policy(&self) {
        let (status, body) = self.call("/v1/apply", &self.admin, NO_LATEST).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("policy no-latest: committed"), "{body}");
    }
}

/// Only an admin writes policies; a deployer's apply of one is refused
/// before anything is written.
#[tokio::test]
async fn only_an_admin_writes_a_policy() {
    let cluster = cluster("policy-admin-only").await;
    let (status, body) = cluster
        .call("/v1/apply", &cluster.deployer, NO_LATEST)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(cluster.council.desired_state().await.policies.is_empty());

    cluster.apply_policy().await;
    let desired = cluster.council.desired_state().await;
    assert!(desired.policies.contains_key("no-latest"));
    assert_eq!(desired.policy_generation, 1);
    // The apply's audit record names the policy it wrote.
    let applied = cluster.records("app.applied").await;
    assert!(
        applied
            .iter()
            .any(|record| record.details.get("policies").map(String::as_str) == Some("no-latest")),
        "{applied:?}"
    );
}

/// A denied apply answers 422 naming the policy, writes nothing, and is
/// audited as `policy.denied`.
#[tokio::test]
async fn a_denied_apply_writes_nothing_and_is_audited() {
    let cluster = cluster("policy-denied").await;
    cluster.apply_policy().await;
    let (status, body) = cluster
        .call("/v1/apply", &cluster.deployer, LATEST_APP)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let refusal: apply::PolicyRefusal = serde_json::from_str(&body).unwrap();
    assert_eq!(
        refusal.policy_denials,
        vec!["policy no-latest denies app default/web: pin a version".to_string()]
    );
    assert!(cluster.council.desired_state().await.apps.is_empty());
    let denied = cluster.records("policy.denied").await;
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0].details["policies"], "no-latest");
    assert_eq!(denied[0].details["token_name"], "ci");
}

/// A warn policy lets the apply through and the stream says why.
#[tokio::test]
async fn a_warning_is_streamed_and_the_apply_goes_ahead() {
    let cluster = cluster("policy-warn").await;
    let warn = NO_LATEST.replace("message = \"pin a version\"", "action = \"warn\"");
    let (status, body) = cluster.call("/v1/apply", &cluster.admin, &warn).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = cluster
        .call("/v1/apply", &cluster.deployer, LATEST_APP)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("warning: policy no-latest warns about app default/web"),
        "{body}"
    );
    assert!(body.contains("app web: committed"), "{body}");
}

/// A policy that refuses every app is removed by an admin, and the
/// removal is audited. A deployer can't remove it.
#[tokio::test]
async fn an_admin_deletes_a_policy_that_refuses_everything() {
    let cluster = cluster("policy-delete").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.admin,
            "[policy.everything]\ndeny = 'true'\n",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = cluster
        .call("/v1/apply", &cluster.admin, "[app.web]\nimage = 'web:v1'\n")
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = cluster
        .call("/v1/policies/everything/delete", &cluster.deployer, "")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = cluster
        .call("/v1/policies/everything/delete", &cluster.admin, "")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let deleted = cluster.records("policy.deleted").await;
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(deleted[0].details["name"], "everything");

    let (status, body) = cluster
        .call("/v1/apply", &cluster.admin, "[app.web]\nimage = 'web:v1'\n")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = cluster
        .call("/v1/policies/everything/delete", &cluster.admin, "")
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A bad policy is refused when it's applied, before Raft sees it.
#[tokio::test]
async fn a_bad_policy_is_refused_at_apply() {
    let cluster = cluster("policy-bad").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.admin,
            "[policy.rule]\ndeny = \"permission.actions == []\"\n",
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("policies see only app"), "{body}");
    assert_eq!(cluster.council.desired_state().await.policy_generation, 0);
}

/// When the policies move after the apply judged its apps, the apps still
/// to write are judged again and fenced on the new generation, or the
/// apply stops naming the new policy.
#[tokio::test]
async fn rejudging_refences_the_remaining_apps_or_names_the_new_denial() {
    let council = seeded_council("policy-rejudge").await;
    let declared = crate::config::Config::parse(
        "[app.web]\nimage = 'web:latest'\n[app.api]\nimage = 'api:v1'\n",
    )
    .unwrap();
    let caller = apply::policy_caller(None);
    let mut pending: std::collections::VecDeque<_> = crate::policy::fence(
        crate::council::config_to_desired_writes(&declared),
        0,
        &std::collections::BTreeMap::new(),
    )
    .into();

    // A lenient change: both apps are fenced again on the new generation.
    council
        .write(crate::council::RaftRequest::PolicySpec {
            name: "never".into(),
            spec: Box::new(crate::config::PolicySpec {
                match_expression: None,
                deny: Some("false".into()),
                message: None,
                action: crate::config::PolicyAction::Deny,
                default: None,
            }),
        })
        .await
        .unwrap();
    apply::rejudge(
        &council,
        &declared,
        &Default::default(),
        &caller,
        &mut pending,
    )
    .await
    .unwrap();
    assert!(pending.iter().all(|write| matches!(
        write,
        crate::council::RaftRequest::PolicyFenced { generation: 1, .. }
    )));

    // A stricter one: the apply stops, naming it.
    let config = crate::config::Config::parse(NO_LATEST).unwrap();
    for write in crate::council::config_to_desired_writes(&config) {
        council.write(write).await.unwrap();
    }
    let error = apply::rejudge(
        &council,
        &declared,
        &Default::default(),
        &caller,
        &mut pending,
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("policy no-latest denies app default/web"),
        "{error}"
    );
}

/// #1118: an apply fills in a policy's default, says so as it streams,
/// stores the filled spec and keeps the declared one beside it. A field
/// the app sets is never overwritten.
#[tokio::test]
async fn an_apply_fills_in_defaults_and_keeps_the_declared_spec() {
    let cluster = cluster("policy-defaults").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.admin,
            "[policy.team-label.default]\nenv.TEAM = 'payments'\nmemory = '256Mi'\n",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let manifest = "[app.web]\nimage = 'web:v1'\nmemory = '1Gi'\n";
    let (status, body) = cluster.call("/v1/apply", &cluster.deployer, manifest).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("policy team-label fills env.TEAM = \\\"payments\\\" on app default/web"),
        "{body}"
    );
    assert!(!body.contains("fills memory"), "{body}");

    let web = crate::meat::AppId::new("web", "default");
    let desired = cluster.council.desired_state().await;
    let effective = &desired.apps[&web];
    assert_eq!(
        effective.env["TEAM"],
        crate::config::EnvValue::Plain("payments".into())
    );
    let declared = crate::config::Config::parse(manifest).unwrap();
    assert_eq!(
        effective.memory, declared.app["web"].memory,
        "the app's own memory wins"
    );
    assert_eq!(desired.declared_apps[&web], declared.app["web"]);
}
