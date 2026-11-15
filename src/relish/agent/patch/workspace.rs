//! A patch workspace: the extracted source on a `patch` branch, with the
//! state `build`, `test` and `try` leave in `.reliaburger-patch/`. `init`
//! makes one; `diff` reports on it.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::builder::{BuiltBinary, Platform, build_changes};
use super::cluster::ClusterRecord;
use super::git::{git, snapshot};
use super::{BRANCH, PATCH_DIR, answer_of, patch_command, shown, step};
use crate::relish::agent::envelope::{AgentError, ErrorCode};
use crate::relish::agent::source::{Answer, Bundle, ExtractData, GitRecord, extract};

/// The state file's format. A workspace from another format is refused
/// rather than misread; `init` a new one.
const STATE_SCHEMA: u32 = 1;

/// How many test runs a workspace remembers.
const KEPT_TEST_RUNS: usize = 20;

/// What a workspace remembers between commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct State {
    pub schema: u32,
    /// The commit `init` made of the extracted source: every diff is
    /// against it.
    pub base: String,
    /// The `relish` version the source came from.
    pub version: String,
    /// The commit the source came from, when the build knew it.
    pub source_commit: Option<String>,
    /// The last build of `relish` for this machine.
    pub host: Option<BuildRun>,
    /// The last build of what the nodes run.
    pub linux: Option<BuildRun>,
    /// The last few test runs, oldest first.
    pub tests: Vec<TestRun>,
    /// The throwaway cluster `try` made, until `destroy` removes it.
    pub cluster: Option<ClusterRecord>,
}

impl State {
    fn path(dir: &Path) -> std::path::PathBuf {
        dir.join(PATCH_DIR).join("state.json")
    }

    /// The state of the workspace at `dir`.
    pub fn load(dir: &Path) -> Result<Self, AgentError> {
        let path = Self::path(dir);
        let text = std::fs::read_to_string(&path).map_err(|_| {
            AgentError::new(
                ErrorCode::NotFound,
                format!("{} isn't a patch workspace", dir.display()),
            )
            .with_hint(format!("{} makes one", patch_command("init", dir)))
        })?;
        let state: State = serde_json::from_str(&text).map_err(|error| {
            AgentError::new(
                ErrorCode::Internal,
                format!("failed to read {}: {error}", path.display()),
            )
        })?;
        if state.schema != STATE_SCHEMA {
            return Err(AgentError::new(
                ErrorCode::Conflict,
                format!(
                    "{} was made by another relish (state format {})",
                    dir.display(),
                    state.schema
                ),
            )
            .with_hint("init a new workspace with this relish"));
        }
        Ok(state)
    }

    /// Write the state back.
    pub fn save(&self, dir: &Path) -> Result<(), AgentError> {
        let path = Self::path(dir);
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(dir.join(PATCH_DIR))?;
            let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
            std::fs::write(&path, format!("{json}\n"))
        };
        write().map_err(|error| {
            AgentError::new(
                ErrorCode::Internal,
                format!("failed to write {}: {error}", path.display()),
            )
        })
    }

    /// Remember a test run, forgetting the oldest past the limit.
    pub fn record_test(&mut self, run: TestRun) {
        self.tests.push(run);
        let excess = self.tests.len().saturating_sub(KEPT_TEST_RUNS);
        self.tests.drain(..excess);
    }
}

/// One build, as the workspace remembers it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildRun {
    /// Which binaries this was.
    pub platform: Platform,
    /// The snapshot of the source it built (a git tree id).
    pub tree: String,
    /// How long it took.
    pub seconds: f64,
    /// What it made.
    pub binaries: Vec<BuiltBinary>,
}

/// One test run, as the workspace remembers it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestRun {
    /// The filter it ran with, or `null` for the whole portable suite.
    pub filter: Option<String>,
    /// The snapshot of the source it tested (a git tree id).
    pub tree: String,
    /// Whether it compiled and every test that ran passed.
    pub ok: bool,
    /// Tests that passed.
    pub passed: u64,
    /// Tests that failed.
    pub failed: u64,
    /// Tests that were ignored: the gated suites, which `patch test` never
    /// runs.
    pub ignored: u64,
    /// How long it took.
    pub seconds: f64,
}

/// A remembered run, and whether it ran against the source as it is now.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Checked<T> {
    #[serde(flatten)]
    pub run: T,
    /// `true` when the source hasn't changed since; `false` means the result
    /// is about an earlier version of the change.
    pub current: bool,
}

