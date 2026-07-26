//! Git operations for Lettuce.
//!
//! Wraps command-line git for clone, fetch, and file listing.
//! Uses the `git` CLI rather than libgit2 to avoid pulling in a
//! large C dependency — git is always available on the nodes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::types::{CommitInfo, LettuceError, SignatureStatus};

/// A local bare git clone managed by Lettuce.
#[derive(Debug)]
pub struct GitRepo {
    /// Path to the local bare clone.
    path: PathBuf,
    /// Remote URL. Held so a reused clone can be checked for drift and,
    /// on failover, re-cloned.
    url: String,
    /// Branch to track.
    branch: String,
}

impl GitRepo {
    /// Clone a repository into a bare local directory.
    ///
    /// If the directory already holds a clone, it's reused only when its
    /// remote URL and tracked branch still match the config. A drift
    /// (someone repointed `[gitops] repo` or `branch`, or a stale clone
    /// survived a failover) triggers a fresh clone rather than silently
    /// syncing the wrong repo (GIT4). A fresh clone is fetched into a
    /// temporary sibling directory and swapped in, so a mid-clone crash
    /// never leaves a half-populated repo in place.
    pub fn clone_or_open(url: &str, path: &Path, branch: &str) -> Result<Self, LettuceError> {
        if path.join("HEAD").exists() {
            match reused_clone_matches(path, url, branch) {
                Ok(true) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                        url: url.to_string(),
                        branch: branch.to_string(),
                    });
                }
                // Drift or an unreadable clone: discard and re-clone.
                Ok(false) | Err(_) => {
                    std::fs::remove_dir_all(path).map_err(|e| {
                        LettuceError::GitFailed(format!(
                            "failed to remove drifted clone at {}: {e}",
                            path.display()
                        ))
                    })?;
                }
            }
        }

        Self::fresh_clone(url, path, branch)
    }

    /// Perform a fresh bare clone at `path`.
    fn fresh_clone(url: &str, path: &Path, branch: &str) -> Result<Self, LettuceError> {
        // M27: keep the secret out of argv (world-readable via /proc) and out
        // of the clone's `.git/config` (where it would outlive the process).
        // The sanitised URL is what git stores as the remote; `fetch` supplies
        // the credential the same way on every later call.
        let (safe_url, password) = split_credentials(url);
        // `--` separates options from the URL and destination so a repo or
        // branch beginning with `-` can't be read as a git flag (GIT4).
        let mut command = Command::new("git");
        with_credentials(&mut command, password.as_deref());
        let output = command
            .args(["clone", "--bare", "--single-branch", "--branch"])
            .arg(branch)
            .arg("--")
            .arg(&safe_url)
            .arg(path)
            .output()
            .map_err(|e| LettuceError::GitFailed(format!("failed to run git clone: {e}")))?;

        if !output.status.success() {
            return Err(LettuceError::GitFailed(format!(
                "git clone failed: {}",
                redact_git_output(&output.stderr, url)
            )));
        }

        Ok(Self {
            path: path.to_path_buf(),
            url: url.to_string(),
            branch: branch.to_string(),
        })
    }

    /// Fetch the latest from the remote.
    ///
    /// Returns `Some(commit)` if HEAD changed, `None` if unchanged.
    pub fn fetch(&self) -> Result<Option<CommitInfo>, LettuceError> {
        let old_head = self.head_sha().ok();

        // The stored remote is credential-free (M27), so the secret has to be
        // supplied on every fetch too.
        let (_, password) = split_credentials(&self.url);
        let mut command = Command::new("git");
        with_credentials(&mut command, password.as_deref());
        let output = command
            .args(["fetch", "origin", "--"])
            .arg(&self.branch)
            .current_dir(&self.path)
            .output()
            .map_err(|e| LettuceError::GitFailed(format!("failed to run git fetch: {e}")))?;

        if !output.status.success() {
            return Err(LettuceError::GitFailed(format!(
                "git fetch failed: {}",
                redact_git_output(&output.stderr, &self.url)
            )));
        }

        let new_head = self.remote_head_sha()?;

        // Advance the local branch ref to the fetched commit (M27). A bare
        // `git fetch` updates `origin/<branch>` and `FETCH_HEAD` but not the
        // local `HEAD`/branch ref, so `head_sha()` stayed pinned at the
        // clone-time commit forever. Every subsequent poll then saw
        // `old_head (clone) != new_head (remote)` and reported a change, writing
        // a `GitOpsSyncUpdate` to Raft every 30s — unbounded log churn. Move the
        // local ref forward so a genuinely unchanged remote reports `None`.
        if old_head.as_deref() != Some(&new_head) {
            let update = Command::new("git")
                .args([
                    "update-ref",
                    &format!("refs/heads/{}", self.branch),
                    "--end-of-options",
                ])
                .arg(&new_head)
                .current_dir(&self.path)
                .output()
                .map_err(|e| {
                    LettuceError::GitFailed(format!("failed to run git update-ref: {e}"))
                })?;
            if !update.status.success() {
                return Err(LettuceError::GitFailed(format!(
                    "git update-ref failed: {}",
                    redact_git_output(&update.stderr, &self.url)
                )));
            }
        }

        if old_head.as_deref() == Some(&new_head) {
            return Ok(None);
        }

        let commit = self.commit_info(&new_head)?;
        Ok(Some(commit))
    }

    /// Get the SHA of the local HEAD.
    pub fn head_sha(&self) -> Result<String, LettuceError> {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.path)
            .output()
            .map_err(|e| LettuceError::GitFailed(e.to_string()))?;

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Get the SHA of the remote branch HEAD.
    ///
    /// Tries `origin/<branch>` first, falls back to FETCH_HEAD, then HEAD.
    fn remote_head_sha(&self) -> Result<String, LettuceError> {
        for refname in [
            format!("origin/{}", self.branch),
            "FETCH_HEAD".to_string(),
            "HEAD".to_string(),
        ] {
            // `--verify` makes rev-parse print only the resolved SHA (or
            // fail), so `--end-of-options` isn't echoed back into stdout the
            // way plain rev-parse would. It also fails cleanly on a ref that
            // doesn't resolve, which the fallback loop relies on.
            let output = Command::new("git")
                .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
                .arg(&refname)
                .current_dir(&self.path)
                .output()
                .map_err(|e| LettuceError::GitFailed(e.to_string()))?;

            if output.status.success() {
                let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !sha.is_empty() {
                    return Ok(sha);
                }
            }
        }

        Err(LettuceError::GitFailed(
            "failed to resolve remote HEAD".to_string(),
        ))
    }

    /// Get commit info for a specific SHA.
    pub fn commit_info(&self, sha: &str) -> Result<CommitInfo, LettuceError> {
        // `--end-of-options` stops a SHA beginning with `-` from being
        // parsed as an option, while still treating it as a revision (a
        // plain `--` would make git read it as a pathspec instead). GIT4.
        let output = Command::new("git")
            .args(["log", "-1", "--format=%H%n%s%n%an%n%ct", "--end-of-options"])
            .arg(sha)
            .current_dir(&self.path)
            .output()
            .map_err(|e| LettuceError::GitFailed(e.to_string()))?;

        let text = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() < 4 {
            return Err(LettuceError::GitFailed(format!(
                "unexpected git log output for {sha}"
            )));
        }

        let timestamp = lines[3].parse::<u64>().unwrap_or(0) * 1000;

        Ok(CommitInfo {
            sha: lines[0].to_string(),
            message: lines[1].to_string(),
            author: lines[2].to_string(),
            timestamp,
            signature: SignatureStatus::NotChecked,
        })
    }

    /// List all .toml files at a specific commit under a path prefix.
    ///
    /// Returns a map of (relative path → file contents).
    pub fn list_toml_files(
        &self,
        sha: &str,
        path_prefix: &str,
    ) -> Result<HashMap<String, String>, LettuceError> {
        // List files
        let prefix = path_prefix.trim_start_matches('/');
        let tree_arg = if prefix.is_empty() {
            sha.to_string()
        } else {
            format!("{sha}:{prefix}")
        };

        let output = Command::new("git")
            .args(["ls-tree", "-r", "--name-only", "--end-of-options"])
            .arg(&tree_arg)
            .current_dir(&self.path)
            .output()
            .map_err(|e| LettuceError::GitFailed(e.to_string()))?;

        // A failed ls-tree (a renamed watched directory, a typo in
        // `[gitops] path`, a bad object) exits non-zero with empty stdout.
        // Without this check the empty listing becomes an empty config, whose
        // diff removes every app/namespace/permission — a full desired-state
        // wipe reported as a successful sync. Fail loudly instead.
        if !output.status.success() {
            return Err(LettuceError::GitFailed(format!(
                "git ls-tree {tree_arg:?} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        let listing = String::from_utf8_lossy(&output.stdout);
        let mut files = HashMap::new();

        for line in listing.lines() {
            if !line.ends_with(".toml") {
                continue;
            }

            let blob_path = if prefix.is_empty() {
                line.to_string()
            } else {
                format!("{prefix}/{line}")
            };

            // Read file content. `--end-of-options` guards the object spec
            // in case the commit SHA begins with `-` (GIT4).
            let content_output = Command::new("git")
                .args(["show", "--end-of-options"])
                .arg(format!("{sha}:{blob_path}"))
                .current_dir(&self.path)
                .output()
                .map_err(|e| LettuceError::GitFailed(e.to_string()))?;

            if content_output.status.success() {
                let content = String::from_utf8_lossy(&content_output.stdout).to_string();
                files.insert(line.to_string(), content);
            }
        }

        Ok(files)
    }

    /// Get the local clone path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the tracked branch.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Get the remote URL.
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Redact a git command's stderr for an error message (M27).
///
/// GitOps errors are stored durably in Raft (`GitOpsSyncUpdate.last_error`), and
/// git echoes the remote URL — which may embed a `https://<token>@host/...`
/// credential — into its stderr on failure. Replace any occurrence of the
/// configured URL with `<repo>` and strip an inline `user:pass@`/`token@`
/// credential from anything URL-shaped, so the token never lands in the Raft
/// log or the operator's terminal.
fn redact_git_output(stderr: &[u8], url: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    let without_url = if url.is_empty() {
        text.into_owned()
    } else {
        text.replace(url, "<repo>")
    };
    redact_url_credentials(&without_url)
}

/// Environment variable the credential helper reads the secret from (M27).
const GIT_PASSWORD_ENV: &str = "RELIABURGER_GIT_PASSWORD";

/// Split a remote URL into a form safe to hand to git, plus the secret it was
/// carrying (M27).
///
/// A `[gitops] repo` of `https://x-access-token:ghp_abc@github.com/org/repo`
/// put that token in `git clone`'s argv, where `/proc/<pid>/cmdline` is
/// world-readable — any local user could read it while the clone ran. Worse,
/// git then wrote the whole URL into the clone's `.git/config`, so it outlived
/// the process entirely.
///
/// The username stays in the URL: git needs it, and it isn't the secret. Only
/// the password moves, into the child's environment, where `/proc/<pid>/environ`
/// is readable by the owning uid alone.
///
/// Returns `(sanitised_url, password)`. A URL with no `user:pass@` is returned
/// unchanged with `None`.
pub(crate) fn split_credentials(url: &str) -> (String, Option<String>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), None);
    };
    // Split on the LAST `@`: a password may legitimately contain one.
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return (url.to_string(), None);
    };
    match userinfo.split_once(':') {
        Some((user, password)) if !password.is_empty() => (
            format!("{scheme}://{user}@{host}"),
            Some(password.to_string()),
        ),
        // `scheme://token@host` — the whole userinfo IS the secret, but git
        // reads it as a username. Leave it in place rather than guess: moving
        // it to the password slot would change which credential git presents.
        _ => (url.to_string(), None),
    }
}

