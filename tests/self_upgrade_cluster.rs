//! Cluster rolling-upgrade integration tests (Phase 14).
//!
//! Four REAL bun processes form a gossip+Raft cluster on localhost; the
//! test plays systemd for each of them and drives upgrades through the
//! same HTTP API relish uses. These are the slowest tests in the repo
//! (a couple of minutes each) — they earn it by proving the rolling walk,
//! leader-last in-place upgrade, pause/resume, and cluster rollback
//! against real processes exec'ing themselves.
//!
//! Note on roles: with four nodes the council reconciler makes ALL of
//! them Raft voters (the cap is seven), so "worker" here is a role in
//! the upgrade plan, not a Raft status. The ordering mechanics the tests
//! assert — workers first, council one at a time, leader last (in place)
//! — are exactly the mechanics under test.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::watch;

use reliaburger::upgrade::signing::{self, encode_public_key};

/// Cluster operations are slow: convergence, four swaps, verification.
const WAIT: Duration = Duration::from_secs(180);

/// Four real bun processes per test, minutes each — far past the default
/// CI test job's budget. Gated behind `RELIABURGER_UPGRADE_TESTS=1` (like
/// the runc/eBPF suites); run via `make test-upgrade` or the dedicated CI
/// job.
fn upgrade_tests_enabled() -> bool {
    std::env::var("RELIABURGER_UPGRADE_TESTS").is_ok()
}

/// Every test boots a full cluster; running them concurrently starves the
/// machine. One at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct ClusterNode {
    name: String,
    bin_dir: PathBuf,
    api: String,
    registry: String,
    stop_tx: watch::Sender<bool>,
    supervisor: Option<tokio::task::JoinHandle<()>>,
}

struct ClusterHarness {
    _root: tempfile::TempDir,
    nodes: Vec<ClusterNode>,
    client: reqwest::Client,
    release_pkcs8: Vec<u8>,
    external_pkcs8: Vec<u8>,
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Copy the compiled bun in as `bun-{version}` with its `.version` sidecar.
fn install_version(bin_dir: &Path, version: &str) {
    let target = bin_dir.join(format!("bun-{version}"));
    std::fs::copy(env!("CARGO_BIN_EXE_bun"), &target).unwrap();
    std::fs::write(bin_dir.join(format!("bun-{version}.version")), version).unwrap();
}

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
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

impl ClusterHarness {
    /// Boot `count` nodes: node 0 bootstraps, the rest join via gossip.
    async fn start(count: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        let (release_pkcs8, release_public) = signing::generate_keypair().unwrap();
        let (external_pkcs8, external_public) = signing::generate_keypair().unwrap();

        let mut nodes = Vec::new();
        let mut seed_gossip: Option<u16> = None;

        for index in 0..count {
            let name = format!("n{index}");
            let node_root = root.path().join(&name);
            let bin_dir = node_root.join("bin");
            std::fs::create_dir_all(&bin_dir).unwrap();
            install_version(&bin_dir, "v0.1.0");
            install_version(&bin_dir, "v0.2.0");
            std::os::unix::fs::symlink("bun-v0.1.0", bin_dir.join("bun")).unwrap();

            let api_port = free_port();
            let gossip_port = free_port();
            let raft_port = free_port();
            let reporting_port = free_port();
            let registry_port = free_port();
            let join = match seed_gossip {
                Some(seed) => format!("join = [\"127.0.0.1:{seed}\"]"),
                None => "join = []".to_string(),
            };
            if seed_gossip.is_none() {
                seed_gossip = Some(gossip_port);
            }

            let config_path = node_root.join("node.toml");
            std::fs::write(
                &config_path,
                format!(
                    r#"
[node]
name = "{name}"

[cluster]
{join}
gossip_port = {gossip_port}
raft_port = {raft_port}
reporting_port = {reporting_port}

[network]
advertise_address = "127.0.0.1"

[storage]
data = "{node_root}/data"
images = "{node_root}/images"
logs = "{node_root}/logs"
metrics = "{node_root}/metrics"
volumes = "{node_root}/volumes"

[images]
registry_bind = "127.0.0.1"
registry_port = {registry_port}

[upgrades]
binary_dir = "{bin}"
external_signing_key = "{external}"
release_keys_override = ["{release}"]
boot_grace_secs = 2
gossip_rejoin_secs = 10
max_boot_attempts = 2
retain_versions = 3
"#,
                    node_root = node_root.display(),
                    bin = bin_dir.display(),
                    external = encode_public_key(&external_public),
                    release = encode_public_key(&release_public),
                ),
            )
            .unwrap();

            let api = format!("127.0.0.1:{api_port}");
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
                        .arg("--cluster")
                        .kill_on_drop(true)
                        .spawn()
                        .expect("failed to spawn bun");

                    tokio::select! {
                        _ = child.wait() => {
                            if *stop_rx.borrow() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                        _ = stop_rx.changed() => {
                            let _ = child.kill().await;
                            break;
                        }
                    }
                }
            });