/// `data` for `relish agent patch init`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct InitData {
    /// The workspace, as given.
    pub dir: String,
    /// How many files of the source bundle were written.
    pub files: usize,
    /// `sha256:` digest of the source bundle.
    pub digest: String,
    /// The `relish` version the source came from.
    pub version: String,
    /// The commit the source came from, when the build knew it.
    pub source_commit: Option<String>,
    /// The workspace's own commit of the extracted source: `diff` is
    /// against it.
    pub base: String,
    /// The branch the workspace is on.
    pub branch: String,
    /// Where `patch` keeps its state, binaries and logs; git ignores it.
    pub state_dir: String,
}

/// Make a workspace at `dir` from `bundle`.
pub(super) fn init(bundle: &Bundle, dir: &Path) -> Result<Answer<InitData>, AgentError> {
    // Before writing anything: without git there's no workspace to make.
    git(Path::new("."), &["--version"], None)?;
    let extracted: ExtractData = extract(bundle, dir)?.data;
    let base = match extracted.git {
        GitRecord::Committed { commit, .. } => commit,
        GitRecord::Skipped { reason } => {
            return Err(AgentError::new(
                ErrorCode::Internal,
                format!("the source was written but couldn't be committed: {reason}"),
            ));
        }
    };
    git(dir, &["checkout", "--quiet", "-b", BRANCH], None)?;
    ignore_patch_files(dir)?;

    let build = bundle.build_info();
    let state = State {
        schema: STATE_SCHEMA,
        base: base.clone(),
        version: build.version.clone(),
        source_commit: build.commit.clone(),
        host: None,
        linux: None,
        tests: Vec::new(),
        cluster: None,
    };
    state.save(dir)?;

    let mut answer = answer_of(InitData {
        dir: shown(dir),
        files: extracted.files,
        digest: extracted.digest,
        version: build.version,
        source_commit: build.commit,
        base,
        branch: BRANCH.to_string(),
        state_dir: PATCH_DIR.to_string(),
    });
    answer.next = vec![
        step(
            patch_command("test", dir),
            "after editing: the portable suite, or one test by name",
            false,
        ),
        step(
            patch_command("build", dir),
            "after editing: relish for this machine, bun for a throwaway cluster",
            false,
        ),
        step(
            patch_command("diff", dir),
            "the change so far and which tests ran against it",
            true,
        ),
    ];
    Ok(answer)
}

/// Keep `patch`'s own files and cargo's output out of every diff. The
/// bundle has no `.gitignore`, so `target/` needs saying too.
///
/// The file is replaced by a rename, never rewritten in place: git reading
/// it sees the old list or the new one, and a symbolic link (a git template
/// can make every repository's exclude file one shared file) is replaced
/// rather than written through.
fn ignore_patch_files(dir: &Path) -> Result<(), AgentError> {
    let info = dir.join(".git/info");
    let exclude = info.join("exclude");
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for line in [format!("/{PATCH_DIR}/"), "/target/".to_string()] {
        if !text.lines().any(|existing| existing == line) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(&info)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&info)?;
        temporary.write_all(text.as_bytes())?;
        temporary.persist(&exclude).map_err(|error| error.error)?;
        Ok(())
    };
    write().map_err(|error| {
        AgentError::new(
            ErrorCode::Internal,
            format!("failed to write {}: {error}", exclude.display()),
        )
    })
}

/// How a file differs from the extracted source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// New.
    Added,
    /// Changed.
    Modified,
    /// Removed.
    Deleted,
    /// Its type changed, such as a file becoming a symbolic link.
    TypeChanged,
}

/// One file the change touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ChangedFile {
    /// Repository-relative path.
    pub path: String,
    /// How it changed.
    pub status: FileStatus,
    /// Lines added, or `null` for a binary file.
    pub insertions: Option<u64>,
    /// Lines removed, or `null` for a binary file.
    pub deletions: Option<u64>,
}

/// `data` for `relish agent patch diff`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiffData {
    /// The workspace.
    pub dir: String,
    /// The `relish` version the source came from.
    pub version: String,
    /// The commit the source came from, when the build knew it.
    pub source_commit: Option<String>,
    /// The workspace's commit of the extracted source.
    pub base: String,
    /// The snapshot of the source as it is now (a git tree id).
    pub tree: String,
    /// Every file the change touches, committed or not.
    pub files: Vec<ChangedFile>,
    /// Changed files that alter the build itself, which `build` refuses
    /// without `--allow-build-changes`.
    pub build_changes: Vec<String>,
    /// The unified diff against the extracted source.
    pub diff: String,
    /// The test runs, oldest first, and whether each ran against this source.
    pub tests: Vec<Checked<TestRun>>,
    /// The last builds, and whether each built this source.
    pub builds: Vec<Checked<BuildRun>>,
}

