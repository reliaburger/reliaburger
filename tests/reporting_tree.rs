/// Integration tests for the reporting tree.
///
/// Verifies the end-to-end flow: workers send StateReports to assigned
/// council members, and when council membership changes, workers re-hash
/// to surviving members.
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use reliaburger::grill::state::ContainerState;
use reliaburger::patty::NodeId;
use reliaburger::reporting::aggregator::ReportAggregator;
use reliaburger::reporting::assignment::assign_parent;
use reliaburger::reporting::transport::InMemoryReportingNetwork;
use reliaburger::reporting::worker::{
    AgentSnapshot, CollectSnapshotRequest, InstanceSnapshot, ReportWorker,
};

use reliaburger::config::node::ReportingTreeSection;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn fast_config() -> ReportingTreeSection {
    ReportingTreeSection {
        report_interval_secs: 1,
        max_events_per_report: 100,
        stale_report_timeout_secs: 30,
    }
}

/// Spawn a fake agent that responds to snapshot requests with one running app.
fn spawn_fake_agent(mut rx: mpsc::Receiver<CollectSnapshotRequest>, shutdown: CancellationToken) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                req = rx.recv() => {
                    if let Some(req) = req {
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
                    } else {
                        break;
                    }
                }
            }
        }
    });
}

/// Reporting tree failover: remove a council member, verify workers
/// re-hash to surviving members and reports arrive correctly.
#[tokio::test]
async fn reporting_tree_failover() {
    let net = InMemoryReportingNetwork::new();
    let shutdown = CancellationToken::new();

    // Council members: c1 (port 10), c2 (port 11), c3 (port 12)
    let c1_transport = net.register(addr(10)).await;
    let c2_transport = net.register(addr(11)).await;
    let c3_transport = net.register(addr(12)).await;

    // Workers: w1 (port 1), w2 (port 2)
    let w1_transport = net.register(addr(1)).await;
    let w2_transport = net.register(addr(2)).await;

    let initial_council = vec![
        (NodeId::new("c1"), addr(10)),
        (NodeId::new("c2"), addr(11)),
        (NodeId::new("c3"), addr(12)),
    ];

    // Set up aggregators for each council member
    let (mut agg1, mut watch_rx1) =
        ReportAggregator::new(c1_transport, fast_config(), shutdown.clone());
    let (mut agg2, mut watch_rx2) =
        ReportAggregator::new(c2_transport, fast_config(), shutdown.clone());
    let (mut agg3, mut watch_rx3) =
        ReportAggregator::new(c3_transport, fast_config(), shutdown.clone());

    tokio::spawn(async move { agg1.run().await });
    tokio::spawn(async move { agg2.run().await });
    tokio::spawn(async move { agg3.run().await });

    // Set up workers
    let (snap_tx1, snap_rx1) = mpsc::channel(16);
    let (snap_tx2, snap_rx2) = mpsc::channel(16);
    spawn_fake_agent(snap_rx1, shutdown.clone());
    spawn_fake_agent(snap_rx2, shutdown.clone());

    let (council_tx1, council_rx1) = watch::channel(initial_council.clone());
    let (council_tx2, council_rx2) = watch::channel(initial_council.clone());

    let mut worker1 = ReportWorker::new(
        NodeId::new("w1"),
        w1_transport,
        fast_config(),
        snap_tx1,
        council_rx1,
        shutdown.clone(),
    );
    let mut worker2 = ReportWorker::new(
        NodeId::new("w2"),
        w2_transport,
        fast_config(),
        snap_tx2,
        council_rx2,
        shutdown.clone(),
    );

    tokio::spawn(async move { worker1.run().await });
    tokio::spawn(async move { worker2.run().await });

    // Determine which council members the workers initially report to
    let council_ids: Vec<NodeId> = initial_council.iter().map(|(id, _)| id.clone()).collect();
    let w1_parent = assign_parent(&NodeId::new("w1"), &council_ids).unwrap();

    // Wait for initial reports to arrive (up to 3 seconds)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);

    // Find which watch corresponds to each parent
    let parent_index = |parent: &NodeId| -> usize {
        if *parent == NodeId::new("c1") {
            0
        } else if *parent == NodeId::new("c2") {
            1
        } else {
            2
        }
    };

    // Wait for at least one report to arrive at w1's parent
    let w1_idx = parent_index(&w1_parent);
    loop {
        let watch = match w1_idx {
            0 => &mut watch_rx1,
            1 => &mut watch_rx2,
            _ => &mut watch_rx3,
        };
        let _ = tokio::time::timeout_at(deadline, watch.changed()).await;
        if watch.borrow().reports.contains_key(&NodeId::new("w1")) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for w1's initial report");
        }
    }

    // Now remove c3 from the council (simulate council member departure)
    let reduced_council = vec![(NodeId::new("c1"), addr(10)), (NodeId::new("c2"), addr(11))];
    council_tx1.send(reduced_council.clone()).unwrap();
    council_tx2.send(reduced_council.clone()).unwrap();

    // After council change, both workers should re-hash to c1 or c2
    let reduced_ids: Vec<NodeId> = reduced_council.iter().map(|(id, _)| id.clone()).collect();
    let w1_new_parent = assign_parent(&NodeId::new("w1"), &reduced_ids).unwrap();
    let w2_new_parent = assign_parent(&NodeId::new("w2"), &reduced_ids).unwrap();

    // Both must map to either c1 or c2 (not c3)
    assert!(
        w1_new_parent == NodeId::new("c1") || w1_new_parent == NodeId::new("c2"),
        "w1 should remap to c1 or c2, got {w1_new_parent:?}"
    );
    assert!(
        w2_new_parent == NodeId::new("c1") || w2_new_parent == NodeId::new("c2"),
        "w2 should remap to c1 or c2, got {w2_new_parent:?}"
    );

    // Wait for reports to arrive at the new parents (up to 3 seconds)
    let deadline2 = tokio::time::Instant::now() + Duration::from_secs(3);

    // Wait for w1's report at its new parent
    loop {
        let watch = if w1_new_parent == NodeId::new("c1") {
            &mut watch_rx1
        } else {
            &mut watch_rx2
        };
        let _ = tokio::time::timeout_at(deadline2, watch.changed()).await;
        if watch.borrow().reports.contains_key(&NodeId::new("w1")) {
            break;
        }
        if tokio::time::Instant::now() >= deadline2 {
            panic!("timed out waiting for w1's report at new parent after failover");
        }
    }

    // Wait for w2's report at its new parent
    loop {
        let watch = if w2_new_parent == NodeId::new("c1") {
            &mut watch_rx1
        } else {
            &mut watch_rx2
        };
        let _ = tokio::time::timeout_at(deadline2, watch.changed()).await;
        if watch.borrow().reports.contains_key(&NodeId::new("w2")) {
            break;
        }
        if tokio::time::Instant::now() >= deadline2 {
            panic!("timed out waiting for w2's report at new parent after failover");
        }
    }

    shutdown.cancel();
}
