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
/// Track `main` at the repository root, unsigned and without a webhook secret.
fn repo_config(repo: &str, poll_interval_secs: u64) -> GitOpsConfig {
    GitOpsConfig {
        repo: repo.to_string(),
        branch: "main".to_string(),
        path: "/".to_string(),
        poll_interval_secs,
        require_signed_commits: false,
        trusted_signing_keys: vec![],
        webhook_secret: None,
        webhook_rate_limit: 10,
    }
}

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

    let config = repo_config(&repo_dir.path().to_string_lossy(), 1);
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
        None,
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
    let config = repo_config(&repo_dir.path().to_string_lossy(), 3600);
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
        None,
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

/// A repo with an app, a namespace and a permission syncs every supported
/// declarative kind through to Raft desired state (12b.2 T6). Before this
/// theme, `resource_change_to_request` returned `None` for anything but an
/// app, so namespaces and permissions were silently dropped.
#[tokio::test]
async fn sync_loop_applies_every_supported_declarative_kind() {
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

    "#,
    );

    let council = single_node_leader().await;
    let shutdown = CancellationToken::new();
    let (_webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();

    let config = repo_config(&repo_dir.path().to_string_lossy(), 1);
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
        None,
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

    let result = apply_changes(&node, &changes, None).await;
    assert!(
        matches!(result, Err(ref id) if id == "app.default/web"),
        "a failed write must be reported, not swallowed: {result:?}"
    );
    // And the app never reached desired state.
    assert!(node.desired_state().await.apps.is_empty());
    node.shutdown().await.ok();
}

/// B15: a write the state machine commits but *refuses* (here an app in an
/// `rbtest-*` test-lease namespace, which only a leased write may create)
/// is a failure, not an applied change. Before the fix `apply_changes`
/// checked only the outer `Err`, so the refused write counted as applied
/// and the runner advanced `last_applied_commit` past a commit that never
/// reached desired state.
#[tokio::test]
async fn apply_changes_reports_a_refused_write_as_a_failure() {
    use reliaburger::lettuce::diff::{ChangePayload, ResourceChange};
    use reliaburger::lettuce::runner::apply_changes;

    let council = single_node_leader().await;
    assert!(council.is_leader().await, "node must be leader");

    let spec = reliaburger::config::Config::parse("[app.web]\nimage = \"x:1\"\n")
        .unwrap()
        .app
        .remove("web")
        .unwrap();
    let changes = vec![ResourceChange::Add {
        resource_id: "app.rbtest-lease/web".to_string(),
        spec: ChangePayload::App(Box::new(spec)),
    }];

    let result = apply_changes(&council, &changes, None).await;
    assert!(
        matches!(result, Err(ref id) if id == "app.rbtest-lease/web"),
        "a refused write must be reported as unapplied: {result:?}"
    );
    assert!(council.desired_state().await.apps.is_empty());
    council.shutdown().await.ok();
}

/// An upstream registry that names one digest for every tag, or is down.
struct FixedRegistry(Option<reliaburger::pickle::types::Digest>);

impl reliaburger::pickle::upstream::UpstreamRegistry for FixedRegistry {
    fn head_manifest_digest<'a>(
        &'a self,
        _image: &'a reliaburger::grill::image::ImageReference,
    ) -> reliaburger::pickle::upstream::UpstreamFuture<'a, reliaburger::pickle::types::Digest> {
        let answer = self.0.clone().ok_or_else(|| {
            reliaburger::pickle::types::PickleError::ReplicationFailed("connection refused".into())
        });
        Box::pin(async move { answer })
    }

    fn fetch_manifest<'a>(
        &'a self,
        _image: &'a reliaburger::grill::image::ImageReference,
    ) -> reliaburger::pickle::upstream::UpstreamFuture<
        'a,
        reliaburger::pickle::upstream::UpstreamManifest,
    > {
        unimplemented!("binding only asks for the digest")
    }

    fn fetch_root<'a>(
        &'a self,
        _image: &'a reliaburger::grill::image::ImageReference,
    ) -> reliaburger::pickle::upstream::UpstreamFuture<
        'a,
        reliaburger::pickle::upstream::UpstreamRoot,
    > {
        unimplemented!("binding only asks for the digest")
    }

    fn fetch_blob<'a>(
        &'a self,
        _image: &'a reliaburger::grill::image::ImageReference,
        _layer: &'a reliaburger::pickle::types::LayerDescriptor,
    ) -> reliaburger::pickle::upstream::UpstreamFuture<'a, Vec<u8>> {
        unimplemented!("binding only asks for the digest")
    }
}

