//! Binary-driven integration tests for cluster scheduling (Stage 4 W6,
//! L1). Three real nodes: gossip + Raft + reporting + a real BunAgent
//! (ProcessGrill) + the HTTP API + the leader scheduler, membership
//! refresher, and placement reconciler — the exact wiring `bun --cluster`
//! runs. Apply an app on one node; assert replicas spread across the
//! cluster.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::agent::{BunAgent, ClusterHandle};
use reliaburger::bun::api::{self, NodeMembershipInfo};
use reliaburger::cluster::orchestrate::{spawn_leader_scheduler, spawn_placement_reconciler};
use reliaburger::cluster::runtime::{self, ClusterParams};
use reliaburger::config::node::ReportingTreeSection;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::process::ProcessGrill;
use reliaburger::relish::client::BunClient;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

#[path = "support/task_harness.rs"]
mod task_harness;
use task_harness::TestTasks;

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A fully wired node: everything `bun --cluster` starts, on one host
/// with per-node port blocks (gossip, +1 raft, +2 reporting, +3 API).
struct Node {
    name: String,
    client: BunClient,
    handle: ClusterHandle,
    thinks_leader: watch::Receiver<bool>,
    rollup_store: Arc<RwLock<reliaburger::mayo::rollup_store::RollupStore>>,
    _runtime: runtime::ClusterRuntime,
    _tasks: TestTasks,
}

use tokio::sync::watch;

