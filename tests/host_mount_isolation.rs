//! Mount isolation for host workloads (#714), against the real kernel.
//!
//! Each test builds a node layout under `/var/lib` (not `/tmp`: a workload
//! gets a private `/tmp`, so a fixture there would vanish from its view for
//! the wrong reason), plants a secret in its storage directories, and runs
//! host workloads through the owner-backed process runtime with the node's
//! mount view, exactly as a root Bun on Linux does. A workload is a
//! `/bin/sh` script that exits 0 only if it saw what it should.
//!
//! Root and cgroup v2 are required: the gate's `unshare(CLONE_NEWNS)`, its
//! mounts and the cgroup join are real host operations.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use reliaburger::bun::task_executor::{TaskInvocation, TaskRunner};
use reliaburger::bun::task_runtime::OwnedRunner;
use reliaburger::config::NodeConfig;
use reliaburger::config::job::JobSpec;
use reliaburger::grill::mount_view::MountView;
use reliaburger::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
use reliaburger::grill::process::ProcessGrill;
use reliaburger::grill::state::ContainerState;
use reliaburger::grill::{AnyGrill, Grill, InstanceId};
use tokio_util::sync::CancellationToken;

#[path = "support/first_launch.rs"]
mod first_launch;

const SECRET: &str = "bun-secret-material";

/// A node's storage, laid out under one fixture root, and the process
/// runtime a root Bun would build for it.
struct Fixture {
    root: PathBuf,
    grill: ProcessGrill,
}

