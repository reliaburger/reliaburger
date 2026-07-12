//! Multi-node cluster integration tests over REAL UDP + TCP transports.
//!
//! Unlike the in-memory mustard/council tests, these exercise the binary's
//! wiring path: `cluster::runtime::start` binds real sockets, nodes join by
//! address, gossip converges, a Raft leader is elected, and the council grows
//! from gossip membership. This is the proof that the cluster runtime forms a
//! real cluster, not just in a harness.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::ClusterHandle;
use reliaburger::cluster::identity::raft_id_from_name;
use reliaburger::cluster::runtime::{self, ClusterParams, ClusterRuntime};
use reliaburger::config::node::ReportingTreeSection;
use reliaburger::grill::state::ContainerState;
use reliaburger::mustard::state::NodeState;
use reliaburger::reporting::worker::{AgentSnapshot, CollectSnapshotRequest, InstanceSnapshot};

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Stand in for the BunAgent: answer the reporting worker's snapshot requests
/// with a fixed snapshot. (The real agent does this from its supervisor; here
/// we isolate the cluster reporting wiring.)
fn spawn_fake_agent(mut rx: mpsc::Receiver<CollectSnapshotRequest>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                req = rx.recv() => {
                    let Some(req) = req else { break };
                    let _ = req.response.send(AgentSnapshot {
                        instances: vec![InstanceSnapshot {
                            app_name: "web".to_string(),
                            namespace: "default".to_string(),
                            instance_id: 0,
                            image: "nginx:latest".to_string(),
                            port: Some(8080),
                            container_state: ContainerState::Running,
                            consecutive_unhealthy: 0,
                            uptime: Duration::from_secs(1),
                            cpu_request_millicores: 250,
                            memory_request_mb: 128,
                        }],
                        allocated_ports: vec![8080],
                        capacity_cpu_millicores: 4000,
                        capacity_memory_mb: 8192,
                    });
                }
            }
        }
    });
}

/// Start a node. Gossip/raft/reporting ports are adjacent (gossip, +1, +2),
/// so each node on the shared loopback IP gets distinct raft/reporting
/// addresses.
async fn start_node(
    name: &str,
    gossip_port: u16,
    seeds: Vec<SocketAddr>,
    shutdown: &CancellationToken,
) -> (ClusterHandle, ClusterRuntime) {
    start_node_with_mayo(name, gossip_port, seeds, shutdown, None).await
}

async fn start_node_with_mayo(
    name: &str,
    gossip_port: u16,
    seeds: Vec<SocketAddr>,
    shutdown: &CancellationToken,
    mayo: Option<std::sync::Arc<tokio::sync::RwLock<reliaburger::mayo::store::MayoStore>>>,
) -> (ClusterHandle, ClusterRuntime) {
    // A fresh, unique data dir per node so the durable Raft store starts empty
    // (stale state from a prior run would suppress the bootstrap).
    let data_dir = std::env::temp_dir().join(format!("rb-cluster-test-{name}-{gossip_port}"));
    let _ = std::fs::remove_dir_all(&data_dir);

    let (mut handle, runtime) = runtime::start(
        ClusterParams {
            node_name: name.into(),
            gossip_addr: local(gossip_port),
            raft_port: gossip_port + 1,
            reporting_port: gossip_port + 2,
            api_port: gossip_port + 3,
            reporting_config: ReportingTreeSection {
                report_interval_secs: 1,
                max_events_per_report: 100,
                stale_report_timeout_secs: 30,
            },
            seeds,
            wrapping_ikm: None,
            bootstrap_security_state: None,
            data_dir,
            mayo,
            // Fast rollups so tests observe delivery quickly.
            rollup_interval: Duration::from_millis(300),
            identity: None,
        },
        shutdown.clone(),
    )
    .await
    .unwrap();

    // Take the snapshot receiver the agent would normally own and answer it
    // with a fake agent, so the reporting worker can build reports.
    let (_dummy_tx, dummy_rx) = mpsc::channel(1);
    let snapshot_rx = std::mem::replace(&mut handle.snapshot_rx, dummy_rx);
    spawn_fake_agent(snapshot_rx, shutdown.clone());

    (handle, runtime)
}

