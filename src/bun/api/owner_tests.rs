//! Resource owners through the real router (#1125): one test per rule in
//! the table in `council::owner`, with a controller holding a token scoped
//! to its namespace, a human deployer, and an admin.

use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::tests::{named_user_token, seeded_council, setup_with_auth_leases_events_and_council};
use super::*;
use crate::meat::AppId;
use crate::sesame::types::{ApiRole, TokenScope};

const NAMESPACE: &str = "[namespace.pg]\n";
const DB: &str = "[app.db]\nimage = 'postgres:17'\nnamespace = 'pg'\n";
const CLAIMED_DB: &str =
    "[app.db]\nimage = 'postgres:17'\nnamespace = 'pg'\nowner = 'pg-failover'\n";

struct Cluster {
    app: Router,
    council: Arc<crate::council::CouncilNode>,
    admin: String,
    human: String,
    controller: String,
    rival: String,
    _shutdown: tokio_util::sync::CancellationToken,
}

/// A token scoped to the `pg` namespace, the way a controller's is.
fn scoped_token(name: &str) -> (crate::sesame::types::ApiToken, String) {
    let created = crate::sesame::token::create_token(
        name,
        ApiRole::Deployer,
        TokenScope {
            apps: None,
            namespaces: Some(vec!["pg".to_string()]),
        },
        None,
    )
    .unwrap();
    (created.token, created.plaintext)
}

async fn cluster(tag: &str) -> Cluster {
    let council = seeded_council(tag).await;
    let (admin_token, admin) = named_user_token("operator", ApiRole::Admin);
    let (human_token, human) = named_user_token("alice", ApiRole::Deployer);
    let (controller_token, controller) = scoped_token("pg-failover");
    let (rival_token, rival) = scoped_token("other-controller");
    let (app, shutdown) = setup_with_auth_leases_events_and_council(
        vec![admin_token, human_token, controller_token, rival_token],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        None,
        Some(Arc::clone(&council)),
    )
    .await;
    let cluster = Cluster {
        app,
        council,
        admin,
        human,
        controller,
        rival,
        _shutdown: shutdown,
    };
    let (status, body) = cluster.call("/v1/apply", &cluster.admin, NAMESPACE).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    cluster
}

impl Cluster {
    async fn call(&self, uri: &str, bearer: &str, body: &str) -> (StatusCode, String) {
        let request = axum::http::Request::post(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn db(&self) -> Option<crate::config::AppSpec> {
        self.council
            .desired_state()
            .await
            .apps
            .get(&AppId::new("db", "pg"))
            .cloned()
    }

    /// The controller claims `db`, which a human applied first.
    async fn claimed(tag: &str) -> Self {
        let cluster = cluster(tag).await;
        let (status, body) = cluster.call("/v1/apply", &cluster.human, DB).await;
        assert!(body.contains("committed"), "{status}: {body}");
        let (status, body) = cluster
            .call("/v1/apply", &cluster.controller, CLAIMED_DB)
            .await;
        assert!(body.contains("committed"), "{status}: {body}");
        assert_eq!(
            cluster.db().await.unwrap().owner.as_deref(),
            Some("pg-failover")
        );
        cluster
    }
}

/// Rule: an apply that doesn't declare the owner skips an owned app, with
/// a note, and still writes everything else in the file.
#[tokio::test]
async fn an_apply_skips_an_app_a_controller_owns() {
    let cluster = Cluster::claimed("owner-skip").await;
    let manifest = "[app.db]\nimage = 'postgres:18'\nnamespace = 'pg'\n\n\
                    [app.web]\nimage = 'web:v1'\nnamespace = 'pg'\n";
    let (status, body) = cluster.call("/v1/apply", &cluster.human, manifest).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("app 'db' in namespace 'pg' is owned by 'pg-failover'; skipped"),
        "{body}"
    );
    let db = cluster.db().await.unwrap();
    assert_eq!(db.image.as_deref(), Some("postgres:17"));
    assert_eq!(db.owner.as_deref(), Some("pg-failover"));
    assert!(
        cluster
            .council
            .desired_state()
            .await
            .apps
            .contains_key(&AppId::new("web", "pg"))
    );
}

/// Rule: the owner's own writes go through.
#[tokio::test]
async fn the_owner_keeps_writing_its_app() {
    let cluster = Cluster::claimed("owner-writes").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.controller,
            &CLAIMED_DB.replace("postgres:17", "postgres:18"),
        )
        .await;
    assert!(body.contains("committed"), "{status}: {body}");
    assert_eq!(
        cluster.db().await.unwrap().image.as_deref(),
        Some("postgres:18")
    );
}

