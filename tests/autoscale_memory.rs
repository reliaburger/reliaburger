//! Memory autoscaling from the real metrics path, on a real Runc node.
//!
//! Memory autoscaling needs a memory request, and only a runtime that can
//! enforce the limit accepts one (ProcessGrill refuses it), so unlike the CPU
//! acceptance in `tests/placement.rs` this one runs on runc. One secure,
//! single-node Bun collects the replica's resident memory in its own
//! collection loop, ships the per-minute rollup to itself as leader, and the
//! leader's autoscaler scales the app. Nothing writes metric rows by hand.
//!
//! Needs root and runc, so it runs under `make test-linux` only.
#![cfg(target_os = "linux")]

#[path = "support/bun_process.rs"]
mod bun_process;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bun_process::{
    BunProcess, assert_success, reserve_address, reserve_ports, run_relish,
    spawn_bun_with_runtime_port_retry, wait_for_relish,
};
use reliaburger::relish::client::BunClient;
use reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;

/// The autoscaled app. Cleanup removes every instance named after it.
const APP: &str = "grower";

/// Stops Bun and removes everything it leaves on the host, including while a
/// failed assertion unwinds: the replicas' runc containers, network
/// namespaces, host veths (and so their host routes) and cgroups. A leftover
/// host route makes the next test that draws the same container address
/// fail with "File exists". Best-effort: nothing here may panic.
struct Cleanup {
    root: PathBuf,
    bun: Option<BunProcess>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        drop(self.bun.take());
        kill_root_processes(&self.root);
        delete_runc_containers(&self.root.join("data/instances/runc/state"));
        remove_network(&format!("default__{APP}"));
        remove_cgroups(Path::new("/sys/fs/cgroup/reliaburger/default"), APP);
    }
}

/// SIGKILL every process whose command line names a path under the root:
/// Bun and the detached owners it launched.
fn kill_root_processes(root: &Path) {
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::{Pid, getpgid, getpgrp};
    use std::os::unix::ffi::OsStrExt;
    let mut needle = root.as_os_str().as_bytes().to_vec();
    needle.push(b'/');
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(command) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if !command.windows(needle.len()).any(|part| part == needle) {
            continue;
        }
        let pid = Pid::from_raw(pid);
        match getpgid(Some(pid)) {
            Ok(group) if group != getpgrp() => {
                let _ = killpg(group, Signal::SIGKILL);
            }
            _ => {
                let _ = kill(pid, Signal::SIGKILL);
            }
        }
    }
    std::thread::sleep(Duration::from_millis(500));
}

fn delete_runc_containers(state: &Path) {
    let Ok(listed) = std::process::Command::new("runc")
        .arg("--root")
        .arg(state)
        .args(["list", "--quiet"])
        .output()
    else {
        return;
    };
    for id in String::from_utf8_lossy(&listed.stdout).lines() {
        let _ = std::process::Command::new("runc")
            .arg("--root")
            .arg(state)
            .args(["delete", "--force", id])
            .output();
    }
}

/// Delete the namespaces and host veths of every instance whose id starts
/// with `prefix`.
fn remove_network(prefix: &str) {
    use reliaburger::grill::{InstanceId, netns};
    for entry in std::fs::read_dir("/run/netns")
        .into_iter()
        .flatten()
        .flatten()
    {
        let namespace = entry.file_name().to_string_lossy().into_owned();
        let Some(instance) = namespace.strip_prefix("rb-") else {
            continue;
        };
        if !instance.starts_with(prefix) {
            continue;
        }
        let veth = netns::host_veth_name(&InstanceId(instance.to_owned()));
        let _ = std::process::Command::new("ip")
            .args(["link", "del", &veth])
            .output();
        let _ = std::process::Command::new("ip")
            .args(["netns", "del", &namespace])
            .output();
    }
}

/// Kill and remove, bottom-up, every cgroup under `parent` named after `app`.
fn remove_cgroups(parent: &Path, app: &str) {
    for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
        if entry.file_name().to_string_lossy().starts_with(app) {
            remove_cgroup_tree(&entry.path());
        }
    }
}