            nodes.push(ClusterNode {
                name,
                bin_dir,
                api,
                registry: format!("127.0.0.1:{registry_port}"),
                stop_tx,
                supervisor: Some(supervisor),
            });
        }

        let harness = Self {
            _root: root,
            nodes,
            client: reqwest::Client::new(),
            release_pkcs8,
            external_pkcs8,
        };

        // Every node healthy, then a leader elected with a full council.
        for node in &harness.nodes {
            let api = node.api.clone();
            let client = harness.client.clone();
            wait_for(&format!("{} healthy", node.name), WAIT, || {
                let client = client.clone();
                let api = api.clone();
                async move {
                    matches!(
                        client.get(format!("http://{api}/v1/health")).send().await,
                        Ok(r) if r.status().is_success()
                    )
                }
            })
            .await;
        }
        wait_for("leader elected", WAIT, || async {
            harness.leader().await.is_some()
        })
        .await;
        harness
    }

    /// The current leader's node name, from the bootstrap node's council
    /// view (the `leader` field is the node name).
    async fn leader(&self) -> Option<String> {
        let response = self
            .client
            .get(format!("http://{}/v1/cluster/council", self.nodes[0].api))
            .send()
            .await
            .ok()?;
        let council: serde_json::Value = response.json().await.ok()?;
        council["leader"].as_str().map(String::from)
    }

    async fn wait_for_leader(&self) -> String {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(leader) = self.leader().await {
                return leader;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for a cluster leader"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn node(&self, name: &str) -> &ClusterNode {
        self.nodes.iter().find(|n| n.name == name).unwrap()
    }

    async fn node_version(&self, api: &str) -> Option<String> {
        let response = self
            .client
            .get(format!("http://{api}/v1/version"))
            .send()
            .await
            .ok()?;
        let value: serde_json::Value = response.json().await.ok()?;
        value["version"].as_str().map(String::from)
    }

    /// The upgrade-plan node list: the leader last, two council members,
    /// the remaining node labelled Worker (see the module note on roles).
    async fn plan_nodes(&self) -> (Vec<serde_json::Value>, String, String) {
        let leader = self.wait_for_leader().await;
        let worker = self
            .nodes
            .iter()
            .map(|n| n.name.clone())
            .find(|name| *name != leader)
            .unwrap();
        let list = self
            .nodes
            .iter()
            .map(|node| {
                let role = if node.name == leader {
                    "Leader"
                } else if node.name == worker {
                    "Worker"
                } else {
                    "Council"
                };
                serde_json::json!({
                    "node_id": node.name,
                    "address": node.api,
                    "role": role,
                })
            })
            .collect();
        (list, leader, worker)
    }

    /// Sign + push the v0.2.0 binary to the leader's registry, start the
    /// upgrade, return (upgrade_id, leader, worker).
    async fn start_upgrade(&self) -> (String, String, String) {
        let (nodes, leader, worker) = self.plan_nodes().await;
        let leader_node = self.node(&leader);

        let bytes = std::fs::read(leader_node.bin_dir.join("bun-v0.2.0")).unwrap();
        let sha256 = signing::sha256_hex(&bytes);
        let push_url = format!(
            "http://{}/v2/reliaburger-bun/blobs/uploads/?digest=sha256:{sha256}",
            leader_node.registry
        );
        let push = self
            .client
            .post(&push_url)
            .body(bytes.clone())
            .send()
            .await
            .expect("blob push");
        assert!(
            push.status().is_success(),
            "blob push failed: {}",
            push.status()
        );

        let request = serde_json::json!({
            "target_version": "v0.2.0",
            "binary_sha256": sha256,
            "embedded_signature": signing::sign(&self.release_pkcs8, &bytes).unwrap(),
            "external_signature": signing::sign(&self.external_pkcs8, &bytes).unwrap(),
            "parallel": 1,
            "registry_address": leader_node.registry,
            "nodes": nodes,
        });
        let response = self
            .client
            .post(format!("http://{}/v1/upgrade/start", leader_node.api))
            .json(&request)
            .send()
            .await
            .expect("upgrade start");
        assert_eq!(response.status().as_u16(), 202, "upgrade start refused");
        let body: serde_json::Value = response.json().await.unwrap();
        (
            body["upgrade_id"].as_str().unwrap_or("?").to_string(),
            leader,
            worker,
        )
    }

    /// Poll the cluster upgrade until it completes (or a pause, if
    /// `allow_pause`), recording when each node first reports Healthy.
    /// Returns (first_healthy_order, final_phase).
    async fn watch_upgrade(&self, allow_pause: bool) -> (Vec<String>, String) {
        let mut healthy_order: Vec<String> = Vec::new();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if tokio::time::Instant::now() >= deadline {
                let state = self.cluster_state().await;
                let leader = self.leader().await;
                let versions = self.versions().await;
                panic!(
                    "timed out watching the upgrade\n  healthy so far: {healthy_order:?}\n  leader: {leader:?}\n  versions: {versions:?}\n  active: {}",
                    state
                        .as_ref()
                        .map(|s| s["active"].to_string())
                        .unwrap_or_else(|| "unreachable".to_string()),
                );
            }
            tokio::time::sleep(Duration::from_millis(500)).await;

            // Any node can serve the replicated state; use one that answers.
            let Some(cluster) = self.cluster_state().await else {
                continue;
            };

            let (nodes_json, phase) = match cluster.get("active").filter(|a| !a.is_null()) {
                Some(active) => (active["nodes"].clone(), phase_label(&active["phase"])),
                None => {
                    // Cleared: completed and archived to history.
                    let last = cluster["history"]
                        .as_array()
                        .and_then(|h| h.last())
                        .cloned()
                        .unwrap_or_default();
                    for node in last["nodes"].as_array().into_iter().flatten() {
                        note_healthy(&mut healthy_order, node);
                    }
                    return (healthy_order, "Completed".to_string());
                }
            };

            for node in nodes_json.as_array().into_iter().flatten() {
                note_healthy(&mut healthy_order, node);
            }
            if allow_pause && phase.starts_with("Paused") {
                return (healthy_order, phase);
            }
        }
    }

    async fn cluster_state(&self) -> Option<serde_json::Value> {
        for node in &self.nodes {
            let Ok(response) = self
                .client
                .get(format!("http://{}/v1/upgrade/cluster", node.api))
                .send()
                .await
            else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            if let Ok(value) = response.json::<serde_json::Value>().await {
                return Some(value);
            }
        }
        None
    }

    async fn versions(&self) -> HashMap<String, String> {
        let mut versions = HashMap::new();
        for node in &self.nodes {
            if let Some(version) = self.node_version(&node.api).await {
                versions.insert(node.name.clone(), version);
            }
        }
        versions
    }

    /// Wait until every replacement API is reachable and reports the target.
    /// The coordinator records completion immediately before the final
    /// leader's `exec`, so its HTTP listener can briefly be between processes.
    async fn wait_for_versions(&self, expected: &str) -> HashMap<String, String> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let versions = self.versions().await;
            if versions.len() == self.nodes.len()
                && versions.values().all(|version| version == expected)
            {
                return versions;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for every node on {expected}: {versions:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn shutdown(mut self) {
        for node in &mut self.nodes {
            let _ = node.stop_tx.send(true);
            if let Some(supervisor) = node.supervisor.take() {
                let _ = supervisor.await;
            }
        }
    }
}

fn note_healthy(order: &mut Vec<String>, node: &serde_json::Value) {
    if node["phase"] == "Healthy"
        && let Some(name) = node["node_id"].as_str()
        && !order.iter().any(|n| n == name)
    {
        order.push(name.to_string());
    }
}

fn phase_label(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "?".to_string()),
        _ => "?".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and a multi-core host"]
async fn rolling_upgrade_walks_workers_council_then_leader() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 on a provisioned multi-core host"
    );
    let _serial = SERIAL.lock().await;
    let harness = ClusterHarness::start(4).await;

    // This test owns the rolling *order* and clean convergence. Workload
    // survival across a node's in-place exec is a node-level invariant,
    // proven deterministically (same pid) in tests/self_upgrade.rs — in a
    // cluster the scheduler owns placement and a single-replica app on a
    // bouncing node is inherently at risk for its ~1s window, so it's the
    // wrong thing to pin here.
    let (_, old_leader, worker) = harness.start_upgrade().await;
    let (healthy_order, phase) = harness.watch_upgrade(false).await;
    assert_eq!(phase, "Completed");

    // Every node ended on the target version.
    let versions = harness.wait_for_versions("v0.2.0").await;
    for node in &harness.nodes {
        assert_eq!(
            versions.get(&node.name).map(String::as_str),
            Some("v0.2.0"),
            "node {} is not on v0.2.0 ({versions:?})",
            node.name
        );
    }

    // Rolling order: the worker first, the (old) leader last. The leader
    // upgrades itself in place (openraft 0.9 can't gracefully hand off
    // against a live leader), so it may still be leader afterwards — what
    // matters is that it went last and the cluster still has a leader.
    assert_eq!(
        healthy_order.first(),
        Some(&worker),
        "worker did not upgrade first: {healthy_order:?}"
    );
    assert_eq!(
        healthy_order.last(),
        Some(&old_leader),
        "old leader did not upgrade last: {healthy_order:?}"
    );
    harness.wait_for_leader().await;

    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and a multi-core host"]