/// Rule: only the token an owner names, or an admin, may declare it.
#[tokio::test]
async fn declaring_another_tokens_name_as_owner_is_forbidden() {
    let cluster = cluster("owner-not-yours").await;
    let (status, body) = cluster.call("/v1/apply", &cluster.human, CLAIMED_DB).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("must be the name of the token"), "{body}");
    assert!(cluster.db().await.is_none());

    let (status, body) = cluster.call("/v1/apply", &cluster.admin, CLAIMED_DB).await;
    assert!(body.contains("committed"), "{status}: {body}");
}

/// Rule: a different owner can't take an owned app until it's released.
#[tokio::test]
async fn a_second_controller_cant_take_an_owned_app() {
    let cluster = Cluster::claimed("owner-rival").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.rival,
            &CLAIMED_DB.replace("pg-failover", "other-controller"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        cluster.db().await.unwrap().owner.as_deref(),
        Some("pg-failover")
    );
}

/// Rule: stop, delete, rollback and release need the owner or an admin. `--force`
/// is about GitOps and doesn't get round an owner.
#[tokio::test]
async fn imperative_changes_to_an_owned_app_need_the_owner_or_an_admin() {
    let cluster = Cluster::claimed("owner-commands").await;
    for uri in [
        "/v1/stop/db/pg",
        "/v1/delete/db/pg",
        "/v1/delete/db/pg?force=true",
        "/v1/rollback/db/pg",
        "/v1/release/db/pg",
    ] {
        let (status, body) = cluster.call(uri, &cluster.human, "").await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {body}");
        assert!(
            body.contains("owned by \\\"pg-failover\\\""),
            "{uri}: {body}"
        );
    }
    assert!(cluster.db().await.is_some());

    let (status, body) = cluster.call("/v1/stop/db/pg", &cluster.admin, "").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = cluster
        .call("/v1/delete/db/pg", &cluster.controller, "")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(cluster.db().await.is_none());
}

/// Rule: a release hands the app back, after which an ordinary apply
/// writes it again. A release of an unowned app is a conflict.
#[tokio::test]
async fn a_release_hands_the_app_back_to_apply() {
    let cluster = Cluster::claimed("owner-release").await;
    let (status, body) = cluster
        .call("/v1/release/db/pg", &cluster.controller, "")
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(body.contains("released"), "{body}");
    assert_eq!(cluster.db().await.unwrap().owner, None);

    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.human,
            &DB.replace("postgres:17", "postgres:18"),
        )
        .await;
    assert!(body.contains("committed"), "{status}: {body}");
    assert_eq!(
        cluster.db().await.unwrap().image.as_deref(),
        Some("postgres:18")
    );

    let (status, body) = cluster.call("/v1/release/db/pg", &cluster.admin, "").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

/// A controller's token is scoped to its namespace: it can't write, stop
/// or release anything outside it, owned or not.
#[tokio::test]
async fn a_controller_token_cant_touch_another_namespace() {
    let cluster = cluster("owner-scope").await;
    let (status, body) = cluster
        .call("/v1/apply", &cluster.admin, "[namespace.shop]\n")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.controller,
            "[app.web]\nimage = 'web:v1'\nnamespace = 'shop'\nowner = 'pg-failover'\n",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    for uri in ["/v1/stop/web/shop", "/v1/release/web/shop"] {
        let (status, body) = cluster.call(uri, &cluster.controller, "").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
    }
    assert!(cluster.council.desired_state().await.apps.is_empty());
}

/// The desired-apps listing names each app's owner, which is how a
/// controller finds the apps it claimed, and a scoped token sees only its
/// own namespace there.
#[tokio::test]
async fn desired_apps_name_the_owner_within_the_tokens_scope() {
    let cluster = Cluster::claimed("owner-listing").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.admin,
            "[namespace.shop]\n[app.web]\nimage = 'web:v1'\nnamespace = 'shop'\n",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let request = axum::http::Request::get("/v1/diagnostics/apps")
        .header("authorization", format!("Bearer {}", cluster.controller))
        .body(Body::empty())
        .unwrap();
    let response = cluster.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let apps: Vec<crate::bun::diagnostics::DesiredAppEvidence> =
        serde_json::from_slice(&bytes).unwrap();
    let seen: Vec<(&str, &str, Option<&str>)> = apps
        .iter()
        .map(|app| {
            (
                app.namespace.as_str(),
                app.app.as_str(),
                app.owner.as_deref(),
            )
        })
        .collect();
    assert_eq!(seen, vec![("pg", "db", Some("pg-failover"))]);
}

