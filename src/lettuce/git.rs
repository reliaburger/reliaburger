//! Git operations for Lettuce.
//!
//! Wraps command-line git for clone, fetch, and file listing.
//! Uses the `git` CLI rather than libgit2 to avoid pulling in a
//! large C dependency — git is always available on the nodes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use super::subprocess::{CommandBudget, run_bounded};
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
    /// Deadline and cancellation every `git` call runs under (B19).
    budget: CommandBudget,
}

impl GitRepo {
    /// Clone a repository into a bare local directory.
    ///
    /// If the directory already holds a clone, it's reused only when its
    /// remote URL and tracked branch still match the config. A drift
    /// (someone repointed `[gitops] repo` or `branch`, or a stale clone
    /// survived a failover) triggers a fresh clone rather than silently
    /// syncing the wrong repo (GIT4). A fresh clone is fetched into a
    /// temporary sibling directory and renamed into place, so a mid-clone
    /// crash never leaves a half-populated repo at `path`. Whatever was at
    /// `path` (a drifted clone, or a directory without HEAD) is replaced
    /// only once the new clone is complete (#2061).
    pub fn clone_or_open(url: &str, path: &Path, branch: &str) -> Result<Self, LettuceError> {
        Self::clone_or_open_with_budget(url, path, branch, CommandBudget::default())
    }

    /// [`GitRepo::clone_or_open`], with every `git` call (the clone
    /// included) bounded by `budget`.
    pub fn clone_or_open_with_budget(
        url: &str,
        path: &Path,
        branch: &str,
        budget: CommandBudget,
    ) -> Result<Self, LettuceError> {
        // A drifted or unreadable clone falls through to a fresh clone; the
        // old one stays until the new one is complete.
        if path.join("HEAD").exists()
            && matches!(reused_clone_matches(path, url, branch, &budget), Ok(true))
            && scrub_stored_remote(path, url, &budget).is_ok()
        {
            return Ok(Self {
                path: path.to_path_buf(),
                url: url.to_string(),
                branch: branch.to_string(),
                budget,
            });
        }
        // Whatever is at `path` now (a drifted clone, or a HEAD-less
        // directory an interrupted clone left) is replaced once the new
        // clone succeeds.
        Self::fresh_clone(url, path, branch, budget)
    }

