//! Multi-node cluster integration tests over REAL UDP + TCP transports.
//!
//! Unlike the in-memory mustard/council tests, these exercise the binary's
//! wiring path: `cluster::runtime::start` binds real sockets, nodes join by
//! address, gossip converges, a Raft leader is elected, and the council grows
//! from gossip membership. This is the proof that the cluster runtime forms a
//! real cluster, not just in a harness.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::ClusterHandle;
use reliaburger::cluster::identity::raft_id_from_name;
use reliaburger::cluster::runtime::{self, ClusterParams, ClusterRuntime};
use reliaburger::config::node::ReportingTreeSection;
use reliaburger::grill::state::ContainerState;
use reliaburger::mustard::state::NodeState;
use reliaburger::reporting::worker::{AgentSnapshot, CollectSnapshotRequest, InstanceSnapshot};
use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::CrlHandle;
use reliaburger::sesame::types::SerialNumber;

#[path = "support/cluster.rs"]
mod cluster_support;
use cluster_support::local;

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
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
                        capabilities: Default::default(),
                        readiness: Some(reliaburger::bun::readiness::NodeReadinessEvidence {
                            ready: true,
                            observed_at_unix_ms: 1,
                            subsystems: vec![reliaburger::bun::readiness::SubsystemEvidence {
                                name: "fake-agent".to_string(),
                                critical: true,
                                state: reliaburger::bun::readiness::SubsystemState::Ready,
                                state_since_unix_ms: 1,
                                last_error: None,
                                last_error_unix_ms: None,
                                restart_count: 0,
                            }],
                        }),
                        egress_degraded: false,
                        egress_affected_workloads: Vec::new(),
                        instances: vec![InstanceSnapshot {
                            execution: None,
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
                            egress_enforcement: Default::default(),
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
            backup: Default::default(),
            labels: std::collections::BTreeMap::new(),
            self_disk_pressured_rx: None,
            readiness: None,
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

fn issued_node_identity(hierarchy: &CaHierarchy, node_id: &str, serial: u64) -> NodeIdentity {
    let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
        node_id,
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let now = SystemTime::now();
    NodeIdentity {
        node_id: node_id.to_string(),
        certificate_der,
        private_key_der,
        serial,
        ca_generation: 0,
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: now,
        not_after: now + Duration::from_secs(365 * 24 * 60 * 60),
    }
}

async fn start_mtls_node(
    name: &str,
    gossip_port: u16,
    seeds: Vec<SocketAddr>,
    identity: NodeIdentity,
    shutdown: &CancellationToken,
) -> (
    ClusterHandle,
    ClusterRuntime,
    reliaburger::sesame::credentials::LiveNodeIdentity,
) {
    let data_dir = std::env::temp_dir().join(format!("rb-cluster-mtls-{name}-{gossip_port}"));
    let _ = std::fs::remove_dir_all(&data_dir);
    let identity_dir = data_dir.join("identity");
    reliaburger::sesame::identity_store::save(&identity_dir, &identity).unwrap();
    let identity = reliaburger::sesame::credentials::LiveNodeIdentity::load(&identity_dir).unwrap();

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
            wrapping_ikm: Some([42; 32]),
            bootstrap_security_state: None,
            data_dir,
            mayo: None,
            rollup_interval: Duration::from_millis(300),
            identity: Some(identity.clone()),
            backup: Default::default(),
            labels: std::collections::BTreeMap::new(),
            self_disk_pressured_rx: None,
            readiness: None,
        },
        shutdown.clone(),
    )
    .await
    .unwrap();

    let (_dummy_tx, dummy_rx) = mpsc::channel(1);
    let snapshot_rx = std::mem::replace(&mut handle.snapshot_rx, dummy_rx);
    spawn_fake_agent(snapshot_rx, shutdown.clone());
    (handle, runtime, identity)
}

