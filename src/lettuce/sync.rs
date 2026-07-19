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

/// Execute a single sync cycle.
///
/// This is the pure logic of the sync loop, separated from the
/// async runtime and Raft interaction so it can be tested in isolation.
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

    // Step 4: Compute diff
    let current = CurrentState {
        apps: current_apps,
        namespaces: current_namespaces,
        permissions: current_permissions,
    };
    let (changes, summary) = diff::compute_diff(&git_config, &current, autoscale_overrides);

    // Step 5: Determine result
    let result = if file_errors.is_empty() {
        SyncResult::Success
    } else {
        SyncResult::PartialSuccess {
            errors: file_errors.values().cloned().collect(),
        }
    };

    SyncOutcome {
        commit: Some(commit),
        result,
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
