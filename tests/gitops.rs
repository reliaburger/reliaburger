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
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn sync_loop_applies_repo_apps_to_raft() {
    if which_git().is_none() {
        eprintln!("git not on PATH; skipping");
        return;
    }

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
    if which_git().is_none() {
        eprintln!("git not on PATH; skipping");
        return;
    }

    let repo_dir = tempfile::tempdir().unwrap();
    make_repo(
        repo_dir.path(),
        r#"
        [app.hooked]
        image = "hook:v1"
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

    // Nudge via the webhook channel.
    tokio::time::sleep(Duration::from_millis(200)).await;
    webhook_tx.send(()).await.unwrap();

    let council_check = Arc::clone(&council);
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

fn which_git() -> Option<()> {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| ())
}
