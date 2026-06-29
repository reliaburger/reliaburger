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
fn spawn_fake_agent(mut rx: mpsc::Receiver<CollectSnapshotRequest>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                req = rx.recv() => {
                    let Some(req) = req else { break };
                    let snapshot = AgentSnapshot {
                        instances: vec![InstanceSnapshot {
                            app_name: "web".to_string(),
                            namespace: "default".to_string(),
                            instance_id: 0,
                            image: "nginx:latest".to_string(),
                            port: Some(8080),
                            container_state: ContainerState::Running,
                            consecutive_unhealthy: 0,
                            uptime: Duration::from_secs(60),
                        }],
                        allocated_ports: vec![8080],
                    };
                    let _ = req.response.send(snapshot);
                }
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_reporting_two_workers_report_to_one_aggregator() {
    let shutdown = CancellationToken::new();

    // One aggregator (the "council member") on a real TCP socket.
    let agg_addr = local(18301);
    let agg_transport = TcpReportingTransport::bind(agg_addr, shutdown.clone())
        .await
        .unwrap();
    let (mut aggregator, watch_rx) =
        ReportAggregator::new(agg_transport, fast_config(), shutdown.clone(), None);
    tokio::spawn(async move { aggregator.run().await });

    // Council list both workers see: the single aggregator.
    let council = vec![(NodeId::new("agg"), agg_addr)];

    // Two workers on their own real TCP sockets, each with a fake agent.
    for (name, port) in [("w1", 18302u16), ("w2", 18303u16)] {
        let transport = TcpReportingTransport::bind(local(port), shutdown.clone())
            .await
            .unwrap();
        let (snap_tx, snap_rx) = mpsc::channel(16);
        spawn_fake_agent(snap_rx, shutdown.clone());
        let (_council_tx, council_rx) = watch::channel(council.clone());
        let mut worker = ReportWorker::new(
            NodeId::new(name),
            transport,
            fast_config(),
            snap_tx,
            council_rx,
            shutdown.clone(),
        );
        tokio::spawn(async move { worker.run().await });
    }

    // Within a few report intervals, the aggregator should hold both reports.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut got_both = false;
    while tokio::time::Instant::now() < deadline {
        let have_both = {
            let reports = &watch_rx.borrow().reports;
            reports.contains_key(&NodeId::new("w1")) && reports.contains_key(&NodeId::new("w2"))
        };
        if have_both {
            got_both = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    assert!(
        got_both,
        "aggregator did not receive both worker reports; have: {:?}",
        watch_rx.borrow().reports.keys().collect::<Vec<_>>()
    );

    shutdown.cancel();
}
