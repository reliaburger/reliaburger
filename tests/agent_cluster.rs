/// Integration tests for agent cluster wiring.
///
/// Verifies that the agent correctly reads cluster data from gossip
/// and Raft subsystems, and responds to snapshot requests from the
/// reporting worker.
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::{AgentCommand, BunAgent, ClusterHandle};
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest};
use reliaburger::grill::ProcessGrill;
use reliaburger::grill::port::PortAllocator;
use reliaburger::meat::NodeId;
use reliaburger::mustard::membership::MembershipSnapshot;
use reliaburger::mustard::state::NodeState;
use reliaburger::mustard::transport::{InMemoryNetwork, MustardTransport, UdpMustardTransport};
use reliaburger::mustard::{GossipConfig, MustardNode};
use reliaburger::reporting::transport::ReportingTransport;
use reliaburger::reporting::worker::CollectSnapshotRequest;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn fast_council_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 200,
        election_timeout_max_ms: 400,
        snapshot_threshold: 100,
        max_in_snapshot_log_to_keep: 50,
    }
}

fn node_info(id: u64, port: u16) -> CouncilNodeInfo {
    CouncilNodeInfo::new(
        format!("127.0.0.1:{port}").parse().unwrap(),
        format!("node-{id}"),
    )
}

/// Agent nodes endpoint returns gossip membership when cluster is wired.
#[tokio::test]
async fn agent_nodes_returns_membership() {
    let shutdown = CancellationToken::new();

    // Create a membership watch channel with some data
    let snapshots = vec![
        MembershipSnapshot {
            node_id: NodeId::new("node-1"),
            address: addr(9443),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: true,
            is_leader: true,
            labels: BTreeMap::from([("zone".to_string(), "us-east".to_string())]),
        },
        MembershipSnapshot {
            node_id: NodeId::new("node-2"),
            address: addr(9444),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: BTreeMap::new(),
        },
    ];
    let (_membership_tx, membership_rx) = watch::channel(snapshots);

    let (_snapshot_tx, snapshot_rx) = mpsc::channel(16);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    let cluster = ClusterHandle {
        membership_rx,
        raft_metrics_rx: None,
        council: None,
        snapshot_rx,
    };

    let grill = ProcessGrill::new();
    let port_allocator = PortAllocator::new(50000, 51000);
    let mut agent =
        BunAgent::with_cluster(grill, port_allocator, cmd_rx, shutdown.clone(), cluster);

    let handle = tokio::spawn(async move { agent.run().await });

    // Query nodes
    let (resp_tx, resp_rx) = oneshot::channel();
    cmd_tx
        .send(AgentCommand::Nodes { response: resp_tx })
        .await
        .unwrap();
    let nodes = resp_rx.await.unwrap();

    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].node_id, "node-1");
    assert_eq!(nodes[0].state, "alive");
    assert!(nodes[0].is_leader);
    assert_eq!(nodes[0].labels.get("zone").unwrap(), "us-east");
    assert_eq!(nodes[1].node_id, "node-2");
    assert!(!nodes[1].is_council);

    shutdown.cancel();
    let _ = handle.await;
}

/// Agent council endpoint returns Raft state when council is wired.
#[tokio::test]
async fn agent_council_returns_raft_state() {
    let shutdown = CancellationToken::new();

    // Set up a single-node Raft cluster
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let council = CouncilNode::new(
        1,
        fast_council_config(),
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
    )
    .await
    .unwrap();
    router.register(1, council.raft().clone()).await;

    let mut members = BTreeMap::new();
    members.insert(1, node_info(1, 9444));
    council.initialize(members).await.unwrap();

    // Wait for leader
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(council.is_leader().await);

    // Write an app
    council
        .write(RaftRequest::AppSpec {
            app_id: reliaburger::meat::AppId::new("web", "prod"),
            spec: Box::new(toml::from_str(r#"image = "test:v1""#).unwrap()),
        })
        .await
        .unwrap();

    let council = std::sync::Arc::new(council);
    let raft_metrics_rx = council.metrics();

    let (_membership_tx, membership_rx) = watch::channel(vec![]);
    let (_snapshot_tx, snapshot_rx) = mpsc::channel(16);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    let cluster = ClusterHandle {
        membership_rx,
        raft_metrics_rx: Some(raft_metrics_rx),
        council: Some(council.clone()),
        snapshot_rx,
    };

    let grill = ProcessGrill::new();
    let port_allocator = PortAllocator::new(50000, 51000);
    let mut agent =
        BunAgent::with_cluster(grill, port_allocator, cmd_rx, shutdown.clone(), cluster);

    let handle = tokio::spawn(async move { agent.run().await });

    // Query council
    let (resp_tx, resp_rx) = oneshot::channel();
    cmd_tx
        .send(AgentCommand::Council { response: resp_tx })
        .await
        .unwrap();
    let status = resp_rx.await.unwrap();

    assert!(status.leader.is_some());
    assert_eq!(status.leader.unwrap(), "node-1");
    assert_eq!(status.members.len(), 1);
    assert_eq!(status.members[0].name, "node-1");
    assert_eq!(status.app_count, 1);
    assert!(status.term > 0);

    shutdown.cancel();
    council.shutdown().await.ok();
    let _ = handle.await;
}

/// UDP gossip transport sends and receives messages.
#[tokio::test]
async fn udp_gossip_transport_round_trip() {
    use reliaburger::mustard::message::{GossipMessage, GossipPayload};

    let t1 = UdpMustardTransport::bind(addr(0)).await.unwrap();
    let t2 = UdpMustardTransport::bind(addr(0)).await.unwrap();

    let t2_addr = t2.local_addr();

    let msg = GossipMessage::new(
        NodeId::new("sender"),
        1,
        GossipPayload::Ping { updates: vec![] },
    );

    t1.send(t2_addr, &msg).await.unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), t2.recv()).await;
    assert!(result.is_ok());
    let (_from, received) = result.unwrap().unwrap();
    assert_eq!(received.sender, NodeId::new("sender"));
}

