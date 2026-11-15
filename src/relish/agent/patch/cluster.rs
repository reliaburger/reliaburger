//! `relish agent patch try` and `destroy`: a throwaway cluster from the
//! workspace's binaries, kept well away from the user's own.
//!
//! Each cluster is a `relish setup --quickstart --development-binaries` run
//! with its own `RELIABURGER_HOME` under [`Tools::clusters`], so its context,
//! credentials, state and Lima VMs live there and nowhere else. Its name and
//! ports differ from the defaults, so it can't collide with a laptop cluster
//! either. The homes share one download cache, so only the first `try` pays
//! for the guest image.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::builder::{BuiltBinary, Platform, binary_dir};
use super::cargo::last_lines;
use super::git::snapshot;
use super::workspace::State;
use super::{Tools, answer_of, open, patch_command, shown, step};
use crate::relish::agent::envelope::{AgentError, ErrorCode};
use crate::relish::agent::source::Answer;

/// How long setup may take. The first one downloads the guest image.
const SETUP_DEADLINE: Duration = Duration::from_secs(45 * 60);

/// How long removing one cluster may take.
const DESTROY_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// The directory under [`Tools::clusters`] the homes share their downloads
/// through. It's never a cluster.
const SHARED: &str = "shared";

/// The record each home keeps of its cluster, for `destroy --all`.
const RECORD_FILE: &str = "patch-cluster.json";

/// Variables that would point a child `relish` at the user's real cluster.
const CLUSTER_VARIABLES: &[&str] = &[
    "RELIABURGER_ENDPOINT",
    "RELIABURGER_TOKEN",
    "RELIABURGER_CA_CERT",
];

/// A throwaway cluster, as the workspace and its home remember it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterRecord {
    /// The cluster's name, `patch-` and eight hex digits.
    pub name: String,
    /// Its `RELIABURGER_HOME`.
    pub home: PathBuf,
    /// The workspace that made it.
    pub workspace: PathBuf,
    /// One node or three.
    pub nodes: usize,
    /// The first node's API port on this machine.
    pub api_port: u16,
    /// The ingress port on this machine.
    pub ingress_port: u16,
    /// The registry port on this machine.
    pub registry_port: u16,
}

/// The host ports a cluster takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ports {
    api: u16,
    ingress: u16,
    registry: u16,
}

/// Ports for a cluster of `nodes`, in blocks of a hundred from 39000: the
/// API on `x17` upwards, the registry on `x50` and ingress on `x80`. That
/// keeps clear of the laptop cluster's 19117 and 18080 and of the defaults,
/// 9117 and 8080. The first block whose ports are all free wins.
fn pick_ports(nodes: usize, free: impl Fn(u16) -> bool) -> Option<Ports> {
    (0..20u16).find_map(|block| {
        let base = 39_000 + block * 100;
        let ports = Ports {
            api: base + 17,
            ingress: base + 80,
            registry: base + 50,
        };
        let api_range = (0..nodes as u16).map(|index| ports.api + index);
        let all_free = api_range.chain([ports.ingress, ports.registry]).all(&free);
        all_free.then_some(ports)
    })
}

/// Whether nothing listens on `port` on the loopback address.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

/// `data` for `relish agent patch try`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TryData {
    /// The workspace.
    pub dir: String,
    /// The throwaway cluster's name.
    pub cluster: String,
    /// Whether setup finished and the cluster is up. `false` is still an
    /// answer: a patched `bun` that doesn't boot is worth knowing about, and
    /// `log_tail` says how it went.
    pub ready: bool,
    /// The cluster's `RELIABURGER_HOME`: set it to talk to this cluster and
    /// no other.
    pub home: String,
    /// One node or three.
    pub nodes: usize,
    /// The first node's API.
    pub api: String,
    /// The cluster's ingress.
    pub ingress: String,
    /// The `relish` that ran setup: the workspace's own when it built one,
    /// otherwise this one.
    pub driver: String,
    /// The binaries the nodes run.
    pub binaries: Vec<BuiltBinary>,
    /// Setup's full output.
    pub log: String,
    /// The end of setup's output, when it failed.
    pub log_tail: Option<String>,
    /// How long setup took.
    pub seconds: f64,
}

/// A short id for a new cluster, from the workspace, the time and the
/// process.
fn new_id(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let seed = format!("{}|{nanos}|{}", dir.display(), std::process::id());
    hex::encode(Sha256::digest(seed.as_bytes()))[..8].to_string()
}

