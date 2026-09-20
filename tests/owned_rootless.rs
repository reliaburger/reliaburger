//! Real rootless OCI ownership, independent of Bun adoption records.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use reliaburger::grill::runc::RuncGrill;
use reliaburger::grill::{ContainerState, Grill, ImageStore, InstanceId, OciSpec};

fn runtime(root: &Path) -> RuncGrill {
    RuncGrill::new(
        root.join("bundles"),
        ImageStore::new(root.join("images")),
        true,
        root.join("state"),
    )
    .with_owner(if root.join("bun-wrapper").exists() {
        root.join("bun-wrapper")
    } else {
        env!("CARGO_BIN_EXE_bun").into()
    })
    .unwrap()
}

fn specification(root: &Path, script: &str) -> OciSpec {
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

async fn read_page(url: &str) -> String {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.get(url).send().await {
                Ok(response) => return response.error_for_status().unwrap().text().await.unwrap(),
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap()
}

async fn stopped(runtime: &RuncGrill, id: &InstanceId) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while runtime.state(id).await.unwrap() != ContainerState::Stopped {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires unprivileged Linux user, rootless runc, slirp4netns and static busybox"]
async fn rootless_short_job_has_network_before_its_first_instruction() {
    assert!(!nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let id = InstanceId("rootless-short".into());
    let first = runtime(root.path());
    first
        .create(
            &id,
            &specification(
                root.path(),
                "/bin/busybox ip link show tap0 >/dev/null || exit 91; printf network-ready; exit 7",
            ),
        )
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    first.start(&id).await.unwrap();
    drop(first);
    let recovered = runtime(root.path());
    stopped(&recovered, &id).await;
    assert_eq!(recovered.exit_code(&id).await, Some(7));
    assert!(recovered.logs(&id).await.unwrap().contains("network-ready"));
    assert!(!root.path().join("state").join(&id.0).exists());
}

#[tokio::test]
#[ignore = "requires unprivileged Linux user, rootless runc, slirp4netns and static busybox"]
async fn rootless_port_and_launcher_survive_recovery_and_helper_replacement() {
    use reliaburger::grill::oci::PortMapping;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("long-data-path-".repeat(12));
    std::fs::create_dir(&data).unwrap();
    let id = InstanceId("rootless-port".into());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut spec = specification(
        &data,
        "printf launch >> /work/launches; exec /bin/busybox httpd -f -p 8080 -h /work",
    );
    spec.port_mapping = Some(PortMapping {
        host_port: port,
        container_port: 8080,
    });
    std::fs::write(data.join("shared/index.html"), "owned-rootless").unwrap();
    let first = runtime(&data);
    first.create(&id, &spec).await.unwrap();
    install_fixture(&data, &id);
    first.start(&id).await.unwrap();
    let launcher = first.pid(&id).await.unwrap();
    let network = first.rootless_network_record(&id).await.unwrap();
    let url = format!("http://127.0.0.1:{port}");
    assert_eq!(read_page(&url).await, "owned-rootless");
    drop(first);
    let recovered = runtime(&data);
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Running);
    assert_eq!(recovered.pid(&id).await, Some(launcher));
    // Fault injection against a just-observed helper; production recovery uses its owner.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(network.owner_pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            recovered.state(&id).await.unwrap();
            if recovered
                .rootless_network_record(&id)
                .await
                .is_some_and(|record| record.owner_pid != network.owner_pid)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(recovered.pid(&id).await, Some(launcher));
    assert_eq!(
        std::fs::read_to_string(data.join("shared/launches")).unwrap(),
        "launch"
    );
    assert_eq!(read_page(&url).await, "owned-rootless");
    let socket = recovered
        .rootless_network_record(&id)
        .await
        .unwrap()
        .api_socket;
    recovered.kill(&id).await.unwrap();
    assert!(!socket.exists());
    assert!(!data.join("state").join(&id.0).exists());
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires unprivileged Linux user, rootless runc, slirp4netns and static busybox"]
async fn rootless_caller_death_recovers_without_an_adoption_record() {
    let root = tempfile::tempdir().unwrap();
    let mut caller = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "owned_rootless_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("RELIABURGER_OWNED_ROOTLESS_FIXTURE", root.path())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !root.path().join("ready").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    caller.kill().await.unwrap();
    caller.wait().await.unwrap();
    let recovered = runtime(root.path());
    let inventory = recovered.launch_inventory().await.unwrap().unwrap();
    assert_eq!(inventory.len(), 1);
    let id = &inventory[0].instance_id;
    assert_eq!(recovered.state(id).await.unwrap(), ContainerState::Running);
    assert_eq!(
        recovered
            .exec(
                id,
                &["/bin/busybox".into(), "echo".into(), "recovered".into()]
            )
            .await
            .unwrap()
            .trim(),
        "recovered"
    );
    recovered.kill(id).await.unwrap();
    assert_eq!(recovered.state(id).await.unwrap(), ContainerState::Stopped);
}

#[tokio::test]
#[ignore = "subprocess fixture for rootless caller death"]
async fn owned_rootless_fixture() {
    let Some(root) = std::env::var_os("RELIABURGER_OWNED_ROOTLESS_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let id = InstanceId("rootless-crash".into());
    let blocked = std::env::var_os("RELIABURGER_ROOTLESS_BLOCK_HELPER").is_some();
    if blocked {
        use std::os::unix::fs::PermissionsExt;
        let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
        std::fs::write(root.join("bun-wrapper"), format!("#!/bin/sh\nif [ \"$1\" = __rootless-network ]; then\n  : > {}\n  while [ ! -e {} ]; do sleep 0.02; done\nfi\nexec {} \"$@\"\n", quote(&root.join("helper-entered")), quote(&root.join("release-helper")), quote(Path::new(env!("CARGO_BIN_EXE_bun"))))).unwrap();
        std::fs::set_permissions(
            root.join("bun-wrapper"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let runtime = runtime(&root);
    runtime
        .create(
            &id,
            &specification(
                &root,
                "printf launched > /work/launched; exec /bin/busybox sleep 60",
            ),
        )
        .await
        .unwrap();
    install_fixture(&root, &id);
    runtime.start(&id).await.unwrap();
    std::fs::write(root.join("ready"), "ready").unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test]
#[ignore = "requires unprivileged Linux user, rootless runc, slirp4netns and static busybox"]
async fn caller_death_during_rootless_helper_startup_cannot_release_the_payload_later() {
    let root = tempfile::tempdir().unwrap();
    let mut caller = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "owned_rootless_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("RELIABURGER_OWNED_ROOTLESS_FIXTURE", root.path())
        .env("RELIABURGER_ROOTLESS_BLOCK_HELPER", "1")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !root.path().join("helper-entered").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    caller.kill().await.unwrap();
    caller.wait().await.unwrap();
    let recovered = runtime(root.path());
    let id = InstanceId("rootless-crash".into());
    recovered.kill(&id).await.unwrap();
    std::fs::write(root.path().join("release-helper"), "continue").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(recovered.state(&id).await.unwrap(), ContainerState::Stopped);
    assert!(!root.path().join("shared/launched").exists());
    assert!(!root.path().join("state").join(&id.0).exists());
}

#[test]
fn rootless_helper_refuses_the_host_namespace_without_starting_slirp() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.path().join("api.sock");
    let error = reliaburger::grill::rootless::run_owned_helper(
        &root.path().join("missing-launcher"),
        std::process::id(),
        &socket,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("namespaces do not belong"),
        "{error}"
    );
    assert!(!socket.exists());
}

#[tokio::test]
#[ignore = "requires unprivileged Linux user, rootless runc, slirp4netns and static busybox"]
async fn helper_refuses_a_container_belonging_to_a_different_live_owner() {
    use reliaburger::grill::command::OwnedCommands;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let id = InstanceId("rootless-parent-check".into());
    let runtime = runtime(root.path());
    runtime
        .create(
            &id,
            &specification(root.path(), "exec /bin/busybox sleep 60"),
        )
        .await
        .unwrap();
    install_fixture(root.path(), &id);
    runtime.start(&id).await.unwrap();
    let network = runtime.rootless_network_record(&id).await.unwrap();
    let other = OwnedCommands::new(root.path().join("other"), env!("CARGO_BIN_EXE_bun").into());
    let attempt = other
        .prepare(Path::new("/bin/sleep"), &["60".into()], &BTreeMap::new())
        .await
        .unwrap();
    other.start(&attempt).await.unwrap();
    let stem = other.log_stem(&attempt).unwrap();
    let api_directory = root.path().join("other-api");
    std::fs::create_dir(&api_directory).unwrap();
    std::fs::set_permissions(&api_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = api_directory.join("api.sock");
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bun"))
            .arg("__rootless-network")
            .arg("--launcher")
            .arg(stem.parent().unwrap())
            .arg("--container-pid")
            .arg(network.container_pid.to_string())
            .arg("--api-socket")
            .arg(&socket)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    other
        .retire(&attempt, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("namespaces do not belong"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!socket.exists());
    assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Running);
    assert_eq!(
        runtime
            .rootless_network_record(&id)
            .await
            .unwrap()
            .owner_pid,
        network.owner_pid
    );
    runtime.kill(&id).await.unwrap();
}
