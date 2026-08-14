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
use crate::council::types::{DesiredState, RaftRequest};

use super::diff::{ChangePayload, ResourceChange};
use super::git::GitRepo;
use super::sync::{SyncOutcome, execute_sync};
use super::types::{
    CommitInfo, CoordinatorElection, CoordinatorElectionReason, GitOpsConfig, SyncHistoryEntry,
    SyncPhase, SyncResult, SyncState,
};

/// How many past syncs `SyncState.history` retains (oldest dropped first).
const MAX_SYNC_HISTORY: usize = 100;

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

        // Tracked locally so a run of failures backs off (GIT4): the delay
        // is `poll * 2^failures`, capped, and is also mirrored into the
        // durable `SyncState.consecutive_failures` so relish/the UI see it.
        let mut consecutive_failures: u32 = 0;

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

            // Git operations shell out — keep them off the runtime. Time the
            // whole cycle so each history entry and `last_sync_duration_ms`
            // reflect how long the sync actually took.
            let config_clone = config.clone();
            let repo_dir_clone = repo_dir.clone();
            let started = std::time::Instant::now();
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
            let duration_ms = started.elapsed().as_millis() as u64;

            let outcome = match outcome {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(e)) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    record_failure(
                        &council,
                        &desired,
                        &format!("repo access failed: {e}"),
                        None,
                        duration_ms,
                    )
                    .await;
                    backoff(poll, consecutive_failures, &shutdown).await;
                    continue;
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    record_failure(
                        &council,
                        &desired,
                        &format!("sync task panicked: {e}"),
                        None,
                        duration_ms,
                    )
                    .await;
                    backoff(poll, consecutive_failures, &shutdown).await;
                    continue;
                }
            };

            // Skipped (HEAD unchanged) and hard failures: nothing to apply.
            match &outcome.result {
                SyncResult::Skipped { .. } => continue,
                SyncResult::Failure { error } => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    record_failure(
                        &council,
                        &desired,
                        error,
                        outcome.commit.as_ref(),
                        duration_ms,
                    )
                    .await;
                    backoff(poll, consecutive_failures, &shutdown).await;
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
                    let sha = outcome
                        .commit
                        .as_ref()
                        .map(|c| c.sha.as_str())
                        .unwrap_or("?");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    record_failure(
                        &council,
                        &desired,
                        &format!(
                            "apply of {sha} failed at {unapplied}; commit not advanced, \
                             will retry"
                        ),
                        outcome.commit.as_ref(),
                        duration_ms,
                    )
                    .await;
                    backoff(poll, consecutive_failures, &shutdown).await;
                    continue;
                }
            };

            // A clean cycle: reset the failure run and record the applied
            // commit (D12) plus the elected coordinator, then clear the
            // durable error.
            consecutive_failures = 0;
            record_success(&council, &desired, &outcome, duration_ms).await;

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

/// Sleep for the back-off delay, waking early on shutdown.
///
/// A run of failures shouldn't tight-loop against a broken remote, so the
/// delay grows with `consecutive_failures` (`backoff_delay`) instead of
/// retrying every `poll` (GIT4).
async fn backoff(poll: Duration, consecutive_failures: u32, shutdown: &CancellationToken) {
    let delay = super::sync::backoff_delay(poll, consecutive_failures);
    tokio::select! {
        _ = shutdown.cancelled() => {}
        _ = tokio::time::sleep(delay) => {}
    }
}

