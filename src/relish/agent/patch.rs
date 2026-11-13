//! `relish agent patch`: build, test and try a fix on a throwaway cluster.
//!
//! An agent that found a bug with `relish agent source` proves its fix here.
//! `init` extracts the source into a workspace on a `patch` branch; the agent
//! edits it; `build` compiles `relish` for this machine and `bun` (with a
//! Linux `relish`) for the nodes; `test` runs the portable suite; `try` boots
//! a separately named cluster from those binaries, with its own
//! `RELIABURGER_HOME`; `diff` shows the change and which tests ran against
//! it, for `relish agent report`; `apply` makes an edit from a unified diff,
//! for an agent with no editor of its own; `destroy` removes the throwaway
//! clusters.
//!
//! Nothing here touches the user's own cluster or contexts, and nothing
//! signs what it builds: a patched binary can't take the countersigned
//! upgrade path, so it can't reach a real cluster by that route either.
//!
//! Every command is a function over a [`Tools`], the programs it runs and
//! where it keeps clusters, so the tests swap in small fake `cargo` and
//! `relish` scripts and never compile the real crate.

mod apply;
mod builder;
mod cargo;
mod cluster;
mod git;
mod suite;
mod workspace;

use std::path::{Path, PathBuf};

use super::budget::Rendered;
use super::envelope::{AgentError, BuildInfo, CommandName, ErrorCode, NextStep};
use super::source::{Answer, Bundle, render_answer};

pub use apply::{AppliedFile, AppliedHunk, ApplyData};
pub use builder::{BuildData, BuildHow, BuildStep, BuiltBinary, Platform};
pub use cargo::CompilerError;
pub use cluster::{DestroyData, DestroyedCluster, TryData};
pub use suite::{TestData, TestFailure};
pub use workspace::{ChangedFile, DiffData, FileStatus, InitData, TestRun};

/// The directory inside a workspace where `patch` keeps its state, its
/// binaries and its logs. Git ignores it.
pub const PATCH_DIR: &str = ".reliaburger-patch";

/// The branch `init` puts the workspace on.
pub const BRANCH: &str = "patch";

/// `relish agent patch`'s subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct PatchOptions {
    #[command(subcommand)]
    pub command: PatchCommand,
}

/// The `relish agent patch` commands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum PatchCommand {
    /// Extract this binary's source into a new workspace, on a `patch` branch.
    Init {
        /// The directory to create; it must not exist or be empty.
        dir: PathBuf,
    },
    /// Build the workspace: `relish` for this machine, `bun` and a Linux
    /// `relish` for the nodes (in the build VM on macOS). Both by default.
    Build {
        /// The workspace `init` made.
        dir: PathBuf,
        /// Only `relish`, for this machine.
        #[arg(long)]
        relish: bool,
        /// Only what the nodes run: `bun` and a Linux `relish`.
        #[arg(long)]
        bun: bool,
        /// Build even though the change touches `build.rs`, a `Cargo.toml`,
        /// `Cargo.lock`, `.cargo/` or the toolchain file.
        #[arg(long)]
        allow_build_changes: bool,
    },
    /// Run the portable test suite in the workspace, optionally filtered.
    ///
    /// A failing test or a compile error still exits 0, as every answer
    /// does: `data.ok` is the verdict. The run doesn't see the session's
    /// `RELIABURGER_*` variables or `CARGO_TARGET_DIR`.
    Test {
        /// The workspace `init` made.
        dir: PathBuf,
        /// Only tests whose name contains this.
        filter: Option<String>,
        /// Test even though the change touches the build, as for `build`.
        #[arg(long)]
        allow_build_changes: bool,
    },
    /// Boot a throwaway cluster from the workspace's binaries, with its own
    /// `RELIABURGER_HOME`.
    Try {
        /// The workspace `init` made.
        dir: PathBuf,
        /// One node, or three.
        #[arg(long, default_value_t = 1, value_parser = parse_nodes)]
        nodes: usize,
    },
    /// The change against the extracted source, and the tests that ran.
    Diff {
        /// The workspace `init` made.
        dir: PathBuf,
    },
    /// Apply a unified diff to the workspace, matching each hunk by its
    /// lines rather than its header's counts. All or nothing.
    Apply {
        /// The workspace `init` made.
        dir: PathBuf,
        /// The diff itself, or `@file` to read it from a file.
        #[arg(allow_hyphen_values = true)]
        diff: String,
        /// Match every hunk and report, but write nothing.
        #[arg(long)]
        check: bool,
    },
    /// Remove the workspace's throwaway cluster, or every one with `--all`.
    /// The workspace and its source stay.
    Destroy {
        /// The workspace whose cluster to remove.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        dir: Option<PathBuf>,
        /// Every throwaway cluster `patch try` made.
        #[arg(long)]
        all: bool,
    },
}