fn web_change(image: &str) -> reliaburger::lettuce::diff::ResourceChange {
    let spec = reliaburger::config::Config::parse(&format!("[app.web]\nimage = \"{image}\"\n"))
        .unwrap()
        .app
        .remove("web")
        .unwrap();
    reliaburger::lettuce::diff::ResourceChange::Add {
        resource_id: "app.default/web".to_string(),
        spec: reliaburger::lettuce::diff::ChangePayload::App(Box::new(spec)),
    }
}

/// F03 U1: GitOps writes the same bound reference a manual apply does.
#[tokio::test]
async fn apply_changes_binds_an_apps_image_to_a_digest() {
    use reliaburger::pickle::binding::ImageBinder;

    let council = single_node_leader().await;
    let digest =
        reliaburger::pickle::types::Digest::new(&format!("sha256:{}", "7".repeat(64))).unwrap();
    let binder = ImageBinder::with_upstream(Arc::new(FixedRegistry(Some(digest.clone()))));

    let result = reliaburger::lettuce::runner::apply_changes(
        &council,
        &[web_change("nginx:1.27")],
        Some(&binder),
    )
    .await;

    assert_eq!(result.unwrap(), 1);
    let image = council.desired_state().await.apps[&AppId::new("web", "default")]
        .image
        .clone();
    assert_eq!(image, Some(format!("nginx:1.27@{}", digest.as_str())));
    council.shutdown().await.ok();
}

