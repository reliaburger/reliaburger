//! `relish agent source extract`: the bundle as a directory an agent can
//! edit, build and test, committed so `git diff` shows what it changed.

use std::path::Path;
use std::process::Command;

use schemars::JsonSchema;
use serde::Serialize;

use super::{Answer, Bundle, quote};
use crate::relish::agent::envelope::{AgentError, ErrorCode};
use crate::relish::source_bundle::{self, SourceBundleError};

/// The stamp `extract` writes next to the tree: which build it came from.
pub const STAMP_FILE: &str = ".reliaburger-source.json";

/// Who the commit of an extracted tree is by. `relish` made it, not the
/// person running it, so it doesn't borrow their git identity.
const COMMIT_AUTHOR: (&str, &str) = ("relish", "relish@reliaburger.invalid");

/// `data` for `relish agent source extract`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ExtractData {
    /// The directory written, as given.
    pub dir: String,
    /// How many files of the bundle were written (the stamp not included).
    pub files: usize,
    /// `sha256:` digest of the bundle that was written.
    pub digest: String,
    /// The stamp file in `dir`, naming the version, commit and digest.
    pub stamp: String,
    /// Whether the tree was committed to a new git repository.
    pub git: GitRecord,
}

/// What became of the git repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GitRecord {
    /// `dir` is a git repository with the whole tree in one commit, so
    /// `git diff` shows every change made after it.
    Committed {
        /// The commit's id (not the build's commit: a new repository).
        commit: String,
        /// Its message, `relish <version> (<build commit>)`.
        message: String,
    },
    /// The tree was written but isn't a repository.
    Skipped {
        /// Why: git isn't installed, or one of its steps failed.
        reason: String,
    },
}

/// What the stamp file holds.
#[derive(Debug, Serialize)]
struct Stamp<'a> {
    version: &'a str,
    commit: Option<&'a str>,
    dirty: Option<bool>,
    digest: String,
}

pub(crate) fn extract(bundle: &Bundle, dir: &Path) -> Result<Answer<ExtractData>, AgentError> {
    let files = source_bundle::extract_from(&bundle.tar, dir).map_err(|error| match error {
        SourceBundleError::NotEmpty(_) => AgentError::new(ErrorCode::Conflict, error.to_string())
            .with_hint("name a directory that doesn't exist yet"),
        other => AgentError::new(ErrorCode::Internal, other.to_string()),
    })?;
    let build = &bundle.build;
    let stamp = Stamp {
        version: &build.version,
        commit: build.commit.as_deref(),
        dirty: build.dirty,
        digest: bundle.digest(),
    };
    let stamp_json = serde_json::to_string_pretty(&stamp)
        .map_err(|error| AgentError::new(ErrorCode::Internal, error.to_string()))?;
    std::fs::write(dir.join(STAMP_FILE), format!("{stamp_json}\n")).map_err(|error| {
        AgentError::new(
            ErrorCode::Internal,
            format!(
                "failed to write {}: {error}",
                dir.join(STAMP_FILE).display()
            ),
        )
    })?;

    let short = build
        .commit
        .as_deref()
        .map(|commit| commit.get(..8).unwrap_or(commit))
        .unwrap_or("unknown commit");
    let message = format!("relish {} ({short})", build.version);
    let git = match commit_tree(dir, &message) {
        Ok(commit) => GitRecord::Committed { commit, message },
        Err(reason) => GitRecord::Skipped { reason },
    };

    let shown = dir.display().to_string();
    let committed = matches!(git, GitRecord::Committed { .. });
    let mut answer = Answer::new(ExtractData {
        dir: shown.clone(),
        files,
        digest: bundle.digest(),
        stamp: STAMP_FILE.to_string(),
        git,
    });
    if committed {
        answer = answer.with_next(
            format!("git -C {} diff", quote(&shown)),
            "after editing, the change against the extracted source",
            true,
        );
    } else {
        answer.warnings.push(
            "the tree isn't a git repository, so compare against a second extract to see a change"
                .to_string(),
        );
    }
    // Building writes a target directory, so it's not read-only by the
    // contract's meaning: an agent should say before it does.
    let manifest = dir.join("Cargo.toml");
    answer = answer.with_next(
        format!(
            "cargo build --locked --bins --manifest-path {}",
            quote(&manifest.display().to_string())
        ),
        "build every binary from this source",
        false,
    );
    Ok(answer)
}

