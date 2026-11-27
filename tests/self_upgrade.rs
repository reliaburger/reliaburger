//! Single-node self-upgrade integration tests (Phase 14).
//!
//! These run the REAL compiled `bun` binary as a subprocess, with the test
//! playing systemd: a supervisor loop that respawns bun whenever it exits.
//! Different "versions" are copies of the same binary distinguished by
//! `.version` sidecar files (debug builds only — see upgrade::version).
//!
//! The exec path cannot be unit-tested (it replaces the calling process),
//! so this file is where swap, revert, and workload survival get proven.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::watch;

use reliaburger::upgrade::signing::{self, encode_public_key};
use reliaburger::upgrade::types::{BinarySource, UpgradeDirective};

#[path = "support/bun_process.rs"]
mod bun_process;

/// How long to wait for slow conditions (binary boots, verification).
const WAIT: Duration = Duration::from_secs(30);

/// How long an accepted upgrade may stay in flight before a version wait
/// gives up. A hang guard, not a speed target: while the marker is on disk
/// the node is still working on the swap, and the node itself commits or
/// reverts it.
const IN_FLIGHT_HANG_GUARD: Duration = Duration::from_secs(300);

/// These tests spawn the real compiled binary under a supervisor loop and
/// take minutes; they'd blow the default CI test job's timeout. Gated
/// behind `RELIABURGER_UPGRADE_TESTS=1` (like the runc/eBPF suites), run
/// via `make test-upgrade` or the dedicated CI job.
fn upgrade_tests_enabled() -> bool {
    std::env::var("RELIABURGER_UPGRADE_TESTS").is_ok()
}

struct RealNodeHarness {
    _root: tempfile::TempDir,
    bin_dir: PathBuf,
    data_dir: PathBuf,
    api: String,
    client: reqwest::Client,
    stop_tx: watch::Sender<bool>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
    release_pkcs8: Vec<u8>,
    external_pkcs8: Vec<u8>,
    deployed_apps: Vec<(String, String)>,
    /// The fixed ports every exec and respawn rebinds, and every workload's
    /// port, leased for the test.
    held_ports: Vec<bun_process::PortBlockReservation>,
}

impl RealNodeHarness {
    /// Set up bin/data dirs, keys, config, versioned binaries v0.1.0 and
    /// v0.2.0 (copies of the compiled test binary), and start the
    /// supervisor loop with the symlink on v0.1.0.
    async fn start() -> Self {
        Self::start_with_cluster(false).await
    }

    async fn start_with_cluster(cluster: bool) -> Self {
        Self::start_configured(cluster, &["v0.1.0", "v0.2.0"], "v0.1.0").await
    }

    /// Install every version in `installed`, point the symlink at
    /// `running`, and start the supervisor there.
    async fn start_configured(cluster: bool, installed: &[&str], running: &str) -> Self {
        Self::start_with_node_config(cluster, installed, running, "").await
    }

    /// [`Self::start_configured`] with `extra` appended to `node.toml`.
    async fn start_with_node_config(
        cluster: bool,
        installed: &[&str],
        running: &str,
        extra: &str,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin_dir = root.path().join("bin");
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        let (release_pkcs8, release_public) = signing::generate_keypair().unwrap();
        let (external_pkcs8, external_public) = signing::generate_keypair().unwrap();

        for version in installed {
            install_version(&bin_dir, version);
        }
        std::os::unix::fs::symlink(format!("bun-{running}"), bin_dir.join("bun")).unwrap();

        // Every upgrade exec and respawn closes the API socket and binds the
        // same port again. A `bind(0)` port comes from the ephemeral range,
        // where another test can take it in that gap (#734); the shared
        // allocator's range lies below it and its lease keeps other fixtures
        // off. Pickle is not addressed by this fixture, so Bun can bind its
        // actual port zero.
        let api_port = bun_process::reserve_port_block_lease(1);
        let api = format!("127.0.0.1:{}", api_port.base);
        let mut held_ports = vec![api_port];

        let config_path = root.path().join("node.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
[storage]
data = "{data}"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"

[images]
registry_bind = "127.0.0.1"
registry_port = 0

# The process runtime runs host apps only (#914): allow testapp.
[process_workloads]
allowed_binaries = ["{testapp}"]
mount_isolation = false

[upgrades]
binary_dir = "{bin}"
external_signing_key = "{external}"
release_keys_override = ["{release}"]
boot_grace_secs = 2
gossip_rejoin_secs = 5
max_boot_attempts = 2
retain_versions = 3
"#,
                data = data_dir.display(),
                root = root.path().display(),
                bin = bin_dir.display(),
                external = encode_public_key(&external_public),
                release = encode_public_key(&release_public),
                testapp = env!("CARGO_BIN_EXE_testapp"),
            ),
        )
        .unwrap();