/// TCP reporting transport sends and receives messages.
#[tokio::test]
async fn tcp_reporting_transport_round_trip() {
    use reliaburger::reporting::transport::TcpReportingTransport;
    use reliaburger::reporting::types::{ReportingMessage, ResourceUsage, StateReport};
    use std::time::SystemTime;

    let shutdown = CancellationToken::new();

    let server = TcpReportingTransport::bind(addr(0), shutdown.clone())
        .await
        .unwrap();
    let server_addr = server.local_addr();

    // Client sends to server
    let report = StateReport {
        node_id: NodeId::new("w1"),
        timestamp: SystemTime::now(),
        running_apps: vec![],
        cached_specs: vec![],
        resource_usage: ResourceUsage::default(),
        event_log: vec![],
    };
    let msg = ReportingMessage::Report(report);

    // The server transport also implements ReportingTransport for sending
    // But for client→server, we call send on any transport pointing at the server
    let client = TcpReportingTransport::bind(addr(0), shutdown.clone())
        .await
        .unwrap();
    client.send(server_addr, &msg).await.unwrap();

    let result = tokio::time::timeout(Duration::from_secs(2), server.recv()).await;
    assert!(result.is_ok());
    let (_, received) = result.unwrap().unwrap();
    match received {
        ReportingMessage::Report(r) => assert_eq!(r.node_id, NodeId::new("w1")),
        _ => panic!("expected Report"),
    }

    shutdown.cancel();
}

/// MustardNode publishes membership snapshots via watch channel.
#[tokio::test]
async fn mustard_node_publishes_membership_watch() {
    let net = InMemoryNetwork::new();
    let t1 = net.register(addr(1)).await;
    let _t2 = net.register(addr(2)).await;
    let shutdown = CancellationToken::new();

    let config = GossipConfig::default();
    let mut node1 = MustardNode::new(NodeId::new("n1"), addr(1), config.clone(), t1);
    node1.add_seed(NodeId::new("n2"), addr(2));

    let (membership_tx, membership_rx) = watch::channel(vec![]);
    node1.set_membership_watch(membership_tx);

    let node_shutdown = shutdown.clone();
    let handle = tokio::spawn(async move { node1.run(node_shutdown).await });

    // Wait for a gossip cycle to publish
    tokio::time::sleep(Duration::from_millis(500)).await;

    let snapshot = membership_rx.borrow().clone();
    // Should have at least node1 itself
    assert!(
        snapshot.iter().any(|m| m.node_id == NodeId::new("n1")),
        "n1 should appear in membership snapshot"
    );

    shutdown.cancel();
    let _ = handle.await;
}

/// CollectSnapshotRequest handler returns instance data.
#[tokio::test]
async fn agent_snapshot_request_returns_instances() {
    let shutdown = CancellationToken::new();

    let (_membership_tx, membership_rx) = watch::channel(vec![]);
    let (snapshot_tx, snapshot_rx) = mpsc::channel(16);
    let (_cmd_tx, cmd_rx) = mpsc::channel(256);

    let cluster = ClusterHandle {
        membership_rx,
        raft_metrics_rx: None,
        council: None,
        snapshot_rx,
    };

    let grill = ProcessGrill::new();
    let port_allocator = PortAllocator::new(50000, 51000);
    let mut agent =
        BunAgent::with_cluster(grill, port_allocator, cmd_rx, shutdown.clone(), cluster);

    let handle = tokio::spawn(async move { agent.run().await });

    // Send a snapshot request
    let (resp_tx, resp_rx) = oneshot::channel();
    snapshot_tx
        .send(CollectSnapshotRequest { response: resp_tx })
        .await
        .unwrap();

    let snapshot = tokio::time::timeout(Duration::from_secs(2), resp_rx)
        .await
        .unwrap()
        .unwrap();

    // No instances deployed, so empty
    assert!(snapshot.instances.is_empty());
    assert!(snapshot.allocated_ports.is_empty());

    shutdown.cancel();
    let _ = handle.await;
}
