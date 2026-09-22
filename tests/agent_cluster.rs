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

/// A previous successful write is not evidence about the current committed state.
#[tokio::test]
async fn scheduler_repairs_a_catalogue_replaced_after_its_last_publication() {
    use reliaburger::cluster::orchestrate::spawn_leader_scheduler;
    use reliaburger::council::types::CouncilResponse;
    use reliaburger::onion::service_id::ServiceId;

    let router = InMemoryRaftRouter::new();
    let council = std::sync::Arc::new(
        CouncilNode::new(
            1,
            fast_council_config(),
            InMemoryRaftNetworkFactory::new(1, router.clone()),
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap(),
    );
    router.register(1, council.raft().clone()).await;
    council
        .initialize(BTreeMap::from([(1, node_info(1, 9444))]))
        .await
        .unwrap();
    let mut metrics = council.metrics();
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics.borrow().current_leader != Some(1) {
            metrics.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let response = council
        .write(RaftRequest::AppSpec {
            app_id: reliaburger::meat::AppId::new("api", "default"),
            spec: Box::new(toml::from_str("image = 'example:v1'\nport = 8080").unwrap()),
        })
        .await
        .unwrap();
    assert!(matches!(response, CouncilResponse::Applied { .. }));
    let shutdown = CancellationToken::new();
    let (_members, membership_rx) = watch::channel(Vec::new());
    let (_reports, reports_rx) = watch::channel(Default::default());
    let _admission = spawn_leader_scheduler(
        council.clone(),
        membership_rx,
        reports_rx,
        false,
        Default::default(),
        shutdown.clone(),
    );
    let service = ServiceId::new("default", "api");
    let result = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if council
                .desired_state()
                .await
                .endpoint_catalog
                .resolve(&service)
                .is_some()
            {
                break;
            }
            metrics.changed().await.unwrap();
        }
        let observed = council.desired_state().await;
        let expected_generation = observed.endpoint_withdrawals.generation;
        let original = observed.endpoint_catalog;
        let response = council
            .write(RaftRequest::PublishEndpoints {
                expected_generation,
                catalog: Box::default(),
            })
            .await
            .unwrap();
        assert!(matches!(response, CouncilResponse::Applied { .. }));
        let stale = council
            .write(RaftRequest::PublishEndpoints {
                expected_generation,
                catalog: Box::new(original.clone()),
            })
            .await
            .unwrap();
        assert!(matches!(stale, CouncilResponse::Refused { .. }));

        loop {
            if council.desired_state().await.endpoint_catalog == original {
                break;
            }
            metrics.changed().await.unwrap();
        }
        // Unchanged committed state must not produce a Raft write every tick.
        let settled = metrics.borrow().last_applied;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(metrics.borrow().last_applied, settled);
    })
    .await;
    shutdown.cancel();
    council.shutdown().await.unwrap();
    assert!(
        result.is_ok(),
        "scheduler trusted an obsolete publication cache"
    );
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
            first_seen: std::time::Instant::now(),
            resources: None,
        },
        MembershipSnapshot {
            node_id: NodeId::new("node-2"),
            address: addr(9444),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels: BTreeMap::new(),
            first_seen: std::time::Instant::now(),
            resources: None,
        },
    ];
    let (_membership_tx, membership_rx) = watch::channel(snapshots);

    let (_snapshot_tx, snapshot_rx) = mpsc::channel(16);
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    let cluster = ClusterHandle {
        local_node_id: NodeId::new("node-1"),
        membership_rx,
        raft_metrics_rx: None,
        council: None,
        snapshot_rx,
        wrapping_ikm: None,
        partition_blocklists: Default::default(),
        crl_handle: Default::default(),
    };

    let grill = ProcessGrill::new();
    let port_allocator = PortAllocator::new(50000, 51000);
    let volumes = tempfile::tempdir().unwrap();
    let mut agent = BunAgent::with_cluster(
        grill,
        port_allocator,
        cmd_rx,
        shutdown.clone(),
        cluster,
        "default".to_string(),
    );
    // Co-located test agents must not touch the shared host firewall.
    agent.set_perimeter_enabled(false);
    agent.set_volumes_dir(volumes.path().to_path_buf());

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
        None,
    )
    .await
    .unwrap();
    router.register(1, council.raft().clone()).await;

    let mut members = BTreeMap::new();
    members.insert(1, node_info(1, 9444));
    council.initialize(members).await.unwrap();

    let mut metrics = council.metrics();
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics.borrow().current_leader != Some(1) {
            metrics.changed().await.unwrap();
        }
    })
    .await
    .expect("single-node council did not elect itself");

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
        local_node_id: NodeId::new("node-1"),
        membership_rx,
        raft_metrics_rx: Some(raft_metrics_rx),
        council: Some(council.clone()),
        snapshot_rx,
        wrapping_ikm: None,
        partition_blocklists: Default::default(),
        crl_handle: Default::default(),
    };

    let grill = ProcessGrill::new();
    let port_allocator = PortAllocator::new(50000, 51000);
    let volumes = tempfile::tempdir().unwrap();
    let mut agent = BunAgent::with_cluster(
        grill,
        port_allocator,
        cmd_rx,
        shutdown.clone(),
        cluster,
        "default".to_string(),
    );
    // Co-located test agents must not touch the shared host firewall.
    agent.set_perimeter_enabled(false);
    agent.set_volumes_dir(volumes.path().to_path_buf());

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
        has_buildah: false,
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
    let (_, _, received) = result.unwrap().unwrap();
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

    let (membership_tx, mut membership_rx) = watch::channel(vec![]);
    node1.set_membership_watch(membership_tx);

    let node_shutdown = shutdown.clone();
    let handle = tokio::spawn(async move { node1.run(node_shutdown).await });

    tokio::time::timeout(Duration::from_secs(2), async {
        while !membership_rx
            .borrow()
            .iter()
            .any(|member| member.node_id == NodeId::new("n1"))
        {
            membership_rx.changed().await.unwrap();
        }
    })
    .await
    .expect("membership watch never published the local node");

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
        local_node_id: NodeId::new("node-1"),
        membership_rx,
        raft_metrics_rx: None,
        council: None,
        snapshot_rx,
        wrapping_ikm: None,
        partition_blocklists: Default::default(),
        crl_handle: Default::default(),
    };

    let grill = ProcessGrill::new();
    let port_allocator = PortAllocator::new(50000, 51000);
    let volumes = tempfile::tempdir().unwrap();
    let mut agent = BunAgent::with_cluster(
        grill,
        port_allocator,
        cmd_rx,
        shutdown.clone(),
        cluster,
        "default".to_string(),
    );
    // Co-located test agents must not touch the shared host firewall.
    agent.set_perimeter_enabled(false);
    agent.set_volumes_dir(volumes.path().to_path_buf());

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