fn io_error(what: &str, path: &Path, error: std::io::Error) -> AgentError {
    AgentError::new(
        ErrorCode::Internal,
        format!("failed to {what} {}: {error}", path.display()),
    )
}

/// Make a cluster's home, sharing the download cache and the managed Lima
/// with every other home.
fn make_home(clusters: &Path, home: &Path) -> Result<(), AgentError> {
    for shared in ["cache", "tools"] {
        let target = clusters.join(SHARED).join(shared);
        std::fs::create_dir_all(&target).map_err(|error| io_error("create", &target, error))?;
    }
    std::fs::create_dir_all(home).map_err(|error| io_error("create", home, error))?;
    #[cfg(unix)]
    for shared in ["cache", "tools"] {
        let link = home.join(shared);
        let target = clusters.join(SHARED).join(shared);
        std::os::unix::fs::symlink(&target, &link)
            .map_err(|error| io_error("link", &link, error))?;
    }
    Ok(())
}

/// A child `relish` that only knows about the cluster in `home`.
fn child_relish(
    program: &Path,
    home: &Path,
    log: &Path,
) -> Result<tokio::process::Command, AgentError> {
    let output = std::fs::File::create(log).map_err(|error| io_error("create", log, error))?;
    let errors = output
        .try_clone()
        .map_err(|error| io_error("open", log, error))?;
    let mut command = tokio::process::Command::new(program);
    command
        .env("RELIABURGER_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(output)
        .stderr(errors)
        .kill_on_drop(true);
    for variable in CLUSTER_VARIABLES {
        command.env_remove(variable);
    }
    Ok(command)
}

/// Run `command` under `deadline`: `Ok(true)` when it succeeded.
async fn finish(
    mut command: tokio::process::Command,
    deadline: Duration,
    program: &Path,
) -> Result<bool, AgentError> {
    let status = tokio::time::timeout(deadline, command.status())
        .await
        .map_err(|_| {
            AgentError::new(
                ErrorCode::Timeout,
                format!(
                    "{} ran past {} minutes",
                    program.display(),
                    deadline.as_secs() / 60
                ),
            )
        })?
        .map_err(|error| {
            AgentError::new(
                ErrorCode::Internal,
                format!("couldn't run {}: {error}", program.display()),
            )
        })?;
    Ok(status.success())
}

/// `relish agent patch try`.
pub(super) async fn try_cluster(
    tools: &Tools,
    dir: &Path,
    nodes: usize,
) -> Result<Answer<TryData>, AgentError> {
    let started = Instant::now();
    let mut state = open(dir)?;
    if let Some(cluster) = &state.cluster {
        return Err(AgentError::new(
            ErrorCode::Conflict,
            format!(
                "this workspace already has a throwaway cluster, {}",
                cluster.name
            ),
        )
        .with_hint(format!(
            "{} removes it; then try again",
            patch_command("destroy", dir)
        )));
    }
    let linux = binary_dir(dir, Platform::Linux);
    let built = state
        .linux
        .clone()
        .filter(|_| linux.join("bun").is_file() && linux.join("relish").is_file())
        .ok_or_else(|| {
            AgentError::new(ErrorCode::NotFound, "nothing is built for the nodes yet").with_hint(
                format!(
                    "{} --bun builds bun and a Linux relish",
                    patch_command("build", dir)
                ),
            )
        })?;
    let ports = pick_ports(nodes, port_is_free).ok_or_else(|| {
        AgentError::new(
            ErrorCode::Conflict,
            "no free block of ports between 39000 and 40999",
        )
        .with_hint("destroy throwaway clusters you're done with: relish agent patch destroy --all")
    })?;

    let workspace = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let id = new_id(&workspace);
    let home = tools.clusters.join(&id);
    let record = ClusterRecord {
        name: format!("patch-{id}"),
        home: home.clone(),
        workspace,
        nodes,
        api_port: ports.api,
        ingress_port: ports.ingress,
        registry_port: ports.registry,
    };
    make_home(&tools.clusters, &home)?;
    // Remember it before setup starts, so destroy finds a half-built one.
    write_record(&record)?;
    state.cluster = Some(record.clone());
    state.save(dir)?;

    let driver = state
        .host
        .as_ref()
        .and_then(|run| run.binaries.iter().find(|binary| binary.name == "relish"))
        .map(|binary| PathBuf::from(&binary.path))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| tools.relish.clone());
    let log = home.join("setup.log");
    let mut setup = child_relish(&driver, &home, &log)?;
    setup
        .args(["setup", "--quickstart", "--name", &record.name])
        .args(["--nodes", &nodes.to_string()])
        .args(["--api-port", &ports.api.to_string()])
        .args(["--ingress-port", &ports.ingress.to_string()])
        .args(["--registry-port", &ports.registry.to_string()])
        .arg("--development-binaries")
        .arg(&linux);
    let ready = finish(setup, SETUP_DEADLINE, &driver).await?;
    let log_tail = (!ready).then(|| {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        last_lines(&text, 20)
    });

    let home_shown = shown(&home);
    let mut answer = answer_of(TryData {
        dir: shown(dir),
        cluster: record.name.clone(),
        ready,
        home: home_shown.clone(),
        nodes,
        api: format!("https://127.0.0.1:{}", ports.api),
        ingress: format!("http://localhost:{}", ports.ingress),
        driver: shown(&driver),
        binaries: built.binaries.clone(),
        log: shown(&log),
        log_tail,
        seconds: started.elapsed().as_secs_f64(),
    });
    if snapshot(dir)? != built.tree {
        answer.warnings.push(
            "the source changed after the last build: this cluster runs the build, not the \
             change as it is now"
                .to_string(),
        );
    }
    let relish = super::super::shell_quote(&shown(&driver));
    let home_word = super::super::shell_quote(&home_shown);
    if ready {
        answer.next.push(step(
            format!("RELIABURGER_HOME={home_word} {relish} status"),
            "the throwaway cluster, and only it",
            true,
        ));
    }
    answer.next.push(step(
        patch_command("destroy", dir),
        "remove the throwaway cluster when you're done",
        false,
    ));
    Ok(answer)
}

