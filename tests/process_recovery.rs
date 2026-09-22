//! Durable process runtime contracts, independent of Bun's later PID record.
use std::path::Path;
use std::time::Duration;

use reliaburger::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
use reliaburger::grill::process::ProcessGrill;
use reliaburger::grill::state::ContainerState;
use reliaburger::grill::{Grill, InstanceId};

fn runtime(directory: &Path) -> ProcessGrill {
    ProcessGrill::with_owner(directory.to_path_buf(), env!("CARGO_BIN_EXE_bun").into())
}

fn spec(script: &str) -> OciSpec {
    OciSpec {
        root: OciRoot {
            path: "/".into(),
            readonly: false,
        },
        process: OciProcess {
            args: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: vec!["OWNER_TEST=preserved".into()],
            cwd: "/".into(),
            user: OciUser { uid: 0, gid: 0 },
        },
        mounts: vec![],
        linux: OciLinux {
            namespaces: vec![],
            resources: None,
            cgroups_path: None,
            uid_mappings: None,
            gid_mappings: None,
        },
        port_mapping: None,
    }
}

async fn stopped(grill: &ProcessGrill, id: &InstanceId) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if matches!(grill.state(id).await, Ok(ContainerState::Stopped)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn runtime_generation_survives_recovery_without_disclosing_control_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let id = InstanceId("default__generation-0".into());
    let first = runtime(directory.path());
    first.create(&id, &spec("exit 0")).await.unwrap();
    let original = first
        .launch_inventory()
        .await
        .unwrap()
        .unwrap()
        .remove(0)
        .generation;
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            directory
                .path()
                .join("process-owners")
                .join(&id.0)
                .join("owner.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let secret = record["nonce"].as_str().unwrap();
    assert_ne!(original.as_str(), secret);
    assert!(!format!("{original:?}").contains(secret));
    drop(first);
    let recovered = runtime(directory.path());
    assert_eq!(
        recovered.launch_inventory().await.unwrap().unwrap()[0].generation,
        original
    );
    recovered.kill(&id).await.unwrap();
    recovered.create(&id, &spec("exit 0")).await.unwrap();
    let replacement = recovered
        .launch_inventory()
        .await
        .unwrap()
        .unwrap()
        .remove(0)
        .generation;
    assert_ne!(
        replacement, original,
        "recreating the same instance must not reuse its generation"
    );
    assert_eq!(
        runtime(directory.path())
            .launch_inventory()
            .await
            .unwrap()
            .unwrap()[0]
            .generation,
        replacement
    );
    recovered.kill(&id).await.unwrap();
}

#[tokio::test]
async fn recovered_runtime_reads_short_job_outcome_without_pid_record() {
    let directory = tempfile::tempdir().unwrap();
    let id = InstanceId("default__short-0".into());
    let first = runtime(directory.path());
    first
        .create(&id, &spec("echo $OWNER_TEST; exit 23"))
        .await
        .unwrap();
    first.start(&id).await.unwrap();
    stopped(&first, &id).await;
    drop(first);
    let recovered = runtime(directory.path());
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Stopped);
    assert_eq!(recovered.exit_code(&id).await, Some(23));
    assert_eq!(recovered.logs(&id).await.unwrap().trim(), "preserved");
}

#[tokio::test]
async fn recovered_runtime_controls_live_generation_without_adoption() {
    let directory = tempfile::tempdir().unwrap();
    let id = InstanceId("default__live-0".into());
    let first = runtime(directory.path());
    first.create(&id, &spec("sleep 30")).await.unwrap();
    first.start(&id).await.unwrap();
    let pid = first.pid(&id).await.unwrap();
    drop(first);
    let recovered = runtime(directory.path());
    let result = recovered.state(&id).await;
    recovered.kill(&id).await.unwrap();
    stopped(&recovered, &id).await;
    assert!(reliaburger::grill::records::process_start_time(pid).is_none());
    assert_eq!(result.unwrap(), ContainerState::Running);
}

