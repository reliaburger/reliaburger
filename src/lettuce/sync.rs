//! Core sync loop for Lettuce GitOps.
//!
//! The sync loop runs on the coordinator node, triggered by either
//! the poll timer or a webhook signal. It fetches from git, verifies
//! signatures, parses TOML, diffs against Raft state, and applies
//! only changed resources.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use crate::config::app::AppSpec;
use crate::config::{Config, NamespaceSpec, PermissionSpec};
use crate::meat::types::AppId;

use super::diff::{self, CurrentState, ResourceChange};
use super::git::GitRepo;
use super::types::*;
use super::verify;

/// Result of a single sync cycle.
#[derive(Debug)]
pub struct SyncOutcome {
    /// The commit that was processed.
    pub commit: Option<CommitInfo>,
    /// The outcome.
    pub result: SyncResult,
    /// Diff summary if changes were applied.
    pub diff_summary: Option<DiffSummary>,
    /// Resource changes to write to Raft.
    pub changes: Vec<ResourceChange>,
    /// Per-file parse errors (non-fatal).
    pub file_errors: HashMap<String, String>,
}

/// A "nothing to do" outcome with the given reason.
fn skipped(reason: &str) -> SyncOutcome {
    SyncOutcome {
        commit: None,
        result: SyncResult::Skipped {
            reason: reason.to_string(),
        },
        diff_summary: None,
        changes: Vec::new(),
        file_errors: HashMap::new(),
    }
}

/// Whether a commit's signature status may be applied under the signing policy.
///
/// Fails **closed**: when `require_signed` is set, only a genuinely `Verified`
/// signature passes. `NotChecked` means verification never ran — no trusted
/// keys are configured, or `git` could not be spawned — and admitting it would
/// silently apply unsigned commits despite `require_signed_commits = true`.
fn signature_admitted(status: &SignatureStatus, require_signed: bool) -> bool {
    if require_signed {
        *status == SignatureStatus::Verified
    } else {
        true
    }
}