/// Write a home's record of its cluster.
fn write_record(record: &ClusterRecord) -> Result<(), AgentError> {
    let path = record.home.join(RECORD_FILE);
    let json = serde_json::to_string_pretty(record)
        .map_err(|error| AgentError::new(ErrorCode::Internal, error.to_string()))?;
    std::fs::write(&path, format!("{json}\n")).map_err(|error| io_error("write", &path, error))
}

/// One cluster `destroy` dealt with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DestroyedCluster {
    /// The cluster's name.
    pub cluster: String,
    /// Its `RELIABURGER_HOME`.
    pub home: String,
    /// The workspace that made it.
    pub workspace: String,
    /// Whether its VMs and home are gone.
    pub removed: bool,
    /// Why not, when they aren't. The home stays, so destroy can try again.
    pub reason: Option<String>,
}

/// `data` for `relish agent patch destroy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct DestroyData {
    /// Each throwaway cluster, removed or not.
    pub clusters: Vec<DestroyedCluster>,
    /// Workspaces whose source stays where it is: `destroy` removes
    /// clusters, never anybody's change.
    pub kept: Vec<String>,
}

/// Remove one throwaway cluster: its VMs, through `relish local destroy`
/// with its own home, and then the home.
async fn destroy_one(
    tools: &Tools,
    record: &ClusterRecord,
) -> Result<DestroyedCluster, AgentError> {
    let mut destroyed = DestroyedCluster {
        cluster: record.name.clone(),
        home: shown(&record.home),
        workspace: shown(&record.workspace),
        removed: false,
        reason: None,
    };
    // No cluster directory means setup never got as far as a VM.
    if record.home.join("clusters").join(&record.name).exists() {
        let log = record.home.join("destroy.log");
        let mut command = child_relish(&tools.relish, &record.home, &log)?;
        command.args(["local", "destroy", "--name", &record.name, "--yes"]);
        if !finish(command, DESTROY_DEADLINE, &tools.relish).await? {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            destroyed.reason = Some(last_lines(&text, 10));
            return Ok(destroyed);
        }
    }
    // The shared cache and Lima are symbolic links here, and removing a
    // directory removes links without following them.
    if let Err(error) = std::fs::remove_dir_all(&record.home) {
        destroyed.reason = Some(format!(
            "failed to remove {}: {error}",
            record.home.display()
        ));
        return Ok(destroyed);
    }
    destroyed.removed = true;
    Ok(destroyed)
}