#[tokio::test]
async fn recovered_preparation_can_be_cancelled_before_execution() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ran");
    let id = InstanceId("default__prepared-0".into());
    let first = runtime(directory.path());
    first
        .create(&id, &spec(&format!("touch '{}'", marker.display())))
        .await
        .unwrap();
    drop(first);
    let recovered = runtime(directory.path());
    recovered.kill(&id).await.unwrap();
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Stopped);
    assert!(recovered.start(&id).await.is_err());
    assert!(!marker.exists());
}

#[tokio::test]
async fn long_data_paths_and_repeated_generations_preserve_control() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("a".repeat(100)).join("b".repeat(100));
    let grill = runtime(&directory);
    let id = InstanceId("default__repeat-0".into());
    for code in 1..=4 {
        grill
            .create(&id, &spec(&format!("exit {code}")))
            .await
            .unwrap();
        grill.start(&id).await.unwrap();
        stopped(&grill, &id).await;
        assert_eq!(grill.exit_code(&id).await, Some(code));
    }
}

#[tokio::test]
async fn delayed_helper_cannot_start_a_replacement_generation() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ran");
    let id = InstanceId("default__fenced-0".into());
    let grill = runtime(directory.path());
    grill.create(&id, &spec("exit 0")).await.unwrap();
    let owner_directory = directory.path().join("process-owners").join(&id.0);
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(owner_directory.join("owner.json")).unwrap())
            .unwrap();
    let nonce = record["nonce"].as_str().unwrap();
    grill.kill(&id).await.unwrap();
    grill
        .create(&id, &spec(&format!("touch '{}'", marker.display())))
        .await
        .unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bun"))
        .args(["__process-owner", "--directory"])
        .arg(&owner_directory)
        .arg("--generation")
        .arg(nonce)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("generation mismatch"));
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Pending);
    assert!(!marker.exists());
    grill.kill(&id).await.unwrap();
}

#[tokio::test]
async fn owner_loss_retires_only_after_the_workload_process_group_is_gone() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("owner-killed");
    let release = directory.path().join("release");
    let done = directory.path().join("done");
    let id = InstanceId("default__uncertain-0".into());
    let grill = runtime(directory.path());
    // The workload kills its own owner to inject failure without recovering a
    // PID in the test. It exits on release or after a bounded fallback timeout.
    let script = format!(
        "echo diagnostic-output; kill -KILL \"$PPID\"; touch '{}'; n=0; while [ ! -f '{}' ] && [ $n -lt 200 ]; do sleep 0.05; n=$((n+1)); done; touch '{}'",
        marker.display(),
        release.display(),
        done.display()
    );
    grill.create(&id, &spec(&script)).await.unwrap();
    grill.start(&id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let recovered = runtime(directory.path());
    let status = recovered.state(&id).await;
    let kill = recovered.kill(&id).await;
    let diagnostic_logs = recovered.logs(&id).await;
    std::fs::write(release, "release").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !done.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        status.is_err(),
        "a live workload without its owner is not absent"
    );
    assert!(kill.is_err(), "cannot signal a PID recovered from disk");
    // Once nothing in the workload's process group remains, the dead owner's
    // generation retires with an unknown exit code instead of wedging.
    stopped(&recovered, &id).await;
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            directory
                .path()
                .join("process-owners")
                .join(&id.0)
                .join("owner.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["phase"]["state"], "retired", "{record}");
    assert!(record["phase"]["exit_code"].is_null(), "{record}");
    let socket_directory = std::path::PathBuf::from(format!(
        "/tmp/rbp-{}-{}",
        nix::unistd::geteuid(),
        record["nonce"].as_str().unwrap()
    ));
    assert!(
        !socket_directory.exists(),
        "retirement left the control socket"
    );
    assert!(diagnostic_logs.unwrap().contains("diagnostic-output"));
}

