//! Binary-driven integration tests for cluster scheduling (Stage 4 W6,
//! L1). Three real nodes: gossip + Raft + reporting + a real BunAgent
//! (ProcessGrill) + the HTTP API + the leader scheduler, membership
//! refresher, and placement reconciler — the exact wiring `bun --cluster`
//! runs. Apply an app on one node; assert replicas spread across the
//! cluster.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::agent::ClusterHandle;
use reliaburger::bun::api::NodeMembershipInfo;
use reliaburger::relish::client::BunClient;
use tokio::sync::{RwLock, mpsc, watch};
use tokio_util::sync::CancellationToken;

#[path = "support/cluster.rs"]
mod cluster_support;
use cluster_support::{MembershipSource, WiredNode, WiredNodeOptions, local, start_wired_node};

/// How often `wait_until` re-checks its condition in this binary.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

async fn wait_until(timeout: Duration, cond: impl FnMut() -> bool) -> bool {
    cluster_support::wait_until(timeout, POLL_INTERVAL, cond).await
}

/// A fully wired node: everything `bun --cluster` starts, on one host
/// with per-node port blocks (gossip, +1 raft, +2 reporting, +3 API).
struct Node {
    name: String,
    client: BunClient,
    handle: ClusterHandle,
    thinks_leader: watch::Receiver<bool>,
    membership_table: Arc<RwLock<Vec<NodeMembershipInfo>>>,
    token_store: Option<reliaburger::sesame::auth::TokenStore>,
    rollup_store: Arc<RwLock<reliaburger::mayo::rollup_store::RollupStore>>,
    _wired: WiredNode,
}

#[derive(Clone)]
struct NodeFaultAuth {
    token: reliaburger::sesame::types::ApiToken,
    plaintext: String,
}

impl NodeFaultAuth {
    /// A fresh unscoped admin token, shared by every node in the test cluster.
    fn admin(name: &str) -> Self {
        let created = reliaburger::sesame::token::create_token(
            name,
            reliaburger::sesame::types::ApiRole::Admin,
            reliaburger::sesame::types::TokenScope::default(),
            None,
        )
        .unwrap();
        Self {
            token: created.token,
            plaintext: created.plaintext,
        }
    }
}

/// Cut `node` off from `peers` on the gossip and Raft transports.
async fn partition(
    node: &Node,
    peers: &[String],
) -> Result<reliaburger::smoker::types::FaultSummary, reliaburger::relish::RelishError> {
    node.client
        .inject_fault(&reliaburger::smoker::types::FaultRequest {
            fault_type: reliaburger::smoker::types::FaultType::CouncilPartition {
                peers: peers.to_vec(),
            },
            target_service: String::new(),
            namespace: None,
            target_instance: None,
            target_node: Some(node.name.clone()),
            duration: Duration::from_secs(60),
            injected_by: String::new(),
            reason: None,
            include_leader: true,
            override_safety: false,
            acknowledged: true,
        })
        .await
}

async fn start_node(
    name: &str,
    gossip_port: u16,
    seeds: Vec<SocketAddr>,
    shutdown: &CancellationToken,
) -> Node {
    start_node_with_auth(name, gossip_port, seeds, shutdown, None).await
}