/// #1124: a template body that declares an `owner` is held to the same rule
/// as a hand-written one, because the apply only ever sees the expanded
/// app. And the expanded app keeps its provenance beside its owner, and in
/// the declared spec a policy default leaves behind.
#[tokio::test]
async fn an_owner_a_template_declares_follows_the_owner_rules() {
    const TEMPLATED_DB: &str = r#"
[template.postgres]
params = { name = "string", controller = "string" }
body = """
[app.${name}]
image = "postgres:17"
namespace = "pg"
owner = "${controller}"
"""

[use.db]
template = "postgres"
name = "db"
controller = "pg-failover"
"#;
    let cluster = cluster("owner-template").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.admin,
            "[policy.team]\nmatch = 'has(app.provenance)'\n[policy.team.default]\nenv.TEAM = 'data'\n",
        )
        .await;
    assert!(body.contains("committed"), "{status}: {body}");

    let (status, body) = cluster
        .call("/v1/apply", &cluster.human, TEMPLATED_DB)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("must be the name of the token"), "{body}");
    assert!(cluster.db().await.is_none());

    let (status, body) = cluster
        .call("/v1/apply", &cluster.controller, TEMPLATED_DB)
        .await;
    assert!(body.contains("committed"), "{status}: {body}");
    let provenance = Some(crate::config::Provenance {
        template: "postgres".into(),
        instance: "db".into(),
    });
    let db = cluster.db().await.unwrap();
    assert_eq!(db.owner.as_deref(), Some("pg-failover"));
    assert_eq!(db.provenance, provenance);
    assert!(db.env.contains_key("TEAM"), "the default filled it in");
    let state = cluster.council.desired_state().await;
    let declared = &state.declared_apps[&AppId::new("db", "pg")];
    assert_eq!(declared.provenance, provenance);
    assert_eq!(declared.owner.as_deref(), Some("pg-failover"));
    assert!(!declared.env.contains_key("TEAM"));
}

impl Cluster {
    /// Post `body` and answer with the status alone, without waiting for a
    /// streamed answer to finish: a job manifest's stream lasts until its
    /// jobs have run, and nothing runs them here.
    async fn start(&self, uri: &str, bearer: &str, body: &str) -> StatusCode {
        let request = axum::http::Request::post(uri)
            .header("authorization", format!("Bearer {bearer}"))
            .body(Body::from(body.to_string()))
            .unwrap();
        self.app.clone().oneshot(request).await.unwrap().status()
    }

    /// The config of every job deployment the cluster accepted.
    async fn deployed_configs(&self) -> Vec<crate::config::Config> {
        self.council
            .desired_state()
            .await
            .task_arrays
            .deployments()
            .map(|(_, record)| (*record.config).clone())
            .collect()
    }
}

const JOB: &str = "[job.migrate]\nimage = 'migrate:v1'\nnamespace = 'pg'\ncommand = ['true']\n";

/// #1429: a job manifest follows the owner rules too. Declaring an owner
/// the caller can't speak for is forbidden, and a rival's claim conflicts,
/// before anything is written.
#[tokio::test]
async fn a_job_manifest_cant_take_an_owned_app() {
    let cluster = Cluster::claimed("owner-job-manifest").await;
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.human,
            &format!("{JOB}{}", CLAIMED_DB.replace("postgres:17", "evil:1")),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = cluster
        .call(
            "/v1/apply",
            &cluster.rival,
            &format!(
                "{JOB}{}",
                CLAIMED_DB
                    .replace("pg-failover", "other-controller")
                    .replace("postgres:17", "evil:1")
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(cluster.deployed_configs().await.is_empty());
    assert_eq!(
        cluster.db().await.unwrap().image.as_deref(),
        Some("postgres:17")
    );
}

/// #1429: a job manifest that doesn't declare the owner leaves the owned
/// app out of what it deploys, as `relish apply` does, and keeps its jobs.
#[tokio::test]
async fn a_job_manifest_skips_an_owned_app() {
    let cluster = Cluster::claimed("owner-job-skip").await;
    let status = cluster
        .start(
            "/v1/apply",
            &cluster.human,
            &format!("{JOB}{}", DB.replace("postgres:17", "postgres:18")),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let configs = cluster.deployed_configs().await;
    assert_eq!(configs.len(), 1);
    assert!(configs[0].app.is_empty(), "{:?}", configs[0].app);
    assert!(configs[0].job.contains_key("migrate"));
}