        if cluster {
            // A real cluster-mode process with no reachable peer. Local API
            // health must not allow its replacement to commit an upgrade.
            use std::io::Write;
            let ports = bun_process::reserve_port_block_lease(3);
            writeln!(std::fs::OpenOptions::new().append(true).open(&config_path).unwrap(),
                "\n[cluster]\ngossip_port = {}\nraft_port = {}\nreporting_port = {}\n[network]\nadvertise_address = \"127.0.0.1\"\n[security]\nrequire_mtls = false",
                ports.base, ports.base + 1, ports.base + 2).unwrap();
            held_ports.push(ports);
        }
        if !extra.is_empty() {
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&config_path)
                    .unwrap(),
                "\n{extra}"
            )
            .unwrap();
        }
        // Workload host ports outside the lease range (#2261).
        bun_process::use_test_workload_ports(&config_path);

        // The supervisor: spawn the symlink, respawn on exit. On a
        // successful upgrade the exec keeps the pid, so wait() simply keeps
        // waiting; only crashes (and reverts' exit(1)) come through here.
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let symlink = bin_dir.join("bun");
        let listen = api.clone();
        let supervisor_config = config_path.clone();
        let supervisor = tokio::spawn(async move {
            loop {
                let mut command = tokio::process::Command::new(&symlink);
                if cluster {
                    command.arg("--cluster");
                }
                let mut child = command
                    .arg("--config")
                    .arg(&supervisor_config)
                    .arg("--listen")
                    .arg(&listen)
                    .arg("--runtime")
                    .arg("process")
                    .kill_on_drop(true)
                    .spawn()
                    .expect("failed to spawn bun");

                tokio::select! {
                    _ = child.wait() => {
                        if *stop_rx.borrow() {
                            break;
                        }
                        // Play systemd Restart=always.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    _ = stop_rx.changed() => {
                        let _ = child.kill().await;
                        break;
                    }
                }
            }
        });

        let harness = Self {
            _root: root,
            bin_dir,
            data_dir,
            api,
            client: reqwest::Client::new(),
            stop_tx,
            supervisor: Some(supervisor),
            release_pkcs8,
            external_pkcs8,
            deployed_apps: Vec::new(),
            held_ports,
        };
        harness.wait_healthy().await;
        harness
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.api)
    }

    async fn wait_healthy(&self) {
        wait_for("node healthy", WAIT, || async {
            matches!(
                self.client.get(self.url("/v1/health")).send().await,
                Ok(r) if r.status().is_success()
            )
        })
        .await;
    }

    /// The version the node reports, if reachable.
    async fn version(&self) -> Option<String> {
        let response = self.client.get(self.url("/v1/version")).send().await.ok()?;
        let value: serde_json::Value = response.json().await.ok()?;
        Some(value["version"].as_str()?.to_string())
    }

    /// Wait until the node reports `expected`.
    ///
    /// While the upgrade marker is on disk the node hasn't decided the swap
    /// yet: it is starting the new binary, hashing it (the version answer
    /// carries the digest) and verifying. With a 600 MB debug build that
    /// takes as long as the host allows, and on a loaded Mac it went past
    /// 30 s (#1049). So the wait lasts as long as the marker does, up to a
    /// hang guard. Once the marker is gone the node has settled, and the
    /// version has `WAIT` to show.
    async fn wait_for_version(&self, expected: &str) {
        let marker = self.data_dir.join("upgrade/marker.json");
        let started = tokio::time::Instant::now();
        let mut settled_at = None;
        loop {
            if self.version().await.as_deref() == Some(expected) {
                return;
            }
            let now = tokio::time::Instant::now();
            if marker.exists() {
                settled_at = None;
                assert!(
                    now - started < IN_FLIGHT_HANG_GUARD,
                    "timed out waiting for version {expected}: the upgrade was still in flight after {:?}",
                    now - started
                );
            } else {
                let settled = *settled_at.get_or_insert(now);
                assert!(
                    now - settled < WAIT,
                    "timed out waiting for version {expected}: no upgrade in flight for {WAIT:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Wait until no upgrade marker is in flight (commit or revert done).
    async fn wait_upgrade_settled(&self) {
        wait_for("upgrade settled", WAIT, || async {
            let Ok(response) = self.client.get(self.url("/v1/version")).send().await else {
                return false;
            };
            let Ok(value) = response.json::<serde_json::Value>().await else {
                return false;
            };
            value["upgrade_in_flight"].as_bool() == Some(false)
        })
        .await;
    }

    /// Deploy a testapp instance serving HTTP on a port leased for the
    /// harness's lifetime, and return the port.
    ///
    /// The port comes from the lease range rather than a fixed number. Fixed
    /// ports 46011–46071 sat inside Linux's ephemeral range (32768–60999),
    /// where any outgoing connection or `bind(0)` on the host could hold one
    /// when the workload bound it (#2261).
    async fn deploy_testapp(&mut self, app: &str) -> u16 {
        self.deploy_testapp_with(app, "").await
    }

    /// [`Self::deploy_testapp`] with `extra` TOML lines added to the app's
    /// table, such as an `env` line.
    async fn deploy_testapp_with(&mut self, app: &str, extra: &str) -> u16 {
        let reservation = bun_process::reserve_port_block_lease(1);
        let port = reservation.base;
        self.held_ports.push(reservation);
        let config = format!(
            r#"
[app.{app}]
exec = "{testapp}"
command = ["--mode", "healthy", "--port", "{port}"]
{extra}
"#,
            testapp = env!("CARGO_BIN_EXE_testapp"),
        );
        let response = self
            .client
            .post(self.url("/v1/apply"))
            .body(config)
            .send()
            .await
            .expect("apply request failed");
        let body = response.text().await.unwrap_or_default();
        assert!(
            !body.contains("\"error\""),
            "deploy of {app} failed: {body}"
        );
        self.deployed_apps
            .push((app.to_string(), "default".to_string()));

        wait_for(&format!("{app} serving on {port}"), WAIT, || async move {
            reqwest::get(format!("http://127.0.0.1:{port}/"))
                .await
                .is_ok()
        })
        .await;
        port
    }

    /// The pid of an app's first instance, from /v1/status, waiting up to
    /// `WAIT` for one; see [`wait_for_workload_pid`].
    async fn workload_pid(&self, app: &str) -> u32 {
        wait_for_workload_pid(&self.client, &self.url("/v1/status"), app, WAIT).await
    }

    /// Build a signed directive for the staged file `bin/bun-{version}`.
    fn directive(&self, version: &str, upgrade_id: &str) -> UpgradeDirective {
        let path = self.bin_dir.join(format!("bun-{version}"));
        let bytes = std::fs::read(&path).unwrap();
        UpgradeDirective {
            upgrade_id: upgrade_id.to_string(),
            target_version: version.parse().unwrap(),
            binary_sha256: signing::sha256_hex(&bytes),
            embedded_signature: signing::sign(&self.release_pkcs8, &bytes).unwrap(),
            external_signature: Some(signing::sign(&self.external_pkcs8, &bytes).unwrap()),
            source: BinarySource::LocalFile { path },
            network_provenance: false,
            allow_downgrade: false,
        }
    }

    /// Write the signature envelope a staged upgrade leaves beside
    /// `bin/bun-{version}`, for a version installed by hand.
    fn seed_signature(&self, version: &str) {
        let bytes = std::fs::read(self.bin_dir.join(format!("bun-{version}"))).unwrap();
        signing::SignatureEnvelope {
            schema: 1,
            sha256: signing::sha256_hex(&bytes),
            embedded: signing::sign(&self.release_pkcs8, &bytes).unwrap(),
            external: Some(signing::sign(&self.external_pkcs8, &bytes).unwrap()),
        }
        .store(&self.bin_dir.join(format!("bun-{version}.sig")))
        .unwrap();
    }

    /// POST an upgrade directive; returns the HTTP status code.
    async fn post_upgrade(&self, directive: &UpgradeDirective) -> u16 {
        self.client
            .post(self.url("/v1/upgrade/apply"))
            .json(directive)
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0)
    }

    /// Roll back via the API; returns the HTTP status code.
    async fn post_rollback(&self) -> u16 {
        self.client
            .post(self.url("/v1/upgrade/rollback"))
            .body("")
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0)
    }

    /// Versions with binaries currently installed in the store dir.
    fn installed_versions(&self) -> Vec<String> {
        let mut versions: Vec<String> = std::fs::read_dir(&self.bin_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let rest = name.strip_prefix("bun-")?;
                (!rest.ends_with(".sig")
                    && !rest.ends_with(".version")
                    && !rest.ends_with(".fail-boot")
                    && !rest.ends_with(".fail-early"))
                .then(|| rest.to_string())
            })
            .collect();
        versions.sort();
        versions
    }

    /// Stop deployed apps (so their processes die) and stop the supervisor.
    async fn shutdown(mut self) {
        for (app, namespace) in self.deployed_apps.clone() {
            let _ = self
                .client
                .post(self.url(&format!("/v1/stop/{app}/{namespace}")))
                .body("")
                .send()
                .await;
        }
        // Give the stop a moment to retire the workloads the normal way; the
        // reaper below catches whatever it didn't.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !self.deployed_apps.is_empty() && tokio::time::Instant::now() < deadline {
            let Some(statuses) = self.statuses().await else {
                break;
            };
            if statuses.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = self.stop_tx.send(true);
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.await;
        }
        reap_workloads(self._root.path());
        let leftover = processes_under(self._root.path());
        assert!(
            leftover.is_empty(),
            "workload processes outlived the harness: {leftover:?}"
        );
    }

    /// Every instance the node reports, if it answers.
    async fn statuses(&self) -> Option<Vec<serde_json::Value>> {
        let response = self.client.get(self.url("/v1/status")).send().await.ok()?;
        response.json().await.ok()
    }
}