/// `git init` `dir` and commit everything in it, returning the commit id,
/// or why it couldn't.
fn commit_tree(dir: &Path, message: &str) -> Result<String, String> {
    let (name, email) = COMMIT_AUTHOR;
    let identity = [
        format!("user.name={name}"),
        format!("user.email={email}"),
        "commit.gpgsign=false".to_string(),
    ];
    git(dir, &["init", "--quiet"], &[])?;
    // --force: the user's global ignore file mustn't leave a bundled file
    // out of the commit, or `git status` would show it as untracked.
    git(dir, &["add", "--all", "--force", "."], &[])?;
    git(
        dir,
        &["commit", "--quiet", "--no-verify", "-m", message],
        &identity,
    )?;
    let head = git(dir, &["rev-parse", "HEAD"], &[])?;
    Ok(head.trim().to_string())
}

/// Run `git -C dir <args>` with `-c` settings, returning its stdout.
fn git(dir: &Path, args: &[&str], settings: &[String]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    for setting in settings {
        command.arg("-c").arg(setting);
    }
    // A caller's GIT_DIR (say, relish run from a git hook) would point
    // every command at another repository.
    for variable in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        command.env_remove(variable);
    }
    let output = command.args(args).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "git isn't on PATH".to_string()
        } else {
            format!("couldn't run git: {error}")
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {}", args[0], stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::super::SourceCommand;
    use super::super::fixture::*;
    use super::super::nodes::ClusterSource;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;

    fn run(dir: &Path) -> (serde_json::Value, u8) {
        answer_json(
            &bundle(),
            &SourceCommand::Extract {
                dir: dir.to_path_buf(),
            },
            &ClusterSource::Offline,
            DEFAULT_MAX_BYTES,
        )
    }

    fn git_output(dir: &Path, args: &[&str]) -> String {
        git(dir, args, &[]).unwrap()
    }

    #[test]
    fn snapshot_of_an_extract() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("source");
        let (value, exit) = run(&dir);
        assert_eq!(exit, 0);
        // The directory and the new repository's commit differ every run.
        let text = value
            .to_string()
            .replace(&dir.display().to_string(), "[dir]");
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["data"]["git"]["state"], "committed");
        value["data"]["git"]["commit"] = "[commit]".into();
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn the_extracted_tree_is_a_clean_git_repository_with_a_stamp() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("source");
        run(&dir);
        assert_eq!(git_output(&dir, &["status", "--porcelain"]), "");
        assert_eq!(
            git_output(&dir, &["log", "--format=%s %an"]).trim(),
            "relish 0.2.7 (a1b2c3d4) relish"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("src/client.rs")).unwrap(),
            CLIENT_RS
        );
        let stamp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(STAMP_FILE)).unwrap()).unwrap();
        assert_eq!(stamp["version"], "0.2.7");
        assert_eq!(stamp["commit"], COMMIT);
        assert_eq!(stamp["dirty"], false);
        assert_eq!(stamp["digest"], bundle().digest());

        // An edit is exactly what `git diff` shows.
        std::fs::write(dir.join("src/lib.rs"), "pub mod client;\npub mod more;\n").unwrap();
        let diff = git_output(&dir, &["diff", "--stat"]);
        assert!(diff.contains("src/lib.rs"), "{diff}");
        assert!(diff.contains("1 file changed"), "{diff}");
    }

    #[test]
    fn a_directory_that_is_not_empty_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mine.txt"), "keep").unwrap();
        let (value, exit) = run(dir.path());
        assert_eq!(exit, 5);
        assert_eq!(value["error"]["code"], "conflict");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("mine.txt")).unwrap(),
            "keep"
        );
        assert!(!dir.path().join(STAMP_FILE).exists());
    }

    #[test]
    fn an_empty_directory_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let (value, exit) = run(dir.path());
        assert_eq!(exit, 0, "{value}");
        assert_eq!(value["data"]["files"], 6);
    }
}
