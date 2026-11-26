//! Actual OCI lifecycle recovery without an agent adoption record.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use reliaburger::grill::runc::RuncGrill;
use reliaburger::grill::{ContainerState, Grill, ImageStore, InstanceId, OciSpec};

#[path = "support/runc_residue.rs"]
mod runc_residue;

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
        "process": {"args": ["/bin/busybox", "sh", "-c", script], "env": ["PATH=/bin", format!("API_TOKEN={SECRET}")], "cwd": "/", "user": {"uid": 0, "gid": 0}},
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

/// Stands in for a value decrypted from `ENC[...]`.
const SECRET: &str = "owned-runc-decrypted-secret";

/// Whether any regular file below `path` contains `needle`.
fn contains_text(path: &Path, needle: &str) -> bool {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("{}: {error}", path.display()),
        // A root filesystem holds only fixture binaries, and could still hold
        // a kernel mount if cleanup regressed; never read through one.
        Ok(metadata) if metadata.is_dir() => std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_name() != "rootfs")
            .any(|entry| contains_text(&entry.path(), needle)),
        Ok(metadata) if metadata.is_file() => {
            String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains(needle)
        }
        Ok(_) => false,
    }
}

fn assert_absent(root: &Path, id: &InstanceId) {
    assert!(!root.join("state").join(&id.0).exists());
    // The spec carries decrypted env; it must not outlive the instance.
    assert!(
        !root
            .join("bundles")
            .join(&id.0)
            .join("config.json")
            .exists()
    );
    // Nor may the retired intent keep it (#476).
    for path in [
        root.join("bundles").join(&id.0),
        root.join("bundles/.intents/records").join(&id.0),
    ] {
        assert!(
            !contains_text(&path, SECRET),
            "{} kept a decrypted secret",
            path.display()
        );
    }
    assert!(!reliaburger::grill::netns::namespace_path(id).exists());
    assert!(
        !Path::new("/sys/class/net")
            .join(reliaburger::grill::netns::host_veth_name(id))
            .exists()
    );
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_owned_preparation_recovers_original_intent_and_retires_without_adoption() {
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
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
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_owned_short_job_keeps_its_actual_exit_and_logs_after_reconstruction() {
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
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
    assert_eq!(recovered.exit_code(&id).await.unwrap(), Some(7));
    assert!(recovered.logs(&id).await.unwrap().contains("short-job"));
    assert_absent(root.path(), &id);
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_owned_launcher_and_exec_retire_after_actual_caller_sigkill() {
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
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
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_owned_adoption_validates_generation_and_restores_live_network() {
    use reliaburger::grill::records::{InstanceRecord, RuntimeKind};
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let specification = spec(root.path(), "exec /bin/busybox sleep 60");
    let first = runtime(root.path());
    first.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    first.start(&id).await.unwrap();
    let pid = first.pid(&id).await.unwrap().unwrap();
    let mut record = InstanceRecord {
        schema: reliaburger::grill::records::RECORD_SCHEMA,
        instance_id: id.0.clone(),
        namespace: "default".into(),
        app_name: "owned-runc".into(),
        replica_index: 0,
        image: "/empty-fixture".into(),
        runtime: RuntimeKind::Runc,
        pid,
        pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap(),
        boot_id: reliaburger::grill::records::current_boot(),
        runc_container_id: Some(id.0.clone()),
        log_stem: first.log_stem(&id).await,
        host_port: None,
        app_spec: None,
        oci_spec: specification,
        rootless_network: None,
        sidecar_restarts: None,
    };
    drop(first);
    let recovered = runtime(root.path());
    assert!(recovered.adopt(&id, &record).await.unwrap());
    assert_eq!(recovered.pid(&id).await.unwrap(), Some(pid));
    assert!(recovered.container_ip(&id).await.is_some());
    assert_eq!(
        recovered
            .exec(
                &id,
                &["/bin/busybox".into(), "echo".into(), "adopted".into()]
            )
            .await
            .unwrap()
            .output,
        "adopted\n"
    );
    // A record naming another launch's logs no longer describes this
    // generation. It's stale, so adoption retires the generation instead of
    // refusing startup (#1849).
    record.log_stem = Some(root.path().join("older-generation/output"));
    let stale = recovered.adopt(&id, &record).await;
    assert!(matches!(stale, Ok(false)), "{stale:?}");
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Stopped);
    assert_absent(root.path(), &id);
}

/// #2334: Bun's record keeps a `PROVIDER[...]` reference and runc's intent
/// the value the provider gave. A restarted Bun must still adopt the
/// instance, and a record from another request must still conflict.
#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_owned_adoption_accepts_a_provider_reference_for_its_resolved_value() {
    use reliaburger::grill::records::{InstanceRecord, RuntimeKind};
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let reference = spec(root.path(), "exec /bin/busybox sleep 60");
    let mut supervised = reference.clone();
    supervised
        .process
        .env
        .push("DB_PASSWORD=PROVIDER[vault:secret/db#password]".into());
    supervised.process.provider_env = vec!["DB_PASSWORD".into()];
    let mut resolved = reference;
    resolved.process.env.push(format!("DB_PASSWORD={SECRET}"));
    let first = runtime(root.path());
    first.create(&id, &resolved).await.unwrap();
    install_fixture(root.path(), &id);
    first.start(&id).await.unwrap();
    let pid = first.pid(&id).await.unwrap().unwrap();
    let mut record = InstanceRecord {
        schema: reliaburger::grill::records::RECORD_SCHEMA,
        instance_id: id.0.clone(),
        namespace: "default".into(),
        app_name: "owned-runc".into(),
        replica_index: 0,
        image: "/empty-fixture".into(),
        runtime: RuntimeKind::Runc,
        pid,
        pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap(),
        boot_id: reliaburger::grill::records::current_boot(),
        runc_container_id: Some(id.0.clone()),
        log_stem: first.log_stem(&id).await,
        host_port: None,
        app_spec: None,
        oci_spec: supervised.clone(),
        rootless_network: None,
        sidecar_restarts: None,
    };
    drop(first);
    let recovered = runtime(root.path());

    record.oci_spec.process.args.push("--elsewhere".into());
    let conflict = recovered.adopt(&id, &record).await;
    assert!(conflict.is_err(), "{conflict:?}");

    record.oci_spec = supervised;
    assert!(recovered.adopt(&id, &record).await.unwrap());
    assert_eq!(recovered.pid(&id).await.unwrap(), Some(pid));
    recovered.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
}

/// #1315: a restarted Bun adopts a sidecar beside its instance. The sidecar
/// keeps its process, and it's still in its instance's network namespace,
/// which it holds no address of its own in: there's no network of its own
/// to adopt, so adoption mustn't look for one.
#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn a_bun_restart_adopts_a_sidecar_with_its_process_and_its_instances_network() {
    let root = crate::runc_residue::RuncRoot::new();
    let parent = instance(root.path());
    let sidecar = reliaburger::grill::oci::sidecar_instance_id(&parent, "probe");
    let app = spec(root.path(), "exec /bin/busybox sleep 60");
    let mut beside = spec(root.path(), "exec /bin/busybox sleep 60");
    let shared = reliaburger::grill::oci::network_namespace_path(&parent);
    beside.linux.namespaces = reliaburger::grill::oci::standard_namespaces(Some(shared.as_str()));
    let first = runtime(root.path());
    // Bun's order: the instance's create makes the namespace the sidecar
    // joins, and the sidecar starts first.
    first.create(&parent, &app).await.unwrap();
    install_fixture(root.path(), &parent);
    first.create(&sidecar, &beside).await.unwrap();
    install_fixture(root.path(), &sidecar);
    first.start(&sidecar).await.unwrap();
    first.start(&parent).await.unwrap();
    let parent_pid = first.pid(&parent).await.unwrap().unwrap();
    let sidecar_pid = first.pid(&sidecar).await.unwrap().unwrap();
    let parent_record = adoption_record(&parent, parent_pid, first.log_stem(&parent).await, app);
    let sidecar_record = adoption_record(
        &sidecar,
        sidecar_pid,
        first.log_stem(&sidecar).await,
        beside,
    );
    drop(first);

    let recovered = runtime(root.path());
    assert!(recovered.adopt(&parent, &parent_record).await.unwrap());
    let adopted = recovered.adopt(&sidecar, &sidecar_record).await;
    assert!(matches!(adopted, Ok(true)), "{adopted:?}");
    assert_eq!(recovered.pid(&sidecar).await.unwrap(), Some(sidecar_pid));
    assert_eq!(
        recovered.state(&sidecar).await.unwrap(),
        ContainerState::Running
    );
    assert_eq!(recovered.container_ip(&sidecar).await, None);
    let network_of = |pid: u32| std::fs::read_link(format!("/proc/{pid}/ns/net")).unwrap();
    let workload = |pids: Vec<u32>| *pids.first().expect("no workload process");
    let parent_workload = workload(recovered.workload_pids(&parent).await.unwrap());
    let sidecar_workload = workload(recovered.workload_pids(&sidecar).await.unwrap());
    assert_eq!(
        network_of(sidecar_workload),
        network_of(parent_workload),
        "the adopted sidecar left its instance's namespace"
    );

    recovered.kill(&sidecar).await.unwrap();
    recovered.kill(&parent).await.unwrap();
    assert_absent(root.path(), &sidecar);
    assert_absent(root.path(), &parent);
}

/// Review of #2269: a sidecar that died while Bun was down. Its record names
/// a process that's gone, so adoption refuses it, and once its retirement
/// has made the runtime forget it, the runtime answers `NotFound`: what the
/// agent must not kill a second time.
#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn a_sidecar_that_died_before_adoption_is_refused_and_then_forgotten() {
    let root = crate::runc_residue::RuncRoot::new();
    let parent = instance(root.path());
    let sidecar = reliaburger::grill::oci::sidecar_instance_id(&parent, "probe");
    let app = spec(root.path(), "exec /bin/busybox sleep 60");
    let mut beside = spec(root.path(), "exec /bin/busybox sleep 60");
    let shared = reliaburger::grill::oci::network_namespace_path(&parent);
    beside.linux.namespaces = reliaburger::grill::oci::standard_namespaces(Some(shared.as_str()));
    let first = runtime(root.path());
    first.create(&parent, &app).await.unwrap();
    install_fixture(root.path(), &parent);
    first.create(&sidecar, &beside).await.unwrap();
    install_fixture(root.path(), &sidecar);
    first.start(&sidecar).await.unwrap();
    first.start(&parent).await.unwrap();
    let sidecar_pid = first.pid(&sidecar).await.unwrap().unwrap();
    let parent_record = adoption_record(
        &parent,
        first.pid(&parent).await.unwrap().unwrap(),
        first.log_stem(&parent).await,
        app,
    );
    let sidecar_record = adoption_record(
        &sidecar,
        sidecar_pid,
        first.log_stem(&sidecar).await,
        beside,
    );
    for pid in first.workload_pids(&sidecar).await.unwrap() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        while first.state(&sidecar).await.unwrap() != ContainerState::Stopped {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the killed sidecar never stopped");
    drop(first);

    let recovered = runtime(root.path());
    assert!(recovered.adopt(&parent, &parent_record).await.unwrap());
    let refused = recovered.adopt(&sidecar, &sidecar_record).await;
    assert!(matches!(refused, Ok(false)), "{refused:?}");
    recovered.forget_retired(&sidecar).await.unwrap();
    let again = recovered.kill(&sidecar).await;
    assert!(
        matches!(again, Err(reliaburger::grill::GrillError::NotFound { .. })),
        "{again:?}"
    );

    recovered.kill(&parent).await.unwrap();
    assert_absent(root.path(), &sidecar);
    assert_absent(root.path(), &parent);
}

fn adoption_record(
    id: &InstanceId,
    pid: u32,
    log_stem: Option<std::path::PathBuf>,
    oci_spec: OciSpec,
) -> reliaburger::grill::records::InstanceRecord {
    reliaburger::grill::records::InstanceRecord {
        schema: reliaburger::grill::records::RECORD_SCHEMA,
        instance_id: id.0.clone(),
        namespace: "default".into(),
        app_name: "owned-runc".into(),
        replica_index: 0,
        image: "/empty-fixture".into(),
        runtime: reliaburger::grill::records::RuntimeKind::Runc,
        pid,
        pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap_or(1),
        boot_id: reliaburger::grill::records::current_boot(),
        runc_container_id: Some(id.0.clone()),
        log_stem,
        host_port: None,
        app_spec: None,
        oci_spec,
        rootless_network: None,
        sidecar_restarts: None,
    }
}

/// The 0.2.8 gate (#1849): Bun was SIGKILLed during a rollout, then the node
/// lost power and its page cache. The adoption record and the generation's
/// launcher binding reached the disk from different moments, and every later
/// Bun exited with "adoption log identity conflicts with runtime generation".
/// Either order is stale: adoption retires that generation, says so, and the
/// node's other instances still adopt.
#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn a_record_and_generation_split_by_a_power_cut_retire_and_others_still_adopt() {
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
    let healthy = instance(root.path());
    // The record is newer: it names a launch whose binding never reached
    // the generation, so the generation has no launcher at all.
    let record_newer = InstanceId(format!("{}-r", healthy.0));
    // The generation is newer: its launcher is bound, but the record was
    // written before it and names no logs.
    let generation_newer = InstanceId(format!("{}-g", healthy.0));
    let specification = spec(root.path(), "exec /bin/busybox sleep 60");
    let first = runtime(root.path());
    let mut records = Vec::new();
    for id in [&healthy, &generation_newer] {
        first.create(id, &specification).await.unwrap();
        install_fixture(root.path(), id);
        first.start(id).await.unwrap();
        let pid = first.pid(id).await.unwrap().unwrap();
        records.push(adoption_record(
            id,
            pid,
            first.log_stem(id).await,
            specification.clone(),
        ));
    }
    records[1].log_stem = None;
    first.create(&record_newer, &specification).await.unwrap();
    install_fixture(root.path(), &record_newer);
    assert_eq!(first.log_stem(&record_newer).await, None);
    let lost_launch = Some(root.path().join("lost-launch/output"));
    records.push(adoption_record(
        &record_newer,
        records[0].pid,
        lost_launch,
        specification.clone(),
    ));
    drop(first);

    let restarted = runtime(root.path());
    let healthy_adoption = restarted.adopt(&healthy, &records[0]).await;
    let generation_newer_adoption = restarted.adopt(&generation_newer, &records[1]).await;
    let record_newer_adoption = restarted.adopt(&record_newer, &records[2]).await;
    let healthy_state = restarted.state(&healthy).await;
    let generation_newer_state = restarted.state(&generation_newer).await;
    let record_newer_state = restarted.state(&record_newer).await;
    restarted.kill(&healthy).await.unwrap();

    assert!(
        matches!(generation_newer_adoption, Ok(false)),
        "a record older than its generation refused startup: {generation_newer_adoption:?}"
    );
    assert!(
        matches!(record_newer_adoption, Ok(false)),
        "a record newer than its generation refused startup: {record_newer_adoption:?}"
    );
    assert_eq!(generation_newer_state.unwrap(), ContainerState::Stopped);
    assert_eq!(record_newer_state.unwrap(), ContainerState::Stopped);
    assert_absent(root.path(), &generation_newer);
    assert_absent(root.path(), &record_newer);
    assert!(matches!(healthy_adoption, Ok(true)), "{healthy_adoption:?}");
    assert_eq!(healthy_state.unwrap(), ContainerState::Running);
    assert_absent(root.path(), &healthy);
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn a_stale_adoption_record_retires_its_generation_and_others_still_adopt() {
    // The 0.1.5 soak (#607): an adoption record whose process identity no
    // longer matched the launcher the owner reported running stopped Bun at
    // every start, taking every workload on the node down with it.
    use reliaburger::grill::records::{InstanceRecord, RuntimeKind};
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
    let stale = instance(root.path());
    let healthy = InstanceId(format!("{}-b", stale.0));
    let specification = spec(root.path(), "exec /bin/busybox sleep 60");
    let first = runtime(root.path());
    let mut records = Vec::new();
    for id in [&stale, &healthy] {
        first.create(id, &specification).await.unwrap();
        install_fixture(root.path(), id);
        first.start(id).await.unwrap();
        let pid = first.pid(id).await.unwrap().unwrap();
        records.push(InstanceRecord {
            schema: reliaburger::grill::records::RECORD_SCHEMA,
            instance_id: id.0.clone(),
            namespace: "default".into(),
            app_name: "owned-runc".into(),
            replica_index: 0,
            image: "/empty-fixture".into(),
            runtime: RuntimeKind::Runc,
            pid,
            pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap(),
            boot_id: reliaburger::grill::records::current_boot(),
            runc_container_id: Some(id.0.clone()),
            log_stem: first.log_stem(id).await,
            host_port: None,
            app_spec: None,
            oci_spec: specification.clone(),
            rootless_network: None,
            sidecar_restarts: None,
        });
    }
    // The stale record names another start of the launcher's pid, further
    // off than the ±2 s an older build tolerated.
    records[0].pid_started_at += 3;
    drop(first);

    let restarted = runtime(root.path());
    let stale_adoption = restarted.adopt(&stale, &records[0]).await;
    let healthy_adoption = restarted.adopt(&healthy, &records[1]).await;
    let stale_state = restarted.state(&stale).await;
    let healthy_state = restarted.state(&healthy).await;
    restarted.kill(&healthy).await.unwrap();

    assert!(
        matches!(stale_adoption, Ok(false)),
        "a stale record refused adoption instead of retiring its generation: {stale_adoption:?}"
    );
    assert_eq!(stale_state.unwrap(), ContainerState::Stopped);
    assert_absent(root.path(), &stale);
    assert!(matches!(healthy_adoption, Ok(true)), "{healthy_adoption:?}");
    assert_eq!(healthy_state.unwrap(), ContainerState::Running);
    assert_absent(root.path(), &healthy);
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_owned_cancelled_preparation_keeps_its_worker_until_queued_cleanup() {
    if let Some(root) = std::env::var_os("RELIABURGER_CANCELLED_PREPARATION_FIXTURE") {
        cancelled_preparation(Path::new(&root)).await;
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = crate::runc_residue::RuncRoot::new();
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
    // Runtime commands use the environment recorded by the caller. Install
    // the fake ip in that caller's PATH, without changing this test process's
    // environment or relying on an owner's wrapper to overwrite it later.
    let caller = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runc_owned_cancelled_preparation_keeps_its_worker_until_queued_cleanup",
            "--ignored",
            "--nocapture",
        ])
        .env("RELIABURGER_CANCELLED_PREPARATION_FIXTURE", root.path())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(45), caller)
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn cancelled_preparation(root: &Path) {
    let id = instance(root);
    let ready = root.join("prepare-ready");
    let release = root.join("release-preparation");
    let runtime = runtime(root);
    let creator = runtime.clone();
    let preparation_id = id.clone();
    let specification = spec(root, "exit 0");
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
    assert_absent(root, &id);
    // Positive retirement permits a new generation using the same name.
    runtime.create(&id, &spec(root, "exit 7")).await.unwrap();
    runtime.kill(&id).await.unwrap();
    assert_absent(root, &id);
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_owned_completed_log_reader_cannot_block_a_replacement_generation() {
    let root = crate::runc_residue::RuncRoot::new();
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
    let stream = tokio::spawn(async move {
        reader
            .follow_logs(&log_id, sender, &Default::default())
            .await
    });
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

/// V02 soak blocker (candidate 3fcb1fd): a restarted Bun follows every
/// adopted container's capture from byte 0. The soak's log spammer had
/// written about a million lines, and splitting them in one synchronous step
/// held a runtime worker for minutes, which starved startup adoption until
/// its 10 s deadline expired. The replay must hand the runtime back between
/// bounded chunks. `#[tokio::test]` is single-threaded, so a stalled 1 ms
/// timer is the runtime being held.
#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn following_a_large_capture_after_a_restart_does_not_hold_the_runtime() {
    const LINES: u64 = 200_000;
    const LINE: &str = "spam the quick brown fox jumps over";
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let first = runtime(root.path());
    first
        .create(
            &id,
            &spec(
                root.path(),
                &format!(
                    "/bin/busybox yes '{LINE}' | /bin/busybox head -n {LINES}; \
                     /bin/busybox touch /work/written; exec /bin/busybox sleep 60"
                ),
            ),
        )
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    first.start(&id).await.unwrap();
    wait_file(&root.path().join("shared/written")).await;
    drop(first);

    // A fresh runtime handle, as after a Bun restart, re-follows from byte 0.
    let restarted = runtime(root.path());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(256);
    let reader = restarted.clone();
    let log_id = id.clone();
    let stream = tokio::spawn(async move {
        reader
            .follow_logs(&log_id, sender, &Default::default())
            .await
    });
    let consumer = tokio::spawn(async move {
        let mut last = None;
        for _ in 0..LINES {
            last = receiver.recv().await;
        }
        last
    });
    let mut longest_stall = Duration::ZERO;
    let started = std::time::Instant::now();
    while !consumer.is_finished() && started.elapsed() < Duration::from_secs(120) {
        let tick = std::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(1)).await;
        longest_stall = longest_stall.max(tick.elapsed());
    }
    let last = consumer.await.unwrap();
    stream.abort();
    restarted.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);

    let last = last.expect("the replay ended before the whole capture arrived");
    assert_eq!(last.line, LINE);
    assert_eq!(
        last.position.unwrap().end_offset,
        LINES * (LINE.len() as u64 + 1)
    );
    assert!(
        longest_stall < Duration::from_secs(1),
        "replaying the capture held the runtime for {longest_stall:?}"
    );
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn generated_cgroup_path_matches_the_actual_container_before_its_first_instruction() {
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let expected = format!("/{}", id.0);
    let host_path = format!("/sys/fs/cgroup{expected}");
    let runtime = runtime(root.path());
    let mut specification = spec(
        root.path(),
        "/bin/busybox cat /proc/self/cgroup > /work/cgroup; exec /bin/busybox sleep 60",
    );
    specification.linux.cgroups_path =
        Some(reliaburger::grill::oci::hierarchy_cgroup_path(&host_path));
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

/// A memory request smaller than `runc init` itself must not hold the
/// container's start: runc 1.4's init holds about 10 MiB of anonymous memory
/// in the workload's cgroup, and as `memory.high` the burger demo's 8 MiB
/// request throttled it past Bun's startup deadline (#1955).
#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn a_memory_request_smaller_than_runc_init_does_not_hold_the_start() {
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let host_path = format!("/sys/fs/cgroup/{}", id.0);
    let runtime = runtime(root.path());
    let app: reliaburger::config::AppSpec = toml::from_str(
        r#"
        image = "test:v1"
        memory = "4Mi-64Mi"
        "#,
    )
    .unwrap();
    let generated = reliaburger::grill::oci::generate_oci_spec(
        "small", "default", &app, &id.0, None, &host_path, None, None,
    );
    let mut specification = spec(
        root.path(),
        "/bin/busybox touch /work/started; exec /bin/busybox sleep 60",
    );
    specification.linux.resources = generated.linux.resources;
    specification.linux.cgroups_path =
        Some(reliaburger::grill::oci::hierarchy_cgroup_path(&host_path));
    runtime.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    let begun = std::time::Instant::now();
    let started = runtime.start(&id).await;
    let took = begun.elapsed();
    let memory_high = std::fs::read_to_string(format!("{host_path}/memory.high"));
    let memory_max = std::fs::read_to_string(format!("{host_path}/memory.max"));
    if started.is_ok() {
        wait_file(&root.path().join("shared/started")).await;
    }
    runtime.kill(&id).await.unwrap();
    if Path::new(&host_path).exists() {
        std::fs::remove_dir(&host_path).unwrap();
    }
    assert_absent(root.path(), &id);
    started.unwrap();
    assert!(took < Duration::from_secs(5), "start took {took:?}");
    assert_eq!(memory_high.unwrap().trim(), "max");
    assert_eq!(memory_max.unwrap().trim(), (64u64 << 20).to_string());
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn recovered_source_identity_belongs_to_the_container_not_its_launcher() {
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let path = format!("/sys/fs/cgroup/{}", id.0);
    let mut specification = spec(root.path(), "exec /bin/busybox sleep 60");
    specification.linux.cgroups_path = Some(format!("/{}", id.0));
    let first = runtime(root.path());
    first.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    first.start(&id).await.unwrap();
    let expected = reliaburger::sesame::egress::cgroup_id_of_path(Path::new(&path)).unwrap();
    let launcher = first.pid(&id).await.unwrap().unwrap();
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
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn source_identity_refuses_a_container_moved_out_of_its_original_cgroup() {
    let root = crate::runc_residue::RuncRoot::new();
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
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn retiring_a_rollout_predecessor_preserves_its_live_successor() {
    let root = crate::runc_residue::RuncRoot::new();
    let app_name = instance(root.path()).0;
    let first_id = reliaburger::grill::InstanceIdentity::new("default", &app_name, 0).instance_id();
    let successor_id =
        reliaburger::grill::InstanceIdentity::canary("default", &app_name, 1, 0).instance_id();
    let runtime = runtime(root.path());
    let mut specification = spec(root.path(), "exec /bin/busybox sleep 60");
    let first_path =
        reliaburger::grill::cgroup::instance_cgroup_path("default", &app_name, &first_id).unwrap();
    specification.linux.cgroups_path = Some(reliaburger::grill::oci::hierarchy_cgroup_path(
        first_path.to_str().unwrap(),
    ));
    runtime.create(&first_id, &specification).await.unwrap();
    install_fixture(root.path(), &first_id);
    runtime.start(&first_id).await.unwrap();
    let successor_path =
        reliaburger::grill::cgroup::instance_cgroup_path("default", &app_name, &successor_id)
            .unwrap();
    specification.linux.cgroups_path = Some(reliaburger::grill::oci::hierarchy_cgroup_path(
        successor_path.to_str().unwrap(),
    ));
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
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn retained_addresses_survive_exit_and_recovery_until_the_original_reference_releases() {
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
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
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn restart_recovers_a_retiring_generation_that_still_holds_its_address() {
    use reliaburger::grill::records::{InstanceRecord, RuntimeKind};
    use reliaburger::grill::runc_intent::NetworkReferenceState;
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let specification = spec(root.path(), "exec /bin/busybox sleep 60");
    let first = runtime(root.path());
    first.create(&id, &specification).await.unwrap();
    install_fixture(root.path(), &id);
    let original = first.retain_network_reference(&id).await.unwrap().unwrap();
    first.start(&id).await.unwrap();
    let pid = first.pid(&id).await.unwrap().unwrap();
    let record = InstanceRecord {
        schema: reliaburger::grill::records::RECORD_SCHEMA,
        instance_id: id.0.clone(),
        namespace: "default".into(),
        app_name: "owned-runc".into(),
        replica_index: 0,
        image: "/empty-fixture".into(),
        runtime: RuntimeKind::Runc,
        pid,
        pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap(),
        boot_id: reliaburger::grill::records::current_boot(),
        runc_container_id: Some(id.0.clone()),
        log_stem: first.log_stem(&id).await,
        host_port: None,
        app_spec: None,
        oci_spec: specification,
        rootless_network: None,
        sidecar_restarts: None,
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
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn previous_boot_intent_cannot_start_or_remove_conflicting_live_resources() {
    let root = crate::runc_residue::RuncRoot::new();
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
    let split = InstanceId(format!("{}-split", id.0));
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
                .create(&split, &spec(root, "exec /bin/busybox sleep 86400"))
                .await
                .unwrap();
            install_fixture(root, &split);
            runtime.start(&split).await.unwrap();
            assert_eq!(
                runtime.state(&split).await.unwrap(),
                ContainerState::Running
            );
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
            // #1849: a record that lost its race with the launcher binding
            // across the power cut is stale, never a reason to refuse startup.
            let split_record = adoption_record(
                &split,
                1,
                Some(root.join("lost-launch/output")),
                spec(root, "exec /bin/busybox sleep 86400"),
            );
            let split_adoption = runtime.adopt(&split, &split_record).await;
            assert!(
                matches!(split_adoption, Ok(false)),
                "a split record refused startup after a reboot: {split_adoption:?}"
            );
            assert_eq!(
                runtime.state(&split).await.unwrap(),
                ContainerState::Stopped
            );
            assert_absent(root, &split);
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
            assert_eq!(runtime.exit_code(&id).await.unwrap(), None);
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
            assert_eq!(runtime.exit_code(&id).await.unwrap(), Some(7));
            runtime.release_network_reference(&successor).await.unwrap();
            assert_absent(root, &id);
            assert_absent(root, &prepared);
            std::fs::write(root.join("verified-boot"), boot).unwrap();
        }
        other => panic!("invalid reboot qualification phase {other}"),
    }
}

/// Which process a pause freezes.
#[derive(Debug, Clone, Copy)]
enum Paused {
    /// Every process Smoker's pause signals: the container's (#2079).
    Workload,
    /// The container's own init, as `runc state` names it.
    ContainerInit,
}

/// Start a container that handles SIGTERM, freeze `paused`, and stop it with
/// a 20 s grace. A paused instance must take its stop signal at once: Bun
/// follows the signal with SIGCONT (#2041), as ProcessGrill does, instead of
/// letting the frozen process sit out its grace and die by SIGKILL.
async fn paused_container_takes_its_stop_signal_at_once(paused: Paused) {
    use reliaburger::bun::agent::{InstanceStop, stop_runtime_instance};
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let runtime = runtime(root.path());
    runtime
        .create(
            &id,
            &spec(
                root.path(),
                "trap '/bin/busybox touch /work/term; exit 0' TERM; \
                 /bin/busybox touch /work/ready; \
                 while :; do /bin/busybox sleep 0.05; done",
            ),
        )
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    runtime.start(&id).await.unwrap();
    wait_file(&root.path().join("shared/ready")).await;
    let pids = match paused {
        Paused::Workload => runtime.workload_pids(&id).await.unwrap(),
        Paused::ContainerInit => vec![container_init(root.path(), &id).expect("a container init")],
    };
    let pids = pause_live(&pids);
    assert!(!pids.is_empty(), "nothing to pause");

    let grace = Duration::from_secs(20);
    let begun = std::time::Instant::now();
    let stopped = tokio::time::timeout(
        Duration::from_secs(60),
        stop_runtime_instance(
            &runtime,
            &id,
            &InstanceStop::for_policy(None, grace),
            Duration::from_secs(10),
        ),
    )
    .await;
    let took = begun.elapsed();
    let handled = root.path().join("shared/term").exists();
    for pid in &pids {
        let _ = reliaburger::smoker::process::resume_process(*pid as i32);
    }
    runtime.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
    stopped.expect("the stop never finished").unwrap();
    assert!(
        handled,
        "the paused container never handled its stop signal"
    );
    assert!(took < grace, "the stop waited out its grace: {took:?}");
}

/// The container's init pid as `runc state` reports it.
fn container_init(root: &Path, id: &InstanceId) -> Option<u32> {
    let output = std::process::Command::new("runc")
        .arg("--root")
        .arg(root.join("state"))
        .args(["state", &id.0])
        .output()
        .ok()?;
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    state["pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_container_paused_by_smoker_takes_its_stop_signal_at_once() {
    paused_container_takes_its_stop_signal_at_once(Paused::Workload).await;
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn runc_container_with_a_paused_init_takes_its_stop_signal_at_once() {
    paused_container_takes_its_stop_signal_at_once(Paused::ContainerInit).await;
}

/// Smoker's pause freezes the container, not the `runc run` launcher that
/// `Grill::pid` names (#2079). SIGSTOP can't be caught or forwarded, so a
/// frozen launcher left the workload running. The workload here writes a
/// heartbeat: while the processes a pause signals are frozen it stands
/// still, and once they're resumed it moves again.
#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux"]
async fn runc_pause_freezes_the_workload_not_its_launcher() {
    use reliaburger::smoker::process::resume_process;
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let runtime = runtime(root.path());
    runtime
        .create(
            &id,
            &spec(
                root.path(),
                "/bin/busybox touch /work/ready; \
                 while :; do echo . >> /work/beats; /bin/busybox sleep 0.05; done",
            ),
        )
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    runtime.start(&id).await.unwrap();
    wait_file(&root.path().join("shared/ready")).await;
    let beats = || {
        std::fs::metadata(root.path().join("shared/beats"))
            .map(|metadata| metadata.len())
            .unwrap_or(0)
    };
    let launcher = runtime.pid(&id).await.unwrap().expect("a launcher");
    // The shell loop is the container's init and lives for the whole test.
    let init = container_init(root.path(), &id).expect("a container init");
    let listed = runtime.workload_pids(&id).await.unwrap();
    let paused = pause_live(&listed);
    // A write already under way when the signal landed may still finish.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let frozen = beats();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let still = beats();
    for pid in &paused {
        resume_process(*pid as i32).unwrap();
    }
    let moved = tokio::time::timeout(Duration::from_secs(10), async {
        while beats() <= still {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    runtime.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
    assert_eq!(
        frozen, still,
        "the workload kept running while {paused:?} were paused"
    );
    assert!(
        !listed.contains(&launcher),
        "the pause named the runc launcher {launcher}: {listed:?}"
    );
    assert!(
        paused.contains(&init),
        "the pause missed the workload's init {init}: {paused:?}"
    );
    moved.expect("the workload never ran again after its resume");
}

/// Pause each of `pids` that still exists, and return those it paused.
/// A workload's pids include short-lived children, such as its loop's
/// `sleep`, which may exit between the listing and the signal; Smoker's
/// pause skips those too. Any other failure is a test failure (#2315).
fn pause_live(pids: &[u32]) -> Vec<u32> {
    use reliaburger::smoker::process::{ProcessFaultError, pause_process};
    pids.iter()
        .copied()
        .filter(|pid| match pause_process(*pid as i32) {
            Ok(()) => true,
            Err(ProcessFaultError::SignalFailed {
                source: nix::errno::Errno::ESRCH,
                ..
            }) => false,
            Err(error) => panic!("cannot pause {pid}: {error}"),
        })
        .collect()
}

/// Start a container that sleeps, in a runtime rooted at `root`, and return
/// the runtime, the instance and its address.
async fn start_sleeper(root: &Path) -> (RuncGrill, InstanceId, std::net::Ipv4Addr) {
    let id = instance(root);
    let runtime = runtime(root);
    runtime
        .create(&id, &spec(root, "exec /bin/busybox sleep 600"))
        .await
        .unwrap();
    install_fixture(root, &id);
    runtime.start(&id).await.unwrap();
    let address = runtime
        .container_ip(&id)
        .await
        .expect("a container address");
    (runtime, id, address)
}

/// Whether the host still routes `address` anywhere.
fn routed(address: std::net::Ipv4Addr) -> bool {
    let output = std::process::Command::new("ip")
        .args(["route", "show", &format!("{address}/32")])
        .output()
        .unwrap();
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

/// Wait until process `pid` has gone.
async fn wait_gone(pid: u32) {
    let proc = Path::new("/proc").join(pid.to_string());
    tokio::time::timeout(Duration::from_secs(20), async {
        while proc.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} outlived its test"));
}

/// A runc test that panics with its container running leaves nothing
/// behind: no container, network namespace, veth or route (#2315). Before
/// its root carried a guard, one such panic held the first container
/// address, and every later runc test failed with `RTNETLINK answers: File
/// exists`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux"]
async fn runc_a_panicking_test_leaves_no_container_or_network_behind() {
    assert!(nix::unistd::geteuid().is_root());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let failed = tokio::spawn(async move {
        let root = runc_residue::RuncRoot::new();
        let (_runtime, id, address) = start_sleeper(root.path()).await;
        let init = container_init(root.path(), &id).expect("a container init");
        started_tx
            .send((root.path().to_path_buf(), id, address, init))
            .unwrap();
        panic!("a failed assertion with the container still running");
    })
    .await;
    assert!(failed.unwrap_err().is_panic());
    let (root, id, address, init) = started_rx.await.unwrap();
    wait_gone(init).await;
    assert!(!root.exists(), "{} survived the panic", root.display());
    assert!(!reliaburger::grill::netns::namespace_path(&id).exists());
    assert!(
        !Path::new("/sys/class/net")
            .join(reliaburger::grill::netns::host_veth_name(&id))
            .exists()
    );
    assert!(!routed(address), "{address} is still routed");
}

/// A runc test still starts after an earlier one that panicked with no
/// guard at all: its container kept running, its root was deleted, and its
/// namespace, veth and route stayed. That is what the pause test's panic
/// left (#2315). Nothing claims that namespace, so a guard's sweep releases
/// it once it is older than [`runc_residue::STALE_AFTER`] (#2332), and the
/// next container takes the same address. The sweep here is told the
/// namespace is old already rather than waiting two minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux"]
async fn runc_a_test_after_an_unguarded_panic_takes_the_same_address() {
    assert!(nix::unistd::geteuid().is_root());
    let unguarded = tempfile::tempdir().unwrap();
    let (stale_runtime, stale, stale_address) = start_sleeper(unguarded.path()).await;
    let stale_init = container_init(unguarded.path(), &stale).expect("a container init");
    drop(stale_runtime);
    drop(unguarded);
    assert!(routed(stale_address));

    let root = runc_residue::RuncRoot::new();
    assert!(
        reliaburger::grill::netns::namespace_path(&stale).exists(),
        "a young unclaimed namespace went at once"
    );
    runc_residue::sweep_stale_namespaces(Duration::ZERO);
    wait_gone(stale_init).await;
    assert!(!reliaburger::grill::netns::namespace_path(&stale).exists());
    let (runtime, id, address) = start_sleeper(root.path()).await;
    assert_eq!(address, stale_address, "the stale address never collided");
    runtime.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
}

/// A runc test still starts after an earlier one was killed outright, by a
/// timeout's SIGKILL, with its container running and its root kept. Its
/// guard's lock went with it, so the next guard releases that root's
/// container and network, and the next container takes the same address.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux"]
async fn runc_a_test_after_a_killed_test_releases_its_root_and_takes_the_same_address() {
    assert!(nix::unistd::geteuid().is_root());
    let killed = tempfile::tempdir().unwrap().keep();
    let residue = runc_residue::RuncResidue::new(&killed);
    let (stale_runtime, stale, stale_address) = start_sleeper(&killed).await;
    let stale_init = container_init(&killed, &stale).expect("a container init");
    drop(stale_runtime);
    residue.abandon();
    assert!(routed(stale_address));

    let root = runc_residue::RuncRoot::new();
    wait_gone(stale_init).await;
    assert!(
        container_init(&killed, &stale).is_none(),
        "runc still lists it"
    );
    assert!(!reliaburger::grill::netns::namespace_path(&stale).exists());
    let (runtime, id, address) = start_sleeper(root.path()).await;
    assert_eq!(address, stale_address, "the stale address never collided");
    runtime.kill(&id).await.unwrap();
    assert_absent(root.path(), &id);
    std::fs::remove_dir_all(&killed).unwrap();
}

/// Kill every process in a stand-in for Bun's systemd unit cgroup the way
/// `KillMode=control-group` does: SIGTERM, a grace period, then SIGKILL.
async fn stop_unit_cgroup(unit: &Path) {
    let members = || {
        std::fs::read_to_string(unit.join("cgroup.procs"))
            .unwrap()
            .lines()
            .map(|line| line.parse::<i32>().unwrap())
            .collect::<Vec<_>>()
    };
    for pid in members() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    std::fs::write(unit.join("cgroup.kill"), "1").unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !members().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_dir(unit).unwrap();
}

/// Issue #241: `systemctl stop` under the default `KillMode=control-group`
/// kills Bun together with every runtime owner it started, while the
/// container itself survives in its own cgroup. The restarted Bun must
/// start: adoption either takes the container back or retires it, and a
/// retained address stays held until its original reference is released.
async fn restart_after_unit_cgroup_stop(retiring: bool) {
    use reliaburger::grill::records::InstanceRecord;
    use reliaburger::grill::runc_intent::NetworkReferenceState;
    assert!(nix::unistd::geteuid().is_root());
    let root = crate::runc_residue::RuncRoot::new();
    let id = instance(root.path());
    let unit = Path::new("/sys/fs/cgroup").join(format!("{}-unit", id.0));
    std::fs::create_dir(&unit).unwrap();
    let mut caller = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "owned_runc_unit_stop_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("RELIABURGER_OWNED_RUNC_UNIT_FIXTURE", root.path())
        .env("RELIABURGER_OWNED_RUNC_UNIT_CGROUP", &unit)
        .env("RELIABURGER_OWNED_RUNC_UNIT_RETIRING", retiring.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_file(&root.path().join("shared/unit-ready")).await;
    let record: InstanceRecord =
        serde_json::from_slice(&std::fs::read(root.path().join("shared/record.json")).unwrap())
            .unwrap();
    stop_unit_cgroup(&unit).await;
    let _ = caller.wait().await;

    let restarted = runtime(root.path());
    let adopted = restarted.adopt(&id, &record).await;
    let held = restarted
        .launch_inventory()
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .find(|launch| launch.instance_id == id)
        .and_then(|launch| launch.network_reference);
    let replacement_refused = restarted
        .create(&id, &spec(root.path(), "exit 0"))
        .await
        .is_err();
    // Clean up before asserting, so a failure leaves nothing behind.
    if let Some(NetworkReferenceState::Held(reference)) = &held {
        let _ = restarted.release_network_reference(reference).await;
    }
    let _ = restarted.kill(&id).await;
    let final_state = restarted.state(&id).await;
    let container_cgroup = Path::new("/sys/fs/cgroup").join(&id.0);
    if container_cgroup.exists() {
        let _ = std::fs::write(container_cgroup.join("cgroup.kill"), "1");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _ = std::fs::remove_dir(&container_cgroup);
    }

    assert!(
        adopted.is_ok(),
        "restart after a unit-cgroup stop refused to start: {adopted:?}"
    );
    // Whatever adoption decided, discovery's address stays held until the
    // original reference is released.
    assert!(
        matches!(held, Some(NetworkReferenceState::Held(_))),
        "the retained address was dropped before its release: {held:?}"
    );
    assert!(
        replacement_refused,
        "a held address was handed to a replacement"
    );
    assert!(matches!(final_state, Ok(ContainerState::Stopped)));
    assert_absent(root.path(), &id);
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn restart_after_unit_cgroup_stop_recovers_a_running_generation() {
    restart_after_unit_cgroup_stop(false).await;
}

#[tokio::test]
#[ignore = "requires root, runc, static /usr/bin/busybox, ip and nft; run with make test-linux or scripts/release/qualify-oci-interruptions.sh"]
async fn restart_after_unit_cgroup_stop_recovers_a_retiring_generation() {
    restart_after_unit_cgroup_stop(true).await;
}

#[tokio::test]
#[ignore = "subprocess fixture for a KillMode=control-group stop"]
async fn owned_runc_unit_stop_fixture() {
    use reliaburger::grill::records::{InstanceRecord, RuntimeKind};
    let Some(root) = std::env::var_os("RELIABURGER_OWNED_RUNC_UNIT_FIXTURE") else {
        return;
    };
    let unit =
        std::path::PathBuf::from(std::env::var_os("RELIABURGER_OWNED_RUNC_UNIT_CGROUP").unwrap());
    let retiring = std::env::var("RELIABURGER_OWNED_RUNC_UNIT_RETIRING").unwrap() == "true";
    // Join the unit first, so every owner this process starts lands in it.
    std::fs::write(unit.join("cgroup.procs"), std::process::id().to_string()).unwrap();
    let root = std::path::PathBuf::from(root);
    let id = instance(&root);
    let runtime = runtime(&root);
    // Like Bun, give the container its own cgroup outside the unit.
    let mut specification = spec(&root, "exec /bin/busybox sleep 600");
    specification.linux.cgroups_path = Some(format!("/{}", id.0));
    runtime.create(&id, &specification).await.unwrap();
    install_fixture(&root, &id);
    runtime
        .retain_network_reference(&id)
        .await
        .unwrap()
        .unwrap();
    runtime.start(&id).await.unwrap();
    let pid = runtime.pid(&id).await.unwrap().unwrap();
    let record = InstanceRecord {
        schema: reliaburger::grill::records::RECORD_SCHEMA,
        instance_id: id.0.clone(),
        namespace: "default".into(),
        app_name: "owned-runc".into(),
        replica_index: 0,
        image: "/empty-fixture".into(),
        runtime: RuntimeKind::Runc,
        pid,
        pid_started_at: reliaburger::grill::records::process_start_time(pid).unwrap(),
        boot_id: reliaburger::grill::records::current_boot(),
        runc_container_id: Some(id.0.clone()),
        log_stem: runtime.log_stem(&id).await,
        host_port: None,
        app_spec: None,
        oci_spec: specification,
        rootless_network: None,
        sidecar_restarts: None,
    };
    std::fs::write(
        root.join("shared/record.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    if retiring {
        // A rollout stopped the old generation; discovery still holds its
        // address, so the intent stays Retiring.
        runtime.kill(&id).await.unwrap();
    }
    std::fs::write(root.join("shared/unit-ready"), "ready").unwrap();
    tokio::time::sleep(Duration::from_secs(600)).await;
}

#[path = "support/task_harness.rs"]
mod capacity_task_harness;

#[path = "support/fixture_dir.rs"]
mod fixture_dir;

mod capacity_contract {

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use reliaburger::bun::agent::BunAgent;
    use reliaburger::bun::api::{self, NodeMembershipInfo};
    use reliaburger::config::Config;
    use reliaburger::council::log_store::MemLogStore;
    use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use reliaburger::council::node::CouncilNode;
    use reliaburger::council::state_machine::CouncilStateMachine;
    use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
    use reliaburger::grill::image::{ClusterFetchFuture, ClusterImageSource, LocalImageBlobs};
    use reliaburger::grill::runc::RuncGrill;
    use reliaburger::grill::{ContainerState, Grill, ImageStore, InstanceId, PortAllocator};
    use reliaburger::relish::client::BunClient;
    use sha2::{Digest, Sha256};
    use tokio::sync::{RwLock, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::capacity_task_harness::TestTasks;
    const TEST_SERVICE_TOKEN: &str = "rbrg_test_service_token";

    /// One node's agent and API over the actual runc runtime.
    ///
    /// The harness owns the agent's volumes directory for its whole life.
    /// A rootful agent mounts a workload identity tmpfs beneath it, and a
    /// `TempDir` dropped over a mount fails to remove it without a word, so
    /// [`Drop`] stops the agent first, then unmounts and removes the
    /// directory with `clear_fixture_dir` (#852).
    struct Harness {
        client: BunClient,
        /// `Some` until `Drop` stops the tasks.
        tasks: Option<TestTasks>,
        _capacity_publisher:
            tokio::sync::watch::Sender<reliaburger::reporting::aggregator::AggregatedState>,
        volumes: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            // Nothing may mount beneath the volumes while they're cleared.
            drop(self.tasks.take());
            if let Err(error) = super::fixture_dir::clear_fixture_dir(&self.volumes) {
                let message = format!(
                    "capacity harness cleanup: cannot remove {}: {error}",
                    self.volumes.display()
                );
                // A second panic while unwinding aborts the binary and hides
                // the first failure.
                if std::thread::panicking() {
                    eprintln!("{message}");
                } else {
                    panic!("{message}");
                }
            }
        }
    }

    impl Harness {
        async fn start(council: Arc<CouncilNode>, grill: RuncGrill, records: PathBuf) -> Self {
            let (cmd_tx, cmd_rx) = mpsc::channel(256);
            let shutdown = CancellationToken::new();
            let mut agent = BunAgent::new(
                reliaburger::grill::AnyGrill::Runc(grill),
                PortAllocator::new(42000, 43000),
                cmd_rx,
                shutdown.clone(),
            );
            agent.set_node_capacity(8000, 16384);
            agent.set_records_dir(records);
            agent.adopt_recorded_instances().await.unwrap();
            let volumes = tempfile::tempdir().unwrap().keep();
            agent.set_volumes_dir(volumes.clone());
            let deploy_history = agent.deploy_history_handle();
            let status_reader = agent.status_reader();
            let job_data = tempfile::tempdir().unwrap();
            let runner = agent.delegated_task_runner(job_data.path()).unwrap();
            let executor = reliaburger::bun::task_array_node::TaskArrayNode::new(
                reliaburger::bun::task_array_node::TaskArrayNodeConfig::for_data_dir(
                    job_data.path(),
                    Default::default(),
                ),
                reliaburger::bun::task_array_node::NodeRunner::Owned(Box::new(runner)),
            )
            .with_budget(agent.execution_budget());
            let task_arrays = Arc::new(
                reliaburger::bun::task_array_leader::TaskArrayService::with_timings(
                    Some(Arc::new(executor)),
                    Duration::from_millis(50),
                    Duration::from_secs(3),
                )
                .with_storage(job_data.path())
                .await
                .unwrap(),
            );
            let agent_task = tokio::spawn(async move {
                agent.run().await;
                drop(agent);
                drop(job_data);
            });

            let node = reliaburger::meat::NodeId::new("node-1");
            let aggregated = reliaburger::reporting::aggregator::AggregatedState {
                leadership_epoch: Some(council.current_term()),
                receive_deadlines: [(
                    node.clone(),
                    tokio::time::Instant::now() + Duration::from_secs(30),
                )]
                .into_iter()
                .collect(),
                reports: [(
                    node.clone(),
                    reliaburger::reporting::types::StateReport {
                        node_id: node,
                        timestamp: std::time::SystemTime::UNIX_EPOCH,
                        running_apps: Vec::new(),
                        cached_specs: Vec::new(),
                        rollouts: Vec::new(),
                        resource_usage: reliaburger::reporting::types::ResourceUsage {
                            cpu_total_millicores: 8000,
                            memory_total_mb: 16384,
                            ..Default::default()
                        },
                        event_log: Vec::new(),
                        has_buildah: false,
                        job_reservations: Default::default(),
                        capacity_refusals: Vec::new(),
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            };
            let (capacity_tx, capacity_rx) = tokio::sync::watch::channel(aggregated);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let app = api::router_with_upgrade(
                cmd_tx,
                None,
                None,
                Some(deploy_history),
                None,
                None,
                Some(council),
                None,
                Some(TEST_SERVICE_TOKEN.to_string()),
                None,
                Some(Arc::new(RwLock::new(vec![NodeMembershipInfo {
                    node_id: reliaburger::meat::NodeId::new("node-1"),
                    address: "127.0.0.1:9001".parse().unwrap(),
                    api_advertised: true,
                    jobs: None,
                }]))),
                None,
                None,
                9117,
                None,
                None,
                Some(capacity_rx),
                "default".to_string(),
                Some("node-1".to_string()),
                reliaburger::bun::build_runner::BuildSettings::with_timeout(900),
                reliaburger::cluster::ClusterHttp::plaintext(),
                5050,
                "http",
                256 * 1024 * 1024,
                false,
                reliaburger::bun::capabilities::StaticCapabilities::default(),
                reliaburger::bun::readiness::ReadinessTracker::new(),
                None,
                None,
                Some(status_reader),
                Some(task_arrays),
            );
            let server_shutdown = shutdown.clone();
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(server_shutdown.cancelled_owned())
                    .await
                    .unwrap();
            });
            let tasks = TestTasks::new(shutdown, vec![agent_task, server_task]);
            let client = BunClient::new(&format!("http://127.0.0.1:{port}"));
            let mut acknowledged = false;
            for _ in 0..20 {
                if client.health().await.is_ok() {
                    acknowledged = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(
                acknowledged,
                "actual owned-capacity API did not acknowledge startup"
            );
            Self {
                client,
                tasks: Some(tasks),
                _capacity_publisher: capacity_tx,
                volumes,
            }
        }
    }
    fn fast_config() -> CouncilConfig {
        CouncilConfig {
            heartbeat_interval_ms: 50,
            election_timeout_min_ms: 150,
            election_timeout_max_ms: 400,
            snapshot_threshold: 100,
            max_in_snapshot_log_to_keep: 50,
        }
    }

    /// A single-node council, initialised so it becomes leader.
    async fn single_node_leader() -> Arc<CouncilNode> {
        let router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, router.clone());
        let node = CouncilNode::new(
            1,
            fast_config(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        router.register(1, &node).await;
        let mut members = BTreeMap::new();
        members.insert(
            1u64,
            CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1".to_string()),
        );
        node.initialize(members).await.unwrap();

        let node = Arc::new(node);
        for _ in 0..40 {
            if node.is_leader().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        node
    }

    /// Hermetic bytes, not a mocked runtime or invented terminal outcome.
    /// Actual ImageStore validates the config and unpacks the fixture layer;
    /// actual owned RuncGrill creates/starts/reaps its BusyBox child.
    struct StaticBusyBoxImage(LocalImageBlobs);

    impl ClusterImageSource for StaticBusyBoxImage {
        fn fetch_cluster_image<'a>(
            &'a self,
            repository: &'a str,
            tag: &'a str,
        ) -> ClusterFetchFuture<'a> {
            Box::pin(async move {
                if tag == "v1"
                    && (repository == "owned-capacity" || repository.ends_with("/owned-capacity"))
                {
                    Ok(Some(self.0.clone()))
                } else {
                    Err("owned-capacity fixture refuses external image fallback".into())
                }
            })
        }
    }

    fn static_busybox_store(root: &Path) -> ImageStore {
        let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (directory, mode) in [("bin", 0o755), ("work", 0o777)] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(mode);
            header.set_uid(0);
            header.set_gid(0);
            header.set_mtime(0);
            header.set_cksum();
            archive
                .append_data(&mut header, directory, std::io::empty())
                .unwrap();
        }
        let busybox = std::fs::read("/usr/bin/busybox").unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(busybox.len() as u64);
        header.set_mode(0o755);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive
            .append_data(&mut header, "bin/busybox", busybox.as_slice())
            .unwrap();
        let layer = archive.into_inner().unwrap().finish().unwrap();
        let config = br#"{"config":{"User":"65534:65534","WorkingDir":"/","Env":["PATH=/bin"]}}"#;
        let store = ImageStore::new(root.join("images"));
        let layer_digest = format!("sha256:{}", hex::encode(Sha256::digest(&layer)));
        let config_digest = format!("sha256:{}", hex::encode(Sha256::digest(config)));
        let layer_path = store.blob_path(&layer_digest);
        let config_path = store.blob_path(&config_digest);
        for (path, data) in [
            (&layer_path, layer.as_slice()),
            (&config_path, config.as_slice()),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        }
        store.set_cluster_source(Arc::new(StaticBusyBoxImage(LocalImageBlobs {
            layers: vec![layer_path],
            config: config_path,
            config_digest,
        })));
        store
    }

    /// Own every actual runtime resource in this fixture root, including the
    /// ordinary job and any subsequently admitted physical batch executions.
    /// The guard runs after the harness's acknowledged task shutdown on unwind.
    struct OwnedRuntimeCleanup {
        runtime: RuncGrill,
        armed: bool,
    }

    impl OwnedRuntimeCleanup {
        async fn finish(&mut self) {
            tokio::time::timeout(Duration::from_secs(20), async {
                for entry in self.runtime.launch_inventory().await.unwrap().unwrap() {
                    self.runtime.kill(&entry.instance_id).await.unwrap();
                    assert_eq!(
                        self.runtime.state(&entry.instance_id).await.unwrap(),
                        ContainerState::Stopped
                    );
                    assert!(
                        !reliaburger::grill::netns::namespace_path(&entry.instance_id).exists()
                    );
                    assert!(
                        !Path::new("/sys/class/net")
                            .join(reliaburger::grill::netns::host_veth_name(
                                &entry.instance_id
                            ))
                            .exists()
                    );
                }
            })
            .await
            .expect("owned-capacity runtime cleanup exceeded its existing owned-fixture bound");
            self.armed = false;
        }
    }

    impl Drop for OwnedRuntimeCleanup {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            let runtime = self.runtime.clone();
            let cleaned = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    tokio::time::timeout(Duration::from_secs(20), async move {
                        let Some(entries) = runtime.launch_inventory().await.ok().flatten() else {
                            return false;
                        };
                        for entry in entries {
                            if runtime.kill(&entry.instance_id).await.is_err() {
                                return false;
                            }
                        }
                        true
                    })
                    .await
                    .unwrap_or(false)
                })
            });
            if !cleaned {
                eprintln!(
                    "owned-capacity fixture cleanup remains unconfirmed; retain failed receipt"
                );
            }
        }
    }

    struct CouncilCleanup(Arc<CouncilNode>);

    impl Drop for CouncilCleanup {
        fn drop(&mut self) {
            let node = self.0.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(10), node.shutdown()).await;
                });
            });
        }
    }

    struct ReleaseOnDrop(PathBuf);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"release");
        }
    }

    /// How long one step of the capacity contract may take: an apply, an
    /// executor's cold start or a batch's retirement. Each takes about a
    /// second alone, but a cold owned-runc start runs some twenty supervised
    /// `ip`, `nft` and `sysctl` commands and a synced image unpack, one
    /// after another. Beside a full `make test-linux`, or in an 8-vCPU VM at
    /// load 17 with two fsync writers, one start took 5 to 25 s and a single
    /// `ip link` 7.5 s, so a 20 s wait failed while nothing was wrong
    /// (#2267). The limit only has to catch a step that never comes.
    const STEP_DEADLINE: Duration = Duration::from_secs(90);

    /// Wait until the singleton's command has written `work/ready` inside
    /// its rootfs. Fails at once if the runtime retires the instance first,
    /// and says how far the start got if the deadline passes.
    async fn wait_for_singleton_ready(
        runtime: &RuncGrill,
        root: &Path,
        identity: &InstanceId,
    ) -> Result<(), String> {
        let rootfs = root.join("bundles").join(&identity.0).join("rootfs");
        let started = std::time::Instant::now();
        while started.elapsed() < STEP_DEADLINE {
            if rootfs.join("work/ready").exists() {
                return Ok(());
            }
            let launched = runtime
                .launch_inventory()
                .await
                .map_err(|error| format!("launch inventory unavailable: {error}"))?
                .unwrap_or_default()
                .iter()
                .any(|launch| launch.instance_id == *identity);
            if !launched {
                return Err(format!(
                    "{} left the launch inventory after {:?} without writing work/ready",
                    identity.0,
                    started.elapsed()
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(format!(
            "{} not ready after {STEP_DEADLINE:?}: network namespace {}, rootfs mounted {}, runc state {}",
            identity.0,
            reliaburger::grill::netns::namespace_path(identity).exists(),
            rootfs.join("bin/busybox").exists(),
            root.join("state").join(&identity.0).exists(),
        ))
    }

    fn fresh_capacity(harness: &Harness, term: u64) {
        harness._capacity_publisher.send_modify(|state| {
            state.leadership_epoch = Some(term);
            for deadline in state.receive_deadlines.values_mut() {
                *deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            }
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires root, runc, overlayfs, cgroup v2, static /usr/bin/busybox, ip and nft; run with scripts/release/qualify-oci-interruptions.sh"]
    async fn runc_ordinary_terminal_receipt_releases_nonzero_capacity_for_actual_batch_http() {
        assert!(nix::unistd::geteuid().is_root());
        assert!(Path::new("/sys/fs/cgroup/cgroup.controllers").is_file());
        let root = crate::runc_residue::RuncRoot::new();
        let suffix = root
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches('.')
            .to_lowercase();
        let namespace = format!("cap-{suffix}");
        let runtime = RuncGrill::new(
            root.path().join("bundles"),
            static_busybox_store(root.path()),
            false,
            root.path().join("state"),
            env!("CARGO_BIN_EXE_bun").into(),
        )
        .unwrap();
        let mut cleanup = OwnedRuntimeCleanup {
            runtime: runtime.clone(),
            armed: true,
        };
        let council = single_node_leader().await;
        let _council_cleanup = CouncilCleanup(council.clone());
        assert!(council.is_leader().await);
        let records = root.path().join("records");
        let harness = Harness::start(council.clone(), runtime.clone(), records.clone()).await;
        let ordinary = Config::parse(&format!(
            "[job.ordinary]\nimage='owned-capacity:v1'\nnamespace='{namespace}'\ncpu='1'\nmemory='128Mi'\n"
        )).unwrap();
        let mut ordinary = ordinary;
        ordinary.job.get_mut("ordinary").unwrap().command = Some(vec![
            "/bin/busybox".into(), "sh".into(), "-c".into(),
            r#"/bin/busybox touch /work/ready; attempts=0; while [ ! -e /work/release ]; do attempts=$((attempts+1)); [ "$attempts" -le 3000 ] || exit 75; /bin/busybox sleep 0.02; done; /bin/busybox touch /work/released; exit 0"#.into(),
        ]);
        assert_eq!(ordinary.job["ordinary"].cpu.unwrap().request, 1000);
        assert_eq!(
            ordinary.job["ordinary"].memory.unwrap().request,
            128 * 1024 * 1024
        );
        tokio::time::timeout(STEP_DEADLINE, harness.client.apply(&ordinary))
            .await
            .expect("real ordinary apply did not acknowledge")
            .unwrap();
        let mut identity = None;
        tokio::time::timeout(STEP_DEADLINE, async {
            while identity.is_none() {
                identity = runtime
                    .launch_inventory()
                    .await
                    .unwrap()
                    .unwrap()
                    .into_iter()
                    .find(|launch| {
                        launch
                            .instance_id
                            .0
                            .starts_with(&format!("{namespace}__executor-"))
                    })
                    .map(|launch| launch.instance_id);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let identity = identity.unwrap();
        let rootfs = root.path().join("bundles").join(&identity.0).join("rootfs");
        let release = ReleaseOnDrop(rootfs.join("work/release"));
        if let Err(reason) = wait_for_singleton_ready(&runtime, root.path(), &identity).await {
            panic!("owned singleton did not start: {reason}");
        }
        assert_eq!(
            runtime.state(&identity).await.unwrap(),
            ContainerState::Running
        );
        let desired = council.desired_state().await;
        let ordinary_id = desired
            .task_arrays
            .jobs()
            .runs()
            .find(|(_, run)| run.name == "ordinary" && run.namespace == namespace)
            .unwrap()
            .0;
        assert_eq!(
            desired
                .task_arrays
                .get(ordinary_id)
                .unwrap()
                .state
                .spec
                .count,
            1
        );
        assert!(
            !desired
                .task_arrays
                .jobs()
                .run(ordinary_id)
                .unwrap()
                .replay_unknown
        );

        let probe = |name: &str, cpu: &str, memory: &str| {
            Config::parse(&format!(
            "[job.{name}]\nimage='owned-capacity:v1'\nnamespace='{namespace}'\ncommand=['/bin/busybox','true']\ncpu='{cpu}'\nmemory='{memory}'\n"
        )).unwrap().job
        };
        // Each request fits the empty node but must wait for the singleton's
        // exact runtime retirement. Test each resource dimension independently.
        let mut pending = Vec::new();
        for (name, cpu, memory) in [
            ("full-before-terminal", "8", "16Gi"),
            ("cpu-before-terminal", "8", "128Mi"),
            ("memory-before-terminal", "100m", "16Gi"),
        ] {
            fresh_capacity(&harness, council.current_term());
            let response = harness
                .client
                .submit_batch(&probe(name, cpu, memory), None)
                .await
                .unwrap();
            assert_eq!(
                response["assigned"], 1,
                "admitted work waits locally: {response}"
            );
            let id = response["batch_id"].as_u64().unwrap();
            pending.push(id);
            tokio::time::sleep(Duration::from_millis(100)).await;
            let summary = harness.client.batch_status(id).await.unwrap();
            assert_eq!(summary["succeeded"], 0);
            assert_eq!(summary["done"], false);
            assert!(
                summary["cohorts"][0]["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|node| node["counters"]["attempts_started"] == 0)
            );
            assert_eq!(
                runtime.state(&identity).await.unwrap(),
                ContainerState::Running
            );
        }
        std::fs::write(&release.0, b"release").unwrap();
        for id in std::iter::once(ordinary_id).chain(pending) {
            let started = std::time::Instant::now();
            let summary = loop {
                let summary = harness.client.batch_status(id).await.unwrap();
                if summary["done"] == true || started.elapsed() >= STEP_DEADLINE {
                    break summary;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            assert_eq!(
                summary["done"], true,
                "positive owned retirement did not free waiting work: {summary}"
            );
            assert_eq!(summary["succeeded"], 1, "{summary}");
            assert_eq!(summary["failed"], 0, "{summary}");
        }

        fresh_capacity(&harness, council.current_term());
        let admitted = tokio::time::timeout(
            STEP_DEADLINE,
            harness
                .client
                .submit_batch(&probe("full-after-terminal", "8", "16Gi"), None),
        )
        .await
        .expect("released capacity admission did not settle")
        .unwrap();
        assert_eq!(admitted["assigned"], 1, "{admitted}");
        assert_eq!(admitted["unschedulable"], serde_json::json!([]));
        let batch_id = admitted["batch_id"].as_u64().unwrap();
        tokio::time::timeout(STEP_DEADLINE, async {
            loop {
                let summary = harness.client.batch_status(batch_id).await.unwrap();
                if summary["done"] == true {
                    assert_eq!(summary["succeeded"], 1, "{summary}");
                    assert_eq!(summary["failed"], 0, "{summary}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("actual newly admitted batch child did not terminate");
        drop(release);
        drop(harness);
        cleanup.finish().await;
    }

    /// Start a capacity harness on a runtime with no images, for tests of
    /// the harness itself.
    async fn bare_harness(root: &Path) -> (Harness, CouncilCleanup) {
        let runtime = RuncGrill::new(
            root.join("bundles"),
            ImageStore::new(root.join("images")),
            false,
            root.join("state"),
            env!("CARGO_BIN_EXE_bun").into(),
        )
        .unwrap();
        let council = single_node_leader().await;
        let cleanup = CouncilCleanup(council.clone());
        let harness = Harness::start(council, runtime, root.join("records")).await;
        (harness, cleanup)
    }

    /// Mount workload identity tmpfs beneath `volumes` the way the agent
    /// does for a rootful workload, one nested inside the other.
    fn mount_identity(volumes: &Path) {
        let outer = reliaburger::sesame::identity::instance_identity_dir(volumes, "a");
        for dir in [&outer, &outer.join("nested")] {
            reliaburger::sesame::identity::prepare_identity_dir(dir).unwrap();
        }
        assert_eq!(mounts_under(volumes).len(), 2, "identity tmpfs not mounted");
    }

    /// The mount points `/proc/self/mountinfo` lists at or beneath `root`.
    fn mounts_under(root: &Path) -> Vec<PathBuf> {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        super::fixture_dir::mounts_beneath(&mountinfo, root)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires root, runc, overlayfs, cgroup v2, ip and nft; run with scripts/release/qualify-oci-interruptions.sh"]
    async fn runc_capacity_harness_unmounts_identity_tmpfs_and_removes_its_volumes() {
        assert!(nix::unistd::geteuid().is_root());
        let root = crate::runc_residue::RuncRoot::new();
        let (harness, _council) = bare_harness(root.path()).await;
        let volumes = harness.volumes.clone();
        mount_identity(&volumes);
        drop(harness);
        assert_eq!(mounts_under(&volumes), Vec::<PathBuf>::new());
        assert!(
            !volumes.exists(),
            "{} survived the harness",
            volumes.display()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires root, runc, overlayfs, cgroup v2, ip and nft; run with scripts/release/qualify-oci-interruptions.sh"]
    async fn runc_capacity_harness_unmounts_identity_tmpfs_while_a_failed_test_unwinds() {
        assert!(nix::unistd::geteuid().is_root());
        let root = crate::runc_residue::RuncRoot::new();
        let (volumes_tx, volumes_rx) = tokio::sync::oneshot::channel();
        let root_path = root.path().to_path_buf();
        let failed = tokio::spawn(async move {
            let (harness, _council) = bare_harness(&root_path).await;
            mount_identity(&harness.volumes);
            volumes_tx.send(harness.volumes.clone()).unwrap();
            panic!("a failed assertion with identity tmpfs still mounted");
        })
        .await;
        assert!(failed.unwrap_err().is_panic());
        let volumes = volumes_rx.await.unwrap();
        assert_eq!(mounts_under(&volumes), Vec::<PathBuf>::new());
        assert!(
            !volumes.exists(),
            "{} survived the harness",
            volumes.display()
        );
    }
}