/// Execute a single sync cycle.
///
/// This is the pure logic of the sync loop, separated from the
/// async runtime and Raft interaction so it can be tested in isolation.
pub fn execute_sync(
    repo: &GitRepo,
    config: &GitOpsConfig,
    current_apps: &HashMap<AppId, AppSpec>,
    current_namespaces: &BTreeMap<String, NamespaceSpec>,
    current_permissions: &BTreeMap<String, PermissionSpec>,
    autoscale_overrides: &[(String, u32)],
    last_applied_sha: Option<&str>,
) -> SyncOutcome {
    // Step 1: Fetch
    let new_commit = match repo.fetch() {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            // No *new* commit since the last fetch — but the current
            // HEAD may still be unapplied (e.g. the very first sync
            // after cloning, where the clone already contains the
            // commit so there's nothing to "fetch"). Apply when HEAD
            // differs from what we last applied; otherwise skip.
            let head = repo.head_sha().ok();
            let up_to_date =
                matches!((head.as_deref(), last_applied_sha), (Some(h), Some(a)) if h == a);
            match (head, up_to_date) {
                (Some(head_sha), false) => match repo.commit_info(&head_sha) {
                    Ok(commit) => commit,
                    Err(_) => {
                        return skipped("HEAD unchanged");
                    }
                },
                _ => return skipped("HEAD unchanged"),
            }
        }
        Err(e) => {
            return SyncOutcome {
                commit: None,
                result: SyncResult::Failure {
                    error: e.to_string(),
                },
                diff_summary: None,
                changes: Vec::new(),
                file_errors: HashMap::new(),
            };
        }
    };

    // Step 2: Verify commit signature
    let mut commit = new_commit;
    if config.require_signed_commits {
        let status = verify::verify_commit(repo.path(), &commit, &config.trusted_signing_keys);
        commit.signature = status.clone();
        if !signature_admitted(&status, true) {
            return SyncOutcome {
                commit: Some(commit.clone()),
                result: SyncResult::Failure {
                    error: format!(
                        "commit {} rejected: require_signed_commits is set but the \
                         signature is {:?} (configure trusted_signing_keys and sign commits)",
                        commit.sha, commit.signature
                    ),
                },
                diff_summary: None,
                changes: Vec::new(),
                file_errors: HashMap::new(),
            };
        }
    }

    // Step 2b: Check for script field changes (auto-enforce signing)
    if !config.require_signed_commits {
        match verify::commit_modifies_script(repo.path(), &commit.sha, last_applied_sha) {
            Ok(true) => {
                let status =
                    verify::verify_commit(repo.path(), &commit, &config.trusted_signing_keys);
                commit.signature = status.clone();
                if !matches!(
                    status,
                    SignatureStatus::Verified | SignatureStatus::NotChecked
                ) {
                    return SyncOutcome {
                        commit: Some(commit.clone()),
                        result: SyncResult::Failure {
                            error: format!(
                                "commit {} modifies script field but is not signed",
                                commit.sha
                            ),
                        },
                        diff_summary: None,
                        changes: Vec::new(),
                        file_errors: HashMap::new(),
                    };
                }
            }
            Ok(false) => {}
            Err(e) => {
                return SyncOutcome {
                    commit: Some(commit.clone()),
                    result: SyncResult::Failure {
                        error: format!("failed to check script changes: {e}"),
                    },
                    diff_summary: None,
                    changes: Vec::new(),
                    file_errors: HashMap::new(),
                };
            }
        }
    }

    // Step 3: Parse TOML files
    let toml_files = match repo.list_toml_files(&commit.sha, &config.path) {
        Ok(files) => files,
        Err(e) => {
            return SyncOutcome {
                commit: Some(commit),
                result: SyncResult::Failure {
                    error: e.to_string(),
                },
                diff_summary: None,
                changes: Vec::new(),
                file_errors: HashMap::new(),
            };
        }
    };

    let (git_config, file_errors) = parse_toml_files(&toml_files);

    // A parse or duplicate-resource error means the merged config is
    // INCOMPLETE: the resources declared in the failed files are absent, so a
    // diff would emit `Remove` for every one of them and the runner would
    // delete live workloads on a single typo, advancing the applied commit as
    // it went. Fail closed — apply nothing and don't advance the commit until
    // the whole tree parses cleanly.
    if !file_errors.is_empty() {
        let mut reasons: Vec<String> = file_errors
            .iter()
            .map(|(file, error)| format!("{file}: {error}"))
            .collect();
        reasons.sort();
        return SyncOutcome {
            commit: Some(commit),
            result: SyncResult::Failure {
                error: format!(
                    "refusing to apply an incomplete config ({} file(s) failed to parse): {}",
                    reasons.len(),
                    reasons.join("; ")
                ),
            },
            diff_summary: None,
            changes: Vec::new(),
            file_errors,
        };
    }

    // Step 3b: Validate exactly as manual `apply` does (config.rs
    // `validate_against`), against the union of the repo's namespaces and the
    // already-committed ones. Without this, a config that `relish apply`
    // rejects — an inverted `[autoscale]` range, a `request > limit` resource
    // spec, a permission/build targeting an unknown namespace — was committed
    // straight to desired state via git and then silently ignored downstream.
    let known_namespaces: Vec<String> = current_namespaces.keys().cloned().collect();
    if let Err(e) = git_config.validate_against(&known_namespaces) {
        return SyncOutcome {
            commit: Some(commit),
            result: SyncResult::Failure {
                error: format!("config validation failed: {e}"),
            },
            diff_summary: None,
            changes: Vec::new(),
            file_errors,
        };
    }

    // Step 4: Compute diff (only on a fully-parsed, valid config).
    let current = CurrentState {
        apps: current_apps,
        namespaces: current_namespaces,
        permissions: current_permissions,
    };
    let (changes, summary) = diff::compute_diff(&git_config, &current, autoscale_overrides);

    SyncOutcome {
        commit: Some(commit),
        result: SyncResult::Success,
        diff_summary: Some(summary),
        changes,
        file_errors,
    }
}

