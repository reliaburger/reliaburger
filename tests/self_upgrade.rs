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

/// How long to wait for slow conditions (binary boots, verification).
const WAIT: Duration = Duration::from_secs(30);

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
}

impl RealNodeHarness {
    /// Set up bin/data dirs, keys, config, versioned binaries v0.1.0 and
    /// v0.2.0 (copies of the compiled test binary), and start the
    /// supervisor loop with the symlink on v0.1.0.
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin_dir = root.path().join("bin");
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        let (release_pkcs8, release_public) = signing::generate_keypair().unwrap();
        let (external_pkcs8, external_public) = signing::generate_keypair().unwrap();

        install_version(&bin_dir, "v0.1.0");
        install_version(&bin_dir, "v0.2.0");
        std::os::unix::fs::symlink("bun-v0.1.0", bin_dir.join("bun")).unwrap();

        // Grab free ports for the API and the Pickle registry
        // (bind-then-release; races are possible but rare enough for tests).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let api = listener.local_addr().unwrap().to_string();
        let registry_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let registry_port = registry_listener.local_addr().unwrap().port();
        drop(listener);
        drop(registry_listener);

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
registry_port = {registry_port}

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
            ),
        )
        .unwrap();

        // The supervisor: spawn the symlink, respawn on exit. On a
        // successful upgrade the exec keeps the pid, so wait() simply keeps
        // waiting; only crashes (and reverts' exit(1)) come through here.
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let symlink = bin_dir.join("bun");
        let listen = api.clone();
        let supervisor_config = config_path.clone();
        let supervisor = tokio::spawn(async move {
            loop {
                let mut child = tokio::process::Command::new(&symlink)
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

    async fn wait_for_version(&self, expected: &str) {
        wait_for(&format!("version {expected}"), WAIT, || async {
            self.version().await.as_deref() == Some(expected)
        })
        .await;
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

    /// Deploy a testapp instance serving HTTP on `port`.
    async fn deploy_testapp(&mut self, app: &str, port: u16) {
        let config = format!(
            r#"
[app.{app}]
image = "proc-grill:image-ignored"
command = ["{testapp}", "--mode", "healthy", "--port", "{port}"]
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
    }

    /// The pid of an app's first instance, from /v1/status.
    async fn workload_pid(&self, app: &str) -> Option<u32> {
        let response = self.client.get(self.url("/v1/status")).send().await.ok()?;
        let statuses: serde_json::Value = response.json().await.ok()?;
        statuses
            .as_array()?
            .iter()
            .find_map(|instance| {
                (instance["app_name"] == app)
                    .then(|| instance["pid"].as_u64())
                    .flatten()
            })
            .map(|pid| pid as u32)
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
        }
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
                    && !rest.ends_with(".fail-boot"))
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
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = self.stop_tx.send(true);
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.await;
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

// Each test runs a full node (or several processes) on this machine;
// running them concurrently starves each other of CPU and flakes the
// timing-sensitive waits. One at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_node_upgrade_preserves_running_containers() {
    if !upgrade_tests_enabled() {
        eprintln!("skipping upgrade integration test (set RELIABURGER_UPGRADE_TESTS=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let mut harness = RealNodeHarness::start().await;
    assert_eq!(harness.version().await.as_deref(), Some("v0.1.0"));

    harness.deploy_testapp("web", 46011).await;
    let pid_before = harness.workload_pid("web").await.expect("workload pid");

    let directive = harness.directive("v0.2.0", "up-1");
    assert_eq!(harness.post_upgrade(&directive).await, 202);

    harness.wait_for_version("v0.2.0").await;
    harness.wait_upgrade_settled().await;

    // The workload sailed through the exec: same pid, still serving.
    let pid_after = harness.workload_pid("web").await.expect("workload pid");
    assert_eq!(
        pid_before, pid_after,
        "workload was restarted by the upgrade"
    );
    let response = reqwest::get("http://127.0.0.1:46011/").await;
    assert!(
        response.is_ok(),
        "workload stopped serving after the upgrade"
    );

    // The swap is visible on disk: symlink on v0.2.0, marker gone.
    let target = std::fs::read_link(harness.bin_dir.join("bun")).unwrap();
    assert_eq!(target, Path::new("bun-v0.2.0"));
    assert!(!harness.data_dir.join("upgrade/marker.json").exists());

    harness.shutdown().await;
}

#[tokio::test]
async fn single_node_rollback_reverts_to_previous_version() {
    if !upgrade_tests_enabled() {
        eprintln!("skipping upgrade integration test (set RELIABURGER_UPGRADE_TESTS=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let mut harness = RealNodeHarness::start().await;
    harness.deploy_testapp("web", 46021).await;
    let pid_before = harness.workload_pid("web").await.expect("workload pid");

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
    let pid_after = harness.workload_pid("web").await.expect("workload pid");
    assert_eq!(pid_before, pid_after);

    harness.shutdown().await;
}

#[tokio::test]
async fn failed_upgrade_triggers_automatic_rollback() {
    if !upgrade_tests_enabled() {
        eprintln!("skipping upgrade integration test (set RELIABURGER_UPGRADE_TESTS=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let mut harness = RealNodeHarness::start().await;
    harness.deploy_testapp("web", 46031).await;
    let pid_before = harness.workload_pid("web").await.expect("workload pid");

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
    let pid_after = harness.workload_pid("web").await.expect("workload pid");
    assert_eq!(
        pid_before, pid_after,
        "workload was restarted by the failed upgrade"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn version_retention_gc_keeps_last_three() {
    if !upgrade_tests_enabled() {
        eprintln!("skipping upgrade integration test (set RELIABURGER_UPGRADE_TESTS=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let harness = RealNodeHarness::start().await;

    for (i, version) in ["v0.2.0", "v0.3.0", "v0.4.0", "v0.5.0"].iter().enumerate() {
        install_version(&harness.bin_dir, version);
        let directive = harness.directive(version, &format!("up-{i}"));
        assert_eq!(
            harness.post_upgrade(&directive).await,
            202,
            "upgrade to {version}"
        );
        harness.wait_for_version(version).await;
        harness.wait_upgrade_settled().await;
    }

    // retain_versions = 3: only the newest three binaries remain.
    assert_eq!(
        harness.installed_versions(),
        vec!["v0.3.0", "v0.4.0", "v0.5.0"]
    );
    assert!(!harness.bin_dir.join("bun-v0.1.0").exists());
    assert!(!harness.bin_dir.join("bun-v0.2.0.sig").exists());

    harness.shutdown().await;
}

#[tokio::test]
async fn upgrade_rejects_bad_external_signature() {
    if !upgrade_tests_enabled() {
        eprintln!("skipping upgrade integration test (set RELIABURGER_UPGRADE_TESTS=1)");
        return;
    }
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
