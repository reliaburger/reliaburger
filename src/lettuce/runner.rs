//! The GitOps sync runner (Stage 4 W10, L13).
//!
//! Ties the pure `execute_sync` logic to the async runtime and Raft:
//! a leader-only task that clones the configured repo, syncs on a poll
//! timer or a webhook nudge, and applies the resulting `ResourceChange`s
//! to Raft desired state — the same `AppSpec`/`AppDelete` writes a
//! manual `relish apply` makes, wrapped in a `GitOpsWrite` that records
//! what GitOps owns (#693).
//!
//! The configuration comes from Raft, not from the node's `node.toml`
//! (#693). A node's `[gitops]` section seeds Raft when the cluster has
//! none; after that every leader syncs the same repository, and a node
//! whose own section differs reports it rather than using it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::council::node::CouncilNode;
use crate::council::types::{CouncilResponse, DesiredState, RaftRequest};
use crate::pickle::binding::ImageBinder;

use super::diff::{ChangePayload, CurrentState, ResourceChange};
use super::git::GitRepo;
use super::subprocess::CommandBudget;
use super::sync::{SyncOutcome, execute_sync};
use super::types::{
    CommitInfo, CoordinatorElection, CoordinatorElectionReason, GitOpsConfig, LettuceError,
    LocalConfigCheck, StoredGitOpsConfig, SyncHistoryEntry, SyncPhase, SyncResult, SyncState,
    check_local_config,
};

/// How many past syncs `SyncState.history` retains (oldest dropped first).
const MAX_SYNC_HISTORY: usize = 100;

/// The poll interval before the cluster's configuration is known.
const DEFAULT_POLL: Duration = Duration::from_secs(30);

/// A running GitOps sync loop.
#[derive(Debug)]
pub struct GitOpsSync {
    /// Finishes once the shutdown token is cancelled.
    pub task: tokio::task::JoinHandle<()>,
    /// How this node's own `[gitops]` section compares with the cluster's,
    /// refreshed on every tick. The API serves it to `relish wtf`.
    pub local_check: watch::Receiver<LocalConfigCheck>,
}

