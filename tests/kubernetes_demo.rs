//! Z1.5: the tutorial's Kubernetes demo runs on a real Runc node.
//!
//! One real Bun, with the runc runtime, eBPF service discovery, the `.internal`
//! DNS responder and ingress, applies `examples/kubernetes/podinfo.yaml` with
//! `relish apply -f`. The frontend, reached through the ingress by its host
//! name, must reach the backend as `backend` and redis as `redis`: the short
//! Kubernetes names (Z1.2), the images' own entrypoints, users and working
//! directories (Z1.1), and the import (Z1.3, Z1.4) all have to work for that.
//!
//! Needs root, runc, nftables, bpffs, a cgroup v2 host and internet access
//! for the image pulls, so it runs in the provisioned Linux VM only.
#![cfg(all(target_os = "linux", feature = "ebpf", feature = "kubernetes"))]

#[path = "support/bun_process.rs"]
mod bun_process;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bun_process::{BunProcess, BunStart, reserve_address, wait_for_bind};
use reliaburger::relish::client::BunClient;

const APPS: [&str; 3] = ["frontend", "backend", "redis"];

/// Stops the demo and removes what it leaves on the host, including while a
/// failed assertion unwinds. Best-effort: nothing here may panic.
struct Cleanup {
    root: PathBuf,
    api: Option<std::net::SocketAddr>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(address) = self.api {
            let _ = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()?;
                        runtime.block_on(async {
                            let client = BunClient::new(&format!("http://{address}"));
                            for app in APPS {
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(30),
                                    client.stop(app, "default"),
                                )
                                .await;
                            }
                        });
                        Ok::<_, std::io::Error>(())
                    })
                    .join()
            });
        }
        kill_root_processes(&self.root);
        delete_runc_containers(&self.root.join("data/instances/runc/state"));
        retire_kernel(&self.root);
        for app in APPS {
            remove_network(&format!("default__{app}"));
            remove_cgroup(&Path::new("/sys/fs/cgroup/reliaburger/default").join(app));
        }
    }
}

/// A podinfo demo applied to one real Runc node, cleaned up when dropped.
struct Demo {
    root: PathBuf,
    api: std::net::SocketAddr,
    /// Talks to the ingress by the demo's host name.
    http: reqwest::Client,
    /// `http://podinfo.localhost:<ingress port>`.
    base: String,
    // Field order is drop order: stop Bun, then clean up after it.
    _bun: BunProcess,
    _cleanup: Cleanup,
}

impl Demo {
    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("bun.log")).unwrap_or_default()
    }

    fn client(&self) -> BunClient {
        BunClient::new(&format!("http://{}", self.api))
    }

    /// The frontends' recent log lines about the cache, for the record.
    async fn frontend_cache_logs(&self) -> String {
        let options = reliaburger::relish::client::LogOptions {
            tail: Some(12),
            follow: false,
            grep: Some("cache".to_string()),
            start: None,
            json_field: None,
        };
        self.client()
            .logs("frontend", "default", &options)
            .await
            .unwrap_or_else(|error| format!("(logs unavailable: {error})"))
    }

    /// Write a value through the frontend into redis and read it back.
    async fn cache_round_trip(&self, value: &str) -> Result<String, String> {
        let stored = self
            .http
            .post(format!("{}/cache/demo", self.base))
            .body(value.to_string())
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !stored.status().is_success() {
            let status = stored.status();
            let body = stored.text().await.unwrap_or_default();
            return Err(format!("store: {status} {body}"));
        }
        let read = self
            .http
            .get(format!("{}/cache/demo", self.base))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = read.status();
        let body = read.text().await.unwrap_or_default();
        if status.is_success() && body.contains(value) {
            Ok(body)
        } else {
            Err(format!("read: {status} {body}"))
        }
    }
}

