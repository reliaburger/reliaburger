//! Actual OCI lifecycle recovery without an agent adoption record.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use reliaburger::grill::runc::RuncGrill;
use reliaburger::grill::{ContainerState, Grill, ImageStore, InstanceId, OciSpec};

fn runtime(root: &Path) -> RuncGrill {
    RuncGrill::new(
        root.join("bundles"),
        ImageStore::new(root.join("images")),
        false,
        root.join("state"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap()
}

fn instance(root: &Path) -> InstanceId {
    InstanceId(format!(
        "rbtest-owned-runc-{}",
        root.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches('.')
    ))
}

fn spec(root: &Path, script: &str) -> OciSpec {
    let mut spec: OciSpec = serde_json::from_value(serde_json::json!({
        "root": {"path": "/empty-fixture", "readonly": true},
        "process": {"args": ["/bin/busybox", "sh", "-c", script], "env": ["PATH=/bin"], "cwd": "/", "user": {"uid": 0, "gid": 0}},
        "mounts": [], "linux": {"namespaces": []}
    })).unwrap();
    spec.mounts = reliaburger::grill::oci::standard_mounts();
    spec.linux.namespaces = reliaburger::grill::oci::standard_namespaces(None);
    std::fs::create_dir_all(root.join("shared")).unwrap();
    spec.mounts.push(reliaburger::grill::oci::OciMount {
        destination: "/work".into(),
        source: Some(root.join("shared")),
        mount_type: Some("bind".into()),
        options: vec!["bind".into(), "rw".into()],
    });
    spec
}

fn install_fixture(root: &Path, id: &InstanceId) {
    let rootfs = root.join("bundles").join(&id.0).join("rootfs");
    std::fs::create_dir_all(rootfs.join("bin")).unwrap();
    std::fs::create_dir_all(rootfs.join("work")).unwrap();
    std::fs::copy("/usr/bin/busybox", rootfs.join("bin/busybox")).unwrap();
}

async fn wait_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn assert_absent(root: &Path, id: &InstanceId) {
    assert!(!root.join("state").join(&id.0).exists());
    assert!(!reliaburger::grill::netns::namespace_path(id).exists());
    assert!(
        !Path::new("/sys/class/net")
            .join(reliaburger::grill::netns::host_veth_name(id))
            .exists()
    );
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn runc_owned_preparation_recovers_original_intent_and_retires_without_adoption() {
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let original = spec(root.path(), "exit 0");
    let first = runtime(root.path());
    first.create(&id, &original).await.unwrap();
    let generation = first
        .launch_inventory()
        .await
        .unwrap()
        .unwrap()
        .remove(0)
        .generation;
    let path = root.path().join("bundles").join(&id.0).join("config.json");
    let prepared = std::fs::read(&path).unwrap();
    assert!(
        first
            .create(&id, &spec(root.path(), "exit 9"))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(path).unwrap(), prepared);
    drop(first);
    let recovered = runtime(root.path());
    let inventory = recovered.launch_inventory().await.unwrap().unwrap();
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].spec, original);
    assert_eq!(inventory[0].generation, generation);
    assert_eq!(generation.as_str().len(), 64);
    recovered.kill(&id).await.unwrap();
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Stopped);
    assert_absent(root.path(), &id);
    recovered.create(&id, &original).await.unwrap();
    assert_ne!(
        recovered.launch_inventory().await.unwrap().unwrap()[0].generation,
        generation
    );
    recovered.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn runc_owned_short_job_keeps_its_actual_exit_and_logs_after_reconstruction() {
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let first = runtime(root.path());
    first
        .create(&id, &spec(root.path(), "printf short-job; exit 7"))
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    first.start(&id).await.unwrap();
    drop(first);
    let recovered = runtime(root.path());
    tokio::time::timeout(Duration::from_secs(20), async {
        while recovered.state(&id).await.unwrap() != ContainerState::Stopped {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(recovered.exit_code(&id).await, Some(7));
    assert!(recovered.logs(&id).await.unwrap().contains("short-job"));
    assert_absent(root.path(), &id);
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn runc_owned_launcher_and_exec_retire_after_actual_caller_sigkill() {
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let mut caller = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "owned_runc_fixture", "--ignored", "--nocapture"])
        .env("RELIABURGER_OWNED_RUNC_FIXTURE", root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_file(&root.path().join("shared/exec-ready")).await;
    caller.kill().await.unwrap();
    caller.wait().await.unwrap();
    let recovered = runtime(root.path());
    assert_eq!(
        recovered.launch_inventory().await.unwrap().unwrap().len(),
        1
    );
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Running);
    recovered.kill(&id).await.unwrap();
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Stopped);
    std::fs::write(root.path().join("shared/release"), "continue").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!root.path().join("shared/late-exec").exists());
    assert_absent(root.path(), &id);
}

#[tokio::test]
#[ignore = "subprocess fixture for owned Runc caller death"]
async fn owned_runc_fixture() {
    let Some(root) = std::env::var_os("RELIABURGER_OWNED_RUNC_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let id = instance(&root);
    let runtime = runtime(&root);
    runtime
        .create(&id, &spec(&root, "exec /bin/busybox sleep 60"))
        .await
        .unwrap();
    install_fixture(&root, &id);
    runtime.start(&id).await.unwrap();
    assert!(runtime.start(&id).await.is_err());
    assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Running);
    runtime.exec(&id, &["/bin/busybox".into(), "sh".into(), "-c".into(), "/bin/busybox touch /work/exec-ready; while [ ! -f /work/release ]; do /bin/busybox sleep 0.02; done; /bin/busybox touch /work/late-exec".into()]).await.unwrap();
    runtime.kill(&id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn runc_owned_adoption_validates_generation_and_restores_live_network() {
    use reliaburger::grill::records::{InstanceRecord, RuntimeKind};
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let specification = spec(root.path(), "exec /bin/busybox sleep 60");
    let first = runtime(root.path());
    first.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    first.start(&id).await.unwrap();
    let pid = first.pid(&id).await.unwrap();
    let mut record = InstanceRecord {
        schema: 2,
        instance_id: id.0.clone(),
        namespace: "default".into(),
        app_name: "owned-runc".into(),
        replica_index: 0,
        is_job: false,
        image: "/empty-fixture".into(),
        runtime: RuntimeKind::Runc,
        pid,
        // One second off, as an NTP step between launch and recovery would
        // leave it: the same process, recorded against a moved clock.
        pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap() + 1,
        runc_container_id: Some(id.0.clone()),
        log_stem: first.log_stem(&id).await,
        host_port: None,
        app_spec: None,
        oci_spec: specification,
        rootless_network: None,
    };
    drop(first);
    let recovered = runtime(root.path());
    assert!(recovered.adopt(&id, &record).await.unwrap());
    assert_eq!(recovered.pid(&id).await, Some(pid));
    assert!(recovered.container_ip(&id).await.is_some());
    assert_eq!(
        recovered
            .exec(
                &id,
                &["/bin/busybox".into(), "echo".into(), "adopted".into()]
            )
            .await
            .unwrap(),
        "adopted\n"
    );
    record.log_stem = Some(root.path().join("older-generation/output"));
    assert!(recovered.adopt(&id, &record).await.is_err());
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Running);
    recovered.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn runc_owned_cancelled_preparation_keeps_its_worker_until_queued_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let bin = root.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let real_ip = std::process::Command::new("sh")
        .args(["-c", "command -v ip"])
        .output()
        .unwrap();
    assert!(real_ip.status.success());
    let real_ip = std::path::PathBuf::from(String::from_utf8(real_ip.stdout).unwrap().trim());
    let ready = root.path().join("prepare-ready");
    let release = root.path().join("release-preparation");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = netns ] && [ \"$2\" = add ]; then\n  touch {}\n  i=0\n  while [ ! -f {} ]; do i=$((i+1)); if [ \"$i\" -gt 1000 ]; then exit 90; fi; sleep 0.02; done\nfi\nexec {} \"$@\"\n",
        quote(&ready),
        quote(&release),
        quote(&real_ip)
    );
    std::fs::write(bin.join("ip"), script).unwrap();
    std::fs::set_permissions(bin.join("ip"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let wrapper = root.path().join("owner-wrapper");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nPATH={}:\"$PATH\"; export PATH\nexec {} \"$@\"\n",
            quote(&bin),
            quote(Path::new(env!("CARGO_BIN_EXE_bun")))
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = RuncGrill::new(
        root.path().join("bundles"),
        ImageStore::new(root.path().join("images")),
        false,
        root.path().join("state"),
        wrapper,
    )
    .unwrap();
    let creator = runtime.clone();
    let preparation_id = id.clone();
    let specification = spec(root.path(), "exit 0");
    let caller = tokio::spawn(async move { creator.create(&preparation_id, &specification).await });
    wait_file(&ready).await;
    caller.abort();
    let _ = caller.await;
    // Cleanup's own worker stays queued after its caller's short wait expires.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), runtime.kill(&id))
            .await
            .is_err()
    );
    std::fs::write(&release, "continue").unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.state(&id).await.unwrap() != ContainerState::Stopped {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_absent(root.path(), &id);
    // Positive retirement permits a new generation using the same name.
    runtime
        .create(&id, &spec(root.path(), "exit 7"))
        .await
        .unwrap();
    runtime.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn runc_owned_completed_log_reader_cannot_block_a_replacement_generation() {
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let runtime = runtime(root.path());
    runtime
        .create(
            &id,
            &spec(root.path(), "printf 'first\\nsecond\\nthird\\n'"),
        )
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    runtime.start(&id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while runtime.state(&id).await.unwrap() != ContainerState::Stopped {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let reader = runtime.clone();
    let log_id = id.clone();
    let stream = tokio::spawn(async move { reader.follow_logs(&log_id, sender).await });
    assert_eq!(receiver.recv().await.unwrap().line, "first");
    // The reader is now stalled on its tiny output channel, after retirement.
    runtime
        .create(&id, &spec(root.path(), "exit 0"))
        .await
        .unwrap();
    runtime.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
    drop(receiver);
    stream.await.unwrap();
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn generated_cgroup_path_matches_the_actual_container_before_its_first_instruction() {
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let expected = format!("/{}", id.0);
    let host_path = format!("/sys/fs/cgroup{expected}");
    let runtime = runtime(root.path());
    let mut specification = spec(
        root.path(),
        "/bin/busybox cat /proc/self/cgroup > /work/cgroup; exec /bin/busybox sleep 60",
    );
    specification.linux.cgroups_path = reliaburger::grill::oci::generate_init_oci_spec(
        &specification.process.args,
        "default",
        "cgroup-check",
        None,
        &host_path,
        None,
    )
    .linux
    .cgroups_path;
    runtime.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    // Model Bun's pre-start policy: the cgroup must be the same kernel object
    // when the container executes its first instruction.
    std::fs::create_dir(&host_path).unwrap();
    let before = reliaburger::sesame::egress::cgroup_id_of_path(Path::new(&host_path)).unwrap();
    runtime.start(&id).await.unwrap();
    wait_file(&root.path().join("shared/cgroup")).await;
    let observed = std::fs::read_to_string(root.path().join("shared/cgroup")).unwrap();
    let still_same =
        reliaburger::sesame::egress::cgroup_id_of_path(Path::new(&host_path)) == Some(before);
    runtime.kill(&id).await.unwrap();
    if Path::new(&host_path).exists() {
        std::fs::remove_dir(&host_path).unwrap();
    }
    assert_absent(root.path(), &id);
    assert_eq!(observed.trim(), format!("0::{expected}"));
    assert!(still_same, "runc replaced Bun's pre-programmed cgroup");
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn recovered_source_identity_belongs_to_the_container_not_its_launcher() {
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let path = format!("/sys/fs/cgroup/{}", id.0);
    let mut specification = spec(root.path(), "exec /bin/busybox sleep 60");
    specification.linux.cgroups_path = Some(format!("/{}", id.0));
    let first = runtime(root.path());
    first.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    first.start(&id).await.unwrap();
    let expected = reliaburger::sesame::egress::cgroup_id_of_path(Path::new(&path)).unwrap();
    let launcher = first.pid(&id).await.unwrap();
    let launcher_cgroup = reliaburger::sesame::egress::cgroup_id_of_pid(launcher);
    let live = first.workload_cgroup(&id).await;
    drop(first);
    let recovered = runtime(root.path());
    let after_recovery = recovered.workload_cgroup(&id).await;
    recovered.kill(&id).await.unwrap();
    let retired = recovered.workload_cgroup(&id).await;
    assert_absent(root.path(), &id);
    assert_ne!(launcher_cgroup, Some(expected));
    assert_eq!(live.unwrap(), Some(expected));
    assert_eq!(after_recovery.unwrap(), Some(expected));
    assert_eq!(retired.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn source_identity_refuses_a_container_moved_out_of_its_original_cgroup() {
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let runtime = runtime(root.path());
    let mut specification = spec(root.path(), "exec /bin/busybox sleep 60");
    specification.linux.cgroups_path = Some(format!("/{}", id.0));
    runtime.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    runtime.start(&id).await.unwrap();
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.path().join("state").join(&id.0).join("state.json")).unwrap(),
    )
    .unwrap();
    let pid = state["init_process_pid"].as_u64().unwrap();
    let relocated = std::path::PathBuf::from(format!("/sys/fs/cgroup/{}-relocated", id.0));
    std::fs::create_dir(&relocated).unwrap();
    std::fs::write(relocated.join("cgroup.procs"), pid.to_string()).unwrap();
    let identity = runtime.workload_cgroup(&id).await;
    runtime.kill(&id).await.unwrap();
    std::fs::remove_dir(relocated).unwrap();
    assert_absent(root.path(), &id);
    assert!(
        identity.is_err(),
        "accepted unverified source identity: {identity:?}"
    );
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn retiring_a_rollout_predecessor_preserves_its_live_successor() {
    let root = tempfile::tempdir().unwrap();
    let app_name = instance(root.path()).0;
    let first_id = reliaburger::grill::InstanceIdentity::new("default", &app_name, 0).instance_id();
    let successor_id =
        reliaburger::grill::InstanceIdentity::canary("default", &app_name, 1, 0).instance_id();
    let runtime = runtime(root.path());
    let mut specification = spec(root.path(), "exec /bin/busybox sleep 60");
    let first_path =
        reliaburger::grill::cgroup::instance_cgroup_path("default", &app_name, &first_id).unwrap();
    specification.linux.cgroups_path = reliaburger::grill::oci::generate_init_oci_spec(
        &specification.process.args,
        "default",
        &app_name,
        None,
        first_path.to_str().unwrap(),
        None,
    )
    .linux
    .cgroups_path;
    runtime.create(&first_id, &specification).await.unwrap();
    install_fixture(root.path(), &first_id);
    runtime.start(&first_id).await.unwrap();
    let successor_path =
        reliaburger::grill::cgroup::instance_cgroup_path("default", &app_name, &successor_id)
            .unwrap();
    specification.linux.cgroups_path = reliaburger::grill::oci::generate_init_oci_spec(
        &specification.process.args,
        "default",
        &app_name,
        None,
        successor_path.to_str().unwrap(),
        None,
    )
    .linux
    .cgroups_path;
    runtime.create(&successor_id, &specification).await.unwrap();
    install_fixture(root.path(), &successor_id);
    let started = runtime.start(&successor_id).await;
    let retired = runtime.kill(&first_id).await;
    let successor = runtime.state(&successor_id).await;
    let executed = runtime
        .exec(&successor_id, &["/bin/busybox".into(), "true".into()])
        .await;
    runtime.kill(&successor_id).await.unwrap();
    runtime.kill(&first_id).await.unwrap();
    assert_absent(root.path(), &first_id);
    assert_absent(root.path(), &successor_id);
    assert!(started.is_ok(), "successor could not start: {started:?}");
    assert!(retired.is_ok(), "predecessor could not retire: {retired:?}");
    assert_eq!(successor.unwrap(), ContainerState::Running);
    assert!(
        executed.is_ok(),
        "successor could not execute: {executed:?}"
    );
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn retained_addresses_survive_exit_and_recovery_until_the_original_reference_releases() {
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let first = runtime(root.path());
    first
        .create(&id, &spec(root.path(), "exit 0"))
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    let original_ip = first.container_ip(&id).await.unwrap();
    let original = first.retain_network_reference(&id).await.unwrap().unwrap();
    first.start(&id).await.unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while first.state(&id).await.unwrap() != ContainerState::Stopped {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(first);
    let recovered = runtime(root.path());
    let retained = recovered.network_reference(&id).await.unwrap();
    let held_inventory = recovered.launch_inventory().await.unwrap().unwrap();
    let held_evidence = held_inventory
        .iter()
        .find(|launch| launch.instance_id == id)
        .unwrap()
        .network_reference
        .clone();
    let other = InstanceId(format!("{}-other", id.0));
    recovered
        .create(&other, &spec(root.path(), "exit 0"))
        .await
        .unwrap();
    let other_ip = recovered.container_ip(&other).await.unwrap();
    recovered.kill(&other).await.unwrap();
    let refused_replacement = recovered
        .create(&id, &spec(root.path(), "exit 1"))
        .await
        .is_err();
    let mut wrong_address = original.clone();
    wrong_address.container_index += 1;
    let refused_wrong_address = recovered
        .release_network_reference(&wrong_address)
        .await
        .is_err();
    recovered
        .release_network_reference(&original)
        .await
        .unwrap();
    recovered
        .release_network_reference(&original)
        .await
        .unwrap();

    let released_inventory = recovered.launch_inventory().await.unwrap().unwrap();
    let released_evidence = released_inventory
        .iter()
        .find(|launch| launch.instance_id == id)
        .unwrap()
        .network_reference
        .clone();

    recovered
        .create(&id, &spec(root.path(), "exit 2"))
        .await
        .unwrap();
    let reused_ip = recovered.container_ip(&id).await.unwrap();
    let successor = recovered
        .retain_network_reference(&id)
        .await
        .unwrap()
        .unwrap();
    let refused_stale_release = recovered
        .release_network_reference(&original)
        .await
        .is_err();
    let successor_still_held = recovered.network_reference(&id).await.unwrap();
    // This fixture never publishes routes, so it may positively discharge its holds.
    recovered
        .release_network_reference(&successor)
        .await
        .unwrap();
    recovered.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
    assert_absent(root.path(), &other);
    assert_eq!(
        held_evidence,
        Some(reliaburger::grill::runc_intent::NetworkReferenceState::Held(original.clone()))
    );
    assert_eq!(
        released_evidence,
        Some(reliaburger::grill::runc_intent::NetworkReferenceState::Released(original.clone()))
    );
    assert_eq!(retained, Some(original));
    assert_ne!(
        original_ip, other_ip,
        "natural exit released a referenced address"
    );
    assert!(refused_replacement && refused_wrong_address && refused_stale_release);
    assert_eq!(
        original_ip, reused_ip,
        "confirmed release did not free the address"
    );
    assert_eq!(successor_still_held, Some(successor));
}

/// V02 soak: a stopped instance waiting for its address release left its
/// intent `retiring`. Every later Bun (restart, SIGKILL, host reboot) refused
/// to adopt it and exited, so systemd restarted it forever.
#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn restart_recovers_a_retiring_generation_that_still_holds_its_address() {
    use reliaburger::grill::records::{InstanceRecord, RuntimeKind};
    use reliaburger::grill::runc_intent::NetworkReferenceState;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let specification = spec(root.path(), "exec /bin/busybox sleep 60");
    let first = runtime(root.path());
    first.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    let original = first.retain_network_reference(&id).await.unwrap().unwrap();
    first.start(&id).await.unwrap();
    let pid = first.pid(&id).await.unwrap();
    let record = InstanceRecord {
        schema: 2,
        instance_id: id.0.clone(),
        namespace: "default".into(),
        app_name: "owned-runc".into(),
        replica_index: 0,
        is_job: false,
        image: "/empty-fixture".into(),
        runtime: RuntimeKind::Runc,
        pid,
        pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap(),
        runc_container_id: Some(id.0.clone()),
        log_stem: first.log_stem(&id).await,
        host_port: None,
        app_spec: None,
        oci_spec: specification,
        rootless_network: None,
    };
    // A rollout stops the instance; discovery still holds its address.
    first.kill(&id).await.unwrap();
    drop(first);

    let restarted = runtime(root.path());
    let after_restart = restarted.adopt(&id, &record).await;
    drop(restarted);

    // The same state after a reboot: the intent names an older kernel.
    let path = root
        .path()
        .join("bundles/.intents/records")
        .join(&id.0)
        .join("intent.json");
    let mut intent: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    intent["boot_id"] = "00000000-0000-4000-8000-000000000001".into();
    std::fs::write(&path, serde_json::to_vec(&intent).unwrap()).unwrap();
    let rebooted = runtime(root.path());
    let after_reboot = rebooted.adopt(&id, &record).await;
    let held = rebooted
        .launch_inventory()
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .find(|launch| launch.instance_id == id)
        .unwrap()
        .network_reference;
    let refused_replacement = rebooted
        .create(&id, &spec(root.path(), "exit 0"))
        .await
        .is_err();
    rebooted.release_network_reference(&original).await.unwrap();
    assert_eq!(rebooted.state(&id).await.unwrap(), ContainerState::Stopped);
    assert_absent(root.path(), &id);

    assert!(
        matches!(after_restart, Ok(false)),
        "restart refused a retiring generation: {after_restart:?}"
    );
    assert!(
        matches!(after_reboot, Ok(false)),
        "reboot refused a retiring generation: {after_reboot:?}"
    );
    assert_eq!(held, Some(NetworkReferenceState::Held(original)));
    assert!(
        refused_replacement,
        "a held address was handed to a replacement"
    );
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft"]
async fn previous_boot_intent_cannot_start_or_remove_conflicting_live_resources() {
    let root = tempfile::tempdir().unwrap();
    let id = instance(root.path());
    let first = runtime(root.path());
    first
        .create(&id, &spec(root.path(), "exit 0"))
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    drop(first);
    let path = root
        .path()
        .join("bundles/.intents/records")
        .join(&id.0)
        .join("intent.json");
    let original = std::fs::read(&path).unwrap();
    let mut record: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let recorded_boot = record["boot_id"].as_str().is_some();
    record["boot_id"] = "00000000-0000-4000-8000-000000000001".into();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let recovered = runtime(root.path());
    let started = recovered.start(&id).await;
    let retired = recovered.kill(&id).await;
    let namespace_retained = reliaburger::grill::netns::namespace_path(&id).exists();
    drop(recovered);
    std::fs::write(path, original).unwrap();
    runtime(root.path()).kill(&id).await.unwrap();
    assert!(
        recorded_boot,
        "OCI intent must remember its original kernel"
    );
    assert!(started.is_err(), "old-boot launch was admitted");
    assert!(
        retired.is_err(),
        "conflicting current-boot resources were deleted"
    );
    assert!(namespace_retained);
}

/// Two-phase fixture: the driver must actually power-cycle the disposable VM.
#[tokio::test]
#[ignore = "run only through scripts/release/qualify-oci-reboot.sh in a disposable Linux VM"]
async fn actual_host_reboot_preserves_holds_and_retires_original_execution() {
    // A missing variable means an automated driver picked this up by
    // mistake. Passing would claim reboot evidence nobody collected.
    let directory = std::env::var("RELIABURGER_REBOOT_DIRECTORY").expect(
        "RELIABURGER_REBOOT_DIRECTORY unset: run this only through \
         scripts/release/qualify-oci-reboot.sh, which power-cycles the VM",
    );
    assert!(nix::unistd::geteuid().is_root());
    let root = Path::new(&directory);
    let id = instance(root);
    let prepared = InstanceId(format!("{}-prepared", id.0));
    let runtime = runtime(root);
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let proof = root.join("proof.json");
    match std::env::var("RELIABURGER_REBOOT_PHASE").unwrap().as_str() {
        "prepare" => {
            assert!(!proof.exists(), "never overwrite earlier reboot evidence");
            let mut specification = spec(
                root,
                "printf 'run\\n' >> /work/runs; exec /bin/busybox sleep 86400",
            );
            specification.linux.cgroups_path = Some(format!("/{}", id.0));
            runtime.create(&id, &specification).await.unwrap();
            install_fixture(root, &id);
            let reference = runtime
                .retain_network_reference(&id)
                .await
                .unwrap()
                .unwrap();
            runtime.start(&id).await.unwrap();
            wait_file(&root.join("shared/runs")).await;
            runtime
                .create(
                    &prepared,
                    &spec(root, "printf unexpected > /work/prepared-ran"),
                )
                .await
                .unwrap();
            assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Running);
            std::fs::write(
                &proof,
                serde_json::to_vec(&serde_json::json!({"boot": boot, "reference": reference}))
                    .unwrap(),
            )
            .unwrap();
            std::fs::File::open(&proof).unwrap().sync_all().unwrap();
            std::fs::File::open(root).unwrap().sync_all().unwrap();
        }
        "verify" => {
            let evidence: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&proof).unwrap()).unwrap();
            assert_ne!(
                evidence["boot"].as_str().unwrap(),
                boot,
                "this is not an actual kernel reboot"
            );
            let reference: reliaburger::grill::runc_intent::NetworkReference =
                serde_json::from_value(evidence["reference"].clone()).unwrap();
            assert!(!reliaburger::grill::netns::namespace_path(&id).exists());
            assert!(
                !Path::new("/sys/class/net")
                    .join(reliaburger::grill::netns::host_veth_name(&id))
                    .exists()
            );
            assert!(!Path::new("/sys/fs/cgroup").join(&id.0).exists());
            assert!(
                root.join("state").join(&id.0).exists(),
                "fixture must retain stale OCI metadata across reboot"
            );
            assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Stopped);
            assert_eq!(runtime.exit_code(&id).await, None);
            assert_eq!(
                runtime.state(&prepared).await.unwrap(),
                ContainerState::Stopped
            );
            assert_eq!(
                runtime.network_reference(&id).await.unwrap(),
                Some(reference.clone())
            );
            assert!(runtime.create(&id, &spec(root, "exit 0")).await.is_err());
            assert_eq!(
                std::fs::read_to_string(root.join("shared/runs")).unwrap(),
                "run\n"
            );
            assert!(!root.join("shared/prepared-ran").exists());
            runtime.release_network_reference(&reference).await.unwrap();
            runtime.create(&id, &spec(root, "exit 7")).await.unwrap();
            install_fixture(root, &id);
            let successor = runtime
                .retain_network_reference(&id)
                .await
                .unwrap()
                .unwrap();
            assert_ne!(successor.generation, reference.generation);
            assert!(runtime.release_network_reference(&reference).await.is_err());
            runtime.start(&id).await.unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                while runtime.state(&id).await.unwrap() != ContainerState::Stopped {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(runtime.exit_code(&id).await, Some(7));
            runtime.release_network_reference(&successor).await.unwrap();
            assert_absent(root, &id);
            assert_absent(root, &prepared);
            std::fs::write(root.join("verified-boot"), boot).unwrap();
        }
        other => panic!("invalid reboot qualification phase {other}"),
    }
}