/// The files that differ between `base` and the tree `tree`.
pub(super) fn changed_files(
    dir: &Path,
    base: &str,
    tree: &str,
) -> Result<Vec<ChangedFile>, AgentError> {
    let statuses = git(
        dir,
        &["diff", "--no-renames", "--name-status", "-z", base, tree],
        None,
    )?;
    let counts = git(
        dir,
        &["diff", "--no-renames", "--numstat", "-z", base, tree],
        None,
    )?;
    let mut files = parse_name_status(&statuses);
    for (path, insertions, deletions) in parse_numstat(&counts) {
        if let Some(file) = files.iter_mut().find(|file| file.path == path) {
            file.insertions = insertions;
            file.deletions = deletions;
        }
    }
    Ok(files)
}

/// `git diff --name-status -z`: a status letter, NUL, a path, NUL.
fn parse_name_status(text: &str) -> Vec<ChangedFile> {
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        let status = match status {
            "A" => FileStatus::Added,
            "D" => FileStatus::Deleted,
            "T" => FileStatus::TypeChanged,
            _ => FileStatus::Modified,
        };
        files.push(ChangedFile {
            path: path.to_string(),
            status,
            insertions: None,
            deletions: None,
        });
    }
    files
}

/// `git diff --numstat -z`: `added<TAB>deleted<TAB>path` then NUL, with `-`
/// for a binary file's counts.
fn parse_numstat(text: &str) -> Vec<(String, Option<u64>, Option<u64>)> {
    text.split('\0')
        .filter_map(|record| {
            let mut parts = record.splitn(3, '\t');
            let added = parts.next()?;
            let deleted = parts.next()?;
            let path = parts.next()?;
            Some((path.to_string(), added.parse().ok(), deleted.parse().ok()))
        })
        .collect()
}