    /// Perform a fresh bare clone, published at `path` only once it is
    /// complete.
    ///
    /// The clone goes into a `.<name>.cloning` sibling and is renamed into
    /// place, so a clone killed by its deadline leaves nothing at `path`.
    /// The staging name is fixed rather than random, so the next attempt
    /// removes what a killed one left.
    fn fresh_clone(
        url: &str,
        path: &Path,
        branch: &str,
        budget: CommandBudget,
    ) -> Result<Self, LettuceError> {
        let staging = staging_path(path)?;
        if staging.exists() {
            remove_clone(&staging, "interrupted clone")?;
        }
        let cloned = Self::clone_into(url, &staging, branch, &budget);
        if let Err(error) = cloned {
            // A failed clone may have written part of the staging directory.
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        if path.exists() {
            remove_clone(path, "replaced clone")?;
        }
        std::fs::rename(&staging, path).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging);
            LettuceError::GitFailed(format!(
                "failed to move the new clone into {}: {e}",
                path.display()
            ))
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            url: url.to_string(),
            branch: branch.to_string(),
            budget,
        })
    }

    /// Run `git clone --bare` into `path`.
    fn clone_into(
        url: &str,
        path: &Path,
        branch: &str,
        budget: &CommandBudget,
    ) -> Result<(), LettuceError> {
        // M27: keep the secret out of argv (world-readable via /proc) and out
        // of the clone's `.git/config` (where it would outlive the process).
        // The sanitised URL is what git stores as the remote; `fetch` supplies
        // the credential the same way on every later call.
        let (safe_url, credentials) = split_credentials(url);
        // `--` separates options from the URL and destination so a repo or
        // branch beginning with `-` can't be read as a git flag (GIT4).
        let mut command = Command::new("git");
        with_credentials(&mut command, &credentials);
        command
            .args(["clone", "--bare", "--single-branch", "--branch"])
            .arg(branch)
            .arg("--")
            .arg(&safe_url)
            .arg(path);
        let output = run_bounded(command, budget, "git clone")?;

        if !output.status.success() {
            return Err(LettuceError::GitFailed(format!(
                "git clone failed: {}",
                redact_git_output(&output.stderr, url)
            )));
        }
        Ok(())
    }

    /// A `git` command that runs in the clone.
    fn git(&self) -> Command {
        let mut command = Command::new("git");
        command.current_dir(&self.path);
        command
    }

    /// Run a `git` command within this repository's budget.
    pub(crate) fn run(&self, command: Command, what: &str) -> Result<Output, LettuceError> {
        run_bounded(command, &self.budget, what)
    }

    /// Fetch the latest from the remote.
    ///
    /// Returns `Some(commit)` if HEAD changed, `None` if unchanged.
    pub fn fetch(&self) -> Result<Option<CommitInfo>, LettuceError> {
        let old_head = self.head_sha().ok();

        // The stored remote is credential-free (M27), so the secret has to be
        // supplied on every fetch too.
        let (_, credentials) = split_credentials(&self.url);
        let mut command = self.git();
        with_credentials(&mut command, &credentials);
        command.args(["fetch", "origin", "--"]).arg(&self.branch);
        let output = self.run(command, "git fetch")?;

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
            let mut command = self.git();
            command
                .args([
                    "update-ref",
                    &format!("refs/heads/{}", self.branch),
                    "--end-of-options",
                ])
                .arg(&new_head);
            let update = self.run(command, "git update-ref")?;
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
        let mut command = self.git();
        command.args(["rev-parse", "--verify", "--quiet", "HEAD"]);
        let output = self.run(command, "git rev-parse")?;
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() || sha.is_empty() {
            return Err(LettuceError::GitFailed(
                "failed to resolve local HEAD".to_string(),
            ));
        }
        Ok(sha)
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
            let mut command = self.git();
            command
                .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
                .arg(&refname);
            let output = self.run(command, "git rev-parse")?;

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
        let mut command = self.git();
        command
            .args(["log", "-1", "--format=%H%n%s%n%an%n%ct", "--end-of-options"])
            .arg(sha);
        let output = self.run(command, "git log")?;

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

        // `-z` prints each path raw and NUL-terminated. Without it git
        // C-quotes any path holding a tab or a non-ASCII byte, and the quoted
        // name neither ends in `.toml` nor names a blob, so the file silently
        // dropped out of desired state and its apps were deleted (B17).
        let mut command = self.git();
        command
            .args(["ls-tree", "-r", "-z", "--name-only", "--end-of-options"])
            .arg(&tree_arg);
        let output = self.run(command, "git ls-tree")?;

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

        let mut files = HashMap::new();

        for entry in output.stdout.split(|byte| *byte == 0) {
            if !entry.ends_with(b".toml") {
                continue;
            }
            // A config path must be text to be a map key; refuse rather
            // than skip, since skipping would delete what it declares.
            let name = std::str::from_utf8(entry).map_err(|_| {
                LettuceError::GitFailed(format!(
                    "{sha}: path {:?} is not valid UTF-8",
                    String::from_utf8_lossy(entry)
                ))
            })?;

            let blob_path = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };

            // Read file content. `--end-of-options` guards the object spec
            // in case the commit SHA begins with `-` (GIT4).
            let mut command = self.git();
            command
                .args(["show", "--end-of-options"])
                .arg(format!("{sha}:{blob_path}"));
            let content_output = self.run(command, "git show")?;

            // A file that's listed but unreadable fails the whole sync (B17).
            // Dropping it would leave a partial desired state, and the diff
            // would delete everything that file declares.
            if !content_output.status.success() {
                return Err(LettuceError::GitFailed(format!(
                    "git show {sha}:{blob_path:?} failed: {}",
                    String::from_utf8_lossy(&content_output.stderr).trim()
                )));
            }
            let content = String::from_utf8_lossy(&content_output.stdout).to_string();
            files.insert(name.to_string(), content);
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

/// Environment variable the credential helper reads the password from (M27).
const GIT_PASSWORD_ENV: &str = "RELIABURGER_GIT_PASSWORD";

/// Environment variable the credential helper reads the username from, for
/// a token-only URL (#2414).
const GIT_USERNAME_ENV: &str = "RELIABURGER_GIT_USERNAME";

/// The credential a remote URL carried, which git is given through a
/// credential helper rather than in the URL (M27, #2414).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct GitCredentials {
    /// A username that is itself the secret: the token in `scheme://TOKEN@host`.
    username: Option<String>,
    /// The password in `scheme://user:password@host`.
    password: Option<String>,
}

