//! `relish agent patch build`: compile the workspace, natively or in the
//! build VM, and say where the binaries are, or why they aren't.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::cargo::{CompilerError, Messages, SHOWN_ERRORS, last_lines, parse_messages};
use super::git::snapshot;
use super::workspace::{BuildRun, State, changed_files};
use super::{HostOs, PATCH_DIR, Tools, answer_of, open, patch_command, shown, step};
use crate::relish::agent::envelope::{AgentError, ErrorCode};
use crate::relish::agent::source::Answer;

const GIB: u64 = 1024 * 1024 * 1024;

/// The free disk a native build needs before it starts. A debug build of
/// both binaries leaves about 10 GiB in `target/`.
pub const BUILD_FREE_DISK: u64 = 15 * GIB;

/// How long one build may take. A first build compiles every dependency.
const BUILD_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// The warning every successful build carries.
pub const UNSIGNED_WARNING: &str = "these binaries are unsigned: relish upgrade and a real \
     cluster's node upgrades refuse them, so use them on a throwaway cluster (patch try)";

/// Which binaries to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Targets {
    /// `relish` for this machine.
    pub host: bool,
    /// What the nodes run: `bun` and a Linux `relish`.
    pub nodes: bool,
}

impl Targets {
    /// `--relish` and `--bun`; neither means both.
    pub fn from_flags(relish: bool, bun: bool) -> Self {
        if !relish && !bun {
            return Targets {
                host: true,
                nodes: true,
            };
        }
        Targets {
            host: relish,
            nodes: bun,
        }
    }
}

/// Where a binary runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// This machine: the `relish` you run against the throwaway cluster.
    Host,
    /// The cluster's Linux nodes: `bun`, and the `relish` they carry.
    Linux,
}

/// Where a build step ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildHow {
    /// `cargo` on this machine.
    Native,
    /// `relish dev`'s Lima build VM, because this machine isn't Linux.
    BuildVm,
}

/// One binary a build made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuiltBinary {
    /// `relish` or `bun`.
    pub name: String,
    /// Where it runs.
    pub platform: Platform,
    /// Where it is, inside the workspace's `.reliaburger-patch/`.
    pub path: String,
    /// Lower-case hex SHA-256 of the file.
    pub sha256: String,
    /// Its size.
    pub bytes: u64,
    /// Always `false`: `patch` never signs what it builds.
    pub signed: bool,
}

/// One step of a build.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
pub struct BuildStep {
    /// The binaries this step builds.
    pub binaries: Vec<String>,
    /// The platforms they're for.
    pub platforms: Vec<Platform>,
    /// Where it ran.
    pub how: BuildHow,
    /// Whether it compiled.
    pub ok: bool,
    /// How long it took.
    pub seconds: f64,
}

/// `data` for `relish agent patch build`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BuildData {
    /// The workspace.
    pub dir: String,
    /// Whether every step compiled. `false` is still an answer: `errors`
    /// says why.
    pub ok: bool,
    /// The binaries built, with their digests.
    pub binaries: Vec<BuiltBinary>,
    /// Each step, in order. A failed step is the last.
    pub steps: Vec<BuildStep>,
    /// The first 20 compiler errors, each with its file and line.
    pub errors: Vec<CompilerError>,
    /// How many distinct compiler errors there were.
    pub total_errors: usize,
    /// The end of cargo's own output, when it failed without a compiler
    /// error to show (a lockfile out of date, say).
    pub cargo_output: Option<String>,
    /// Changed files that alter the build itself, built because of
    /// `--allow-build-changes`.
    pub build_changes: Vec<String>,
    /// The snapshot of the source that was built (a git tree id).
    pub tree: String,
    /// How long the whole build took.
    pub seconds: f64,
}

/// Changed paths that alter the build itself rather than the code: the
/// build script, a manifest, the lockfile, cargo's configuration or the
/// toolchain. A build script and a new dependency both run code on this
/// machine at build time, so `build` won't without saying so.
pub fn build_changes(paths: &[&str]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            **path == "build.rs"
                || name == "Cargo.toml"
                || name == "Cargo.lock"
                || path.starts_with(".cargo/")
                || **path == "rust-toolchain"
                || **path == "rust-toolchain.toml"
        })
        .map(|path| path.to_string())
        .collect()
}

