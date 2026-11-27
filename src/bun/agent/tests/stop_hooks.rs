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
        managed_volumes: Vec::new(),
        host_path_sources: Vec::new(),
        reusable_executor: false,
        host_process: true,
        initialiser: false,
        root: OciRoot {
            path: crate::grill::oci::HOST_ROOT.into(),
            readonly: false,
        },
        process: OciProcess {
            no_new_privileges: false,
            rlimits: Vec::new(),
            args: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: Vec::new(),
            provider_env: Vec::new(),
            cwd: "/".into(),
            user: OciUser { uid: 0, gid: 0 },
            capabilities: None,
            oom_score_adj: None,
            overrides: None,
        },
        mounts: Vec::new(),
        linux: OciLinux {
            seccomp: None,
            masked_paths: Vec::new(),
            readonly_paths: Vec::new(),
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

/// Removing a retired instance's identity directory and record is disk work
/// off the loop. Outside a turn, as here, the stop waits for it for 10 s
/// (`OUTSIDE_TURN_RUNTIME_PATIENCE`), then answers `StillRunning`: ask again
/// later. On a host flushing hard the removal took longer, and the stop
/// tests above failed on that answer (#1901). The tests' own `stop_app`
/// asks again, as the loop's next turn would.
#[tokio::test]
async fn a_whole_stop_waits_out_a_slow_artifact_cleanup() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    // Just past that patience, so the first ask can't collect it.
    agent
        .loop_stalls
        .set(LoopStall::ArtifactCleanup, Duration::from_millis(10_500));
    let from = grill.calls().len();

    agent
        .stop_app("web", "default")
        .await
        .expect("a slow cleanup is waited out, not reported as a failure");

    let operations = operations_on(&grill, &id, from);
    assert!(operations.contains(&"stop".to_string()), "{operations:?}");
    assert!(agent.loop_stalls.reached(LoopStall::ArtifactCleanup) >= 1);
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

// ---------------------------------------------------------------------------
// A runtime too busy to answer (#1898). A process owner answers a state
// query "busy" while another operation holds its lock, which on a host
// flushing hard can last seconds. That isn't an answer about the instance,
// so the stop asks again until its bound runs out.

/// A mock instance, running, that stops when asked.
async fn running_mock(id: &InstanceId) -> MockGrill {
    let grill = MockGrill::new();
    let policy = StopPolicy {
        signal: StopSignal::Term,
        grace: Some(Duration::from_secs(5)),
        pre_stop: None,
    };
    grill.create(id, &workload("true", policy)).await.unwrap();
    grill.start(id).await.unwrap();
    grill
}

#[tokio::test]
async fn a_busy_runtime_is_asked_again_until_it_reports_the_exit() {
    let id = InstanceId("default__busy-0".into());
    let grill = running_mock(&id).await;
    grill.set_state_unavailable_times(&id, 2);
    let from = grill.calls().len();

    let stop = InstanceStop::for_policy(None, Duration::from_secs(5));
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .expect("two busy answers then the exit is a confirmed stop");

    let operations = operations_on(&grill, &id, from);
    assert_eq!(
        operations.iter().filter(|op| *op == "state").count(),
        3,
        "{operations:?}"
    );
    assert!(!operations.contains(&"kill".to_string()), "{operations:?}");
}

#[tokio::test]
async fn a_runtime_busy_for_good_still_ends_the_stop_within_its_bounds() {
    let id = InstanceId("default__busy-1".into());
    let grill = running_mock(&id).await;
    grill.set_state_unavailable_times(&id, u32::MAX);
    let grace = Duration::from_millis(300);
    let confirmation = Duration::from_millis(300);

    let begun = Instant::now();
    let stop = InstanceStop::for_policy(None, grace);
    let error = stop_runtime_instance(&grill, &id, &stop, confirmation)
        .await
        .expect_err("a stop nobody can confirm isn't a stop");
    let took = begun.elapsed();

    assert!(
        matches!(error, BunError::StopUnconfirmed { .. }),
        "{error:?}"
    );
    assert!(
        grill
            .calls()
            .iter()
            .any(|(op, instance)| op == "kill" && instance == &id),
        "the grace ran out, so the instance is killed"
    );
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[tokio::test]
async fn a_runtime_that_cannot_inspect_the_instance_fails_the_stop_at_once() {
    let id = InstanceId("default__broken-0".into());
    let grill = running_mock(&id).await;
    grill.set_fail_state(true);

    let begun = Instant::now();
    let stop = InstanceStop::for_policy(None, Duration::from_secs(5));
    let error = stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .expect_err("an inspection failure isn't a busy owner");

    assert!(matches!(error, BunError::Grill(_)), "{error:?}");
    assert!(begun.elapsed() < Duration::from_secs(1));
}

// ---------------------------------------------------------------------------
// Node shutdown (#2118). Bun stopping takes every instance with it, and each
// stops the way an ordinary stop would: its pre-stop command, its signal,
// then its own grace. All at once, so one slow app costs the others nothing,
// and all within one budget that fits inside the unit's `TimeoutStopSec`.
//
// The deploy runs on the real clock; the shutdown on tokio's paused one, so
// every grace and budget below is exact, however loaded the host. Nothing
// here spawns a task while the clock is paused: `shutdown_all` polls its
// stops together with `join_all`, inside the call.

/// Instances of `config` deployed on a mock runtime whose workloads ignore
/// their stop signal, so only the grace running out (or the kill) ends them.
async fn stubborn_agent(config: Config) -> (TestAgent, MockGrill, Vec<InstanceId>) {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, config).await);
    let ids: Vec<InstanceId> = agent
        .supervisor
        .list_instances()
        .iter()
        .map(|instance| instance.id.clone())
        .collect();
    grill.set_ignore_stop(true);
    for id in &ids {
        grill.set_state(id, ContainerState::Running);
    }
    // From here on, sleeps and timeouts advance virtual time only.
    tokio::time::pause();
    (agent, grill, ids)
}

