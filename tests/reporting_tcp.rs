//! Reporting tree over the REAL TCP transport, in isolation.
//!
//! The in-memory `reporting_tree.rs` covers worker/aggregator logic; this
//! proves the same flow works over `TcpReportingTransport` on real sockets:
//! two workers snapshot their (fake) agent and report to one aggregator,
//! whose aggregated view ends up holding both reports. De-risks the reporting
//! transport ahead of wiring it into the cluster runtime.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use reliaburger::config::node::ReportingTreeSection;
use reliaburger::grill::state::ContainerState;
use reliaburger::meat::NodeId;
use reliaburger::reporting::aggregator::ReportAggregator;
use reliaburger::reporting::transport::TcpReportingTransport;
use reliaburger::reporting::worker::{
    AgentSnapshot, CollectSnapshotRequest, InstanceSnapshot, ReportWorker,
};

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn fast_config() -> ReportingTreeSection {
    ReportingTreeSection {
        report_interval_secs: 1,
        max_events_per_report: 100,
        stale_report_timeout_secs: 30,
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
                        egress_degraded: false,
                        egress_affected_workloads: Vec::new(),
                        instances: vec![InstanceSnapshot {
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
                            egress_enforcement: Default::default(),
                        }],
                        allocated_ports: vec![8080],
                        capacity_cpu_millicores: 4000,
                        capacity_memory_mb: 8192,
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

    // Two workers on their own real TCP sockets, each with a fake agent.
    for name in ["w1", "w2"] {
        let transport = TcpReportingTransport::bind(local(0), shutdown.clone())
            .await
            .unwrap();
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
