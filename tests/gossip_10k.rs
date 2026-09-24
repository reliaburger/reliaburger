//! 10,000-member deterministic gossip scale acceptance.
//!
//! A real 10,000-node deployment distributes one membership table per
//! machine. Allocating all 100 million membership records in one test process
//! measures a laptop pretending to be a datacentre, not the protocol. The
//! Criterion suites retain full multi-node convergence coverage through 1,000
//! nodes; this test exercises the per-node 10,000-member invariant through the
//! real message handler and dissemination queue.
//!
//! It runs in about a second in a debug build, so it is part of `make test`.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Instant;

use reliaburger::meat::NodeId;
use reliaburger::mustard::{
    GossipConfig, GossipMessage, GossipPayload, InMemoryNetwork, InMemoryTransport,
    MAX_PIGGYBACK_UPDATES, MAX_SYNC_DATAGRAMS, MAX_SYNC_ENTRIES, MembershipUpdate, MustardNode,
    NodeState,
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

/// One node holding a 10,000-member table, learnt through the real message
/// handler from datagrams sent by `n1` at `address(1)`.
async fn observer_with_members(
    network: &InMemoryNetwork,
    cluster_size: usize,
) -> MustardNode<InMemoryTransport> {
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
        })
        .collect();

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
    observer
}

#[tokio::test]
async fn one_node_handles_10k_member_protocol_state() {
    let cluster_size = 10_000;
    let network = InMemoryNetwork::new();

    let ingest_started = Instant::now();
    let mut observer = observer_with_members(&network, cluster_size).await;
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

/// Anti-entropy must not undo the per-node bound: however large the table,
/// one push-pull reply is at most `MAX_SYNC_DATAGRAMS` datagrams of at most
/// `MAX_PIGGYBACK_UPDATES` entries, and successive exchanges rotate through
/// the table so every member still goes out.
#[tokio::test]
async fn push_pull_with_10k_members_is_bounded_and_sweeps_the_table() {
    let cluster_size = 10_000;
    let network = InMemoryNetwork::new();
    let requester = network.register(address(1)).await;
    let mut observer = observer_with_members(&network, cluster_size).await;
    // Discard the ACKs to the ingest PINGs.
    while requester.try_recv().is_some() {}

    let most_exchanges = cluster_size.div_ceil(MAX_SYNC_ENTRIES);
    let mut seen = HashSet::with_capacity(cluster_size);
    let mut exchanges = 0;
    while seen.len() < cluster_size {
        observer
            .handle_message(
                address(1),
                GossipMessage::new(
                    NodeId::new("n1"),
                    1,
                    GossipPayload::Sync {
                        entries: vec![],
                        wants_reply: true,
                    },
                ),
            )
            .await;
        let mut datagrams = 0;
        while let Some((_, reply)) = requester.try_recv() {
            assert!(reply.payload.updates().len() <= MAX_PIGGYBACK_UPDATES);
            seen.extend(
                reply
                    .payload
                    .updates()
                    .iter()
                    .map(|update| update.node_id.clone()),
            );
            datagrams += 1;
        }
        assert!(
            (1..=MAX_SYNC_DATAGRAMS).contains(&datagrams),
            "one exchange sent {datagrams} datagrams"
        );
        exchanges += 1;
        assert!(
            exchanges <= most_exchanges,
            "{exchanges} exchanges covered only {} of {cluster_size} members",
            seen.len()
        );
    }
    eprintln!("{exchanges} bounded push-pull replies swept all {cluster_size} members");
}