/// Start one Runc Bun with eBPF, DNS and ingress, apply the podinfo manifest
/// and wait until the frontend answers and reaches the backend and redis by
/// name. `extra_config` is appended to the node config.
async fn start_demo(extra_config: &str) -> Demo {
    assert!(nix::unistd::geteuid().is_root(), "run as root");
    let root = tempfile::tempdir().unwrap().keep();
    let mut cleanup = Cleanup {
        root: root.clone(),
        api: None,
    };
    let ingress = reserve_address().port();
    let config = root.join("node.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"
[images]
registry_bind = "127.0.0.1"
registry_port = 0
[ebpf]
enabled = true
[dns]
enabled = true
listen = "0.0.0.0:53"
[ingress]
enabled = true
http_port = {ingress}
https_port = {https}
{extra_config}
"#,
            root = root.display(),
            https = reserve_address().port(),
        ),
    )
    .unwrap();

    let mut bun = BunProcess::spawn_runtime(
        &config,
        "127.0.0.1:0".parse().unwrap(),
        false,
        root.join("bun.log"),
        "runc",
    );
    let BunStart::Ready(api) = wait_for_bind(&mut bun, "127.0.0.1:0".parse().unwrap()) else {
        panic!("bun lost a port race on an ephemeral API port");
    };
    cleanup.api = Some(api);

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/kubernetes/podinfo.yaml");
    let applied = tokio::time::timeout(
        Duration::from_secs(600),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(["apply", "-f", manifest.to_str().unwrap()])
            .env("RELIABURGER_ENDPOINT", format!("http://{api}"))
            .env_remove("RELIABURGER_TOKEN")
            .env_remove("RELIABURGER_CA_CERT")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("apply finished")
    .unwrap();
    let log = || std::fs::read_to_string(root.join("bun.log")).unwrap_or_default();
    assert!(
        applied.status.success(),
        "apply failed:\n{}\n{}\nbun log:\n{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr),
        log()
    );

    // Through the ingress, by host name, the way the tutorial's browser does.
    let http = reqwest::Client::builder()
        .resolve(
            "podinfo.localhost",
            std::net::SocketAddr::from(([127, 0, 0, 1], ingress)),
        )
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("http://podinfo.localhost:{ingress}");
    let home = eventually(Duration::from_secs(180), || async {
        let response = http.get(&base).send().await.ok()?;
        let body = response.text().await.ok()?;
        body.contains("podinfo").then_some(body)
    })
    .await
    .unwrap_or_else(|| panic!("the frontend never answered through ingress:\n{}", log()));
    assert!(home.contains("\"hostname\""), "{home}");

    // The frontend forwards /api/echo to --backend-url=http://backend:9898,
    // and answers with the list of backend responses.
    let echoed = eventually(Duration::from_secs(60), || async {
        let response = http
            .post(format!("{base}/api/echo"))
            .body("reliaburger-demo")
            .send()
            .await
            .ok()?;
        let body = response.text().await.ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
        parsed.is_array().then_some(body)
    })
    .await
    .unwrap_or_else(|| panic!("frontend never reached the backend by name:\n{}", log()));
    assert!(echoed.contains("reliaburger-demo"), "{echoed}");

    let demo = Demo {
        root,
        api,
        http,
        base,
        _bun: bun,
        _cleanup: cleanup,
    };
    // The frontend's /cache API stores in --cache-server=tcp://redis:6379.
    let cached = eventually(Duration::from_secs(60), || async {
        demo.cache_round_trip("kept-in-redis").await.ok()
    })
    .await;
    assert!(
        cached.is_some(),
        "frontend never reached redis by name:\n{}",
        demo.log()
    );
    demo
}

#[tokio::test]
#[ignore = "requires root, runc, nftables, bpffs and internet access (provisioned Linux VM)"]
async fn podinfo_demo_frontend_reaches_backend_and_redis_by_name() {
    let demo = start_demo("").await;

    // Three frontends, as the manifest asks.
    let statuses = demo.client().cluster_status().await.unwrap();
    let frontends = statuses
        .iter()
        .filter(|row| row.instance.app_name == "frontend")
        .count();
    assert_eq!(frontends, 3, "{statuses:?}");
}

