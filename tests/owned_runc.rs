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
    )
    .with_owner(env!("CARGO_BIN_EXE_bun").into())
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
    recovered.kill(&id).await.unwrap();
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Stopped);
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
        pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap(),
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
    )
    .with_owner(wrapper)
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
    assert_eq!(receiver.recv().await.unwrap(), "first");
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