/// One cargo invocation, or one build in the VM.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// `cargo build` here, for `bins`; `host` and `linux` say which
    /// platforms the results count for.
    Native {
        bins: Vec<&'static str>,
        host: bool,
        linux: bool,
    },
    /// `bun` and `relish` in the build VM.
    BuildVm,
}

/// The steps that build `targets` on `os`.
fn plan(os: HostOs, targets: Targets) -> Result<Vec<Step>, AgentError> {
    let mut steps = Vec::new();
    match os {
        // One cargo run: the relish it builds is both this machine's and the
        // nodes'.
        HostOs::Linux => {
            let mut bins = vec!["relish"];
            if targets.nodes {
                bins.push("bun");
            }
            steps.push(Step::Native {
                bins,
                host: targets.host,
                linux: targets.nodes,
            });
        }
        HostOs::Macos | HostOs::Other => {
            if targets.host {
                steps.push(Step::Native {
                    bins: vec!["relish"],
                    host: true,
                    linux: false,
                });
            }
            if targets.nodes {
                if os == HostOs::Other {
                    return Err(AgentError::new(
                        ErrorCode::UnsupportedPlatform,
                        "bun builds on Linux, or on macOS in relish dev's build VM",
                    )
                    .with_hint("pass --relish to build only relish for this machine"));
                }
                steps.push(Step::BuildVm);
            }
        }
    }
    Ok(steps)
}

/// What one cargo run left behind.
pub(super) struct CargoRun {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub seconds: f64,
}