/// `relish agent patch diff`.
pub(super) fn diff(dir: &Path) -> Result<Answer<DiffData>, AgentError> {
    let state = State::load(dir)?;
    let tree = snapshot(dir)?;
    let files = changed_files(dir, &state.base, &tree)?;
    let text = git(
        dir,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            &state.base,
            &tree,
        ],
        None,
    )?;
    let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
    let tests: Vec<Checked<TestRun>> = state
        .tests
        .iter()
        .map(|run| Checked {
            current: run.tree == tree,
            run: run.clone(),
        })
        .collect();
    let builds: Vec<Checked<BuildRun>> = [&state.host, &state.linux]
        .into_iter()
        .flatten()
        .map(|run| Checked {
            current: run.tree == tree,
            run: run.clone(),
        })
        .collect();

    let mut answer = answer_of(DiffData {
        dir: shown(dir),
        version: state.version.clone(),
        source_commit: state.source_commit.clone(),
        base: state.base.clone(),
        build_changes: build_changes(&paths),
        tree,
        files,
        diff: text,
        tests,
        builds,
    });
    if answer.data.files.is_empty() {
        answer
            .warnings
            .push("nothing has changed since init".to_string());
    }
    if !answer.data.files.is_empty() && !answer.data.tests.iter().any(|run| run.current) {
        answer.warnings.push(
            "no test ran against this version of the change; run patch test before reporting it"
                .to_string(),
        );
        answer.next.push(step(
            patch_command("test", dir),
            "the portable suite against the change as it is now",
            false,
        ));
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::*;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;
    use crate::relish::agent::source::fixture as source_fixture;
    use crate::relish::agent::source::render_answer;

    fn rendered<T: Serialize>(
        command: crate::relish::agent::CommandName,
        result: Result<Answer<T>, AgentError>,
    ) -> crate::relish::agent::Rendered {
        render_answer(
            command,
            source_fixture::bundle().build_info(),
            result,
            &[],
            DEFAULT_MAX_BYTES,
            "relish agent patch",
        )
    }

    #[test]
    fn snapshot_of_an_init() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("work");
        let result = init(&source_fixture::bundle(), &dir);
        let value = normalised(
            &rendered(crate::relish::agent::CommandName::PatchInit, result),
            root.path(),
        );
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn init_leaves_a_clean_patch_branch_that_ignores_its_own_files() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let branch = git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"], None).unwrap();
        assert_eq!(branch.trim(), BRANCH);
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/relish"), "built").unwrap();
        assert_eq!(git(&dir, &["status", "--porcelain"], None).unwrap(), "");
        let state = State::load(&dir).unwrap();
        assert_eq!(state.version, "0.2.7");
        assert_eq!(state.source_commit.as_deref(), Some(source_fixture::COMMIT));
    }

    #[cfg(unix)]
    #[test]
    fn the_ignore_list_never_writes_through_a_shared_exclude_file() {
        // A git template can make .git/info/exclude a symbolic link to one
        // file every repository shares (#1726, #1738, #1748).
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join("shared-exclude");
        std::fs::write(&shared, "# mine\n").unwrap();
        let dir = root.path().join("work");
        std::fs::create_dir_all(dir.join(".git/info")).unwrap();
        std::os::unix::fs::symlink(&shared, dir.join(".git/info/exclude")).unwrap();

        ignore_patch_files(&dir).unwrap();
        ignore_patch_files(&dir).unwrap();

        assert_eq!(std::fs::read_to_string(&shared).unwrap(), "# mine\n");
        let exclude = dir.join(".git/info/exclude");
        assert!(!exclude.symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            format!("# mine\n/{PATCH_DIR}/\n/target/\n"),
            "each line once, after what was there"
        );
    }

    #[test]
    fn init_refuses_a_directory_with_something_in_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mine.txt"), "keep").unwrap();
        let error = init(&source_fixture::bundle(), root.path()).unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
    }

    #[test]
    fn a_directory_that_is_not_a_workspace_is_not_found_with_a_hint() {
        let root = tempfile::tempdir().unwrap();
        let error = State::load(root.path()).unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
        assert!(error.hint.unwrap().contains("relish agent patch init"));
    }

    #[test]
    fn a_snapshot_counts_untracked_files_and_leaves_the_index_alone() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        let before = snapshot(&dir).unwrap();
        assert_eq!(
            before,
            snapshot(&dir).unwrap(),
            "the same files, the same id"
        );
        std::fs::write(dir.join("src/new.rs"), "pub fn new() {}\n").unwrap();
        let after = snapshot(&dir).unwrap();
        assert_ne!(before, after);
        let status = git(&dir, &["status", "--porcelain"], None).unwrap();
        assert_eq!(status.trim(), "?? src/new.rs", "nothing was staged");
    }

    #[test]
    fn snapshot_of_a_diff() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        std::fs::write(dir.join("src/lib.rs"), "pub mod client;\npub mod retry;\n").unwrap();
        std::fs::write(dir.join("src/retry.rs"), "pub fn retry() {}\n").unwrap();
        std::fs::remove_file(dir.join("src/bin/tool.rs")).unwrap();
        // Committed or not, it's all in the change.
        git(&dir, &["add", "src/retry.rs"], None).unwrap();
        let result = diff(&dir);
        let value = normalised(
            &rendered(crate::relish::agent::CommandName::PatchDiff, result),
            fixture.root.path(),
        );
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn a_diff_says_which_test_runs_are_current() {
        let fixture = Fixture::new("exit 0\n");
        let dir = fixture.dir();
        std::fs::write(dir.join("src/lib.rs"), "pub mod client;\n// fixed\n").unwrap();
        let mut state = State::load(&dir).unwrap();
        let run = |tree: String| TestRun {
            filter: None,
            tree,
            ok: true,
            passed: 3,
            failed: 0,
            ignored: 1,
            seconds: 1.0,
        };
        state.record_test(run("stale".to_string()));
        state.record_test(run(snapshot(&dir).unwrap()));
        state.save(&dir).unwrap();
        let answer = diff(&dir).unwrap();
        let current: Vec<bool> = answer.data.tests.iter().map(|run| run.current).collect();
        assert_eq!(current, [false, true]);
        assert!(answer.warnings.is_empty(), "{:?}", answer.warnings);
    }

    #[test]
    fn an_unchanged_workspace_says_so() {
        let fixture = Fixture::new("exit 0\n");
        let answer = diff(&fixture.dir()).unwrap();
        assert!(answer.data.files.is_empty());
        assert_eq!(answer.warnings, ["nothing has changed since init"]);
    }

    #[test]
    fn a_workspace_keeps_its_last_twenty_test_runs() {
        let fixture = Fixture::new("exit 0\n");
        let mut state = State::load(&fixture.dir()).unwrap();
        for index in 0..25 {
            state.record_test(TestRun {
                filter: Some(format!("run-{index}")),
                tree: String::new(),
                ok: true,
                passed: 0,
                failed: 0,
                ignored: 0,
                seconds: 0.0,
            });
        }
        assert_eq!(state.tests.len(), KEPT_TEST_RUNS);
        assert_eq!(state.tests[0].filter.as_deref(), Some("run-5"));
    }
}
