//! Reporting tree over the REAL TCP transport, in isolation.
//!
//! The in-memory `reporting_tree.rs` covers worker/aggregator logic; this
//! proves the same flow works over `TcpReportingTransport` on real sockets:
//! two workers snapshot their (fake) agent and report to one aggregator,
//! whose aggregated view ends up holding both reports. De-risks the reporting
//! transport ahead of wiring it into the cluster runtime.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use reliaburger::config::node::ReportingTreeSection;
use reliaburger::grill::state::ContainerState;
use reliaburger::meat::NodeId;
use reliaburger::reporting::aggregator::ReportAggregator;
use reliaburger::reporting::transport::{TcpReportingSender, TcpReportingTransport};
use reliaburger::reporting::worker::{
    AgentSnapshot, CollectSnapshotRequest, InstanceSnapshot, ReportWorker,
};

#[path = "support/cluster.rs"]
mod cluster_support;
use cluster_support::local;

fn fast_config() -> ReportingTreeSection {
    ReportingTreeSection {
        report_interval_secs: 1,
        max_events_per_report: 100,
        stale_report_timeout_secs: 30,
        stale_node_grace_secs: 300,
    }
}

/// Answer snapshot requests with a fixed one-instance snapshot.
fn spawn_fake_agent(
    mut rx: mpsc::Receiver<CollectSnapshotRequest>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                req = rx.recv() => {
                    let Some(req) = req else { break };
                    let snapshot = AgentSnapshot {
                        capabilities: Default::default(),
                        readiness: None,
                        egress_degraded: false,
                        egress_affected_workloads: Vec::new(),
                        instances: vec![InstanceSnapshot {
                            execution: Some(reliaburger::grill::RuntimeExecution {
                                instance_id: reliaburger::grill::InstanceId("default__web-g7-0".into()),
                                generation: "a".repeat(64).try_into().unwrap(),
                            }),
                            app_name: "web".to_string(),
                            namespace: "default".to_string(),
                            instance_id: 0,
                            image: "nginx:latest".to_string(),
                            port: Some(8080),
                            container_state: ContainerState::Running,
                            consecutive_unhealthy: 0,
                            uptime: Duration::from_secs(60),
                            cpu_request_millicores: 250,
                            memory_request_mb: 128,
                            gpu_request: 0,
                            egress_enforcement: Default::default(),
                        }],
                        allocated_ports: vec![8080],
                        capacity_cpu_millicores: 4000,
                        capacity_memory_mb: 8192,
                        capacity_gpus: 0,
                        job_reservations: Default::default(),
                        capacity_refusals: Vec::new(),
                        rollouts: Vec::new(),
                    };
                    let _ = req.response.send(snapshot);
                }
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_reporting_two_workers_report_to_one_aggregator() {
    let shutdown = CancellationToken::new();

    // One aggregator (the "council member") on a real TCP socket.
    let agg_transport = TcpReportingTransport::bind(local(0), shutdown.clone())
        .await
        .unwrap();
    let agg_addr = agg_transport.local_addr();
    let (mut aggregator, mut watch_rx) = ReportAggregator::new(
        agg_transport,
        fast_config(),
        shutdown.clone(),
        None,
        None,
        None,
    );
    let mut tasks = vec![tokio::spawn(async move { aggregator.run().await })];

    // Council list both workers see: the single aggregator.
    let council = vec![(NodeId::new("agg"), agg_addr)];

    // Two outbound-only workers, each with a fake agent.
    for name in ["w1", "w2"] {
        let transport = TcpReportingSender::new(None, Default::default());
        let (snap_tx, snap_rx) = mpsc::channel(16);
        tasks.push(spawn_fake_agent(snap_rx, shutdown.clone()));
        let (_council_tx, council_rx) = watch::channel(council.clone());
        let mut worker = ReportWorker::new(
            NodeId::new(name),
            transport,
            fast_config(),
            snap_tx,
            council_rx,
            shutdown.clone(),
        );
        tasks.push(tokio::spawn(async move { worker.run().await }));
    }

    // Wait on the state channel itself. Polling used to miss updates on busy
    // CI runners and produced the observed empty-report flake.
    let got_both = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let have_both = {
                let view = watch_rx.borrow_and_update();
                view.reports.contains_key(&NodeId::new("w1"))
                    && view.reports.contains_key(&NodeId::new("w2"))
            };
            if have_both {
                return true;
            }
            if watch_rx.changed().await.is_err() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false);

    assert!(
        got_both,
        "aggregator did not receive both worker reports; have: {:?}",
        watch_rx.borrow().reports.keys().collect::<Vec<_>>()
    );

    {
        let view = watch_rx.borrow();
        for name in ["w1", "w2"] {
            let execution = view.reports[&NodeId::new(name)].running_apps[0]
                .execution
                .as_ref()
                .unwrap();
            assert_eq!(execution.instance_id.0, "default__web-g7-0");
            assert_eq!(execution.generation.as_str(), "a".repeat(64));
        }
    }

    shutdown.cancel();
    for task in tasks {
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("reporting task did not stop after cancellation")
            .expect("reporting task panicked");
    }
}

