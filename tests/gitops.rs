//! Integration tests for the Lettuce GitOps sync loop (Stage 4 W10,
//! L13). A real on-disk git repo with an app TOML, a single-node
//! council, and the sync loop applying the repo's desired state to
//! Raft — the same AppSpec writes a manual `relish apply` makes.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
use reliaburger::lettuce::runner::spawn_gitops_sync;
use reliaburger::lettuce::types::GitOpsConfig;
use reliaburger::meat::types::AppId;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn fast_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 150,
        election_timeout_max_ms: 400,
        snapshot_threshold: 100,
        max_in_snapshot_log_to_keep: 50,
    }
}

/// A single-node council, initialised so it becomes leader.
async fn single_node_leader() -> Arc<CouncilNode> {
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let node = CouncilNode::new(
        1,
        fast_config(),
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
    // Wait for leadership.
    for _ in 0..40 {
        if node.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node
}

fn git(dir: &std::path::Path, args: &[&str]) {
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

/// Create a git repo containing `apps.toml`, return its path.
fn make_repo(dir: &std::path::Path, toml: &str) {
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("apps.toml"), toml).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "initial"]);
}

async fn wait_for<F>(timeout: Duration, mut cond: F) -> bool
where
    F: FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool>>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn sync_loop_applies_repo_apps_to_raft() {
    assert!(
        which_git().is_some(),
        "git is required for the GitOps suite"
    );

    let repo_dir = tempfile::tempdir().unwrap();
    make_repo(
        repo_dir.path(),
        r#"
        [app.fromgit]
        image = "git:v1"
        replicas = 2
    "#,
    );

    let council = single_node_leader().await;
    let shutdown = CancellationToken::new();
    let (_webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();

    let config = GitOpsConfig {
        repo: repo_dir.path().to_string_lossy().to_string(),
        branch: "main".to_string(),
        path: "/".to_string(),
        poll_interval_secs: 1,
        require_signed_commits: false,
        trusted_signing_keys: vec![],
        webhook_secret: None,
        recursive: false,
        webhook_rate_limit: 10,
    };
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
    );

    // The app from git should appear in Raft desired state.
    let council_check = Arc::clone(&council);
    let applied = wait_for(Duration::from_secs(15), || {
        let c = Arc::clone(&council_check);
        Box::pin(async move {
            c.desired_state()
                .await
                .apps
                .contains_key(&AppId::new("fromgit", "default"))
        })
    })
    .await;
    assert!(applied, "gitops sync never applied the repo's app to Raft");

    shutdown.cancel();
    council.shutdown().await.ok();
}

#[tokio::test]
async fn webhook_triggers_immediate_sync() {
    assert!(
        which_git().is_some(),
        "git is required for the GitOps suite"
    );

    let repo_dir = tempfile::tempdir().unwrap();
    make_repo(
        repo_dir.path(),
        r#"
        [app.baseline]
        image = "baseline:v1"
    "#,
    );

    let council = single_node_leader().await;
    let shutdown = CancellationToken::new();
    let (webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();

    // Long poll interval: only a webhook can make the sync happen quickly.
    let config = GitOpsConfig {
        repo: repo_dir.path().to_string_lossy().to_string(),
        branch: "main".to_string(),
        path: "/".to_string(),
        poll_interval_secs: 3600,
        require_signed_commits: false,
        trusted_signing_keys: vec![],
        webhook_secret: None,
        recursive: false,
        webhook_rate_limit: 10,
    };
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
    );

    // The runner always applies the first commit before waiting for a timer or
    // webhook, so wait for that baseline before creating the change we mean
    // to exercise.
    let council_check = Arc::clone(&council);
    let initial_sync = wait_for(Duration::from_secs(10), || {
        let c = Arc::clone(&council_check);
        Box::pin(async move {
            c.desired_state()
                .await
                .apps
                .contains_key(&AppId::new("baseline", "default"))
        })
    })
    .await;
    assert!(initial_sync, "initial GitOps sync did not complete");

    std::fs::write(
        repo_dir.path().join("apps.toml"),
        r#"
        [app.baseline]
        image = "baseline:v1"

        [app.hooked]
        image = "hook:v1"
    "#,
    )
    .unwrap();
    git(repo_dir.path(), &["add", "."]);
    git(repo_dir.path(), &["commit", "-q", "-m", "add hooked app"]);

    // The next timer tick is an hour away, so only this notification can
    // make the second commit visible during the test.
    webhook_tx.send(()).await.unwrap();

    let applied = wait_for(Duration::from_secs(10), || {
        let c = Arc::clone(&council_check);
        Box::pin(async move {
            c.desired_state()
                .await
                .apps
                .contains_key(&AppId::new("hooked", "default"))
        })
    })
    .await;
    assert!(
        applied,
        "webhook did not trigger a sync well before the poll interval"
    );

    shutdown.cancel();
    council.shutdown().await.ok();
}

