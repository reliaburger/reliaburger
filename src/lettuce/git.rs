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
    /// Remote URL (stored for future re-clone on failover).
    _url: String,
    /// Branch to track.
    branch: String,
}

impl GitRepo {
    /// Clone a repository into a bare local directory.
    ///
    /// If the directory already exists, validates it's a git repo.
    pub fn clone_or_open(url: &str, path: &Path, branch: &str) -> Result<Self, LettuceError> {
        if path.join("HEAD").exists() {
            // Already cloned
            return Ok(Self {
                path: path.to_path_buf(),
                _url: url.to_string(),
                branch: branch.to_string(),
            });
        }

        let output = Command::new("git")
            .args([
                "clone",
                "--bare",
                "--single-branch",
                "--branch",
                branch,
                url,
            ])
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
            _url: url.to_string(),
            branch: branch.to_string(),
        })
    }

    /// Fetch the latest from the remote.
    ///
    /// Returns `Some(commit)` if HEAD changed, `None` if unchanged.
    pub fn fetch(&self) -> Result<Option<CommitInfo>, LettuceError> {
        let old_head = self.head_sha().ok();

        let output = Command::new("git")
            .args(["fetch", "origin", &self.branch])
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
            let output = Command::new("git")
                .args(["rev-parse", &refname])
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
    fn commit_info(&self, sha: &str) -> Result<CommitInfo, LettuceError> {
        let output = Command::new("git")
            .args(["log", "-1", "--format=%H%n%s%n%an%n%ct", sha])
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
            .args(["ls-tree", "-r", "--name-only", &tree_arg])
            .current_dir(&self.path)
            .output()
            .map_err(|e| LettuceError::GitFailed(e.to_string()))?;

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

            // Read file content
            let content_output = Command::new("git")
                .args(["show", &format!("{sha}:{blob_path}")])
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
}