/// Spawn the GitOps sync loop.
///
/// Every cluster node runs it. Each tick compares the node's own
/// `[gitops]` section (`local_config`) with the configuration in Raft and
/// publishes the result on [`GitOpsSync::local_check`]. Only the leader
/// goes further: it seeds Raft from `local_config` when the cluster has no
/// configuration, then syncs whatever Raft holds. The git clone and sync
/// run in `spawn_blocking` because `GitRepo` shells out to `git`.
///
/// It syncs when the poll timer fires or a webhook arrives (whichever
/// comes first). Every sync reconciles Git against desired state, even
/// when Git hasn't moved, so a manual change to a GitOps resource is
/// repaired on the next tick (B16). The task finishes once `shutdown` is
/// cancelled; a `git` child running at that moment is killed rather than
/// waited for (B19).
///
/// With a `binder` (the leader's runtime pulls images), each app's images
/// bind to digests as they're written, as a manual apply binds them.
pub fn spawn_gitops_sync(
    council: Arc<CouncilNode>,
    local_config: Option<GitOpsConfig>,
    mut webhook_rx: mpsc::Receiver<()>,
    data_dir: PathBuf,
    shutdown: CancellationToken,
    binder: Option<ImageBinder>,
) -> GitOpsSync {
    let (check_tx, local_check) = watch::channel(LocalConfigCheck::Unknown);
    let task = tokio::spawn(async move {
        let repo_dir = data_dir.join("gitops-repo");
        let mut poll = local_config.as_ref().map_or(DEFAULT_POLL, poll_interval);
        let mut ticker = tokio::time::interval(poll);
        let mut triggers = council.gitops_trigger_updates();
        let mut leadership = council.metrics();

        // Tracked locally so a run of failures backs off (GIT4): the delay
        // is `poll * 2^failures`, capped, and is also mirrored into the
        // durable `SyncState.consecutive_failures` so relish/the UI see it.
        let mut consecutive_failures: u32 = 0;

        loop {
            let durable_wake = tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => false,
                change = triggers.changed() => { if change.is_err() { break } true },
                change = leadership.changed() => { if change.is_err() { break } true },
                signal = webhook_rx.recv() => {
                    if signal.is_none() {
                        break; // sender dropped
                    }
                    // Drain any queued webhook signals so a burst
                    // collapses into a single sync.
                    while webhook_rx.try_recv().is_ok() {}
                    false
                }
            };

            // The term comes first: a snapshot read after it is at least as
            // new, and every write below must commit in this same term, or
            // the state machine refuses it (finding 7).
            let term = council.current_term();
            let mut desired = council.desired_state().await;
            let master_key = council.wrapping_ikm().map(|key| key.as_slice());
            let mut cluster_config = unseal(desired.gitops_config.as_ref(), master_key);
            publish_local_check(&check_tx, local_config.as_ref(), cluster_config.as_ref());

            if !council.is_leader().await {
                continue;
            }

            if cluster_config.is_none() {
                let Some(local) = &local_config else {
                    continue; // GitOps is off for this cluster.
                };
                if !seed_cluster_config(&council, local, master_key).await {
                    continue;
                }
                desired = council.desired_state().await;
                cluster_config = unseal(desired.gitops_config.as_ref(), master_key);
                publish_local_check(&check_tx, local_config.as_ref(), cluster_config.as_ref());
            }
            let config = match cluster_config {
                None => continue,
                Some(Ok(config)) => config,
                Some(Err(error)) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    record_failure(&council, &desired, &error.to_string(), None, 0).await;
                    backoff(poll, consecutive_failures, &shutdown).await;
                    continue;
                }
            };
            let configured_poll = poll_interval(&config);
            if configured_poll != poll {
                poll = configured_poll;
                ticker = tokio::time::interval(poll);
                // An interval's first tick is immediate; this one has run.
                ticker.reset();
            }

            if durable_wake
                && !desired
                    .gitops_sync_state
                    .as_ref()
                    .is_some_and(|sync| sync.requested_generation > sync.completed_generation)
            {
                continue;
            }
            let current_apps = desired.apps.clone();
            let declared_apps = desired.declared_apps.clone();
            let current_namespaces = desired.namespaces.clone();
            let current_permissions = desired.permissions.clone();
            let current_policies = desired.policies.clone();
            let owned = desired.gitops_owned.clone();
            let overrides = desired.autoscale_overrides.clone();
            let last_sha = desired
                .gitops_sync_state
                .as_ref()
                .and_then(|s| s.last_applied_commit.as_ref())
                .map(|c| c.sha.clone());

            // Git operations shell out — keep them off the runtime. Time the
            // whole cycle so each history entry and `last_sync_duration_ms`
            // reflect how long the sync actually took.
            //
            // Awaiting the blocking task is safe during shutdown: every `git`
            // child runs under the shutdown token (B19), so cancellation
            // kills the child and the task returns at once. Dropping the
            // task instead would only stop us waiting; git would carry on.
            let config_clone = config.clone();
            let repo_dir_clone = repo_dir.clone();
            let budget = CommandBudget {
                cancel: shutdown.clone(),
                ..CommandBudget::default()
            };
            let last_sha_clone = last_sha.clone();
            let started = std::time::Instant::now();
            let outcome = tokio::task::spawn_blocking(move || {
                let repo = GitRepo::clone_or_open_with_budget(
                    &config_clone.repo,
                    &repo_dir_clone,
                    &config_clone.branch,
                    budget,
                )?;
                let current = CurrentState {
                    apps: &current_apps,
                    declared_apps: &declared_apps,
                    namespaces: &current_namespaces,
                    permissions: &current_permissions,
                    policies: &current_policies,
                    owned: &owned,
                };
                Ok::<_, LettuceError>(execute_sync(
                    &repo,
                    &config_clone,
                    &current,
                    &overrides,
                    last_sha_clone.as_deref(),
                ))
            })
            .await;
            let duration_ms = started.elapsed().as_millis() as u64;
            if shutdown.is_cancelled() {
                break;
            }

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

            // Hard failures: nothing to apply.
            match &outcome.result {
                SyncResult::Skipped { .. } => {
                    let mut state = desired.gitops_sync_state.clone().unwrap_or_default();
                    state.completed_generation = state.requested_generation;
                    if let Err(error) = council
                        .write(RaftRequest::GitOpsSyncUpdate(Box::new(state)))
                        .await
                    {
                        eprintln!("gitops: could not acknowledge unchanged sync: {error}");
                    }
                    continue;
                }
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

            let warnings_changed = desired
                .gitops_sync_state
                .as_ref()
                .is_none_or(|state| state.warnings != outcome.warnings);
            if warnings_changed {
                for warning in &outcome.warnings {
                    eprintln!("gitops: warning: {warning}");
                }
            }

            // Apply every change through the SAME desired-state writes a
            // manual `relish apply` makes (12b.2 T6), each wrapped so the
            // state machine records what GitOps owns. A refused change no
            // longer blocks the rest (#693): every change is tried and
            // every refusal named.
            //
            // Atomicity (D12): `last_applied_commit` advances only if
            // EVERY write in the sync succeeded. A partial sync leaves the
            // commit unadvanced and the next tick re-applies the whole set.
            // Writes are idempotent (spec upsert / delete), so re-applying
            // an already-committed change is a harmless no-op.
            let report = apply_changes(&council, term, &outcome.changes, binder.as_ref()).await;
            if let Some(reason) = &report.abandoned {
                eprintln!("gitops: sync abandoned: {reason}");
                continue;
            }
            if !report.refused.is_empty() {
                consecutive_failures = consecutive_failures.saturating_add(1);
                record_partial(&council, &desired, &outcome, &report.refused, duration_ms).await;
                backoff(poll, consecutive_failures, &shutdown).await;
                continue;
            }
            let applied = report.applied;

            // A clean cycle: reset the failure run and record the applied
            // commit (D12) plus the elected coordinator, then clear the
            // durable error. A tick that found Git and the cluster already
            // in agreement, with nothing to clear, writes nothing: every
            // poll reconciles now (B16), and recording each one would grow
            // the Raft log every `poll_interval_secs` for no reason.
            consecutive_failures = 0;
            let sha = outcome.commit.as_ref().map(|c| c.sha.as_str());
            let same_commit = sha.is_some() && sha == last_sha.as_deref();
            if same_commit && applied == 0 && !warnings_changed && sync_state_is_clean(&desired) {
                continue;
            }
            record_success(&council, &desired, &outcome, duration_ms).await;

            if applied > 0 {
                let what = if same_commit {
                    "repaired drift with"
                } else {
                    "applied"
                };
                println!(
                    "gitops: {what} {applied} change(s) from {}",
                    sha.unwrap_or("?")
                );
            }
        }
    });
    GitOpsSync { task, local_check }
}