// ---------------------------------------------------------------------------
// mTLS reporting
// ---------------------------------------------------------------------------

use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::CrlHandle;
use reliaburger::sesame::types::SerialNumber;

fn identity(hierarchy: &CaHierarchy, node_id: &str, serial: u64) -> NodeIdentity {
    let (cert_der, key_der, serial) = ca::issue_node_cert(
        node_id,
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let now = std::time::SystemTime::now();
    NodeIdentity {
        node_id: node_id.to_string(),
        certificate_der: cert_der,
        private_key_der: key_der,
        serial,
        ca_generation: 0,
        trust: reliaburger::sesame::trust::TrustSet::single(
            hierarchy.node.ca.certificate_der.clone(),
            hierarchy.root.ca.certificate_der.clone(),
        ),
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: now,
        not_after: now + Duration::from_secs(365 * 24 * 3600),
    }
}

fn tls_pair(id: &NodeIdentity) -> (tokio_rustls::TlsAcceptor, tokio_rustls::TlsConnector) {
    let crl = CrlHandle::default();
    let server = reliaburger::sesame::mtls::build_mtls_server_config(id, crl.clone()).unwrap();
    let client = reliaburger::sesame::mtls::build_mtls_client_config(id, crl).unwrap();
    (
        tokio_rustls::TlsAcceptor::from(server),
        tokio_rustls::TlsConnector::from(client),
    )
}

/// Spawn an aggregator + one worker and return whether the aggregator sees the
/// worker's report within the deadline. `worker_tls` being `None` means the
/// worker dials plaintext.
async fn report_arrives(
    agg_tls: Option<(tokio_rustls::TlsAcceptor, tokio_rustls::TlsConnector)>,
    worker_tls: Option<(tokio_rustls::TlsAcceptor, tokio_rustls::TlsConnector)>,
    deadline: Duration,
) -> bool {
    report_arrives_past_idle_sockets(agg_tls, worker_tls, 0, deadline).await
}

/// As `report_arrives`, with `idle_sockets` plain TCP connections opened to
/// the aggregator first that never start a handshake or send a byte.
async fn report_arrives_past_idle_sockets(
    agg_tls: Option<(tokio_rustls::TlsAcceptor, tokio_rustls::TlsConnector)>,
    worker_tls: Option<(tokio_rustls::TlsAcceptor, tokio_rustls::TlsConnector)>,
    idle_sockets: usize,
    deadline: Duration,
) -> bool {
    let shutdown = CancellationToken::new();

    let (agg_acc, agg_con) = match agg_tls {
        Some((a, c)) => (Some(a), Some(c)),
        None => (None, None),
    };
    let agg_transport =
        TcpReportingTransport::bind_tls(local(0), shutdown.clone(), agg_acc, agg_con)
            .await
            .unwrap();
    let agg_addr = agg_transport.local_addr();
    let mut idle = Vec::with_capacity(idle_sockets);
    for _ in 0..idle_sockets {
        idle.push(tokio::net::TcpStream::connect(agg_addr).await.unwrap());
    }
    let (mut aggregator, mut watch_rx) = ReportAggregator::new(
        agg_transport,
        fast_config(),
        shutdown.clone(),
        None,
        None,
        None,
    );
    let aggregator_task = tokio::spawn(async move { aggregator.run().await });

    let (w_acc, w_con) = match worker_tls {
        Some((a, c)) => (Some(a), Some(c)),
        None => (None, None),
    };
    let transport = TcpReportingTransport::bind_tls(local(0), shutdown.clone(), w_acc, w_con)
        .await
        .unwrap();
    let (snap_tx, snap_rx) = mpsc::channel(16);
    let agent_task = spawn_fake_agent(snap_rx, shutdown.clone());
    let council = vec![(NodeId::new("agg"), agg_addr)];
    let (_council_tx, council_rx) = watch::channel(council);
    let mut worker = ReportWorker::new(
        NodeId::new("w1"),
        transport,
        fast_config(),
        snap_tx,
        council_rx,
        shutdown.clone(),
    );
    let worker_task = tokio::spawn(async move { worker.run().await });

    let arrived = tokio::time::timeout(deadline, async {
        loop {
            if watch_rx
                .borrow_and_update()
                .reports
                .contains_key(&NodeId::new("w1"))
            {
                return true;
            }
            if watch_rx.changed().await.is_err() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false);
    drop(idle);
    shutdown.cancel();
    for task in [aggregator_task, agent_task, worker_task] {
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("reporting task did not stop after cancellation")
            .expect("reporting task panicked");
    }
    arrived
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reporting_round_trips_over_mtls() {
    let hierarchy = ca::generate_ca_hierarchy("reporting-test", b"ikm").unwrap();
    let agg = tls_pair(&identity(&hierarchy, "agg", 10));
    let worker = tls_pair(&identity(&hierarchy, "w1", 11));

    assert!(
        report_arrives(Some(agg), Some(worker), Duration::from_secs(10)).await,
        "an mTLS worker's report should reach the mTLS aggregator"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reporting_listener_refuses_a_plaintext_worker_when_mtls_is_required() {
    let hierarchy = ca::generate_ca_hierarchy("reporting-test", b"ikm").unwrap();
    let agg = tls_pair(&identity(&hierarchy, "agg", 10));

    assert!(
        !report_arrives(Some(agg), None, Duration::from_secs(3)).await,
        "a plaintext worker must not report to an mTLS aggregator"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_sockets_do_not_take_report_slots_from_mtls_peers() {
    // #2380: plain TCP sockets that never start a TLS handshake, more of them
    // than the listener reads at once, must not keep an mTLS node's report out.
    let hierarchy = ca::generate_ca_hierarchy("reporting-test", b"ikm").unwrap();
    let agg = tls_pair(&identity(&hierarchy, "agg", 10));
    let worker = tls_pair(&identity(&hierarchy, "w1", 11));

    assert!(
        report_arrives_past_idle_sockets(Some(agg), Some(worker), 100, Duration::from_secs(4))
            .await,
        "idle sockets kept an mTLS worker's report out"
    );
}

// ---------------------------------------------------------------------------
// Reporting at fleet scale (#1411)
// ---------------------------------------------------------------------------

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use reliaburger::mustard::membership::MembershipSnapshot;
use reliaburger::mustard::state::NodeState;
use reliaburger::reporting::ReportingError;
use reliaburger::reporting::transport::{InboundReport, ReportingTransport};
use reliaburger::reporting::types::ReportingMessage;

/// A worker's outbound transport that counts every send and every send the
/// leader didn't admit, which is what a hollow node's log calls "admission
/// at ... failed".
struct CountingSender {
    inner: TcpReportingSender,
    sent: Arc<AtomicUsize>,
    refused: Arc<AtomicUsize>,
}

impl ReportingTransport for CountingSender {
    async fn send(
        &self,
        target: std::net::SocketAddr,
        message: &ReportingMessage,
    ) -> Result<(), ReportingError> {
        self.sent.fetch_add(1, Ordering::Relaxed);
        let result = self.inner.send(target, message).await;
        if result.is_err() {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    async fn recv(&self) -> Option<InboundReport> {
        None
    }
}

/// What a reporting storm measured.
#[derive(Debug)]
struct StormOutcome {
    sent: usize,
    refused: usize,
    /// Nodes whose report the leader's aggregated view held at the end.
    reported: usize,
}

/// `nodes` report workers, each with a fake agent, report to one leader
/// over real TCP for `duration`. All of them start at once, so their report
/// ticks line up, as they do after a leader change, when every worker
/// re-points and reports at the same moment.
async fn report_storm(nodes: usize, interval_secs: u64, duration: Duration) -> StormOutcome {
    let shutdown = CancellationToken::new();
    let config = ReportingTreeSection {
        report_interval_secs: interval_secs,
        ..fast_config()
    };
    let agg_transport = TcpReportingTransport::bind(local(0), shutdown.clone())
        .await
        .unwrap();
    let agg_addr = agg_transport.local_addr();
    // The leader's aggregator checks every report against gossip's member
    // list, as it does in a cluster, so the list is as long as the fleet.
    let members = (0..nodes)
        .map(|index| MembershipSnapshot {
            node_id: NodeId::new(format!("hollow-{index:04}")),
            address: local(0),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: Default::default(),
            first_seen: std::time::Instant::now(),
            resources: None,
        })
        .collect();
    let (_membership_tx, membership_rx) = watch::channel(members);
    let (mut aggregator, watch_rx) = ReportAggregator::new(
        agg_transport,
        config.clone(),
        shutdown.clone(),
        None,
        None,
        Some(membership_rx),
    );
    let mut tasks = vec![tokio::spawn(async move { aggregator.run().await })];

    let sent = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    let council = vec![(NodeId::new("leader"), agg_addr)];
    for index in 0..nodes {
        let transport = CountingSender {
            inner: TcpReportingSender::new(None, Default::default()),
            sent: Arc::clone(&sent),
            refused: Arc::clone(&refused),
        };
        let (snap_tx, snap_rx) = mpsc::channel(16);
        tasks.push(spawn_fake_agent(snap_rx, shutdown.clone()));
        let (_council_tx, council_rx) = watch::channel(council.clone());
        let mut worker = ReportWorker::new(
            NodeId::new(format!("hollow-{index:04}")),
            transport,
            config.clone(),
            snap_tx,
            council_rx,
            shutdown.clone(),
        );
        tasks.push(tokio::spawn(async move { worker.run().await }));
    }

    tokio::time::sleep(duration).await;
    let reported = watch_rx.borrow().reports.len();
    shutdown.cancel();
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(15), task).await;
    }
    StormOutcome {
        sent: sent.load(Ordering::Relaxed),
        refused: refused.load(Ordering::Relaxed),
        reported,
    }
}

/// The hollow-node record of 28 October 2026 counted 15,310 refused report
/// admissions at 500 nodes and about 600 a second at 1,000 (#1411). Five
/// hundred workers whose ticks line up must all be admitted, every time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "run with make test-hollow"]
async fn five_hundred_workers_report_to_one_leader_without_a_refusal() {
    let outcome = report_storm(500, 5, Duration::from_secs(32)).await;
    eprintln!("500 workers: {outcome:?}");
    assert!(
        outcome.sent >= 500 * 2 * 5,
        "too few reports sent: {outcome:?}"
    );
    assert_eq!(
        outcome.refused, 0,
        "the leader refused reports: {outcome:?}"
    );
    assert_eq!(
        outcome.reported, 500,
        "a node's report is missing: {outcome:?}"
    );
}