/// Record a hard sync failure durably in `SyncState` (GIT4).
///
/// Before this, a failed sync only hit stderr, so relish and the UI
/// couldn't tell a broken sync from a quiet one. Now the last error,
/// failure count and attempt time land in Raft.
async fn record_failure(
    council: &CouncilNode,
    desired: &DesiredState,
    error: &str,
    commit: Option<&CommitInfo>,
    duration_ms: u64,
) {
    // Redact any credential embedded in a URL before the error is echoed or
    // written to Raft `last_error` (M27) — defence in depth on top of the
    // source-level redaction in `git::redact_git_output`.
    let error = super::git::redact_url_credentials(error);
    eprintln!("gitops: sync failed: {error}");
    let now = now_millis();
    let mut sync_state = desired.gitops_sync_state.clone().unwrap_or_default();
    sync_state.phase = SyncPhase::Error;
    sync_state.last_error = Some(error.clone());
    sync_state.consecutive_failures = sync_state.consecutive_failures.saturating_add(1);
    sync_state.last_attempt_at = Some(now);
    sync_state.last_sync_duration_ms = duration_ms;

    // A history entry is per-commit: repo-access and panic failures never
    // reached a commit, so there is nothing to attribute the failure to and
    // we record only the scalar error above. A commit-bearing failure (a bad
    // signature, a parse/validation error, a half-applied set) does get an
    // entry so the run shows up in the UI's timeline.
    if let Some(commit) = commit {
        push_history(
            &mut sync_state,
            SyncHistoryEntry {
                commit: commit.clone(),
                timestamp: now,
                duration_ms,
                result: SyncResult::Failure {
                    error: error.clone(),
                },
                diff_summary: None,
            },
        );
    }

    record_coordinator(council, &mut sync_state).await;
    if let Err(e) = council
        .write(RaftRequest::GitOpsSyncUpdate(Box::new(sync_state)))
        .await
    {
        eprintln!("gitops: failed to record sync failure: {e}");
    }
}

/// Record a clean sync in `SyncState`: advance the applied commit, clear
/// the error, reset the failure count, append a history entry and stamp the
/// coordinator.
async fn record_success(
    council: &CouncilNode,
    desired: &DesiredState,
    outcome: &SyncOutcome,
    duration_ms: u64,
) {
    let Some(commit) = &outcome.commit else {
        return;
    };
    let now = now_millis();
    let mut sync_state = desired.gitops_sync_state.clone().unwrap_or_default();
    sync_state.last_applied_commit = Some(commit.clone());
    sync_state.last_fetched_commit = Some(commit.clone());
    sync_state.phase = SyncPhase::Idle;
    sync_state.last_error = None;
    sync_state.consecutive_failures = 0;
    sync_state.last_sync_at = Some(now);
    sync_state.last_attempt_at = Some(now);
    sync_state.last_sync_duration_ms = duration_ms;
    sync_state.last_diff_summary = outcome.diff_summary.clone();
    sync_state.file_errors = outcome.file_errors.clone();
    push_history(
        &mut sync_state,
        SyncHistoryEntry {
            commit: commit.clone(),
            timestamp: now,
            duration_ms,
            result: outcome.result.clone(),
            diff_summary: outcome.diff_summary.clone(),
        },
    );
    record_coordinator(council, &mut sync_state).await;
    if let Err(e) = council
        .write(RaftRequest::GitOpsSyncUpdate(Box::new(sync_state)))
        .await
    {
        eprintln!("gitops: failed to record sync state: {e}");
    }
}

/// Append a sync to `history`, dropping the oldest once it exceeds
/// [`MAX_SYNC_HISTORY`]. A `VecDeque` gives O(1) push/pop at both ends, so the
/// ring stays bounded without ever reallocating the whole buffer.
fn push_history(sync_state: &mut SyncState, entry: SyncHistoryEntry) {
    sync_state.history.push_back(entry);
    while sync_state.history.len() > MAX_SYNC_HISTORY {
        sync_state.history.pop_front();
    }
}

/// Stamp the current GitOps coordinator into `sync_state`, honestly.
///
/// The coordinator is the Raft leader — the only node that can apply a sync
/// (see [`super::sync::coordinator_for_leader`]). We compare against the
/// previously-recorded coordinator so a leadership handover is logged as a
/// failover rather than silently swapping the name.
async fn record_coordinator(council: &CouncilNode, sync_state: &mut SyncState) {
    let previous = sync_state.coordinator_node_id.clone();
    if let Some(election) = current_coordinator(council, previous.as_deref()).await {
        if election.reason == CoordinatorElectionReason::Failover {
            println!(
                "gitops: coordinator failover — {} now drives syncs (was {})",
                election.node_id,
                previous.as_deref().unwrap_or("?")
            );
        }
        sync_state.coordinator_node_id = Some(election.node_id);
    }
}

