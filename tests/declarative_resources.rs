//! Acceptance test for 12b.2 T6 — "Complete declarative resources".
//!
//! One config containing every resource kind (app, job, namespace,
//! permission, build) must converge to the SAME Raft desired state
//! whether it's applied by hand (`relish apply`) or through a Lettuce
//! GitOps sync. Both paths funnel through
//! `council::config_to_desired_writes`, so this asserts byte-identical
//! desired state for the declarative kinds (apps, namespaces,
//! permissions). Jobs and builds aren't reconciled desired state and are
//! validated but not written.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::config::Config;
use reliaburger::council::config_to_desired_writes;
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, DesiredState};
use reliaburger::lettuce::runner::spawn_gitops_sync;
use reliaburger::lettuce::types::GitOpsConfig;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The one config every kind lives in. The namespace budget (8 CPUs)
/// comfortably fits the app (1 replica of the default request).
const EVERY_KIND: &str = r#"
[namespace.prod]
cpu = "8000m"
memory = "16Gi"
max_apps = 50
max_replicas = 200

[permission.deployer]
actions = ["deploy", "scale", "logs"]
apps = ["web"]
namespaces = ["prod"]

[app.web]
image = "web:v1"
namespace = "prod"
replicas = 1

[job.migrate]
image = "migrate:v1"

[build.img]
context = "."
destination = "pickle://prod/img:v1"
namespace = "prod"
"#;

fn fast_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 150,
        election_timeout_max_ms: 400,
        snapshot_threshold: 100,
        max_in_snapshot_log_to_keep: 50,
    }
}

async fn single_node_leader(id: u64) -> Arc<CouncilNode> {
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(id, router.clone());
    let node = CouncilNode::new(
        id,
        fast_config(),
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(id, node.raft().clone()).await;
    let mut members = BTreeMap::new();
    members.insert(
        id,
        CouncilNodeInfo::new(
            format!("127.0.0.1:90{id:02}").parse().unwrap(),
            format!("node-{id}"),
        ),
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

/// The declarative-kind slice of desired state, as canonical JSON. Two
/// syncs of the same config must produce byte-identical output here.
fn declarative_json(state: &DesiredState) -> String {
    // AppId isn't a JSON map key, so list name→image pairs sorted for a
    // deterministic ordering independent of HashMap iteration.
    let mut apps: Vec<(String, Option<String>)> = state
        .apps
        .iter()
        .map(|(id, spec)| (id.to_string(), spec.image.clone()))
        .collect();
    apps.sort();
    serde_json::to_string(&serde_json::json!({
        "apps": apps,
        "namespaces": state.namespaces,
        "permissions": state.permissions,
    }))
    .unwrap()
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

fn which_git() -> Option<()> {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| ())
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
async fn manual_apply_and_gitops_converge_identically() {
    assert!(
        which_git().is_some(),
        "git is required for the declarative-resource suite"
    );

    // The config parses and validates (the same gate both paths run).
    let config = Config::parse(EVERY_KIND).unwrap();
    config
        .validate()
        .expect("the every-kind config must validate");

    // --- Manual apply path ---------------------------------------------
    // `cluster_apply` writes exactly `config_to_desired_writes(&config)`;
    // driving those writes onto a leader is the manual apply, minus the
    // HTTP/SSE shell.
    let manual = single_node_leader(1).await;
    for request in config_to_desired_writes(&config) {
        manual.write(request).await.expect("manual apply write");
    }
    let manual_state = declarative_json(&manual.desired_state().await);

    // --- GitOps path ---------------------------------------------------
    let repo_dir = tempfile::tempdir().unwrap();
    git(repo_dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(repo_dir.path().join("cluster.toml"), EVERY_KIND).unwrap();
    git(repo_dir.path(), &["add", "."]);
    git(repo_dir.path(), &["commit", "-q", "-m", "initial"]);

    let gitops = single_node_leader(2).await;
    let shutdown = CancellationToken::new();
    let (_webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();
    let gitops_config = GitOpsConfig {
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
        Arc::clone(&gitops),
        gitops_config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
    );

    let target = manual_state.clone();
    let converged = wait_for(Duration::from_secs(20), || {
        let c = Arc::clone(&gitops);
        let target = target.clone();
        Box::pin(async move { declarative_json(&c.desired_state().await) == target })
    })
    .await;

    let gitops_state = declarative_json(&gitops.desired_state().await);
    assert!(
        converged,
        "gitops desired state must match manual apply byte-for-byte.\n\
         manual: {manual_state}\ngitops: {gitops_state}"
    );

    shutdown.cancel();
    manual.shutdown().await.ok();
    gitops.shutdown().await.ok();
}