impl Fixture {
    fn new() -> Self {
        first_launch::first_launch(&[(Path::new(env!("CARGO_BIN_EXE_bun")), &["--version"])]);
        let root = tempfile::Builder::new()
            .prefix("rb-714-")
            .tempdir_in("/var/lib")
            .unwrap()
            .keep();
        let mut config = NodeConfig::default();
        config.storage.data = root.join("data");
        config.storage.images = root.join("images");
        config.storage.logs = root.join("logs");
        config.storage.metrics = root.join("metrics");
        config.storage.volumes = root.join("volumes");
        for directory in ["data", "images", "logs", "metrics", "volumes/vol", "out"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(root.join("data/secret"), SECRET).unwrap();
        std::fs::write(root.join("volumes/vol/file"), SECRET).unwrap();
        std::fs::create_dir_all("/run/reliaburger").unwrap();
        let view = MountView::for_node(&config, None);
        let grill = ProcessGrill::with_owner(
            config.storage.data.join("instances"),
            env!("CARGO_BIN_EXE_bun").into(),
        )
        .with_mount_view(Some(view));
        eprintln!("mount isolation fixture: {}", root.display());
        Self { root, grill }
    }

    /// A `/tmp` file name no other fixture uses, so a stray file from an
    /// earlier run can't stand in for this one's.
    fn marker(&self) -> String {
        format!(
            "/tmp/{}-marker",
            self.root.file_name().unwrap().to_string_lossy()
        )
    }

    fn path(&self, relative: &str) -> String {
        self.root.join(relative).display().to_string()
    }

    /// The probe every test runs first: none of Bun's paths, the host's
    /// tools still there. A native helper keeps its own directory under
    /// `instances` and its socket under `/run/reliaburger`, so the probe
    /// names what's beside them instead.
    fn hidden_probe(&self) -> String {
        format!(
            r#"for path in {secret} {volume} {instances}; do
  if [ -e "$path" ]; then echo "visible: $path"; exit 1; fi
done
[ -x /usr/bin/env ] || {{ echo "the host's root is missing"; exit 2; }}
"#,
            secret = self.path("data/secret"),
            volume = self.path("volumes/vol/file"),
            instances = self.path("data/instances/process-owners"),
        )
    }

    async fn start(&self, name: &str, script: &str) -> InstanceId {
        let id = InstanceId(format!("default__{name}-0"));
        self.grill.create(&id, &spec(name, script)).await.unwrap();
        self.grill.start(&id).await.unwrap();
        id
    }

    /// Run `script` as a host app to its end; its exit code and output.
    async fn run(&self, name: &str, script: &str) -> (Option<i32>, String) {
        let id = self.start(name, script).await;
        tokio::time::timeout(Duration::from_secs(30), async {
            while !matches!(self.grill.state(&id).await, Ok(ContainerState::Stopped)) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{name} never stopped"));
        let code = self.grill.exit_code(&id).await.unwrap();
        let mut output =
            String::from_utf8_lossy(&self.grill.stdout(&id).await.unwrap_or_default()).into_owned();
        output.push_str(&String::from_utf8_lossy(
            &self.grill.stderr(&id).await.unwrap_or_default(),
        ));
        (code, output)
    }

    async fn pid(&self, id: &InstanceId) -> u32 {
        self.grill.pid(id).await.unwrap().expect("a running pid")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A host workload running `script` under `/bin/sh`, in a cgroup of its
/// own under Bun's tree.
fn spec(name: &str, script: &str) -> OciSpec {
    OciSpec {
        managed_volumes: Vec::new(),
        mount_bind_backs: Vec::new(),
        reusable_executor: false,
        host_process: true,
        initialiser: false,
        root: OciRoot {
            path: reliaburger::grill::oci::HOST_ROOT.into(),
            readonly: false,
        },
        process: OciProcess {
            rlimits: Vec::new(),
            args: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: vec![],
            provider_env: Vec::new(),
            cwd: "/".into(),
            user: OciUser { uid: 0, gid: 0 },
            capabilities: None,
            oom_score_adj: None,
            overrides: None,
        },
        mounts: vec![],
        linux: OciLinux {
            namespaces: vec![],
            resources: None,
            cgroups_path: Some(format!("/reliaburger/rbtest-714/{name}")),
            uid_mappings: None,
            gid_mappings: None,
            sysctl: Default::default(),
            devices: Vec::new(),
        },
        port_mapping: None,
        stop: None,
        runtime_knobs: None,
    }
}

async fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// Whether the mount namespace of `pid` has a mount at `path`.
fn mounted_in(pid: u32, path: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/mountinfo"))
        .unwrap_or_default()
        .lines()
        .any(|line| line.split(' ').nth(4) == Some(path))
}

fn mount_tmpfs(path: &str) {
    std::fs::create_dir_all(path).unwrap();
    nix::mount::mount(
        Some("tmpfs"),
        path,
        Some("tmpfs"),
        nix::mount::MsFlags::empty(),
        Some("size=1m"),
    )
    .unwrap();
}

#[tokio::test]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn a_host_app_cannot_see_buns_storage_or_another_workloads_tmp() {
    let fixture = Fixture::new();
    let marker = fixture.marker();
    let neighbour = fixture
        .start(
            "neighbour",
            &format!("echo neighbour > {marker}; exec sleep 60"),
        )
        .await;
    let pid = fixture.pid(&neighbour).await;
    // The neighbour's /tmp is its own: the host's has no marker.
    wait_for("the neighbour writes its marker", || {
        Path::new(&format!("/proc/{pid}/root{marker}")).exists()
    })
    .await;
    assert!(!Path::new(&marker).exists());

    let script = format!(
        r#"{probe}
[ -e {marker} ] && {{ echo "another workload's /tmp is visible"; exit 3; }}
own=$(sed -n 's/^0:://p' /proc/self/cgroup)
[ -e "/sys/fs/cgroup$own/cgroup.procs" ] || {{ echo "own cgroup $own missing"; exit 4; }}
[ -e /sys/fs/cgroup/reliaburger/rbtest-714/neighbour ] && {{ echo "a neighbour's cgroup is visible"; exit 5; }}
[ -z "$(ls -A {data})" ] || {{ echo "data is not empty"; exit 6; }}
[ -z "$(ls -A /run/reliaburger)" ] || {{ echo "Bun's sockets are visible"; exit 7; }}
exit 0"#,
        probe = fixture.hidden_probe(),
        data = fixture.path("data"),
    );
    let (code, output) = fixture.run("probe", &script).await;
    fixture.grill.kill(&neighbour).await.unwrap();
    assert_eq!(code, Some(0), "{output}");
}

/// A fresh host job attempt is a process-runtime instance of the job's
/// own spec, as `OwnedRunner` launches it where the host executor isn't
/// available.
#[tokio::test]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn a_fresh_host_job_cannot_see_a_hidden_path() {
    let fixture = Fixture::new();
    let template: JobSpec =
        toml::from_str("runtime='process'\nexec='/bin/sh'\nnamespace='rbtest-714'").unwrap();
    let mut job = reliaburger::grill::oci::generate_job_oci_spec(
        "fresh",
        "rbtest-714",
        &template,
        "/reliaburger/rbtest-714/fresh-job",
        None,
    );
    job.process.args = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("{}\nexit 0", fixture.hidden_probe()),
    ];
    job.process.env.clear();
    let id = InstanceId("rbtest-714__fresh-0".into());
    fixture.grill.create(&id, &job).await.unwrap();
    fixture.grill.start(&id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !matches!(fixture.grill.state(&id).await, Ok(ContainerState::Stopped)) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let output = fixture.grill.stdout(&id).await.unwrap_or_default();
    assert_eq!(
        fixture.grill.exit_code(&id).await.unwrap(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output)
    );
}

/// A runner of native host executors over the fixture's runtime: a host
/// job with CPU and memory limits runs in the reusable helper, whose tasks
/// are its children and share its view.
fn native_runner(fixture: &Fixture) -> OwnedRunner<AnyGrill> {
    let budget = reliaburger::bun::execution_budget::ExecutionBudget::new(
        reliaburger::meat::Resources::new(2000, 256 << 20, 0),
    );
    OwnedRunner::with_slot_count(AnyGrill::Process(fixture.grill.clone()), 2).with_budget(budget)
}

fn native_template(namespace: &str) -> JobSpec {
    toml::from_str(&format!(
        "runtime='process'\nexec='/bin/sh'\nnamespace='{namespace}'\ncpu='100m-1000m'\nmemory='32Mi'"
    ))
    .unwrap()
}

fn shell_task(template: &JobSpec, index: u32, script: &str) -> TaskInvocation {
    TaskInvocation {
        template: Some(std::sync::Arc::new(template.clone())),
        index,
        attempt: 1,
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        run: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn a_native_host_task_cannot_see_a_hidden_path() {
    let fixture = Fixture::new();
    let runner = native_runner(&fixture);
    let template = native_template("rbtest-714-native");
    let cancel = CancellationToken::new();
    let script = format!(
        "{}\ngrep -q /task /proc/self/cgroup || {{ echo 'escaped its task cgroup'; exit 7; }}\nexit 0",
        fixture.hidden_probe()
    );
    let attempt = runner
        .run(
            &shell_task(&template, 0, &script),
            Duration::from_secs(20),
            &cancel,
        )
        .await;
    assert!(
        attempt.outcome.succeeded(),
        "{attempt:?}: {}",
        String::from_utf8_lossy(&attempt.output.head)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn tasks_of_one_native_executor_share_one_tmp_and_another_jobs_do_not() {
    let fixture = Fixture::new();
    let runner = native_runner(&fixture);
    let cancel = CancellationToken::new();
    let first = native_template("rbtest-714-first");
    let other = native_template("rbtest-714-other");
    let shared = fixture.marker();
    let write = runner
        .run(
            &shell_task(&first, 0, &format!("echo first > {shared}")),
            Duration::from_secs(20),
            &cancel,
        )
        .await;
    assert!(write.outcome.succeeded(), "{write:?}");
    let same = runner
        .run(
            &shell_task(&first, 1, &format!("cat {shared}")),
            Duration::from_secs(20),
            &cancel,
        )
        .await;
    assert!(same.outcome.succeeded(), "{same:?}");
    assert_eq!(String::from_utf8_lossy(&same.output.head).trim(), "first");
    let another = runner
        .run(
            &shell_task(&other, 0, &format!("[ ! -e {shared} ]")),
            Duration::from_secs(20),
            &cancel,
        )
        .await;
    assert!(
        another.outcome.succeeded(),
        "another job's executor saw the first's /tmp: {another:?}"
    );
    assert!(!Path::new(&shared).exists(), "the host's /tmp has it");
}

/// `relish exec` and stop hooks join the app's namespace with `setns`:
/// they see its `/tmp`, and nothing Bun hides.
#[tokio::test]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn relish_exec_into_a_host_app_sees_the_apps_tmp() {
    let fixture = Fixture::new();
    let marker = fixture.marker();
    let app = fixture
        .start(
            "execd",
            &format!("echo the-apps-own > {marker}; exec sleep 60"),
        )
        .await;
    let pid = fixture.pid(&app).await;
    wait_for("the app writes its marker", || {
        Path::new(&format!("/proc/{pid}/root{marker}")).exists()
    })
    .await;
    assert!(!Path::new(&marker).exists());
    let output = fixture
        .grill
        .exec(&app, &["/bin/cat".to_string(), marker.clone()])
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0, "{}", output.output);
    assert_eq!(output.output.trim(), "the-apps-own");
    let probe = fixture
        .grill
        .exec(
            &app,
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("{}\nexit 0", fixture.hidden_probe()),
            ],
        )
        .await
        .unwrap();
    fixture.grill.kill(&app).await.unwrap();
    assert_eq!(probe.exit_code, 0, "{}", probe.output);
}

/// The mask on a hidden directory stays on top of a mount Bun makes under
/// it later: the propagated mount is tucked beneath the mask (Linux commit
/// 1064f874).
#[tokio::test]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn a_volume_bun_mounts_after_the_workload_starts_stays_hidden() {
    let fixture = Fixture::new();
    let app = fixture.start("late", "exec sleep 60").await;
    let pid = fixture.pid(&app).await;
    let late = fixture.path("volumes/late");
    mount_tmpfs(&late);
    std::fs::write(format!("{late}/file"), SECRET).unwrap();
    // It reaches the workload's namespace, which is what MS_SLAVE is for...
    wait_for("the new volume propagates", || mounted_in(pid, &late)).await;
    // ...under the mask.
    let output = fixture
        .grill
        .exec(
            &app,
            &[
                "/bin/sh".into(),
                "-c".into(),
                format!("[ ! -e {late}/file ]"),
            ],
        )
        .await
        .unwrap();
    fixture.grill.kill(&app).await.unwrap();
    let _ = nix::mount::umount2(late.as_str(), nix::mount::MntFlags::MNT_DETACH);
    assert_eq!(
        output.exit_code, 0,
        "the late volume is visible: {}",
        output.output
    );
}

/// The reason for `MS_SLAVE`, not `MS_PRIVATE`: Bun's unmount reaches a
/// running workload's namespace, so the workload doesn't pin the volume.
/// Under `MS_PRIVATE` the workload keeps its copy and this fails.
#[tokio::test]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn bun_can_unmount_a_volume_while_a_host_workload_runs() {
    let fixture = Fixture::new();
    let early = fixture.path("volumes/early");
    mount_tmpfs(&early);
    let app = fixture.start("pinning", "exec sleep 60").await;
    let pid = fixture.pid(&app).await;
    wait_for("the workload starts in its view", || {
        mounted_in(pid, &fixture.path("data"))
    })
    .await;
    assert!(
        mounted_in(pid, &early),
        "the workload starts with the volume"
    );
    nix::mount::umount(early.as_str()).unwrap();
    wait_for("the unmount reaches the workload", || {
        !mounted_in(pid, &early)
    })
    .await;
    fixture.grill.kill(&app).await.unwrap();
}

#[tokio::test]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn a_workload_writes_to_the_host_root_outside_the_hidden_list() {
    let fixture = Fixture::new();
    let out = fixture.path("out/written");
    let (code, output) = fixture
        .run("writer", &format!("echo from-the-workload > {out}"))
        .await;
    assert_eq!(code, Some(0), "{output}");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap().trim(),
        "from-the-workload"
    );
}