async fn start_node_with_auth(
    name: &str,
    gossip_port: u16,
    seeds: Vec<SocketAddr>,
    shutdown: &CancellationToken,
    auth: Option<NodeFaultAuth>,
) -> Node {
    let wired = start_wired_node(WiredNodeOptions {
        name: name.to_string(),
        gossip_port,
        seeds,
        shutdown: shutdown.clone(),
        data_dir_prefix: "rb-placement",
        stale_report_timeout_secs: 30,
        metrics_rollup: Some(Duration::from_millis(500)),
        // Fast learning period so the test doesn't wait long.
        scheduler: Some(reliaburger::config::node::ReconstructionSection {
            report_threshold_percent: 95,
            learning_period_timeout_secs: 2,
            large_cluster_timeout_secs: 4,
            large_cluster_node_count: 5000,
        }),
        lease_reaper: true,
        membership: MembershipSource::Gossip,
        service_identity: auth
            .as_ref()
            .map(|_| "placement-test-internal-service-identity".to_string()),
        operator_token: auth.as_ref().map(|auth| auth.token.clone()),
        fault_injection: auth.is_some(),
    })
    .await;

    let client = BunClient::new_with_token(
        &format!("http://127.0.0.1:{}", wired.api_port),
        auth.as_ref().map(|auth| auth.plaintext.as_str()),
    );
    for _ in 0..40 {
        if client.health().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The agent owns the real `ClusterHandle`; rebuild a thin stand-in for
    // the test's own council and partition checks.
    Node {
        name: name.to_string(),
        client,
        handle: ClusterHandle {
            local_node_id: reliaburger::meat::NodeId::new(name),
            membership_rx: wired.membership_rx.clone(),
            raft_metrics_rx: None,
            council: Some(Arc::clone(&wired.council)),
            snapshot_rx: mpsc::channel(1).1,
            wrapping_ikm: None,
            partition_blocklists: wired.partition_blocklists.clone(),
            crl_handle: Default::default(),
        },
        thinks_leader: wired.thinks_leader.clone(),
        membership_table: Arc::clone(&wired.membership_table),
        token_store: wired.token_store.clone(),
        rollup_store: Arc::clone(&wired.rollup_store),
        _wired: wired,
    }
}

/// Total instances across all three nodes' `/v1/status`.
async fn total_instances(nodes: &[&Node]) -> usize {
    let mut total = 0;
    for node in nodes {
        if let Ok(statuses) = node.client.status().await {
            total += statuses.iter().filter(|s| s.app_name == "web").count();
        }
    }
    total
}

/// How many distinct nodes are running at least one "web" instance.
async fn nodes_running_web(nodes: &[&Node]) -> usize {
    let mut count = 0;
    for node in nodes {
        if let Ok(statuses) = node.client.status().await
            && statuses.iter().any(|s| s.app_name == "web")
        {
            count += 1;
        }
    }
    count
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn apply_on_any_node_places_across_the_cluster() {
    let shutdown = CancellationToken::new();

    let n1 = start_node("p1", 18441, vec![], &shutdown).await;
    let n2 = start_node("p2", 18445, vec![local(18441)], &shutdown).await;
    let n3 = start_node("p3", 18449, vec![local(18441)], &shutdown).await;
    let nodes = [&n1, &n2, &n3];

    // Wait for a leader to emerge and reports to arrive (the scheduler
    // needs capacity data before it can place).
    let ready = wait_until(Duration::from_secs(30), || {
        nodes.iter().any(|n| *n.thinks_leader.borrow())
    })
    .await;
    assert!(ready, "no leader elected");
    tokio::time::sleep(Duration::from_secs(3)).await; // let reports land

    // Apply a 3-replica app. Bound the call so a hung stream can't
    // wedge the test; the deploy proceeds asynchronously regardless.
    let config = reliaburger::config::Config::parse(
        r#"
        [app.web]
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
        replicas = 3
    "#,
    )
    .unwrap();
    // Apply via a follower to also exercise leader-forwarding, then wait
    // for each node's reconciler to start its share — three instances
    // spread across three distinct nodes. Under load a single apply can
    // race leadership/forwarding and be dropped, so re-apply periodically:
    // the spec is desired state, so re-applying is idempotent.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut spread = false;
    let mut last_apply: Option<tokio::time::Instant> = None;
    while tokio::time::Instant::now() < deadline {
        if total_instances(&nodes).await == 3 && nodes_running_web(&nodes).await == 3 {
            spread = true;
            break;
        }
        if last_apply.is_none_or(|t| t.elapsed() >= Duration::from_secs(8)) {
            let applier = nodes
                .iter()
                .find(|n| !*n.thinks_leader.borrow())
                .or_else(|| nodes.first())
                .expect("at least one node");
            let _ =
                tokio::time::timeout(Duration::from_secs(15), applier.client.apply(&config)).await;
            last_apply = Some(tokio::time::Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if !spread {
        // Diagnostics: has the spec reached Raft, and has the leader
        // produced placements?
        for n in &nodes {
            if let Some(council) = &n.handle.council {
                let ds = council.desired_state().await;
                eprintln!(
                    "node {}: leader={} apps={:?} placements={:?}",
                    n.name,
                    *n.thinks_leader.borrow(),
                    ds.apps.keys().collect::<Vec<_>>(),
                    ds.scheduling
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.len()))
                        .collect::<Vec<_>>(),
                );
            }
        }
        let placed = total_instances(&nodes).await;
        let distinct = nodes_running_web(&nodes).await;
        panic!("expected 3 instances across 3 nodes; got {placed} across {distinct}");
    }

    // Idempotency: after convergence the reconcilers must not thrash —
    // instance counts stay put across several more reconcile cycles.
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        total_instances(&nodes).await,
        3,
        "reconcilers must not create extra instances once converged"
    );

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.handle.council {
            c.shutdown().await.ok();
        }
        let _ = &n.name;
    }
}

/// How many "web" instances are in a *live* state (not stopped/failed)
/// across all nodes. A cluster stop leaves the stopped instance in the
/// status list (terminal states are only filtered from reporting, CP6), so
/// "torn down" means no live replica, not an empty list.
async fn live_web_instances(nodes: &[&Node]) -> usize {
    let mut count = 0;
    for node in nodes {
        if let Ok(statuses) = node.client.status().await {
            count += statuses
                .iter()
                .filter(|s| {
                    s.app_name == "web"
                        && !matches!(s.state.as_str(), "stopped" | "stopping" | "failed")
                })
                .count();
        }
    }
    count
}

/// DEP2: a cluster stop clears desired state through Raft, so the app is
/// gone from every council's `desired_state()` and no reconciler resurrects
/// it on the next tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn cluster_stop_scales_to_zero_until_apply_and_delete_removes_the_app() {
    let shutdown = CancellationToken::new();

    let n1 = start_node("s1", 18461, vec![], &shutdown).await;
    let n2 = start_node("s2", 18465, vec![local(18461)], &shutdown).await;
    let n3 = start_node("s3", 18469, vec![local(18461)], &shutdown).await;
    let nodes = [&n1, &n2, &n3];

    let ready = wait_until(Duration::from_secs(30), || {
        nodes.iter().any(|n| *n.thinks_leader.borrow())
    })
    .await;
    assert!(ready, "no leader elected");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let config = reliaburger::config::Config::parse(
        r#"
        [app.web]
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
        replicas = 1
    "#,
    )
    .unwrap();

    // Get the app running somewhere, re-applying idempotently under load.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut placed = false;
    let mut last_apply: Option<tokio::time::Instant> = None;
    while tokio::time::Instant::now() < deadline {
        if total_instances(&nodes).await >= 1 && nodes_running_web(&nodes).await >= 1 {
            placed = true;
            break;
        }
        if last_apply.is_none_or(|t| t.elapsed() >= Duration::from_secs(8)) {
            let applier = nodes.first().expect("a node");
            let _ =
                tokio::time::timeout(Duration::from_secs(15), applier.client.apply(&config)).await;
            last_apply = Some(tokio::time::Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(placed, "app never came up to stop");

    // Stop it through any node (exercises leader-forwarding of the delete).
    let stopper = nodes.first().expect("a node");
    tokio::time::timeout(
        Duration::from_secs(15),
        stopper.client.stop("web", "default"),
    )
    .await
    .expect("stop did not hang")
    .expect("stop succeeded");

    // Desired state clears on the leader, and it stays clear: give the
    // reconcilers several ticks to (not) resurrect the app. "Torn down"
    // means no *live* replica remains anywhere.
    let torn_down_by = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut cleared = false;
    while tokio::time::Instant::now() < torn_down_by {
        if live_web_instances(&nodes).await == 0 {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(cleared, "the stopped app was not torn down");

    // Stop keeps the spec, marked stopped, and nothing resurrects it.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let app = reliaburger::meat::AppId::new("web", "default");
    for n in &nodes {
        if let Some(council) = &n.handle.council {
            let ds = council.desired_state().await;
            assert!(
                ds.apps.contains_key(&app) && ds.stopped_apps.contains(&app),
                "node {}: stop should keep the spec and mark the app stopped",
                n.name
            );
        }
    }
    assert_eq!(
        live_web_instances(&nodes).await,
        0,
        "a reconciler resurrected the stopped app"
    );

    // Applying the same config straight away brings it back.
    tokio::time::timeout(Duration::from_secs(15), stopper.client.apply(&config))
        .await
        .expect("apply did not hang")
        .expect("apply succeeded");
    let back_by = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < back_by && live_web_instances(&nodes).await == 0 {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        live_web_instances(&nodes).await >= 1,
        "apply after stop did not start the app again"
    );

    // Delete removes it from desired state for good.
    tokio::time::timeout(
        Duration::from_secs(15),
        stopper.client.delete("web", "default"),
    )
    .await
    .expect("delete did not hang")
    .expect("delete succeeded");
    let gone_by = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < gone_by && live_web_instances(&nodes).await > 0 {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tokio::time::sleep(Duration::from_secs(6)).await;
    for n in &nodes {
        if let Some(council) = &n.handle.council {
            let ds = council.desired_state().await;
            assert!(
                !ds.apps.contains_key(&app) && !ds.stopped_apps.contains(&app),
                "node {}: desired state still holds the deleted app",
                n.name
            );
        }
    }
    assert_eq!(
        live_web_instances(&nodes).await,
        0,
        "the deleted app still runs"
    );

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.handle.council {
            c.shutdown().await.ok();
        }
        let _ = &n.name;
    }
}

/// Total "scaler" instances across all nodes.
async fn total_scaler_instances(nodes: &[&Node]) -> usize {
    let mut total = 0;
    for node in nodes {
        if let Ok(statuses) = node.client.status().await {
            total += statuses.iter().filter(|s| s.app_name == "scaler").count();
        }
    }
    total
}

/// Start three placement nodes on `base`, `base + 4` and `base + 8`, wait
/// for a leader, and deploy `config` until the single "scaler" replica
/// places.
async fn start_autoscale_cluster(
    base: u16,
    config: &reliaburger::config::Config,
    shutdown: &CancellationToken,
) -> [Node; 3] {
    let n1 = start_node("s1", base, vec![], shutdown).await;
    let n2 = start_node("s2", base + 4, vec![local(base)], shutdown).await;
    let n3 = start_node("s3", base + 8, vec![local(base)], shutdown).await;
    let nodes = [n1, n2, n3];

    let ready = wait_until(Duration::from_secs(30), || {
        nodes.iter().any(|n| *n.thinks_leader.borrow())
    })
    .await;
    assert!(ready, "no leader elected");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Apply and wait for the single replica to place, re-applying
    // periodically in case a first attempt races leadership under load
    // (the spec is desired state, so re-applying is idempotent).
    let refs = [&nodes[0], &nodes[1], &nodes[2]];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut last_apply: Option<tokio::time::Instant> = None;
    while tokio::time::Instant::now() < deadline {
        if total_scaler_instances(&refs).await >= 1 {
            break;
        }
        if last_apply.is_none_or(|t| t.elapsed() >= Duration::from_secs(8)) {
            let applier = refs
                .iter()
                .find(|n| *n.thinks_leader.borrow())
                .or_else(|| refs.first())
                .expect("at least one node");
            let _ =
                tokio::time::timeout(Duration::from_secs(15), applier.client.apply(config)).await;
            last_apply = Some(tokio::time::Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    nodes
}

/// Wait up to `timeout` for the "scaler" app to reach `want` instances;
/// on failure print every node's autoscale overrides and panic.
async fn assert_scaler_reaches(nodes: &[&Node], want: usize, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if total_scaler_instances(nodes).await >= want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    for n in nodes {
        if let Some(c) = &n.handle.council {
            let ds = c.desired_state().await;
            eprintln!("node {}: overrides={:?}", n.name, ds.autoscale_overrides);
        }
    }
    panic!(
        "autoscaler did not scale up; instances: {}",
        total_scaler_instances(nodes).await
    );
}

/// W8 (L3): a high CPU reading drives the autoscaler to raise the replica
/// override, and the scheduler + reconcilers grow the app to max.
///
/// Feeds the leader's rollup store directly with the series the node
/// collector really records: `process_cpu_percent`, percent of one core,
/// labelled like `mayo::collector` labels it. The app declares no CPU
/// request (ProcessGrill refuses one), so utilisation is measured against a
/// whole core: 90% of a core is well above the 50% target.
/// This test once fed a fake series named `cpu` that nothing in production
/// records, which let it pass while the feature never fired.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn autoscaler_scales_up_on_high_metric() {
    use reliaburger::mayo::rollup::{NodeRollup, RollupAggregate, RollupEntry};

    let shutdown = CancellationToken::new();
    // A 1-replica app that autoscales on cpu, target 50%, max 3.
    let config = reliaburger::config::Config::parse(
        r#"
        [app.scaler]
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
        replicas = 1

        [app.scaler.autoscale]
        metric = "cpu"
        target = "50%"
        min = 1
        max = 3
        cooldown = "0s"
    "#,
    )
    .unwrap();
    let [n1, n2, n3] = start_autoscale_cluster(18541, &config, &shutdown).await;
    let nodes = [&n1, &n2, &n3];

    // The leader's autoscaler reads its own store; feed every node's so it
    // doesn't matter which one leads.
    for node in &nodes {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // The collector labels per-app metrics `namespace/app`, and the
        // autoscaler match is namespace-qualified (M26).
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("app".to_string(), "default/scaler".to_string());
        labels.insert("namespace".to_string(), "default".to_string());
        labels.insert("instance".to_string(), "default__scaler-0".to_string());
        labels.insert("node".to_string(), node.name.clone());
        let rollup = NodeRollup {
            node_id: reliaburger::meat::NodeId::new(&node.name),
            timestamp: now.saturating_sub(30),
            entries: vec![RollupEntry {
                metric_name: "process_cpu_percent".to_string(),
                labels,
                aggregate: RollupAggregate {
                    min: 90.0,
                    max: 90.0,
                    sum: 90.0,
                    count: 1,
                },
            }],
        };
        let mut w = node.rollup_store.write().await;
        w.ingest(&rollup);
        w.flush().await.unwrap();
    }

    assert_scaler_reaches(&nodes, 3, Duration::from_secs(30)).await;

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.handle.council {
            c.shutdown().await.ok();
        }
    }
}

/// The autoscaler fires from the REAL metrics path, with nothing faked:
/// a ProcessGrill replica busy-loops, each node samples its instances with
/// the same `SystemCollector::collect_agent_instance_metrics` call Bun's
/// collection loop makes, the real rollup worker ships the per-minute
/// aggregates to the leader, and the leader's autoscaler scales the app.
///
/// The only glue the test supplies is the one-second tick that Bun runs in
/// its binary. Rollups cover the previous COMPLETE minute, so the first
/// signal reaches the leader 60–120 s after the burn starts; hence the long
/// timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn autoscaler_scales_up_from_real_collector_cpu() {
    let shutdown = CancellationToken::new();
    // One busy-looping shell burns ~100% of a core; with no CPU request
    // that is ~4x the 25% target, headroom for a loaded CI host.
    let config = reliaburger::config::Config::parse(
        r#"
        [app.scaler]
        image = "proc-grill:image-ignored"
        command = ["sh", "-c", "while :; do :; done"]
        replicas = 1

        [app.scaler.autoscale]
        metric = "cpu"
        target = "25%"
        min = 1
        max = 2
        evaluation_window = "3m"
        cooldown = "0s"
    "#,
    )
    .unwrap();
    let [n1, n2, n3] = start_autoscale_cluster(27341, &config, &shutdown).await;
    let nodes = [&n1, &n2, &n3];

    for node in &nodes {
        let mayo = node._wired.mayo.clone().expect("placement nodes run Mayo");
        let agent = node._wired.cmd_tx.clone();
        let name = node.name.clone();
        let stop = shutdown.clone();
        tokio::spawn(async move {
            let mut collector = reliaburger::mayo::collector::SystemCollector::new();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tick.tick() => {}
                }
                collector.refresh();
                let samples = collector
                    .collect_agent_instance_metrics(&agent, &name)
                    .await;
                let mut store = mayo.write().await;
                for sample in &samples {
                    store.insert_now(&sample.key, sample.value);
                }
            }
        });
    }

    // Max is 2 so the test burns at most two cores while it runs.
    assert_scaler_reaches(&nodes, 2, Duration::from_secs(240)).await;

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.handle.council {
            c.shutdown().await.ok();
        }
    }
}

/// W11 (L14): the quorum safety rail rejects a node-level fault that
/// would risk Raft majority. On a 3-member council `max_allowed = 1`, so
/// fully isolating one follower is accepted, but a second partition while
/// that voter is gone from the live view is refused by the quorum rail.
/// Drives the real transport-blocklist path through a council partition;
/// a service-to-service eBPF partition does not affect Raft quorum.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn fault_injection_rejected_when_quorum_at_risk() {
    let auth = NodeFaultAuth::admin("partition-admin");
    let shutdown = CancellationToken::new();
    let n1 = start_node_with_auth("r1", 18741, vec![], &shutdown, Some(auth.clone())).await;
    let n2 = start_node_with_auth(
        "r2",
        18745,
        vec![local(18741)],
        &shutdown,
        Some(auth.clone()),
    )
    .await;
    let n3 = start_node_with_auth("r3", 18749, vec![local(18741)], &shutdown, Some(auth)).await;
    let nodes = [&n1, &n2, &n3];

    // Wait for a 3-member council so `council_size == 3`.
    let ready = wait_until(Duration::from_secs(30), || {
        nodes.iter().any(|n| *n.thinks_leader.borrow())
    })
    .await;
    assert!(ready, "no leader elected");
    // Wait for the council to actually grow to 3 voters. The self-healing
    // reconciler admits members one action per tick with a stability
    // window, so growth takes several ticks rather than one.
    let grown = wait_until(Duration::from_secs(60), || {
        nodes.iter().any(|n| {
            n.handle.council.as_ref().is_some_and(|c| {
                c.metrics()
                    .borrow()
                    .membership_config
                    .membership()
                    .voter_ids()
                    .count()
                    >= 3
            })
        })
    })
    .await;
    assert!(grown, "council did not grow to 3 voters");

    let leader = nodes
        .iter()
        .find(|n| *n.thinks_leader.borrow())
        .expect("leader exists");

    let mut followers = nodes.iter().filter(|node| node.name != leader.name);
    let isolated = followers.next().expect("leader has a first follower");
    let other = followers.next().expect("leader has a second follower");

    // First node-level fault: cut one follower off from both peers. One
    // unavailable voter is within the quorum budget, so it's accepted.
    partition(isolated, &[leader.name.clone(), other.name.clone()])
        .await
        .expect("first partition should be within the quorum budget");

    // The quorum rail counts voters missing from the leader's live API
    // membership. Until SWIM drops the isolated follower, the only thing
    // refusing a second fault is the single-reservation rule, which would
    // let this test pass without the quorum rail.
    assert!(
        wait_until_api_view_drops(leader, &isolated.name, Duration::from_secs(30)).await,
        "{} never dropped the isolated {} from its API membership",
        leader.name,
        isolated.name
    );

    // Second node-level fault: would take a second voter of the 3-member
    // council out, so the quorum rail must reject it.
    let rejected = partition(leader, std::slice::from_ref(&other.name)).await;
    assert_quorum_refusal(&rejected);

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.handle.council {
            c.shutdown().await.ok();
        }
    }
}

