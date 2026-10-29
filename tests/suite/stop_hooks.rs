//! Stop hooks (#1121) through the production process runtime: an owned
//! `ProcessGrill`, whose workloads each run under their own `bun` owner
//! process. The owner reads the stop signal from the spec it was launched
//! with, and runs the pre-stop command beside the workload.

use std::path::Path;
use std::time::{Duration, Instant};

use reliaburger::bun::agent::{InstanceStop, stop_runtime_instance};
use reliaburger::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
use reliaburger::grill::process::ProcessGrill;
use reliaburger::grill::state::ContainerState;
use reliaburger::grill::stop::{PreStop, StopPolicy, StopSignal};
use reliaburger::grill::{Grill, InstanceId};

const CONFIRMATION: Duration = Duration::from_secs(10);

fn runtime(directory: &Path) -> ProcessGrill {
    // Every owned start runs a `bun` owner that gets 15 s to answer, so its
    // first launch mustn't land inside that (#1149).
    crate::first_launch::first_launch(&[(Path::new(env!("CARGO_BIN_EXE_bun")), &["--version"])]);
    ProcessGrill::with_owner(directory.to_path_buf(), env!("CARGO_BIN_EXE_bun").into())
}

fn workload(script: &str, stop: StopPolicy) -> OciSpec {
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
        stop: Some(stop),
        runtime_knobs: None,
    }
}

async fn started(grill: &ProcessGrill, id: &InstanceId, spec: &OciSpec, ready: &Path) {
    grill.create(id, spec).await.unwrap();
    grill.start(id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the workload never got ready");
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn an_owned_workload_gets_its_own_signal_after_its_pre_stop() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (log, ready) = (dir.path().join("log"), dir.path().join("ready"));
    let grill = runtime(&dir.path().join("owners"));
    let id = InstanceId("default__owned-hooks-0".into());
    let policy = StopPolicy {
        signal: StopSignal::Int,
        grace: Some(Duration::from_secs(10)),
        pre_stop: Some(PreStop {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("echo pre-stop >> {}", log.display()),
            ],
            timeout: Duration::from_secs(10),
        }),
    };
    let script = format!(
        "for s in TERM INT; do trap \"echo $s >> {log}; exit 0\" $s; done; touch {ready}; \
         while :; do sleep 0.05; done",
        log = log.display(),
        ready = ready.display()
    );
    started(&grill, &id, &workload(&script, policy.clone()), &ready).await;

    let stop = InstanceStop::for_policy(Some(&policy), Duration::from_secs(30));
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .unwrap();

    assert_eq!(lines(&log), ["pre-stop", "INT"]);
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
}

#[tokio::test]
async fn an_owned_workload_that_ignores_its_signal_is_killed_after_its_grace() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let ready = dir.path().join("ready");
    let grill = runtime(&dir.path().join("owners"));
    let id = InstanceId("default__owned-stubborn-0".into());
    let grace = Duration::from_secs(1);
    let policy = StopPolicy {
        signal: StopSignal::Usr1,
        grace: Some(grace),
        pre_stop: None,
    };
    let script = format!(
        "trap '' USR1; touch {}; while :; do sleep 0.05; done",
        ready.display()
    );
    started(&grill, &id, &workload(&script, policy.clone()), &ready).await;

    let begun = Instant::now();
    let stop = InstanceStop::for_policy(Some(&policy), Duration::from_secs(30));
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .unwrap();
    let took = begun.elapsed();

    assert!(took >= grace, "killed before its grace ran out: {took:?}");
    assert!(
        took < Duration::from_secs(15),
        "the app's grace, not the 30s default, must apply: {took:?}"
    );
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
}