#[tokio::test]
async fn corrupt_intent_refuses_recovery_and_duplicate_launch() {
    let directory = tempfile::tempdir().unwrap();
    let id = InstanceId("default__corrupt-0".into());
    let grill = runtime(directory.path());
    grill.create(&id, &spec("exit 0")).await.unwrap();
    let path = directory
        .path()
        .join("process-owners")
        .join(&id.0)
        .join("owner.json");
    std::fs::write(path, "{").unwrap();
    let recovered = runtime(directory.path());
    assert!(recovered.state(&id).await.is_err());
    assert!(recovered.kill(&id).await.is_err());
    assert!(recovered.create(&id, &spec("exit 0")).await.is_err());
}

#[tokio::test]
async fn cancellation_of_start_preserves_the_owned_launch_transaction() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let launched = directory.path().join("helper-started");
    let wrapper = directory.path().join("delayed-bun");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\ntouch '{}'\nsleep 0.2\nexec '{}' \"$@\"\n",
            launched.display(),
            env!("CARGO_BIN_EXE_bun")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let grill = ProcessGrill::with_owner(directory.path().to_path_buf(), wrapper);
    let id = InstanceId("default__cancelled-caller-0".into());
    grill.create(&id, &spec("sleep 30")).await.unwrap();
    let starting = tokio::spawn({
        let grill = grill.clone();
        let id = id.clone();
        async move { grill.start(&id).await }
    });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !launched.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    starting.abort();
    assert!(starting.await.unwrap_err().is_cancelled());
    let recovered = runtime(directory.path());
    tokio::time::timeout(Duration::from_secs(15), async {
        while !matches!(recovered.state(&id).await, Ok(ContainerState::Running)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    recovered.kill(&id).await.unwrap();
    stopped(&recovered, &id).await;
}

#[tokio::test]
async fn recovery_finishes_socket_cleanup_only_after_durable_absence_proof() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let release = directory.path().join("release");
    let id = InstanceId("default__retiring-0".into());
    let grill = runtime(directory.path());
    grill.create(&id, &spec(&format!(
        "n=0; while [ ! -f '{}' ] && [ $n -lt 200 ]; do sleep 0.05; n=$((n+1)); done; exit 17", release.display()
    ))).await.unwrap();
    grill.start(&id).await.unwrap();
    let record_path = directory
        .path()
        .join("process-owners")
        .join(&id.0)
        .join("owner.json");
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    let socket_directory = std::path::PathBuf::from(format!(
        "/tmp/rbp-{}-{}",
        nix::unistd::geteuid(),
        record["nonce"].as_str().unwrap()
    ));
    // Fail the owner's private-directory validation after execution. This
    // preserves Retiring proof but prevents the final cleanup acknowledgement.
    std::fs::set_permissions(&socket_directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    std::fs::write(release, "release").unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let record: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
            if record["phase"]["state"] == "retiring" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let recovered = runtime(directory.path());
    assert!(recovered.state(&id).await.is_err());
    std::fs::set_permissions(&socket_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Stopped);
    assert_eq!(recovered.exit_code(&id).await, Some(17));
    assert!(!socket_directory.exists());
}

#[tokio::test]
async fn failed_first_preparation_never_publishes_an_incomplete_instance() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let id = InstanceId("default__oversized-0".into());
    assert!(
        grill
            .create(&id, &spec(&"x".repeat(1024 * 1024)))
            .await
            .is_err()
    );
    assert!(
        !directory.path().join("process-owners").join(&id.0).exists(),
        "failed preparation published an instance without durable intent"
    );
    grill.create(&id, &spec("exit 0")).await.unwrap();
    grill.kill(&id).await.unwrap();
}

#[tokio::test]
async fn owner_reaping_survives_parent_exec() {
    use std::os::unix::process::CommandExt;
    const PHASE: &str = "RELIABURGER_OWNER_REEXEC_PHASE";
    const DIRECTORY: &str = "RELIABURGER_OWNER_REEXEC_DIRECTORY";
    let Some(phase) = std::env::var_os(PHASE) else {
        let directory = tempfile::tempdir().unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(45),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "owner_reaping_survives_parent_exec",
                    "--nocapture",
                ])
                .env(PHASE, "start")
                .env(DIRECTORY, directory.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "child fixture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let directory = std::path::PathBuf::from(std::env::var_os(DIRECTORY).unwrap());
    let grill = runtime(&directory);
    let id = InstanceId("default__reexec-0".into());
    if phase == "start" {
        grill.create(&id, &spec("sleep 30")).await.unwrap();
        grill.start(&id).await.unwrap();
        let error = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "owner_reaping_survives_parent_exec",
                "--nocapture",
            ])
            .env(PHASE, "recovered")
            .exec();
        panic!("fixture exec failed: {error}");
    }
    assert_eq!(phase, "recovered");
    grill.kill(&id).await.unwrap();
    stopped(&grill, &id).await;
    // The exec discarded the original Tokio child waiter. A durable owner
    // must not become an unreaped child of the replacement runtime.
    assert_eq!(
        nix::sys::wait::waitpid(
            nix::unistd::Pid::from_raw(-1),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG)
        ),
        Err(nix::errno::Errno::ECHILD),
        "replacement runtime inherited an unreaped helper"
    );
}