/// With the registry down and nothing cached, the change fails and names
/// the image, so the commit isn't advanced and the next poll retries.
#[tokio::test]
async fn apply_changes_fails_a_change_whose_image_cannot_be_bound() {
    use reliaburger::pickle::binding::ImageBinder;

    let council = single_node_leader().await;
    let binder = ImageBinder::with_upstream(Arc::new(FixedRegistry(None)));

    let result = reliaburger::lettuce::runner::apply_changes(
        &council,
        &[web_change("nginx:1.27")],
        Some(&binder),
    )
    .await;

    assert!(
        matches!(&result, Err(message) if message.starts_with("app.default/web") && message.contains("nginx:1.27")),
        "{result:?}"
    );
    assert!(council.desired_state().await.apps.is_empty());
    council.shutdown().await.ok();
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
    let config = repo_config(&repo_dir.path().to_string_lossy(), 1);
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
        None,
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
    let config = repo_config("/nonexistent/repo/does/not/exist.git", 1);
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
        None,
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

/// B16: after a sync, manual changes (a different image, a deleted app)
/// are repaired by the next poll even though Git hasn't moved. The old
/// loop skipped every poll whose HEAD equalled the last applied commit,
/// so drift stood until the next commit. An autoscale override is not
/// drift and survives the same polls.
#[tokio::test]
async fn an_unchanged_commit_still_repairs_manual_drift() {
    use reliaburger::config::Config;
    use reliaburger::council::types::RaftRequest;

    assert!(
        which_git().is_some(),
        "git is required for the GitOps suite"
    );

    let repo_dir = tempfile::tempdir().unwrap();
    make_repo(
        repo_dir.path(),
        r#"
        [app.web]
        image = "web:v1"

        [app.api]
        image = "api:v1"

        [app.worker]
        image = "worker:v1"
        replicas = 2
    "#,
    );

    let council = single_node_leader().await;
    let shutdown = CancellationToken::new();
    let (_webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();
    let config = repo_config(&repo_dir.path().to_string_lossy(), 1);
    spawn_gitops_sync(
        Arc::clone(&council),
        config,
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
        None,
    );

    let web = AppId::new("web", "default");
    let api = AppId::new("api", "default");
    let worker = AppId::new("worker", "default");
    let council_check = Arc::clone(&council);
    let synced = wait_for(Duration::from_secs(15), || {
        let c = Arc::clone(&council_check);
        Box::pin(async move {
            c.desired_state()
                .await
                .gitops_sync_state
                .is_some_and(|s| s.last_applied_commit.is_some())
        })
    })
    .await;
    assert!(synced, "the initial sync never completed");
    let applied_sha = council
        .desired_state()
        .await
        .gitops_sync_state
        .and_then(|s| s.last_applied_commit)
        .map(|c| c.sha);

    // Manual changes through the same Raft writes `relish apply`,
    // `relish delete` and the autoscaler make.
    let rogue = Config::parse("[app.web]\nimage = \"web:rogue\"\n")
        .unwrap()
        .app
        .remove("web")
        .unwrap();
    council
        .write(RaftRequest::AppSpec {
            app_id: web.clone(),
            spec: Box::new(rogue),
        })
        .await
        .unwrap();
    council
        .write(RaftRequest::AppDelete {
            app_id: api.clone(),
        })
        .await
        .unwrap();
    council
        .write(RaftRequest::AutoscaleOverride {
            app_id: worker.clone(),
            replicas: 5,
            reason: "load".to_string(),
        })
        .await
        .unwrap();

    let repaired = wait_for(Duration::from_secs(15), || {
        let c = Arc::clone(&council_check);
        let web = web.clone();
        let api = api.clone();
        Box::pin(async move {
            let state = c.desired_state().await;
            state
                .apps
                .get(&web)
                .is_some_and(|spec| spec.image.as_deref() == Some("web:v1"))
                && state.apps.contains_key(&api)
        })
    })
    .await;
    assert!(
        repaired,
        "polls with an unchanged commit must converge the cluster back to Git"
    );

    let state = council.desired_state().await;
    assert_eq!(
        state
            .gitops_sync_state
            .and_then(|s| s.last_applied_commit)
            .map(|c| c.sha),
        applied_sha,
        "Git never moved, so the applied commit must not either"
    );
    assert!(
        state
            .autoscale_overrides
            .iter()
            .any(|(key, replicas)| key == &worker.to_string() && *replicas == 5),
        "an autoscale override is not drift and must survive: {:?}",
        state.autoscale_overrides
    );

    shutdown.cancel();
    council.shutdown().await.ok();
}

/// B19: shutdown during a stuck `git fetch` ends the sync loop promptly
/// and kills the fetch, rather than waiting for the remote.
#[tokio::test]
async fn shutdown_during_a_stuck_fetch_stops_the_loop_promptly() {
    assert!(
        which_git().is_some(),
        "git is required for the GitOps suite"
    );

    let repo_dir = tempfile::tempdir().unwrap();
    make_repo(repo_dir.path(), "[app.web]\nimage = \"web:v1\"\n");
    let url = repo_dir.path().to_string_lossy().to_string();

    // Pre-seed the loop's clone and make every fetch from it hang: the
    // upload-pack records its pid and then sleeps. The trailing `#`
    // comments out the repository path git appends.
    let data_dir = tempfile::tempdir().unwrap();
    let clone = data_dir.path().join("gitops-repo");
    reliaburger::lettuce::git::GitRepo::clone_or_open(&url, &clone, "main").unwrap();
    let pid_file = data_dir.path().join("upload-pack.pid");
    // The shell creates a redirection's target before it writes to it, so
    // the pid goes to a temporary name and is renamed into place: the pid
    // file never exists half-written (#461).
    let hang = format!(
        "echo $$ > {pid}.tmp && mv {pid}.tmp {pid}; exec sleep 300 #",
        pid = pid_file.display()
    );
    git(&clone, &["config", "remote.origin.uploadpack", &hang]);

    let council = single_node_leader().await;
    let shutdown = CancellationToken::new();
    let (_webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let handle = spawn_gitops_sync(
        Arc::clone(&council),
        repo_config(&url, 1),
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
        None,
    );

    let stuck = wait_for(Duration::from_secs(15), || {
        let pid_file = pid_file.clone();
        Box::pin(async move { read_pid(&pid_file).is_some() })
    })
    .await;
    assert!(stuck, "the sync never reached the hanging fetch");
    let pid = read_pid(&pid_file).expect("the pid file was complete a moment ago");

    shutdown.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(
        stopped.is_ok(),
        "the sync loop kept waiting on a stuck fetch after shutdown"
    );
    // SIGKILL lands asynchronously: the process can still show as running
    // for a moment after the loop returns, so wait (bounded) for it to go
    // rather than taking one look.
    let gone = wait_for(Duration::from_secs(5), || {
        Box::pin(async move { !is_running(pid) })
    })
    .await;
    assert!(
        gone,
        "the stuck upload-pack is still running ({})",
        process_state(pid)
    );

    council.shutdown().await.ok();
}

/// The pid in `path`, once the file exists and holds a whole pid.
fn read_pid(path: &std::path::Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// `ps`'s state letters for `pid`, empty once the process is gone.
fn process_state(pid: i32) -> String {
    let ps = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&ps.stdout).trim().to_string()
}

/// Whether `pid` names a live process; a zombie has already been killed.
fn is_running(pid: i32) -> bool {
    let state = process_state(pid);
    !state.is_empty() && !state.starts_with('Z')
}

fn which_git() -> Option<()> {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| ())
}

#[tokio::test]
async fn job_refusal_keeps_the_last_applied_commit_and_all_desired_resources() {
    let repo_dir = tempfile::tempdir().unwrap();
    make_repo(repo_dir.path(), "[app.web]\nimage = 'web:v1'\n");
    let council = single_node_leader().await;
    let shutdown = CancellationToken::new();
    let (webhook_tx, webhook_rx) = mpsc::channel::<()>(4);
    let data_dir = tempfile::tempdir().unwrap();
    spawn_gitops_sync(
        Arc::clone(&council),
        repo_config(&repo_dir.path().to_string_lossy(), 1),
        webhook_rx,
        data_dir.path().to_path_buf(),
        shutdown.clone(),
        None,
    );
    let check = Arc::clone(&council);
    assert!(
        wait_for(Duration::from_secs(15), || {
            let c = Arc::clone(&check);
            Box::pin(async move {
                c.desired_state()
                    .await
                    .gitops_sync_state
                    .is_some_and(|s| s.last_applied_commit.is_some())
            })
        })
        .await,
        "initial supported app sync did not finish"
    );
    let before = council.desired_state().await;
    let applied = before
        .gitops_sync_state
        .as_ref()
        .unwrap()
        .last_applied_commit
        .as_ref()
        .unwrap()
        .sha
        .clone();
    std::fs::write(repo_dir.path().join("apps.toml"), "[app.web]\nimage = 'web:v2'\n[job.migrate]\nimage = 'migrate:v1'\nrun_before = ['app.web']\n[namespace.new-team]\nmax_apps = 10\n[permission.new-deployer]\nactions = ['deploy']\nnamespaces = ['new-team']\n").unwrap();
    git(repo_dir.path(), &["add", "."]);
    git(
        repo_dir.path(),
        &["commit", "-q", "-m", "unsupported migration"],
    );
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let rejected = String::from_utf8(output.stdout).unwrap().trim().to_string();
    webhook_tx.send(()).await.unwrap();
    let check = Arc::clone(&council);
    let attempted = wait_for(Duration::from_secs(15), || {
        let c = Arc::clone(&check);
        let rejected = rejected.clone();
        Box::pin(async move {
            c.desired_state().await.gitops_sync_state.is_some_and(|s| {
                s.last_error.is_some()
                    || s.last_applied_commit
                        .is_some_and(|commit| commit.sha == rejected)
            })
        })
    })
    .await;
    shutdown.cancel();
    let after = council.desired_state().await;
    council.shutdown().await.ok();
    assert!(attempted, "candidate sync never completed or refused");
    let state = after.gitops_sync_state.unwrap();
    assert!(
        state
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("GitOps does not reconcile jobs")
                && error.contains("job.migrate")),
        "job refusal was not recorded: {:?}",
        state.last_error
    );
    assert_eq!(state.last_applied_commit.unwrap().sha, applied);
    assert_eq!(
        after.apps, before.apps,
        "app revision escaped before migration refusal"
    );
    assert_eq!(after.namespaces, before.namespaces);
    assert_eq!(after.permissions, before.permissions);
    assert!(
        state
            .history
            .iter()
            .any(|entry| entry.commit.sha == rejected
                && matches!(
                    entry.result,
                    reliaburger::lettuce::types::SyncResult::Failure { .. }
                )),
        "refused commit missing from history"
    );
}
