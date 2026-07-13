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
            let current_namespaces = desired.namespaces.clone();
            let current_permissions = desired.permissions.clone();
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
                    &current_namespaces,
                    &current_permissions,
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

            // Apply each change through the SAME desired-state writes a
            // manual `relish apply` makes (12b.2 T6). Every kind now maps
            // to a request — apps, namespaces, permissions — so nothing is
            // silently skipped the way a `None` used to drop jobs and
            // namespaces on the floor.
            //
            // Atomicity (D12): `last_applied_commit` advances only if
            // EVERY write in the sync succeeds. The old code advanced the
            // commit regardless of per-change failures, so a failed write
            // was marked "applied" and never retried — the resource just
            // vanished until the next unrelated commit. Now a failure
            // leaves the commit unadvanced, and the next tick re-applies
            // the whole set. Writes are idempotent (spec upsert / delete),
            // so re-applying an already-committed change is a harmless
            // no-op.
            let applied = match apply_changes(&council, &outcome.changes).await {
                Ok(applied) => applied,
                Err(unapplied) => {
                    eprintln!(
                        "gitops: sync of {} failed at {unapplied}; commit not advanced, \
                         will retry next tick",
                        outcome
                            .commit
                            .as_ref()
                            .map(|c| c.sha.as_str())
                            .unwrap_or("?")
                    );
                    continue;
                }
            };

            // Record the sync state (last applied commit → last_sha) only
            // after every change committed cleanly.
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

/// Apply a sync's changes to Raft, stopping at the first failure.
///
/// Returns `Ok(count)` with the number of writes committed when every
/// change applied, or `Err(resource_id)` naming the change that failed.
/// The caller must advance `last_applied_commit` only on `Ok` — that's
/// the D12 atomicity guarantee: a half-applied sync leaves the commit
/// unadvanced so the next tick re-applies the whole (idempotent) set.
pub async fn apply_changes(
    council: &CouncilNode,
    changes: &[ResourceChange],
) -> Result<usize, String> {
    let mut applied = 0usize;
    for change in changes {
        let Some(request) = change_to_request(change) else {
            continue; // jobs/builds: not reconciled desired state
        };
        if let Err(e) = council.write(request).await {
            eprintln!("gitops: failed to apply {}: {e}", change_id(change));
            return Err(change_id(change).to_string());
        }
        applied += 1;
    }
    Ok(applied)
}

/// Map a diff `ResourceChange` to the Raft write that realises it.
///
/// Returns `None` for kinds that aren't reconciled desired state (jobs,
/// builds) — those carry a `Generic` payload, so their add/update yields
/// nothing to write. Every declarative kind (app, namespace, permission)
/// maps to a real write for both add/update and remove.
fn change_to_request(change: &ResourceChange) -> Option<RaftRequest> {
    match change {
        ResourceChange::Add { resource_id, spec }
        | ResourceChange::Update {
            resource_id, spec, ..
        } => payload_to_request(resource_id, spec),
        ResourceChange::Remove { resource_id } => remove_to_request(resource_id),
    }
}

/// An add/update payload → its `Spec` write.
fn payload_to_request(resource_id: &str, payload: &ChangePayload) -> Option<RaftRequest> {
    match payload {
        ChangePayload::App(spec) => {
            // Key on the spec's own namespace, exactly as
            // `config_to_desired_writes` does — not a hardcoded `default`.
            // Otherwise a `namespace = "prod"` app lands under `default`
            // via GitOps but `prod` via manual apply: the two paths would
            // diverge (caught by the T6 acceptance test).
            let name = resource_id.strip_prefix("app.")?;
            let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
            Some(RaftRequest::AppSpec {
                app_id: AppId::new(name, &namespace),
                spec: spec.clone(),
            })
        }
        ChangePayload::Namespace(spec) => Some(RaftRequest::NamespaceSpec {
            name: resource_id.strip_prefix("namespace.")?.to_string(),
            spec: spec.clone(),
        }),
        ChangePayload::Permission(spec) => Some(RaftRequest::PermissionSpec {
            name: resource_id.strip_prefix("permission.")?.to_string(),
            spec: spec.clone(),
        }),
        // Jobs (run to completion) and builds (imperative) aren't
        // reconciled desired state; nothing to write.
        ChangePayload::Generic => None,
    }
}

/// A removal `resource_id` → its `Delete` write.
fn remove_to_request(resource_id: &str) -> Option<RaftRequest> {
    if let Some(app_id) = app_id_from_resource(resource_id) {
        return Some(RaftRequest::AppDelete { app_id });
    }
    if let Some(name) = resource_id.strip_prefix("namespace.") {
        return Some(RaftRequest::NamespaceDelete {
            name: name.to_string(),
        });
    }
    if let Some(name) = resource_id.strip_prefix("permission.") {
        return Some(RaftRequest::PermissionDelete {
            name: name.to_string(),
        });
    }
    None
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
        let req = payload_to_request("app.web", &ChangePayload::App(Box::new(spec)));
        assert!(matches!(req, Some(RaftRequest::AppSpec { .. })));

        // Generic payloads (jobs etc.) are skipped, not mis-applied.
        assert!(payload_to_request("job.x", &ChangePayload::Generic).is_none());
    }

    #[test]
    fn namespace_and_permission_payloads_become_writes() {
        let ns = crate::config::NamespaceSpec {
            cpu: Some("8000m".to_string()),
            memory: None,
            gpu: None,
            max_apps: None,
            max_replicas: None,
        };
        assert!(matches!(
            payload_to_request("namespace.prod", &ChangePayload::Namespace(Box::new(ns))),
            Some(RaftRequest::NamespaceSpec { .. })
        ));
        let perm = crate::config::PermissionSpec {
            actions: vec!["deploy".to_string()],
            apps: vec![],
            namespaces: None,
        };
        assert!(matches!(
            payload_to_request("permission.dep", &ChangePayload::Permission(Box::new(perm))),
            Some(RaftRequest::PermissionSpec { .. })
        ));
    }

    #[test]
    fn removals_map_to_delete_writes_for_every_kind() {
        assert!(matches!(
            change_to_request(&ResourceChange::Remove {
                resource_id: "app.web".to_string()
            }),
            Some(RaftRequest::AppDelete { .. })
        ));
        assert!(matches!(
            change_to_request(&ResourceChange::Remove {
                resource_id: "namespace.prod".to_string()
            }),
            Some(RaftRequest::NamespaceDelete { .. })
        ));
        assert!(matches!(
            change_to_request(&ResourceChange::Remove {
                resource_id: "permission.dep".to_string()
            }),
            Some(RaftRequest::PermissionDelete { .. })
        ));
        // An unknown kind maps to nothing rather than a bogus write.
        assert!(
            change_to_request(&ResourceChange::Remove {
                resource_id: "job.x".to_string()
            })
            .is_none()
        );
    }
}
