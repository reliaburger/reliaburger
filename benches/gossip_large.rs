/// Large-scale gossip convergence benchmarks.
///
/// Separate from the main gossip benchmarks because these take minutes
/// to run. Tests SWIM convergence at 500 and 1000 nodes.
///
/// Run with: cargo bench --bench gossip_large
///
/// For 10000-node convergence (takes ~1 hour), use the ignored test instead:
///   cargo test --release gossip_10k -- --ignored --nocapture
use std::net::SocketAddr;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reliaburger::mustard::{
    GossipConfig, GossipMessage, GossipPayload, InMemoryNetwork, MustardNode, MustardTransport,
};
use reliaburger::patty::NodeId;

/// Generate a unique SocketAddr for node index i.
/// Encodes index across IP octets and port to support up to ~16M nodes.
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
        cleanup_timeout: std::time::Duration::from_secs(60),
    }
}

/// Create N nodes in a ring, run gossip rounds until all nodes
/// know about all others, return the number of rounds needed.
async fn rounds_to_converge(cluster_size: usize) -> usize {
    let net = InMemoryNetwork::new();
    let config = fast_config();

    let mut nodes = Vec::new();
    let mut addresses = Vec::new();

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

        // Check convergence (every 10th round for large clusters)
        if round % 10 == 0 || cluster_size <= 100 {
            let converged = nodes
                .iter()
                .all(|n| n.membership.active_members().len() == cluster_size);
            if converged {
                return round;
            }
        }
    }

    5000 // didn't converge
}

/// Benchmark convergence for large clusters (500, 1000 nodes).
fn bench_large_convergence(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("gossip_convergence_large");
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(120));
    for &size in &[500, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| rt.block_on(rounds_to_converge(size)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_large_convergence);
criterion_main!(benches);