#[tokio::test]
async fn a_node_shutdown_runs_pre_stop_and_gives_the_app_its_own_grace() {
    let config = Config::parse(
        r#"
        [app.db]
        image = "db:v1"
        port = 5432
        pre_stop = ["/bin/checkpoint"]
        pre_stop_timeout = "2s"
        stop_grace = "2s"
        "#,
    )
    .unwrap();
    let (mut agent, grill, ids) = stubborn_agent(config).await;
    let id = &ids[0];
    let from = grill.calls().len();

    let begun = tokio::time::Instant::now();
    agent.shutdown_all().await;
    let took = begun.elapsed();

    let operations = operations_on(&grill, id, from);
    let exec = operations.iter().position(|op| op == "exec");
    let stop = operations.iter().position(|op| op == "stop");
    let kill = operations.iter().position(|op| op == "kill");
    assert!(
        matches!((exec, stop, kill), (Some(exec), Some(stop), Some(kill)) if exec < stop && stop < kill),
        "pre_stop, then the signal, then the kill once the grace ran out: {operations:?}"
    );
    // The app's own 2 s, not the agent's default 10 s nor the old fixed 5 s.
    // The clock is paused, so only the polling step (100 ms) adds to it.
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_millis(2500),
        "the app's stop_grace of 2s must be the wait: {took:?}"
    );
}

