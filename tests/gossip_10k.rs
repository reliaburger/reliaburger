/// 10,000-node gossip convergence test.
///
/// Validates the whitepaper's claim that SWIM gossip scales to 10k nodes.
/// This test is ignored by default because it takes ~1 hour. Run it with:
///
///   cargo test --release gossip_10k -- --ignored --nocapture
///
/// Or via Make:
///
///   make bench-10k
use std::net::SocketAddr;
use std::time::Instant;

use reliaburger::mustard::{
    GossipConfig, GossipMessage, GossipPayload, InMemoryNetwork, MustardNode, MustardTransport,
};
use reliaburger::patty::NodeId;

fn node_addr(i: usize) -> SocketAddr {
    let port = (i % 60000) as u16 + 1024;
    let b3 = ((i / 60000) & 0xFF) as u8;
    SocketAddr::from(([10, 0, b3, 1], port))
}

fn fast_config() -> GossipConfig {
    GossipConfig {
        protocol_interval: std::time::Duration::from_millis(50),
        probe_timeout: std::time::Duration::from_millis(20),
        suspicion_timeout: std::time::Duration::from_millis(100),
        indirect_probe_count: 2,
    }
}

#[tokio::test]
#[ignore]
async fn gossip_10k_nodes_converge() {
    let cluster_size = 10_000;
    let net = InMemoryNetwork::new();
    let config = fast_config();

    eprintln!("Setting up {cluster_size} nodes...");
    let setup_start = Instant::now();

    let mut nodes = Vec::with_capacity(cluster_size);
    let mut addresses = Vec::with_capacity(cluster_size);

    for i in 0..cluster_size {
        let a = node_addr(i);
        addresses.push(a);
        let t = net.register(a).await;
        let node = MustardNode::new(NodeId::new(format!("n{i}")), a, config.clone(), t);
        nodes.push(node);
    }

    // Ring topology
    for i in 0..nodes.len() {
        let next = (i + 1) % nodes.len();
        let id = NodeId::new(format!("n{next}"));
        let a = addresses[next];
        nodes[i].add_seed(id, a);
    }

    eprintln!(
        "Setup took {:.1?}. Starting gossip rounds...",
        setup_start.elapsed()
    );
    let gossip_start = Instant::now();

    for round in 1..5000 {
        // Each node sends a PING
        for node in &mut nodes {
            if let Some((_id, target_addr)) = node.pick_probe_target() {
                let updates = node.dissemination.select_updates();
                let ping = GossipMessage::new(
                    node.node_id.clone(),
                    node.incarnation,
                    GossipPayload::Ping { updates },
                );
                let _ = node.transport.send(target_addr, &ping).await;
            }
        }

        // Drain messages twice (PINGs then ACKs)
        for _ in 0..2 {
            for node in &mut nodes {
                while let Some((from, msg)) = node.transport.try_recv() {
                    node.handle_message(from, msg).await;
                }
            }
        }

        // Check convergence every 10 rounds
        if round % 10 == 0 {
            let min_members = nodes
                .iter()
                .map(|n| n.membership.active_members().len())
                .min()
                .unwrap_or(0);

            if round % 50 == 0 {
                eprintln!(
                    "  round {round}: min membership = {min_members}/{cluster_size} ({:.1?} elapsed)",
                    gossip_start.elapsed()
                );
            }

            if min_members == cluster_size {
                let elapsed = gossip_start.elapsed();
                eprintln!("\nConverged in {round} rounds ({elapsed:.1?}).");
                eprintln!(
                    "At 500ms protocol interval, that's {:.1}s wall-clock time.",
                    round as f64 * 0.5
                );
                return;
            }
        }
    }

    let min_members = nodes
        .iter()
        .map(|n| n.membership.active_members().len())
        .min()
        .unwrap_or(0);
    panic!("Did not converge after 5000 rounds. Min membership: {min_members}/{cluster_size}");
}
