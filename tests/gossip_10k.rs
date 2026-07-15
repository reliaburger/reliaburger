//! 10,000-member deterministic gossip scale acceptance.
//!
//! A real 10,000-node deployment distributes one membership table per
//! machine. Allocating all 100 million membership records in one test process
//! measures a laptop pretending to be a datacentre, not the protocol. The
//! Criterion suites retain full multi-node convergence coverage through 1,000
//! nodes; this test exercises the per-node 10,000-member invariant through the
//! real message handler and dissemination queue.
//!
//! Run it with `make bench-10k`.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Instant;

use reliaburger::meat::NodeId;
use reliaburger::mustard::{
    GossipConfig, GossipMessage, GossipPayload, InMemoryNetwork, MAX_PIGGYBACK_UPDATES,
    MembershipUpdate, MustardNode, NodeState,
};

#[path = "../benches/support/gossip.rs"]
mod gossip_support;

fn address(index: usize) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], index as u16 + 1_024))
}

#[tokio::test]
async fn seeded_simulation_converges_without_a_sentinel_result() {
    let mut simulation = gossip_support::GossipSimulation::new(25).await;
    let rounds = simulation.converge(2_000).await.unwrap();
    assert!(rounds > 0);
}

#[tokio::test]
#[ignore = "10,000-member scale acceptance test; run with make bench-10k"]
async fn one_node_handles_10k_member_protocol_state() {
    let cluster_size = 10_000;
    let network = InMemoryNetwork::new();
    let transport = network.register(address(0)).await;
    let mut observer = MustardNode::new(
        NodeId::new("n0"),
        address(0),
        GossipConfig::default(),
        transport,
    );

    let updates: Vec<_> = (1..cluster_size)
        .map(|index| MembershipUpdate {
            node_id: NodeId::new(format!("n{index}")),
            address: address(index),
            state: NodeState::Alive,
            incarnation: 1,
            lamport: index as u64,
        })
        .collect();

    let ingest_started = Instant::now();
    for chunk in updates.chunks(MAX_PIGGYBACK_UPDATES) {
        observer
            .handle_message(
                address(1),
                GossipMessage::new(
                    NodeId::new("n1"),
                    1,
                    GossipPayload::Ping {
                        updates: chunk.to_vec(),
                    },
                ),
            )
            .await;
    }
    let ingest_elapsed = ingest_started.elapsed();

    assert_eq!(observer.membership.len(), cluster_size);
    let (probe, _) = observer
        .pick_probe_target()
        .expect("a 10,000-member table has a probe target");
    assert_ne!(probe, NodeId::new("n0"));

    let dissemination_started = Instant::now();
    let mut selected_nodes = HashSet::with_capacity(cluster_size - 1);
    let mut batches = 0;
    while selected_nodes.len() < cluster_size - 1 {
        let batch = observer.dissemination.select_updates();
        assert!(!batch.is_empty(), "updates expired before first broadcast");
        assert!(batch.len() <= MAX_PIGGYBACK_UPDATES);
        selected_nodes.extend(batch.into_iter().map(|update| update.node_id));
        batches += 1;
        assert!(
            batches <= cluster_size * MAX_PIGGYBACK_UPDATES,
            "dissemination did not expose every member in bounded batches"
        );
    }

    eprintln!(
        "one node ingested {cluster_size} members in {ingest_elapsed:.1?}; first dissemination of every update took {:.1?} across {batches} batches",
        dissemination_started.elapsed()
    );
}
