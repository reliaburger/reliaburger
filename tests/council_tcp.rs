//! Council membership growth over the REAL TCP Raft network, in isolation
//! (no gossip, no reconciler). Mirrors the in-memory `single_node_grows_to_three`
//! but over `TcpRaftNetworkFactory` + `serve_raft_rpc`, so it guards the
//! transport/openraft membership path independently of the cluster glue.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{TcpRaftNetworkFactory, serve_raft_rpc};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn info(id: u64, port: u16) -> CouncilNodeInfo {
    CouncilNodeInfo {
        addr: addr(port),
        name: format!("n{id}"),
    }
}

async fn make_node(id: u64, port: u16, shutdown: &CancellationToken) -> Arc<CouncilNode> {
    let factory = TcpRaftNetworkFactory::new(id);
    let council = CouncilNode::new(
        id,
        CouncilConfig::default(),
        factory,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    let council = Arc::new(council);

    let listener = tokio::net::TcpListener::bind(addr(port)).await.unwrap();
    let raft = council.raft().clone();
    let sd = shutdown.clone();
    tokio::spawn(async move {
        serve_raft_rpc(listener, raft, sd).await;
    });
    council
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_council_grows_one_to_three() {
    let shutdown = CancellationToken::new();

    let n1 = make_node(1, 18001, &shutdown).await;
    let n2 = make_node(2, 18002, &shutdown).await;
    let n3 = make_node(3, 18003, &shutdown).await;

    // Bootstrap node 1 as a single-member cluster.
    let mut members = BTreeMap::new();
    members.insert(1, info(1, 18001));
    n1.initialize(members).await.unwrap();

    for _ in 0..50 {
        if n1.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(n1.is_leader().await, "node 1 should be leader");

    // Each learner is pointed at its OWN raft address — the bug we fixed was
    // pointing them all at one shared port.
    n1.add_learner(2, info(2, 18002))
        .await
        .expect("add_learner 2");
    n1.add_learner(3, info(3, 18003))
        .await
        .expect("add_learner 3");
    n1.change_membership(BTreeSet::from([1, 2, 3]))
        .await
        .expect("change_membership");

    let expected: BTreeSet<u64> = BTreeSet::from([1, 2, 3]);
    let mut converged = false;
    for _ in 0..100 {
        let voters: BTreeSet<u64> = n1
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect();
        if voters == expected {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(converged, "voters did not reach {{1,2,3}}");

    shutdown.cancel();
    n1.shutdown().await.ok();
    n2.shutdown().await.ok();
    n3.shutdown().await.ok();
}
