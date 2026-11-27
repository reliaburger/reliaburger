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
    MustardTransport, NodeState,
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
                        floors: vec![],
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

/// How many protocol rounds a healthy node's refutation takes to reach
/// every live member while each of them still has a correlated failure's
/// `Dead` rumours to send (#2441).
///
/// `cluster_size` nodes all know each other; `dead` of them died together,
/// so every survivor holds them `Dead` with a full broadcast budget queued.
/// Node `h` was suspected at incarnation 1 and refutes. Each round, every
/// survivor sends one PING to a peer chosen from `seed`, and every PING is
/// answered with an ACK, each carrying what the sender's queue selects.
async fn rounds_for_a_refutation_behind_a_failure_backlog(
    cluster_size: usize,
    dead: usize,
    seed: u64,
) -> Option<usize> {
    let network = InMemoryNetwork::new();
    let names: Vec<NodeId> = (0..cluster_size)
        .map(|index| NodeId::new(format!("n{index}")))
        .collect();
    let survivors = cluster_size - dead;
    let mut nodes = Vec::with_capacity(survivors);
    for (index, name) in names.iter().enumerate().take(survivors) {
        let transport = network.register(address(index)).await;
        let mut node = MustardNode::new(
            name.clone(),
            address(index),
            GossipConfig::default(),
            transport,
        );
        let now = Instant::now();
        for (peer, peer_name) in names.iter().enumerate() {
            if peer != index {
                node.membership.add_node(
                    peer_name.clone(),
                    address(peer),
                    1,
                    Default::default(),
                    now,
                );
            }
        }
        for (peer, peer_name) in names.iter().enumerate().skip(survivors) {
            node.membership.declare_dead(peer_name);
            node.dissemination.enqueue(
                MembershipUpdate {
                    node_id: peer_name.clone(),
                    address: address(peer),
                    state: NodeState::Dead,
                    incarnation: 1,
                },
                cluster_size,
            );
        }
        nodes.push(node);
    }
    // Node 0 is `h`. Everyone holds it Suspect; it hears the claim and
    // refutes to incarnation 2.
    let h = names[0].clone();
    for node in nodes.iter_mut().skip(1) {
        node.membership.suspect(&h);
    }
    let claim = GossipMessage::new(
        names[1].clone(),
        1,
        GossipPayload::Ping {
            updates: vec![MembershipUpdate {
                node_id: h.clone(),
                address: address(0),
                state: NodeState::Suspect,
                incarnation: 1,
            }],
        },
    );
    nodes[0].handle_message(address(1), claim).await;
    assert_eq!(nodes[0].incarnation, 2);
    while nodes[1].transport.try_recv().is_some() {}

    let mut state = seed;
    for round in 1..=200 {
        for (index, node) in nodes.iter_mut().enumerate() {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let mut target = (state >> 33) as usize % (survivors - 1);
            if target >= index {
                target += 1;
            }
            let ping = GossipMessage::new(
                node.node_id.clone(),
                node.incarnation,
                GossipPayload::Ping {
                    updates: node.dissemination.select_updates(),
                },
            );
            node.transport.send(address(target), &ping).await.unwrap();
        }
        // PINGs, then the ACKs they draw.
        for _ in 0..2 {
            for node in nodes.iter_mut() {
                while let Some((from, message)) = node.transport.try_recv() {
                    node.handle_message(from, message).await;
                }
            }
        }
        let everyone_heard = nodes.iter().skip(1).all(|node| {
            node.membership
                .get(&h)
                .is_some_and(|member| member.state == NodeState::Alive && member.incarnation == 2)
        });
        if everyone_heard {
            return Some(round);
        }
    }
    None
}

/// #2441: a refutation relayed by a peer used to wait behind every queued
/// `Dead` there, so after a correlated failure it reached the cluster only
/// as fast as the refuting node could ping, while the suspicion timers ran.
/// It has to beat the 5 s suspicion timeout: ten 500 ms rounds.
#[tokio::test]
async fn a_refutation_outruns_a_correlated_failures_backlog() {
    for seed in [1, 2, 3] {
        let rounds = rounds_for_a_refutation_behind_a_failure_backlog(300, 30, seed).await;
        eprintln!("seed {seed}: refutation reached every survivor in {rounds:?} rounds");
        assert!(
            rounds.is_some_and(|rounds| rounds <= 10),
            "seed {seed}: the refutation took {rounds:?} rounds, past the suspicion timeout"
        );
    }
}