/// A delayed catalogue must not restore this worker's already retired endpoint.
#[tokio::test]
async fn worker_without_council_metrics_excludes_its_own_stale_endpoints() {
    use reliaburger::cluster::orchestrate::IngressAssignment;
    use reliaburger::onion::catalog::{CatalogBackend, EndpointCatalog};
    use reliaburger::onion::service_id::ServiceId;

    let shutdown = CancellationToken::new();
    let (_membership_tx, membership_rx) = watch::channel(Vec::new());
    let (_snapshot_tx, snapshot_rx) = mpsc::channel(1);
    let (commands, command_rx) = mpsc::channel(8);
    let cluster = ClusterHandle {
        local_node_id: NodeId::new("worker"),
        membership_rx,
        raft_metrics_rx: None,
        council: None,
        snapshot_rx,
        wrapping_ikm: None,
        partition_blocklists: Default::default(),
        crl_handle: Default::default(),
    };
    let volumes = tempfile::tempdir().unwrap();
    let mut agent = BunAgent::with_cluster(
        ProcessGrill::new(),
        PortAllocator::new(50000, 51000),
        command_rx,
        shutdown.clone(),
        cluster,
        "default".into(),
    );
    agent.set_perimeter_enabled(false);
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let dns = agent.service_map_watch();
    let routes = agent.routing_table_handle();
    let actor = tokio::spawn(async move { agent.run().await });

    let shared = ServiceId::new("default", "shared");
    let retired = ServiceId::new("default", "retired");
    let local = CatalogBackend {
        execution: None,
        node_id: "worker".into(),
        node_ip: "192.0.2.1".parse().unwrap(),
        host_port: 30001,
        healthy: true,
    };
    let remote = CatalogBackend {
        execution: None,
        node_id: "remote".into(),
        node_ip: "192.0.2.2".parse().unwrap(),
        host_port: 30002,
        healthy: true,
    };
    let catalog = EndpointCatalog::rebuild([
        (shared.clone(), 8080, vec![local.clone(), remote]),
        (retired.clone(), 8080, vec![local]),
    ])
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        commands
            .send(AgentCommand::SyncClusterCatalog {
                catalog: Box::new(catalog),
                ingress: vec![IngressAssignment {
                    namespace: "default".into(),
                    name: "shared".into(),
                    config: toml::from_str("host = \"shared.test\"\ntls = \"disabled\"").unwrap(),
                }],
            })
            .await
            .unwrap();
        let (response, reply) = oneshot::channel();
        commands
            .send(AgentCommand::ResolveAll { response })
            .await
            .unwrap();
        reply.await.unwrap()
    })
    .await;
    // Retire the actor before asserting so a regression cannot leak its tasks.
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), actor)
        .await
        .unwrap()
        .unwrap();
    let resolved = result.expect("catalogue query timed out");
    let shared_response = resolved
        .iter()
        .find(|entry| entry.app_name == "shared")
        .unwrap();
    assert_eq!(
        shared_response.total_backends, 1,
        "worker restored its own stale endpoint"
    );
    assert_eq!(shared_response.backends[0].host_port, 30002);
    let retired_response = resolved
        .iter()
        .find(|entry| entry.app_name == "retired")
        .unwrap();
    assert_eq!(retired_response.total_backends, 0);
    let snapshot = dns.borrow();
    assert!(snapshot.resolve(&retired).unwrap().backends.is_empty());
    assert_eq!(snapshot.resolve(&shared).unwrap().backends.len(), 1);
    let table = routes.read().await;
    let route = table.lookup("shared.test", "/").unwrap();
    assert_eq!(route.backends.len(), 1);
    assert_eq!(route.select_backend().unwrap().addr.port(), 30002);
}