/// Assert that a node fault was refused by the quorum rail specifically.
///
/// Other refusals (one node fault already holding the cluster reservation,
/// an unknown leader) would also stop the fault, but they'd pass this test
/// even if the quorum rail were deleted.
fn assert_quorum_refusal<T: std::fmt::Debug>(result: &Result<T, reliaburger::relish::RelishError>) {
    let quorum = matches!(
        result,
        Err(reliaburger::relish::RelishError::ApiError { status: 400, body })
            // Under CI load the leader can briefly suspect a healthy voter too,
            // so it may count one or two affected; either way it's the quorum rail.
            if body.contains("quorum risk: ") && body.contains("council nodes already affected, max allowed is 1")
    );
    assert!(
        quorum,
        "expected the quorum rail's 400 refusal, got {result:?}"
    );
}

/// Wait until `observer`'s API membership table (what the fault safety
/// rails read) no longer lists `target`.
async fn wait_until_api_view_drops(observer: &Node, target: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let listed = observer
            .membership_table
            .read()
            .await
            .iter()
            .any(|member| member.node_id.0 == target);
        if !listed {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// One node's view of another's SWIM state, by name.
fn peer_state(observer: &Node, target: &str) -> Option<reliaburger::mustard::state::NodeState> {
    observer
        .handle
        .membership_rx
        .borrow()
        .iter()
        .find(|m| m.node_id.0 == target)
        .map(|m| m.state)
}

/// W11 (L15): a chaos partition populates the real gossip + Raft
/// transport blocklists, so the isolated node stops answering SWIM
/// probes and its peers mark it Dead. Healing clears the blocklists and
/// the node rejoins. This drives the binary path: a council partition fault
/// on the isolated node, membership observed through the peers' watch.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn partition_isolates_a_node_for_real() {
    use reliaburger::mustard::state::NodeState;

    let auth = NodeFaultAuth::admin("partition-admin");
    let shutdown = CancellationToken::new();
    let n1 = start_node_with_auth("q1", 18641, vec![], &shutdown, Some(auth.clone())).await;
    let n2 = start_node_with_auth(
        "q2",
        18645,
        vec![local(18641)],
        &shutdown,
        Some(auth.clone()),
    )
    .await;
    let n3 = start_node_with_auth("q3", 18649, vec![local(18641)], &shutdown, Some(auth)).await;
    let nodes = [&n1, &n2, &n3];

    // Gossip can converge before Raft has elected a stable three-voter council.
    // The reservation endpoint correctly refuses during that bootstrap window.
    let converged = wait_until(Duration::from_secs(30), || {
        nodes.iter().all(|obs| {
            ["q1", "q2", "q3"]
                .iter()
                .all(|t| peer_state(obs, t) == Some(NodeState::Alive))
                && obs.handle.council.as_ref().is_some_and(|council| {
                    let metrics = council.metrics().borrow().clone();
                    metrics.current_leader.is_some()
                        && metrics.membership_config.membership().voter_ids().count() == 3
                        && metrics
                            .membership_config
                            .membership()
                            .get_joint_config()
                            .len()
                            == 1
                })
        })
    })
    .await;
    assert!(
        converged,
        "gossip and the three-voter council never fully converged"
    );

    // Cut q3 off from q1 and q2. The partition is injected ON q3, whose
    // agent holds the real blocklist handles; the transport drops traffic
    // both to and from the blocked peers, so detection is symmetric.
    let fault = partition(&n3, &["q1".to_string(), "q2".to_string()])
        .await
        .expect("partition injection should succeed");

    // q1 must stop seeing q3 as Alive within the SWIM failure-detection
    // window. The membership snapshot drops down nodes, so a confirmed-Dead
    // q3 disappears from the view entirely (`None`) — either way it is no
    // longer a live peer.
    let detected = wait_until(Duration::from_secs(30), || {
        peer_state(&n1, "q3") != Some(NodeState::Alive)
    })
    .await;
    assert!(
        detected,
        "q1 never dropped the partitioned q3 from its live view; saw {:?}",
        peer_state(&n1, "q3")
    );

    // Heal: clear q3's blocklists and it should rejoin and be Alive again.
    n3.client
        .clear_fault(fault.id, Some("q3"), true)
        .await
        .expect("heal should succeed");

    let recovered = wait_until(Duration::from_secs(30), || {
        peer_state(&n1, "q3") == Some(NodeState::Alive)
    })
    .await;
    assert!(
        recovered,
        "q3 never recovered to Alive after healing; saw {:?}",
        peer_state(&n1, "q3")
    );

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.handle.council {
            c.shutdown().await.ok();
        }
    }
}