#[tokio::test]
async fn inventory_finds_prepared_live_and_completed_unrecorded_launches() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    assert!(grill.launch_inventory().await.unwrap().unwrap().is_empty());
    for name in ["prepared", "live", "completed"] {
        let id = InstanceId(format!("default__{name}-0"));
        grill
            .create(
                &id,
                &spec(if name == "live" {
                    "sleep 30"
                } else {
                    "exit 19"
                }),
            )
            .await
            .unwrap();
        if name != "prepared" {
            grill.start(&id).await.unwrap();
        }
        if name == "completed" {
            stopped(&grill, &id).await;
        }
    }
    let result = runtime(directory.path()).launch_inventory().await;
    grill
        .kill(&InstanceId("default__live-0".into()))
        .await
        .unwrap();
    stopped(&grill, &InstanceId("default__live-0".into())).await;
    let inventory = result.unwrap().unwrap();
    assert_eq!(
        inventory
            .iter()
            .map(|entry| entry.instance_id.0.as_str())
            .collect::<Vec<_>>(),
        [
            "default__completed-0",
            "default__live-0",
            "default__prepared-0"
        ]
    );
    assert_eq!(inventory[1].spec, spec("sleep 30"));
}

#[tokio::test]
async fn inventory_refuses_damaged_or_unexpected_published_entries() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let id = InstanceId("default__valid-0".into());
    grill.create(&id, &spec("exit 0")).await.unwrap();
    let root = directory.path().join("process-owners");
    // A staged intent cannot execute and is not a published generation.
    std::fs::create_dir(root.join(".preparing-abandoned")).unwrap();
    assert_eq!(grill.launch_inventory().await.unwrap().unwrap().len(), 1);
    std::fs::write(root.join("unexpected"), b"bad").unwrap();
    assert!(grill.launch_inventory().await.is_err());
    std::fs::remove_file(root.join("unexpected")).unwrap();
    std::fs::write(root.join(&id.0).join("owner.json"), b"bad").unwrap();
    assert!(grill.launch_inventory().await.is_err());
}

fn recovery_agent(directory: &Path) -> reliaburger::bun::agent::BunAgent<ProcessGrill> {
    let (_, receiver) = tokio::sync::mpsc::channel(8);
    let mut agent = reliaburger::bun::agent::BunAgent::new(
        runtime(directory),
        reliaburger::grill::PortAllocator::new(30000, 30100),
        receiver,
        tokio_util::sync::CancellationToken::new(),
    );
    agent.set_records_dir(directory.to_path_buf());
    agent.set_volumes_dir(directory.join("volumes"));
    agent
}

#[tokio::test]
async fn agent_retires_launches_that_have_no_adoption_record() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let live = InstanceId("default__orphan-0".into());
    let prepared = InstanceId("default__orphan-init-0".into());
    for id in [&live, &prepared] {
        grill.create(id, &spec("sleep 30")).await.unwrap();
    }
    grill.start(&live).await.unwrap();
    let result = recovery_agent(directory.path())
        .adopt_recorded_instances()
        .await;
    let live_state = grill.state(&live).await;
    let prepared_state = grill.state(&prepared).await;
    grill.kill(&live).await.unwrap();
    stopped(&grill, &live).await;
    assert_eq!(result.unwrap(), 0);
    assert_eq!(live_state.unwrap(), ContainerState::Stopped);
    assert_eq!(prepared_state.unwrap(), ContainerState::Stopped);
}