async fn start_node(
    name: &str,
    gossip_port: u16,
    seeds: Vec<SocketAddr>,
    shutdown: &CancellationToken,
) -> Node {
    let raft_port = gossip_port + 1;
    let reporting_port = gossip_port + 2;
    let api_port = gossip_port + 3;

    let data_dir = std::env::temp_dir().join(format!("rb-placement-{name}-{gossip_port}"));
    let _ = std::fs::remove_dir_all(&data_dir);
    let reconciler_state_dir = data_dir.clone();

    let mayo = Arc::new(RwLock::new(reliaburger::mayo::store::MayoStore::new(
        data_dir.join("metrics"),
    )));
    let readiness = reliaburger::bun::readiness::ReadinessTracker::new();
    readiness.register("agent", true).await;

    let (handle, cluster_runtime) = runtime::start(
        ClusterParams {
            node_name: name.into(),
            gossip_addr: local(gossip_port),
            raft_port,
            reporting_port,
            api_port,
            reporting_config: ReportingTreeSection {
                report_interval_secs: 1,
                max_events_per_report: 100,
                stale_report_timeout_secs: 30,
            },
            seeds,
            wrapping_ikm: None,
            bootstrap_security_state: None,
            data_dir,
            mayo: Some(mayo),
            rollup_interval: Duration::from_millis(500),
            identity: None,
            backup: Default::default(),
            labels: std::collections::BTreeMap::new(),
            self_disk_pressured_rx: None,
            readiness: Some(readiness.clone()),
        },
        shutdown.clone(),
    )
    .await
    .unwrap();

    let council = handle.council.clone();
    let membership_rx = handle.membership_rx.clone();
    let metrics_rx = handle.raft_metrics_rx.clone();
    let aggregated_rx = cluster_runtime.aggregated_rx.clone();
    let rollup_store = Arc::clone(&cluster_runtime.rollup_store);
    let directory_rx = cluster_runtime.directory_rx.clone();

    // Real agent with a ProcessGrill, built with the cluster handle so
    // it answers reporting snapshots with real capacity.
    let (cmd_tx, cmd_rx) = mpsc::channel(256);
    let mut agent = BunAgent::with_cluster(
        ProcessGrill::new(),
        PortAllocator::new(gossip_port + 100, gossip_port + 400),
        cmd_rx,
        shutdown.clone(),
        // The agent needs its OWN handle clone; ClusterHandle isn't
        // Clone (it owns snapshot_rx), so build a second runtime handle
        // by re-taking the pieces. Instead we move `handle` into the
        // agent and keep clones of the watch receivers above.
        handle,
    );
    agent.set_node_capacity(8000, 16384);
    agent.set_readiness_tracker(readiness.clone());
    // Several agents share this host; don't spawn nft against the real
    // host firewall (`with_cluster` enables it by default on Linux).
    agent.set_perimeter_enabled(false);
    let agent_task = reliaburger::bun::readiness::spawn_owned(
        "agent",
        true,
        readiness,
        shutdown.clone(),
        async move { agent.run().await },
    );
    let mut tasks = vec![agent_task];

    // Membership table (peer API addresses = gossip IP + offset 3).
    let membership_table: Arc<RwLock<Vec<NodeMembershipInfo>>> = Arc::new(RwLock::new(Vec::new()));
    {
        let mut rx = membership_rx.clone();
        let table = Arc::clone(&membership_table);
        let sd = shutdown.clone();
        let membership_task = tokio::spawn(async move {
            loop {
                let snapshot: Vec<NodeMembershipInfo> = rx
                    .borrow()
                    .iter()
                    .filter(|m| m.state == reliaburger::mustard::state::NodeState::Alive)
                    .map(|m| NodeMembershipInfo {
                        node_id: m.node_id.clone(),
                        address: SocketAddr::new(m.address.ip(), m.address.port() + 3),
                    })
                    .collect();
                *table.write().await = snapshot;
                tokio::select! {
                    _ = sd.cancelled() => break,
                    changed = rx.changed() => if changed.is_err() { break },
                }
            }
        });
        tasks.push(membership_task);
    }

    // Leader scheduler + autoscaler (fast interval for the test).
    if let Some(council) = &council {
        spawn_leader_scheduler(
            Arc::clone(council),
            membership_rx.clone(),
            aggregated_rx,
            false,
            // Fast learning period so the test doesn't wait long.
            reliaburger::config::node::ReconstructionSection {
                report_threshold_percent: 95,
                learning_period_timeout_secs: 2,
                large_cluster_timeout_secs: 4,
                large_cluster_node_count: 5000,
            },
            shutdown.clone(),
        );
        reliaburger::cluster::orchestrate::spawn_autoscaler(
            Arc::clone(council),
            Arc::clone(&rollup_store),
            Duration::from_millis(500),
            shutdown.clone(),
        );
    }

    // Placement reconciler (API is raft_port + 2 = gossip + 3).
    if let Some(metrics_rx) = metrics_rx.clone() {
        spawn_placement_reconciler(
            name.to_string(),
            metrics_rx,
            directory_rx,
            2, // api_port - raft_port
            None,
            cmd_tx.clone(),
            shutdown.clone(),
            reliaburger::cluster::ClusterHttp::plaintext(),
            Some(reconciler_state_dir),
        );
    }

    // API server.
    let listener = tokio::net::TcpListener::bind(local(api_port))
        .await
        .unwrap();
    let app = api::router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        council.clone(),
        None,
        None,
        None,
        Some(Arc::clone(&membership_table)),
        None,
        api_port,
        None,
    );
    let sd = shutdown.clone();
    let api_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { sd.cancelled().await })
            .await
            .ok();
    });
    tasks.push(api_task);

    // Derive a leadership watch from the raft metrics.
    let (leader_tx, leader_rx) = watch::channel(false);
    if let Some(mut metrics_rx) = metrics_rx {
        let leadership_task = tokio::spawn(async move {
            loop {
                let is_leader = {
                    let m = metrics_rx.borrow();
                    m.current_leader == Some(m.id)
                };
                let _ = leader_tx.send(is_leader);
                if metrics_rx.changed().await.is_err() {
                    break;
                }
            }
        });
        tasks.push(leadership_task);
    }

    let client = BunClient::new(&format!("http://127.0.0.1:{api_port}"));
    for _ in 0..40 {
        if client.health().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // We moved `handle` into the agent; rebuild a thin stand-in for the
    // test's own leadership checks from the metrics watch above.
    Node {
        name: name.to_string(),
        client,
        handle: ClusterHandle {
            membership_rx,
            raft_metrics_rx: None,
            council,
            snapshot_rx: mpsc::channel(1).1,
            wrapping_ikm: None,
            partition_blocklists: Default::default(),
            crl_handle: Default::default(),
        },
        thinks_leader: leader_rx,
        rollup_store,
        _runtime: cluster_runtime,
        _tasks: TestTasks::new(shutdown.clone(), tasks),
    }
}

async fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
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
async fn cluster_stop_clears_desired_state_and_does_not_resurrect() {
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

    // The desired-state map must not contain the app on any council.
    tokio::time::sleep(Duration::from_secs(6)).await;
    for n in &nodes {
        if let Some(council) = &n.handle.council {
            let ds = council.desired_state().await;
            let app = reliaburger::meat::AppId::new("web", "default");
            assert!(
                !ds.apps.contains_key(&app),
                "node {}: desired state still holds the stopped app",
                n.name
            );
            assert!(
                !ds.scheduling.contains_key(&app),
                "node {}: scheduling still holds the stopped app",
                n.name
            );
        }
    }
    assert_eq!(
        live_web_instances(&nodes).await,
        0,
        "a reconciler resurrected the stopped app"
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

/// W8 (L3): a high metric drives the autoscaler to raise the replica
/// override, and the scheduler + reconcilers grow the app to max.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn autoscaler_scales_up_on_high_metric() {
    use reliaburger::mayo::rollup::{NodeRollup, RollupAggregate, RollupEntry};

    let shutdown = CancellationToken::new();
    let n1 = start_node("s1", 18541, vec![], &shutdown).await;
    let n2 = start_node("s2", 18545, vec![local(18541)], &shutdown).await;
    let n3 = start_node("s3", 18549, vec![local(18541)], &shutdown).await;
    let nodes = [&n1, &n2, &n3];

    let ready = wait_until(Duration::from_secs(30), || {
        nodes.iter().any(|n| *n.thinks_leader.borrow())
    })
    .await;
    assert!(ready, "no leader elected");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Deploy a 1-replica app that autoscales on cpu, target 50%, max 3.
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
    // Apply and wait for the single replica to place, re-applying
    // periodically in case a first attempt races leadership under load
    // (the spec is desired state, so re-applying is idempotent).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut last_apply: Option<tokio::time::Instant> = None;
    while tokio::time::Instant::now() < deadline {
        if total_scaler_instances(&nodes).await >= 1 {
            break;
        }
        if last_apply.is_none_or(|t| t.elapsed() >= Duration::from_secs(8)) {
            let applier = nodes
                .iter()
                .find(|n| *n.thinks_leader.borrow())
                .or_else(|| nodes.first())
                .expect("at least one node");
            let _ =
                tokio::time::timeout(Duration::from_secs(15), applier.client.apply(&config)).await;
            last_apply = Some(tokio::time::Instant::now());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Feed a sustained HIGH cpu metric (0.95 » 0.50 target) into every
    // node's rollup store, labelled for the app. The leader's autoscaler
    // reads its own store.
    for node in &nodes {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // The real collector labels per-app metrics `namespace/app` (see
        // mayo::collector), and the autoscaler match is namespace-qualified
        // (M26). Feed the metric in that same form, not the bare app name.
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("app".to_string(), "default/scaler".to_string());
        let rollup = NodeRollup {
            node_id: reliaburger::meat::NodeId::new(&node.name),
            timestamp: now.saturating_sub(30),
            entries: vec![RollupEntry {
                metric_name: "cpu".to_string(),
                labels,
                aggregate: RollupAggregate {
                    min: 0.95,
                    max: 0.95,
                    sum: 0.95,
                    count: 1,
                },
            }],
        };
        let mut w = node.rollup_store.write().await;
        w.ingest(&rollup);
        w.flush().await.unwrap();
    }

    // The autoscaler should raise the override; scheduler + reconcilers
    // grow the app toward max (3).
    let scaled = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut ok = false;
        while tokio::time::Instant::now() < deadline {
            if total_scaler_instances(&nodes).await >= 3 {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        ok
    };

    if !scaled {
        for n in &nodes {
            if let Some(c) = &n.handle.council {
                let ds = c.desired_state().await;
                eprintln!("node {}: overrides={:?}", n.name, ds.autoscale_overrides);
            }
        }
        panic!(
            "autoscaler did not scale up; instances: {}",
            total_scaler_instances(&nodes).await
        );
    }

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.handle.council {
            c.shutdown().await.ok();
        }
    }
}

/// W11 (L14): the quorum safety rail rejects a node-level fault that
/// would risk Raft majority. On a 3-member council `max_allowed = 1`, so
/// the first partition fault is accepted but a second one — which would
/// put two council members at risk — is rejected with a 4xx. Drives the
/// binary path through `/v1/fault`.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn fault_injection_rejected_when_quorum_at_risk() {
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let shutdown = CancellationToken::new();
    let n1 = start_node("r1", 18741, vec![], &shutdown).await;
    let n2 = start_node("r2", 18745, vec![local(18741)], &shutdown).await;
    let n3 = start_node("r3", 18749, vec![local(18741)], &shutdown).await;
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

    let partition_fault = |by: &str| FaultRequest {
        fault_type: FaultType::Partition {
            source_app: None,
            source_cgroup_id: 0,
        },
        target_service: String::new(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(60),
        injected_by: by.into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };

    // First node-level fault: within the quorum budget, accepted.
    leader
        .client
        .inject_fault(&partition_fault("first"))
        .await
        .expect("first partition should be within the quorum budget");

    // Second node-level fault: would put a majority of the 3-member
    // council at risk, so the rail must reject it.
    let rejected = leader.client.inject_fault(&partition_fault("second")).await;
    assert!(
        rejected.is_err(),
        "second node fault should be rejected to protect quorum, got {rejected:?}"
    );
    let msg = format!("{}", rejected.unwrap_err()).to_lowercase();
    assert!(
        msg.contains("quorum"),
        "rejection should cite the quorum rail, got: {msg}"
    );

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.handle.council {
            c.shutdown().await.ok();
        }
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
/// the node rejoins. This drives the binary path: `/v1/chaos/partition`
/// on the isolated node, membership observed through the peers' watch.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
#[ignore = "slow multi-node placement acceptance; run with make test-cluster"]
async fn partition_isolates_a_node_for_real() {
    use reliaburger::mustard::state::NodeState;

    let shutdown = CancellationToken::new();
    let n1 = start_node("q1", 18641, vec![], &shutdown).await;
    let n2 = start_node("q2", 18645, vec![local(18641)], &shutdown).await;
    let n3 = start_node("q3", 18649, vec![local(18641)], &shutdown).await;
    let nodes = [&n1, &n2, &n3];

    // Everyone sees everyone Alive before we cut the wire.
    let converged = wait_until(Duration::from_secs(30), || {
        nodes.iter().all(|obs| {
            ["q1", "q2", "q3"]
                .iter()
                .all(|t| peer_state(obs, t) == Some(NodeState::Alive))
        })
    })
    .await;
    assert!(converged, "cluster never fully converged to Alive");

    // Cut q3 off from q1 and q2. The partition is injected ON q3, whose
    // agent holds the real blocklist handles; the transport drops traffic
    // both to and from the blocked peers, so detection is symmetric.
    n3.client
        .inject_partition(&["q1".to_string(), "q2".to_string()], 60)
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
        .heal_partition()
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