/// Phase 15 M8: an authenticated node-kill request reaches the named node,
/// closes all three cluster transports, becomes externally observable as
/// node failure, and is manually reversible through that node's still-live
/// management API.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node node-failure acceptance; run with make test-cluster"]
async fn authenticated_node_kill_fails_and_restores_a_real_cluster_member() {
    use reliaburger::mustard::state::NodeState;
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let auth = NodeFaultAuth::admin("chaos-admin");
    let shutdown = CancellationToken::new();
    let n1 = start_node_with_auth("f1", 18941, vec![], &shutdown, Some(auth.clone())).await;
    let n2 = start_node_with_auth(
        "f2",
        18945,
        vec![local(18941)],
        &shutdown,
        Some(auth.clone()),
    )
    .await;
    let n3 = start_node_with_auth("f3", 18949, vec![local(18941)], &shutdown, Some(auth)).await;
    let nodes = [&n1, &n2, &n3];

    let converged = wait_until(Duration::from_secs(30), || {
        nodes.iter().all(|observer| {
            ["f1", "f2", "f3"]
                .iter()
                .all(|target| peer_state(observer, target) == Some(NodeState::Alive))
        })
    })
    .await;
    assert!(converged, "cluster never fully converged to Alive");
    let voters_ready = wait_until(Duration::from_secs(60), || {
        nodes.iter().all(|node| {
            node.handle.council.as_ref().is_some_and(|council| {
                council
                    .metrics()
                    .borrow()
                    .membership_config
                    .membership()
                    .voter_ids()
                    .count()
                    == 3
            })
        })
    })
    .await;
    assert!(voters_ready, "council never grew to three voters");

    let source = nodes
        .iter()
        .find(|node| *node.thinks_leader.borrow())
        .copied()
        .expect("leader exists");
    let target = nodes
        .iter()
        .find(|node| node.name != source.name)
        .copied()
        .expect("follower exists");
    let request = FaultRequest {
        fault_type: FaultType::NodeKill {
            kill_containers: false,
        },
        target_service: String::new(),
        namespace: None,
        target_instance: None,
        target_node: Some(target.name.clone()),
        duration: Duration::from_secs(60),
        injected_by: "untrusted-body".to_string(),
        reason: Some("node failure acceptance".to_string()),
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    };
    let summary = source
        .client
        .inject_fault(&request)
        .await
        .expect("authorised node fault should reach its target");

    let failed = wait_until(Duration::from_secs(30), || {
        nodes
            .iter()
            .filter(|observer| observer.name != target.name)
            .all(|observer| peer_state(observer, &target.name) != Some(NodeState::Alive))
    })
    .await;
    assert!(
        failed,
        "{} still sees {} Alive after node kill",
        source.name, target.name
    );

    let other = nodes
        .iter()
        .find(|node| node.name != source.name && node.name != target.name)
        .copied()
        .expect("second follower exists");
    assert!(
        wait_until_api_view_drops(other, &target.name, Duration::from_secs(30)).await,
        "{} never dropped the killed {} from its API membership",
        other.name,
        target.name
    );
    let mut unsafe_second_kill = request.clone();
    unsafe_second_kill.target_node = Some(other.name.clone());
    let refused = other.client.inject_fault(&unsafe_second_kill).await;
    assert_quorum_refusal(&refused);
    assert!(
        other.client.list_faults().await.unwrap().is_empty(),
        "a refused second voter fault must leave no active effect"
    );
    assert!(
        source.client.list_faults().await.unwrap().is_empty(),
        "a refused second voter fault must not mutate the routing node"
    );

    target
        .client
        .clear_fault(summary.id, Some(&target.name), true)
        .await
        .expect("authorised manual reversal should reopen target transports");
    let recovered = wait_until(Duration::from_secs(30), || {
        peer_state(source, &target.name) == Some(NodeState::Alive)
    })
    .await;
    assert!(
        recovered,
        "{} did not rejoin after node fault reversal",
        target.name
    );

    shutdown.cancel();
    for node in nodes {
        if let Some(council) = &node.handle.council {
            council.shutdown().await.ok();
        }
    }
}