fn write_job_checkpoint(directory: &Path, phase: &str) {
    let config =
        reliaburger::config::Config::parse("[job.work]\nimage = 'proc-grill:image-ignored'\n")
            .unwrap();
    let job = serde_json::json!({
        "name":"work", "namespace":"default", "spec": config.job["work"],
        "runtime":"Process", "generation":1, "restart_count":1,
        "phase":phase, "runtime_absent":false,
    });
    std::fs::write(
        directory.join("job-attempts.checkpoint"),
        serde_json::to_vec(&serde_json::json!({"schema":2, "jobs":[job]})).unwrap(),
    )
    .unwrap();
}

fn recovered_job(directory: &Path) -> serde_json::Value {
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("job-attempts.checkpoint")).unwrap())
            .unwrap();
    value["jobs"][0].clone()
}

#[tokio::test]
async fn agent_recovers_short_job_exit_without_an_adoption_record() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let id = InstanceId("default__work-0".into());
    grill.create(&id, &spec("exit 23")).await.unwrap();
    write_job_checkpoint(directory.path(), "Launching");
    grill.start(&id).await.unwrap();
    stopped(&grill, &id).await;
    recovery_agent(directory.path())
        .adopt_recorded_instances()
        .await
        .unwrap();
    let job = recovered_job(directory.path());
    assert_eq!(job["phase"], serde_json::json!({"Exited":{"code":23}}));
    assert_eq!(job["runtime_absent"], true);
    assert_eq!(job["restart_count"], 1);
}

#[tokio::test]
async fn preparing_retry_never_inherits_previous_generations_exit_code() {
    for previous_launch in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let grill = runtime(directory.path());
        let id = InstanceId("default__work-0".into());
        if previous_launch {
            grill.create(&id, &spec("exit 23")).await.unwrap();
            grill.start(&id).await.unwrap();
            stopped(&grill, &id).await;
        }
        write_job_checkpoint(directory.path(), "Preparing");
        recovery_agent(directory.path())
            .adopt_recorded_instances()
            .await
            .unwrap();
        let job = recovered_job(directory.path());
        assert_eq!(job["phase"], "Unknown");
        assert_eq!(job["runtime_absent"], true);
        assert_eq!(job["restart_count"], 1);
    }
}

#[tokio::test]
async fn authorised_job_without_runtime_intent_refuses_recovery() {
    let directory = tempfile::tempdir().unwrap();
    write_job_checkpoint(directory.path(), "Launching");
    assert!(
        recovery_agent(directory.path())
            .adopt_recorded_instances()
            .await
            .is_err()
    );
    assert_eq!(recovered_job(directory.path())["phase"], "Launching");
}

#[tokio::test]
async fn agent_preflights_all_launch_intents_before_retiring_any() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let live = InstanceId("default__live-0".into());
    grill.create(&live, &spec("sleep 30")).await.unwrap();
    grill.start(&live).await.unwrap();
    let bad = InstanceId("default__broken-0".into());
    grill.create(&bad, &spec("exit 0")).await.unwrap();
    std::fs::write(
        directory
            .path()
            .join("process-owners")
            .join(&bad.0)
            .join("owner.json"),
        b"bad",
    )
    .unwrap();
    let result = recovery_agent(directory.path())
        .adopt_recorded_instances()
        .await;
    let state = grill.state(&live).await;
    grill.kill(&live).await.unwrap();
    stopped(&grill, &live).await;
    assert!(result.is_err());
    assert_eq!(state.unwrap(), ContainerState::Running);
}

#[derive(Debug, Clone, Copy)]
enum QueuedMutation {
    Start,
    Stop,
    Kill,
    Create,
}