/// Parse a set of TOML files into a merged Config.
///
/// Returns the merged config and a map of per-file errors.
///
/// Files merge in **sorted path order**, not the arbitrary order a
/// `HashMap` iterates in (GIT4). Two files declaring the same resource
/// used to resolve to whichever the hash happened to visit last, so an
/// identical repo could converge differently between nodes or runs. A
/// deterministic order fixes that, and a duplicate resource across files
/// is now surfaced as a per-file error naming the collision rather than
/// silently overwritten.
fn parse_toml_files(files: &HashMap<String, String>) -> (Config, HashMap<String, String>) {
    let mut merged = Config::default();
    let mut errors = HashMap::new();

    // Sort by path so the merge is deterministic across nodes and runs.
    let mut ordered: Vec<(&String, &String)> = files.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));

    for (path, content) in ordered {
        let file_config = match Config::parse(content) {
            Ok(config) => config,
            Err(e) => {
                errors.insert(path.clone(), e.to_string());
                continue;
            }
        };

        // A resource named in two files is ambiguous: report it against
        // this later-sorted file and let the earlier definition stand,
        // rather than silently letting hash order pick a winner.
        if let Some(duplicate) = first_duplicate(&merged, &file_config) {
            errors.insert(
                path.clone(),
                format!("duplicate resource {duplicate} already declared in an earlier file"),
            );
            continue;
        }

        merged.app.extend(file_config.app);
        merged.job.extend(file_config.job);
        merged.namespace.extend(file_config.namespace);
        merged.permission.extend(file_config.permission);
        merged.build.extend(file_config.build);
    }

    (merged, errors)
}

/// The first resource in `incoming` that `merged` already declares, if
/// any. Returns a `kind.name` label for the error message.
fn first_duplicate(merged: &Config, incoming: &Config) -> Option<String> {
    for name in incoming.app.keys() {
        if merged.app.contains_key(name) {
            return Some(format!("app.{name}"));
        }
    }
    for name in incoming.job.keys() {
        if merged.job.contains_key(name) {
            return Some(format!("job.{name}"));
        }
    }
    for name in incoming.namespace.keys() {
        if merged.namespace.contains_key(name) {
            return Some(format!("namespace.{name}"));
        }
    }
    for name in incoming.permission.keys() {
        if merged.permission.contains_key(name) {
            return Some(format!("permission.{name}"));
        }
    }
    for name in incoming.build.keys() {
        if merged.build.contains_key(name) {
            return Some(format!("build.{name}"));
        }
    }
    None
}

/// Compute the back-off delay for consecutive failures.
///
/// Exponential: base_interval * 2^failures, capped at base * 8.
pub fn backoff_delay(base_interval: Duration, consecutive_failures: u32) -> Duration {
    let multiplier = 2u32.saturating_pow(consecutive_failures).min(8);
    base_interval * multiplier
}