/// The poll interval a configuration asks for, at least a second.
fn poll_interval(config: &GitOpsConfig) -> Duration {
    Duration::from_secs(config.poll_interval_secs.max(1))
}

/// The cluster's configuration with its credentials, if it has one.
fn unseal(
    stored: Option<&StoredGitOpsConfig>,
    master_key: Option<&[u8]>,
) -> Option<Result<GitOpsConfig, LettuceError>> {
    stored.map(|stored| stored.unseal(master_key))
}

/// Publish how this node's `[gitops]` compares with the cluster's, and log
/// when that changes, so an ignored local section doesn't go unnoticed.
fn publish_local_check(
    check_tx: &watch::Sender<LocalConfigCheck>,
    local: Option<&GitOpsConfig>,
    cluster: Option<&Result<GitOpsConfig, LettuceError>>,
) {
    let check = check_local_config(local, cluster.map(Result::as_ref));
    let changed = check_tx.send_if_modified(|current| {
        if *current == check {
            return false;
        }
        *current = check.clone();
        true
    });
    if !changed {
        return;
    }
    match &check {
        LocalConfigCheck::Differs { fields } => eprintln!(
            "gitops: this node's [gitops] differs from the cluster's ({}); the cluster's \
             configuration in Raft is used. Change it with `relish gitops set`",
            fields.join(", ")
        ),
        LocalConfigCheck::Unreadable { reason } => {
            eprintln!("gitops: can't read the cluster's GitOps configuration: {reason}")
        }
        _ => {}
    }
}

/// Seed the cluster's configuration from this node's `[gitops]` section.
///
/// Returns whether Raft now holds a configuration (this node's, or one
/// another node seeded first). A section whose credentials can't be
/// sealed is never written in clear: it's logged and skipped.
async fn seed_cluster_config(
    council: &CouncilNode,
    local: &GitOpsConfig,
    master_key: Option<&[u8]>,
) -> bool {
    let stored = match StoredGitOpsConfig::seal(local, master_key) {
        Ok(stored) => stored,
        Err(error) => {
            eprintln!("gitops: not seeding the cluster from [gitops]: {error}");
            return false;
        }
    };
    match council
        .write(RaftRequest::GitOpsConfigSet {
            config: Some(Box::new(stored)),
            seed: true,
        })
        .await
    {
        Ok(CouncilResponse::Refused { .. }) => true,
        Ok(_) => {
            println!(
                "gitops: seeded the cluster's GitOps configuration from this node's [gitops] \
                 ({} {})",
                super::git::redact_url_credentials(&local.repo),
                local.branch
            );
            true
        }
        Err(error) => {
            eprintln!("gitops: could not seed the cluster's GitOps configuration: {error}");
            false
        }
    }
}

