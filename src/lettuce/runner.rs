//! The GitOps sync runner (Stage 4 W10, L13).
//!
//! Ties the pure `execute_sync` logic to the async runtime and Raft:
//! a leader-only task that clones the configured repo, syncs on a poll
//! timer or a webhook nudge, and applies the resulting `ResourceChange`s
//! to Raft desired state — the same `AppSpec`/`AppDelete` writes a
//! manual `relish apply` makes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::council::node::CouncilNode;
use crate::council::types::RaftRequest;
use crate::meat::types::AppId;

use super::diff::{ChangePayload, ResourceChange};
use super::git::GitRepo;
use super::sync::execute_sync;
use super::types::{GitOpsConfig, SyncResult};

/// Spawn the leader-only GitOps sync loop.
///
/// Syncs when the poll timer fires or a webhook arrives (whichever
/// comes first). Non-leaders idle; the git clone and sync run in
/// `spawn_blocking` because `GitRepo` shells out to `git`.
pub fn spawn_gitops_sync(
    council: Arc<CouncilNode>,
    config: GitOpsConfig,
    mut webhook_rx: mpsc::Receiver<()>,
    data_dir: PathBuf,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let repo_dir = data_dir.join("gitops-repo");
        let poll = Duration::from_secs(config.poll_interval_secs.max(1));
        let mut ticker = tokio::time::interval(poll);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
                signal = webhook_rx.recv() => {
                    if signal.is_none() {
                        break; // sender dropped
                    }
                    // Drain any queued webhook signals so a burst
                    // collapses into a single sync.
                    while webhook_rx.try_recv().is_ok() {}
                }
            }

            if !council.is_leader().await {
                continue;
            }

            let desired = council.desired_state().await;
            let current_apps = desired.apps.clone();
            let overrides = desired.autoscale_overrides.clone();
            let last_sha = desired
                .gitops_sync_state
                .as_ref()
                .and_then(|s| s.last_applied_commit.as_ref())
                .map(|c| c.sha.clone());

            // Git operations shell out — keep them off the runtime.
            let config_clone = config.clone();
            let repo_dir_clone = repo_dir.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let repo = GitRepo::clone_or_open(
                    &config_clone.repo,
                    &repo_dir_clone,
                    &config_clone.branch,
                )?;
                Ok::<_, super::types::LettuceError>(execute_sync(
                    &repo,
                    &config_clone,
                    &current_apps,
                    &overrides,
                    last_sha.as_deref(),
                ))
            })
            .await;

            let outcome = match outcome {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(e)) => {
                    eprintln!("gitops: repo access failed: {e}");
                    continue;
                }
                Err(e) => {
                    eprintln!("gitops: sync task panicked: {e}");
                    continue;
                }
            };

            // Skipped (HEAD unchanged) and hard failures: nothing to apply.
            match &outcome.result {
                SyncResult::Skipped { .. } => continue,
                SyncResult::Failure { error } => {
                    eprintln!("gitops: sync failed: {error}");
                    continue;
                }
                SyncResult::Success | SyncResult::PartialSuccess { .. } => {}
            }

            // Apply each change through the standard desired-state path.
            let mut applied = 0usize;
            for change in &outcome.changes {
                let request = match change {
                    ResourceChange::Add { resource_id, spec }
                    | ResourceChange::Update {
                        resource_id, spec, ..
                    } => match resource_change_to_request(resource_id, spec) {
                        Some(req) => req,
                        None => continue,
                    },
                    ResourceChange::Remove { resource_id } => {
                        let Some(app_id) = app_id_from_resource(resource_id) else {
                            continue;
                        };
                        RaftRequest::AppDelete { app_id }
                    }
                };
                if let Err(e) = council.write(request).await {
                    eprintln!("gitops: failed to apply {}: {e}", change_id(change));
                } else {
                    applied += 1;
                }
            }

            // Record the sync state (last applied commit → last_sha).
            if let Some(commit) = &outcome.commit {
                let mut sync_state = desired.gitops_sync_state.clone().unwrap_or_default();
                sync_state.last_applied_commit = Some(commit.clone());
                sync_state.last_fetched_commit = Some(commit.clone());
                if let Err(e) = council
                    .write(RaftRequest::GitOpsSyncUpdate(Box::new(sync_state)))
                    .await
                {
                    eprintln!("gitops: failed to record sync state: {e}");
                }
            }

            if applied > 0 {
                println!(
                    "gitops: applied {applied} change(s) from {}",
                    outcome
                        .commit
                        .as_ref()
                        .map(|c| c.sha.as_str())
                        .unwrap_or("?")
                );
            }
        }
    });
}

/// `"app.web"` → the `AppSpec` write, if the payload is an app.
fn resource_change_to_request(resource_id: &str, payload: &ChangePayload) -> Option<RaftRequest> {
    match payload {
        ChangePayload::App(spec) => {
            let app_id = app_id_from_resource(resource_id)?;
            Some(RaftRequest::AppSpec {
                app_id,
                spec: spec.clone(),
            })
        }
        // Jobs/namespaces/permissions aren't cluster-scheduled desired
        // state yet; skip rather than mis-apply.
        ChangePayload::Generic => None,
    }
}

/// Parse `"app.<name>"` into an `AppId` in the default namespace.
fn app_id_from_resource(resource_id: &str) -> Option<AppId> {
    let name = resource_id.strip_prefix("app.")?;
    Some(AppId::new(name, "default"))
}

fn change_id(change: &ResourceChange) -> &str {
    match change {
        ResourceChange::Add { resource_id, .. }
        | ResourceChange::Update { resource_id, .. }
        | ResourceChange::Remove { resource_id } => resource_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_id_parses_app_prefix() {
        assert_eq!(
            app_id_from_resource("app.web"),
            Some(AppId::new("web", "default"))
        );
        assert!(app_id_from_resource("job.migrate").is_none());
    }

    #[test]
    fn app_payload_becomes_an_appspec_write() {
        let spec = crate::config::Config::parse("[app.web]\nimage = \"x:1\"\n")
            .unwrap()
            .app
            .remove("web")
            .unwrap();
        let req = resource_change_to_request("app.web", &ChangePayload::App(Box::new(spec)));
        assert!(matches!(req, Some(RaftRequest::AppSpec { .. })));

        // Generic payloads (jobs etc.) are skipped, not mis-applied.
        assert!(resource_change_to_request("job.x", &ChangePayload::Generic).is_none());
    }
}
