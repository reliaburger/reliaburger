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
    /// Configuration tree errors that refused the sync.
    pub file_errors: HashMap<String, String>,
}

/// A failed outcome for `commit` with the given error.
fn failed(commit: Option<CommitInfo>, error: String) -> SyncOutcome {
    SyncOutcome {
        commit,
        result: SyncResult::Failure { error },
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
///
/// Every cycle reconciles, whether or not Git moved (B16). The old loop
/// returned "HEAD unchanged" as soon as the fetch found nothing new, so a
/// manual change to an app (a different image, a deleted app) stood until
/// the next commit. Now the cycle always diffs the current commit's config
/// against the cluster's desired state; when the two already agree the
/// diff is empty and the runner writes nothing.
pub fn execute_sync(
    repo: &GitRepo,
    config: &GitOpsConfig,
    current_apps: &HashMap<AppId, AppSpec>,
    current_namespaces: &BTreeMap<String, NamespaceSpec>,
    current_permissions: &BTreeMap<String, PermissionSpec>,
    autoscale_overrides: &[(String, u32)],
    last_applied_sha: Option<&str>,
) -> SyncOutcome {
    // Step 1: Fetch, then settle on the commit to reconcile: the new one,
    // or the current HEAD when nothing new arrived.
    let fetched = repo.fetch().and_then(|new_commit| match new_commit {
        Some(commit) => Ok(commit),
        None => repo.head_sha().and_then(|head| repo.commit_info(&head)),
    });
    let mut commit = match fetched {
        Ok(commit) => commit,
        Err(e) => return failed(None, e.to_string()),
    };

    // Step 2: Verify commit signature
    if config.require_signed_commits {
        let status = match verify::verify_commit(repo, &commit, &config.trusted_signing_keys) {
            Ok(status) => status,
            Err(e) => {
                let error = format!("commit {} signature check failed: {e}", commit.sha);
                return failed(Some(commit), error);
            }
        };
        commit.signature = status.clone();
        if !signature_admitted(&status, true) {
            let error = format!(
                "commit {} rejected: require_signed_commits is set but the \
                 signature is {:?} (configure trusted_signing_keys and sign commits)",
                commit.sha, commit.signature
            );
            return failed(Some(commit), error);
        }
    }

    // Step 3: Parse TOML files
    let toml_files = match repo.list_toml_files(&commit.sha, &config.path) {
        Ok(files) => files,
        Err(e) => return failed(Some(commit), e.to_string()),
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
        return failed(Some(commit), format!("config validation failed: {e}"));
    }

    // Jobs have no durable GitOps execution identity or dispatch path. Refuse
    // the entire validated tree before a dependent app can become desired state.
    if !git_config.job.is_empty() {
        let jobs = git_config
            .job
            .keys()
            .map(|name| format!("job.{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        return failed(
            Some(commit),
            format!(
                "GitOps does not reconcile jobs ({jobs}); use relish apply for jobs and run_before migrations, or relish batch for batch submission"
            ),
        );
    }

    // Step 3c: A script change needs a trusted signature even when
    // signing isn't required globally.
    if !config.require_signed_commits
        && let Err(error) =
            admit_script_changes(repo, config, &mut commit, &toml_files, last_applied_sha)
    {
        return failed(Some(commit), error);
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

/// Admit `commit` under the script-signing rule, or say why not.
///
/// A commit that adds, edits or removes any `script` value must carry a
/// signature from a trusted key, whatever `require_signed_commits` says
/// (M20). The comparison is semantic (B13): the scripts parsed out of the
/// last applied tree against the scripts parsed out of this one. The old
/// check searched the diff text for an added line containing `script`,
/// which missed an edit inside a multiline body, a deletion, and a `git
/// diff` that failed with empty output.
///
/// When the comparison can't be completed (the previous tree can't be
/// read or parsed) the commit is treated as changing scripts: only a
/// verified signature admits it. On the first sync there is no previous
/// tree, so any script at all needs a signature.
fn admit_script_changes(
    repo: &GitRepo,
    config: &GitOpsConfig,
    commit: &mut CommitInfo,
    candidate: &HashMap<String, String>,
    last_applied_sha: Option<&str>,
) -> Result<(), String> {
    let comparison = match last_applied_sha {
        Some(previous) if previous == commit.sha => Ok(false),
        Some(previous) => repo
            .list_toml_files(previous, &config.path)
            .and_then(|previous| verify::scripts_changed(&previous, candidate)),
        None => verify::scripts_changed(&HashMap::new(), candidate),
    };
    let reason = match comparison {
        Ok(false) => return Ok(()),
        Ok(true) => "modifies a script field".to_string(),
        Err(e) => format!("can't be shown to leave scripts unchanged ({e})"),
    };

    let status =
        verify::verify_commit(repo, commit, &config.trusted_signing_keys).map_err(|e| {
            format!(
                "commit {} {reason}; signature check failed: {e}",
                commit.sha
            )
        })?;
    commit.signature = status.clone();
    // Only a *verified* signature admits it (M20). `NotChecked` means
    // verification never ran because no trusted keys are configured, the
    // common default, and admitting it would make this gate a no-op.
    if status == SignatureStatus::Verified {
        return Ok(());
    }
    Err(format!(
        "commit {} {reason} but has no verified signature (status: {status:?}); \
         configure [gitops] trusted_signing_keys and sign the commit",
        commit.sha
    ))
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
    use crate::relish::compile::{DuplicatePolicy, compile_sources};
    let snapshot = files
        .iter()
        .map(|(path, content)| (std::path::PathBuf::from(path), content.clone()))
        .collect();
    match compile_sources(&snapshot, DuplicatePolicy::Refuse) {
        Ok(result) => (result.config, HashMap::new()),
        Err(error) => (
            Config::default(),
            HashMap::from([(error.path.display().to_string(), error.message)]),
        ),
    }
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
    fn gitops_verified_commit_uses_the_watched_tree_defaults_and_namespaces() {
        let repository = SigningRepo::new();
        let root = repository.work.join("configs");
        std::fs::create_dir_all(root.join("team")).unwrap();
        std::fs::write(
            root.join("_defaults.toml"),
            "image = 'web:v2'\nmemory = '128Mi'\n[env]\nSOURCE = 'shared'\n",
        )
        .unwrap();
        std::fs::write(root.join("team/app.toml"), "[app.web]\nport = 8080\n").unwrap();
        let expected = crate::relish::compile::compile(&root).unwrap().config.app["web"].clone();
        // Independent fixture expectations also catch a defect shared by both callers.
        assert_eq!(expected.image.as_deref(), Some("web:v2"));
        assert_eq!(expected.namespace.as_deref(), Some("team"));
        assert_eq!(expected.port, Some(8080));
        assert_eq!(
            expected.memory.as_ref().map(|range| range.request),
            Some(128 * 1024 * 1024)
        );
        assert_eq!(
            expected.env["SOURCE"],
            crate::config::EnvValue::Plain("shared".into())
        );
        for signed in [false, true] {
            let sha = commit_in(&repository.work, "watched config tree", signed);
            let mut config = repository.config();
            config.path = "/configs".into();
            config.require_signed_commits = signed;
            let outcome = execute_sync(
                &repository.repo,
                &config,
                &HashMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &[],
                None,
            );
            assert!(
                matches!(outcome.result, SyncResult::Success),
                "signed={signed}: {:?}",
                outcome.result
            );
            assert_eq!(outcome.commit.as_ref().unwrap().sha, sha);
            if signed {
                assert_eq!(
                    outcome.commit.as_ref().unwrap().signature,
                    SignatureStatus::Verified
                );
            }
            assert_eq!(outcome.changes.len(), 1);
            let ResourceChange::Add {
                resource_id,
                spec: diff::ChangePayload::App(spec),
            } = &outcome.changes[0]
            else {
                panic!(
                    "expected exactly the watched app, got {:?}",
                    outcome.changes
                );
            };
            assert!(resource_id.contains("team"), "{resource_id}");
            assert_eq!(**spec, expected);
        }
    }

    #[test]
    fn gitops_tree_matches_cli_defaults_and_directory_namespaces() {
        let files = HashMap::from([
            (
                "_defaults.toml".into(),
                "image = 'web:v1'\nmemory = '128Mi'\n[env]\nPARENT = 'yes'\n".into(),
            ),
            (
                "team/_defaults.toml".into(),
                "cpu = '250m'\n[env]\nCHILD = 'yes'\n".into(),
            ),
            ("team/app.toml".into(), "[app.web]\nport = 8080\n".into()),
            (
                "team/deep/app.toml".into(),
                "[app.worker]\nimage = 'worker:v1'\n".into(),
            ),
        ]);
        let directory = tempfile::tempdir().unwrap();
        for (name, content) in &files {
            let file = directory.path().join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, content).unwrap();
        }
        let cli = crate::relish::compile::compile(directory.path())
            .unwrap()
            .config;
        let (git, errors) = parse_toml_files(&files);
        assert!(errors.is_empty(), "{errors:?}");
        // These literals describe the input, independently of the common resolver.
        assert_eq!(cli.app["web"].image.as_deref(), Some("web:v1"));
        assert_eq!(cli.app["worker"].image.as_deref(), Some("worker:v1"));
        assert_eq!(cli.app["web"].port, Some(8080));
        for app in ["web", "worker"] {
            let spec = &cli.app[app];
            assert_eq!(
                spec.memory.as_ref().map(|range| range.request),
                Some(128 * 1024 * 1024)
            );
            assert_eq!(spec.cpu.as_ref().map(|range| range.request), Some(250));
            assert_eq!(
                spec.env["PARENT"],
                crate::config::EnvValue::Plain("yes".into())
            );
            assert_eq!(
                spec.env["CHILD"],
                crate::config::EnvValue::Plain("yes".into())
            );
        }
        assert_eq!(git, cli);
        assert_eq!(git.app["web"].namespace.as_deref(), Some("team"));
        assert_eq!(git.app["worker"].namespace.as_deref(), Some("deep"));
    }

    #[test]
    fn gitops_directory_namespace_is_used_without_defaults() {
        let files = HashMap::from([(
            "team/app.toml".into(),
            "[app.web]\nimage = 'web:v1'\n".into(),
        )]);
        let (config, errors) = parse_toml_files(&files);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(config.app["web"].namespace.as_deref(), Some("team"));
    }

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
        assert!(
            config.app.is_empty(),
            "an incomplete snapshot must not escape"
        );
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
        // A duplicate refuses the whole resolved snapshot.
        assert!(config.app.is_empty());
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

    /// A real repository for the script-signing tests: a working clone that
    /// commits (signed with an SSH key, or not) and pushes to a bare
    /// remote, and a Lettuce clone that trusts that key.
    struct SigningRepo {
        _dir: tempfile::TempDir,
        work: std::path::PathBuf,
        repo: GitRepo,
        fingerprint: String,
    }

    impl SigningRepo {
        fn new() -> Self {
            use std::process::Command;

            let dir = tempfile::TempDir::new().unwrap();
            let bare = dir.path().join("repo.git");
            let work = dir.path().join("work");
            let key = dir.path().join("signing-key");
            let status = Command::new("ssh-keygen")
                .args([
                    "-q",
                    "-t",
                    "ed25519",
                    "-N",
                    "",
                    "-C",
                    "dev@example.com",
                    "-f",
                ])
                .arg(&key)
                .status()
                .unwrap();
            assert!(status.success(), "ssh-keygen failed");
            let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
            let listing = Command::new("ssh-keygen")
                .arg("-lf")
                .arg(key.with_extension("pub"))
                .output()
                .unwrap();
            let fingerprint = String::from_utf8_lossy(&listing.stdout)
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_string();
            let allowed = dir.path().join("allowed_signers");
            std::fs::write(&allowed, format!("dev@example.com {public}")).unwrap();

            git_ok(
                dir.path(),
                &["init", "-q", "--bare", "--initial-branch=main"],
                &bare,
            );
            git_ok(dir.path(), &["clone", "-q"], &bare);
            std::fs::rename(dir.path().join("repo"), &work).unwrap();
            for (key_name, value) in [
                ("user.email", "dev@example.com".to_string()),
                ("user.name", "Dev".to_string()),
                ("gpg.format", "ssh".to_string()),
                ("user.signingkey", key.display().to_string()),
                ("commit.gpgsign", "false".to_string()),
            ] {
                git_ok(
                    &work,
                    &["config", key_name, &value],
                    std::path::Path::new(""),
                );
            }
            git_ok(
                &work,
                &["checkout", "-q", "-B", "main"],
                std::path::Path::new(""),
            );
            std::fs::write(work.join("base.toml"), "[app.base]\nimage = \"base:v1\"\n").unwrap();
            commit_in(&work, "base", false);
            let url = format!("file://{}", bare.display());
            let clone = dir.path().join("clone");
            let repo = GitRepo::clone_or_open(&url, &clone, "main").unwrap();
            git_ok(
                &clone,
                &[
                    "config",
                    "gpg.ssh.allowedSignersFile",
                    &allowed.display().to_string(),
                ],
                std::path::Path::new(""),
            );
            Self {
                _dir: dir,
                work,
                repo,
                fingerprint,
            }
        }

        /// Write `apps.toml`, commit it (signed or not) and push. Returns
        /// the new commit's SHA.
        fn commit_apps(&mut self, content: &str, signed: bool) -> String {
            std::fs::write(self.work.join("apps.toml"), content).unwrap();
            commit_in(&self.work, "change apps", signed)
        }

        fn config(&self) -> GitOpsConfig {
            GitOpsConfig {
                repo: self.repo.url().to_string(),
                branch: "main".to_string(),
                path: "/".to_string(),
                poll_interval_secs: 30,
                require_signed_commits: false,
                trusted_signing_keys: vec![self.fingerprint.clone()],
                webhook_secret: None,
                webhook_rate_limit: 10,
            }
        }

        fn sync(&self, last_applied: Option<&str>) -> SyncOutcome {
            execute_sync(
                &self.repo,
                &self.config(),
                &HashMap::new(),
                &BTreeMap::new(),
                &BTreeMap::new(),
                &[],
                last_applied,
            )
        }
    }

    /// Commit everything in `work` (signed or not), push it, and return
    /// the new SHA.
    fn commit_in(work: &std::path::Path, message: &str, signed: bool) -> String {
        let empty = std::path::Path::new("");
        git_ok(work, &["add", "-A"], empty);
        let sign = if signed { "-S" } else { "--no-gpg-sign" };
        git_ok(
            work,
            &["commit", "-q", "--allow-empty", sign, "-m", message],
            empty,
        );
        git_ok(work, &["push", "-q", "origin", "main"], empty);
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(work)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Run `git args [extra]` in `cwd`, asserting success. An empty
    /// `extra` path is left off.
    fn git_ok(cwd: &std::path::Path, args: &[&str], extra: &std::path::Path) {
        let mut command = std::process::Command::new("git");
        command.args(args).current_dir(cwd);
        if !extra.as_os_str().is_empty() {
            command.arg(extra);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn assert_refused(outcome: &SyncOutcome, case: &str) {
        match &outcome.result {
            SyncResult::Failure { error } => assert!(
                error.contains("no verified signature"),
                "{case}: refused for the wrong reason: {error}"
            ),
            other => panic!("{case}: an unsigned script change was admitted: {other:?}"),
        }
        assert!(
            outcome.changes.is_empty(),
            "{case}: a refusal emits no changes"
        );
    }

    fn assert_admitted(outcome: &SyncOutcome, case: &str) {
        assert!(
            matches!(outcome.result, SyncResult::Success),
            "{case}: a signed script change was refused: {:?}",
            outcome.result
        );
    }

    const MULTILINE_OLD: &str =
        "[app.web]\nreplicas = 1\nscript = \"\"\"\nset -e\necho old\n\"\"\"\n";
    const MULTILINE_NEW: &str =
        "[app.web]\nreplicas = 1\nscript = \"\"\"\nset -e\necho new\n\"\"\"\n";
    const LITERAL_OLD: &str = "[app.web]\nreplicas = 1\nscript = '''\nset -e\necho old\n'''\n";
    const LITERAL_NEW: &str = "[app.web]\nreplicas = 1\nscript = '''\nset -e\necho new\n'''\n";
    const BASIC_OLD: &str = "[app.web]\nreplicas = 1\nscript = \"echo old\"\n";
    const BASIC_NEW: &str = "[app.web]\nreplicas = 1\nscript = \"echo new\"\n";
    const NO_SCRIPT: &str = "[app.web]\nimage = \"web:v1\"\n";

    /// B13: every way of changing a script, relative to an applied commit,
    /// is refused unsigned and admitted with a trusted signature. The old
    /// check only saw an added diff line containing `script`, so editing a
    /// multiline body or deleting the script slipped through unsigned.
    #[test]
    fn every_script_change_needs_a_trusted_signature() {
        let cases = [
            ("multiline body edit", MULTILINE_OLD, MULTILINE_NEW),
            ("literal string body edit", LITERAL_OLD, LITERAL_NEW),
            ("basic string edit", BASIC_OLD, BASIC_NEW),
            ("script deleted", MULTILINE_OLD, NO_SCRIPT),
            ("script added", NO_SCRIPT, BASIC_NEW),
        ];
        for (case, before, after) in cases {
            let mut repo = SigningRepo::new();
            let applied = repo.commit_apps(before, true);

            repo.commit_apps(after, false);
            assert_refused(&repo.sync(Some(&applied)), case);

            repo.commit_apps(after, true);
            assert_admitted(&repo.sync(Some(&applied)), case);
        }
    }

    /// B13 negative control: an unsigned commit that leaves every script
    /// alone is admitted, even when the file holds scripts.
    #[test]
    fn an_unsigned_change_that_leaves_scripts_alone_is_admitted() {
        let mut repo = SigningRepo::new();
        let applied = repo.commit_apps(MULTILINE_OLD, true);
        let reformatted = MULTILINE_OLD.replace("replicas = 1", "replicas = 2");
        repo.commit_apps(&reformatted, false);
        assert_admitted(&repo.sync(Some(&applied)), "replicas-only change");
    }

    /// B13: on the first sync there is nothing to compare against, so a
    /// script present at all needs a signature.
    #[test]
    fn a_script_on_the_initial_sync_needs_a_trusted_signature() {
        let mut repo = SigningRepo::new();
        repo.commit_apps(MULTILINE_OLD, false);
        assert_refused(&repo.sync(None), "initial sync, unsigned");

        repo.commit_apps(MULTILINE_OLD, true);
        assert_admitted(&repo.sync(None), "initial sync, signed");
    }

    /// B13: when the previous tree can't be read, the comparison is
    /// incomplete, and an incomplete comparison is never read as "no
    /// script change".
    #[test]
    fn an_unreadable_previous_tree_needs_a_trusted_signature() {
        let mut repo = SigningRepo::new();
        let missing = "0123456789abcdef0123456789abcdef01234567";

        repo.commit_apps(NO_SCRIPT, false);
        let outcome = repo.sync(Some(missing));
        match &outcome.result {
            SyncResult::Failure { error } => {
                assert!(error.contains("can't be shown"), "got: {error}")
            }
            other => panic!("an incomplete comparison was admitted: {other:?}"),
        }

        repo.commit_apps(NO_SCRIPT, true);
        assert_admitted(&repo.sync(Some(missing)), "unreadable previous, signed");
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
    #[test]
    fn gitops_refuses_jobs_before_reporting_a_successful_app_sync() {
        for signed in [false, true] {
            let mut repository = SigningRepo::new();
            let sha = repository.commit_apps(
                "[app.web]\nimage = 'web:v1'\n[job.migrate]\nimage = 'migrate:v1'\n",
                signed,
            );
            let mut config = repository.config();
            config.require_signed_commits = signed;
            for last_applied in [None, Some(sha.as_str())] {
                let outcome = execute_sync(
                    &repository.repo,
                    &config,
                    &HashMap::new(),
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                    &[],
                    last_applied,
                );
                let SyncResult::Failure { error } = &outcome.result else {
                    panic!(
                        "signed={signed}, last={last_applied:?}: unsupported jobs were silently accepted: {:?}",
                        outcome.result
                    );
                };
                assert!(error.contains("GitOps does not reconcile jobs"), "{error}");
                assert!(error.contains("job.migrate"), "{error}");
                assert!(
                    error.contains("relish apply") && error.contains("relish batch"),
                    "{error}"
                );
                assert!(
                    outcome.changes.is_empty(),
                    "partial app changes escaped: {:?}",
                    outcome.changes
                );
                assert!(outcome.diff_summary.is_none());
                if signed {
                    assert_eq!(outcome.commit.unwrap().signature, SignatureStatus::Verified);
                }
            }
        }
    }

    #[test]
    fn gitops_refuses_migration_prerequisites_and_standalone_jobs() {
        for manifest in [
            "[job.migrate]\nimage = 'migrate:v1'\n",
            "[app.web]\nimage = 'web:v1'\n[job.migrate]\nimage = 'migrate:v1'\nrun_before = ['app.web']\n",
            "[job.migrate]\nimage = 'migrate:v1'\nschedule = '* * * * *'\n",
        ] {
            let mut repository = SigningRepo::new();
            // A job-only watched tree must also refuse, even with no app diff.
            std::fs::remove_file(repository.work.join("base.toml")).unwrap();
            repository.commit_apps(manifest, false);
            let outcome = repository.sync(None);
            let SyncResult::Failure { error } = &outcome.result else {
                panic!(
                    "unsupported migration/jobs reported success: {:?}",
                    outcome.result
                );
            };
            assert!(
                error.contains("GitOps does not reconcile jobs") && error.contains("job.migrate"),
                "{error}"
            );
            assert!(outcome.changes.is_empty());
            assert!(outcome.diff_summary.is_none());
        }
    }
}