/// Whether the recorded sync state has no error left to clear.
fn sync_state_is_clean(desired: &DesiredState) -> bool {
    desired.gitops_sync_state.as_ref().is_some_and(|state| {
        state.last_error.is_none()
            && state.consecutive_failures == 0
            && state.requested_generation == state.completed_generation
    })
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
    // signature, a parse/validation error) does get an entry so the run
    // shows up in the UI's timeline.
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

/// Record a sync in which some changes were refused (#693, finding 4).
///
/// Every other change applied. The result is `PartialSuccess` naming each
/// offender, in `last_error` and in the history; the applied commit stays
/// where it was, so the next tick retries the refused changes (D12).
async fn record_partial(
    council: &CouncilNode,
    desired: &DesiredState,
    outcome: &SyncOutcome,
    refused: &[String],
    duration_ms: u64,
) {
    let sha = outcome
        .commit
        .as_ref()
        .map_or("?", |commit| commit.sha.as_str());
    let error = super::git::redact_url_credentials(&partial_sync_error(sha, refused));
    eprintln!("gitops: {error}");
    let now = now_millis();
    let mut sync_state = desired.gitops_sync_state.clone().unwrap_or_default();
    sync_state.phase = SyncPhase::Error;
    sync_state.last_error = Some(error);
    sync_state.consecutive_failures = sync_state.consecutive_failures.saturating_add(1);
    sync_state.last_attempt_at = Some(now);
    sync_state.last_sync_duration_ms = duration_ms;
    sync_state.warnings = outcome.warnings.clone();
    if let Some(commit) = &outcome.commit {
        sync_state.last_fetched_commit = Some(commit.clone());
        push_history(
            &mut sync_state,
            SyncHistoryEntry {
                commit: commit.clone(),
                timestamp: now,
                duration_ms,
                result: SyncResult::PartialSuccess {
                    errors: refused.to_vec(),
                },
                diff_summary: outcome.diff_summary.clone(),
            },
        );
    }
    record_coordinator(council, &mut sync_state).await;
    if let Err(e) = council
        .write(RaftRequest::GitOpsSyncUpdate(Box::new(sync_state)))
        .await
    {
        eprintln!("gitops: failed to record a partial sync: {e}");
    }
}

/// The sync status line for a partial sync: every refused change, named.
fn partial_sync_error(sha: &str, refused: &[String]) -> String {
    format!(
        "partial sync of {sha}: {} change(s) refused: {}; commit not advanced, will retry",
        refused.len(),
        refused.join("; ")
    )
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
    sync_state.completed_generation = sync_state.requested_generation;
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
    sync_state.warnings = outcome.warnings.clone();
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

/// What applying a sync's changes did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// Writes the cluster committed.
    pub applied: usize,
    /// Every change that didn't apply, as `resource_id: reason`. A change
    /// a policy refused names the policy.
    pub refused: Vec<String>,
    /// What `warn` policies said about the apps that applied.
    pub warnings: Vec<String>,
    /// Set when leadership changed part-way and the sync stopped: the
    /// remaining changes belong to the next leader's sync.
    pub abandoned: Option<String>,
}

/// How many times one app is evaluated again after the policy set moved
/// under it, before the sync gives up on it until the next tick.
const POLICY_ATTEMPTS: usize = 3;

/// Apply a sync's changes to Raft, every one of them (#693, finding 4).
///
/// Each change goes in a `GitOpsWrite` stamped with `term`, the term the
/// sync read desired state in, so the state machine records ownership and
/// refuses the write if leadership has moved since. A change the cluster
/// refuses, or whose image can't be bound, is named in
/// [`ApplyReport::refused`] and the rest still apply. The caller must
/// advance `last_applied_commit` only when nothing was refused: that's the
/// D12 guarantee that the next tick re-applies the (idempotent) set.
///
/// Every added or changed app is first judged by the policies in force
/// (#1117), the same ones `relish apply` uses, and its write is fenced on
/// the policy generation it was judged under. A denied app is refused,
/// naming the policy, and the rest still apply. When the policy set moves
/// between judging and committing, the app is judged again.
///
/// When the term moves, or this node stops leading, the sync is abandoned
/// at once rather than applying the rest of a stale diff.
pub async fn apply_changes(
    council: &CouncilNode,
    term: u64,
    changes: &[ResourceChange],
    binder: Option<&ImageBinder>,
) -> ApplyReport {
    let mut report = ApplyReport::default();
    let mut policies = PolicyCache::default();
    for change in changes {
        if let Some(reason) = leadership_moved(council, term).await {
            report.abandoned = Some(reason);
            return report;
        }
        let Some(request) = change_to_request(change) else {
            continue; // jobs/builds: not reconciled desired state
        };
        let adopting = matches!(change, ResourceChange::Adopt { .. });
        // Policies judge what git declares, before an image is bound, the
        // way `relish policy test` sees it. An adopted app changes nothing,
        // and a new policy never touches what already runs.
        let outcome = match (request, adopting) {
            (RaftRequest::AppSpec { app_id, spec }, false) => {
                let app = JudgedApp {
                    term,
                    binder,
                    app_id: &app_id,
                    declared: &spec,
                };
                match write_judged_app(council, &mut policies, app).await {
                    Judged::Written { outcome, warnings } => {
                        report.warnings.extend(warnings);
                        outcome
                    }
                    Judged::Refused(reason) => {
                        report
                            .refused
                            .push(format!("{}: {reason}", change_id(change)));
                        continue;
                    }
                    Judged::Unbound(error) => {
                        report
                            .refused
                            .push(format!("{} ({error})", change_id(change)));
                        continue;
                    }
                }
            }
            (request, _) => {
                council
                    .write(RaftRequest::GitOpsWrite {
                        term,
                        change: Box::new(request),
                    })
                    .await
            }
        };
        // A committed entry the state machine refused left desired state
        // unchanged, so it's as unapplied as a failed write (B15).
        match outcome {
            Ok(CouncilResponse::Refused { reason }) => {
                if let Some(moved) = leadership_moved(council, term).await {
                    report.abandoned = Some(moved);
                    return report;
                }
                report
                    .refused
                    .push(format!("{}: {reason}", change_id(change)));
            }
            Ok(_) => report.applied += 1,
            Err(e) => {
                if let Some(moved) = leadership_moved(council, term).await {
                    report.abandoned = Some(moved);
                    return report;
                }
                report
                    .refused
                    .push(format!("{}: write failed: {e}", change_id(change)));
            }
        }
    }
    report
}

/// The policies last read from desired state, compiled, with the
/// generation they were read at, so a sync compiles each set once.
#[derive(Default)]
struct PolicyCache {
    compiled: Option<(
        u64,
        Result<crate::policy::PolicySet, crate::policy::AdmitError>,
    )>,
}

impl PolicyCache {
    /// The policies in force and their generation, read together.
    async fn current(
        &mut self,
        council: &CouncilNode,
    ) -> (
        u64,
        &Result<crate::policy::PolicySet, crate::policy::AdmitError>,
    ) {
        let generation = council
            .read_desired(|desired| desired.policy_generation)
            .await;
        let fresh = match self.compiled.take() {
            Some((at, set)) if at == generation => (at, set),
            _ => {
                let (at, specs) = council
                    .read_desired(|desired| (desired.policy_generation, desired.policies.clone()))
                    .await;
                (at, crate::policy::PolicySet::compile(&specs))
            }
        };
        let (at, set) = self.compiled.insert(fresh);
        (*at, set)
    }
}

/// What judging and writing one app did.
enum Judged {
    /// The write was proposed; `outcome` is what the council said.
    Written {
        outcome: Result<CouncilResponse, crate::council::CouncilError>,
        warnings: Vec<String>,
    },
    /// A policy refused the app, or the policy set kept moving.
    Refused(String),
    /// The app's image couldn't be bound to a digest.
    Unbound(String),
}

/// One app a sync adds or changes, as git declares it.
struct JudgedApp<'a> {
    /// The term the sync read desired state in.
    term: u64,
    /// Binds the app's images to digests, when the cluster has a registry.
    binder: Option<&'a ImageBinder>,
    /// The app's identity.
    app_id: &'a crate::meat::types::AppId,
    /// Its spec, as git declares it.
    declared: &'a crate::config::AppSpec,
}

