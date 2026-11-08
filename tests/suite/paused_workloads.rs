//! A paused workload on the production process runtime: an owned
//! `ProcessGrill`, whose workloads each run under their own `bun` owner.
//!
//! Smoker's `pause` fault sends SIGSTOP to an instance's pid and its clear
//! sends SIGCONT. On macOS the owner once read the stop as the workload's
//! exit and blocked waiting for it, so from the pause on every status read
//! timed out (`pid: null, runtime_unknown: true`), the clear didn't bring
//! the evidence back, and a delete left the instance `stopping` with its
//! workload still running.

use std::path::Path;
use std::time::{Duration, Instant};

use reliaburger::bun::agent::{InstanceStop, stop_runtime_instance};
use reliaburger::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
use reliaburger::grill::process::ProcessGrill;
use reliaburger::grill::state::ContainerState;
use reliaburger::grill::{Grill, InstanceId};
use reliaburger::smoker::process::{pause_process, resume_process};

const CONFIRMATION: Duration = Duration::from_secs(10);

/// How long one runtime read may take. The owner answers in milliseconds;
/// one that doesn't answer fails its read after two seconds.
const READ_BOUND: Duration = Duration::from_secs(5);

fn runtime(directory: &Path) -> ProcessGrill {
    // Every owned start runs a `bun` owner that gets 15 s to answer, so its
    // first launch mustn't land inside that (#1149).
    crate::first_launch::first_launch(&[(Path::new(env!("CARGO_BIN_EXE_bun")), &["--version"])]);
    ProcessGrill::with_owner(directory.to_path_buf(), env!("CARGO_BIN_EXE_bun").into())
}

fn workload(script: &str) -> OciSpec {
    OciSpec {
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
            env: Vec::new(),
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
            cgroups_path: None,
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

/// Start `script` and wait for it to touch `ready`; returns its pid.
async fn started(grill: &ProcessGrill, id: &InstanceId, script: &str, ready: &Path) -> u32 {
    grill.create(id, &workload(script)).await.unwrap();
    grill.start(id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the workload never got ready");
    grill
        .pid(id)
        .await
        .unwrap()
        .expect("a running workload has a pid")
}

/// Whether `ps` reports `pid` as stopped (state `T`).
fn is_stopped(pid: u32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with('T')
}

async fn wait_until_stopped(pid: u32, stopped: bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_stopped(pid) != stopped {
        assert!(
            Instant::now() < deadline,
            "pid {pid} never became stopped={stopped}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Let the owner's 10 ms tick see the change.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// The reads `relish status` makes for an instance (pid, liveness, exit
/// code) answer, and say the workload runs as `pid`.
async fn assert_runtime_evidence(grill: &ProcessGrill, id: &InstanceId, pid: u32, when: &str) {
    let read = async {
        (
            grill.pid(id).await,
            grill.has_exited(id).await,
            grill.exit_code(id).await,
            grill.state(id).await,
        )
    };
    let (read_pid, exited, exit_code, state) = tokio::time::timeout(READ_BOUND, read)
        .await
        .unwrap_or_else(|_| panic!("{when}: the runtime didn't answer"));
    assert_eq!(read_pid.unwrap(), Some(pid), "{when}: pid");
    assert!(!exited.unwrap(), "{when}: read as exited");
    assert_eq!(exit_code.unwrap(), None, "{when}: exit code");
    assert_eq!(state.unwrap(), ContainerState::Running, "{when}: state");
}

fn gone(pid: u32) -> bool {
    reliaburger::grill::records::process_start_time(pid).is_none()
}

#[tokio::test]
async fn a_paused_then_resumed_workload_keeps_its_evidence_and_stops() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let ready = dir.path().join("ready");
    let grill = runtime(&dir.path().join("owners"));
    let id = InstanceId("gym-staging__paused-0".into());
    let script = format!("touch {}; exec sleep 300", ready.display());
    let pid = started(&grill, &id, &script, &ready).await;

    pause_process(pid as i32).unwrap();
    wait_until_stopped(pid, true).await;
    assert_runtime_evidence(&grill, &id, pid, "while paused").await;

    resume_process(pid as i32).unwrap();
    wait_until_stopped(pid, false).await;
    assert_runtime_evidence(&grill, &id, pid, "after the pause is cleared").await;

    let stop = InstanceStop::for_policy(None, Duration::from_secs(10));
    tokio::time::timeout(
        Duration::from_secs(30),
        stop_runtime_instance(&grill, &id, &stop, CONFIRMATION),
    )
    .await
    .expect("the stop never finished")
    .unwrap();
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
    assert!(gone(pid), "the workload outlived its stop");
}

/// A delete of an instance that is still paused stops it with its own
/// signal, within its grace: the stop doesn't wait for the pause to be
/// cleared, nor fall back on the force-kill.
#[tokio::test]
async fn a_still_paused_workload_takes_its_stop_signal_at_once() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (log, ready) = (dir.path().join("log"), dir.path().join("ready"));
    let grill = runtime(&dir.path().join("owners"));
    let id = InstanceId("gym-staging__paused-stop-0".into());
    let script = format!(
        "trap \"echo TERM >> {log}; exit 0\" TERM; touch {ready}; \
         while :; do sleep 0.05; done",
        log = log.display(),
        ready = ready.display()
    );
    let pid = started(&grill, &id, &script, &ready).await;
    pause_process(pid as i32).unwrap();
    wait_until_stopped(pid, true).await;
    assert_runtime_evidence(&grill, &id, pid, "while paused").await;

    let grace = Duration::from_secs(20);
    let begun = Instant::now();
    let stop = InstanceStop::for_policy(None, grace);
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .unwrap();
    let took = begun.elapsed();

    assert!(took < grace, "the stop waited out its grace: {took:?}");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap_or_default().trim(),
        "TERM",
        "the paused workload never handled its stop signal"
    );
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
    assert_eq!(grill.exit_code(&id).await.unwrap(), Some(0));
    assert!(gone(pid), "the workload outlived its stop");
}