impl GitCredentials {
    fn is_empty(&self) -> bool {
        self.username.is_none() && self.password.is_none()
    }
}

/// Split a remote URL into a form safe to hand to git, plus the secret it was
/// carrying (M27).
///
/// A `[gitops] repo` of `https://x-access-token:ghp_abc@github.com/org/repo`
/// put that token in `git clone`'s argv, where `/proc/<pid>/cmdline` is
/// world-readable — any local user could read it while the clone ran. Worse,
/// git then wrote the whole URL into the clone's `.git/config`, so it outlived
/// the process entirely.
///
/// For `user:password@`, the username stays in the URL (it isn't the secret)
/// and only the password moves. For `TOKEN@` (#2414) the whole userinfo is
/// the secret, so it all moves, and the helper hands it back to git as the
/// username with an empty password: the same credential git would have
/// presented from the URL. Either way the secret reaches the child only
/// through its environment, where `/proc/<pid>/environ` is readable by the
/// owning uid alone.
///
/// A URL with no userinfo (or an scp-style `git@host:path`) is returned
/// unchanged with no credentials.
pub(crate) fn split_credentials(url: &str) -> (String, GitCredentials) {
    let unchanged = || (url.to_string(), GitCredentials::default());
    let Some((scheme, rest)) = url.split_once("://") else {
        return unchanged();
    };
    // Split on the LAST `@`: a password may legitimately contain one.
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return unchanged();
    };
    match userinfo.split_once(':') {
        Some((user, password)) if !password.is_empty() => (
            format!("{scheme}://{user}@{host}"),
            GitCredentials {
                username: None,
                password: Some(password.to_string()),
            },
        ),
        // `user:@host` names a user with an empty password: nothing secret.
        Some(_) => unchanged(),
        None if userinfo.is_empty() => unchanged(),
        None => (
            format!("{scheme}://{host}"),
            GitCredentials {
                username: Some(userinfo.to_string()),
                password: Some(String::new()),
            },
        ),
    }
}

