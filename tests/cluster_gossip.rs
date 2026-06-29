//! Multi-node cluster integration tests over REAL UDP + TCP transports.
//!
//! Unlike the in-memory mustard/council tests, these exercise the binary's
//! wiring path: `cluster::runtime::start` binds real sockets, nodes join by
//! address, gossip converges, a Raft leader is elected, and the council grows
//! from gossip membership. This is the proof that the cluster runtime forms a
//! real cluster, not just in a harness.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::ClusterHandle;
use reliaburger::cluster::identity::raft_id_from_name;
use reliaburger::cluster::runtime::{self, ClusterParams, ClusterRuntime};
use reliaburger::mustard::state::NodeState;

fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Start a node. Gossip and raft ports are adjacent (offset 1), so each node
/// on the shared loopback IP gets a distinct raft address.
async fn start_node(
    name: &str,
    gossip_port: u16,
    seeds: Vec<SocketAddr>,
    shutdown: &CancellationToken,
) -> (ClusterHandle, ClusterRuntime) {
    runtime::start(
        ClusterParams {
            node_name: name.into(),
            gossip_addr: local(gossip_port),
            raft_port: gossip_port + 1,
            seeds,
            wrapping_ikm: None,
        },
        shutdown.clone(),
    )
    .await
    .unwrap()
}

/// Distinct alive node names in a membership snapshot.
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

/// Current Raft voter id set as seen by this node.
fn voter_ids(h: &ClusterHandle) -> BTreeSet<u64> {
    let Some(rx) = &h.raft_metrics_rx else {
        return BTreeSet::new();
    };
    rx.borrow()
        .membership_config
        .membership()
        .voter_ids()
        .collect()
}

fn thinks_it_is_leader(h: &ClusterHandle) -> bool {
    let Some(rx) = &h.raft_metrics_rx else {
        return false;
    };
    let m = rx.borrow();
    m.current_leader == Some(m.id)
}

/// Poll `cond` every 200ms until it returns true or the timeout elapses.
async fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_nodes_join_by_address_and_converge() {
    let shutdown = CancellationToken::new();

    // node-1 is the bootstrap node (no seeds); node-2/3 join by its address.
    let h1 = start_node("node-1", 17441, vec![], &shutdown).await;
    let h2 = start_node("node-2", 17443, vec![local(17441)], &shutdown).await;
    let h3 = start_node("node-3", 17445, vec![local(17441)], &shutdown).await;
    let handles = [&h1.0, &h2.0, &h3.0];

    let expected = vec!["node-1".to_string(), "node-2".into(), "node-3".into()];
    let converged = wait_until(Duration::from_secs(15), || {
        handles
            .iter()
            .all(|h| alive_names(&h.membership_rx.borrow()) == expected)
    })
    .await;

    if !converged {
        let views: Vec<_> = handles
            .iter()
            .map(|h| alive_names(&h.membership_rx.borrow()))
            .collect();
        panic!("gossip did not converge to 3 nodes; views: {views:?}");
    }
    for h in handles {
        assert_eq!(alive_names(&h.membership_rx.borrow()).len(), 3);
    }

    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_node_council_elects_leader_and_grows() {
    let shutdown = CancellationToken::new();

    let h1 = start_node("c1", 17541, vec![], &shutdown).await;
    let h2 = start_node("c2", 17543, vec![local(17541)], &shutdown).await;
    let h3 = start_node("c3", 17545, vec![local(17541)], &shutdown).await;
    let handles = [&h1.0, &h2.0, &h3.0];

    let expected: BTreeSet<u64> = ["c1", "c2", "c3"]
        .iter()
        .map(|n| raft_id_from_name(n))
        .collect();

    // Gossip converges, a leader emerges, and the council grows to all three.
    let grown = wait_until(Duration::from_secs(25), || {
        handles
            .iter()
            .any(|h| voter_ids(h) == expected && thinks_it_is_leader(h))
    })
    .await;

    if !grown {
        let sets: Vec<_> = handles.iter().map(|h| voter_ids(h)).collect();
        panic!("council did not grow to 3 voters; voter sets: {sets:?}");
    }

    // Exactly one leader, and every node agrees on the 3-voter set.
    assert_eq!(handles.iter().filter(|h| thinks_it_is_leader(h)).count(), 1);
    for h in handles {
        assert_eq!(voter_ids(h), expected);
    }

    shutdown.cancel();
    for h in handles {
        if let Some(c) = &h.council {
            c.shutdown().await.ok();
        }
    }
}