/// The current GitOps coordinator, derived from Raft leadership.
///
/// Returns `None` when leadership is unknown (no metrics yet). Otherwise the
/// leader's node name, tagged `Initial` or `Failover` relative to `previous`.
async fn current_coordinator(
    council: &CouncilNode,
    previous: Option<&str>,
) -> Option<CoordinatorElection> {
    let metrics = council.metrics().borrow().clone();
    let leader_id = metrics.current_leader?;
    let membership = metrics.membership_config.membership().clone();
    let leader_name = membership
        .nodes()
        .find(|(id, _)| **id == leader_id)
        .map(|(_, info)| info.name.clone())?;
    Some(super::sync::coordinator_for_leader(
        &leader_name,
        previous,
        now_millis(),
    ))
}

/// Current Unix time in milliseconds.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
            // The resource id already carries the full identity
            // (`app.<namespace>/<name>`), derived the same way
            // `config_to_desired_writes` derives its `AppId` (GIT2). Parse
            // it back rather than re-deriving from the spec so add, update
            // and remove all agree on one identity.
            let app_id = super::diff::parse_app_resource_id(resource_id)?;
            Some(RaftRequest::AppSpec {
                app_id,
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
    if let Some(app_id) = super::diff::parse_app_resource_id(resource_id) {
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
    use crate::lettuce::types::{DiffSummary, SignatureStatus};
    use crate::meat::types::AppId;

    fn sample_commit(sha: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            message: "test commit".to_string(),
            author: "dev".to_string(),
            timestamp: 1700000000000,
            signature: SignatureStatus::NotChecked,
        }
    }

    fn sample_entry(sha: &str) -> SyncHistoryEntry {
        SyncHistoryEntry {
            commit: sample_commit(sha),
            timestamp: 1700000000000,
            duration_ms: 5,
            result: SyncResult::Success,
            diff_summary: Some(DiffSummary {
                added: 1,
                modified: 0,
                removed: 0,
            }),
        }
    }

    #[test]
    fn push_history_appends_in_order() {
        let mut state = SyncState::default();
        push_history(&mut state, sample_entry("aaa"));
        push_history(&mut state, sample_entry("bbb"));
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.history.front().unwrap().commit.sha, "aaa");
        assert_eq!(state.history.back().unwrap().commit.sha, "bbb");
    }

    #[test]
    fn push_history_is_bounded_and_drops_the_oldest() {
        let mut state = SyncState::default();
        for i in 0..(MAX_SYNC_HISTORY + 50) {
            push_history(&mut state, sample_entry(&format!("c{i}")));
        }
        assert_eq!(state.history.len(), MAX_SYNC_HISTORY);
        // The oldest 50 were evicted; the window now starts at c50.
        assert_eq!(state.history.front().unwrap().commit.sha, "c50");
        assert_eq!(
            state.history.back().unwrap().commit.sha,
            format!("c{}", MAX_SYNC_HISTORY + 49)
        );
    }

    #[test]
    fn app_removal_parses_the_full_namespaced_identity() {
        assert_eq!(
            remove_to_request("app.prod/web"),
            Some(RaftRequest::AppDelete {
                app_id: AppId::new("web", "prod")
            })
        );
        // A bare-name id (no namespace segment) isn't a valid app id.
        assert!(remove_to_request("app.web").is_none());
        assert!(remove_to_request("job.migrate").is_none());
    }

    #[test]
    fn app_payload_becomes_a_namespaced_appspec_write() {
        let spec = crate::config::Config::parse("[app.web]\nimage = \"x:1\"\n")
            .unwrap()
            .app
            .remove("web")
            .unwrap();
        // The resource id carries the namespace; the write keys on it.
        let req = payload_to_request("app.prod/web", &ChangePayload::App(Box::new(spec)));
        assert!(matches!(
            req,
            Some(RaftRequest::AppSpec { app_id, .. }) if app_id == AppId::new("web", "prod")
        ));

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
                resource_id: "app.default/web".to_string()
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