/// Workloads run under detached process owners so they survive Bun's exec.
/// That also means they survive the test: killing Bun (or a panic that drops
/// the supervisor) leaves them serving on their fixed ports, and the next run
/// finds its port taken. So the harness reaps them on every path.
impl Drop for RealNodeHarness {
    fn drop(&mut self) {
        reap_workloads(self._root.path());
    }
}

/// Pids of processes whose command line names `root` (the process owners,
/// whose `--directory` lives under the harness's data directory).
fn processes_under(root: &Path) -> Vec<u32> {
    let Ok(output) = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(root)
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// SIGKILL every process owner under `root` and the workloads it runs.
///
/// The children are listed before their owner dies (they'd be re-parented and
/// lost), and the owner dies before them (so it can't restart one).
fn reap_workloads(root: &Path) {
    for owner in processes_under(root) {
        let children = std::process::Command::new("pgrep")
            .args(["-P", &owner.to_string()])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let mut pids = vec![owner.to_string()];
        pids.extend(children.split_whitespace().map(str::to_string));
        for pid in pids {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid])
                .status();
        }
    }
}

/// Copy the compiled bun binary in as `bun-{version}` with its `.version`
/// sidecar (the debug-only override that makes identical bytes report
/// different versions — see book §14.1 for why this is a file, not an env
/// var).
fn install_version(bin_dir: &Path, version: &str) {
    let target = bin_dir.join(format!("bun-{version}"));
    std::fs::copy(env!("CARGO_BIN_EXE_bun"), &target).unwrap();
    std::fs::write(bin_dir.join(format!("bun-{version}.version")), version).unwrap();
}

