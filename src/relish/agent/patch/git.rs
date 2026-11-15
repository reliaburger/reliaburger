//! The git a workspace needs: a branch, an ignore list, and a snapshot of
//! the working tree that never touches the agent's own index.

use std::path::Path;
use std::process::Command;

use crate::relish::agent::envelope::{AgentError, ErrorCode};

/// Run `git -C dir <args>`, returning its stdout. `index` points git at
/// another index file than the repository's own.
pub(super) fn git(dir: &Path, args: &[&str], index: Option<&Path>) -> Result<String, AgentError> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=false"]);
    // A caller's GIT_DIR (say, relish run from a git hook) would point every
    // command at another repository.
    for variable in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        command.env_remove(variable);
    }
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.args(args).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AgentError::new(ErrorCode::UnsupportedPlatform, "git isn't on PATH")
                .with_hint("relish agent patch needs git; install it and run init again")
        } else {
            AgentError::new(ErrorCode::Internal, format!("couldn't run git: {error}"))
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError::new(
            ErrorCode::Internal,
            format!("git {} failed: {}", args[0], stderr.trim()),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Write the working tree, as it is now and with git's ignore rules, as a
/// tree object, and return its id.
///
/// It goes through a scratch copy of the index, so the agent's own staging
/// is left exactly as it was, and a file the agent added but never `git
/// add`ed still counts. Two snapshots of the same files have the same id,
/// which is how `try` and `diff` tell whether a build or a test run is still
/// current.
pub(super) fn snapshot(dir: &Path) -> Result<String, AgentError> {
    let git_dir = git(dir, &["rev-parse", "--absolute-git-dir"], None)?;
    let git_dir = Path::new(git_dir.trim());
    let scratch = git_dir.join("relish-patch-index");
    let real = git_dir.join("index");
    // Starting from the real index keeps git's file-stat cache, so it only
    // rehashes what changed.
    if real.exists() {
        std::fs::copy(&real, &scratch).map_err(|error| {
            AgentError::new(
                ErrorCode::Internal,
                format!("failed to copy {}: {error}", real.display()),
            )
        })?;
    }
    let tree = git(dir, &["add", "--all", "."], Some(&scratch))
        .and_then(|_| git(dir, &["write-tree"], Some(&scratch)));
    let _ = std::fs::remove_file(&scratch);
    Ok(tree?.trim().to_string())
}