/// Judge one app by the policies in force, fill in their defaults, bind
/// its images and write it fenced on their generation, judging again when
/// the set moves before the write commits. A defaulted app's write
/// carries its declared spec, so the next sync diffs git against that.
async fn write_judged_app(
    council: &CouncilNode,
    policies: &mut PolicyCache,
    app: JudgedApp<'_>,
) -> Judged {
    let caller = crate::policy::Caller::gitops();
    for _ in 0..POLICY_ATTEMPTS {
        let (generation, set) = policies.current(council).await;
        let set = match set {
            Ok(set) => set,
            Err(error) => return Judged::Refused(error.to_string()),
        };
        let judgement = crate::policy::judge_app(
            set,
            app.app_id,
            app.declared,
            &caller,
            crate::policy::Source::GitOps,
        );
        if !judgement.verdict.is_allowed() {
            let denials: Vec<String> = judgement
                .verdict
                .denials
                .iter()
                .map(ToString::to_string)
                .collect();
            eprintln!("gitops: {}", denials.join("; "));
            return Judged::Refused(denial_reason(&judgement.verdict.denials));
        }
        let warnings: Vec<String> = judgement
            .verdict
            .warnings
            .iter()
            .map(ToString::to_string)
            .collect();
        for filled in &judgement.filled {
            println!("gitops: {filled}");
        }
        let mut spec = judgement.spec;
        if let Some(binder) = app.binder {
            let catalog = council.manifest_catalog().await;
            match binder.bind_app(&app.app_id.name, &mut spec, &catalog).await {
                Ok(bindings) => {
                    for binding in bindings {
                        println!("gitops: {binding}");
                    }
                }
                Err(error) => return Judged::Unbound(error.to_string()),
            }
        }
        // What git declared rides along when the stored spec differs from
        // it, by a default or a bound image (#1118).
        let declared = (spec != *app.declared).then(|| Box::new(app.declared.clone()));
        let fenced = RaftRequest::PolicyFenced {
            generation,
            change: Box::new(RaftRequest::GitOpsWrite {
                term: app.term,
                change: Box::new(RaftRequest::AppSpec {
                    app_id: app.app_id.clone(),
                    spec: Box::new(spec),
                }),
            }),
            declared,
        };
        match council.write(fenced).await {
            Ok(CouncilResponse::PolicyGenerationMoved { .. }) => continue,
            outcome => {
                for warning in &warnings {
                    println!("gitops: warning: {warning}");
                }
                return Judged::Written { outcome, warnings };
            }
        }
    }
    Judged::Refused(format!(
        "the policies changed {POLICY_ATTEMPTS} times while it was being applied; the next sync tries again"
    ))
}