fn remove_cgroup_tree(path: &Path) {
    let _ = std::fs::write(path.join("cgroup.kill"), "1");
    for child in std::fs::read_dir(path).into_iter().flatten().flatten() {
        if child.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_cgroup_tree(&child.path());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::remove_dir(path).is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Running replicas of `app` on the node.
async fn running_replicas(client: &BunClient, app: &str) -> usize {
    client
        .status()
        .await
        .map(|statuses| {
            statuses
                .iter()
                .filter(|status| status.app_name == app && status.state == "running")
                .count()
        })
        .unwrap_or(0)
}

/// A replica whose shell holds 48 MiB of resident memory, against a 16 MiB
/// request and a 50% target, is six times over target, so the autoscaler
/// must scale from `min = 1` to `max = 2`. The collector samples the
/// container's main process (the shell), which is where the string lives.
///
/// Rollups cover the previous complete minute and the autoscaler evaluates
/// every 30 seconds, so the signal reaches a decision 60–150 s after the
/// replica starts; hence the long deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and runc; run with make test-linux"]
async fn runc_autoscaler_scales_up_on_real_memory_growth() {
    assert!(
        nix::unistd::geteuid().is_root(),
        "run this acceptance as root (make test-linux does)"
    );
    let root = tempfile::tempdir().unwrap();
    // Declared after `root`, so it runs before the directory is removed.
    let mut cleanup = Cleanup {
        root: root.path().to_path_buf(),
        bun: None,
    };
    let cluster_dir = root.path().join("cluster");
    assert_success(
        &run_relish(&[
            "init",
            cluster_dir.to_str().unwrap(),
            "--cluster-name",
            "memory-autoscale",
            "--node-id",
            "node-01",
        ]),
        "initialise memory autoscale fixture",
    );
    let node_path = cluster_dir.join("reliaburger.toml");
    let mut node = reliaburger::config::NodeConfig::from_file(&node_path).unwrap();
    node.node.name = Some("node-01".into());
    node.network.advertise_address = Some("127.0.0.1".into());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;
    node.images.mirrors = reliaburger::testkit::pinned_images::local_test_mirrors().unwrap();
    // Sample every second so the first complete rollup minute is dense.
    node.metrics.collection_interval_secs = 1;
    node.metrics.rollup_interval_secs = 10;
    let (bun, address) = spawn_bun_with_runtime_port_retry(true, "runc", || {
        let [gossip, raft, reporting] = reserve_ports();
        node.cluster.gossip_port = gossip;
        node.cluster.raft_port = raft;
        node.cluster.reporting_port = reporting;
        std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();
        (
            node_path.clone(),
            reserve_address(),
            root.path().join("memory-autoscale-bun.log"),
        )
    });
    let bun = cleanup.bun.insert(bun);
    let endpoint = format!("https://{address}");
    let ca = cluster_dir.join("identity/root-ca.crt");
    let ca_arg = ca.to_str().unwrap();
    wait_for_relish(
        bun,
        &["--endpoint", &endpoint, "--ca-cert", ca_arg, "status"],
    );
    let token = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca_arg,
        "token",
        "create",
        "--name",
        "memory-autoscale-admin",
        "--role",
        "admin",
    ]);
    assert_success(&token, "create memory autoscale administrator");
    let token = String::from_utf8(token.stdout).unwrap();
    let client =
        BunClient::new_with_ca(&endpoint, Some(token.trim()), &std::fs::read(&ca).unwrap())
            .unwrap();

    // `yes | head -c` avoids NUL bytes, which a shell drops from a command
    // substitution. The loop keeps the shell, and the string, alive.
    let manifest = reliaburger::config::Config::parse(&format!(
        r#"
        [app.{APP}]
        image = "{PINNED_TEST_WORKLOAD_IMAGE}"
        command = ["sh", "-c", "x=$(yes | head -c 50331648); while :; do sleep 5; done"]
        replicas = 1
        memory = "16Mi-256Mi"

        [app.{APP}.autoscale]
        metric = "memory"
        target = "50%"
        min = 1
        max = 2
        evaluation_window = "3m"
        cooldown = "0s"
        "#
    ))
    .unwrap();
    let applied = tokio::time::timeout(Duration::from_secs(300), client.apply(&manifest)).await;
    assert!(
        matches!(applied, Ok(Ok(_))),
        "apply failed: {applied:?}\n{}",
        log(&bun.log_path)
    );

    let placed = Instant::now() + Duration::from_secs(180);
    while running_replicas(&client, APP).await < 1 {
        bun.assert_running();
        assert!(
            Instant::now() < placed,
            "the first replica never ran\n{}",
            log(&bun.log_path)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let deadline = Instant::now() + Duration::from_secs(240);
    while running_replicas(&client, APP).await < 2 {
        bun.assert_running();
        assert!(
            Instant::now() < deadline,
            "the autoscaler never scaled on memory; status: {:?}\n{}",
            client.status().await,
            log(&bun.log_path)
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let _ = tokio::time::timeout(Duration::from_secs(30), client.delete(APP, "default")).await;
}

fn log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}