/// Run `cargo <args>` in the workspace, with its target directory inside
/// it, under `deadline`.
pub(super) async fn run_cargo(
    tools: &Tools,
    dir: &Path,
    args: &[&str],
    deadline: Duration,
) -> Result<CargoRun, AgentError> {
    let started = Instant::now();
    let mut command = tokio::process::Command::new(&tools.cargo);
    command
        // The subcommand, then our options, then the caller's, which may end
        // in `--` and arguments for the test binaries.
        .args(&args[..1])
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(dir.join("target"))
        .args(&args[1..])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    for variable in session_variables(std::env::vars_os().map(|(name, _)| name)) {
        command.env_remove(variable);
    }
    let output = tokio::time::timeout(deadline, command.output())
        .await
        .map_err(|_| {
            AgentError::new(
                ErrorCode::Timeout,
                format!(
                    "cargo {} ran past {} minutes",
                    args[0],
                    deadline.as_secs() / 60
                ),
            )
        })?
        .map_err(|error| {
            AgentError::new(
                ErrorCode::BuildFailed,
                format!("couldn't run {}: {error}", tools.cargo.display()),
            )
            .with_hint("install Rust with rustup (https://rustup.rs), then build again")
        })?;
    Ok(CargoRun {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// The names, out of `names`, of the session's variables that a build or a
/// test run in the workspace mustn't see.
///
/// `CARGO_TARGET_DIR` would send the build somewhere other than the
/// workspace's `target/`. Every `RELIABURGER_*` goes too: a release build's
/// commit and clean-source check (`RELIABURGER_GIT_SHA`,
/// `RELIABURGER_REQUIRE_CLEAN_SOURCE`) belong to the release; the agent's
/// own `RELIABURGER_TOKEN`, `RELIABURGER_ENDPOINT` or `RELIABURGER_HOME`
/// would reach the tests that read them (#1876); and a gated suite's
/// switch, such as `RELIABURGER_RUNC_TESTS`, would run tests the portable
/// suite leaves out.
pub(super) fn session_variables(
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> Vec<std::ffi::OsString> {
    names
        .into_iter()
        .filter(|name| {
            name == "CARGO_TARGET_DIR"
                || name
                    .to_str()
                    .is_some_and(|name| name.starts_with("RELIABURGER_"))
        })
        .collect()
}

/// Refuse to start when the workspace's disk has less than `needed` free.
pub(super) fn check_disk(dir: &Path, needed: u64) -> Result<(), AgentError> {
    let free = crate::relish::quickstart::preflight::free_disk(dir).map_err(|error| {
        AgentError::new(
            ErrorCode::Internal,
            format!("couldn't read the free disk at {}: {error}", dir.display()),
        )
    })?;
    if free < needed {
        return Err(AgentError::new(
            ErrorCode::BuildFailed,
            format!(
                "{} has {:.1} GiB free and a build needs {} GiB",
                dir.display(),
                free as f64 / GIB as f64,
                needed / GIB
            ),
        )
        .with_hint("free some disk, or init the workspace somewhere with more room"));
    }
    Ok(())
}

/// Refuse a change to the build unless `allowed`.
pub(super) fn check_build_changes(
    dir: &Path,
    state: &State,
    tree: &str,
    allowed: bool,
) -> Result<Vec<String>, AgentError> {
    let files = changed_files(dir, &state.base, tree)?;
    let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
    let changes = build_changes(&paths);
    if !changes.is_empty() && !allowed {
        return Err(AgentError::new(
            ErrorCode::Forbidden,
            format!("the change touches the build: {}", changes.join(", ")),
        )
        .with_hint(
            "a build script or a new dependency runs code on this machine as it builds; \
             read the change, then pass --allow-build-changes",
        ));
    }
    Ok(changes)
}

/// SHA-256 and size of the file at `path`, read off the async runtime.
async fn digest_file(path: PathBuf) -> Result<(String, u64), AgentError> {
    let shown = path.display().to_string();
    tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(&path)?;
        Ok::<_, std::io::Error>((hex::encode(Sha256::digest(&bytes)), bytes.len() as u64))
    })
    .await
    .map_err(|error| AgentError::new(ErrorCode::Internal, error.to_string()))?
    .map_err(|error| {
        AgentError::new(
            ErrorCode::Internal,
            format!("failed to read {shown}: {error}"),
        )
    })
}

/// Copy `from` to `to`, replacing it, and describe the copy.
async fn keep_binary(
    name: &str,
    platform: Platform,
    from: &Path,
    to: &Path,
) -> Result<BuiltBinary, AgentError> {
    let copy = || -> std::io::Result<()> {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Replace rather than overwrite: a running copy of the old binary
        // keeps its inode.
        let _ = std::fs::remove_file(to);
        std::fs::copy(from, to).map(|_| ())
    };
    copy().map_err(|error| {
        AgentError::new(
            ErrorCode::Internal,
            format!(
                "failed to copy {} to {}: {error}",
                from.display(),
                to.display()
            ),
        )
    })?;
    describe(name, platform, to).await
}

async fn describe(name: &str, platform: Platform, path: &Path) -> Result<BuiltBinary, AgentError> {
    let (sha256, bytes) = digest_file(path.to_path_buf()).await?;
    Ok(BuiltBinary {
        name: name.to_string(),
        platform,
        path: path.display().to_string(),
        sha256,
        bytes,
        signed: false,
    })
}

/// Where `build` keeps a platform's binaries.
pub(super) fn binary_dir(dir: &Path, platform: Platform) -> PathBuf {
    let leaf = match platform {
        Platform::Host => "bin",
        Platform::Linux => "linux",
    };
    dir.join(PATCH_DIR).join(leaf)
}

/// What one step produced.
struct StepResult {
    step: BuildStep,
    binaries: Vec<BuiltBinary>,
    messages: Messages,
    stderr: String,
}

async fn run_step(tools: &Tools, dir: &Path, step: &Step) -> Result<StepResult, AgentError> {
    match step {
        Step::Native { bins, host, linux } => {
            let mut args = vec!["build", "--locked", "--message-format=json"];
            // A node's bun refuses to start without eBPF: durable service
            // discovery needs it.
            if *linux {
                args.extend(["--features", "ebpf"]);
            }
            for bin in bins {
                args.extend(["--bin", bin]);
            }
            let run = run_cargo(tools, dir, &args, BUILD_DEADLINE).await?;
            let messages = parse_messages(&run.stdout);
            let mut platforms = Vec::new();
            if *host {
                platforms.push(Platform::Host);
            }
            if *linux {
                platforms.push(Platform::Linux);
            }
            let mut binaries = Vec::new();
            if run.success {
                for (name, path) in &messages.executables {
                    if !bins.contains(&name.as_str()) {
                        continue;
                    }
                    for platform in &platforms {
                        // The host only gets relish.
                        if *platform == Platform::Host && name != "relish" {
                            continue;
                        }
                        let to = binary_dir(dir, *platform).join(name);
                        binaries.push(keep_binary(name, *platform, Path::new(path), &to).await?);
                    }
                }
            }
            Ok(StepResult {
                step: BuildStep {
                    binaries: bins.iter().map(|bin| bin.to_string()).collect(),
                    platforms,
                    how: BuildHow::Native,
                    ok: run.success,
                    seconds: run.seconds,
                },
                binaries,
                messages,
                stderr: run.stderr,
            })
        }
        Step::BuildVm => build_in_vm(dir).await,
    }
}

async fn build_in_vm(dir: &Path) -> Result<StepResult, AgentError> {
    use crate::relish::dev::{self, VmBuildOutput};
    if !dev::lima_available() {
        return Err(AgentError::new(
            ErrorCode::UnsupportedPlatform,
            "building bun on macOS needs Lima, for relish dev's build VM",
        )
        .with_hint("brew install lima, or pass --relish to build only relish"));
    }
    let home = dirs::home_dir().unwrap_or_default();
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if !canonical.starts_with(&home) {
        return Err(AgentError::new(
            ErrorCode::UnsupportedPlatform,
            format!(
                "the build VM only sees your home directory, and {} isn't in it",
                dir.display()
            ),
        )
        .with_hint("init the workspace under your home directory"));
    }
    let linux = binary_dir(dir, Platform::Linux);
    std::fs::create_dir_all(&linux).map_err(|error| {
        AgentError::new(
            ErrorCode::Internal,
            format!("failed to create {}: {error}", linux.display()),
        )
    })?;
    let started = Instant::now();
    let built = dev::build_binaries_in_vm(
        &canonical,
        &linux.join("bun"),
        &linux.join("relish"),
        VmBuildOutput::Capture,
    )
    .await
    .map_err(|error| AgentError::new(ErrorCode::BuildFailed, error.to_string()))?;
    let messages = parse_messages(&built.cargo_stdout);
    let mut binaries = Vec::new();
    if let Some((bun, relish)) = &built.binaries {
        binaries.push(describe("bun", Platform::Linux, bun).await?);
        binaries.push(describe("relish", Platform::Linux, relish).await?);
    }
    Ok(StepResult {
        step: BuildStep {
            binaries: vec!["bun".to_string(), "relish".to_string()],
            platforms: vec![Platform::Linux],
            how: BuildHow::BuildVm,
            ok: built.binaries.is_some(),
            seconds: started.elapsed().as_secs_f64(),
        },
        binaries,
        messages,
        stderr: String::new(),
    })
}

/// `relish agent patch build`.
pub(super) async fn build(
    tools: &Tools,
    dir: &Path,
    targets: Targets,
    allow_build_changes: bool,
) -> Result<Answer<BuildData>, AgentError> {
    let started = Instant::now();
    let mut state = open(dir)?;
    let tree = snapshot(dir)?;
    let build_changes = check_build_changes(dir, &state, &tree, allow_build_changes)?;
    let steps = plan(tools.os, targets)?;
    if steps.iter().any(|step| matches!(step, Step::Native { .. })) {
        check_disk(dir, tools.free_disk_needed)?;
    }

    let mut data = BuildData {
        dir: shown(dir),
        ok: true,
        binaries: Vec::new(),
        steps: Vec::new(),
        errors: Vec::new(),
        total_errors: 0,
        cargo_output: None,
        build_changes,
        tree: tree.clone(),
        seconds: 0.0,
    };
    for step in &steps {
        let result = run_step(tools, dir, step).await?;
        let ok = result.step.ok;
        if ok {
            remember(&mut state, &tree, &result);
        } else {
            data.ok = false;
            data.total_errors = result.messages.errors.len();
            data.errors = result
                .messages
                .errors
                .into_iter()
                .take(SHOWN_ERRORS)
                .collect();
            if data.errors.is_empty() {
                data.cargo_output = Some(last_lines(&result.stderr, 20));
            }
        }
        data.binaries.extend(result.binaries);
        data.steps.push(result.step);
        if !ok {
            break;
        }
    }
    data.seconds = started.elapsed().as_secs_f64();
    state.save(dir)?;

    let mut answer = answer_of(data);
    if answer.data.ok {
        answer.warnings.push(UNSIGNED_WARNING.to_string());
        if !answer.data.build_changes.is_empty() {
            answer.warnings.push(format!(
                "built with changes to the build itself: {}",
                answer.data.build_changes.join(", ")
            ));
        }
        answer.next.push(step(
            patch_command("test", dir),
            "the portable suite against this change",
            false,
        ));
        if state.linux.as_ref().is_some_and(|run| run.tree == tree) {
            answer.next.push(step(
                patch_command("try", dir),
                "boot a throwaway cluster from these binaries",
                false,
            ));
        }
    } else {
        answer.next.push(step(
            patch_command("build", dir),
            "after fixing the errors",
            false,
        ));
    }
    Ok(answer)
}

/// Record a successful step's binaries in the workspace's state.
fn remember(state: &mut State, tree: &str, result: &StepResult) {
    for platform in &result.step.platforms {
        let binaries: Vec<BuiltBinary> = result
            .binaries
            .iter()
            .filter(|binary| binary.platform == *platform)
            .cloned()
            .collect();
        let run = Some(BuildRun {
            platform: *platform,
            tree: tree.to_string(),
            seconds: result.step.seconds,
            binaries,
        });
        match platform {
            Platform::Host => state.host = run,
            Platform::Linux => state.linux = run,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixture::*;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;
    use crate::relish::agent::envelope::CommandName;
    use crate::relish::agent::source::fixture as source_fixture;
    use crate::relish::agent::source::render_answer;

    /// A cargo that "builds" every `--bin` it's given into the target
    /// directory, and says so in cargo's JSON.
    const CARGO_BUILDS: &str = r#"
target=""
prev=""
bins=""
for arg in "$@"; do
  [ "$prev" = "--target-dir" ] && target="$arg"
  [ "$prev" = "--bin" ] && bins="$bins $arg"
  prev="$arg"
done
mkdir -p "$target/debug"
echo "$@" > "$target/args"
for bin in $bins; do
  printf 'binary %s\n' "$bin" > "$target/debug/$bin"
  printf '{"reason":"compiler-artifact","target":{"name":"%s"},"executable":"%s/debug/%s"}\n' "$bin" "$target" "$bin"
done
echo '{"reason":"build-finished","success":true}'
"#;

    /// A cargo whose build fails with a type error.
    const CARGO_FAILS: &str = r#"
printf '%s\n' '{"reason":"compiler-message","target":{"name":"reliaburger"},"message":{"rendered":"error[E0308]: mismatched types\n --> src/client.rs:12:5\n","code":{"code":"E0308"},"level":"error","message":"mismatched types","spans":[{"file_name":"src/client.rs","line_start":12,"column_start":5,"is_primary":true}]}}'
printf '%s\n' '{"reason":"compiler-message","target":{"name":"reliaburger"},"message":{"rendered":"error: aborting due to 1 previous error\n","code":null,"level":"error","message":"aborting due to 1 previous error","spans":[]}}'
echo 'error: could not compile `reliaburger` (lib) due to 1 previous error' >&2
exit 101
"#;

    /// A cargo that refuses to start, as with a stale lockfile.
    const CARGO_REFUSES: &str = r#"
echo 'error: the lock file /w/Cargo.lock needs to be updated but --locked was passed to prevent this' >&2
exit 101
"#;

    fn rendered(result: Result<Answer<BuildData>, AgentError>) -> crate::relish::agent::Rendered {
        render_answer(
            CommandName::PatchBuild,
            source_fixture::bundle().build_info(),
            result,
            &[],
            DEFAULT_MAX_BYTES,
            "relish agent patch build",
        )
    }

    #[test]
    fn neither_flag_builds_both() {
        assert_eq!(
            Targets::from_flags(false, false),
            Targets {
                host: true,
                nodes: true
            }
        );
        assert_eq!(
            Targets::from_flags(true, false),
            Targets {
                host: true,
                nodes: false
            }
        );
    }

    #[test]
    fn linux_builds_everything_in_one_cargo_run_and_macos_uses_the_build_vm() {
        let both = Targets::from_flags(false, false);
        assert_eq!(
            plan(HostOs::Linux, both).unwrap(),
            [Step::Native {
                bins: vec!["relish", "bun"],
                host: true,
                linux: true
            }]
        );
        assert_eq!(
            plan(HostOs::Linux, Targets::from_flags(false, true)).unwrap(),
            [Step::Native {
                bins: vec!["relish", "bun"],
                host: false,
                linux: true
            }],
            "the nodes need a Linux relish next to bun"
        );
        assert_eq!(
            plan(HostOs::Macos, both).unwrap(),
            [
                Step::Native {
                    bins: vec!["relish"],
                    host: true,
                    linux: false
                },
                Step::BuildVm
            ]
        );
        let error = plan(HostOs::Other, both).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedPlatform);
        assert!(plan(HostOs::Other, Targets::from_flags(true, false)).is_ok());
    }

    #[test]
    fn build_changes_are_the_build_script_manifests_lockfile_and_toolchain() {
        let paths = [
            "src/relish/client.rs",
            "build.rs",
            "src/relish/agent/patch/builder.rs",
            "src/nested/build.rs",
            "Cargo.toml",
            "fuzz/Cargo.toml",
            "Cargo.lock",
            ".cargo/config.toml",
            "rust-toolchain.toml",
        ];
        assert_eq!(
            build_changes(&paths),
            [
                "build.rs",
                "Cargo.toml",
                "fuzz/Cargo.toml",
                "Cargo.lock",
                ".cargo/config.toml",
                "rust-toolchain.toml"
            ]
        );
    }

    #[tokio::test]
    async fn snapshot_of_a_build() {
        let fixture = Fixture::new(CARGO_BUILDS);
        let dir = fixture.dir();
        std::fs::write(dir.join("src/lib.rs"), "pub mod client;\n// fixed\n").unwrap();
        let result = build(
            &fixture.tools,
            &dir,
            Targets::from_flags(false, false),
            false,
        )
        .await;
        let value = normalised(&rendered(result), fixture.root.path());
        insta::assert_json_snapshot!(value);
    }

    #[tokio::test]
    async fn a_build_keeps_the_binaries_and_remembers_what_it_built() {
        let fixture = Fixture::new(CARGO_BUILDS);
        let dir = fixture.dir();
        let answer = build(
            &fixture.tools,
            &dir,
            Targets::from_flags(true, false),
            false,
        )
        .await
        .unwrap();
        assert!(answer.data.ok);
        let kept = binary_dir(&dir, Platform::Host).join("relish");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "binary relish\n");
        assert_eq!(answer.data.binaries.len(), 1);
        assert!(!answer.data.binaries[0].signed);
        let args = std::fs::read_to_string(dir.join("target/args")).unwrap();
        assert!(args.starts_with("build --manifest-path "), "{args}");
        assert!(
            args.ends_with(" --locked --message-format=json --bin relish\n"),
            "{args}"
        );
        let state = State::load(&dir).unwrap();
        assert_eq!(state.host.unwrap().tree, answer.data.tree);
        assert!(state.linux.is_none());
        assert!(answer.warnings.contains(&UNSIGNED_WARNING.to_string()));
    }

    #[tokio::test]
    async fn snapshot_of_a_build_with_compiler_errors() {
        let fixture = Fixture::new(CARGO_FAILS);
        let result = build(
            &fixture.tools,
            &fixture.dir(),
            Targets::from_flags(true, false),
            false,
        )
        .await;
        let rendered = rendered(result);
        assert_eq!(
            rendered.exit.code(),
            0,
            "a failed compile is still an answer"
        );
        insta::assert_json_snapshot!(normalised(&rendered, fixture.root.path()));
    }

    #[tokio::test]
    async fn cargo_refusing_to_start_shows_its_own_words() {
        let fixture = Fixture::new(CARGO_REFUSES);
        let answer = build(
            &fixture.tools,
            &fixture.dir(),
            Targets::from_flags(true, false),
            false,
        )
        .await
        .unwrap();
        assert!(!answer.data.ok);
        assert!(answer.data.errors.is_empty());
        assert!(
            answer
                .data
                .cargo_output
                .unwrap()
                .contains("needs to be updated")
        );
    }

    #[tokio::test]
    async fn snapshot_of_a_build_refused_for_touching_the_build() {
        let fixture = Fixture::new(CARGO_BUILDS);
        let dir = fixture.dir();
        std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
        let result = build(
            &fixture.tools,
            &dir,
            Targets::from_flags(true, false),
            false,
        )
        .await;
        let rendered = rendered(result);
        assert_eq!(rendered.exit.code(), 4);
        assert!(!dir.join("target/args").exists(), "cargo never ran");
        insta::assert_json_snapshot!(normalised(&rendered, fixture.root.path()));
    }

    #[tokio::test]
    async fn allow_build_changes_builds_and_says_so() {
        let fixture = Fixture::new(CARGO_BUILDS);
        let dir = fixture.dir();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"changed\"\n").unwrap();
        let answer = build(&fixture.tools, &dir, Targets::from_flags(true, false), true)
            .await
            .unwrap();
        assert!(answer.data.ok);
        assert_eq!(answer.data.build_changes, ["Cargo.toml"]);
        assert!(
            answer
                .warnings
                .iter()
                .any(|warning| warning.contains("Cargo.toml"))
        );
    }

    #[tokio::test]
    async fn only_the_nodes_build_has_ebpf_which_their_bun_needs_to_start() {
        let fixture = Fixture::new(CARGO_BUILDS);
        let dir = fixture.dir();
        let args = || std::fs::read_to_string(dir.join("target/args")).unwrap();
        build(
            &fixture.tools,
            &dir,
            Targets::from_flags(true, false),
            false,
        )
        .await
        .unwrap();
        assert!(!args().contains("ebpf"), "{}", args());
        build(
            &fixture.tools,
            &dir,
            Targets::from_flags(false, true),
            false,
        )
        .await
        .unwrap();
        assert!(
            args().contains(" --features ebpf --bin relish --bin bun"),
            "{}",
            args()
        );
    }

    #[test]
    fn too_little_disk_refuses_to_build() {
        let dir = tempfile::tempdir().unwrap();
        let error = check_disk(dir.path(), u64::MAX).unwrap_err();
        assert_eq!(error.code, ErrorCode::BuildFailed);
        assert!(error.message.contains("GiB free"), "{}", error.message);
        assert!(check_disk(dir.path(), 0).is_ok());
    }

    #[tokio::test]
    async fn a_missing_cargo_is_build_failed_with_how_to_install_rust() {
        let mut fixture = Fixture::new("");
        fixture.tools.cargo = fixture.root.path().join("no-cargo-here");
        let error = build(
            &fixture.tools,
            &fixture.dir(),
            Targets::from_flags(true, false),
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::BuildFailed);
        assert!(error.hint.unwrap().contains("rustup"));
    }

    #[test]
    fn the_sessions_reliaburger_variables_and_target_dir_stay_out_of_cargo() {
        let names = [
            "PATH",
            "HOME",
            "RUSTUP_HOME",
            "CARGO_HOME",
            "CARGO_TARGET_DIR",
            "RELIABURGER_TOKEN",
            "RELIABURGER_ENDPOINT",
            "RELIABURGER_CA_CERT",
            "RELIABURGER_HOME",
            "RELIABURGER_GIT_SHA",
            "RELIABURGER_REQUIRE_CLEAN_SOURCE",
            "RELIABURGER_RUNC_TESTS",
            "RELIABURGER",
            "MY_RELIABURGER_TOKEN",
        ]
        .map(std::ffi::OsString::from);
        let removed = session_variables(names);
        assert_eq!(
            removed,
            [
                "CARGO_TARGET_DIR",
                "RELIABURGER_TOKEN",
                "RELIABURGER_ENDPOINT",
                "RELIABURGER_CA_CERT",
                "RELIABURGER_HOME",
                "RELIABURGER_GIT_SHA",
                "RELIABURGER_REQUIRE_CLEAN_SOURCE",
                "RELIABURGER_RUNC_TESTS",
            ]
            .map(std::ffi::OsString::from)
        );
    }
}