/// A repo with an app, a job, a namespace and a permission syncs every
/// declarative kind through to Raft desired state (12b.2 T6). Before this
/// theme, `resource_change_to_request` returned `None` for anything but an
/// app, so namespaces and permissions were silently dropped.
#[tokio::test]
async fn sync_loop_applies_every_declarative_kind() {
    assert!(
        which_git().is_some(),
        "git is required for the GitOps suite"
    );

    let repo_dir = tempfile::tempdir().unwrap();
    make_repo(
        repo_dir.path(),
        r#"
        [namespace.prod]
        cpu = "8000m"
        max_apps = 50

        [permission.deployer]
        actions = ["deploy", "scale"]
        namespaces = ["prod"]

        [app.web]
        image = "web:v1"
        namespace = "prod"

        [job.migrate]
        image = "migrate:v1"
    "#,
    );

    let council = single_node_leader().await;
    let shutdown = CancellationToken::new();
    let (_webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();

    let config = GitOpsConfig {
        repo: repo_dir.path().to_string_lossy().to_string(),
        branch: "main".to_string(),
        path: "/".to_string(),
        poll_interval_secs: 1,
        require_signed_commits: false,
        trusted_signing_keys: vec![],
        webhook_secret: None,
        recursive: false,
        webhook_rate_limit: 10,
    };
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
    );

    let council_check = Arc::clone(&council);
    let converged = wait_for(Duration::from_secs(15), || {
        let c = Arc::clone(&council_check);
        Box::pin(async move {
            let state = c.desired_state().await;
            state.namespaces.contains_key("prod")
                && state.permissions.contains_key("deployer")
                && state.apps.contains_key(&AppId::new("web", "prod"))
        })
    })
    .await;
    assert!(
        converged,
        "gitops sync must apply namespace, permission and app to Raft"
    );

    // The commit advances only once everything committed (D12): a set
    // that fully applied records last_applied_commit.
    let sync_state = council.desired_state().await.gitops_sync_state;
    assert!(
        sync_state.and_then(|s| s.last_applied_commit).is_some(),
        "a fully-applied sync must advance last_applied_commit"
    );

    shutdown.cancel();
    council.shutdown().await.ok();
}