/// Start three authenticated nodes and wait until a 3-replica `web` runs one
/// replica on each. Used by the tests that act on a replica somewhere else.
/// Each node gets a child of `shutdown`, so a test can stop one node alone.
async fn start_spread_web_cluster(
    prefix: &str,
    first_port: u16,
    shutdown: &CancellationToken,
) -> [Node; 3] {
    let auth = NodeFaultAuth::admin(&format!("{prefix}-admin"));
    let n1 = start_node_with_auth(
        &format!("{prefix}1"),
        first_port,
        vec![],
        &shutdown.child_token(),
        Some(auth.clone()),
    )
    .await;
    let n2 = start_node_with_auth(
        &format!("{prefix}2"),
        first_port + 4,
        vec![local(first_port)],
        &shutdown.child_token(),
        Some(auth.clone()),
    )
    .await;
    let n3 = start_node_with_auth(
        &format!("{prefix}3"),
        first_port + 8,
        vec![local(first_port)],
        &shutdown.child_token(),
        Some(auth),
    )
    .await;
    {
        let nodes = [&n1, &n2, &n3];
        let ready = wait_until(Duration::from_secs(30), || {
            nodes.iter().any(|n| *n.thinks_leader.borrow())
        })
        .await;
        assert!(ready, "no leader elected");
        tokio::time::sleep(Duration::from_secs(3)).await;
        let config = reliaburger::config::Config::parse(
            r#"
            [app.web]
            image = "proc-grill:image-ignored"
            command = ["sh", "-c", "while true; do echo tick from $$; sleep 0.3; done"]
            port = 8080
            replicas = 3
        "#,
        )
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut last_apply: Option<tokio::time::Instant> = None;
        loop {
            if live_web_instances(&nodes).await == 3 && nodes_running_web(&nodes).await == 3 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "web never spread one replica per node"
            );
            if last_apply.is_none_or(|t| t.elapsed() >= Duration::from_secs(8)) {
                let _ =
                    tokio::time::timeout(Duration::from_secs(15), n1.client.apply(&config)).await;
                last_apply = Some(tokio::time::Instant::now());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    [n1, n2, n3]
}

/// Z6.7: losing the leader node of three must bring the app back to three
/// replicas on the survivors, without moving the replica on the survivor that
/// doesn't need a second one and without churning generations of
/// replacements. `relish wtf` flags the gap while it lasts.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn losing_the_leader_node_places_only_its_replica_on_the_survivors() {
    let shutdown = CancellationToken::new();
    let nodes = start_spread_web_cluster("nl", 19941, &shutdown).await;
    let entry = &nodes[0];
    // Lose the leader when it isn't the entry node, as the tour's node-3 was;
    // otherwise any other node, so the test never loses its own client.
    let doomed = nodes[1..]
        .iter()
        .find(|node| *node.thinks_leader.borrow())
        .unwrap_or(&nodes[2]);
    let survivors: Vec<&Node> = nodes
        .iter()
        .filter(|node| node.name != doomed.name)
        .collect();
    let mut before = std::collections::HashMap::new();
    for node in &survivors {
        before.insert(node.name.clone(), web_process(node).await.unwrap().0);
    }

    doomed._wired.shutdown.cancel();

    // wtf sees the gap before the scheduler has closed it.
    let flagged = async {
        loop {
            if let Ok(inputs) = reliaburger::relish::wtf::collect(&entry.client, None).await {
                let report = reliaburger::relish::wtf::diagnose(&inputs);
                if report
                    .warnings
                    .iter()
                    .any(|finding| finding.id == "under-replicated")
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    assert!(
        tokio::time::timeout(Duration::from_secs(30), flagged)
            .await
            .is_ok(),
        "wtf never noticed that web ran fewer replicas than it wanted"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    while live_web_instances(&survivors).await < 3 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "web never got back to three replicas on the survivors"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Hold still: no more replacements start once three run.
    let settled = std::collections::BTreeSet::from_iter(web_instance_ids(&survivors).await);
    tokio::time::sleep(Duration::from_secs(15)).await;
    let later = std::collections::BTreeSet::from_iter(web_instance_ids(&survivors).await);
    assert!(
        later.is_subset(&settled),
        "new replicas kept starting after three ran: {settled:?} then {later:?}"
    );
    assert_eq!(live_web_instances(&survivors).await, 3);
    // Every survivor still runs web, and one of them didn't restart at all:
    // only the missing replica was placed.
    let mut untouched = 0;
    for node in &survivors {
        let (pid, _) = web_process(node)
            .await
            .unwrap_or_else(|| panic!("{} lost its replica", node.name));
        if before[&node.name] == pid {
            untouched += 1;
        }
    }
    assert!(untouched >= 1, "every survivor's replica was replaced");
    let report = reliaburger::relish::wtf::diagnose(
        &reliaburger::relish::wtf::collect(&entry.client, None)
            .await
            .unwrap(),
    );
    assert!(
        !report
            .warnings
            .iter()
            .any(|finding| finding.id == "under-replicated"),
        "{:?}",
        report.warnings
    );
    shutdown.cancel();
}

/// V02 chaos C2: a node-kill fault on a worker (not the leader) that also
/// kills its containers must bring the app back to three running replicas on
/// the two survivors. The killed node's API stays open, so whatever it
/// reports is observed but not counted: only the survivors carry the load.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn a_killed_worker_has_its_replica_rescheduled_on_the_survivors() {
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let shutdown = CancellationToken::new();
    let nodes = start_spread_web_cluster("kw", 20441, &shutdown).await;
    // The quorum rail admits one affected voter only once three vote.
    let voters_ready = wait_until(Duration::from_secs(60), || {
        nodes.iter().all(|node| {
            node.handle.council.as_ref().is_some_and(|council| {
                council
                    .metrics()
                    .borrow()
                    .membership_config
                    .membership()
                    .voter_ids()
                    .count()
                    == 3
            })
        })
    })
    .await;
    assert!(voters_ready, "council never grew to three voters");
    let leader = nodes
        .iter()
        .find(|node| *node.thinks_leader.borrow())
        .expect("a leader exists");
    let entry = &nodes[0];
    let target = nodes
        .iter()
        .find(|node| node.name != leader.name && node.name != entry.name)
        .or_else(|| nodes.iter().find(|node| node.name != leader.name))
        .expect("a worker exists");
    let survivors: Vec<&Node> = nodes
        .iter()
        .filter(|node| node.name != target.name)
        .collect();

    entry
        .client
        .inject_fault(&FaultRequest {
            fault_type: FaultType::NodeKill {
                kill_containers: true,
            },
            target_service: String::new(),
            namespace: None,
            target_instance: None,
            target_node: Some(target.name.clone()),
            duration: Duration::from_secs(300),
            injected_by: String::new(),
            reason: Some("chaos C2 worker failure".to_string()),
            include_leader: false,
            override_safety: false,
            acknowledged: true,
        })
        .await
        .expect("a worker node kill is admitted");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let running = running_web_instances(&survivors).await;
        if running >= 3 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "web never got back to three running replicas on the survivors \
             after {} was killed; survivors run {running}, the killed node reports {:?}",
            target.name,
            target.client.status().await.map(|statuses| statuses
                .into_iter()
                .filter(|s| s.app_name == "web")
                .map(|s| s.state)
                .collect::<Vec<_>>())
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    shutdown.cancel();
}

/// The number of `web` instances in the `running` state on `nodes`.
async fn running_web_instances(nodes: &[&Node]) -> usize {
    let mut count = 0;
    for node in nodes {
        if let Ok(statuses) = node.client.status().await {
            count += statuses
                .iter()
                .filter(|s| s.app_name == "web" && s.state == "running")
                .count();
        }
    }
    count
}

/// The ids of every live `web` instance on `nodes`.
async fn web_instance_ids(nodes: &[&Node]) -> Vec<String> {
    let mut ids = Vec::new();
    for node in nodes {
        if let Ok(statuses) = node.client.status().await {
            ids.extend(
                statuses
                    .into_iter()
                    .filter(|s| {
                        s.app_name == "web"
                            && !matches!(s.state.as_str(), "stopped" | "stopping" | "failed")
                    })
                    .map(|s| format!("{}/{}", node.name, s.id)),
            );
        }
    }
    ids
}

/// The pid and restart count of `node`'s `web` replica.
async fn web_process(node: &Node) -> Option<(u32, u32)> {
    node.client
        .status()
        .await
        .ok()?
        .into_iter()
        .find(|status| status.app_name == "web" && status.state == "running")
        .and_then(|status| Some((status.pid?, status.restart_count)))
}

/// Z2.1: a workload fault sent to one node kills the replica that runs on
/// another. The laptop only talks to node 1, so `relish fault kill` has to
/// reach wherever the replica lives, and the replica rail has to count the
/// replicas on every node rather than node 1's share.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn a_kill_sent_to_one_node_kills_a_replica_on_another() {
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let shutdown = CancellationToken::new();
    let nodes = start_spread_web_cluster("wk", 19541, &shutdown).await;
    let [entry, target, bystander] = &nodes;

    let (target_pid, _) = web_process(target).await.expect("target runs web");
    let (bystander_pid, _) = web_process(bystander).await.expect("bystander runs web");

    let kill = |count, node: Option<&str>| FaultRequest {
        fault_type: FaultType::Kill { count },
        target_service: "web".to_string(),
        namespace: None,
        target_instance: None,
        target_node: node.map(str::to_string),
        duration: Duration::from_secs(0),
        injected_by: String::new(),
        reason: Some("cross-node workload fault".to_string()),
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    };

    // Killing every replica is refused, even though the entry node holds
    // only one of them.
    let refused = entry.client.inject_fault(&kill(3, None)).await;
    assert!(
        matches!(&refused, Err(reliaburger::relish::RelishError::ApiError { status: 400, body })
            if body.contains("replica")),
        "expected the replica rail to refuse, got {refused:?}"
    );

    let summary = entry
        .client
        .inject_fault(&kill(1, Some(&target.name)))
        .await
        .expect("the entry node should route the kill to the owner");
    assert_eq!(summary.node.as_deref(), Some(target.name.as_str()));

    let restarted = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some((pid, restarts)) = web_process(target).await
                && pid != target_pid
                && restarts > 0
            {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    assert!(
        restarted,
        "{}'s web replica was not killed and restarted",
        target.name
    );
    assert_eq!(
        web_process(bystander).await.map(|(pid, _)| pid),
        Some(bystander_pid),
        "a replica on an untargeted node must not be touched"
    );

    // The routed fault is listed with its owner, cluster-wide.
    let listing = entry.client.list_cluster_faults().await.unwrap();
    assert!(
        listing
            .faults
            .iter()
            .all(|fault| fault.node.as_deref() == Some(target.name.as_str())),
        "{:?}",
        listing.faults
    );

    shutdown.cancel();
    for node in &nodes {
        if let Some(council) = &node.handle.council {
            council.shutdown().await.ok();
        }
    }
}

/// Z6.1: a network fault acts where a connection starts, so a DNS fault on
/// `web` sent to one node is installed on every node (any of them may run a
/// caller), listed with each holder, and cleared everywhere by service name.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn a_network_fault_sent_to_one_node_is_installed_where_every_caller_runs() {
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let shutdown = CancellationToken::new();
    let nodes = start_spread_web_cluster("nf", 19841, &shutdown).await;
    let entry = &nodes[0];

    let summary = entry
        .client
        .inject_fault(&FaultRequest {
            fault_type: FaultType::DnsNxdomain,
            target_service: "web".to_string(),
            namespace: None,
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(120),
            injected_by: String::new(),
            reason: Some("callers everywhere".to_string()),
            include_leader: false,
            override_safety: false,
            acknowledged: true,
        })
        .await
        .expect("a destination-wide network fault should be routed to every node");
    let mut holders: Vec<String> = std::iter::once(&summary)
        .chain(&summary.routed)
        .filter_map(|fault| fault.node.clone())
        .collect();
    holders.sort();
    let mut names: Vec<String> = nodes.iter().map(|node| node.name.clone()).collect();
    names.sort();
    assert_eq!(holders, names);

    let listing = entry.client.list_cluster_faults().await.unwrap();
    assert_eq!(listing.faults.len(), 3, "{:?}", listing.faults);

    entry
        .client
        .clear_faults_by_service("web", Some("default"))
        .await
        .expect("clear by service reaches every node");
    let listing = entry.client.list_cluster_faults().await.unwrap();
    assert!(listing.faults.is_empty(), "{:?}", listing.faults);

    shutdown.cancel();
    for node in &nodes {
        if let Some(council) = &node.handle.council {
            council.shutdown().await.ok();
        }
    }
}

/// Read a followed log stream until `done` says so or `timeout` passes,
/// returning every event seen so far.
async fn read_follow_events<B: AsRef<[u8]>>(
    body: &mut (impl futures_util::Stream<Item = reqwest::Result<B>> + Unpin),
    decoder: &mut reliaburger::ketchup::sse::SseDecoder,
    events: &mut Vec<reliaburger::ketchup::sse::SseEvent>,
    timeout: Duration,
    mut done: impl FnMut(&[reliaburger::ketchup::sse::SseEvent]) -> bool,
) {
    use futures_util::StreamExt;

    let deadline = tokio::time::Instant::now() + timeout;
    while !done(events) {
        match tokio::time::timeout_at(deadline, body.next()).await {
            Ok(Some(Ok(chunk))) => events.extend(decoder.push(chunk.as_ref())),
            Ok(Some(Err(_)) | None) | Err(_) => return,
        }
    }
}

/// The nodes that produced followed lines, from their `[node instance]` prefix.
fn followed_nodes(events: &[reliaburger::ketchup::sse::SseEvent]) -> Vec<String> {
    let mut nodes: Vec<String> = events
        .iter()
        .filter(|event| event.event.is_none())
        .filter_map(|event| {
            let rest = event.data.strip_prefix('[')?;
            Some(rest.split_once(' ')?.0.to_string())
        })
        .collect();
    nodes.sort();
    nodes.dedup();
    nodes
}

/// Z2.2: `relish logs -f` and `relish top` from one node see every node.
/// The follow merges each node's lines under a `[node instance]` prefix, and
/// when a node dies mid-stream it warns and keeps streaming the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn follow_and_top_cover_every_node_and_survive_one_leaving() {
    let shutdown = CancellationToken::new();
    let nodes = start_spread_web_cluster("lf", 19641, &shutdown).await;
    let entry = &nodes[0];
    // Lose a follower rather than the leader, so the test watches the
    // follow's reaction rather than an election.
    let doomed = nodes[1..]
        .iter()
        .find(|node| !*node.thinks_leader.borrow())
        .expect("a follower other than the entry node");
    let names: Vec<String> = nodes.iter().map(|node| node.name.clone()).collect();

    let top = entry.client.cluster_top().await.unwrap();
    assert!(top.warnings.is_empty(), "{:?}", top.warnings);
    let mut top_nodes: Vec<_> = top
        .rows
        .iter()
        .filter(|row| row.instance.app_name == "web")
        .map(|row| row.node.clone())
        .collect();
    top_nodes.sort();
    assert_eq!(top_nodes, names);

    let response = entry
        .client
        .http()
        .unwrap()
        .get(format!("{}/v1/logs/web/default", entry.client.base_url()))
        .query(&[("follow", "true")])
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let mut body = response.bytes_stream();
    let mut decoder = reliaburger::ketchup::sse::SseDecoder::default();
    let mut events = Vec::new();

    read_follow_events(
        &mut body,
        &mut decoder,
        &mut events,
        Duration::from_secs(20),
        |events| followed_nodes(events).len() == 3,
    )
    .await;
    assert_eq!(followed_nodes(&events), names, "lines from every node");
    let line = events
        .iter()
        .find(|event| event.data.starts_with(&format!("[{} ", doomed.name)))
        .unwrap();
    assert!(line.data.contains("tick from"), "{}", line.data);

    // Take a node away mid-stream: the follow warns about it and carries on.
    doomed._wired.shutdown.cancel();
    let before = events.len();
    read_follow_events(
        &mut body,
        &mut decoder,
        &mut events,
        Duration::from_secs(45),
        |events| {
            events[before..].iter().any(|event| {
                event.event.as_deref() == Some(reliaburger::ketchup::sse::WARNING_EVENT)
                    && event.data.contains(&doomed.name)
            })
        },
    )
    .await;
    let warned = events[before..].iter().any(|event| {
        event.event.as_deref() == Some(reliaburger::ketchup::sse::WARNING_EVENT)
            && event.data.contains(&doomed.name)
    });
    assert!(
        warned,
        "no warning about {}: {:?}",
        doomed.name,
        &events[before..]
    );
    let after_warning = events.len();
    read_follow_events(
        &mut body,
        &mut decoder,
        &mut events,
        Duration::from_secs(10),
        |events| events.len() >= after_warning + 5,
    )
    .await;
    // The survivors keep streaming. Which of them runs the rescheduled
    // replicas is the scheduler's business, not this test's.
    let survivors = followed_nodes(&events[after_warning..]);
    assert!(
        !survivors.is_empty() && !survivors.contains(&doomed.name),
        "the follow should keep streaming the survivors, got {survivors:?}"
    );

    shutdown.cancel();
    for node in &nodes {
        if let Some(council) = &node.handle.council {
            council.shutdown().await.ok();
        }
    }
}

/// Z2.3: `relish wtf` and `relish path` reach every node through the node
/// the CLI talks to. A laptop host can only reach node 1's forwarded port, so
/// neither may dial a node's own advertised address.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn wtf_and_path_reach_every_node_through_the_entry_node() {
    let shutdown = CancellationToken::new();
    let nodes = start_spread_web_cluster("wt", 19741, &shutdown).await;
    // Enter through the middle node, so the path's source (the lowest-named
    // node running `web`) is somewhere else.
    let entry = &nodes[1];
    // wtf reads the council from the leader, which it finds through the
    // entry node's view; wait until every node knows the full council.
    let voters_ready = wait_until(Duration::from_secs(60), || {
        nodes.iter().all(|node| {
            node.handle.council.as_ref().is_some_and(|council| {
                let metrics = council.metrics().borrow().clone();
                metrics.membership_config.membership().voter_ids().count() == 3
                    && metrics.current_leader.is_some()
            })
        })
    })
    .await;
    assert!(voters_ready, "council never grew to three voters");

    let inputs = reliaburger::relish::wtf::collect(&entry.client, Some("web"))
        .await
        .expect("wtf collects through the entry node");
    let observed = inputs
        .cluster
        .nodes
        .value()
        .expect("node evidence is available")
        .clone();
    let mut reachable: Vec<_> = observed
        .iter()
        .filter(|node| node.agent_reachable)
        .map(|node| node.node_id.clone())
        .collect();
    reachable.sort();
    let names: Vec<_> = nodes.iter().map(|node| node.name.clone()).collect();
    assert_eq!(reachable, names, "every node answered through the relay");
    assert!(
        inputs.cluster.council.value().is_some(),
        "the leader's council view came through the relay: {:?}",
        inputs.cluster.council
    );

    let result = reliaburger::relish::path_cmd::probe_path(
        &reliaburger::onion::trace::TraceRequest {
            source: "web".to_string(),
            source_namespace: "default".to_string(),
            destination: "web".to_string(),
            destination_namespace: "default".to_string(),
            port: None,
            count: None,
        },
        &entry.client,
    )
    .await
    .expect("path probe runs on the source node through the relay");
    assert_eq!(result.source_node, nodes[0].name);

    shutdown.cancel();
    for node in &nodes {
        if let Some(council) = &node.handle.council {
            council.shutdown().await.ok();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn ingress_reaches_nodes_without_local_replicas() {
    let shutdown = CancellationToken::new();
    let n1 = start_node("ingress1", 19441, vec![], &shutdown).await;
    let n2 = start_node("ingress2", 19445, vec![local(19441)], &shutdown).await;
    let n3 = start_node("ingress3", 19449, vec![local(19441)], &shutdown).await;
    let nodes = [&n1, &n2, &n3];
    let config = reliaburger::config::Config::parse(
        r#"
        [app.web]
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
        replicas = 1
        port = 8080
        [app.web.ingress]
        host = "remote.local"
    "#,
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut converged = false;
    let mut last_apply = tokio::time::Instant::now() - Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        if last_apply.elapsed() >= Duration::from_secs(8) {
            let _ = tokio::time::timeout(Duration::from_secs(5), n1.client.apply(&config)).await;
            last_apply = tokio::time::Instant::now();
        }
        let mut routed_nodes = 0;
        let mut instances = 0;
        for node in nodes {
            let routes = node.client.routes().await.expect("node must serve routes");
            if routes
                .iter()
                .any(|r| r.host == "remote.local" && r.healthy_backends == 1)
            {
                routed_nodes += 1;
            }
            instances += node
                .client
                .status()
                .await
                .expect("node must serve status")
                .iter()
                .filter(|s| s.app_name == "web")
                .count();
        }
        if instances == 1 && routed_nodes == 3 {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if converged {
        for node in nodes {
            let instances = node.client.cluster_status().await.unwrap();
            assert_eq!(instances.len(), 1);
            assert_eq!(instances[0].instance.app_name, "web");
            assert!(!instances[0].node.is_empty());
            for path in [
                "/ui/app/web/default",
                "/ui/fragment/app/web/default/instances",
            ] {
                let response = node
                    .client
                    .http()
                    .unwrap()
                    .get(format!("{}{path}", node.client.base_url()))
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success());
                let html = response.text().await.unwrap();
                assert!(
                    html.contains(&instances[0].instance.id),
                    "{path} must show the remote instance on every node: {html}"
                );
            }
        }
    }
    shutdown.cancel();
    for node in nodes {
        if let Some(council) = &node.handle.council {
            council.shutdown().await.unwrap();
        }
    }
    assert!(
        converged,
        "all three ingress nodes must see the single healthy replica"
    );
}

/// Wait for the same live evidence that the fault API actually reads. The
/// membership-table task can lag the underlying gossip watch after reversal.
async fn wait_for_fault_admission_views(nodes: &[&Node]) -> usize {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let leader_id = nodes[0]
                .handle
                .council
                .as_ref()
                .unwrap()
                .metrics()
                .borrow()
                .current_leader;
            let mut ready = leader_id.is_some();
            for node in nodes {
                let council = node.handle.council.as_ref().unwrap();
                let metrics = council.metrics().borrow().clone();
                let membership = metrics.membership_config.membership();
                let table = node.membership_table.read().await;
                ready &= metrics.current_leader == leader_id
                    && membership.voter_ids().count() == nodes.len()
                    && membership.get_joint_config().len() == 1
                    && nodes.iter().all(|peer| {
                        peer_state(node, &peer.name)
                            == Some(reliaburger::mustard::state::NodeState::Alive)
                            && table.iter().any(|member| member.node_id.0 == peer.name)
                    });
                drop(table);
                ready &= council
                    .desired_state()
                    .await
                    .node_fault_reservations
                    .active
                    .is_none();
            }
            if ready {
                let index = nodes
                    .iter()
                    .position(|node| {
                        Some(reliaburger::cluster::identity::raft_id_from_name(
                            &node.name,
                        )) == leader_id
                    })
                    .unwrap();
                if nodes[index]
                    .handle
                    .council
                    .as_ref()
                    .unwrap()
                    .is_leader()
                    .await
                {
                    return index;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("all fault-admission views must converge with no active reservation")
}

/// C06: requests sent through different APIs share one committed reservation.
/// A leader failure retains that ownership until target-side reversal is proven.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node reservation acceptance; run with make test-cluster"]
async fn concurrent_node_kills_and_leader_change_preserve_reserved_capacity() {
    use reliaburger::mustard::state::NodeState;
    use reliaburger::smoker::types::{FaultRequest, FaultType};
    let auth = NodeFaultAuth::admin("reservation-admin");
    let shutdown = CancellationToken::new();
    let n1 =
        start_node_with_auth("reservation1", 20341, vec![], &shutdown, Some(auth.clone())).await;
    let n2 = start_node_with_auth(
        "reservation2",
        20345,
        vec![local(20341)],
        &shutdown,
        Some(auth.clone()),
    )
    .await;
    let n3 = start_node_with_auth(
        "reservation3",
        20349,
        vec![local(20341)],
        &shutdown,
        Some(auth),
    )
    .await;
    let nodes = [&n1, &n2, &n3];
    let leader = nodes[wait_for_fault_admission_views(&nodes).await];
    let followers: Vec<_> = nodes
        .iter()
        .filter(|node| node.name != leader.name)
        .copied()
        .collect();
    let request = |target: &Node, duration| FaultRequest {
        fault_type: FaultType::NodeKill {
            kill_containers: false,
        },
        target_service: String::new(),
        namespace: None,
        target_instance: None,
        target_node: Some(target.name.clone()),
        duration,
        injected_by: "untrusted-body".into(),
        reason: Some("concurrent reservation acceptance".into()),
        include_leader: true,
        override_safety: true,
        acknowledged: true,
    };
    let first = request(followers[0], Duration::from_secs(30));
    let second = request(followers[1], Duration::from_secs(30));
    let (left, right) = tokio::join!(
        followers[0].client.inject_fault(&first),
        followers[1].client.inject_fault(&second),
    );
    assert_eq!(
        usize::from(left.is_ok()) + usize::from(right.is_ok()),
        1,
        "exactly one competing kill may be admitted: {left:?}, {right:?}"
    );
    assert_eq!(
        nodes
            .iter()
            .filter(|node| node.handle.partition_blocklists.node_gate.is_quiesced())
            .count(),
        1,
        "exactly one transport gate may close, regardless of HTTP outcomes"
    );
    let (target, summary) = match (left, right) {
        (Ok(summary), Err(_)) => (followers[0], summary),
        (Err(_), Ok(summary)) => (followers[1], summary),
        _ => unreachable!("checked one admission"),
    };
    let council = leader.handle.council.as_ref().unwrap();
    let reservation = council
        .desired_state()
        .await
        .node_fault_reservations
        .active
        .unwrap();
    assert_eq!(
        reservation.request.target_node.as_deref(),
        Some(target.name.as_str())
    );
    target
        .client
        .clear_fault(summary.id, Some(&target.name), true)
        .await
        .unwrap();
    let (old_leader, sender) = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let leader = nodes[wait_for_fault_admission_views(&nodes).await];
            let sender = nodes
                .iter()
                .find(|node| node.name != leader.name)
                .copied()
                .unwrap();
            match sender
                .client
                .inject_fault(&request(leader, Duration::from_secs(12)))
                .await
            {
                Ok(_) => break (leader, sender),
                // This exact refusal precedes reservation/mutation. Membership
                // can change between our probe and the API's own safety check.
                Err(reliaburger::relish::RelishError::ApiError { status: 503, body })
                    if body
                        == "node fault safety cannot map the council leader to live membership" => {
                }
                Err(error) => panic!("leader fault admission failed: {error}"),
            }
        }
    })
    .await
    .expect("leader fault admission did not converge");
    let mut inherited = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline {
        for node in nodes.iter().filter(|node| node.name != old_leader.name) {
            if *node.thinks_leader.borrow() {
                inherited = node
                    .handle
                    .council
                    .as_ref()
                    .unwrap()
                    .desired_state()
                    .await
                    .node_fault_reservations
                    .active;
                if inherited.is_some() {
                    break;
                }
            }
        }
        if inherited.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let inherited = inherited.expect("new leader must inherit the outstanding reservation");
    assert!(inherited.sequence > reservation.sequence);
    assert_eq!(
        inherited.request.target_node.as_deref(),
        Some(old_leader.name.as_str())
    );
    let refused = sender
        .client
        .inject_fault(&request(sender, Duration::from_secs(5)))
        .await;
    assert!(
        refused.is_err(),
        "leader change must not free fault capacity"
    );
    assert!(sender.client.list_faults().await.unwrap().is_empty());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        let current = nodes
            .iter()
            .find(|node| *node.thinks_leader.borrow())
            .copied();
        if let Some(current) = current {
            let state = current
                .handle
                .council
                .as_ref()
                .unwrap()
                .desired_state()
                .await;
            if state.node_fault_reservations.active.is_none()
                && peer_state(current, &old_leader.name) == Some(NodeState::Alive)
            {
                break;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let faults = old_leader.client.list_faults().await;
            let views: Vec<_> = nodes
                .iter()
                .map(|node| {
                    (
                        node.name.clone(),
                        *node.thinks_leader.borrow(),
                        peer_state(node, &old_leader.name),
                        node.handle.partition_blocklists.node_gate.is_quiesced(),
                    )
                })
                .collect();
            panic!(
                "expiry and confirmed reversal must recover capacity after election; faults={faults:?}, views={views:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    shutdown.cancel();
    for node in nodes {
        node.handle.council.as_ref().unwrap().shutdown().await.ok();
    }
}

/// Administrative apply must retain the user's authority at the leader.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "multi-node apply forwarding acceptance; run with make test-cluster"]
async fn follower_apply_preserves_user_authority_for_administrative_manifests() {
    use reliaburger::sesame::types::{ApiRole, TokenScope};
    let auth = NodeFaultAuth::admin("apply-admin");
    let shutdown = CancellationToken::new();
    let n1 = start_node_with_auth("apply1", 20401, vec![], &shutdown, Some(auth.clone())).await;
    let n2 = start_node_with_auth(
        "apply2",
        20405,
        vec![local(20401)],
        &shutdown,
        Some(auth.clone()),
    )
    .await;
    let n3 = start_node_with_auth(
        "apply3",
        20409,
        vec![local(20401)],
        &shutdown,
        Some(auth.clone()),
    )
    .await;
    let nodes = [&n1, &n2, &n3];
    assert!(
        wait_until(Duration::from_secs(60), || nodes.iter().all(|node| {
            node.handle.council.as_ref().is_some_and(|council| {
                let metrics = council.metrics().borrow().clone();
                metrics.current_leader.is_some()
                    && metrics.membership_config.membership().voter_ids().count() == 3
            })
        }))
        .await
    );
    let follower = nodes
        .iter()
        .find(|node| !*node.thinks_leader.borrow())
        .unwrap();
    let manifest = "[namespace.release]\nmax_apps = 2\n[permission.ci]\nactions = [\"deploy\"]\napps = [\"web\"]\nnamespaces = [\"release\"]\n";
    let response = reqwest::Client::new()
        .post(format!("{}/v1/apply", follower.client.base_url()))
        .bearer_auth(&auth.plaintext)
        .body(manifest)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    assert!(content_type.starts_with("text/event-stream"));
    assert!(body.contains("committed to the cluster"), "{body}");
    assert!(!body.contains("error"), "{body}");
    let leader = nodes
        .iter()
        .find(|node| *node.thinks_leader.borrow())
        .unwrap();
    let state = leader
        .handle
        .council
        .as_ref()
        .unwrap()
        .desired_state()
        .await;
    assert_eq!(state.namespaces["release"].max_apps, Some(2));
    assert!(state.permissions.contains_key("ci"));

    // A follower can temporarily lag credential revocation. The leader's
    // refusal must retain its HTTP status and plain-text body at the client.
    let replacement = reliaburger::sesame::token::create_token(
        "replacement-admin",
        ApiRole::Admin,
        TokenScope::default(),
        None,
    )
    .unwrap();
    *leader.token_store.as_ref().unwrap().write().await = vec![replacement.token];
    let response = reqwest::Client::new()
        .post(format!("{}/v1/apply", follower.client.base_url()))
        .bearer_auth(&auth.plaintext)
        .body(manifest)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn capacity_refusal_from_the_live_scheduler_forwards_without_committing_an_app() {
    use reliaburger::meat::scheduler::ScheduleError;
    use reliaburger::relish::RelishError;
    let auth = NodeFaultAuth::admin("capacity-admin");
    let shutdown = CancellationToken::new();
    let n1 = start_node_with_auth("cap1", 26341, vec![], &shutdown, Some(auth.clone())).await;
    let n2 = start_node_with_auth(
        "cap2",
        26345,
        vec![local(26341)],
        &shutdown,
        Some(auth.clone()),
    )
    .await;
    let n3 = start_node_with_auth("cap3", 26349, vec![local(26341)], &shutdown, Some(auth)).await;
    let nodes = [&n1, &n2, &n3];
    assert!(
        wait_until(Duration::from_secs(40), || nodes.iter().any(|node| {
            *node.thinks_leader.borrow()
                && node.handle.council.as_ref().is_some_and(|council| {
                    council
                        .metrics()
                        .borrow()
                        .membership_config
                        .membership()
                        .voter_ids()
                        .count()
                        == 3
                })
        }))
        .await
    );
    let follower = nodes
        .iter()
        .find(|node| !*node.thinks_leader.borrow())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let peers = follower.client.nodes().await.unwrap();
            if peers.len() == 3 && peers.iter().all(|peer| peer.api_address.is_some()) {
                let expected = [("cap1", 26344), ("cap2", 26348), ("cap3", 26352)];
                for (id, port) in expected {
                    let peer = peers.iter().find(|peer| peer.node_id == id).unwrap();
                    assert_eq!(peer.api_address, Some(local(port)));
                    // Every node requires the original bearer identity. A successful
                    // read also proves that each advertised endpoint is serving.
                    follower
                        .client
                        .for_node(peer)
                        .unwrap()
                        .status()
                        .await
                        .unwrap();
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let lease = follower
        .client
        .create_test_lease(120, Some("rbtest-capacity-contract"))
        .await
        .unwrap();
    let mut config = reliaburger::config::Config::parse(&format!(
        "[app.capacity]\nimage = \"proc-grill:image-ignored\"\ncommand = [\"sleep\", \"300\"]\ncpu = \"100000m\"\nmemory = \"1Mi\"\nnamespace = \"{}\"\n", lease.namespace
    )).unwrap();
    let app_id = reliaburger::meat::AppId::new("capacity", &lease.namespace);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match follower
                .client
                .apply_capacity_with_lease(&config, &lease.lease_id)
                .await
            {
                Err(RelishError::SchedulingRejected(ScheduleError::NoEligibleNodes {
                    app_id: rejected,
                })) => {
                    assert_eq!(rejected, app_id);
                    break;
                }
                Err(RelishError::ApiError { status: 503, .. }) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                other => panic!("expected typed scheduling refusal, got {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    let leader = nodes
        .iter()
        .find(|node| *node.thinks_leader.borrow())
        .unwrap();
    assert!(
        !leader
            .handle
            .council
            .as_ref()
            .unwrap()
            .desired_state()
            .await
            .apps
            .contains_key(&app_id)
    );
    assert!(follower.client.cluster_status().await.unwrap().is_empty());

    // ProcessGrill deliberately refuses resource limits. The oversized spec
    // exercises scheduler refusal before deployment; the runnable fixture uses
    // this runtime's supported contract. Real capacity benchmarks require OCI.
    let spec = config.app.get_mut("capacity").unwrap();
    spec.cpu = None;
    spec.memory = None;
    follower
        .client
        .apply_capacity_with_lease(&config, &lease.lease_id)
        .await
        .unwrap();
    let running = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let rows = follower.client.cluster_status().await.unwrap();
            if rows
                .iter()
                .filter(|row| {
                    row.instance.app_name == "capacity"
                        && row.instance.namespace == lease.namespace
                        && row.instance.state == "running"
                })
                .count()
                == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if running.is_err() {
        for node in nodes {
            eprintln!(
                "{}: status {:?}; deploys {:?}",
                node.name,
                node.client.status().await,
                node.client.deploy_operations().await
            );
        }
    }
    running.expect("accepted workload must actually run");
    assert!(matches!(
        follower
            .client
            .apply_capacity_with_lease(&config, &lease.lease_id)
            .await,
        Err(RelishError::SchedulingRejected(
            ScheduleError::InvalidSpec { .. }
        ))
    ));
    follower
        .client
        .release_test_lease(&lease.lease_id)
        .await
        .unwrap();
    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node leased storage acceptance; run with make test-cluster"]
async fn leased_storage_cleanup_waits_for_former_placements_and_failed_deletion() {
    let auth = NodeFaultAuth::admin("storage-admin");
    let shutdown = CancellationToken::new();
    let n1 = start_node_with_auth("storage1", 26841, vec![], &shutdown, Some(auth.clone())).await;
    let n2 = start_node_with_auth(
        "storage2",
        26845,
        vec![local(26841)],
        &shutdown,
        Some(auth.clone()),
    )
    .await;
    let n3 =
        start_node_with_auth("storage3", 26849, vec![local(26841)], &shutdown, Some(auth)).await;
    let nodes = [&n1, &n2, &n3];
    assert!(
        wait_until(Duration::from_secs(40), || nodes.iter().any(|node| *node
            .thinks_leader
            .borrow()
            && node
                .handle
                .council
                .as_ref()
                .unwrap()
                .metrics()
                .borrow()
                .membership_config
                .membership()
                .voter_ids()
                .count()
                == 3))
        .await
    );
    let leader = nodes
        .iter()
        .find(|node| *node.thinks_leader.borrow())
        .unwrap();
    let lease = leader
        .client
        .create_test_lease(120, Some("rbtest-storage-contract"))
        .await
        .unwrap();
    let mut config = reliaburger::config::Config::parse(&format!("[app.web]\nimage = 'proc-grill:image-ignored'\ncommand = ['sleep', '120']\nreplicas = 3\nnamespace = '{}'\n[[app.web.volumes]]\npath = '/data'\n", lease.namespace)).unwrap();
    leader
        .client
        .apply_with_lease(&config, &lease.lease_id)
        .await
        .unwrap();
    let roots: Vec<_> = [
        ("storage1", 26841),
        ("storage2", 26845),
        ("storage3", 26849),
    ]
    .into_iter()
    .map(|(name, port)| std::env::temp_dir().join(format!("rb-placement-{name}-{port}/volumes")))
    .collect();
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let rows = leader.client.cluster_status().await.unwrap();
            if rows
                .iter()
                .filter(|row| {
                    row.instance.namespace == lease.namespace && row.instance.state == "running"
                })
                .count()
                == 3
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("three real leased processes must run");
    let owned: Vec<_> = roots
        .iter()
        .filter(|root| root.join(&lease.namespace).join("web/data").exists())
        .collect();
    assert!(
        owned.len() >= 2,
        "the fixture must cover multiple placement owners"
    );
    for root in &owned {
        std::fs::write(
            root.join(&lease.namespace).join("web/data/marker"),
            "preserve until lease retirement",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("default/ordinary/data")).unwrap();
        std::fs::write(root.join("default/ordinary/data/keep"), "ordinary data").unwrap();
    }
    config.app.get_mut("web").unwrap().replicas = reliaburger::config::types::Replicas::Fixed(1);
    leader
        .client
        .apply_with_lease(&config, &lease.lease_id)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let rows = leader.client.cluster_status().await.unwrap();
            if rows
                .iter()
                .filter(|row| {
                    row.instance.namespace == lease.namespace && row.instance.state == "running"
                })
                .count()
                == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("scale-down must retire former runtimes");
    for root in &owned {
        assert!(
            root.join(&lease.namespace).join("web/data/marker").exists(),
            "rebalance deleted storage"
        );
    }
    let blocked_root = owned[0];
    let blocker = blocked_root
        .join(".snapshots")
        .join(&lease.namespace)
        .join("web");
    std::fs::create_dir_all(&blocker).unwrap();
    // No DELETE or client heartbeat follows this renewal: the server reaper
    // must own both expiry and eventual storage cleanup.
    leader
        .client
        .renew_test_lease(&lease.lease_id, 3)
        .await
        .unwrap();
    {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let state = leader
                    .handle
                    .council
                    .as_ref()
                    .unwrap()
                    .desired_state()
                    .await;
                if state.test_leases.get(&lease.lease_id).is_some_and(|lease| {
                    matches!(
                        lease.state,
                        reliaburger::testkit::lease::TestLeaseState::Cleaning { .. }
                    )
                }) && owned
                    .iter()
                    .skip(1)
                    .all(|root| !root.join(&lease.namespace).join("web").exists())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("unblocked owners must retire while one storage owner remains");
    }
    assert!(
        blocked_root
            .join(&lease.namespace)
            .join("web/data/marker")
            .exists()
    );
    std::fs::remove_dir(&blocker).unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if !leader
                .handle
                .council
                .as_ref()
                .unwrap()
                .desired_state()
                .await
                .test_leases
                .contains_key(&lease.lease_id)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("expiry must finish after the final storage owner is repaired");
    for root in owned {
        assert!(!root.join(&lease.namespace).join("web").exists());
        assert!(
            !root
                .join(".test-storage")
                .join(format!("{}__web.checkpoint", lease.namespace))
                .exists()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("default/ordinary/data/keep")).unwrap(),
            "ordinary data"
        );
    }
    shutdown.cancel();
}