#[tokio::test]
async fn a_node_shutdown_stops_every_instance_at_once() {
    let config = Config::parse(
        r#"
        [app.db]
        image = "db:v1"
        port = 5432
        stop_grace = "2s"

        [app.queue]
        image = "queue:v1"
        port = 5672
        stop_grace = "2s"
        "#,
    )
    .unwrap();
    let (mut agent, grill, ids) = stubborn_agent(config).await;
    assert_eq!(ids.len(), 2, "{ids:?}");

    let begun = tokio::time::Instant::now();
    agent.shutdown_all().await;
    let took = begun.elapsed();

    for id in &ids {
        let operations = operations_on(&grill, id, 0);
        assert!(operations.contains(&"kill".to_string()), "{operations:?}");
    }
    // One after the other would be 4 s: two graces, not one. The clock is
    // paused, so this measures the stop path, not the host's load.
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_millis(2500),
        "the two graces must run side by side: {took:?}"
    );
}

#[tokio::test]
async fn a_node_shutdown_ends_within_its_budget_whatever_the_app_asks_for() {
    let config = Config::parse(
        r#"
        [app.db]
        image = "db:v1"
        port = 5432
        pre_stop = ["/bin/checkpoint"]
        pre_stop_timeout = "1h"
        stop_grace = "1h"
        "#,
    )
    .unwrap();
    let (mut agent, grill, ids) = stubborn_agent(config).await;
    // The pre-stop command never finishes.
    grill.block_execs();
    agent.set_shutdown_budget(Duration::from_secs(1));

    let begun = tokio::time::Instant::now();
    agent.shutdown_all().await;
    let took = begun.elapsed();

    let operations = operations_on(&grill, &ids[0], 0);
    assert!(operations.contains(&"exec".to_string()), "{operations:?}");
    assert!(
        operations.contains(&"kill".to_string()),
        "the budget ran out, so the instance is killed: {operations:?}"
    );
    // The 1 s budget, then the kill round, which answers at once here.
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_millis(1500),
        "{took:?}"
    );
}

/// Every `TimeoutStopSec` in `unit`, in seconds.
fn stop_timeouts(unit: &str) -> Vec<u64> {
    unit.lines()
        .filter_map(|line| line.trim().strip_prefix("TimeoutStopSec="))
        .map(|value| value.parse().expect("TimeoutStopSec in plain seconds"))
        .collect()
}