/// Forget `record` in its workspace, when the workspace still points at it.
fn forget(record: &ClusterRecord) {
    let Ok(mut state) = State::load(&record.workspace) else {
        return;
    };
    if state
        .cluster
        .as_ref()
        .is_some_and(|cluster| cluster.home == record.home)
    {
        state.cluster = None;
        // The cluster is gone either way; a workspace that can't be
        // written just keeps a stale record, which destroy clears later.
        let _ = state.save(&record.workspace);
    }
}

/// Every cluster record under `clusters`.
fn every_record(clusters: &Path) -> Vec<ClusterRecord> {
    let Ok(entries) = std::fs::read_dir(clusters) else {
        return Vec::new();
    };
    let mut records: Vec<ClusterRecord> = entries
        .flatten()
        .filter(|entry| entry.file_name() != SHARED)
        .filter_map(|entry| {
            let text = std::fs::read_to_string(entry.path().join(RECORD_FILE)).ok()?;
            serde_json::from_str(&text).ok()
        })
        .collect();
    records.sort_by(|a, b| a.name.cmp(&b.name));
    records
}

/// `relish agent patch destroy`.
pub(super) async fn destroy(
    tools: &Tools,
    dir: Option<&Path>,
    all: bool,
) -> Result<Answer<DestroyData>, AgentError> {
    let records = match (dir, all) {
        (_, true) => every_record(&tools.clusters),
        (Some(dir), false) => open(dir)?.cluster.into_iter().collect(),
        (None, false) => {
            return Err(AgentError::new(
                ErrorCode::Usage,
                "name a workspace, or pass --all",
            ));
        }
    };
    let mut data = DestroyData {
        clusters: Vec::new(),
        kept: Vec::new(),
    };
    for record in &records {
        let destroyed = destroy_one(tools, record).await?;
        if destroyed.removed {
            forget(record);
        }
        if !data.kept.contains(&destroyed.workspace) {
            data.kept.push(destroyed.workspace.clone());
        }
        data.clusters.push(destroyed);
    }
    if let (Some(dir), false) = (dir, all)
        && data.kept.is_empty()
    {
        data.kept.push(shown(dir));
    }
    let mut answer = answer_of(data);
    if records.is_empty() {
        answer
            .warnings
            .push("there was no throwaway cluster to destroy".to_string());
    }
    if answer.data.clusters.iter().any(|cluster| !cluster.removed) {
        answer.warnings.push(
            "a cluster couldn't be removed; its home is still there, and destroy can try again"
                .to_string(),
        );
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::super::builder::{Targets, build};
    use super::super::fixture::*;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;
    use crate::relish::agent::envelope::CommandName;
    use crate::relish::agent::source::fixture as source_fixture;
    use crate::relish::agent::source::render_answer;

    /// A cargo that builds bun and relish, as the builder's tests' does.
    const CARGO_BUILDS: &str = r#"
target=""; prev=""; bins=""
for arg in "$@"; do
  [ "$prev" = "--target-dir" ] && target="$arg"
  [ "$prev" = "--bin" ] && bins="$bins $arg"
  prev="$arg"
done
mkdir -p "$target/debug"
for bin in $bins; do
  printf 'binary %s\n' "$bin" > "$target/debug/$bin"
  printf '{"reason":"compiler-artifact","target":{"name":"%s"},"executable":"%s/debug/%s"}\n' "$bin" "$target" "$bin"
done
"#;

    /// A fixture whose workspace has built everything, so `try` can run.
    async fn built() -> Fixture {
        let fixture = Fixture::new(CARGO_BUILDS);
        build(
            &fixture.tools,
            &fixture.dir(),
            Targets::from_flags(false, false),
            false,
        )
        .await
        .unwrap();
        // The workspace's relish drives setup, so make it one that records
        // what it was asked.
        write_script(
            &binary_dir(&fixture.dir(), Platform::Host).join("relish"),
            FAKE_RELISH,
        );
        fixture
    }

    fn rendered<T: Serialize>(
        command: CommandName,
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

    /// The cluster id in `value`, replaced with a placeholder.
    fn without_id(value: serde_json::Value, id: &str) -> serde_json::Value {
        serde_json::from_str(&value.to_string().replace(id, "[id]")).unwrap()
    }

    #[test]
    fn ports_skip_a_block_with_anything_taken() {
        let ports = pick_ports(3, |port| port != 39_018).unwrap();
        assert_eq!(
            ports,
            Ports {
                api: 39_117,
                ingress: 39_180,
                registry: 39_150
            }
        );
        assert_eq!(pick_ports(1, |_| true).unwrap().api, 39_017);
        assert!(pick_ports(1, |_| false).is_none());
    }

    #[test]
    fn no_block_takes_a_default_port() {
        let reserved = [9117, 8080, 19117, 18080, 15050, 29117, 29118, 29119, 28080];
        for block in 0..20u16 {
            // Only this block is free, so it's the one picked.
            let base = 39_000 + block * 100;
            let ports = pick_ports(3, |port| (base..base + 100).contains(&port)).unwrap();
            for port in [ports.api, ports.api + 2, ports.ingress, ports.registry] {
                assert!(!reserved.contains(&port), "{port}");
            }
        }
    }

    #[tokio::test]
    async fn try_needs_the_nodes_binaries_first() {
        let fixture = Fixture::new(CARGO_BUILDS);
        let error = try_cluster(&fixture.tools, &fixture.dir(), 1)
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound);
        assert!(error.hint.unwrap().contains("--bun"));
    }

    #[tokio::test]
    async fn snapshot_of_a_try() {
        let fixture = built().await;
        let dir = fixture.dir();
        let result = try_cluster(&fixture.tools, &dir, 1).await;
        let id = State::load(&dir).unwrap().cluster.unwrap().home;
        let id = id.file_name().unwrap().to_str().unwrap().to_string();
        let mut value = normalised(
            &rendered(CommandName::PatchTry, result),
            fixture.root.path(),
        );
        // Whichever block of ports was free on this machine.
        value["data"]["api"] = "[api]".into();
        value["data"]["ingress"] = "[ingress]".into();
        insta::assert_json_snapshot!(without_id(value, &id));
    }

    #[tokio::test]
    async fn try_runs_setup_with_its_own_home_and_none_of_the_users_cluster() {
        let fixture = built().await;
        let dir = fixture.dir();
        let answer = try_cluster(&fixture.tools, &dir, 3).await.unwrap();
        assert!(answer.data.ready);
        let record = State::load(&dir).unwrap().cluster.unwrap();
        assert!(record.home.starts_with(&fixture.tools.clusters));
        assert!(record.name.starts_with("patch-"));
        let calls = std::fs::read_to_string(record.home.join("calls")).unwrap();
        let expected = format!(
            "setup --quickstart --name {} --nodes 3 --api-port {} --ingress-port {} \
             --registry-port {} --development-binaries {}",
            record.name,
            record.api_port,
            record.ingress_port,
            record.registry_port,
            dir.join(".reliaburger-patch/linux").display()
        );
        assert_eq!(calls.lines().next().unwrap(), expected);
        assert_eq!(calls.lines().nth(1).unwrap(), "endpoint=unset token=unset");
        // The workspace's own relish drove setup: it was built.
        assert!(
            answer
                .data
                .driver
                .ends_with(".reliaburger-patch/bin/relish")
        );
        assert!(record.home.join("cache").is_symlink());
    }

    #[tokio::test]
    async fn a_second_try_is_a_conflict_until_destroy() {
        let fixture = built().await;
        let dir = fixture.dir();
        try_cluster(&fixture.tools, &dir, 1).await.unwrap();
        let error = try_cluster(&fixture.tools, &dir, 1).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        destroy(&fixture.tools, Some(&dir), false).await.unwrap();
        assert!(try_cluster(&fixture.tools, &dir, 1).await.is_ok());
    }

    #[tokio::test]
    async fn a_setup_that_fails_is_an_answer_with_the_end_of_its_log() {
        let fixture = built().await;
        let dir = fixture.dir();
        // The workspace's relish drives setup; make it one that fails.
        write_script(
            &dir.join(".reliaburger-patch/bin/relish"),
            "echo 'bun on rb-1 exited: panicked at src/bun/agent.rs:12' >&2\nexit 1\n",
        );
        let answer = try_cluster(&fixture.tools, &dir, 1).await.unwrap();
        assert!(!answer.data.ready);
        assert!(answer.data.log_tail.unwrap().contains("panicked"));
        assert_eq!(answer.next.len(), 1, "only destroy: {:?}", answer.next);
    }

    #[tokio::test]
    async fn a_try_after_an_edit_warns_that_it_runs_the_old_build() {
        let fixture = built().await;
        let dir = fixture.dir();
        std::fs::write(dir.join("src/lib.rs"), "pub mod client;\n// later\n").unwrap();
        let answer = try_cluster(&fixture.tools, &dir, 1).await.unwrap();
        assert!(
            answer.warnings[0].contains("source changed"),
            "{:?}",
            answer.warnings
        );
    }

    #[tokio::test]
    async fn snapshot_of_a_destroy() {
        let fixture = built().await;
        let dir = fixture.dir();
        try_cluster(&fixture.tools, &dir, 1).await.unwrap();
        let record = State::load(&dir).unwrap().cluster.unwrap();
        let id = record
            .home
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let result = destroy(&fixture.tools, Some(&dir), false).await;
        let calls_were_in_home = !record.home.exists();
        assert!(calls_were_in_home, "the home is gone");
        let value = normalised(
            &rendered(CommandName::PatchDestroy, result),
            fixture.root.path(),
        );
        insta::assert_json_snapshot!(without_id(value, &id));
        assert!(State::load(&dir).unwrap().cluster.is_none());
        assert!(
            fixture.tools.clusters.join("shared/cache").is_dir(),
            "the shared cache stays"
        );
        assert!(dir.join("src/lib.rs").is_file(), "the workspace stays");
    }

    #[tokio::test]
    async fn destroy_runs_local_destroy_in_the_clusters_own_home() {
        let fixture = built().await;
        let dir = fixture.dir();
        try_cluster(&fixture.tools, &dir, 1).await.unwrap();
        let record = State::load(&dir).unwrap().cluster.unwrap();
        // Keep a copy of what the fake relish was asked, since destroy
        // removes the home.
        let log = fixture.root.path().join("calls");
        write_script(
            &fixture.tools.relish,
            &format!("echo \"$RELIABURGER_HOME $@\" >> '{}'\n", log.display()),
        );
        destroy(&fixture.tools, Some(&dir), false).await.unwrap();
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(
            calls.trim(),
            format!(
                "{} local destroy --name {} --yes",
                record.home.display(),
                record.name
            )
        );
    }

    #[tokio::test]
    async fn a_destroy_that_fails_keeps_the_home_for_another_go() {
        let fixture = built().await;
        let dir = fixture.dir();
        try_cluster(&fixture.tools, &dir, 1).await.unwrap();
        write_script(
            &fixture.tools.relish,
            "echo 'VM rb-x-1 is busy' >&2\nexit 1\n",
        );
        let answer = destroy(&fixture.tools, Some(&dir), false).await.unwrap();
        let cluster = &answer.data.clusters[0];
        assert!(!cluster.removed);
        assert!(cluster.reason.as_deref().unwrap().contains("busy"));
        let record = State::load(&dir).unwrap().cluster.unwrap();
        assert!(record.home.is_dir());
    }

    #[tokio::test]
    async fn destroy_all_removes_every_workspaces_cluster() {
        let first = built().await;
        let dir = first.dir();
        try_cluster(&first.tools, &dir, 1).await.unwrap();
        // A second workspace with the same build, sharing the clusters
        // directory.
        let second = first.root.path().join("second");
        workspace_init(&second);
        let linux = binary_dir(&second, Platform::Linux);
        std::fs::create_dir_all(&linux).unwrap();
        for bin in ["bun", "relish"] {
            std::fs::copy(binary_dir(&dir, Platform::Linux).join(bin), linux.join(bin)).unwrap();
        }
        let mut state = State::load(&second).unwrap();
        state.linux = State::load(&dir).unwrap().linux;
        state.save(&second).unwrap();
        try_cluster(&first.tools, &second, 1).await.unwrap();

        let answer = destroy(&first.tools, None, true).await.unwrap();
        assert_eq!(answer.data.clusters.len(), 2);
        assert!(answer.data.clusters.iter().all(|cluster| cluster.removed));
        assert_eq!(answer.data.kept.len(), 2);
        assert!(every_record(&first.tools.clusters).is_empty());
        assert!(State::load(&second).unwrap().cluster.is_none());
    }

    #[tokio::test]
    async fn destroy_with_nothing_to_destroy_says_so() {
        let fixture = Fixture::new(CARGO_BUILDS);
        let answer = destroy(&fixture.tools, Some(&fixture.dir()), false)
            .await
            .unwrap();
        assert!(answer.data.clusters.is_empty());
        assert_eq!(
            answer.warnings,
            ["there was no throwaway cluster to destroy"]
        );
    }

    fn workspace_init(dir: &Path) {
        super::super::workspace::init(&source_fixture::bundle(), dir).unwrap();
    }
}
