//! Policies in the council (#1117), end to end on a real council.
//!
//! - A denied `relish apply` leaves no Raft write, but does leave an audit
//!   record naming the policy.
//! - A GitOps commit with one denied app applies the rest, and the partial
//!   sync result names the policy.
//! - A policy that changes between judging an app and committing it makes
//!   the write retry, rather than land under the old policy.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::audit::{AuditFilter, AuditLimits, AuditLog};
use reliaburger::bun::events::EventStore;
use reliaburger::config::Config;
use reliaburger::council::config_to_desired_writes;
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, CouncilResponse, RaftRequest};
use reliaburger::lettuce::runner::spawn_gitops_sync;
use reliaburger::lettuce::types::GitOpsConfig;
use reliaburger::meat::types::AppId;
use reliaburger::policy::{AdmitError, Caller, Source, admit};
use reliaburger::sesame::types::ApiRole;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

const NO_LATEST: &str = r#"
[policy.no-latest]
deny = "app.image.endsWith(':latest')"
message = "pin a version"
"#;

pub(crate) async fn single_node_leader(id: u64) -> Arc<CouncilNode> {
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(id, router.clone());
    let node = CouncilNode::new(
        id,
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
    router.register(id, node.raft().clone()).await;
    let members = BTreeMap::from([(
        id,
        CouncilNodeInfo::new(
            format!("127.0.0.1:91{id:02}").parse().unwrap(),
            format!("node-{id}"),
        ),
    )]);
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

pub(crate) async fn post_apply(
    router: &axum::Router,
    manifest: &str,
) -> (axum::http::StatusCode, String) {
    use tower::ServiceExt;
    let request = axum::http::Request::post("/v1/apply")
        .body(axum::body::Body::from(manifest.to_string()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn last_log_index(council: &CouncilNode) -> Option<u64> {
    council.metrics().borrow().last_log_index
}

#[tokio::test]
async fn a_denied_apply_leaves_no_raft_write_but_an_audit_entry() {
    let council = single_node_leader(1).await;
    let directory = tempfile::tempdir().unwrap();
    let log = AuditLog::open(
        directory.path().join("audit"),
        AuditLimits::from_total(1 << 20),
    )
    .unwrap();
    let mut events = EventStore::new();
    events.set_audit_log(log.clone());
    let (tx, _rx) = mpsc::channel(8);
    let router = reliaburger::bun::api::router(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(Arc::clone(&council)),
        None,
        None,
        None,
        None,
        None,
        0,
        Some(Arc::new(RwLock::new(events))),
    );

    let (status, body) = post_apply(&router, NO_LATEST).await;
    assert!(status.is_success(), "{status}: {body}");
    assert!(body.contains("policy no-latest: committed"), "{body}");

    let before = last_log_index(&council);
    let (status, body) = post_apply(&router, "[app.web]\nimage = 'web:latest'\n").await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "{body}"
    );
    assert!(
        body.contains("policy no-latest denies app default/web"),
        "{body}"
    );

    assert_eq!(last_log_index(&council), before, "a denial writes nothing");
    assert!(council.desired_state().await.apps.is_empty());
    let denied = log
        .query(AuditFilter {
            action: Some("policy.denied".to_string()),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0].details["policies"], "no-latest");
    council.shutdown().await.ok();
}

pub(crate) fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn a_gitops_commit_with_one_denied_app_applies_the_rest_and_names_the_policy() {
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(
        repository.path().join("cluster.toml"),
        format!("{NO_LATEST}\n[app.good]\nimage = 'good:v1'\n\n[app.bad]\nimage = 'bad:latest'\n"),
    )
    .unwrap();
    git(repository.path(), &["add", "."]);
    git(
        repository.path(),
        &["commit", "-q", "-m", "policies and apps"],
    );

    let council = single_node_leader(2).await;
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

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let error = loop {
        let state = council.desired_state().await;
        if let Some(error) = state
            .gitops_sync_state
            .as_ref()
            .and_then(|sync| sync.last_error.clone())
        {
            break error;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the sync never reported its partial result"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        error.contains("app.default/bad: denied by policy no-latest: pin a version"),
        "{error}"
    );
    let state = council.desired_state().await;
    assert!(state.apps.contains_key(&AppId::new("good", "default")));
    assert!(!state.apps.contains_key(&AppId::new("bad", "default")));
    assert!(state.gitops_owned.policies.contains("no-latest"));
    shutdown.cancel();
    council.shutdown().await.ok();
}

/// The protocol both apply paths follow: judge, write fenced, and on
/// `PolicyGenerationMoved` judge again. A stricter policy committed in
/// between refuses the app; the old judgement never lands.
#[tokio::test]
async fn a_policy_changed_mid_apply_makes_the_write_retry() {
    let council = single_node_leader(3).await;
    let caller = Caller::token("ci", ApiRole::Deployer);
    let config = Config::parse("[app.web]\nimage = 'web:latest'\n").unwrap();

    // Judged under no policies, at generation 0.
    let (policies, generation) = council
        .read_desired(|desired| (desired.policies.clone(), desired.policy_generation))
        .await;
    let admitted = admit(
        &policies,
        generation,
        &config,
        &caller,
        Source::Apply,
        config_to_desired_writes(&config),
    )
    .unwrap();

    // An admin commits a stricter policy before the app's write commits.
    for write in config_to_desired_writes(&Config::parse(NO_LATEST).unwrap()) {
        council.write(write).await.unwrap();
    }

    // The fenced write is refused, and nothing landed under the old set.
    let response = council.write(admitted.writes[0].clone()).await.unwrap();
    assert_eq!(
        response,
        CouncilResponse::PolicyGenerationMoved { current: 1 }
    );
    assert!(council.desired_state().await.apps.is_empty());

    // Judged again, under the new set, the apply is refused.
    let (policies, generation) = council
        .read_desired(|desired| (desired.policies.clone(), desired.policy_generation))
        .await;
    let error = admit(
        &policies,
        generation,
        &config,
        &caller,
        Source::Apply,
        config_to_desired_writes(&config),
    )
    .unwrap_err();
    assert!(matches!(error, AdmitError::Denied(_)), "{error}");

    // A pinned image passes the new set and lands on the new generation.
    let pinned = Config::parse("[app.web]\nimage = 'web:v1'\n").unwrap();
    let admitted = admit(
        &policies,
        generation,
        &pinned,
        &caller,
        Source::Apply,
        config_to_desired_writes(&pinned),
    )
    .unwrap();
    assert!(matches!(
        &admitted.writes[0],
        RaftRequest::PolicyFenced { generation: 1, .. }
    ));
    let response = council.write(admitted.writes[0].clone()).await.unwrap();
    assert!(
        !matches!(
            response,
            CouncilResponse::Refused { .. } | CouncilResponse::PolicyGenerationMoved { .. }
        ),
        "{response:?}"
    );
    assert!(
        council
            .desired_state()
            .await
            .apps
            .contains_key(&AppId::new("web", "default"))
    );
    council.shutdown().await.ok();
}

pub(crate) fn head(dir: &std::path::Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Wait for the sync of `sha` to be recorded, and return its diff summary.
pub(crate) async fn synced(
    council: &CouncilNode,
    sha: &str,
) -> reliaburger::lettuce::types::DiffSummary {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let state = council.desired_state().await;
        let entry = state.gitops_sync_state.as_ref().and_then(|sync| {
            sync.history
                .iter()
                .rev()
                .find(|entry| entry.commit.sha == sha)
                .cloned()
        });
        if let Some(entry) = entry {
            assert_eq!(
                entry.result,
                reliaburger::lettuce::types::SyncResult::Success,
                "{entry:?}"
            );
            return entry.diff_summary.expect("a successful sync has a diff");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "commit {sha} was never synced"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// #1118, the diff trap: a policy fills `env.TEAM` into an app git
/// declares without one. The cluster keeps the declared spec beside the
/// effective one, and GitOps diffs git against the declared one, so later
/// syncs see no change in that app and don't re-apply it.
#[tokio::test]
async fn repeated_syncs_of_a_defaulted_app_do_not_reapply_it() {
    const TEAM_LABEL: &str = r#"
[policy.team-label]
match = "app.namespace == 'default'"

[policy.team-label.default]
env.TEAM = "payments"
"#;
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init", "-q", "-b", "main"]);
    let manifest = repository.path().join("cluster.toml");
    std::fs::write(
        &manifest,
        format!("{TEAM_LABEL}\n[app.web]\nimage = 'web:v1'\n"),
    )
    .unwrap();
    git(repository.path(), &["add", "."]);
    git(
        repository.path(),
        &["commit", "-q", "-m", "a defaulted app"],
    );
    let first = head(repository.path());

    let council = single_node_leader(4).await;
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

    let summary = synced(&council, &first).await;
    assert_eq!(summary.added, 2, "the policy and the app: {summary:?}");
    let web = AppId::new("web", "default");
    let state = council.desired_state().await;
    assert!(
        state.apps[&web].env.contains_key("TEAM"),
        "the default is filled in"
    );
    assert!(
        state.declared_apps[&web].env.is_empty(),
        "git's spec is kept beside it"
    );
    let effective = state.apps[&web].clone();

    // An unrelated change: only the new app is added. Without the declared
    // spec, `web` would count as modified and be written again.
    std::fs::write(
        &manifest,
        format!("{TEAM_LABEL}\n[app.web]\nimage = 'web:v1'\n\n[app.other]\nimage = 'other:v1'\n"),
    )
    .unwrap();
    git(repository.path(), &["commit", "-q", "-am", "another app"]);
    let summary = synced(&council, &head(repository.path())).await;
    assert_eq!((summary.added, summary.modified), (1, 0), "{summary:?}");

    // A commit that changes nothing the cluster sees: no change at all.
    std::fs::write(
        &manifest,
        format!(
            "# the team label comes from policy\n{TEAM_LABEL}\n[app.web]\nimage = 'web:v1'\n\n[app.other]\nimage = 'other:v1'\n"
        ),
    )
    .unwrap();
    git(repository.path(), &["commit", "-q", "-am", "a comment"]);
    let summary = synced(&council, &head(repository.path())).await;
    assert_eq!(
        (summary.added, summary.modified, summary.removed),
        (0, 0, 0),
        "{summary:?}"
    );
    assert_eq!(council.desired_state().await.apps[&web], effective);
    shutdown.cancel();
    council.shutdown().await.ok();
}

/// #1119: a new policy never touches running work. An app committed before
/// the policy keeps its spec, and `GET /v1/policies/audit` (what `relish
/// policy audit` and `relish wtf` read) names it as something the policy
/// would now refuse.
#[tokio::test]
async fn a_new_policy_leaves_running_work_alone_and_the_audit_names_it() {
    use tower::ServiceExt;
    let council = single_node_leader(5).await;
    let (tx, _rx) = mpsc::channel(8);
    let router = reliaburger::bun::api::router(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(Arc::clone(&council)),
        None,
        None,
        None,
        None,
        None,
        0,
        None,
    );
    let (status, body) = post_apply(&router, "[app.web]\nimage = 'web:latest'\n").await;
    assert!(status.is_success(), "{status}: {body}");
    let web = AppId::new("web", "default");
    let before = council.desired_state().await.apps[&web].clone();
    let (status, body) = post_apply(&router, NO_LATEST).await;
    assert!(status.is_success(), "{status}: {body}");

    let request = axum::http::Request::get("/v1/policies/audit")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let audit: reliaburger::policy::audit::PolicyAudit = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(audit.verdict.denials.len(), 1, "{audit:?}");
    assert_eq!(
        audit.verdict.denials[0].to_string(),
        "policy no-latest denies app default/web: pin a version"
    );
    assert_eq!(council.desired_state().await.apps[&web], before);
    council.shutdown().await.ok();
}