/// systemd kills Bun at the unit's `TimeoutStopSec`, and with
/// `KillMode=process` its workloads then outlive it unstopped. The shutdown
/// budget, both kill rounds and a few seconds for the rest of Bun's exit
/// must fit inside every unit we ship or document.
#[test]
fn the_shutdown_budget_fits_inside_every_units_stop_timeout() {
    let units = [
        ("quickstart", crate::relish::quickstart::provision::SERVICE),
        (
            "appliance image",
            include_str!(
                "../../../../image/mkosi.extra/usr/lib/systemd/system/reliaburger.service"
            ),
        ),
        (
            "docs/linux-servers.md",
            include_str!("../../../../docs/linux-servers.md"),
        ),
    ];
    let needed = shutdown_completion_bound() + Duration::from_secs(5);
    for (name, unit) in units {
        let timeouts = stop_timeouts(unit);
        assert!(!timeouts.is_empty(), "{name} sets no TimeoutStopSec");
        for timeout in timeouts {
            assert!(
                needed <= Duration::from_secs(timeout),
                "{name}: TimeoutStopSec={timeout} is shorter than the {needed:?} a shutdown may take"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// A runtime that doesn't answer a stop or kill in time (#1902). A process
// owner stalled by a long fsync misses its 2 s request timeout. That says
// nothing about the workload, and a signal sent twice is harmless, so the
// stop asks again: the graceful signal within the grace, the kill within the
// confirmation bound.

#[tokio::test]
async fn a_stop_the_runtime_did_not_answer_is_asked_again_within_the_grace() {
    let id = InstanceId("default__stalled-0".into());
    let grill = running_mock(&id).await;
    grill.set_unanswered_times(crate::grill::mock::MockCall::Stop, &id, 1);
    let from = grill.calls().len();

    let stop = InstanceStop::for_policy(None, Duration::from_secs(5));
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .expect("one unanswered stop request then an answer is a confirmed stop");

    let operations = operations_on(&grill, &id, from);
    assert_eq!(
        operations.iter().filter(|op| *op == "stop").count(),
        2,
        "{operations:?}"
    );
    assert!(!operations.contains(&"kill".to_string()), "{operations:?}");
}

#[tokio::test]
async fn a_stop_never_answered_within_the_grace_falls_through_to_the_kill() {
    let id = InstanceId("default__stalled-1".into());
    let grill = running_mock(&id).await;
    grill.set_unanswered_times(crate::grill::mock::MockCall::Stop, &id, u32::MAX);
    let grace = Duration::from_millis(300);

    let begun = Instant::now();
    let stop = InstanceStop::for_policy(None, grace);
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .expect("the kill still ends the instance");

    assert!(
        grill
            .calls()
            .iter()
            .any(|(op, instance)| op == "kill" && instance == &id),
        "the grace ran out unanswered, so the instance is killed"
    );
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "{:?}",
        begun.elapsed()
    );
}

#[tokio::test]
async fn a_kill_the_runtime_did_not_answer_is_asked_again() {
    let id = InstanceId("default__stalled-2".into());
    let grill = running_mock(&id).await;
    grill.set_ignore_stop(true);
    grill.set_unanswered_times(crate::grill::mock::MockCall::Kill, &id, 1);
    let from = grill.calls().len();

    let stop = InstanceStop::for_policy(None, Duration::from_millis(200));
    stop_runtime_instance(&grill, &id, &stop, CONFIRMATION)
        .await
        .expect("one unanswered kill then an answer is a confirmed stop");

    let operations = operations_on(&grill, &id, from);
    assert_eq!(
        operations.iter().filter(|op| *op == "kill").count(),
        2,
        "{operations:?}"
    );
}

#[tokio::test]
async fn a_kill_never_answered_ends_the_stop_unconfirmed_within_its_bound() {
    let id = InstanceId("default__stalled-3".into());
    let grill = running_mock(&id).await;
    grill.set_ignore_stop(true);
    grill.set_unanswered_times(crate::grill::mock::MockCall::Kill, &id, u32::MAX);
    let confirmation = Duration::from_millis(300);

    let begun = Instant::now();
    let stop = InstanceStop::for_policy(None, Duration::from_millis(200));
    let error = stop_runtime_instance(&grill, &id, &stop, confirmation)
        .await
        .expect_err("a kill nobody answers isn't a stop");

    assert!(
        matches!(error, BunError::StopUnconfirmed { .. }),
        "{error:?}"
    );
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "{:?}",
        begun.elapsed()
    );
}

/// #1902 review: the grace runs from the first ask, not from the answer.
/// A stop answered near the end of its grace waits out only what's left
/// of it before the kill, so the whole stop stays within grace plus
/// confirmation, which `stop_completion_bound` relies on.
#[tokio::test]
async fn a_stop_answered_late_in_the_grace_still_kills_within_grace_and_confirmation() {
    let id = InstanceId("default__stalled-4".into());
    let grill = running_mock(&id).await;
    grill.set_ignore_stop(true);
    // Eight unanswered asks, 100 ms apart: the answer lands about 0.8 s
    // into a 1 s grace.
    grill.set_unanswered_times(crate::grill::mock::MockCall::Stop, &id, 8);
    let grace = Duration::from_secs(1);
    let confirmation = Duration::from_millis(300);

    let begun = Instant::now();
    let stop = InstanceStop::for_policy(None, grace);
    stop_runtime_instance(&grill, &id, &stop, confirmation)
        .await
        .expect("the kill ends the instance");
    let took = begun.elapsed();

    assert!(
        grill
            .calls()
            .iter()
            .any(|(op, instance)| op == "kill" && instance == &id),
        "the workload ignored its signal, so it's killed"
    );
    assert!(took < grace + confirmation, "{took:?}");
}