/// Decide the GitOps coordinator when `leader` is the node driving syncs.
///
/// The coordinator **is** the Raft leader. Only the leader can write desired
/// state, so only it can apply a sync; the sync loop is leader-gated for
/// exactly that reason. Making the coordinator anything other than the leader
/// would name a node that never actually runs a sync — which is what the old
/// "pick a non-leader" selection did, and why the field read as fiction.
///
/// Failover therefore rides on Raft leader election: when a new node wins
/// leadership its loop takes over and records itself here. The returned reason
/// distinguishes the first election (`Initial`) from a handover (`Failover`)
/// so operators can see the coordinator move.
pub fn coordinator_for_leader(
    leader: &str,
    previous: Option<&str>,
    now_ms: u64,
) -> CoordinatorElection {
    let reason = match previous {
        Some(prev) if prev != leader => CoordinatorElectionReason::Failover,
        _ => CoordinatorElectionReason::Initial,
    };
    CoordinatorElection {
        node_id: leader.to_string(),
        reason,
        timestamp: now_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_toml_files_success() {
        let mut files = HashMap::new();
        files.insert(
            "app.toml".to_string(),
            "[app.web]\nimage = \"myapp:v1\"\n".to_string(),
        );
        files.insert(
            "job.toml".to_string(),
            "[job.migrate]\nimage = \"migrate:v1\"\n".to_string(),
        );

        let (config, errors) = parse_toml_files(&files);
        assert!(errors.is_empty());
        assert_eq!(config.app.len(), 1);
        assert_eq!(config.job.len(), 1);
    }

    #[test]
    fn parse_toml_files_partial_error() {
        let mut files = HashMap::new();
        files.insert(
            "good.toml".to_string(),
            "[app.web]\nimage = \"myapp:v1\"\n".to_string(),
        );
        files.insert("bad.toml".to_string(), "not valid toml [[[".to_string());

        let (config, errors) = parse_toml_files(&files);
        assert_eq!(config.app.len(), 1, "good file should be parsed");
        assert_eq!(errors.len(), 1, "bad file should produce error");
        assert!(errors.contains_key("bad.toml"));
    }

    /// GIT4: a resource declared in two files resolves deterministically —
    /// the earlier-sorted file wins, and the later one is reported as a
    /// duplicate rather than silently overwriting via hash order.
    #[test]
    fn duplicate_resource_across_files_is_deterministic_and_reported() {
        let mut files = HashMap::new();
        files.insert(
            "a-first.toml".to_string(),
            "[app.web]\nimage = \"web:v1\"\n".to_string(),
        );
        files.insert(
            "b-second.toml".to_string(),
            "[app.web]\nimage = \"web:v2\"\n".to_string(),
        );

        let (config, errors) = parse_toml_files(&files);
        // The alphabetically-earlier file's definition stands.
        assert_eq!(config.app.len(), 1);
        assert_eq!(config.app["web"].image.as_deref(), Some("web:v1"));
        // The later file's collision is surfaced, not swallowed.
        assert!(errors.contains_key("b-second.toml"));
        assert!(errors["b-second.toml"].contains("duplicate resource app.web"));
    }

    /// C11: with `require_signed_commits`, verification must fail closed —
    /// only `Verified` is admissible. `NotChecked` (empty trusted keys or git
    /// unavailable) must be refused, not silently applied.
    #[test]
    fn require_signed_commits_admits_only_verified() {
        assert!(signature_admitted(&SignatureStatus::Verified, true));
        for status in [
            SignatureStatus::NotChecked,
            SignatureStatus::Unsigned,
            SignatureStatus::UntrustedKey,
            SignatureStatus::InvalidSignature,
        ] {
            assert!(
                !signature_admitted(&status, true),
                "{status:?} must be refused when signatures are required"
            );
        }
    }

    #[test]
    fn without_required_signing_any_status_is_admitted() {
        for status in [
            SignatureStatus::Verified,
            SignatureStatus::NotChecked,
            SignatureStatus::Unsigned,
        ] {
            assert!(signature_admitted(&status, false));
        }
    }

    /// C10: a parse error in one file must fail the whole sync and emit NO
    /// changes — never a `Remove` for the resources the broken file would have
    /// declared, which would delete live workloads and advance the commit.
    #[test]
    fn parse_error_fails_closed_and_emits_no_removals() {
        use std::process::Command;

        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().join("repo.git");
        let work = dir.path().join("work");
        let run = |args: &[&str], cwd: &std::path::Path| {
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
        };
        Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&bare)
            .output()
            .unwrap();
        Command::new("git")
            .args(["clone"])
            .arg(&bare)
            .arg(&work)
            .output()
            .unwrap();
        run(&["config", "user.email", "t@t.test"], &work);
        run(&["config", "user.name", "T"], &work);
        run(&["checkout", "-B", "main"], &work);
        std::fs::write(work.join("keep.toml"), "[app.web]\nimage = \"web:v1\"\n").unwrap();
        // Intended to declare `app.critical`, but the string is unterminated.
        std::fs::write(work.join("broken.toml"), "[app.critical]\nimage = \"oops\n").unwrap();
        run(&["add", "."], &work);
        run(&["commit", "-m", "init"], &work);
        run(&["push", "origin", "main"], &work);

        let url = format!("file://{}", bare.display());
        let repo = GitRepo::clone_or_open(&url, &dir.path().join("clone"), "main").unwrap();
        let config = GitOpsConfig {
            repo: url,
            branch: "main".to_string(),
            path: "/".to_string(),
            poll_interval_secs: 30,
            require_signed_commits: false,
            trusted_signing_keys: vec![],
            webhook_secret: None,
            recursive: false,
            webhook_rate_limit: 10,
        };

        // Raft currently holds `critical` (from the now-broken file) and `web`.
        // A naive incomplete diff would `Remove` `critical`.
        let mut current: HashMap<AppId, AppSpec> = HashMap::new();
        current.insert(
            AppId::new("critical", "default"),
            toml::from_str(r#"image = "c:v1""#).unwrap(),
        );
        current.insert(
            AppId::new("web", "default"),
            toml::from_str(r#"image = "web:v1""#).unwrap(),
        );
        let namespaces = BTreeMap::new();
        let permissions = BTreeMap::new();

        let outcome = execute_sync(
            &repo,
            &config,
            &current,
            &namespaces,
            &permissions,
            &[],
            None,
        );

        assert!(
            matches!(outcome.result, SyncResult::Failure { .. }),
            "a parse error must fail the sync, got {:?}",
            outcome.result
        );
        assert!(
            outcome.changes.is_empty(),
            "a failed parse must emit no changes — never a Remove of a live app"
        );
        assert!(outcome.file_errors.contains_key("broken.toml"));
    }

    /// C13: a config that parses but fails validation (here, a namespace with
    /// a zero cap, which `relish apply` rejects) must fail the GitOps sync with
    /// no changes — never committed straight to desired state.
    #[test]
    fn invalid_config_fails_the_sync_via_the_same_validation_as_apply() {
        use std::process::Command;

        let dir = tempfile::TempDir::new().unwrap();
        let bare = dir.path().join("repo.git");
        let work = dir.path().join("work");
        let run = |args: &[&str], cwd: &std::path::Path| {
            Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap();
        };
        Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&bare)
            .output()
            .unwrap();
        Command::new("git")
            .args(["clone"])
            .arg(&bare)
            .arg(&work)
            .output()
            .unwrap();
        run(&["config", "user.email", "t@t.test"], &work);
        run(&["config", "user.name", "T"], &work);
        run(&["checkout", "-B", "main"], &work);
        // Valid TOML, invalid semantics: a zero namespace cap.
        std::fs::write(work.join("ns.toml"), "[namespace.prod]\nmax_apps = 0\n").unwrap();
        run(&["add", "."], &work);
        run(&["commit", "-m", "init"], &work);
        run(&["push", "origin", "main"], &work);

        let url = format!("file://{}", bare.display());
        let repo = GitRepo::clone_or_open(&url, &dir.path().join("clone"), "main").unwrap();
        let config = GitOpsConfig {
            repo: url,
            branch: "main".to_string(),
            path: "/".to_string(),
            poll_interval_secs: 30,
            require_signed_commits: false,
            trusted_signing_keys: vec![],
            webhook_secret: None,
            recursive: false,
            webhook_rate_limit: 10,
        };

        let outcome = execute_sync(
            &repo,
            &config,
            &HashMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &[],
            None,
        );

        assert!(
            matches!(outcome.result, SyncResult::Failure { .. }),
            "an invalid config must fail the sync, got {:?}",
            outcome.result
        );
        assert!(
            outcome.changes.is_empty(),
            "a validation failure must emit no changes"
        );
    }

    #[test]
    fn coordinator_is_the_leader_and_first_election_is_initial() {
        // No previous coordinator: the leader takes the role fresh.
        let election = coordinator_for_leader("node-01", None, 1000);
        assert_eq!(election.node_id, "node-01");
        assert_eq!(election.reason, CoordinatorElectionReason::Initial);

        // Same leader as before: still Initial (no handover happened).
        let unchanged = coordinator_for_leader("node-01", Some("node-01"), 1000);
        assert_eq!(unchanged.reason, CoordinatorElectionReason::Initial);
    }

    #[test]
    fn coordinator_change_records_a_failover() {
        // Leadership moved from node-01 to node-02; the new leader records the
        // handover so operators see the coordinator move.
        let election = coordinator_for_leader("node-02", Some("node-01"), 2000);
        assert_eq!(election.node_id, "node-02");
        assert_eq!(election.reason, CoordinatorElectionReason::Failover);
        assert_eq!(election.timestamp, 2000);
    }

    #[test]
    fn backoff_zero_failures() {
        let base = Duration::from_secs(30);
        assert_eq!(backoff_delay(base, 0), Duration::from_secs(30));
    }

    #[test]
    fn backoff_one_failure() {
        let base = Duration::from_secs(30);
        assert_eq!(backoff_delay(base, 1), Duration::from_secs(60));
    }

    #[test]
    fn backoff_three_failures() {
        let base = Duration::from_secs(30);
        assert_eq!(backoff_delay(base, 3), Duration::from_secs(240));
    }

    #[test]
    fn backoff_capped_at_8x() {
        let base = Duration::from_secs(30);
        assert_eq!(backoff_delay(base, 10), Duration::from_secs(240));
    }
}