/// Option C: root in the view has no `CAP_SYS_ADMIN` or `CAP_SYS_PTRACE`,
/// so it can't take a mask off, `setns` back to the host's namespace, or
/// read Bun's files through another process's `/proc/<pid>/root`.
#[tokio::test]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn an_isolated_root_workload_cannot_unmount_a_mask_or_setns() {
    let fixture = Fixture::new();
    let data = fixture.path("data");
    let script = format!(
        r#"umount {data} 2>/dev/null && {{ echo "unmounted a mask"; exit 1; }}
umount -l {data} 2>/dev/null && {{ echo "lazily unmounted a mask"; exit 2; }}
nsenter -t 1 -m true 2>/dev/null && {{ echo "setns into PID 1's namespace"; exit 3; }}
cat /proc/1/root{data}/secret 2>/dev/null && {{ echo "read through PID 1's root"; exit 4; }}
cat /proc/{test}/root{data}/secret 2>/dev/null && {{ echo "read through Bun's root"; exit 5; }}
unshare -Urm sh -c "umount {data} 2>/dev/null; cat {data}/secret" 2>/dev/null && {{ echo "a user namespace undid the mask"; exit 6; }}
grep CapBnd /proc/self/status
exit 0"#,
        test = std::process::id(),
    );
    let (code, output) = fixture.run("escape", &script).await;
    assert_eq!(code, Some(0), "{output}");
    let bounding = output
        .lines()
        .find_map(|line| line.strip_prefix("CapBnd:"))
        .map(|value| u64::from_str_radix(value.trim(), 16).unwrap())
        .unwrap_or_else(|| panic!("no CapBnd in {output}"));
    for (bit, name) in [
        (21, "CAP_SYS_ADMIN"),
        (19, "CAP_SYS_PTRACE"),
        (16, "CAP_SYS_MODULE"),
        (39, "CAP_BPF"),
        (22, "CAP_SYS_BOOT"),
    ] {
        assert_eq!(bounding & (1 << bit), 0, "{name} is still in {bounding:x}");
    }
    // Everything else root had is still there.
    assert_ne!(bounding & (1 << 0), 0, "CAP_CHOWN was dropped too");
    assert!(!output.contains(SECRET), "{output}");
}

#[tokio::test]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn a_workloads_tmp_and_var_tmp_share_one_256_mib_cap() {
    let fixture = Fixture::new();
    let script = r#"[ "$(stat -c %d /tmp)" = "$(stat -c %d /var/tmp)" ] || { echo "two filesystems"; exit 1; }
df -B1 --output=size /tmp | tail -1
df -B1 --output=size /dev/shm | tail -1
dd if=/dev/zero of=/tmp/a bs=1M count=200 2>/dev/null || { echo "200 MiB didn't fit in /tmp"; exit 2; }
dd if=/dev/zero of=/var/tmp/b bs=1M count=100 2>/dev/null && { echo "/var/tmp has its own room"; exit 3; }
exit 0"#;
    let (code, output) = fixture.run("tmpcap", script).await;
    assert_eq!(code, Some(0), "{output}");
    let sizes: Vec<u64> = output
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();
    assert_eq!(sizes, vec![256 << 20, 64 << 20], "{output}");
}
