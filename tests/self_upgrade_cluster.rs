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

use std::collections::{HashMap, HashSet};
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
    service_token: String,
}

fn free_tcp_port(allocated: &mut HashSet<u16>) -> u16 {
    loop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        if allocated.insert(port) {
            return port;
        }
    }
}

fn free_udp_port(allocated: &mut HashSet<u16>) -> u16 {
    loop {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        if allocated.insert(port) {
            return port;
        }
    }
}

/// Copy the prepared executable as `bun-{version}` with its `.version` sidecar.
fn install_version(source: &Path, bin_dir: &Path, version: &str) {
    let target = bin_dir.join(format!("bun-{version}"));
    std::fs::copy(source, &target).unwrap();
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
        Self::start_with_keyless(count, &[]).await
    }

    /// Boot `count` nodes, leaving `upgrades.external_signing_key` out of
    /// the node.toml of the nodes whose index is in `keyless`. Those nodes
    /// refuse every cluster (network) upgrade directive.
    async fn start_with_keyless(count: usize, keyless: &[usize]) -> Self {
        let root = tempfile::tempdir().unwrap();
        // The registry deliberately caps each upload at 512 MiB. Debug symbols
        // can exceed that on CI; remove them from this private fixture only,
        // before copying, hashing or signing any candidate. Cargo's executable
        // stays intact for backtraces in the other suites.
        let source = root.path().join("bun-fixture");
        std::fs::copy(env!("CARGO_BIN_EXE_bun"), &source).unwrap();
        let stripped = std::process::Command::new("strip")
            .arg("-S")
            .arg(&source)
            .output()
            .expect("strip must be installed for cluster-upgrade acceptance");
        assert!(
            stripped.status.success(),
            "strip failed: {}",
            String::from_utf8_lossy(&stripped.stderr)
        );
        let fixture_size = std::fs::metadata(&source).unwrap().len();
        assert!(
            fixture_size <= 512 * 1024 * 1024,
            "upgrade fixture is {fixture_size} bytes, above the registry upload limit"
        );
        let (release_pkcs8, release_public) = signing::generate_keypair().unwrap();
        let (external_pkcs8, external_public) = signing::generate_keypair().unwrap();

        // The clustered registry fails closed: blob writes need the service
        // token derived from the cluster master key (or a minted user token).
        // Share one key file across the nodes so the harness can push the
        // upgrade binary the way a real operator's tooling would.
        let master_key = [42u8; 32];
        let master_key_path = root.path().join("master.key");
        std::fs::write(&master_key_path, hex::encode(master_key)).unwrap();
        let mut permissions = std::fs::metadata(&master_key_path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o600);
        std::fs::set_permissions(&master_key_path, permissions).unwrap();
        let service_token = reliaburger::sesame::token::derive_service_token(&master_key).unwrap();

        let mut nodes = Vec::new();
        let mut seed_gossip: Option<u16> = None;
        // Keep every endpoint numerically distinct for the lifetime of this
        // harness. Binding port 0 and immediately dropping the socket can
        // return the same number on the very next call; that made a node's
        // registry collide with its API or Raft listener and crash-loop.
        let mut allocated_ports = HashSet::new();

        for index in 0..count {
            let name = format!("n{index}");
            let node_root = root.path().join(&name);
            let bin_dir = node_root.join("bin");
            std::fs::create_dir_all(&bin_dir).unwrap();
            install_version(&source, &bin_dir, "v0.1.0");
            install_version(&source, &bin_dir, "v0.2.0");
            std::os::unix::fs::symlink("bun-v0.1.0", bin_dir.join("bun")).unwrap();

            let api_port = free_tcp_port(&mut allocated_ports);
            let gossip_port = free_udp_port(&mut allocated_ports);
            let raft_port = free_tcp_port(&mut allocated_ports);
            let reporting_port = free_tcp_port(&mut allocated_ports);
            let registry_port = free_tcp_port(&mut allocated_ports);
            let join = match seed_gossip {
                Some(seed) => format!("join = [\"127.0.0.1:{seed}\"]"),
                None => "join = []".to_string(),
            };
            if seed_gossip.is_none() {
                seed_gossip = Some(gossip_port);
            }

            let external_key_line = if keyless.contains(&index) {
                String::new()
            } else {
                format!(
                    "external_signing_key = \"{}\"",
                    encode_public_key(&external_public)
                )
            };
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

[security]
master_key_path = "{master_key}"

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
{external_key_line}
release_keys_override = ["{release}"]
boot_grace_secs = 2
gossip_rejoin_secs = 10
max_boot_attempts = 2
retain_versions = 3
"#,
                    node_root = node_root.display(),
                    master_key = master_key_path.display(),
                    bin = bin_dir.display(),
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
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            release_pkcs8,
            external_pkcs8,
            service_token,
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

    async fn leader_from(&self, node: &ClusterNode) -> Option<String> {
        let response = self
            .client
            .get(format!("http://{}/v1/cluster/council", node.api))
            .send()
            .await
            .ok()?;
        let council: serde_json::Value = response.json().await.ok()?;
        council["leader"].as_str().map(String::from)
    }

    /// The current leader's node name, from the bootstrap node's council
    /// view (the `leader` field is the node name).
    async fn leader(&self) -> Option<String> {
        self.leader_from(&self.nodes[0]).await
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

    /// Wait until the leader agrees that it is the leader and has archived
    /// the previous operation and rebuilt its live membership. Another node
    /// can observe completion earlier, but the next write must go through
    /// the leader's local Raft and membership state.
    async fn wait_for_idle_leader(&self) -> String {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(leader) = self.leader().await {
                let node = self.node(&leader);
                let leader_agrees = self.leader_from(node).await.as_deref() == Some(&leader);
                let upgrade_idle = self
                    .cluster_state_from(node)
                    .await
                    .is_some_and(|state| state["active"].is_null());
                let membership_ready = self.knows_every_member(node).await;
                if leader_agrees && upgrade_idle && membership_ready {
                    return leader;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for an idle upgrade coordinator"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Whether `node` sees every harness node alive AND has heard each one
    /// advertise its API endpoint. An upgrade plan is validated against
    /// exactly that view, and a leader that has just restarted learns its
    /// peers from a membership sync before their own gossip arrives: until
    /// then it would refuse the plan as "not advertised yet".
    async fn knows_every_member(&self, node: &ClusterNode) -> bool {
        let members = async {
            let response = self
                .client
                .get(format!("http://{}/v1/cluster/nodes", node.api))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?;
            response
                .json::<Vec<reliaburger::bun::agent::NodeStatus>>()
                .await
                .ok()
        }
        .await;
        members.is_some_and(|members| {
            self.nodes.iter().all(|expected| {
                members.iter().any(|member| {
                    member.node_id == expected.name
                        && member.state == "alive"
                        && member
                            .api_address
                            .is_some_and(|address| address.to_string() == expected.api)
                })
            })
        })
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
    fn plan_nodes_for(&self, leader: &str) -> (Vec<serde_json::Value>, String) {
        let worker = self
            .nodes
            .iter()
            .map(|n| n.name.clone())
            .find(|name| name != leader)
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
        (list, worker)
    }

    async fn plan_nodes(&self) -> (Vec<serde_json::Value>, String, String) {
        let leader = self.wait_for_idle_leader().await;
        let (list, worker) = self.plan_nodes_for(&leader);
        (list, leader, worker)
    }

    /// Sign + push the v0.2.0 binary to the leader's registry, start the
    /// upgrade, return (upgrade_id, leader, worker).
    async fn start_upgrade(&self) -> (String, String, String) {
        self.start_upgrade_fetching_from(None).await
    }

    /// [`Self::start_upgrade`], but tell the nodes to fetch the binary from
    /// `registry` (a proxy in front of the leader's) when given.
    async fn start_upgrade_fetching_from(
        &self,
        registry: Option<String>,
    ) -> (String, String, String) {
        let (nodes, leader, worker) = self.plan_nodes().await;
        let leader_node = self.node(&leader);
        let registry_address = registry.unwrap_or_else(|| leader_node.registry.clone());

        let bytes = std::fs::read(leader_node.bin_dir.join("bun-v0.2.0")).unwrap();
        let sha256 = signing::sha256_hex(&bytes);
        let push_url = format!(
            "http://{}/v2/reliaburger-bun/blobs/uploads/?digest=sha256:{sha256}",
            leader_node.registry
        );
        let push = self
            .client
            .post(&push_url)
            // Candidate uploads and hashing need a separate budget from the
            // five-second status requests, even without debug symbols.
            .timeout(Duration::from_secs(60))
            .header("authorization", format!("Bearer {}", self.service_token))
            .body(bytes.clone())
            .send()
            .await
            .expect("blob push");
        let status = push.status();
        assert!(
            status.is_success(),
            "blob push failed: {status}: {}",
            push.text().await.unwrap_or_default()
        );

        let request = serde_json::json!({
            "target_version": "v0.2.0",
            "binary_sha256": sha256,
            "embedded_signature": signing::sign(&self.release_pkcs8, &bytes).unwrap(),
            "external_signature": signing::sign(&self.external_pkcs8, &bytes).unwrap(),
            "parallel": 1,
            "registry_address": registry_address,
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
    async fn watch_upgrade(&self, upgrade_id: &str, allow_pause: bool) -> (Vec<String>, String) {
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
                Some(active) => {
                    let Some(observed_id) = active["upgrade_id"].as_str() else {
                        continue;
                    };
                    if !same_logical_upgrade(observed_id, upgrade_id) {
                        continue;
                    }
                    (active["nodes"].clone(), phase_label(&active["phase"]))
                }
                None => {
                    // A follower may still report no active operation just
                    // after the leader accepts the start. Only this run's
                    // archived record proves that it completed.
                    let Some(last) = cluster["history"].as_array().and_then(|history| {
                        history.iter().rev().find(|entry| {
                            entry["upgrade_id"]
                                .as_str()
                                .is_some_and(|id| same_logical_upgrade(id, upgrade_id))
                        })
                    }) else {
                        continue;
                    };
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

    async fn cluster_state_from(&self, node: &ClusterNode) -> Option<serde_json::Value> {
        let response = self
            .client
            .get(format!("http://{}/v1/upgrade/cluster", node.api))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.json().await.ok()
    }

    async fn cluster_state(&self) -> Option<serde_json::Value> {
        for node in &self.nodes {
            if let Some(value) = self.cluster_state_from(node).await {
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

    /// A relish client for `node`, authenticated as the cluster (the
    /// registry refuses anonymous blob writes).
    fn relish_client(&self, node: &ClusterNode) -> reliaburger::relish::client::BunClient {
        reliaburger::relish::client::BunClient::new_with_token(
            &format!("http://{}", node.api),
            Some(&self.service_token),
        )
    }

    /// Write `bytes` as `{dir}/bun-{version}` with a signed `.sig` envelope,
    /// the shape `relish upgrade start --binary` expects.
    fn signed_candidate(&self, dir: &Path, version: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(format!("bun-{version}"));
        std::fs::write(&path, bytes).unwrap();
        signing::SignatureEnvelope {
            schema: 1,
            sha256: signing::sha256_hex(bytes),
            embedded: signing::sign(&self.release_pkcs8, bytes).unwrap(),
            external: Some(signing::sign(&self.external_pkcs8, bytes).unwrap()),
        }
        .store(&dir.join(format!("bun-{version}.sig")))
        .unwrap();
        path
    }

    fn start_args(
        binary: PathBuf,
        allow_downgrade: bool,
    ) -> reliaburger::relish::upgrade::StartArgs {
        reliaburger::relish::upgrade::StartArgs {
            version: None,
            binary: Some(binary),
            sig: None,
            parallel: 1,
            registry: None,
            metadata_url: String::new(),
            node_addresses: Vec::new(),
            allow_downgrade,
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

fn same_logical_upgrade(observed: &str, started: &str) -> bool {
    observed == started || observed.starts_with(&format!("{started}-retry"))
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
    let (upgrade_id, old_leader, worker) = harness.start_upgrade().await;
    let (healthy_order, phase) = harness.watch_upgrade(&upgrade_id, false).await;
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

/// Forward exactly ONE TCP connection from a fresh loopback port to
/// `target`, then stop listening. Plays a quickstart host forward that the
/// nodes can't use: any later connection is refused.
async fn one_shot_forward(target: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let Ok((mut inbound, _)) = listener.accept().await else {
            return;
        };
        drop(listener);
        let Ok(mut outbound) = tokio::net::TcpStream::connect(&target).await else {
            return;
        };
        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
    });
    origin
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and a multi-core host"]
async fn relish_pushes_through_a_forward_while_nodes_fetch_from_the_cluster_address() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 on a provisioned multi-core host"
    );
    let _serial = SERIAL.lock().await;
    let harness = ClusterHarness::start(4).await;
    let leader = harness.wait_for_idle_leader().await;
    let leader_node = harness.node(&leader);

    // The quickstart shape: relish reaches the registry only through a
    // host forward, which means nothing to the nodes. It closes after the
    // push, so a node told to fetch from it would fail its download.
    let forward = one_shot_forward(leader_node.registry.clone()).await;
    let client = harness.relish_client(leader_node).with_service_endpoints(
        reliaburger::bun::capabilities::ServiceEndpoints {
            registry: Some(forward),
            ..Default::default()
        },
    );
    let staging = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(leader_node.bin_dir.join("bun-v0.2.0")).unwrap();
    let candidate = harness.signed_candidate(staging.path(), "v0.2.0", &bytes);

    reliaburger::relish::upgrade::start(&client, ClusterHarness::start_args(candidate, false))
        .await
        .expect("relish upgrade start");

    let recorded = harness
        .cluster_state()
        .await
        .expect("cluster upgrade state");
    let active = &recorded["active"];
    assert_eq!(
        active["registry_address"], leader_node.registry,
        "nodes must fetch from the leader's cluster registry address, not the forward"
    );
    let upgrade_id = active["upgrade_id"].as_str().unwrap().to_string();
    let (_, phase) = harness.watch_upgrade(&upgrade_id, false).await;
    assert_eq!(phase, "Completed");
    harness.wait_for_versions("v0.2.0").await;

    harness.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and a multi-core host"]
async fn start_refuses_same_version_other_bytes_and_unrequested_downgrades() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 on a provisioned multi-core host"
    );
    let _serial = SERIAL.lock().await;
    let harness = ClusterHarness::start(4).await;
    let leader = harness.wait_for_idle_leader().await;
    let leader_node = harness.node(&leader);
    let client = harness.relish_client(leader_node);
    let staging = tempfile::tempdir().unwrap();
    let running = std::fs::read(leader_node.bin_dir.join("bun-v0.1.0")).unwrap();

    // Same version, different bytes: refused before anything is recorded.
    let mut rebuilt = running.clone();
    rebuilt.extend_from_slice(b"\n# a different build of v0.1.0\n");
    let candidate = harness.signed_candidate(staging.path(), "v0.1.0", &rebuilt);
    let err =
        reliaburger::relish::upgrade::start(&client, ClusterHarness::start_args(candidate, false))
            .await
            .expect_err("a same-version candidate with other bytes must be refused");
    assert!(
        err.to_string().contains("different binary"),
        "unexpected refusal: {err}"
    );
    assert!(harness.cluster_state().await.unwrap()["active"].is_null());

    // Same version, same bytes: "already running", nothing recorded.
    let same = tempfile::tempdir().unwrap();
    let candidate = harness.signed_candidate(same.path(), "v0.1.0", &running);
    reliaburger::relish::upgrade::start(&client, ClusterHarness::start_args(candidate, false))
        .await
        .expect("identical bytes report already running");
    assert!(harness.cluster_state().await.unwrap()["active"].is_null());

    // An older version needs --allow-downgrade.
    let candidate = harness.signed_candidate(staging.path(), "v0.0.9", &running);
    let err =
        reliaburger::relish::upgrade::start(&client, ClusterHarness::start_args(candidate, false))
            .await
            .expect_err("a downgrade without the flag must be refused");
    assert!(
        err.to_string().contains("--allow-downgrade"),
        "unexpected refusal: {err}"
    );
    assert!(harness.cluster_state().await.unwrap()["active"].is_null());
    assert!(
        harness.versions().await.values().all(|v| v == "v0.1.0"),
        "no node may have moved"
    );

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

    let (upgrade_id, _, worker) = {
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

    let (_, phase) = harness.watch_upgrade(&upgrade_id, true).await;
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

    let (_, phase) = harness.watch_upgrade(&upgrade_id, false).await;
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

    let (upgrade_id, _, _) = harness.start_upgrade().await;
    let (_, phase) = harness.watch_upgrade(&upgrade_id, false).await;
    assert_eq!(phase, "Completed");
    harness.wait_for_versions("v0.2.0").await;

    // Roll the whole cluster back to v0.1.0.
    let leader = harness.wait_for_idle_leader().await;
    let (nodes, _) = harness.plan_nodes_for(&leader);
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
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(status.as_u16(), 202, "rollback refused: {body}");
    let body: serde_json::Value = serde_json::from_str(&body).expect("rollback response JSON");
    let rollback_id = body["upgrade_id"]
        .as_str()
        .expect("rollback response upgrade_id");

    let (_, phase) = harness.watch_upgrade(rollback_id, false).await;
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

/// The V02 soak's dead end: a node without an external key refused every
/// directive, the run paused, and the paused run blocked every later start.
/// The leader now asks each node first and refuses the start outright.
#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and a multi-core host"]
async fn start_refuses_when_a_node_cannot_verify_network_upgrades() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 on a provisioned multi-core host"
    );
    let _serial = SERIAL.lock().await;
    let harness = ClusterHarness::start_with_keyless(2, &[1]).await;
    let leader = harness.wait_for_idle_leader().await;
    let leader_node = harness.node(&leader);
    let client = harness.relish_client(leader_node);
    let staging = tempfile::tempdir().unwrap();
    let bytes = std::fs::read(leader_node.bin_dir.join("bun-v0.2.0")).unwrap();
    let candidate = harness.signed_candidate(staging.path(), "v0.2.0", &bytes);

    let err =
        reliaburger::relish::upgrade::start(&client, ClusterHarness::start_args(candidate, false))
            .await
            .expect_err("a node without an external key must stop the start");
    let message = err.to_string();
    assert!(
        message.contains("node n1") && message.contains("upgrades.external_signing_key"),
        "unexpected refusal: {message}"
    );
    assert!(
        harness.cluster_state().await.unwrap()["active"].is_null(),
        "nothing may be recorded for a refused start"
    );

    harness.shutdown().await;
}

/// A paused run is no longer a dead end: `relish upgrade abort` ends one
/// that moved no node, and `relish upgrade rollback` replaces one outright.
#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and a multi-core host"]
async fn paused_upgrade_can_be_aborted_or_replaced_by_a_rollback() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 on a provisioned multi-core host"
    );
    let _serial = SERIAL.lock().await;
    let harness = ClusterHarness::start(4).await;

    // Poison the worker's v0.2.0 so the first swap reverts and the run
    // pauses with every node still on v0.1.0.
    let (_, _, worker) = harness.plan_nodes().await;
    std::fs::write(
        harness.node(&worker).bin_dir.join("bun-v0.2.0.fail-boot"),
        "",
    )
    .unwrap();
    let (first_id, _, _) = harness.start_upgrade().await;
    let (_, phase) = harness.watch_upgrade(&first_id, true).await;
    assert!(phase.starts_with("Paused"), "expected a pause, got {phase}");
    harness.wait_for_versions("v0.1.0").await;

    // Abort: the paused run moves to history, marked Aborted.
    let leader = harness.wait_for_leader().await;
    let client = harness.relish_client(harness.node(&leader));
    reliaburger::relish::upgrade::abort(&client)
        .await
        .expect("abort a paused run that moved no node");
    let state = harness.cluster_state().await.unwrap();
    assert!(state["active"].is_null(), "abort left {}", state["active"]);
    let archived = state["history"]
        .as_array()
        .and_then(|history| history.last())
        .cloned()
        .unwrap_or_default();
    assert_eq!(archived["upgrade_id"], first_id);
    assert_eq!(phase_label(&archived["phase"]), "Aborted");

    // With the slot free, a new start is accepted (and pauses again on the
    // still-poisoned worker)...
    harness.wait_for_idle_leader().await;
    let (second_id, _, _) = harness.start_upgrade().await;
    assert_ne!(second_id, first_id);
    let (_, phase) = harness.watch_upgrade(&second_id, true).await;
    assert!(phase.starts_with("Paused"), "expected a pause, got {phase}");
    harness.wait_for_versions("v0.1.0").await;

    // ...and a cluster rollback replaces the paused run instead of being
    // refused with "already in progress".
    let leader = harness.wait_for_leader().await;
    wait_for("the leader to know every node's API", WAIT, || async {
        harness.knows_every_member(harness.node(&leader)).await
    })
    .await;
    let client = harness.relish_client(harness.node(&leader));
    reliaburger::relish::upgrade::rollback(&client, Some("v0.1.0".to_string()), Vec::new())
        .await
        .expect("rollback replaces a paused run");
    wait_for("the rollback to finish", WAIT, || async {
        harness.cluster_state().await.is_some_and(|state| {
            state["active"].is_null()
                && state["history"].as_array().is_some_and(|history| {
                    history.iter().any(|entry| {
                        entry["upgrade_id"] == second_id
                            && phase_label(&entry["phase"]) == "Aborted"
                    }) && history.last().is_some_and(|entry| {
                        entry["upgrade_id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with("rollback-v0.1.0"))
                            && phase_label(&entry["phase"]) == "Completed"
                    })
                })
        })
    })
    .await;
    let versions = harness.wait_for_versions("v0.1.0").await;
    assert_eq!(versions.len(), harness.nodes.len(), "{versions:?}");

    harness.shutdown().await;
}

/// A TCP proxy to `upstream` that hangs up on every connection for the
/// first `outage`, the way a registry looks while its bun restarts. Returns
/// its address and how many connections it hung up on.
async fn registry_with_outage(
    upstream: String,
    outage: Duration,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let hangups = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hangups.clone();
    let back_at = tokio::time::Instant::now() + outage;
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                return;
            };
            if tokio::time::Instant::now() < back_at {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(inbound);
                continue;
            }
            let upstream = upstream.clone();
            tokio::spawn(async move {
                if let Ok(mut outbound) = tokio::net::TcpStream::connect(&upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    (address, hangups)
}

/// V02 soak: a leader bun was SIGKILLed mid-walk, came back, and directed
/// the next node before its own registry was listening. The node's binary
/// fetch failed and that single blip paused the run. A registry that is
/// down when the directive lands must be ridden out, not paused on.
#[tokio::test]
#[ignore = "requires RELIABURGER_UPGRADE_TESTS=1 and a multi-core host"]
async fn a_registry_outage_at_directive_time_does_not_pause_the_upgrade() {
    assert!(
        upgrade_tests_enabled(),
        "set RELIABURGER_UPGRADE_TESTS=1 on a provisioned multi-core host"
    );
    let _serial = SERIAL.lock().await;
    let harness = ClusterHarness::start(4).await;
    let leader = harness.wait_for_idle_leader().await;

    // Longer than a node's own fetch budget (10 s), so the node answers
    // the first directive 503 and the orchestrator has to re-send it.
    let (proxy, hangups) = registry_with_outage(
        harness.node(&leader).registry.clone(),
        Duration::from_secs(25),
    )
    .await;
    let (upgrade_id, _, _) = harness.start_upgrade_fetching_from(Some(proxy)).await;
    let (_, phase) = harness.watch_upgrade(&upgrade_id, true).await;

    assert_eq!(phase, "Completed", "the outage paused the upgrade");
    assert!(
        hangups.load(std::sync::atomic::Ordering::SeqCst) > 1,
        "no fetch hit the outage, so the test proved nothing"
    );
    harness.wait_for_versions("v0.2.0").await;
    harness.shutdown().await;
}