async fn upgrade_failure_pauses_cluster_and_reverts_node() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 on a provisioned multi-core host"
    );
    let _serial = SERIAL.lock().await;
    let harness = ClusterHarness::start(4).await;

    let (_, _, worker) = {
        // Poison ONLY the worker's v0.2.0 binary: it crash-loops, reverts
        // itself, and the leader pauses the run.
        let (_, _, worker) = harness.plan_nodes().await;
        std::fs::write(
            harness.node(&worker).bin_dir.join("bun-v0.2.0.fail-boot"),
            "",
        )
        .unwrap();
        harness.start_upgrade().await
    };

    let (_, phase) = harness.watch_upgrade(true).await;
    assert!(phase.starts_with("Paused"), "expected a pause, got {phase}");

    // The worker reverted itself; nobody else was touched.
    let versions = harness.wait_for_versions("v0.1.0").await;
    assert_eq!(
        versions.len(),
        harness.nodes.len(),
        "unreachable nodes: {versions:?}"
    );
    assert!(
        versions.values().all(|v| v == "v0.1.0"),
        "only the worker should have been attempted: {versions:?}"
    );

    // Cure the binary and resume: the run finishes.
    std::fs::remove_file(harness.node(&worker).bin_dir.join("bun-v0.2.0.fail-boot")).unwrap();
    let leader = harness.wait_for_leader().await;
    let resume = harness
        .client
        .post(format!(
            "http://{}/v1/upgrade/resume",
            harness.node(&leader).api
        ))
        .body("")
        .send()
        .await
        .expect("resume");
    assert_eq!(resume.status().as_u16(), 202, "resume refused");

    let (_, phase) = harness.watch_upgrade(false).await;
    assert_eq!(phase, "Completed");
    let versions = harness.wait_for_versions("v0.2.0").await;
    assert_eq!(
        versions.len(),
        harness.nodes.len(),
        "unreachable nodes: {versions:?}"
    );
    assert!(
        versions.values().all(|v| v == "v0.2.0"),
        "cluster did not finish after resume: {versions:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and a multi-core host"]