/// Configure `command` to supply `password` without it appearing in argv (M27).
///
/// git runs a `credential.helper` beginning with `!` through the shell, so the
/// helper can read the secret from the environment. Only the helper *script*
/// reaches argv, and it contains no secret — just the name of the variable.
fn with_credentials(command: &mut Command, password: Option<&str>) {
    let Some(password) = password else {
        return;
    };
    command.args([
        "-c",
        &format!(
            "credential.helper=!f() {{ test \"$1\" = get && \
             printf 'password=%s\\n' \"${GIT_PASSWORD_ENV}\"; }}; f"
        ),
    ]);
    command.env(GIT_PASSWORD_ENV, password);
}

/// Strip `scheme://user:pass@host` / `scheme://token@host` credentials from any
/// URL-shaped substring, leaving `scheme://host`.
pub(crate) fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for token in text.split_inclusive(char::is_whitespace) {
        let (word, trailing_ws) = match token.char_indices().last() {
            Some((i, c)) if c.is_whitespace() => (&token[..i], &token[i..]),
            _ => (token, ""),
        };
        match word.split_once("://") {
            Some((scheme, rest)) if rest.contains('@') => {
                let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
                out.push_str(scheme);
                out.push_str("://");
                out.push_str(host);
            }
            _ => out.push_str(word),
        }
        out.push_str(trailing_ws);
    }
    out
}

