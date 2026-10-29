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
    assert!(
        body.contains("sees only app, workload, caller and source"),
        "{body}"
    );
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
                applies_to: crate::config::PolicyTarget::Apps,
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

impl Cluster {
    async fn get(&self, uri: &str, bearer: &str) -> (StatusCode, String) {
        let request = axum::http::Request::get(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .extension(axum::extract::ConnectInfo(
                "203.0.113.9:40000".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// #1119: a policy that applies to jobs refuses a job manifest, naming the
/// job, before anything is written; the refusal is audited.
#[tokio::test]
async fn a_jobs_policy_refuses_a_job_manifest() {
    let cluster = cluster("policy-jobs").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.admin,
            "[policy.pinned]\napplies_to = 'jobs'\ndeny = \"job.image.endsWith(':latest')\"\nmessage = 'pin a version'\n",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.deployer,
            "[job.nightly]\nimage = 'report:latest'\nschedule = '0 3 * * *'\n",
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let refusal: apply::PolicyRefusal = serde_json::from_str(&body).unwrap();
    assert_eq!(
        refusal.policy_denials,
        vec!["policy pinned denies job default/nightly: pin a version".to_string()]
    );
    assert_eq!(cluster.records("policy.denied").await.len(), 1);
    assert_eq!(
        cluster
            .council
            .desired_state()
            .await
            .task_arrays
            .jobs()
            .definitions()
            .count(),
        0
    );
}

/// #1119: an audit policy lets the apply through without a word to the
/// caller, and records the finding in the audit log.
#[tokio::test]
async fn an_audit_policy_records_and_lets_the_apply_through() {
    let cluster = cluster("policy-audit-action").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.admin,
            "[policy.limits]\naction = 'audit'\ndeny = '!has(app.memory)'\nmessage = 'set a memory limit'\n",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.deployer,
            "[app.web]\nimage = 'web:v1'\n",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("limits"), "the caller isn't told: {body}");
    let audited = cluster.records("policy.audited").await;
    assert_eq!(audited.len(), 1, "{audited:?}");
    assert_eq!(audited[0].details["policies"], "limits");
    assert!(
        cluster
            .council
            .desired_state()
            .await
            .apps
            .contains_key(&crate::meat::AppId::new("web", "default"))
    );
}

/// #1119: a policy committed after an app never touches it. The audit
/// route lists it as something the policy would now refuse, any token may
/// read it, and nothing changes.
#[tokio::test]
async fn the_audit_lists_running_work_a_new_policy_would_refuse() {
    let cluster = cluster("policy-audit-route").await;
    let (status, body) = cluster
        .call("/v1/apply", &cluster.deployer, LATEST_APP)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let web = crate::meat::AppId::new("web", "default");
    let before = cluster.council.desired_state().await.apps[&web].clone();
    cluster.apply_policy().await;

    let (status, body) = cluster.get("/v1/policies/audit", &cluster.deployer).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let audit: crate::policy::audit::PolicyAudit = serde_json::from_str(&body).unwrap();
    assert_eq!((audit.policies, audit.apps, audit.generation), (1, 1, 1));
    assert_eq!(
        audit
            .verdict
            .denials
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["policy no-latest denies app default/web: pin a version".to_string()]
    );
    assert_eq!(cluster.council.desired_state().await.apps[&web], before);
}

/// A jobs policy that refuses `:latest` images, as a deny or a warn rule.
fn pinned_jobs_policy(action: &str) -> String {
    format!(
        "[policy.pinned-jobs]\napplies_to = 'jobs'\naction = '{action}'\ndeny = \"job.image.endsWith(':latest')\"\nmessage = 'pin a version'\n"
    )
}

/// Every route that admits a job besides `/v1/apply`, with a body that
/// submits one `report:latest` job named `nightly` in `default` (#1424).
const JOB_ROUTES: &[(&str, &str)] = &[
    (
        "/v1/jobs/runs",
        r#"{"name":"nightly","namespace":"default","definition":{"template":{"image":"report:latest","command":["true"]},"cron":{"expression":"0 3 * * *"}}}"#,
    ),
    (
        "/v1/batch",
        r#"{"jobs":[{"name":"nightly","namespace":"default","spec":{"image":"report:latest","command":["true"]}}]}"#,
    ),
    (
        "/v1/batch/array",
        r#"{"name":"nightly","namespace":"default","template":{"image":"report:latest","command":["true"]},"spec":{"count":1}}"#,
    ),
    (
        "/v1/batch/manifest",
        r#"{"name":"nightly","namespace":"default","cohort":[{"name":"small","count":1,"template":{"image":"report:latest","command":["true"]}}]}"#,
    ),
];

/// #1424: a jobs policy refuses a job on every route that admits one, not
/// only on `/v1/apply`. The refusal names the job, writes nothing and is
/// audited.
#[tokio::test]
async fn a_jobs_policy_refuses_a_job_on_every_submission_route() {
    let cluster = cluster("policy-job-routes-deny").await;
    let (status, body) = cluster
        .call("/v1/apply", &cluster.admin, &pinned_jobs_policy("deny"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (index, (route, request)) in JOB_ROUTES.iter().enumerate() {
        let (status, body) = cluster.call(route, &cluster.deployer, request).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{route}: {body}");
        let refusal: apply::PolicyRefusal = serde_json::from_str(&body).unwrap();
        assert_eq!(
            refusal.policy_denials,
            vec!["policy pinned-jobs denies job default/nightly: pin a version".to_string()],
            "{route}"
        );
        assert_eq!(
            cluster.records("policy.denied").await.len(),
            index + 1,
            "{route}"
        );
    }
    let desired = cluster.council.desired_state().await;
    assert_eq!(desired.task_arrays.jobs().definitions().count(), 0);
}

/// #1424: a warn policy lets the job through on every route, and the
/// answer carries the warning.
#[tokio::test]
async fn a_jobs_warn_policy_warns_on_every_submission_route() {
    let cluster = cluster("policy-job-routes-warn").await;
    let (status, body) = cluster
        .call("/v1/apply", &cluster.admin, &pinned_jobs_policy("warn"))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for (route, request) in JOB_ROUTES {
        let request = request.replace("nightly", &format!("nightly-{}", route.len()));
        let (status, body) = cluster.call(route, &cluster.deployer, &request).await;
        assert!(status.is_success(), "{route}: {status} {body}");
        let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
        let warnings = answer["policy_warnings"].as_array().cloned();
        assert_eq!(warnings.unwrap_or_default().len(), 1, "{route}: {body}");
        assert!(
            body.contains("policy pinned-jobs warns about job"),
            "{route}: {body}"
        );
    }
}