/// How the partial sync result names the policies that refused an app.
fn denial_reason(denials: &[crate::policy::Finding]) -> String {
    let reasons: Vec<String> = denials
        .iter()
        .map(|finding| format!("denied by policy {}: {}", finding.policy, finding.message))
        .collect();
    reasons.join("; ")
}

/// Why this node may no longer apply a sync read in `term`, if it can't.
async fn leadership_moved(council: &CouncilNode, term: u64) -> Option<String> {
    let current = council.current_term();
    if current != term {
        return Some(format!(
            "leadership changed (term {term} became {current}) during the sync"
        ));
    }
    if !council.is_leader().await {
        return Some("this node is no longer the leader".to_string());
    }
    None
}

/// Map a diff `ResourceChange` to the Raft write that realises it.
///
/// Returns `None` for kinds that aren't reconciled desired state (jobs,
/// builds) — those carry a `Generic` payload, so their add/update yields
/// nothing to write. Every declarative kind (app, namespace, permission,
/// policy) maps to a real write for both add/update and remove.
fn change_to_request(change: &ResourceChange) -> Option<RaftRequest> {
    match change {
        ResourceChange::Add { resource_id, spec }
        | ResourceChange::Adopt { resource_id, spec }
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
        ChangePayload::Policy(spec) => Some(RaftRequest::PolicySpec {
            name: resource_id.strip_prefix("policy.")?.to_string(),
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
    if let Some(name) = resource_id.strip_prefix("policy.") {
        return Some(RaftRequest::PolicyDelete {
            name: name.to_string(),
        });
    }
    None
}

fn change_id(change: &ResourceChange) -> &str {
    match change {
        ResourceChange::Add { resource_id, .. }
        | ResourceChange::Update { resource_id, .. }
        | ResourceChange::Adopt { resource_id, .. }
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
            secret_key: false,
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
    async fn leader_council() -> Arc<CouncilNode> {
        use std::collections::BTreeMap;

        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::{CouncilConfig, CouncilNodeInfo};

        let raft_router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, raft_router.clone());
        let node = CouncilNode::new(
            1,
            CouncilConfig::default(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        raft_router.register(1, node.raft().clone()).await;
        let members = BTreeMap::from([(
            1,
            CouncilNodeInfo {
                addr: "127.0.0.1:9445".parse().unwrap(),
                name: "node-1".into(),
            },
        )]);
        node.initialize(members).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !node.is_leader().await {
            assert!(tokio::time::Instant::now() < deadline, "no leader");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Arc::new(node)
    }

    fn app_add(resource_id: &str) -> ResourceChange {
        let spec = crate::config::Config::parse("[app.web]\nimage = \"x:1\"\n")
            .unwrap()
            .app
            .remove("web")
            .unwrap();
        ResourceChange::Add {
            resource_id: resource_id.to_string(),
            spec: ChangePayload::App(Box::new(spec)),
        }
    }

    /// The changes the tests below apply: two the cluster refuses (an app
    /// in a test-lease namespace, and a delete of an app GitOps never
    /// wrote) between two it accepts.
    fn mixed_changes() -> Vec<ResourceChange> {
        vec![
            app_add("app.default/first"),
            app_add("app.rbtest-lease/web"),
            ResourceChange::Remove {
                resource_id: "app.default/manual".into(),
            },
            app_add("app.default/last"),
        ]
    }

    /// #693 (finding 4): a refused change no longer stops the sync; every
    /// change after it is still applied.
    #[tokio::test]
    async fn one_refused_change_does_not_block_the_others() {
        let council = leader_council().await;
        let report = apply_changes(&council, council.current_term(), &mixed_changes(), None).await;
        assert_eq!(report.applied, 2, "{report:?}");
        assert!(report.abandoned.is_none());
        let state = council.desired_state().await;
        assert!(state.apps.contains_key(&AppId::new("first", "default")));
        assert!(state.apps.contains_key(&AppId::new("last", "default")));
        assert!(
            state
                .gitops_owned
                .apps
                .contains(&AppId::new("last", "default"))
        );
        council.shutdown().await.ok();
    }

    /// #693 (finding 4): the partial result names every offender, in the
    /// report and in the sync status line.
    #[tokio::test]
    async fn partial_sync_names_every_offender() {
        let council = leader_council().await;
        let report = apply_changes(&council, council.current_term(), &mixed_changes(), None).await;
        assert_eq!(report.refused.len(), 2, "{report:?}");
        assert!(report.refused[0].starts_with("app.rbtest-lease/web: "));
        assert!(report.refused[1].starts_with("app.default/manual: "));
        let line = partial_sync_error("abc123", &report.refused);
        assert!(line.contains("2 change(s) refused"), "{line}");
        assert!(line.contains("app.rbtest-lease/web"), "{line}");
        assert!(line.contains("app.default/manual"), "{line}");
        council.shutdown().await.ok();
    }

    /// #693 (finding 7): a sync whose snapshot was read in an earlier term
    /// writes nothing; the next leader's sync takes over.
    #[tokio::test]
    async fn sync_is_abandoned_when_the_term_changed() {
        let council = leader_council().await;
        let stale_term = council.current_term().saturating_sub(1);
        let report = apply_changes(&council, stale_term, &mixed_changes(), None).await;
        assert!(report.abandoned.is_some(), "{report:?}");
        assert_eq!(report.applied, 0);
        assert!(council.desired_state().await.apps.is_empty());
        council.shutdown().await.ok();
    }

    #[test]
    fn an_unchanged_commit_still_records_completion_of_a_pending_generation() {
        let mut desired = DesiredState {
            gitops_sync_state: Some(SyncState {
                requested_generation: 1,
                completed_generation: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!sync_state_is_clean(&desired));
        desired
            .gitops_sync_state
            .as_mut()
            .unwrap()
            .completed_generation = 1;
        assert!(sync_state_is_clean(&desired));
    }
}