async fn cancelled_queued_mutation_preserves_successor(mutation: QueuedMutation) {
    use reliaburger::grill::process_owner;
    use ring::rand::SecureRandom;
    use std::future::Future;
    use std::task::Poll;

    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let id = InstanceId("default__queued-0".into());
    let marker = directory.path().join("unexpected-execution");
    grill
        .create(&id, &spec(&format!("touch '{}'", marker.display())))
        .await
        .unwrap();
    if matches!(mutation, QueuedMutation::Create) {
        grill.kill(&id).await.unwrap();
    }
    let owner_directory = directory.path().join("process-owners").join(&id.0);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(owner_directory.join("client.lock"))
        .unwrap();
    lock.lock().unwrap();
    let stale_spec = spec("exit 42");
    let mut operation: std::pin::Pin<
        Box<dyn Future<Output = Result<(), reliaburger::grill::GrillError>> + Send + '_>,
    > = match mutation {
        QueuedMutation::Start => Box::pin(grill.start(&id)),
        QueuedMutation::Stop => Box::pin(grill.stop(&id)),
        QueuedMutation::Kill => Box::pin(grill.kill(&id)),
        QueuedMutation::Create => Box::pin(grill.create(&id, &stale_spec)),
    };
    // Poll exactly once while the operation lock forbids mutation. Dropping
    // the caller leaves any already queued blocking mutation alive.
    std::future::poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    drop(operation);
    let read_record = || -> process_owner::OwnerRecord {
        serde_json::from_slice(&std::fs::read(owner_directory.join("owner.json")).unwrap()).unwrap()
    };
    let persist_record = |record: &process_owner::OwnerRecord| {
        use std::os::unix::fs::PermissionsExt;
        let mut temporary = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o600))
            .tempfile_in(&owner_directory)
            .unwrap();
        serde_json::to_writer(temporary.as_file_mut(), record).unwrap();
        temporary.as_file().sync_all().unwrap();
        temporary
            .persist(owner_directory.join("owner.json"))
            .unwrap();
        std::fs::File::open(&owner_directory)
            .unwrap()
            .sync_all()
            .unwrap();
    };
    // Model the exclusive lock holder committing cancellation and a successor
    // before the cancelled caller's queued mutation acquires that same lock.
    let mut record = read_record();
    record.phase = process_owner::OwnerPhase::Cancelled;
    persist_record(&record);
    let mut nonce = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut nonce).unwrap();
    record.nonce = hex::encode(nonce);
    record.phase = if matches!(mutation, QueuedMutation::Create) {
        process_owner::OwnerPhase::Retired {
            exit_code: Some(37),
        }
    } else {
        process_owner::OwnerPhase::Prepared
    };
    persist_record(&record);
    let expected = serde_json::to_value(&record).unwrap();
    drop(lock);
    // Cover the owner's bounded startup interval too, including cold debug
    // executable loading. The broken implementation changes the record first.
    let changed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if serde_json::to_value(read_record()).unwrap() != expected || marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    grill.kill(&id).await.unwrap();
    stopped(&grill, &id).await;
    assert!(
        !changed,
        "cancelled {mutation:?} changed a successor generation"
    );
}

#[tokio::test]
async fn cancelled_queued_start_cannot_activate_a_successor_generation() {
    cancelled_queued_mutation_preserves_successor(QueuedMutation::Start).await;
}

#[tokio::test]
async fn cancelled_queued_stop_cannot_cancel_a_successor_generation() {
    cancelled_queued_mutation_preserves_successor(QueuedMutation::Stop).await;
}

#[tokio::test]
async fn cancelled_queued_kill_cannot_cancel_a_successor_generation() {
    cancelled_queued_mutation_preserves_successor(QueuedMutation::Kill).await;
}

#[tokio::test]
async fn cancelled_queued_create_cannot_overwrite_a_successor_outcome() {
    cancelled_queued_mutation_preserves_successor(QueuedMutation::Create).await;
}