/// Configure `command` to supply `credentials` without them appearing in
/// argv (M27, #2414).
///
/// git runs a `credential.helper` beginning with `!` through the shell, so the
/// helper can read the secret from the environment. Only the helper *script*
/// reaches argv, and it contains no secret — just the names of the variables.
/// `${VAR+x}` is set only when the variable is, so an empty password is
/// still printed: git then sends it rather than asking for one.
fn with_credentials(command: &mut Command, credentials: &GitCredentials) {
    if credentials.is_empty() {
        return;
    }
    command.args([
        "-c",
        &format!(
            "credential.helper=!f() {{ test \"$1\" = get || return 0; \
             if [ -n \"${{{GIT_USERNAME_ENV}+x}}\" ]; then \
             printf 'username=%s\\n' \"${GIT_USERNAME_ENV}\"; fi; \
             if [ -n \"${{{GIT_PASSWORD_ENV}+x}}\" ]; then \
             printf 'password=%s\\n' \"${GIT_PASSWORD_ENV}\"; fi; }}; f"
        ),
    ]);
    // Set what this URL carries, and clear the other, so a variable Bun
    // happened to inherit can't stand in for it.
    for (name, value) in [
        (GIT_USERNAME_ENV, &credentials.username),
        (GIT_PASSWORD_ENV, &credentials.password),
    ] {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
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

/// The sibling a fresh clone of `path` is staged in.
fn staging_path(path: &Path) -> Result<std::path::PathBuf, LettuceError> {
    let name = path.file_name().ok_or_else(|| {
        LettuceError::GitFailed(format!("clone path {} has no name", path.display()))
    })?;
    let mut staging = std::ffi::OsString::from(".");
    staging.push(name);
    staging.push(".cloning");
    Ok(path.with_file_name(staging))
}

/// Remove a clone directory that can't be used, saying which kind.
fn remove_clone(path: &Path, what: &str) -> Result<(), LettuceError> {
    std::fs::remove_dir_all(path).map_err(|e| {
        LettuceError::GitFailed(format!(
            "failed to remove {what} at {}: {e}",
            path.display()
        ))
    })
}

/// Make the clone's stored remote the credential-free form of `url`.
///
/// A clone made by an older Bun, or from a token-only URL before #2414,
/// holds the credential in its `config`, where it outlives every process.
/// Only a stored URL that differs is rewritten.
fn scrub_stored_remote(path: &Path, url: &str, budget: &CommandBudget) -> Result<(), LettuceError> {
    let (safe_url, _) = split_credentials(url);
    let mut command = Command::new("git");
    command
        .args(["config", "--", "remote.origin.url"])
        .current_dir(path);
    let stored = run_bounded(command, budget, "git config")?;
    if String::from_utf8_lossy(&stored.stdout).trim() == safe_url {
        return Ok(());
    }
    let mut command = Command::new("git");
    command
        .args(["config", "--replace-all", "--", "remote.origin.url"])
        .arg(&safe_url)
        .current_dir(path);
    let rewrite = run_bounded(command, budget, "git config")?;
    if !rewrite.status.success() {
        return Err(LettuceError::GitFailed(
            "failed to remove the credential from the clone's remote URL".into(),
        ));
    }
    Ok(())
}

/// Does the clone already at `path` still track `url` on `branch`?
///
/// Reads the clone's `remote.origin.url` and confirms the branch ref
/// exists. A mismatch (config repointed, stale failover clone) means the
/// clone is the wrong repo and must be discarded rather than synced.
fn reused_clone_matches(
    path: &Path,
    url: &str,
    branch: &str,
    budget: &CommandBudget,
) -> Result<bool, LettuceError> {
    let mut command = Command::new("git");
    command
        .args(["config", "--", "remote.origin.url"])
        .current_dir(path);
    let origin = run_bounded(command, budget, "git config")?;

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
    let mut command = Command::new("git");
    command
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("refs/heads/{branch}"))
        .current_dir(path);
    let branch_ref = run_bounded(command, budget, "git rev-parse")?;

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
        let (url, credentials) =
            split_credentials("https://x-access-token:ghp_secret@github.com/org/repo");
        assert_eq!(url, "https://x-access-token@github.com/org/repo");
        assert_eq!(credentials.password.as_deref(), Some("ghp_secret"));
        assert_eq!(credentials.username, None);
        assert!(
            !url.contains("ghp_secret"),
            "the secret must not survive in the URL"
        );
    }

    #[test]
    fn split_credentials_handles_an_at_sign_in_the_password() {
        // Splitting on the FIRST `@` would truncate the password and produce a
        // nonsense host.
        let (url, credentials) = split_credentials("https://user:p@ss@example.com/repo");
        assert_eq!(url, "https://user@example.com/repo");
        assert_eq!(credentials.password.as_deref(), Some("p@ss"));
    }

    #[test]
    fn split_credentials_leaves_urls_without_a_secret_alone() {
        for url in [
            "https://github.com/org/repo",
            "git@github.com:org/repo.git",
            "https://user:@github.com/org/repo",
        ] {
            let (out, credentials) = split_credentials(url);
            assert_eq!(out, url, "{url} should be unchanged");
            assert_eq!(credentials, GitCredentials::default(), "{url}");
        }
    }

    /// #2414: in `https://TOKEN@host` the whole userinfo is the secret, so
    /// it all leaves the URL, and comes back as the username.
    #[test]
    fn a_token_only_url_keeps_the_token_out_of_the_url_git_is_given() {
        let (url, credentials) = split_credentials("https://ghp_abc123@github.com/org/config");
        assert_eq!(url, "https://github.com/org/config");
        assert!(!url.contains("ghp_abc123"));
        assert_eq!(credentials.username.as_deref(), Some("ghp_abc123"));
        assert_eq!(credentials.password.as_deref(), Some(""));
    }

    /// The environment variables `command` sets, by name.
    fn child_env(command: &Command) -> Vec<(String, Option<String>)> {
        command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect()
    }

    /// #2414: the token-only username reaches git through the helper's
    /// environment, never argv.
    #[test]
    fn the_credential_helper_supplies_a_token_only_username_from_the_environment() {
        let (_, credentials) = split_credentials("https://ghp_abc123@github.com/org/config");
        let mut command = Command::new("git");
        with_credentials(&mut command, &credentials);
        let argv: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            !argv.iter().any(|a| a.contains("ghp_abc123")),
            "the token leaked into argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a.contains(GIT_USERNAME_ENV)),
            "{argv:?}"
        );
        let env = child_env(&command);
        assert!(
            env.iter()
                .any(|(k, v)| k == GIT_USERNAME_ENV && v.as_deref() == Some("ghp_abc123")),
            "{env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == GIT_PASSWORD_ENV && v.as_deref() == Some("")),
            "{env:?}"
        );
    }

    /// The credential helper reaches argv, so it must name the environment
    /// variable rather than contain the secret.
    #[test]
    fn the_credential_helper_argv_carries_no_secret() {
        let mut command = Command::new("git");
        let (_, credentials) = split_credentials("https://x-access-token:ghp_secret@example.com/r");
        with_credentials(&mut command, &credentials);
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
        let env = child_env(&command);
        assert!(
            env.iter()
                .any(|(k, v)| k == GIT_PASSWORD_ENV && v.as_deref() == Some("ghp_secret")),
            "the secret was not passed via the environment: {env:?}"
        );
    }

    #[test]
    fn no_password_configures_no_helper() {
        let mut command = Command::new("git");
        with_credentials(&mut command, &GitCredentials::default());
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
        create_test_repo_with_files(&[("app.toml", "[app.web]\nimage = \"myapp:v1\"\n")])
    }

    /// Create a test git repo whose one commit holds `files` (path, content).
    fn create_test_repo_with_files(files: &[(&str, &str)]) -> (TempDir, PathBuf) {
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

        for (path, content) in files {
            let file = working.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, content).unwrap();
        }

        Command::new("git")
            .args(["add", "--all"])
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

    /// B17: git quotes a path with non-ASCII bytes or a tab in plain
    /// `ls-tree` output (`"caf\303\251.toml"`), and the quoted name neither
    /// ends in `.toml` nor names a blob. Those files were silently left
    /// out of desired state, so the sync deleted whatever they declared.
    #[test]
    fn list_toml_files_reads_non_ascii_and_tab_filenames() {
        let (dir, repo_path) = create_test_repo_with_files(&[
            ("café.toml", "[app.cafe]\nimage = \"cafe:v1\"\n"),
            ("tab\tname.toml", "[app.tab]\nimage = \"tab:v1\"\n"),
            (
                "über dir/zürich.toml",
                "[app.zurich]\nimage = \"zurich:v1\"\n",
            ),
            ("plain.toml", "[app.plain]\nimage = \"plain:v1\"\n"),
        ]);
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());
        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        let sha = repo.head_sha().unwrap();

        let files = repo.list_toml_files(&sha, "/").unwrap();

        let mut names: Vec<&str> = files.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "café.toml",
                "plain.toml",
                "tab\tname.toml",
                "über dir/zürich.toml"
            ]
        );
        assert!(files["café.toml"].contains("[app.cafe]"));
        assert!(files["tab\tname.toml"].contains("[app.tab]"));
        assert!(files["über dir/zürich.toml"].contains("[app.zurich]"));

        // Under a path prefix the listing is relative to it.
        let files = repo.list_toml_files(&sha, "über dir").unwrap();
        assert_eq!(files.keys().collect::<Vec<_>>(), ["zürich.toml"]);
        assert!(files["zürich.toml"].contains("[app.zurich]"));
    }

    /// B17: a listed `.toml` whose blob can't be read used to be dropped,
    /// leaving a partial desired state that deletes what the file
    /// declared. The whole listing must fail instead.
    #[test]
    fn list_toml_files_fails_when_a_blob_cannot_be_read() {
        let (dir, repo_path) = create_test_repo_with_files(&[
            ("app.toml", "[app.web]\nimage = \"myapp:v1\"\n"),
            ("other.toml", "[app.other]\nimage = \"other:v1\"\n"),
        ]);
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());
        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        let sha = repo.head_sha().unwrap();

        let blob = git_stdout(&clone_path, &["rev-parse", &format!("{sha}:other.toml")]);
        remove_object(&clone_path, &blob);

        let result = repo.list_toml_files(&sha, "/");
        assert!(
            matches!(result, Err(LettuceError::GitFailed(ref message)) if message.contains("other.toml")),
            "an unreadable blob must fail the listing, not vanish from it: {result:?}"
        );
    }

    /// Make every fetch from `clone` hang: the upload-pack it runs records
    /// a descendant's pid in `pid_file`, leaves that descendant sleeping,
    /// and never exits itself. The trailing `#` comments out the repository
    /// path git appends.
    fn make_fetch_hang(clone: &Path, pid_file: &Path) {
        let hang = format!(
            "sh -c 'echo $$ > {}; exec sleep 300' & exec sleep 300 #",
            pid_file.display()
        );
        git_stdout(clone, &["config", "remote.origin.uploadpack", &hang]);
    }

    fn is_running(pid: i32) -> bool {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&output.stdout);
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    fn descendant_pid(pid_file: &Path) -> i32 {
        // Generous: git spawning the hook's shell can take seconds on a
        // loaded host, and the wait ends as soon as the file appears.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Ok(text) = fs::read_to_string(pid_file)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the hanging upload-pack never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn wait_until_gone(pid: i32) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !is_running(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// B19: a fetch against a remote that never answers used to block the
    /// only sync loop forever. It must fail within its deadline, leave no
    /// process behind, and the next fetch must work.
    #[test]
    fn a_hung_fetch_fails_within_its_deadline_and_leaves_nothing_running() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());
        GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        let pid_file = dir.path().join("upload-pack.pid");
        make_fetch_hang(&clone_path, &pid_file);

        let budget = CommandBudget {
            timeout: std::time::Duration::from_millis(1500),
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let repo = GitRepo::clone_or_open_with_budget(&url, &clone_path, "main", budget).unwrap();

        let started = std::time::Instant::now();
        let result = repo.fetch();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the fetch outlived its deadline: {:?}",
            started.elapsed()
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("git fetch timed out"), "got: {error}");
        let pid = descendant_pid(&pid_file);
        assert!(wait_until_gone(pid), "upload-pack's descendant survived");

        git_stdout(
            &clone_path,
            &["config", "--unset", "remote.origin.uploadpack"],
        );
        assert!(repo.fetch().is_ok(), "the next fetch must succeed");
    }

    /// B19: shutdown during a stuck fetch stops it at once rather than
    /// waiting for the deadline.
    #[test]
    fn cancelling_a_hung_fetch_returns_promptly() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());
        GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        let pid_file = dir.path().join("upload-pack.pid");
        make_fetch_hang(&clone_path, &pid_file);

        let cancel = tokio_util::sync::CancellationToken::new();
        let budget = CommandBudget {
            timeout: std::time::Duration::from_secs(300),
            cancel: cancel.clone(),
        };
        let repo = GitRepo::clone_or_open_with_budget(&url, &clone_path, "main", budget).unwrap();
        // Cancel once the hang has started, not after a fixed delay: on a
        // loaded host git may not have spawned upload-pack within 500 ms,
        // and a fetch cancelled before that leaves no descendant to check.
        let watched = pid_file.clone();
        let canceller = std::thread::spawn(move || {
            let pid = descendant_pid(&watched);
            let cancelled_at = std::time::Instant::now();
            cancel.cancel();
            (pid, cancelled_at)
        });

        let error = repo.fetch().unwrap_err().to_string();
        let (pid, cancelled_at) = canceller.join().unwrap();
        assert!(cancelled_at.elapsed() < std::time::Duration::from_secs(10));
        assert!(error.contains("cancelled"), "got: {error}");
        assert!(wait_until_gone(pid));
    }

    fn git_stdout(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// The clone's stored `remote.origin.url`.
    fn stored_remote(clone: &Path) -> String {
        git_stdout(clone, &["config", "--", "remote.origin.url"])
    }

    /// #2414: a token-only URL (`scheme://TOKEN@host`) used to be handed to
    /// git whole, so the token sat in argv and in the clone's config.
    #[test]
    fn a_clone_from_a_token_only_url_keeps_the_token_out_of_its_config() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://ghp_abc123@localhost{}", repo_path.display());

        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        repo.fetch().unwrap();
        let config = fs::read_to_string(clone_path.join("config")).unwrap();
        assert!(
            !config.contains("ghp_abc123"),
            "the token reached the clone's config: {config}"
        );
    }

    /// #2414: a clone made before the fix holds the token in its config;
    /// opening it rewrites the remote to the credential-free URL.
    #[test]
    fn an_existing_clone_whose_config_holds_a_token_is_rewritten_on_open() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let safe = format!("file://localhost{}", repo_path.display());
        let url = format!("file://ghp_abc123@localhost{}", repo_path.display());
        GitRepo::clone_or_open(&safe, &clone_path, "main").unwrap();
        // What an older Bun left behind.
        git_stdout(&clone_path, &["config", "--", "remote.origin.url", &url]);
        let head = git_stdout(&clone_path, &["rev-parse", "HEAD"]);

        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        assert_eq!(stored_remote(&clone_path), safe);
        assert_eq!(
            git_stdout(&clone_path, &["rev-parse", "HEAD"]),
            head,
            "the clone is reused, not replaced"
        );
        repo.fetch().unwrap();
    }

    /// An HTTP server that answers every request with 401, as a remote
    /// that rejects the credential would, and records each `Authorization`
    /// header it was sent. Returns its port and a receiver for the headers.
    fn credential_capturing_server() -> (u16, std::sync::mpsc::Receiver<String>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut authorization = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("authorization")
                    {
                        authorization = Some(value.trim().to_string());
                    }
                }
                if let Some(header) = authorization {
                    let _ = tx.send(header);
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        (port, rx)
    }

    /// #2414: whichever URL form holds the credential, git presents the same
    /// HTTP Basic credential it would from the URL itself: a token-only URL
    /// sends the token as the username with an empty password.
    #[test]
    fn git_presents_the_url_credential_through_the_helper() {
        use base64::Engine;
        for (userinfo, expected) in [
            ("ghp_abc123", "ghp_abc123:"),
            ("x-access-token:ghp_abc123", "x-access-token:ghp_abc123"),
        ] {
            let (port, header) = credential_capturing_server();
            let dir = TempDir::new().unwrap();
            let url = format!("http://{userinfo}@127.0.0.1:{port}/org/config.git");
            let error = GitRepo::clone_or_open_with_budget(
                &url,
                &dir.path().join("clone"),
                "main",
                CommandBudget {
                    timeout: std::time::Duration::from_secs(20),
                    cancel: tokio_util::sync::CancellationToken::new(),
                },
            )
            .unwrap_err();
            // git waits for each answer, so every header has been sent by
            // the time the clone has failed.
            let sent: Vec<String> = header
                .try_iter()
                .map(|header| {
                    let encoded = header.strip_prefix("Basic ").unwrap().to_string();
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .unwrap();
                    String::from_utf8(decoded).unwrap()
                })
                .collect();
            assert!(
                sent.iter().any(|credential| credential == expected),
                "{userinfo}: git sent {sent:?}, not {expected:?} ({error})"
            );
            assert!(!error.to_string().contains("ghp_abc123"), "{error}");
        }
    }

    /// Delete one object from a bare repo: unpack every pack into loose
    /// objects, then remove that object's file.
    fn remove_object(repo: &Path, object: &str) {
        let pack_dir = repo.join("objects/pack");
        for entry in fs::read_dir(&pack_dir).unwrap() {
            let path = entry.unwrap().path();
            if path
                .extension()
                .is_some_and(|extension| extension == "pack")
            {
                let pack = fs::read(&path).unwrap();
                for extension in ["pack", "idx", "rev"] {
                    let _ = fs::remove_file(path.with_extension(extension));
                }
                let mut child = Command::new("git")
                    .arg("unpack-objects")
                    .current_dir(repo)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                std::io::Write::write_all(child.stdin.as_mut().unwrap(), &pack).unwrap();
                assert!(child.wait().unwrap().success());
            }
        }
        let loose = repo.join("objects").join(&object[..2]).join(&object[2..]);
        fs::remove_file(&loose).unwrap();
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

    /// A clone killed before it wrote HEAD left a non-empty directory with no
    /// HEAD at the path, and every later `git clone` failed with "destination
    /// path already exists" until someone removed it by hand.
    #[test]
    fn a_half_written_clone_without_head_is_replaced() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        std::fs::create_dir_all(clone_path.join("objects/pack")).unwrap();
        std::fs::write(clone_path.join("objects/pack/partial.pack"), b"x").unwrap();
        let url = format!("file://{}", repo_path.display());

        let repo = GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();

        assert!(clone_path.join("HEAD").exists());
        assert!(repo.head_sha().is_ok());
    }

    /// A failed clone leaves nothing at the path or beside it.
    #[test]
    fn a_failed_clone_leaves_nothing_behind() {
        let dir = TempDir::new().unwrap();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", dir.path().join("missing").display());

        assert!(GitRepo::clone_or_open(&url, &clone_path, "main").is_err());

        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "a failed clone left {left:?}");
    }

    /// A drifted clone was deleted before the new one was fetched, so a
    /// fetch that failed left no clone at all. It is now replaced only once
    /// the new clone is complete.
    #[test]
    fn a_failed_reclone_keeps_the_drifted_clone() {
        let (dir, repo_path) = create_test_repo();
        let clone_path = dir.path().join("clone");
        let url = format!("file://{}", repo_path.display());
        GitRepo::clone_or_open(&url, &clone_path, "main").unwrap();
        let missing = format!("file://{}", dir.path().join("missing").display());

        assert!(GitRepo::clone_or_open(&missing, &clone_path, "main").is_err());

        assert!(
            clone_path.join("HEAD").exists(),
            "the old clone was deleted"
        );
        let origin = Command::new("git")
            .args(["config", "--", "remote.origin.url"])
            .current_dir(&clone_path)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&origin.stdout).trim(), url);
    }
}