impl PatchCommand {
    /// The name the envelope's `command` field carries.
    pub fn name(&self) -> CommandName {
        match self {
            PatchCommand::Init { .. } => CommandName::PatchInit,
            PatchCommand::Build { .. } => CommandName::PatchBuild,
            PatchCommand::Test { .. } => CommandName::PatchTest,
            PatchCommand::Try { .. } => CommandName::PatchTry,
            PatchCommand::Diff { .. } => CommandName::PatchDiff,
            PatchCommand::Apply { .. } => CommandName::PatchApply,
            PatchCommand::Destroy { .. } => CommandName::PatchDestroy,
        }
    }
}

/// `--nodes`: a quickstart cluster has one node or three.
fn parse_nodes(value: &str) -> Result<usize, String> {
    match value {
        "1" => Ok(1),
        "3" => Ok(3),
        _ => Err(format!("{value:?} isn't 1 or 3")),
    }
}

/// The operating system the host runs, as far as `patch` cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    /// Builds `bun` natively.
    Linux,
    /// Builds `bun` in `relish dev`'s Lima build VM.
    Macos,
    /// Can't build `bun` at all.
    Other,
}

impl HostOs {
    /// The host this binary runs on.
    pub fn current() -> Self {
        match std::env::consts::OS {
            "linux" => HostOs::Linux,
            "macos" => HostOs::Macos,
            _ => HostOs::Other,
        }
    }
}

/// The programs `patch` runs and where it keeps its clusters.
#[derive(Debug, Clone)]
pub struct Tools {
    /// `cargo`.
    pub cargo: PathBuf,
    /// This `relish`: it drives `setup` when the workspace has no `relish`
    /// of its own, and always drives `destroy`.
    pub relish: PathBuf,
    /// Where each throwaway cluster gets its own `RELIABURGER_HOME`, next to
    /// a download cache they share.
    pub clusters: PathBuf,
    /// The host's operating system.
    pub os: HostOs,
    /// The free disk a native build or test run needs before it starts.
    pub free_disk_needed: u64,
}

impl Tools {
    /// The real tools: `cargo` from `PATH`, this binary, and
    /// `~/.reliaburger-patch`.
    pub fn current() -> Result<Self, AgentError> {
        let relish = std::env::current_exe().map_err(|error| {
            AgentError::new(
                ErrorCode::Internal,
                format!("couldn't find this relish: {error}"),
            )
        })?;
        let home = dirs::home_dir().ok_or_else(|| {
            AgentError::new(ErrorCode::Internal, "couldn't find the home directory")
        })?;
        Ok(Tools {
            cargo: PathBuf::from("cargo"),
            relish,
            clusters: home.join(".reliaburger-patch"),
            os: HostOs::current(),
            free_disk_needed: builder::BUILD_FREE_DISK,
        })
    }
}

/// `dir` as an answer shows it.
fn shown(dir: &Path) -> String {
    dir.display().to_string()
}

/// A next step that names a workspace.
fn step(run: String, why: &str, read_only: bool) -> NextStep {
    NextStep {
        run,
        why: why.to_string(),
        read_only,
    }
}