async fn spawn_identity_observing_api(
    identity: &NodeIdentity,
    saw_client_certificate: Arc<AtomicBool>,
    shutdown: CancellationToken,
) -> SocketAddr {
    use tower::Service;

    let acceptor = tokio_rustls::TlsAcceptor::from(
        reliaburger::sesame::mtls::build_api_server_config(identity, CrlHandle::default()).unwrap(),
    );
    let router = axum::Router::new().route("/probe", axum::routing::get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(local(0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut make_service = router.into_make_service();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                accepted = listener.accept() => {
                    let Ok((tcp, _)) = accepted else { continue };
                    let acceptor = acceptor.clone();
                    let saw_client_certificate = saw_client_certificate.clone();
                    let service = match make_service.call(()).await {
                        Ok(service) => service,
                        Err(infallible) => match infallible {},
                    };
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        saw_client_certificate.store(
                            tls.get_ref().1.peer_certificates().is_some_and(|c| !c.is_empty()),
                            Ordering::SeqCst,
                        );
                        let service = hyper_util::service::TowerToHyperService::new(service);
                        let _ = hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                        .await;
                    });
                }
            }
        }
    });
    address
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

/// How often `wait_until` re-checks its condition in this binary.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

async fn wait_until(timeout: Duration, cond: impl FnMut() -> bool) -> bool {
    cluster_support::wait_until(timeout, POLL_INTERVAL, cond).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow multi-node gossip and council acceptance; run with make test-cluster"]
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
#[ignore = "slow multi-node gossip and council acceptance; run with make test-cluster"]
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
    // Wait for every node to agree on the 3-voter set with exactly one leader,
    // not just for one node to see it: the membership change that admits the
    // third voter commits on the leader a tick before it reaches the
    // followers' Raft metrics, so a wait that stops at the first node to
    // notice would race the followers under load.
    let grown = wait_until(Duration::from_secs(25), || {
        handles.iter().all(|h| voter_ids(h) == expected)
            && handles.iter().filter(|h| thinks_it_is_leader(h)).count() == 1
    })
    .await;

    if !grown {
        let sets: Vec<_> = handles.iter().map(|h| voter_ids(h)).collect();
        let leaders = handles.iter().filter(|h| thinks_it_is_leader(h)).count();
        panic!("council did not converge on 3 voters; voter sets: {sets:?}, leaders: {leaders}");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow multi-node mTLS acceptance; run with make test-cluster"]
async fn generated_security_model_protects_all_live_cluster_transports() {
    let shutdown = CancellationToken::new();
    let _cancel_on_drop = CancelOnDrop(shutdown.clone());
    let hierarchy = ca::generate_ca_hierarchy("cluster-mtls-test", b"cluster-mtls-ikm").unwrap();
    let identity_1 = issued_node_identity(&hierarchy, "tls-1", 10);
    let identity_2 = issued_node_identity(&hierarchy, "tls-2", 11);
    let identity_3 = issued_node_identity(&hierarchy, "tls-3", 12);

    let node_1 = start_mtls_node("tls-1", 17841, vec![], identity_1.clone(), &shutdown).await;
    let node_2 = start_mtls_node(
        "tls-2",
        17843,
        vec![local(17841)],
        identity_2.clone(),
        &shutdown,
    )
    .await;
    let node_3 = start_mtls_node("tls-3", 17845, vec![local(17841)], identity_3, &shutdown).await;
    let nodes = [&node_1, &node_2, &node_3];

    let expected_voters: BTreeSet<u64> = ["tls-1", "tls-2", "tls-3"]
        .iter()
        .map(|name| raft_id_from_name(name))
        .collect();
    assert!(
        wait_until(Duration::from_secs(25), || {
            nodes
                .iter()
                .all(|node| voter_ids(&node.0) == expected_voters)
                && nodes
                    .iter()
                    .filter(|node| thinks_it_is_leader(&node.0))
                    .count()
                    == 1
        })
        .await,
        "the mTLS Raft council did not converge on three voters"
    );

    let expected_reporters: BTreeSet<String> = ["tls-1", "tls-2", "tls-3"]
        .iter()
        .map(|name| name.to_string())
        .collect();
    assert!(
        wait_until(Duration::from_secs(15), || {
            nodes
                .iter()
                .find(|node| thinks_it_is_leader(&node.0))
                .map(|leader| {
                    leader
                        .1
                        .aggregated_rx
                        .borrow()
                        .reports
                        .keys()
                        .map(|node_id| node_id.0.clone())
                        .collect::<BTreeSet<_>>()
                        == expected_reporters
                })
                .unwrap_or(false)
        })
        .await,
        "the mTLS reporting tree did not deliver all three node reports"
    );

    // Keep the running runtime and all its listeners/connectors. Revoke the
    // original leaves after replacement so static credentials cannot pass.
    for (index, node) in nodes.iter().enumerate() {
        let replacement =
            issued_node_identity(&hierarchy, &format!("tls-{}", index + 1), 20 + index as u64);
        node.2.replace(replacement).await.unwrap();
    }
    let revoked = reliaburger::sesame::types::Crl {
        retired_nodes: Default::default(),
        entries: (10..=12)
            .map(|serial| reliaburger::sesame::types::CrlEntry {
                serial: SerialNumber(serial),
                issuer: reliaburger::sesame::types::CaRole::Node,
                revoked_at: SystemTime::now(),
                reason: "superseded test identity".into(),
                expires_at: None,
            })
            .collect(),
        version: 1,
        updated_at: SystemTime::now(),
    };
    for node in nodes {
        node.0.crl_handle.update(revoked.clone());
    }
    let reports_after = SystemTime::now() + Duration::from_secs(1);
    let leader = nodes
        .iter()
        .find(|node| thinks_it_is_leader(&node.0))
        .unwrap();
    let council = leader.0.council.as_ref().unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        council.write(reliaburger::council::types::RaftRequest::AllocateSerial),
    )
    .await
    .unwrap()
    .unwrap();
    let committed = council.raft().metrics().borrow().last_applied;
    assert!(
        wait_until(Duration::from_secs(15), || {
            nodes.iter().all(|node| {
                node.0
                    .raft_metrics_rx
                    .as_ref()
                    .unwrap()
                    .borrow()
                    .last_applied
                    >= committed
            })
        })
        .await,
        "Raft stopped replicating after the old leaves were revoked"
    );
    assert!(
        wait_until(Duration::from_secs(15), || {
            let reports = leader.1.aggregated_rx.borrow();
            reports.reports.len() == 3
                && reports
                    .reports
                    .values()
                    .all(|report| report.timestamp > reports_after)
        })
        .await,
        "reporting did not reconnect with renewed credentials"
    );

    let saw_client_certificate = Arc::new(AtomicBool::new(false));
    let api_address = spawn_identity_observing_api(
        &identity_2,
        saw_client_certificate.clone(),
        shutdown.clone(),
    )
    .await;
    let peer_http = reliaburger::cluster::ClusterHttp::secure(
        reliaburger::sesame::mtls::build_cluster_http_client(&identity_1, CrlHandle::default())
            .unwrap(),
    );
    let response = peer_http
        .client()
        .get(peer_http.url(&api_address.to_string(), "/probe"))
        .send()
        .await
        .expect("the mTLS peer API call should succeed");
    assert_eq!(response.text().await.unwrap(), "ok");
    assert!(
        saw_client_certificate.load(Ordering::SeqCst),
        "the cross-node API call did not present the source node identity"
    );

    shutdown.cancel();
    for node in nodes {
        if let Some(council) = &node.0.council {
            council.shutdown().await.ok();
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

    // The rollup generator rolls up one COMPLETE aligned minute — the minute
    // before the one it pushes in (`[aligned_end - 60, aligned_end)`). A single
    // sample at a fixed offset (the old `now - 30`) only lands inside that
    // window for half the wall-clock phases: if the test happens to run in the
    // second half of a minute, `now - 30` falls in the *current* minute, the
    // rolled-up previous minute is empty, and the leader ingests rollups with
    // zero entries forever (buffer_len stays 0). That is the flake — nothing to
    // do with leader resolution or delivery, which both work every run.
    //
    // Seed one sample squarely inside each aligned minute the worker might roll
    // up across the ~20s window (the previous minute and the current one, since
    // a push can cross a boundary), so whichever complete minute the generator
    // queries always has data.
    let minute_start = now - (now % 60);
    for &window_start in &[minute_start - 60, minute_start] {
        store.insert(
            &MetricKey::simple("cpu"),
            Sample::at(window_start + 30, 42.0),
        );
    }
    store.flush().await.unwrap();
    std::sync::Arc::new(tokio::sync::RwLock::new(store))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow multi-node gossip and council acceptance; run with make test-cluster"]
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

    // L11: the aggregator must ingest rollups pushed by the workers. Leadership
    // can settle on a different node than the first to claim it, and the workers
    // push to whichever node is currently leader — so re-find the leader on every
    // poll rather than binding it once. (Under the ci profile retries=0, a
    // one-shot sample of a node that then loses leadership fails outright with 0.)
    let ingested = wait_until(Duration::from_secs(20), || {
        nodes
            .iter()
            .find(|n| thinks_it_is_leader(&n.0))
            .and_then(|n| n.1.rollup_store.try_read().ok())
            .map(|s| s.buffer_len() >= 3)
            .unwrap_or(false)
    })
    .await;

    let leader = nodes
        .iter()
        .find(|n| thinks_it_is_leader(&n.0))
        .expect("leader exists");
    let rollup_store = std::sync::Arc::clone(&leader.1.rollup_store);
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
        None,
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
#[ignore = "slow multi-node gossip and council acceptance; run with make test-cluster"]
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

/// Serve the real renewal API over TLS; optionally hold the first request until
/// the test has shut down this council member. The caller owns every task.
async fn renewal_api(
    council: Arc<reliaburger::council::CouncilNode>,
    identity: reliaburger::sesame::credentials::LiveNodeIdentity,
    hold: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    requests: Arc<std::sync::atomic::AtomicUsize>,
    shutdown: CancellationToken,
    tasks: &mut tokio::task::JoinSet<()>,
) -> SocketAddr {
    let (tx, _rx) = mpsc::channel(1);
    let app = reliaburger::bun::api::router(
        tx,
        None,
        None,
        None,
        None,
        None,
        Some(council),
        None,
        Some("renewal-test-token".into()),
        None,
        None,
        None,
        0,
        None,
    )
    .layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let hold = hold.clone();
            let requests = requests.clone();
            async move {
                if requests.fetch_add(1, Ordering::SeqCst) == 0
                    && let Some((entered, release)) = hold
                {
                    entered.notify_one();
                    release.notified().await;
                }
                next.run(request).await
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind(local(0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(
        reliaburger::sesame::mtls::build_live_api_server_config(&identity, CrlHandle::default())
            .unwrap(),
    );
    tasks.spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = connections.join_next(), if !connections.is_empty() => {},
                accepted = listener.accept() => {
                    let (tcp, _) = accepted.unwrap();
                    let acceptor = acceptor.clone(); let app = app.clone();
                    connections.spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        let leaf = tls.get_ref().1.peer_certificates().unwrap()[0].clone();
                        let service = app.layer(axum::Extension(reliaburger::sesame::renewal::TlsPeerCertificate(leaf)));
                        let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                            .serve_connection(hyper_util::rt::TokioIo::new(tls), hyper_util::service::TowerToHyperService::new(service)).await;
                    });
                }
            }
        }
    });
    address
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow multi-node certificate renewal acceptance; run with make test-cluster"]
async fn node_renewal_retries_directly_after_leader_failure_and_persists_the_new_leaf() {
    use reliaburger::sesame::{
        renewal_worker::{NodeRenewalWorker, RenewalState},
        types::SecurityState,
    };
    let shutdown = CancellationToken::new();
    let _cancel_on_drop = CancelOnDrop(shutdown.clone());
    let hierarchy = ca::generate_ca_hierarchy("renewal-cluster", &[42; 32]).unwrap();
    let names = ["renew-1", "renew-2", "renew-3"];
    let ports = [17881, 17883, 17885];
    let mut nodes = Vec::new();
    for index in 0..3 {
        nodes.push(
            start_mtls_node(
                names[index],
                ports[index],
                if index == 0 {
                    vec![]
                } else {
                    vec![local(ports[0])]
                },
                issued_node_identity(&hierarchy, names[index], 10 + index as u64),
                &shutdown,
            )
            .await,
        );
    }
    let voters: BTreeSet<_> = names.iter().map(|name| raft_id_from_name(name)).collect();
    assert!(
        wait_until(Duration::from_secs(30), || {
            nodes.iter().all(|node| voter_ids(&node.0) == voters)
                && nodes
                    .iter()
                    .filter(|node| thinks_it_is_leader(&node.0))
                    .count()
                    == 1
        })
        .await,
        "initial council did not converge"
    );
    let old_leader = nodes
        .iter()
        .position(|node| thinks_it_is_leader(&node.0))
        .unwrap();
    let worker_index = (old_leader + 1) % 3;
    let old_council = nodes[old_leader].0.council.as_ref().unwrap();
    old_council
        .write(reliaburger::council::RaftRequest::SecurityStateInit(
            Box::new(SecurityState {
                certificate_authorities: vec![hierarchy.root.ca.clone(), hierarchy.node.ca.clone()],
                next_serial: 100,
                ..Default::default()
            }),
        ))
        .await
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls: Vec<_> = (0..3)
        .map(|_| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
        .collect();
    let mut tasks = tokio::task::JoinSet::new();
    let mut members = Vec::new();
    for index in 0..3 {
        let address = renewal_api(
            nodes[index].0.council.as_ref().unwrap().clone(),
            nodes[index].2.clone(),
            (index == old_leader).then(|| (entered.clone(), release.clone())),
            calls[index].clone(),
            shutdown.clone(),
            &mut tasks,
        )
        .await;
        members.push(reliaburger::bun::api::NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId::new(names[index]),
            address,
            api_advertised: true,
        });
    }
    let local_api = members[worker_index].address;
    let members = Arc::new(tokio::sync::RwLock::new(members));
    let (worker, monitor) = NodeRenewalWorker::new(
        nodes[worker_index].2.clone(),
        nodes[worker_index].0.crl_handle.clone(),
        "renewal-test-token",
    )
    .unwrap();
    let council = nodes[worker_index].0.council.as_ref().unwrap().clone();
    let stop = shutdown.clone();
    tasks.spawn(async move {
        worker.run(council, members, local_api, stop).await;
    });
    assert!(
        wait_until(Duration::from_secs(5), || monitor.state()
            == RenewalState::Valid)
        .await
    );

    // Model a node whose existing leaf has passed its signed lifetime midpoint.
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::default();
    params.serial_number = Some(20u64.into());
    params.subject_alt_names = vec![rcgen::SanType::URI(
        ca::node_spiffe_uri(names[worker_index]).try_into().unwrap(),
    )];
    params.extended_key_usages = vec![
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        rcgen::ExtendedKeyUsagePurpose::ClientAuth,
    ];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::seconds(300);
    params.not_after = now + time::Duration::seconds(120);
    let issuer = hierarchy
        .node
        .certificate_params
        .clone()
        .self_signed(&hierarchy.node.signing_keypair)
        .unwrap();
    let mut due = (*nodes[worker_index].2.snapshot()).clone();
    due.certificate_der = params
        .signed_by(&key, &issuer, &hierarchy.node.signing_keypair)
        .unwrap()
        .der()
        .to_vec();
    due.private_key_der = key.serialize_der();
    due.serial = SerialNumber(20);
    nodes[worker_index].2.replace(due).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    old_council.shutdown().await.unwrap();
    release.notify_one();
    assert!(
        wait_until(Duration::from_secs(15), || {
            nodes
                .iter()
                .enumerate()
                .any(|(index, node)| index != old_leader && thinks_it_is_leader(&node.0))
        })
        .await,
        "surviving council members did not elect a leader"
    );
    assert!(
        wait_until(Duration::from_secs(25), || {
            nodes[worker_index].2.snapshot().serial.0 >= 100
                && monitor.state() == RenewalState::Valid
        })
        .await,
        "node did not renew through the new leader"
    );
    let new_leader = nodes
        .iter()
        .enumerate()
        .find(|(index, node)| *index != old_leader && thinks_it_is_leader(&node.0))
        .unwrap()
        .0;
    assert!(calls[old_leader].load(Ordering::SeqCst) > 0);
    assert!(calls[new_leader].load(Ordering::SeqCst) > 0);
    let installed = nodes[worker_index].2.snapshot();
    let identity_dir = std::env::temp_dir()
        .join(format!(
            "rb-cluster-mtls-{}-{}",
            names[worker_index], ports[worker_index]
        ))
        .join("identity");
    let restarted =
        reliaburger::sesame::credentials::LiveNodeIdentity::load(&identity_dir).unwrap();
    assert_eq!(
        restarted.snapshot().certificate_der,
        installed.certificate_der
    );
    assert!(installed.not_after > SystemTime::now() + Duration::from_secs(24 * 3600));
    shutdown.cancel();
    for (index, node) in nodes.iter().enumerate() {
        if index != old_leader {
            node.0.council.as_ref().unwrap().shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow multi-node test-token cleanup acceptance; run with make test-cluster"]
async fn leased_token_cleanup_survives_leader_failure_without_revoking_operator_tokens() {
    use reliaburger::council::RaftRequest;
    use reliaburger::sesame::types::{ApiRole, ApiToken, TokenScope};
    use reliaburger::testkit::lease::{TestLease, now_unix_millis, spawn_cluster_lease_reaper};
    let shutdown = CancellationToken::new();
    let _cancel_on_drop = CancelOnDrop(shutdown.clone());
    let hierarchy = ca::generate_ca_hierarchy("leased-token-cluster", &[42; 32]).unwrap();
    let names = ["token-1", "token-2", "token-3"];
    let ports = [17941, 17943, 17945];
    let mut nodes = Vec::new();
    for index in 0..3 {
        nodes.push(
            start_mtls_node(
                names[index],
                ports[index],
                if index == 0 {
                    vec![]
                } else {
                    vec![local(ports[0])]
                },
                issued_node_identity(&hierarchy, names[index], 10 + index as u64),
                &shutdown,
            )
            .await,
        );
    }
    let voters: BTreeSet<_> = names.iter().map(|name| raft_id_from_name(name)).collect();
    assert!(
        wait_until(Duration::from_secs(30), || nodes
            .iter()
            .all(|node| voter_ids(&node.0) == voters)
            && nodes
                .iter()
                .filter(|node| thinks_it_is_leader(&node.0))
                .count()
                == 1)
        .await
    );
    let old_leader = nodes
        .iter()
        .position(|node| thinks_it_is_leader(&node.0))
        .unwrap();
    let council = nodes[old_leader].0.council.as_ref().unwrap();
    let operator = ApiToken {
        name: "operator".into(),
        token_hash: vec![1; 32],
        token_salt: vec![2; 16],
        role: ApiRole::Admin,
        scope: TokenScope::default(),
        expires_at: None,
        created_at: SystemTime::now(),
    };
    council
        .write(RaftRequest::CreateApiToken(operator.clone()))
        .await
        .unwrap();
    let now = now_unix_millis();
    let lease = TestLease::new(
        "token-cleanup".into(),
        "operator-fingerprint".into(),
        "operator".into(),
        "rbtest-leader".into(),
        now,
        now + 3_000,
    )
    .unwrap();
    council
        .write(RaftRequest::TestLeaseCreate(lease.clone()))
        .await
        .unwrap();
    let token = ApiToken {
        name: "rbtest-leader-scope".into(),
        token_hash: vec![3; 32],
        token_salt: vec![4; 16],
        role: ApiRole::Deployer,
        scope: TokenScope {
            apps: None,
            namespaces: Some(vec![lease.namespace.clone()]),
        },
        expires_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(lease.expires_at_unix_ms)),
        created_at: SystemTime::now(),
    };
    let admitted = council
        .write(RaftRequest::TestLeaseApiToken {
            lease_id: lease.lease_id.clone(),
            owner_id: lease.owner_id.clone(),
            observed_at_unix_ms: now_unix_millis(),
            token: Box::new(token),
        })
        .await
        .unwrap();
    assert!(!matches!(
        admitted,
        reliaburger::council::CouncilResponse::Refused { .. }
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut replicated = true;
            for node in &nodes {
                replicated &= node
                    .0
                    .council
                    .as_ref()
                    .unwrap()
                    .security_state()
                    .await
                    .api_tokens
                    .len()
                    == 2;
            }
            if replicated {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    council.shutdown().await.unwrap();
    let mut reapers = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        if index != old_leader {
            reapers.push(spawn_cluster_lease_reaper(
                node.0.council.as_ref().unwrap().clone(),
                shutdown.clone(),
            ));
        }
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut reclaimed = true;
            for (index, node) in nodes.iter().enumerate() {
                if index == old_leader {
                    continue;
                }
                let council = node.0.council.as_ref().unwrap();
                reclaimed &= !council
                    .desired_state()
                    .await
                    .test_leases
                    .contains_key(&lease.lease_id);
                reclaimed &= council.security_state().await.api_tokens == vec![operator.clone()];
            }
            if reclaimed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    shutdown.cancel();
    for reaper in reapers {
        reaper.await.unwrap();
    }
}
