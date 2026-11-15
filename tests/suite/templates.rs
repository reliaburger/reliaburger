//! Templates that expand to ordinary apps (#1124), end to end on a real
//! council.
//!
//! - `relish apply` expands a template before it sends the manifest, and
//!   the apply endpoint expands one that arrives unexpanded. Either way Raft
//!   holds an ordinary app with its provenance, and nothing else.
//! - A policy judges the expanded app, so it can refuse one.
//! - GitOps expands a template from one file for a `use` in another, and
//!   later syncs of the same commit content don't re-apply the app.

use std::sync::Arc;

use reliaburger::config::template::Provenance;
use reliaburger::council::node::CouncilNode;
use reliaburger::lettuce::runner::spawn_gitops_sync;
use reliaburger::lettuce::types::GitOpsConfig;
use reliaburger::meat::types::AppId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::policies::{git, head, post_apply, single_node_leader, synced};

const POSTGRES: &str = r#"
[template.postgres]
params = { name = "string", size = "string", replicas = { type = "int", default = 1 } }
body = """
[app.${name}]
image = "postgres:17"
replicas = ${replicas}
[[app.${name}.volumes]]
path = "/var/lib/postgresql/data"
size = "${size}"
"""
"#;

const ORDERS_DB: &str = r#"
[use.orders-db]
template = "postgres"
name = "orders-db"
size = "50Gi"
"#;

fn router(council: &Arc<CouncilNode>) -> axum::Router {
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

fn orders_db_provenance() -> Option<Provenance> {
    Some(Provenance {
        template: "postgres".into(),
        instance: "orders-db".into(),
    })
}

/// What `relish apply` sends is the expanded config; the endpoint gets the
/// same result from the raw manifest. Both store one ordinary app.
#[tokio::test]
async fn a_template_applies_as_an_ordinary_app_with_its_provenance() {
    let manifest = format!("{POSTGRES}\n{ORDERS_DB}");
    let expanded = reliaburger::relish::manifest::parse(&manifest)
        .unwrap()
        .config;
    let sent_by_relish = toml::to_string_pretty(&expanded).unwrap();
    assert!(!sent_by_relish.contains("[template"), "{sent_by_relish}");

    let mut stored = Vec::new();
    for (id, body) in [(11, sent_by_relish), (12, manifest)] {
        let council = single_node_leader(id).await;
        let (status, response) = post_apply(&router(&council), &body).await;
        assert!(status.is_success(), "{status}: {response}");
        let state = council.desired_state().await;
        assert_eq!(state.apps.len(), 1, "{:?}", state.apps.keys());
        let app = state.apps[&AppId::new("orders-db", "default")].clone();
        assert_eq!(app.provenance, orders_db_provenance());
        assert_eq!(app.volumes[0].size.as_deref(), Some("50Gi"));
        stored.push(app);
        council.shutdown().await.ok();
    }
    assert_eq!(stored[0], stored[1], "both paths store the same app");
}

#[tokio::test]
async fn a_policy_refuses_an_app_a_template_expanded_to() {
    let council = single_node_leader(13).await;
    let router = router(&council);
    let (status, body) = post_apply(
        &router,
        r#"
[policy.db-standby]
match = "has(app.provenance) && app.provenance.template == 'postgres'"
deny = "app.replicas < 2"
message = "a database needs a standby"
"#,
    )
    .await;
    assert!(status.is_success(), "{status}: {body}");

    let (status, body) = post_apply(&router, &format!("{POSTGRES}\n{ORDERS_DB}")).await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "{body}"
    );
    assert!(
        body.contains("policy db-standby denies app default/orders-db"),
        "{body}"
    );
    assert!(council.desired_state().await.apps.is_empty());

    let (status, body) =
        post_apply(&router, &format!("{POSTGRES}\n{ORDERS_DB}replicas = 2\n")).await;
    assert!(status.is_success(), "{status}: {body}");
    council.shutdown().await.ok();
}

#[tokio::test]
async fn a_use_that_cannot_expand_is_refused_and_writes_nothing() {
    let council = single_node_leader(14).await;
    let (status, body) = post_apply(
        &router(&council),
        &format!("{POSTGRES}\n[use.orders-db]\ntemplate = 'postgres'\nname = 'orders-db'\n"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("needs a value for \\\"size\\\""), "{body}");
    assert!(council.desired_state().await.apps.is_empty());
    council.shutdown().await.ok();
}

/// The diff trap again (#1118's test, for templates): what git declares is
/// the expanded app, provenance and all, and that's what the cluster keeps.
/// So syncing a commit that doesn't change the app writes nothing, and
/// changing a parameter is an ordinary modification.
#[tokio::test]
async fn repeated_syncs_of_a_templated_app_do_not_reapply_it() {
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(repository.path().join("templates.toml"), POSTGRES).unwrap();
    let databases = repository.path().join("databases.toml");
    std::fs::write(&databases, ORDERS_DB).unwrap();
    git(repository.path(), &["add", "."]);
    git(
        repository.path(),
        &["commit", "-q", "-m", "a templated app"],
    );

    let council = single_node_leader(15).await;
    let shutdown = CancellationToken::new();
    let (_webhook, webhook_rx) = mpsc::channel::<()>(4);
    let data = tempfile::tempdir().unwrap();
    spawn_gitops_sync(
        Arc::clone(&council),
        Some(GitOpsConfig {
            repo: repository.path().to_string_lossy().to_string(),
            branch: "main".to_string(),
            path: "/".to_string(),
            poll_interval_secs: 1,
            require_signed_commits: false,
            trusted_signing_keys: vec![],
            webhook_secret: None,
            webhook_rate_limit: 10,
        }),
        webhook_rx,
        data.path().to_path_buf(),
        shutdown.clone(),
        None,
    );

    let summary = synced(&council, &head(repository.path())).await;
    assert_eq!(summary.added, 1, "only the expanded app: {summary:?}");
    let orders = AppId::new("orders-db", "default");
    let state = council.desired_state().await;
    assert_eq!(state.apps[&orders].provenance, orders_db_provenance());
    let effective = state.apps[&orders].clone();

    // An unrelated app: only it is added.
    std::fs::write(
        repository.path().join("web.toml"),
        "[app.web]\nimage = 'web:v1'\n",
    )
    .unwrap();
    git(repository.path(), &["add", "."]);
    git(repository.path(), &["commit", "-q", "-m", "another app"]);
    let summary = synced(&council, &head(repository.path())).await;
    assert_eq!((summary.added, summary.modified), (1, 0), "{summary:?}");

    // A commit that changes nothing the cluster sees: no change at all.
    std::fs::write(&databases, format!("# our databases\n{ORDERS_DB}")).unwrap();
    git(repository.path(), &["commit", "-q", "-am", "a comment"]);
    let summary = synced(&council, &head(repository.path())).await;
    assert_eq!(
        (summary.added, summary.modified, summary.removed),
        (0, 0, 0),
        "{summary:?}"
    );
    assert_eq!(council.desired_state().await.apps[&orders], effective);

    // A changed parameter changes the expanded app, and only that app.
    std::fs::write(&databases, format!("{ORDERS_DB}replicas = 2\n")).unwrap();
    git(repository.path(), &["commit", "-q", "-am", "a standby"]);
    let summary = synced(&council, &head(repository.path())).await;
    assert_eq!(
        (summary.added, summary.modified, summary.removed),
        (0, 1, 0),
        "{summary:?}"
    );
    shutdown.cancel();
    council.shutdown().await.ok();
}