/// The tour's fault beats on the real demo: network faults act on the
/// frontend's own connections to redis, including the ones its pool already
/// holds open.
#[tokio::test]
#[ignore = "requires root, runc, nftables, bpffs and internet access (provisioned Linux VM)"]
async fn podinfo_demo_feels_network_faults_between_frontend_and_redis() {
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let demo = start_demo(
        r#"
[testing]
safety_class = "development"
allowed_operations = ["inject_workload_faults"]
"#,
    )
    .await;
    let client = demo.client();
    let fault = |fault_type| FaultRequest {
        fault_type,
        target_service: "redis".to_string(),
        namespace: Some("default".to_string()),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(120),
        injected_by: String::new(),
        reason: Some("tour".to_string()),
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    };

    // Z6.1 + Z6.2: a partition from the frontend cuts the connections its
    // redis pool already holds, so the very next cache call fails instead of
    // riding an old connection.
    demo.cache_round_trip("before-the-partition")
        .await
        .expect("the pool is warm before the fault");
    client
        .inject_fault(&fault(FaultType::Partition {
            source_app: Some("frontend".to_string()),
        }))
        .await
        .unwrap_or_else(|error| panic!("partition refused: {error}\n{}", demo.log()));
    // The ingress spreads calls over the three frontends, so six calls
    // reach each one's pool twice. Every one must fail straight away.
    let mut outcomes = Vec::new();
    for attempt in 0..6 {
        outcomes.push(
            demo.cache_round_trip(&format!("during-the-partition-{attempt}"))
                .await,
        );
    }
    eprintln!("under partition, podinfo says: {outcomes:?}");
    eprintln!(
        "frontend logs under partition:\n{}",
        demo.frontend_cache_logs().await
    );
    assert!(
        outcomes.iter().all(Result::is_err),
        "a frontend kept reaching redis through a partition: {outcomes:?}\n{}",
        demo.log()
    );

    client
        .clear_faults_by_service("redis", Some("default"))
        .await
        .unwrap();
    let healed = eventually(Duration::from_secs(20), || async {
        demo.cache_round_trip("after-the-partition").await.ok()
    })
    .await;
    assert!(healed.is_some(), "redis never came back:\n{}", demo.log());
}

/// Poll `check` until it returns `Some`, for at most `limit`.
async fn eventually<T, F, Fut>(limit: Duration, mut check: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + limit;
    loop {
        if let Some(value) = check().await {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
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

/// Unpin and detach the eBPF programs Bun attached: left behind, they'd
/// intercept every connect() on the host.
fn retire_kernel(root: &Path) {
    let policy = root.join("data/kernel-policy");
    let Ok(bytes) = std::fs::read(policy.join("owner.json")) else {
        return;
    };
    let Ok(owner) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    let (Some(cgroup), Some(pins)) = (
        owner["cgroup_path"].as_str(),
        owner["pin_directory"].as_str(),
    ) else {
        return;
    };
    if !Path::new(pins).exists() {
        return;
    }
    if let Err(error) = reliaburger::onion::ebpf::loader::OnionEbpf::retire_owned_state(
        Path::new(cgroup),
        &policy,
        Path::new(pins),
    ) {
        eprintln!("test cleanup: cannot retire the kernel programs: {error}");
    }
    let _ = std::fs::remove_dir(pins);
}

/// Delete the namespaces and veths of every instance whose id starts with
/// `prefix`.
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

/// Kill a cgroup subtree's processes and remove it bottom-up.
fn remove_cgroup(path: &Path) {
    for entry in std::fs::read_dir(path.parent().unwrap_or(path))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let wanted = path
            .file_name()
            .is_some_and(|app| name.starts_with(&*app.to_string_lossy()));
        if wanted {
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