async fn read_exec_pid(path: &Path) -> u32 {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(value) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = value.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

async fn exec_process_is_gone(pid: u32) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        while reliaburger::grill::records::process_start_time(pid).is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

async fn interrupted_exec_is_retired(cancel_caller: bool) {
    let directory = tempfile::tempdir().unwrap();
    let grill = std::sync::Arc::new(runtime(directory.path()));
    let id = InstanceId("default__exec-0".into());
    grill.create(&id, &spec("sleep 60")).await.unwrap();
    grill.start(&id).await.unwrap();
    let marker = directory.path().join("exec-pid");
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("echo $$ > '{}'; exec sleep 60", marker.display()),
    ];
    let task = {
        let grill = grill.clone();
        let id = id.clone();
        tokio::spawn(async move { grill.exec(&id, &command).await })
    };
    let pid = read_exec_pid(&marker).await;
    if cancel_caller {
        task.abort();
    } else {
        grill.kill(&id).await.unwrap();
    }
    let gone = exec_process_is_gone(pid).await;
    if !gone {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    task.abort();
    let _ = task.await;
    if cancel_caller {
        grill.kill(&id).await.unwrap();
    }
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(grill.state(&id).await, Ok(ContainerState::Stopped)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if result.is_err() {
        let owner_dir = directory.path().join("process-owners").join(&id.0);
        for entry in std::fs::read_dir(&owner_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                eprintln!(
                    "aux {:?}: {:?}, log {:?}",
                    path,
                    std::fs::read_to_string(path.join("owner.json")),
                    std::fs::read_to_string(path.join("owner.log"))
                );
            } else {
                eprintln!("file {:?}: {:?}", path, std::fs::read_to_string(&path));
            }
        }
    }
    assert!(result.is_ok(), "parent did not retire");
    assert!(
        gone,
        "exec subprocess survived cancellation or confirmed workload retirement"
    );
}

#[tokio::test]
async fn cancelled_exec_retires_its_process() {
    interrupted_exec_is_retired(true).await;
}

#[tokio::test]
async fn workload_retirement_waits_for_exec_processes() {
    interrupted_exec_is_retired(false).await;
}

#[tokio::test]
async fn exec_returns_output_only_after_retiring_surviving_children() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let id = InstanceId("default__exec-output-0".into());
    grill.create(&id, &spec("sleep 60")).await.unwrap();
    grill.start(&id).await.unwrap();
    let marker = directory.path().join("child-pid");
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "sleep 60 & echo $! > '{}'; printf stdout; printf stderr >&2; exit 7",
            marker.display()
        ),
    ];
    let output = grill.exec(&id, &command).await;
    let pid = read_exec_pid(&marker).await;
    let gone = exec_process_is_gone(pid).await;
    grill.kill(&id).await.unwrap();
    stopped(&grill, &id).await;
    assert_eq!(output.unwrap(), "stdout\nstderr");
    assert!(gone, "exec returned while its descendant remained");
}

#[tokio::test]
async fn exec_rejects_empty_commands_and_excessive_output() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let id = InstanceId("default__exec-limits-0".into());
    grill.create(&id, &spec("sleep 60")).await.unwrap();
    grill.start(&id).await.unwrap();
    let empty = grill.exec(&id, &[]).await;
    let oversized_request = grill
        .exec(&id, &["/bin/echo".into(), "x".repeat(64 * 1024)])
        .await;
    let long_request = grill
        .exec(&id, &["/bin/echo".into(), "x".repeat(2048)])
        .await;
    let oversized = grill
        .exec(
            &id,
            &[
                "/bin/sh".into(),
                "-c".into(),
                "head -c 1048577 /dev/zero".into(),
            ],
        )
        .await;
    let following = grill
        .exec(&id, &["/bin/echo".into(), "still-running".into()])
        .await;
    grill.kill(&id).await.unwrap();
    stopped(&grill, &id).await;
    assert!(empty.unwrap_err().to_string().contains("no exec command"));
    assert!(
        oversized_request
            .unwrap_err()
            .to_string()
            .contains("request exceeds")
    );
    assert_eq!(long_request.unwrap(), format!("{}\n", "x".repeat(2048)));
    assert!(oversized.unwrap_err().to_string().contains("1 MiB"));
    assert_eq!(following.unwrap(), "still-running\n");
}

