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

    let mayo = Arc::new(RwLock::new(reliaburger::mayo::store::MayoStore::new(
        data_dir.join("metrics"),
    )));

    let (handle, cluster_runtime) = runtime::start(
        ClusterParams {
            node_name: name.into(),
            gossip_addr: local(gossip_port),
            raft_port,
            reporting_port,
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

    // Leak the runtime for the whole test (its tasks must outlive us).
    Box::leak(Box::new(cluster_runtime));

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
    tokio::spawn(async move { agent.run().await });

    // Membership table (peer API addresses = gossip IP + offset 3).
    let membership_table: Arc<RwLock<Vec<NodeMembershipInfo>>> = Arc::new(RwLock::new(Vec::new()));
    {
        let mut rx = membership_rx.clone();
        let table = Arc::clone(&membership_table);
        let sd = shutdown.clone();
        tokio::spawn(async move {
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
    }

    // Leader scheduler + autoscaler (fast interval for the test).
    if let Some(council) = &council {
        spawn_leader_scheduler(
            Arc::clone(council),
            membership_rx.clone(),
            aggregated_rx,
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
            2, // api_port - raft_port
            None,
            cmd_tx.clone(),
            shutdown.clone(),
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
        api_port,
    );
    let sd = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { sd.cancelled().await })
            .await
            .ok();
    });

    // Derive a leadership watch from the raft metrics.
    let (leader_tx, leader_rx) = watch::channel(false);
    if let Some(mut metrics_rx) = metrics_rx {
        tokio::spawn(async move {
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
        },
        thinks_leader: leader_rx,
        rollup_store,
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
        cpu = "200m-400m"
    "#,
    )
    .unwrap();
    // Apply via a follower to also exercise leader-forwarding.
    let applier = nodes
        .iter()
        .find(|n| !*n.thinks_leader.borrow())
        .expect("a follower exists");
    let _ = tokio::time::timeout(Duration::from_secs(20), applier.client.apply(&config)).await;

    // Each node's reconciler should start exactly its share; in total,
    // three instances spread across three distinct nodes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut spread = false;
    while tokio::time::Instant::now() < deadline {
        if total_instances(&nodes).await == 3 && nodes_running_web(&nodes).await == 3 {
            spread = true;
            break;
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
        cpu = "100m-200m"

        [app.scaler.autoscale]
        metric = "cpu"
        target = "50%"
        min = 1
        max = 3
        cooldown = "0s"
    "#,
    )
    .unwrap();
    let applier = nodes
        .iter()
        .find(|n| *n.thinks_leader.borrow())
        .expect("leader");
    let _ = tokio::time::timeout(Duration::from_secs(20), applier.client.apply(&config)).await;

    // Wait until the single replica is placed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while tokio::time::Instant::now() < deadline {
        if total_scaler_instances(&nodes).await >= 1 {
            break;
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
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("app".to_string(), "scaler".to_string());
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
