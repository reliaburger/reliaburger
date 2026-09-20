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
            if grill.state(id).await.unwrap() == ContainerState::Stopped {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
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
async fn owner_loss_preserves_uncertainty_and_never_signals_recorded_pid() {
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
    assert!(status.is_err(), "missing owner must not imply absence");
    assert!(kill.is_err(), "cannot signal a PID recovered from disk");
    assert!(
        recovered.state(&id).await.is_err(),
        "natural exit is still unobserved by the owner"
    );
    // The test has positively observed its workload finish; remove only this
    // fixture's abandoned control socket so it does not litter the host.
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
    let socket_directory = std::path::PathBuf::from(format!(
        "/tmp/rbp-{}-{}",
        nix::unistd::geteuid(),
        record["nonce"].as_str().unwrap()
    ));
    std::fs::remove_file(socket_directory.join("control.sock")).unwrap();
    std::fs::remove_dir(socket_directory).unwrap();
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
        while recovered.state(&id).await.unwrap() != ContainerState::Running {
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