/// `relish agent patch <command> <dir>`, quoted.
fn patch_command(command: &str, dir: &Path) -> String {
    format!(
        "relish agent patch {command} {}",
        super::shell_quote(&shown(dir))
    )
}

/// Answer one `relish agent patch` command with `tools`. `bundle` is only
/// read by `init`.
pub async fn answer(
    command: &PatchCommand,
    bundle: impl FnOnce() -> Result<Bundle, AgentError>,
    tools: &Tools,
    max_bytes: usize,
    rerun: &str,
) -> Rendered {
    let name = command.name();
    let build = BuildInfo::current();
    match command {
        PatchCommand::Init { dir } => match bundle() {
            Ok(bundle) => {
                let result = workspace::init(&bundle, dir);
                render_answer(name, bundle.build_info(), result, &[], max_bytes, rerun)
            }
            Err(error) => render_answer::<()>(name, build, Err(error), &[], max_bytes, rerun),
        },
        PatchCommand::Build {
            dir,
            relish,
            bun,
            allow_build_changes,
        } => {
            let targets = builder::Targets::from_flags(*relish, *bun);
            let result = builder::build(tools, dir, targets, *allow_build_changes).await;
            render_answer(name, build, result, &[], max_bytes, rerun)
        }
        PatchCommand::Test {
            dir,
            filter,
            allow_build_changes,
        } => {
            let result = suite::test(tools, dir, filter.as_deref(), *allow_build_changes).await;
            render_answer(name, build, result, &[], max_bytes, rerun)
        }
        PatchCommand::Try { dir, nodes } => {
            let result = cluster::try_cluster(tools, dir, *nodes).await;
            render_answer(name, build, result, &[], max_bytes, rerun)
        }
        PatchCommand::Diff { dir } => {
            let result = workspace::diff(dir);
            render_answer(name, build, result, &[], max_bytes, rerun)
        }
        PatchCommand::Apply { dir, diff, check } => {
            let result = apply::read_diff(diff).and_then(|text| apply::apply(dir, &text, *check));
            render_answer(name, build, result, &[], max_bytes, rerun)
        }
        PatchCommand::Destroy { dir, all } => {
            let result = cluster::destroy(tools, dir.as_deref(), *all).await;
            render_answer(name, build, result, &[], max_bytes, rerun)
        }
    }
}

/// Run one `relish agent patch` command with the real tools.
pub async fn run(options: &PatchOptions, max_bytes: usize, rerun: &str) -> Rendered {
    let tools = match Tools::current() {
        Ok(tools) => tools,
        Err(error) => {
            return render_answer::<()>(
                options.command.name(),
                BuildInfo::current(),
                Err(error),
                &[],
                max_bytes,
                rerun,
            );
        }
    };
    let bundle =
        || Bundle::load().map_err(|error| AgentError::new(ErrorCode::Internal, error.to_string()));
    answer(&options.command, bundle, &tools, max_bytes, rerun).await
}

/// What every command but `init` and `destroy --all` starts with: the
/// workspace's state, or why `dir` isn't a workspace.
fn open(dir: &Path) -> Result<workspace::State, AgentError> {
    workspace::State::load(dir)
}