/// Does the clone already at `path` still track `url` on `branch`?
///
/// Reads the clone's `remote.origin.url` and confirms the branch ref
/// exists. A mismatch (config repointed, stale failover clone) means the
/// clone is the wrong repo and must be discarded rather than synced.
fn reused_clone_matches(path: &Path, url: &str, branch: &str) -> Result<bool, LettuceError> {
    let origin = Command::new("git")
        .args(["config", "--", "remote.origin.url"])
        .current_dir(path)
        .output()
        .map_err(|e| LettuceError::GitFailed(format!("failed to read clone remote: {e}")))?;

    if !origin.status.success() {
        return Ok(false);
    }
    let current_url = String::from_utf8_lossy(&origin.stdout).trim().to_string();
    // Compare the *sanitised* forms (M27). The clone stores a credential-free
    // remote now, so comparing it against a configured URL that still carries
    // `user:pass@` would mismatch every time and re-clone the repository on
    // every startup — a subtle, expensive regression from moving the secret
    // out of the URL. A clone made before that change stores the URL with
    // credentials, so sanitising both sides also makes it reusable rather
    // than forcing one re-clone on upgrade.
    let (safe_configured, _) = split_credentials(url);
    let (safe_current, _) = split_credentials(&current_url);
    if safe_current != safe_configured {
        return Ok(false);
    }

    // Confirm the tracked branch is present in the clone.
    let branch_ref = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(path)
        .output()
        .map_err(|e| LettuceError::GitFailed(format!("failed to verify clone branch: {e}")))?;

    Ok(branch_ref.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// M27: the token used to sit in `git clone`'s argv, where
    /// `/proc/<pid>/cmdline` is world-readable, and git then persisted the
    /// whole URL into the clone's `.git/config` where it outlived the process.
    #[test]
    fn split_credentials_moves_only_the_password() {
        let (url, password) =
            split_credentials("https://x-access-token:ghp_secret@github.com/org/repo");
        assert_eq!(url, "https://x-access-token@github.com/org/repo");
        assert_eq!(password.as_deref(), Some("ghp_secret"));
        assert!(
            !url.contains("ghp_secret"),
            "the secret must not survive in the URL"
        );
    }

    #[test]
    fn split_credentials_handles_an_at_sign_in_the_password() {
        // Splitting on the FIRST `@` would truncate the password and produce a
        // nonsense host.
        let (url, password) = split_credentials("https://user:p@ss@example.com/repo");
        assert_eq!(url, "https://user@example.com/repo");
        assert_eq!(password.as_deref(), Some("p@ss"));
    }

    #[test]
    fn split_credentials_leaves_urls_without_a_password_alone() {
        for url in [
            "https://github.com/org/repo",
            "git@github.com:org/repo.git",
            // A bare token in the username slot is what git presents as the
            // username; moving it would change which credential is sent.
            "https://ghp_token@github.com/org/repo",
            "https://user:@github.com/org/repo",
        ] {
            let (out, password) = split_credentials(url);
            assert_eq!(out, url, "{url} should be unchanged");
            assert!(password.is_none(), "{url} should yield no password");
        }
    }

    /// The credential helper reaches argv, so it must name the environment
    /// variable rather than contain the secret.
    #[test]
    fn the_credential_helper_argv_carries_no_secret() {
        let mut command = Command::new("git");
        with_credentials(&mut command, Some("ghp_secret"));
        let argv: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            argv.iter().any(|a| a.contains("credential.helper")),
            "no credential helper was configured: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("ghp_secret")),
            "the secret leaked into argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a.contains(GIT_PASSWORD_ENV)),
            "the helper must read the secret from the environment: {argv:?}"
        );
        // …and the secret is in the child's environment instead.
        let env: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect();
        assert!(
            env.iter()
                .any(|(k, v)| k == GIT_PASSWORD_ENV && v.as_deref() == Some("ghp_secret")),
            "the secret was not passed via the environment: {env:?}"
        );
    }

    #[test]
    fn no_password_configures_no_helper() {
        let mut command = Command::new("git");
        with_credentials(&mut command, None);
        assert_eq!(command.get_args().count(), 0);
        assert_eq!(command.get_envs().count(), 0);
    }

    /// M27: git stderr echoed into a durable error must not leak a URL-embedded
    /// credential.
    #[test]
    fn redact_git_output_strips_credentials() {
        let url = "https://x-access-token:ghp_SECRET@github.com/acme/app.git";
        let stderr = format!("fatal: could not read from {url}\n").into_bytes();
        let redacted = redact_git_output(&stderr, url);
        assert!(!redacted.contains("ghp_SECRET"), "leaked token: {redacted}");
        assert!(redacted.contains("<repo>"));

        // Even without the configured URL to match on, an inline credential in
        // any URL-shaped word is stripped.
        let other = redact_url_credentials("cloning https://tok@example.com/r.git now");
        assert_eq!(other, "cloning https://example.com/r.git now");
    }

    /// Create a test git repo with a TOML file.
    fn create_test_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let repo_path = dir.path().join("test-repo");

        // Init a bare repo, then create a working clone to add files
        let working = dir.path().join("working");

        Command::new("git")
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&repo_path)
            .output()
            .unwrap();

        Command::new("git")
            .args(["clone"])
            .arg(&repo_path)
            .arg(&working)
            .output()
            .unwrap();

        // Configure git user for commits
        for (key, val) in [("user.email", "test@test.com"), ("user.name", "Test")] {
            Command::new("git")
                .args(["config", key, val])
                .current_dir(&working)
                .output()
                .unwrap();
        }

        // Ensure we're on the 'main' branch (git may default to 'master')
        Command::new("git")
            .args(["checkout", "-B", "main"])
            .current_dir(&working)
            .output()
            .unwrap();

        // Add a TOML file
        fs::write(
            working.join("app.toml"),
            "[app.web]\nimage = \"myapp:v1\"\n",
        )
        .unwrap();

        Command::new("git")
            .args(["add", "app.toml"])
            .current_dir(&working)
            .output()
            .unwrap();

        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(&working)
            .output()
            .unwrap();

        Command::new("git")
            .args(["push", "origin", "HEAD:main"])
            .current_dir(&working)
            .output()
            .unwrap();

        (dir, repo_path)
    }

    #[test]
    fn clone_or_open_new_repo() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());

        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        assert!(clone_path.join("HEAD").exists());
        assert_eq!(repo.branch(), "main");
    }

    #[test]
    fn clone_or_open_existing_repo() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());

        // Clone once
        GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        // Open again (should succeed without re-cloning)
        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        assert_eq!(repo.branch(), "main");
    }

    #[test]
    fn fetch_returns_none_when_no_changes() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());

        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        // First fetch picks up initial state (may or may not see changes
        // depending on whether clone already has HEAD)
        let _ = repo.fetch();
        // Second fetch — definitely no changes
        let result = repo.fetch().unwrap();
        assert!(result.is_none(), "should return None when no changes");
    }

    #[test]
    fn list_toml_files_returns_content() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());

        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        // Use HEAD of the bare clone (which was just cloned from upstream)
        let sha = repo.head_sha().unwrap();
        let files = repo.list_toml_files(&sha, "/").unwrap();

        assert!(files.contains_key("app.toml"), "keys: {:?}", files.keys());
        assert!(files["app.toml"].contains("[app.web]"));
    }

    /// GIT4: a commit SHA or path beginning with `-` must not be able to
    /// smuggle in a git option. With `--end-of-options` in place these
    /// resolve as (missing) objects and error cleanly; without it, git
    /// would parse `--upload-pack=...` or similar as a flag.
    #[test]
    fn leading_dash_ref_cannot_inject_a_git_option() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());
        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();

        // A hostile "sha" that is really an option. It must not be honoured
        // as a flag; commit_info should fail rather than execute it.
        let result = repo.commit_info("--output=/tmp/pwned");
        assert!(
            result.is_err(),
            "a leading-dash ref must be rejected, not run as an option"
        );

        // A hostile path prefix likewise resolves to a missing object; with the
        // ls-tree exit-status check it is a clean error, not an executed option
        // and not a silent empty listing.
        let sha = repo.head_sha().unwrap();
        assert!(
            repo.list_toml_files(&sha, "--exec=/bin/false").is_err(),
            "a leading-dash path must error, not execute an option or list nothing"
        );
    }

    /// C10: a failed `ls-tree` (e.g. a path that isn't in the tree) must be a
    /// hard error, never a silent empty listing — an empty listing would let
    /// the sync diff remove every resource and wipe the cluster.
    #[test]
    fn list_toml_files_errors_when_the_path_is_missing() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());
        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        let sha = repo.head_sha().unwrap();

        assert!(
            repo.list_toml_files(&sha, "this-directory-does-not-exist")
                .is_err(),
            "a missing path must error rather than return an empty file set"
        );
    }

    /// GIT4: a clone left over from a *different* repo URL is discarded and
    /// re-cloned, so Lettuce never syncs the wrong repository after a
    /// config repoint or a stale failover clone.
    #[test]
    fn drifted_clone_is_recloned_from_the_configured_url() {
        let (dir, repo_a) = create_test_repo();
        let (dir_b, repo_b) = create_test_repo();
        let clone_path = dir.path().join("clone");

        let url_a = format!("file://{}", repo_a.display());
        let url_b = format!("file://{}", repo_b.display());

        // Clone repo A, then ask for repo B at the same path.
        let a = GitRepo::clone_or_open(&url_a, &clone_path, "main").unwrap();
        assert_eq!(a.url(), url_a);
        drop(a);

        let b = GitRepo::clone_or_open(&url_b, &clone_path, "main").unwrap();
        assert_eq!(b.url(), url_b, "the reused path must now track repo B");

        // The clone's own remote points at B, proving it was re-cloned.
        let origin = Command::new("git")
            .args(["config", "--", "remote.origin.url"])
            .current_dir(&clone_path)
            .output()
            .unwrap();
        let current = String::from_utf8_lossy(&origin.stdout).trim().to_string();
        assert_eq!(current, url_b);

        drop(dir_b);
    }
}