/// D12 atomicity: when a write in the change set fails, `apply_changes`
/// stops and reports the failure so the caller does NOT advance
/// `last_applied_commit`. A council that was never initialised isn't the
/// leader, so every `write` is refused — a faithful stand-in for a mid-
/// sync failure. Before the fix, the runner advanced the commit
/// regardless, marking a failed write "applied" so it never retried.
#[tokio::test]
async fn apply_changes_stops_and_reports_on_write_failure() {
    use reliaburger::lettuce::diff::{ChangePayload, ResourceChange};
    use reliaburger::lettuce::runner::apply_changes;

    // An uninitialised node: no leader, so client writes are refused.
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let node = CouncilNode::new(
        1,
        fast_config(),
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    let node = Arc::new(node);
    assert!(!node.is_leader().await, "node must not be leader");

    let spec = reliaburger::config::Config::parse("[app.web]\nimage = \"x:1\"\n")
        .unwrap()
        .app
        .remove("web")
        .unwrap();
    let changes = vec![ResourceChange::Add {
        resource_id: "app.default/web".to_string(),
        spec: ChangePayload::App(Box::new(spec)),
    }];

    let result = apply_changes(&node, &changes).await;
    assert!(
        matches!(result, Err(ref id) if id == "app.default/web"),
        "a failed write must be reported, not swallowed: {result:?}"
    );
    // And the app never reached desired state.
    assert!(node.desired_state().await.apps.is_empty());
    node.shutdown().await.ok();
}

/// GIT2: a `prod/web` removed from git deletes `prod/web`, and a
/// same-named `default/web` that git never mentioned is untouched. Before
/// the fix the diff keyed on the bare name, so a `prod` deletion could
/// take out `default/web` (or spare the wrong app).
#[tokio::test]
async fn sync_deletes_the_namespaced_app_not_the_default_one() {
    use reliaburger::config::Config;
    use reliaburger::council::types::RaftRequest;

    assert!(
        which_git().is_some(),
        "git is required for the GitOps suite"
    );

    let council = single_node_leader().await;

    // Seed two same-named apps in different namespaces directly into Raft,
    // as if a previous sync (or manual apply) had created them.
    for namespace in ["prod", "default"] {
        let spec = Config::parse("[app.web]\nimage = \"web:v1\"\n")
            .unwrap()
            .app
            .remove("web")
            .unwrap();
        council
            .write(RaftRequest::AppSpec {
                app_id: AppId::new("web", namespace),
                spec: Box::new(spec),
            })
            .await
            .unwrap();
    }

    // Git declares only default/web, so prod/web must be reconciled away.
    let repo_dir = tempfile::tempdir().unwrap();
    make_repo(
        repo_dir.path(),
        r#"
        [app.web]
        image = "web:v1"
    "#,
    );

    let shutdown = CancellationToken::new();
    let (_webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();
    let config = GitOpsConfig {
        repo: repo_dir.path().to_string_lossy().to_string(),
        branch: "main".to_string(),
        path: "/".to_string(),
        poll_interval_secs: 1,
        require_signed_commits: false,
        trusted_signing_keys: vec![],
        webhook_secret: None,
        recursive: false,
        webhook_rate_limit: 10,
    };
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
    );

    let council_check = Arc::clone(&council);
    let converged = wait_for(Duration::from_secs(15), || {
        let c = Arc::clone(&council_check);
        Box::pin(async move {
            let apps = c.desired_state().await.apps;
            // prod/web gone, default/web still present.
            !apps.contains_key(&AppId::new("web", "prod"))
                && apps.contains_key(&AppId::new("web", "default"))
        })
    })
    .await;
    assert!(
        converged,
        "gitops must delete prod/web and keep default/web"
    );

    shutdown.cancel();
    council.shutdown().await.ok();
}

/// GIT4 durable errors: a sync that can't reach its repo records the
/// failure in `SyncState` (last_error + a non-zero failure count) so
/// relish and the UI can see a broken sync, not just stderr.
#[tokio::test]
async fn a_failed_sync_is_recorded_in_sync_state() {
    assert!(
        which_git().is_some(),
        "git is required for the GitOps suite"
    );

    let council = single_node_leader().await;
    let shutdown = CancellationToken::new();
    let (_webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();

    // A repo path that doesn't exist: the clone fails on every attempt.
    let config = GitOpsConfig {
        repo: "/nonexistent/repo/does/not/exist.git".to_string(),
        branch: "main".to_string(),
        path: "/".to_string(),
        poll_interval_secs: 1,
        require_signed_commits: false,
        trusted_signing_keys: vec![],
        webhook_secret: None,
        recursive: false,
        webhook_rate_limit: 10,
    };
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
    );

    let council_check = Arc::clone(&council);
    let recorded = wait_for(Duration::from_secs(15), || {
        let c = Arc::clone(&council_check);
        Box::pin(async move {
            match c.desired_state().await.gitops_sync_state {
                Some(state) => state.last_error.is_some() && state.consecutive_failures > 0,
                None => false,
            }
        })
    })
    .await;
    assert!(
        recorded,
        "a hard sync failure must be durable in SyncState, not stderr-only"
    );

    shutdown.cancel();
    council.shutdown().await.ok();
}

fn which_git() -> Option<()> {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| ())
}