/// An answer with no next steps or warnings yet.
fn answer_of<T>(data: T) -> Answer<T> {
    Answer {
        data,
        next: Vec::new(),
        warnings: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A workspace extracted from the source fixture, and fake `cargo` and
    //! `relish` scripts that stand in for the real ones.

    use super::*;
    use crate::relish::agent::source::fixture as source_fixture;

    /// A temporary directory holding a workspace at `work`, and the tools.
    pub struct Fixture {
        pub root: tempfile::TempDir,
        pub tools: Tools,
    }

    impl Fixture {
        /// A fresh workspace with the given fake `cargo` script body.
        pub fn new(cargo_script: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let cargo = root.path().join("cargo");
            write_script(&cargo, cargo_script);
            let relish = root.path().join("relish");
            write_script(&relish, FAKE_RELISH);
            let tools = Tools {
                cargo,
                relish,
                clusters: root.path().join("clusters"),
                os: HostOs::Linux,
                free_disk_needed: 0,
            };
            let fixture = Fixture { root, tools };
            workspace::init(&source_fixture::bundle(), &fixture.dir()).unwrap();
            fixture
        }

        /// The workspace.
        pub fn dir(&self) -> PathBuf {
            self.root.path().join("work")
        }
    }

    /// A `cargo` that, like the real one, refuses a `--manifest-path` that
    /// doesn't exist before it runs anything; otherwise one test passes.
    pub const CARGO_CHECKS_THE_MANIFEST: &str = r#"
manifest=""
previous=""
for argument in "$@"; do
  [ "$previous" = --manifest-path ] && manifest="$argument"
  previous="$argument"
done
if [ ! -f "$manifest" ]; then
  echo "error: manifest path \`$manifest\` does not exist" >&2
  exit 101
fi
echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s'
"#;

    /// `path` relative to this process's working directory, the way a
    /// person types `./fix`, without changing the working directory, which
    /// the other tests in the process share.
    pub fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let mut relative = PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.join(path.strip_prefix("/").unwrap())
    }

    /// Write an executable shell script.
    pub fn write_script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// A `relish` that records its arguments and environment in
    /// `$RELIABURGER_HOME/calls`, and for `setup` makes the cluster directory
    /// a real setup would.
    pub const FAKE_RELISH: &str = r#"
echo "$@" >> "$RELIABURGER_HOME/calls"
echo "endpoint=${RELIABURGER_ENDPOINT:-unset} token=${RELIABURGER_TOKEN:-unset}" >> "$RELIABURGER_HOME/calls"
if [ "$1" = setup ]; then
  mkdir -p "$RELIABURGER_HOME/clusters/$4"
  echo "cluster $4 ready"
fi
"#;

    /// Parse a rendered envelope, with `root` and anything that differs run
    /// to run replaced by placeholders, for a snapshot. The canonical form
    /// of `root` goes first: on macOS the temporary directory sits behind a
    /// `/private` symbolic link.
    pub fn normalised(rendered: &Rendered, root: &Path) -> serde_json::Value {
        let canonical = root.canonicalize().unwrap();
        let text = rendered
            .json
            .replace(&canonical.display().to_string(), "[root]")
            .replace(&root.display().to_string(), "[root]");
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        scrub(&mut value);
        value
    }

    /// Replace durations, commit and tree ids with placeholders.
    fn scrub(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map.iter_mut() {
                    match key.as_str() {
                        "seconds" if value.is_number() => *value = 0.into(),
                        "base" | "tree" | "built_tree" | "current_tree" if value.is_string() => {
                            *value = "[id]".into()
                        }
                        _ => scrub(value),
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(scrub),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_its_envelope_name() {
        let dir = PathBuf::from("w");
        let cases = [
            (PatchCommand::Init { dir: dir.clone() }, "patch init"),
            (
                PatchCommand::Build {
                    dir: dir.clone(),
                    relish: false,
                    bun: false,
                    allow_build_changes: false,
                },
                "patch build",
            ),
            (
                PatchCommand::Test {
                    dir: dir.clone(),
                    filter: None,
                    allow_build_changes: false,
                },
                "patch test",
            ),
            (
                PatchCommand::Try {
                    dir: dir.clone(),
                    nodes: 1,
                },
                "patch try",
            ),
            (PatchCommand::Diff { dir: dir.clone() }, "patch diff"),
            (
                PatchCommand::Apply {
                    dir: dir.clone(),
                    diff: "@fix.diff".to_string(),
                    check: false,
                },
                "patch apply",
            ),
            (
                PatchCommand::Destroy {
                    dir: None,
                    all: true,
                },
                "patch destroy",
            ),
        ];
        for (command, name) in cases {
            assert_eq!(command.name().as_str(), name);
        }
    }
}