async fn cluster_rollback_returns_every_node_to_previous_version() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 on a provisioned multi-core host"
    );
    let _serial = SERIAL.lock().await;
    let harness = ClusterHarness::start(4).await;

    harness.start_upgrade().await;
    let (_, phase) = harness.watch_upgrade(false).await;
    assert_eq!(phase, "Completed");
    harness.wait_for_versions("v0.2.0").await;

    // Roll the whole cluster back to v0.1.0.
    let (nodes, leader, _) = harness.plan_nodes().await;
    let request = serde_json::json!({ "target_version": "v0.1.0", "nodes": nodes });
    let response = harness
        .client
        .post(format!(
            "http://{}/v1/upgrade/cluster-rollback",
            harness.node(&leader).api
        ))
        .json(&request)
        .send()
        .await
        .expect("cluster rollback");
    assert_eq!(response.status().as_u16(), 202, "rollback refused");

    let (_, phase) = harness.watch_upgrade(false).await;
    assert_eq!(phase, "Completed");
    let versions = harness.wait_for_versions("v0.1.0").await;
    assert_eq!(
        versions.len(),
        harness.nodes.len(),
        "unreachable nodes: {versions:?}"
    );
    assert!(
        versions.values().all(|v| v == "v0.1.0"),
        "rollback incomplete: {versions:?}"
    );

    harness.shutdown().await;
}
