//! Multi-node gossip integration test over REAL UDP transports.
//!
//! Unlike the in-memory mustard tests, this exercises the binary's wiring
//! path: `cluster::runtime::start` binds real UDP sockets, a node joins by
//! address (seeds), and membership converges across all three nodes. This
//! is the proof that the cluster runtime forms gossip membership for real,
//! not just in a harness.

use std::net::SocketAddr;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use reliaburger::cluster::runtime::{self, ClusterParams};
use reliaburger::mustard::state::NodeState;

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Count distinct alive node names in a membership snapshot.
fn alive_names(snap: &[reliaburger::mustard::membership::MembershipSnapshot]) -> Vec<String> {
    let mut names: Vec<String> = snap
        .iter()
        .filter(|m| m.state == NodeState::Alive)
        .map(|m| m.node_id.0.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[tokio::test]
async fn three_nodes_join_by_address_and_converge() {
    let shutdown = CancellationToken::new();

    // Pick three gossip ports. node-1 is the bootstrap node (no seeds);
    // node-2 and node-3 join by node-1's address only.
    let (p1, p2, p3) = (17441, 17442, 17443);

    let (h1, _rt1) = runtime::start(
        ClusterParams {
            node_name: "node-1".into(),
            gossip_addr: local(p1),
            seeds: vec![],
        },
        shutdown.clone(),
    )
    .await
    .unwrap();

    let (h2, _rt2) = runtime::start(
        ClusterParams {
            node_name: "node-2".into(),
            gossip_addr: local(p2),
            seeds: vec![local(p1)],
        },
        shutdown.clone(),
    )
    .await
    .unwrap();

    let (h3, _rt3) = runtime::start(
        ClusterParams {
            node_name: "node-3".into(),
            gossip_addr: local(p3),
            seeds: vec![local(p1)],
        },
        shutdown.clone(),
    )
    .await
    .unwrap();

    // Poll until every node sees all three by name, or time out.
    let expected = vec!["node-1".to_string(), "node-2".into(), "node-3".into()];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let converged = loop {
        let all = [&h1, &h2, &h3]
            .iter()
            .all(|h| alive_names(&h.membership_rx.borrow()) == expected);
        if all {
            break true;
        }
        if tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    if !converged {
        let views: Vec<_> = [&h1, &h2, &h3]
            .iter()
            .map(|h| alive_names(&h.membership_rx.borrow()))
            .collect();
        panic!("gossip did not converge to 3 nodes; views: {views:?}");
    }

    // No phantom members: exactly three on each node.
    for h in [&h1, &h2, &h3] {
        assert_eq!(alive_names(&h.membership_rx.borrow()).len(), 3);
    }

    shutdown.cancel();
}
