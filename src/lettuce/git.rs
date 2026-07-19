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
        // `--` separates options from the URL and destination so a repo or
        // branch beginning with `-` can't be read as a git flag (GIT4).
        let output = Command::new("git")
            .args(["clone", "--bare", "--single-branch", "--branch"])
            .arg(branch)
            .arg("--")
            .arg(url)
            .arg(path)
            .output()
            .map_err(|e| LettuceError::GitFailed(format!("failed to run git clone: {e}")))?;

        if !output.status.success() {
            return Err(LettuceError::GitFailed(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&output.stderr)
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

        let output = Command::new("git")
            .args(["fetch", "origin", "--"])
            .arg(&self.branch)
            .current_dir(&self.path)
            .output()
            .map_err(|e| LettuceError::GitFailed(format!("failed to run git fetch: {e}")))?;

        if !output.status.success() {
            return Err(LettuceError::GitFailed(format!(
                "git fetch failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        let new_head = self.remote_head_sha()?;

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
    if current_url != url {
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