/// Poll `condition` until true or panic after `timeout`.
async fn wait_for<F, Fut>(what: &str, timeout: Duration, condition: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The pid of `app`'s first instance that reports one in the `/v1/status`
/// body `body`, if any.
fn instance_pid(body: &str, app: &str) -> Option<u32> {
    let statuses: serde_json::Value = serde_json::from_str(body).ok()?;
    statuses.as_array()?.iter().find_map(|instance| {
        (instance["app_name"] == app)
            .then(|| instance["pid"].as_u64())
            .flatten()
            .and_then(|pid| u32::try_from(pid).ok())
    })
}

/// Read `status_url` until `app` reports a pid, and return it.
///
/// A status read can come back with the instance but no pid: when the
/// runtime doesn't answer within the status reader's deadline, the instance
/// shows `pid: null` and `runtime_unknown: true` (#2236). That's "not known
/// yet", not "no process", so this keeps asking. It fails after `timeout`
/// with the last body it read, so a failure shows what the node reported
/// (#2261).
async fn wait_for_workload_pid(
    client: &reqwest::Client,
    status_url: &str,
    app: &str,
    timeout: Duration,
) -> u32 {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_body = String::from("(no answer)");
    loop {
        let body = match client.get(status_url).send().await {
            Ok(response) => response.text().await.ok(),
            Err(_) => None,
        };
        if let Some(body) = body {
            if let Some(pid) = instance_pid(&body, app) {
                return pid;
            }
            last_body = body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{app} reported no pid within {timeout:?}; last /v1/status body: {last_body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Serve each body in `bodies` as one HTTP response, in order, repeating the
/// last, and return the server's address.
async fn serve_status_bodies(bodies: Vec<&'static str>) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // An in-process stand-in for Bun's API that nothing rebinds, so an
    // ephemeral port is fine here.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for index in 0.. {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await;
            let body = bodies[index.min(bodies.len() - 1)];
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    address
}

const RUNTIME_UNKNOWN_STATUS: &str =
    r#"[{"app_name":"web","id":"default__web-0","pid":null,"runtime_unknown":true}]"#;

/// A status read that misses the runtime's answer reports the instance with
/// no pid and `runtime_unknown`. The harness used to take that as "no pid"
/// and fail the test at once; it now asks again.
#[tokio::test]
async fn workload_pid_waits_out_a_runtime_unknown_status() {
    let address = serve_status_bodies(vec![
        RUNTIME_UNKNOWN_STATUS,
        RUNTIME_UNKNOWN_STATUS,
        r#"[{"app_name":"web","id":"default__web-0","pid":4242,"runtime_unknown":false}]"#,
    ])
    .await;
    let pid = wait_for_workload_pid(
        &reqwest::Client::new(),
        &format!("http://{address}/v1/status"),
        "web",
        WAIT,
    )
    .await;
    assert_eq!(pid, 4242);
}

/// When the pid never comes, the failure shows the last status body.
#[tokio::test]
#[should_panic(expected = "runtime_unknown\":true")]
async fn workload_pid_prints_the_status_body_when_it_gives_up() {
    let address = serve_status_bodies(vec![RUNTIME_UNKNOWN_STATUS]).await;
    wait_for_workload_pid(
        &reqwest::Client::new(),
        &format!("http://{address}/v1/status"),
        "web",
        Duration::from_millis(500),
    )
    .await;
}

// Each test runs a full node (or several processes) on this machine;
// running them concurrently starves each other of CPU and flakes the
// timing-sensitive waits. One at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn single_node_upgrade_preserves_running_containers() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 before running ignored upgrade tests"
    );
    let _serial = SERIAL.lock().await;
    let mut harness = RealNodeHarness::start().await;
    assert_eq!(harness.version().await.as_deref(), Some("v0.1.0"));

    harness.deploy_testapp("web").await;
    // Preserve an actual canary generation across exec, then replace it again.
    let canary_port = harness.deploy_testapp("web").await;
    let pid_before = harness.workload_pid("web").await;

    let directive = harness.directive("v0.2.0", "up-1");
    assert_eq!(harness.post_upgrade(&directive).await, 202);

    harness.wait_for_version("v0.2.0").await;
    harness.wait_upgrade_settled().await;

    // The workload sailed through the exec: same pid, still serving.
    let pid_after = harness.workload_pid("web").await;
    assert_eq!(
        pid_before, pid_after,
        "workload was restarted by the upgrade"
    );
    let response = reqwest::get(format!("http://127.0.0.1:{canary_port}/")).await;
    assert!(
        response.is_ok(),
        "workload stopped serving after the upgrade"
    );

    // The swap is visible on disk: symlink on v0.2.0, marker gone.
    let target = std::fs::read_link(harness.bin_dir.join("bun")).unwrap();
    assert_eq!(target, Path::new("bun-v0.2.0"));
    assert!(!harness.data_dir.join("upgrade/marker.json").exists());

    harness.deploy_testapp("web").await;
    let status: serde_json::Value = harness
        .client
        .get(harness.url("/v1/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let web = status
        .as_array()
        .unwrap()
        .iter()
        .find(|instance| instance["app_name"] == "web")
        .unwrap();
    assert_eq!(
        web["id"], "default__web-g2-0",
        "replacement reused an adopted generation: {status}"
    );
    assert_ne!(harness.workload_pid("web").await, pid_after);
    harness.shutdown().await;
}

/// #2334: an app with a `PROVIDER[...]` variable survives the upgrade's
/// exec too. Bun's adoption record keeps the reference and the runtime's
/// launch keeps the value the provider gave; before, the two never matched,
/// so the new Bun killed the app and started it again.
#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn single_node_upgrade_adopts_an_app_that_uses_a_secret_provider() {
    use std::os::unix::fs::PermissionsExt as _;
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 before running ignored upgrade tests"
    );
    let _serial = SERIAL.lock().await;
    let provider_dir = tempfile::tempdir().unwrap();
    let provider = provider_dir.path().join("provider");
    std::fs::write(
        &provider,
        "#!/bin/sh\nhead -n 1 > /dev/null\nprintf '{\"version\":1,\"value\":\"from-the-provider\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut harness = RealNodeHarness::start_with_node_config(
        false,
        &["v0.1.0", "v0.2.0"],
        "v0.1.0",
        &format!(
            "[secret_providers.vault]\nbinary = \"{}\"\ntimeout_secs = 60",
            provider.display()
        ),
    )
    .await;

    harness
        .deploy_testapp_with(
            "web",
            r#"env = { DB_PASSWORD = "PROVIDER[vault:secret/db#password]" }"#,
        )
        .await;
    let pid_before = harness.workload_pid("web").await;

    let directive = harness.directive("v0.2.0", "up-provider");
    assert_eq!(harness.post_upgrade(&directive).await, 202);
    harness.wait_for_version("v0.2.0").await;
    harness.wait_upgrade_settled().await;

    assert_eq!(
        harness.workload_pid("web").await,
        pid_before,
        "the upgrade restarted an app that uses a secret provider"
    );
    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn single_node_rollback_reverts_to_previous_version() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 before running ignored upgrade tests"
    );
    let _serial = SERIAL.lock().await;
    let mut harness = RealNodeHarness::start().await;
    harness.deploy_testapp("web").await;
    let pid_before = harness.workload_pid("web").await;

    // Upgrade to v0.2.0 first, then roll back.
    let directive = harness.directive("v0.2.0", "up-1");
    assert_eq!(harness.post_upgrade(&directive).await, 202);
    harness.wait_for_version("v0.2.0").await;
    harness.wait_upgrade_settled().await;

    assert_eq!(harness.post_rollback().await, 202);
    harness.wait_for_version("v0.1.0").await;
    harness.wait_upgrade_settled().await;

    let target = std::fs::read_link(harness.bin_dir.join("bun")).unwrap();
    assert_eq!(target, Path::new("bun-v0.1.0"));
    // The workload survived BOTH swaps.
    let pid_after = harness.workload_pid("web").await;
    assert_eq!(pid_before, pid_after);

    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn failed_upgrade_triggers_automatic_rollback() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 before running ignored upgrade tests"
    );
    let _serial = SERIAL.lock().await;
    let mut harness = RealNodeHarness::start().await;
    harness.deploy_testapp("web").await;
    let pid_before = harness.workload_pid("web").await;

    // Poison v0.2.0: it exits right after startup recovery, burning boot
    // attempts until the crash-loop revert kicks in.
    std::fs::write(harness.bin_dir.join("bun-v0.2.0.fail-boot"), "").unwrap();

    let directive = harness.directive("v0.2.0", "up-1");
    assert_eq!(harness.post_upgrade(&directive).await, 202);

    // The node must come back on v0.1.0 without human help.
    harness.wait_for_version("v0.1.0").await;
    harness.wait_upgrade_settled().await;
    let target = std::fs::read_link(harness.bin_dir.join("bun")).unwrap();
    assert_eq!(target, Path::new("bun-v0.1.0"));

    // The revert is on the record.
    let status: serde_json::Value = harness
        .client
        .get(harness.url("/v1/upgrade/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let outcomes: Vec<&str> = status["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["outcome"].as_str())
        .collect();
    assert!(
        outcomes.contains(&"Reverted"),
        "expected a Reverted history entry, got {outcomes:?}"
    );

    // The workload never noticed: it was reparented during the crash loop
    // and adopted back by the reverted binary.
    let pid_after = harness.workload_pid("web").await;
    assert_eq!(
        pid_before, pid_after,
        "workload was restarted by the failed upgrade"
    );

    harness.shutdown().await;
}

/// #2415: a new version that exits before it has even read its config
/// still burns a boot attempt on every start, so the crash loop ends in a
/// revert with no operator help. Before the fix the boot count lived after
/// config load, and this node restarted `v0.2.0` for ever.
#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn an_upgrade_that_fails_before_config_load_reverts_by_itself() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 before running ignored upgrade tests"
    );
    let _serial = SERIAL.lock().await;
    let mut harness = RealNodeHarness::start().await;
    harness.deploy_testapp("web").await;
    let pid_before = harness.workload_pid("web").await;

    // Poison v0.2.0 so it exits before it loads its config, where a
    // renamed key or a stricter validation would stop a real release.
    std::fs::write(harness.bin_dir.join("bun-v0.2.0.fail-early"), "").unwrap();

    let directive = harness.directive("v0.2.0", "up-early");
    assert_eq!(harness.post_upgrade(&directive).await, 202);

    harness.wait_for_version("v0.1.0").await;
    harness.wait_upgrade_settled().await;
    let target = std::fs::read_link(harness.bin_dir.join("bun")).unwrap();
    assert_eq!(target, Path::new("bun-v0.1.0"));
    assert!(
        !harness.bin_dir.join(".bun.upgrade-pending").exists(),
        "the completed revert must remove the boot-count pointer"
    );

    let status: serde_json::Value = harness
        .client
        .get(harness.url("/v1/upgrade/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let outcomes: Vec<&str> = status["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["outcome"].as_str())
        .collect();
    assert!(
        outcomes.contains(&"Reverted"),
        "expected a Reverted history entry, got {outcomes:?}"
    );

    let pid_after = harness.workload_pid("web").await;
    assert_eq!(
        pid_before, pid_after,
        "workload was restarted by the failed upgrade"
    );

    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn version_retention_gc_keeps_last_three() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 before running ignored upgrade tests"
    );
    let _serial = SERIAL.lock().await;
    // Start on v0.4.0 with three older versions already installed, each
    // with its signature sidecar, so one real upgrade crosses the window.
    // Starting on v0.1.0 would not do: GC protects the version a commit
    // came from, so the oldest binary would legitimately survive.
    let older = ["v0.1.0", "v0.2.0", "v0.3.0"];
    let mut installed = older.to_vec();
    installed.push("v0.4.0");
    let harness = RealNodeHarness::start_configured(false, &installed, "v0.4.0").await;
    harness.wait_for_version("v0.4.0").await;
    for version in older {
        harness.seed_signature(version);
    }

    install_version(&harness.bin_dir, "v0.5.0");
    let directive = harness.directive("v0.5.0", "up-retention");
    assert_eq!(
        harness.post_upgrade(&directive).await,
        202,
        "upgrade to v0.5.0"
    );
    harness.wait_for_version("v0.5.0").await;
    // Commit prunes before it clears the in-flight marker, so once the node
    // reports settled the store must already hold only the retained set.
    harness.wait_upgrade_settled().await;

    // retain_versions = 3: only the newest three binaries remain, and the
    // pruned ones took their `.sig` sidecars with them.
    assert_eq!(
        harness.installed_versions(),
        vec!["v0.3.0", "v0.4.0", "v0.5.0"]
    );
    for pruned in ["v0.1.0", "v0.2.0"] {
        assert!(!harness.bin_dir.join(format!("bun-{pruned}")).exists());
        assert!(
            !harness.bin_dir.join(format!("bun-{pruned}.sig")).exists(),
            "{pruned}'s signature outlived its binary"
        );
    }
    assert!(
        harness.bin_dir.join("bun-v0.3.0.sig").exists(),
        "a retained version lost its signature"
    );

    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn upgrade_rejects_bad_external_signature() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 before running ignored upgrade tests"
    );
    let _serial = SERIAL.lock().await;
    let harness = RealNodeHarness::start().await;

    let mut directive = harness.directive("v0.2.0", "up-1");
    // Sign the wrong bytes with the right key.
    directive.external_signature =
        Some(signing::sign(&harness.external_pkcs8, b"not the binary").unwrap());

    let status = harness.post_upgrade(&directive).await;
    assert_eq!(status, 409, "bad signature must be refused");

    // Nothing changed: still v0.1.0, symlink untouched, no marker.
    assert_eq!(harness.version().await.as_deref(), Some("v0.1.0"));
    let target = std::fs::read_link(harness.bin_dir.join("bun")).unwrap();
    assert_eq!(target, Path::new("bun-v0.1.0"));
    assert!(!harness.data_dir.join("upgrade/marker.json").exists());

    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn same_version_upgrade_never_swaps_silently() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 before running ignored upgrade tests"
    );
    let _serial = SERIAL.lock().await;
    let harness = RealNodeHarness::start().await;
    let running = std::fs::read(harness.bin_dir.join("bun-v0.1.0")).unwrap();

    // The running bytes again: nothing to do, reported as such.
    let response = harness
        .client
        .post(harness.url("/v1/upgrade/apply"))
        .json(&harness.directive("v0.1.0", "same-bytes"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "already_running", "{body}");

    // A different build under the running version: refused, untouched.
    let rebuilt_path = harness.data_dir.join("rebuilt-bun");
    let mut rebuilt = running.clone();
    rebuilt.extend_from_slice(b"\n# a different build of v0.1.0\n");
    std::fs::write(&rebuilt_path, &rebuilt).unwrap();
    let mut directive = harness.directive("v0.1.0", "same-version");
    directive.binary_sha256 = signing::sha256_hex(&rebuilt);
    directive.embedded_signature = signing::sign(&harness.release_pkcs8, &rebuilt).unwrap();
    directive.external_signature = Some(signing::sign(&harness.external_pkcs8, &rebuilt).unwrap());
    directive.source = BinarySource::LocalFile { path: rebuilt_path };
    assert_eq!(harness.post_upgrade(&directive).await, 409);

    assert_eq!(harness.version().await.as_deref(), Some("v0.1.0"));
    assert_eq!(
        std::fs::read(harness.bin_dir.join("bun-v0.1.0")).unwrap(),
        running,
        "the running version's bytes must be untouched"
    );
    assert!(!harness.data_dir.join("upgrade/marker.json").exists());

    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and real bun processes; run with make test-upgrade-node"]
async fn isolated_replacement_reverts_without_restarting_its_workload() {
    assert!(upgrade_tests_enabled());
    let _serial = SERIAL.lock().await;
    let mut harness = RealNodeHarness::start_with_cluster(true).await;
    harness.deploy_testapp("rejoin").await;
    let pid = harness.workload_pid("rejoin").await;
    assert_eq!(
        harness
            .post_upgrade(&harness.directive("v0.2.0", "no-rejoin"))
            .await,
        202
    );
    harness.wait_for_version("v0.2.0").await;
    harness.wait_healthy().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        harness.data_dir.join("upgrade/marker.json").exists(),
        "local health and boot grace must not commit without a peer"
    );
    harness.wait_for_version("v0.1.0").await;
    harness.wait_upgrade_settled().await;
    assert_eq!(harness.workload_pid("rejoin").await, pid);
    assert_eq!(
        std::fs::read_link(harness.bin_dir.join("bun")).unwrap(),
        Path::new("bun-v0.1.0")
    );
    harness.shutdown().await;
}