/// Distinct alive node names in a membership snapshot.
fn alive_names(snap: &[reliaburger::mustard::membership::MembershipSnapshot]) -> Vec<String> {
    let mut names: Vec<String> = snap
        .iter()
        .filter(|m| m.state == NodeState::Alive)
        .map(|m| m.node_id.0.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Current Raft voter id set as seen by this node.
fn voter_ids(h: &ClusterHandle) -> BTreeSet<u64> {
    let Some(rx) = &h.raft_metrics_rx else {
        return BTreeSet::new();
    };
    rx.borrow()
        .membership_config
        .membership()
        .voter_ids()
        .collect()
}

fn thinks_it_is_leader(h: &ClusterHandle) -> bool {
    let Some(rx) = &h.raft_metrics_rx else {
        return false;
    };
    let m = rx.borrow();
    m.current_leader == Some(m.id)
}

/// Poll `cond` every 200ms until it returns true or the timeout elapses.
async fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_join_by_address_and_converge() {
    let shutdown = CancellationToken::new();

    // node-1 is the bootstrap node (no seeds); node-2/3 join by its address.
    let h1 = start_node("node-1", 17441, vec![], &shutdown).await;
    let h2 = start_node("node-2", 17443, vec![local(17441)], &shutdown).await;
    let h3 = start_node("node-3", 17445, vec![local(17441)], &shutdown).await;
    let handles = [&h1.0, &h2.0, &h3.0];

    let expected = vec!["node-1".to_string(), "node-2".into(), "node-3".into()];
    let converged = wait_until(Duration::from_secs(15), || {
        handles
            .iter()
            .all(|h| alive_names(&h.membership_rx.borrow()) == expected)
    })
    .await;

    if !converged {
        let views: Vec<_> = handles
            .iter()
            .map(|h| alive_names(&h.membership_rx.borrow()))
            .collect();
        panic!("gossip did not converge to 3 nodes; views: {views:?}");
    }
    for h in handles {
        assert_eq!(alive_names(&h.membership_rx.borrow()).len(), 3);
    }

    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_council_elects_leader_and_grows() {
    let shutdown = CancellationToken::new();

    let h1 = start_node("c1", 17541, vec![], &shutdown).await;
    let h2 = start_node("c2", 17543, vec![local(17541)], &shutdown).await;
    let h3 = start_node("c3", 17545, vec![local(17541)], &shutdown).await;
    let handles = [&h1.0, &h2.0, &h3.0];

    let expected: BTreeSet<u64> = ["c1", "c2", "c3"]
        .iter()
        .map(|n| raft_id_from_name(n))
        .collect();

    // Gossip converges, a leader emerges, and the council grows to all three.
    let grown = wait_until(Duration::from_secs(25), || {
        handles
            .iter()
            .any(|h| voter_ids(h) == expected && thinks_it_is_leader(h))
    })
    .await;

    if !grown {
        let sets: Vec<_> = handles.iter().map(|h| voter_ids(h)).collect();
        panic!("council did not grow to 3 voters; voter sets: {sets:?}");
    }

    // Exactly one leader, and every node agrees on the 3-voter set.
    assert_eq!(handles.iter().filter(|h| thinks_it_is_leader(h)).count(), 1);
    for h in handles {
        assert_eq!(voter_ids(h), expected);
    }

    // Flat-star reporting tree: every node reports its state to the leader,
    // so the leader's aggregator ends up holding a report from all three.
    let leader_agg = [&h1, &h2, &h3]
        .into_iter()
        .find(|n| thinks_it_is_leader(&n.0))
        .map(|n| n.1.aggregated_rx.clone())
        .expect("a leader exists");
    let expected_reporters: BTreeSet<String> =
        ["c1", "c2", "c3"].iter().map(|s| s.to_string()).collect();
    let reported = wait_until(Duration::from_secs(15), || {
        let names: BTreeSet<String> = leader_agg
            .borrow()
            .reports
            .keys()
            .map(|n| n.0.clone())
            .collect();
        names == expected_reporters
    })
    .await;
    assert!(
        reported,
        "leader did not receive reports from all nodes; have: {:?}",
        leader_agg.borrow().reports.keys().collect::<Vec<_>>()
    );

    shutdown.cancel();
    for h in handles {
        if let Some(c) = &h.council {
            c.shutdown().await.ok();
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 4 W4: rollups + real resource reporting (L6/L11)
// ---------------------------------------------------------------------------

/// A MayoStore holding one flushed sample, so rollups have data.
async fn seeded_mayo_store(
    name: &str,
) -> std::sync::Arc<tokio::sync::RwLock<reliaburger::mayo::store::MayoStore>> {
    use reliaburger::mayo::types::{MetricKey, Sample};

    let dir = std::env::temp_dir().join(format!("rb-w4-mayo-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut store = reliaburger::mayo::store::MayoStore::new(dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    store.insert(&MetricKey::simple("cpu"), Sample::at(now - 30, 42.0));
    store.flush().await.unwrap();
    std::sync::Arc::new(tokio::sync::RwLock::new(store))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollup_worker_delivers_node_rollups_to_the_leader() {
    let shutdown = CancellationToken::new();

    let m1 = seeded_mayo_store("r1").await;
    let m2 = seeded_mayo_store("r2").await;
    let m3 = seeded_mayo_store("r3").await;
    let h1 = start_node_with_mayo("r1", 17641, vec![], &shutdown, Some(m1)).await;
    let h2 = start_node_with_mayo("r2", 17643, vec![local(17641)], &shutdown, Some(m2)).await;
    let h3 = start_node_with_mayo("r3", 17645, vec![local(17641)], &shutdown, Some(m3)).await;
    let nodes = [&h1, &h2, &h3];

    // Wait for a leader.
    let elected = wait_until(Duration::from_secs(25), || {
        nodes.iter().any(|n| thinks_it_is_leader(&n.0))
    })
    .await;
    assert!(elected, "no leader elected");

    let leader = nodes
        .iter()
        .find(|n| thinks_it_is_leader(&n.0))
        .expect("leader exists");
    let rollup_store = std::sync::Arc::clone(&leader.1.rollup_store);

    // L11: the aggregator must ingest rollups pushed by the workers.
    let ingested = wait_until(Duration::from_secs(15), || {
        rollup_store
            .try_read()
            .map(|s| s.buffer_len() >= 3)
            .unwrap_or(false)
    })
    .await;
    assert!(
        ingested,
        "leader rollup store never ingested rollups from all nodes (have {})",
        rollup_store.try_read().map(|s| s.buffer_len()).unwrap_or(0)
    );

    // The /v1/metrics/cluster endpoint on the leader must serve from the
    // store instead of the eternal "no rollup store configured".
    let (cmd_tx, _cmd_rx) = mpsc::channel(1);
    let app = reliaburger::bun::api::router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(std::sync::Arc::clone(&rollup_store)),
        None,
        None,
        9117,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let api_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { api_shutdown.cancelled().await })
            .await
            .ok();
    });

    // The server task needs a beat to start accepting; retry briefly.
    let mut body = String::new();
    for _ in 0..20 {
        if let Ok(response) =
            reqwest::get(format!("http://127.0.0.1:{port}/v1/metrics/cluster")).await
        {
            body = response.text().await.unwrap_or_default();
            if !body.is_empty() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!body.is_empty(), "metrics endpoint never responded");
    assert!(
        !body.contains("no rollup store configured"),
        "endpoint still reports a missing rollup store: {body}"
    );

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.0.council {
            c.shutdown().await.ok();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_reports_carry_nonzero_capacity_and_usage() {
    let shutdown = CancellationToken::new();

    let h1 = start_node("cap1", 17741, vec![], &shutdown).await;
    let h2 = start_node("cap2", 17743, vec![local(17741)], &shutdown).await;
    let nodes = [&h1, &h2];

    let elected = wait_until(Duration::from_secs(25), || {
        nodes.iter().any(|n| thinks_it_is_leader(&n.0))
    })
    .await;
    assert!(elected, "no leader elected");

    let leader_agg = nodes
        .iter()
        .find(|n| thinks_it_is_leader(&n.0))
        .map(|n| n.1.aggregated_rx.clone())
        .expect("leader exists");

    // L6: reports used to carry zeroed resource usage. The fake agent
    // supplies capacity 4000m/8192MB and one instance requesting
    // 250m/128MB; the aggregated view must show exactly that.
    let real_resources = wait_until(Duration::from_secs(15), || {
        let state = leader_agg.borrow();
        state.reports.len() == 2
            && state.reports.values().all(|r| {
                r.resource_usage.cpu_total_millicores == 4000
                    && r.resource_usage.memory_total_mb == 8192
                    && r.resource_usage.cpu_used_millicores == 250
                    && r.resource_usage.memory_used_mb == 128
            })
    })
    .await;
    assert!(real_resources, "aggregated reports still carry zeroes");

    shutdown.cancel();
    for n in nodes {
        if let Some(c) = &n.0.council {
            c.shutdown().await.ok();
        }
    }
}
