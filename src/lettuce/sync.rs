//! Core sync loop for Lettuce GitOps.
//!
//! The sync loop runs on the coordinator node, triggered by either
//! the poll timer or a webhook signal. It fetches from git, verifies
//! signatures, parses TOML, diffs against Raft state, and applies
//! only changed resources.

use std::collections::HashMap;
use std::time::Duration;

use crate::config::Config;
use crate::config::app::AppSpec;
use crate::meat::types::AppId;

use super::diff::{self, ResourceChange};
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

/// Execute a single sync cycle.
///
/// This is the pure logic of the sync loop, separated from the
/// async runtime and Raft interaction so it can be tested in isolation.
pub fn execute_sync(
    repo: &GitRepo,
    config: &GitOpsConfig,
    current_apps: &HashMap<AppId, AppSpec>,
    autoscale_overrides: &[(String, u32)],
    last_applied_sha: Option<&str>,
) -> SyncOutcome {
    // Step 1: Fetch
    let new_commit = match repo.fetch() {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            return SyncOutcome {
                commit: None,
                result: SyncResult::Skipped {
                    reason: "HEAD unchanged".to_string(),
                },
                diff_summary: None,
                changes: Vec::new(),
                file_errors: HashMap::new(),
            };
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
        match status {
            SignatureStatus::Verified | SignatureStatus::NotChecked => {}
            _ => {
                return SyncOutcome {
                    commit: Some(commit.clone()),
                    result: SyncResult::Failure {
                        error: format!("commit {} signature: {:?}", commit.sha, commit.signature),
                    },
                    diff_summary: None,
                    changes: Vec::new(),
                    file_errors: HashMap::new(),
                };
            }
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
    let (changes, summary) = diff::compute_diff(&git_config, current_apps, autoscale_overrides);

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
fn parse_toml_files(files: &HashMap<String, String>) -> (Config, HashMap<String, String>) {
    let mut merged = Config::default();
    let mut errors = HashMap::new();

    for (path, content) in files {
        match Config::parse(content) {
            Ok(file_config) => {
                merged.app.extend(file_config.app);
                merged.job.extend(file_config.job);
                merged.namespace.extend(file_config.namespace);
                merged.permission.extend(file_config.permission);
                merged.build.extend(file_config.build);
            }
            Err(e) => {
                errors.insert(path.clone(), e.to_string());
            }
        }
    }

    (merged, errors)
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
