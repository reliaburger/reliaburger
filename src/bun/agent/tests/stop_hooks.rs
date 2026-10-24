//! Stop hooks (#1121): an app's stop signal, grace and pre-stop command.
//!
//! The first group drives the stop sequence against real processes through
//! `ProcessGrill`, so a trap in a shell script is what proves which signal
//! arrived and when. The second group checks the wiring with the mock
//! runtime: an operator stop and a deploy's retirement both run the
//! instance's own pre-stop command before they signal it.

use std::path::Path;
use std::time::{Duration, Instant};

use super::*;
use crate::grill::ProcessGrill;
use crate::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
use crate::grill::stop::{PreStop, StopPolicy, StopSignal};

/// Generous for the runtime's own work; the graces under test are shorter.
const CONFIRMATION: Duration = Duration::from_secs(5);

/// A host workload running `script` under `/bin/sh`, stopped as `stop` says.
fn workload(script: &str, stop: StopPolicy) -> OciSpec {
    OciSpec {
        reusable_executor: false,
        host_process: true,
        initialiser: false,
        root: OciRoot {
            path: crate::grill::oci::HOST_ROOT.into(),
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
        mounts: Vec::new(),
        linux: OciLinux {
            namespaces: Vec::new(),
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

/// Start `spec` and wait until its script has said it's ready, so a signal
/// can't land before its traps are set.
async fn started(grill: &ProcessGrill, id: &InstanceId, spec: &OciSpec, ready: &Path) {
    grill.create(id, spec).await.unwrap();
    grill.start(id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
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

/// A script that records each listed signal in `log` and exits 0 on it.
fn trapping(log: &Path, ready: &Path) -> String {
    let log = log.display();
    format!(
        "for s in TERM INT QUIT; do trap \"echo $s >> {log}; exit 0\" $s; done; \
         touch {ready}; while :; do sleep 0.05; done",
        ready = ready.display()
    )
}

#[tokio::test]
async fn the_configured_signal_arrives_and_a_trapping_workload_exits_within_its_grace() {
    let dir = tempfile::tempdir().unwrap();
    let (log, ready) = (dir.path().join("log"), dir.path().join("ready"));
    let grill = ProcessGrill::new();
    let id = InstanceId("default__trap-0".into());
    let policy = StopPolicy {
        signal: StopSignal::Int,
        grace: Some(Duration::from_secs(10)),
        pre_stop: None,
    };
    started(
        &grill,
        &id,
        &workload(&trapping(&log, &ready), policy.clone()),
        &ready,
    )
    .await;

    let begun = Instant::now();
    let stop = InstanceStop::for_policy(Some(&policy), Duration::from_secs(30));
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .unwrap();

    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "a workload that exits on its signal mustn't wait out the grace: {:?}",
        begun.elapsed()
    );
    assert_eq!(lines(&log), ["INT"], "SIGINT, not SIGTERM, must arrive");
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
    assert_eq!(
        grill.exit_code(&id).await.unwrap(),
        Some(0),
        "it exited by itself, not by the kill"
    );
}

#[tokio::test]
async fn a_workload_that_ignores_its_signal_is_killed_once_the_grace_runs_out() {
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("ready");
    let grill = ProcessGrill::new();
    let id = InstanceId("default__stubborn-0".into());
    let grace = Duration::from_secs(1);
    let policy = StopPolicy {
        signal: StopSignal::Term,
        grace: Some(grace),
        pre_stop: None,
    };
    let script = format!(
        "trap '' TERM; touch {}; while :; do sleep 0.05; done",
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
        took < grace + Duration::from_secs(4),
        "the app's 1s grace, not the 30s default, must apply: {took:?}"
    );
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
    assert_ne!(
        grill.exit_code(&id).await.unwrap(),
        Some(0),
        "it must have been killed, not have exited"
    );
}

#[tokio::test]
async fn pre_stop_runs_inside_the_workloads_environment_before_the_signal() {
    let dir = tempfile::tempdir().unwrap();
    let (log, ready) = (dir.path().join("log"), dir.path().join("ready"));
    let grill = ProcessGrill::new();
    let id = InstanceId("default__hooked-0".into());
    let policy = StopPolicy {
        signal: StopSignal::Term,
        grace: Some(Duration::from_secs(10)),
        pre_stop: Some(PreStop {
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("echo pre-stop >> {}", log.display()),
            ],
            timeout: Duration::from_secs(5),
        }),
    };
    started(
        &grill,
        &id,
        &workload(&trapping(&log, &ready), policy.clone()),
        &ready,
    )
    .await;

    let stop = InstanceStop::for_policy(Some(&policy), Duration::from_secs(30));
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .unwrap();

    assert_eq!(lines(&log), ["pre-stop", "TERM"]);
}

#[tokio::test]
async fn a_failing_pre_stop_never_prevents_the_stop() {
    for command in [
        vec!["/bin/sh".to_string(), "-c".into(), "exit 3".into()],
        vec!["/nonexistent/pre-stop".to_string()],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (log, ready) = (dir.path().join("log"), dir.path().join("ready"));
        let grill = ProcessGrill::new();
        let id = InstanceId("default__broken-0".into());
        let policy = StopPolicy {
            signal: StopSignal::Quit,
            grace: Some(Duration::from_secs(10)),
            pre_stop: Some(PreStop {
                command: command.clone(),
                timeout: Duration::from_secs(5),
            }),
        };
        started(
            &grill,
            &id,
            &workload(&trapping(&log, &ready), policy.clone()),
            &ready,
        )
        .await;

        let stop = InstanceStop::for_policy(Some(&policy), Duration::from_secs(30));
        stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
            .await
            .unwrap();

        assert_eq!(lines(&log), ["QUIT"], "{command:?}");
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
    }
}

#[tokio::test]
async fn a_pre_stop_that_overruns_costs_only_its_own_bound() {
    let dir = tempfile::tempdir().unwrap();
    let (log, ready) = (dir.path().join("log"), dir.path().join("ready"));
    let grill = ProcessGrill::new();
    let id = InstanceId("default__slow-0".into());
    let bound = Duration::from_secs(1);
    let policy = StopPolicy {
        signal: StopSignal::Term,
        grace: Some(Duration::from_secs(10)),
        pre_stop: Some(PreStop {
            command: vec!["/bin/sleep".into(), "30".into()],
            timeout: bound,
        }),
    };
    started(
        &grill,
        &id,
        &workload(&trapping(&log, &ready), policy.clone()),
        &ready,
    )
    .await;

    let begun = Instant::now();
    let stop = InstanceStop::for_policy(Some(&policy), Duration::from_secs(30));
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .unwrap();
    let took = begun.elapsed();

    assert!(took >= bound, "the pre-stop bound was skipped: {took:?}");
    assert!(
        took < bound + Duration::from_secs(4),
        "an overrunning pre-stop must be cut off at its bound: {took:?}"
    );
    assert_eq!(lines(&log), ["TERM"]);
}

#[test]
fn an_instance_without_a_policy_keeps_the_callers_grace_and_no_pre_stop() {
    let stop = InstanceStop::for_policy(None, Duration::from_secs(30));
    assert_eq!(stop.grace, Duration::from_secs(30));
    assert_eq!(stop.pre_stop, None);
    let signal_only = StopPolicy {
        signal: StopSignal::Int,
        grace: None,
        pre_stop: None,
    };
    let stop = InstanceStop::for_policy(Some(&signal_only), Duration::from_secs(7));
    assert_eq!(stop.grace, Duration::from_secs(7));
}

/// Runtime calls on `id` from `from` onwards, by operation name.
fn operations_on(grill: &MockGrill, id: &InstanceId, from: usize) -> Vec<String> {
    grill.calls()[from..]
        .iter()
        .filter(|(_, instance)| instance == id)
        .map(|(operation, _)| operation.clone())
        .collect()
}

fn hooked_web(image: &str) -> Config {
    Config::parse(&format!(
        r#"
        [app.web]
        image = "{image}"
        port = 8080
        pre_stop = ["/bin/drain"]
        pre_stop_timeout = "2s"
        "#
    ))
    .unwrap()
}

#[tokio::test]
async fn an_operator_stop_runs_the_pre_stop_command_before_the_signal() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, hooked_web("myapp:v1")).await);
    let id = InstanceId("default__web-0".into());
    let from = grill.calls().len();

    agent.stop_app("web", "default").await.unwrap();

    let operations = operations_on(&grill, &id, from);
    let exec = operations.iter().position(|op| op == "exec");
    let stop = operations.iter().position(|op| op == "stop");
    assert!(
        matches!((exec, stop), (Some(exec), Some(stop)) if exec < stop),
        "pre_stop must run before the signal: {operations:?}"
    );
}

#[tokio::test]
async fn an_app_without_pre_stop_is_signalled_without_an_exec() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let from = grill.calls().len();

    agent.stop_app("web", "default").await.unwrap();

    let operations = operations_on(&grill, &id, from);
    assert!(operations.contains(&"stop".to_string()), "{operations:?}");
    assert!(!operations.contains(&"exec".to_string()), "{operations:?}");
}

#[tokio::test]
async fn a_rolling_deploy_runs_the_old_instances_pre_stop_before_retiring_it() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, hooked_web("myapp:v1")).await);
    let old = InstanceId(live_web_ids(&agent)[0].clone());
    let from = grill.calls().len();

    // The new spec drops the hook: the old instance still stops its own way.
    let mut next = basic_config();
    next.app.get_mut("web").unwrap().image = Some("myapp:v2".into());
    expect_complete(&drain_deploy(&mut agent, next).await);

    let operations = operations_on(&grill, &old, from);
    let exec = operations.iter().position(|op| op == "exec");
    let stop = operations.iter().position(|op| op == "stop");
    assert!(
        matches!((exec, stop), (Some(exec), Some(stop)) if exec < stop),
        "the retired instance's pre_stop must run before its signal: {operations:?}"
    );
}