#[tokio::test]
async fn killed_exec_caller_closes_ownership_without_stopping_application() {
    const DIRECTORY: &str = "RELIABURGER_EXEC_CALLER_DIRECTORY";
    let id = InstanceId("default__exec-crash-0".into());
    if let Some(path) = std::env::var_os(DIRECTORY) {
        let directory = std::path::PathBuf::from(path);
        let grill = runtime(&directory);
        let command = vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "echo $$ > '{}'; exec sleep 60",
                directory.join("exec-pid").display()
            ),
        ];
        let _ = grill.exec(&id, &command).await;
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    grill.create(&id, &spec("sleep 60")).await.unwrap();
    grill.start(&id).await.unwrap();
    let mut caller = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "killed_exec_caller_closes_ownership_without_stopping_application",
            "--nocapture",
        ])
        .env(DIRECTORY, directory.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = read_exec_pid(&directory.path().join("exec-pid")).await;
    caller.kill().await.unwrap();
    let gone = exec_process_is_gone(pid).await;
    let main_state = grill.state(&id).await;
    grill.kill(&id).await.unwrap();
    stopped(&grill, &id).await;
    assert!(gone, "exec escaped actual caller SIGKILL");
    assert_eq!(main_state.unwrap(), ContainerState::Running);
}

#[tokio::test]
async fn lost_exec_owner_keeps_application_retirement_unconfirmed() {
    let directory = tempfile::tempdir().unwrap();
    let grill = runtime(directory.path());
    let id = InstanceId("default__exec-owner-loss-0".into());
    let release = directory.path().join("release");
    let owner_pid = directory.path().join("owner-pid");
    grill
        .create(
            &id,
            &spec(&format!(
                "echo $PPID > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done",
                owner_pid.display(),
                release.display()
            )),
        )
        .await
        .unwrap();
    grill.start(&id).await.unwrap();
    let parent_owner = read_exec_pid(&owner_pid).await;
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        grill.exec(
            &id,
            &[
                "/bin/sh".into(),
                "-c".into(),
                "kill -KILL \"$PPID\"; exit 0".into(),
            ],
        ),
    )
    .await
    .unwrap();
    std::fs::write(&release, "exit").unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let state = grill.state(&id).await;
    let parent_dir = directory.path().join("process-owners").join(&id.0);
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(parent_dir.join("owner.json")).unwrap()).unwrap();
    // Fault-injection cleanup: this fixture's retained helper deliberately
    // cannot certify absence after its auxiliary owner was killed.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(parent_owner as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    for path in std::iter::once(parent_dir.clone()).chain(
        std::fs::read_dir(&parent_dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir()),
    ) {
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("owner.json")).unwrap()).unwrap();
        let socket_directory = std::path::PathBuf::from(format!(
            "/tmp/rbp-{}-{}",
            nix::unistd::geteuid(),
            record["nonce"].as_str().unwrap()
        ));
        let _ = std::fs::remove_file(socket_directory.join("control.sock"));
        let _ = std::fs::remove_dir(socket_directory);
    }
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("without retirement proof")
    );
    assert!(!matches!(state, Ok(ContainerState::Stopped)));
    assert_eq!(record["phase"]["state"], "running");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn exec_owner_can_launch_after_its_binary_has_been_unlinked() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("bun-old");
    tokio::fs::copy(env!("CARGO_BIN_EXE_bun"), &executable)
        .await
        .unwrap();
    let grill = ProcessGrill::with_owner(directory.path().join("logs"), executable.clone());
    let id = InstanceId("default__exec-old-binary-0".into());
    grill.create(&id, &spec("sleep 60")).await.unwrap();
    grill.start(&id).await.unwrap();
    tokio::fs::remove_file(executable).await.unwrap();
    let result = grill
        .exec(&id, &["/bin/echo".into(), "mapped-image".into()])
        .await;
    grill.kill(&id).await.unwrap();
    stopped(&grill, &id).await;
    assert_eq!(result.unwrap(), "mapped-image\n");
}
