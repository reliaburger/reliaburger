/// Gossip protocol benchmarks.
///
/// Measures SWIM protocol performance at different scales:
/// - Single message send/recv throughput via InMemoryTransport
/// - Single gossip round (PING + process + ACK)
/// - Rounds to convergence for varying cluster sizes
use std::net::SocketAddr;
use std::time::Instant;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reliaburger::mustard::{
    DisseminationQueue, GossipConfig, GossipMessage, GossipPayload, InMemoryNetwork,
    InMemoryTransport, MembershipUpdate, MustardNode, MustardTransport, NodeState,
};
use reliaburger::patty::NodeId;

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn fast_config() -> GossipConfig {
    GossipConfig {
        protocol_interval: std::time::Duration::from_millis(50),
        probe_timeout: std::time::Duration::from_millis(20),
        suspicion_timeout: std::time::Duration::from_millis(100),
        indirect_probe_count: 2,
    }
}

/// Benchmark raw InMemoryTransport send + recv throughput.
fn bench_transport_throughput(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("transport_send_recv", |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                let net = InMemoryNetwork::new();
                let t1 = net.register(addr(1)).await;
                let t2 = net.register(addr(2)).await;

                use reliaburger::mustard::MustardTransport;
                let msg = GossipMessage::new(
                    NodeId::new("sender"),
                    1,
                    GossipPayload::Ping { updates: vec![] },
                );

                let start = Instant::now();
                for _ in 0..iters {
                    t1.send(addr(2), &msg).await.unwrap();
                    t2.recv().await.unwrap();
                }
                start.elapsed()
            })
        });
    });
}

/// Benchmark a single gossip round: one node sends PING, other
/// processes it and sends ACK, first node processes ACK.
fn bench_single_gossip_round(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("single_gossip_round", |b| {
        b.iter_custom(|iters| {
            rt.block_on(async {
                let net = InMemoryNetwork::new();
                let t1 = net.register(addr(1)).await;
                let t2 = net.register(addr(2)).await;

                let mut node1: MustardNode<InMemoryTransport> =
                    MustardNode::new(NodeId::new("n1"), addr(1), fast_config(), t1);
                let mut node2: MustardNode<InMemoryTransport> =
                    MustardNode::new(NodeId::new("n2"), addr(2), fast_config(), t2);

                node1.add_seed(NodeId::new("n2"), addr(2));
                node2.add_seed(NodeId::new("n1"), addr(1));

                let start = Instant::now();
                for _ in 0..iters {
                    // n1 sends PING
                    let updates = node1.dissemination.select_updates();
                    let ping = GossipMessage::new(
                        node1.node_id.clone(),
                        node1.incarnation,
                        GossipPayload::Ping { updates },
                    );
                    node1.transport.send(addr(2), &ping).await.unwrap();

                    // n2 processes PING (sends ACK internally)
                    let (from, msg) = node2.transport.try_recv().unwrap();
                    node2.handle_message(from, msg).await;

                    // n1 processes ACK
                    let (from, msg) = node1.transport.try_recv().unwrap();
                    node1.handle_message(from, msg).await;
                }
                start.elapsed()
            })
        });
    });
}

/// Helper: create N nodes in a ring, run gossip rounds until all nodes
/// know about all others, return the number of rounds needed.
async fn rounds_to_converge(cluster_size: usize) -> usize {
    let net = InMemoryNetwork::new();
    let config = fast_config();

    let mut nodes = Vec::new();
    let mut addresses = Vec::new();

    for i in 0..cluster_size {
        let a = addr(1000 + i as u16);
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

    for round in 1..500 {
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

        // Check convergence
        let converged = nodes
            .iter()
            .all(|n| n.membership.active_members().len() == cluster_size);
        if converged {
            return round;
        }
    }

    500 // didn't converge
}

/// Benchmark rounds-to-converge for different cluster sizes.
/// Validates SWIM's O(log N) convergence property.
fn bench_convergence(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("gossip_convergence");
    for &size in &[5, 10, 25, 50] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| rt.block_on(rounds_to_converge(size)));
        });
    }
    group.finish();
}

/// Measure dissemination queue throughput: enqueue + select cycles.
fn bench_dissemination_queue(c: &mut Criterion) {
    c.bench_function("dissemination_enqueue_select", |b| {
        b.iter(|| {
            let mut queue = DisseminationQueue::new();
            // Enqueue 20 updates
            for i in 0..20 {
                queue.enqueue(
                    MembershipUpdate {
                        node_id: NodeId::new(format!("n{i}")),
                        address: addr(9000 + i),
                        state: if i % 3 == 0 {
                            NodeState::Suspect
                        } else {
                            NodeState::Alive
                        },
                        incarnation: 1,
                        lamport: i as u64,
                    },
                    100,
                );
            }
            // Select until empty
            let mut total = 0;
            loop {
                let batch = queue.select_updates();
                if batch.is_empty() {
                    break;
                }
                total += batch.len();
            }
            total
        });
    });
}

criterion_group!(
    benches,
    bench_transport_throughput,
    bench_single_gossip_round,
    bench_convergence,
    bench_dissemination_queue,
);
criterion_main!(benches);
