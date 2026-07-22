//! Council membership growth over the REAL TCP Raft network, in isolation
//! (no gossip, no reconciler). Mirrors the in-memory `single_node_grows_to_three`
//! but over `TcpRaftNetworkFactory` + `serve_raft_rpc`, so it guards the
//! transport/openraft membership path independently of the cluster glue.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{TcpRaftNetworkFactory, serve_raft_rpc};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::CrlHandle;
use reliaburger::sesame::types::{CaRole, CrlEntry, SerialNumber};

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn info_at(id: u64, addr: SocketAddr) -> CouncilNodeInfo {
    CouncilNodeInfo {
        addr,
        name: format!("n{id}"),
    }
}

/// TLS material shared by a set of test nodes: one Node CA, one identity per
/// node, and a shared CRL handle.
struct TlsSetup {
    hierarchy: CaHierarchy,
}

impl TlsSetup {
    fn new() -> Self {
        Self {
            hierarchy: ca::generate_ca_hierarchy("council-tcp-test", b"ikm").unwrap(),
        }
    }

    fn identity(&self, node_id: &str, serial: u64) -> NodeIdentity {
        let (cert_der, key_der, serial) = ca::issue_node_cert(
            node_id,
            SerialNumber(serial),
            &self.hierarchy.node.signing_keypair,
            &self.hierarchy.node.certificate_params,
        )
        .unwrap();
        let now = SystemTime::now();
        NodeIdentity {
            node_id: node_id.to_string(),
            certificate_der: cert_der,
            private_key_der: key_der,
            serial,
            ca_generation: 0,
            node_ca_der: self.hierarchy.node.ca.certificate_der.clone(),
            root_ca_der: self.hierarchy.root.ca.certificate_der.clone(),
            not_before: now,
            not_after: now + Duration::from_secs(365 * 24 * 3600),
        }
    }
}

fn revoked_crl(serial: u64) -> CrlHandle {
    CrlHandle::new(reliaburger::sesame::types::Crl {
        entries: vec![CrlEntry {
            serial: SerialNumber(serial),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "test".to_string(),
        }],
        version: 1,
        updated_at: SystemTime::now(),
    })
}

/// A test node paired with the address its Raft listener actually bound.
type Node = (Arc<CouncilNode>, SocketAddr);

/// A node with optional mTLS, on an ephemeral port (avoids TIME_WAIT reuse
/// collisions between runs). When `tls` is `Some((identity, crl))`, the Raft
/// listener requires client certs and the factory dials peers over mTLS.
async fn make_node_tls(
    id: u64,
    shutdown: &CancellationToken,
    tls: Option<(&NodeIdentity, CrlHandle)>,
) -> Node {
    let (factory, acceptor) = match tls {
        Some((identity, crl)) => {
            let server =
                reliaburger::sesame::mtls::build_mtls_server_config(identity, crl.clone()).unwrap();
            let client =
                reliaburger::sesame::mtls::build_mtls_client_config(identity, crl).unwrap();
            (
                TcpRaftNetworkFactory::new_tls(id, tokio_rustls::TlsConnector::from(client)),
                Some(tokio_rustls::TlsAcceptor::from(server)),
            )
        }
        None => (TcpRaftNetworkFactory::new(id), None),
    };
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

    let listener = tokio::net::TcpListener::bind(addr(0)).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let raft = council.raft().clone();
    let sd = shutdown.clone();
    tokio::spawn(async move {
        serve_raft_rpc(listener, raft, sd, acceptor, 0).await;
    });
    (council, bound)
}

async fn make_node(id: u64, shutdown: &CancellationToken) -> Node {
    make_node_tls(id, shutdown, None).await
}

/// The voter set a node has *applied* (its committed membership view).
fn applied_voters(node: &CouncilNode) -> BTreeSet<u64> {
    node.metrics()
        .borrow()
        .membership_config
        .membership()
        .voter_ids()
        .collect()
}

/// Drive node 1 to leadership, then grow the council to `{1,2,3}` and wait up
/// to `attempts` × 100 ms for **a follower** to apply the three-voter set.
///
/// Convergence is checked on a follower, not the leader: the leader appends
/// the new membership to its own log optimistically, so its voter set shows
/// `{1,2,3}` even when replication to the followers never commits. Only a
/// follower reaching `{1,2,3}` proves the config actually replicated — the
/// signal that fails (correctly) when a peer's mTLS handshake is refused.
async fn grow_one_to_three(nodes: &[Node; 3], attempts: u32) -> bool {
    let n1 = &nodes[0].0;
    let mut members = BTreeMap::new();
    members.insert(1, info_at(1, nodes[0].1));
    n1.initialize(members).await.unwrap();

    for _ in 0..50 {
        if n1.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !n1.is_leader().await {
        return false;
    }

    // A learner add commits against the single-voter quorum (the leader), so
    // it succeeds even if the learner is unreachable. Bound it anyway.
    let five_s = Duration::from_secs(5);
    if tokio::time::timeout(five_s, n1.add_learner(2, info_at(2, nodes[1].1)))
        .await
        .is_err()
    {
        return false;
    }
    if tokio::time::timeout(five_s, n1.add_learner(3, info_at(3, nodes[2].1)))
        .await
        .is_err()
    {
        return false;
    }
    // change_membership needs the new-config quorum; with unreachable peers it
    // errors or times out. Either way, don't early-return — the follower-side
    // convergence check below is the real signal.
    let _ = tokio::time::timeout(five_s, n1.change_membership(BTreeSet::from([1, 2, 3]))).await;

    let expected: BTreeSet<u64> = BTreeSet::from([1, 2, 3]);
    for _ in 0..attempts {
        if applied_voters(&nodes[1].0) == expected && applied_voters(&nodes[2].0) == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn shutdown_all(shutdown: &CancellationToken, nodes: [Node; 3]) {
    shutdown.cancel();
    for (council, _) in nodes {
        council.shutdown().await.ok();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plaintext_raft_still_works_when_mtls_is_off() {
    let shutdown = CancellationToken::new();
    let nodes = [
        make_node(1, &shutdown).await,
        make_node(2, &shutdown).await,
        make_node(3, &shutdown).await,
    ];

    assert!(
        grow_one_to_three(&nodes, 100).await,
        "plaintext voters did not reach {{1,2,3}}"
    );

    shutdown_all(&shutdown, nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_council_grows_one_to_three_over_mtls() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let crl = CrlHandle::default();
    let (id1, id2, id3) = (
        tls.identity("n1", 10),
        tls.identity("n2", 11),
        tls.identity("n3", 12),
    );

    let nodes = [
        make_node_tls(1, &shutdown, Some((&id1, crl.clone()))).await,
        make_node_tls(2, &shutdown, Some((&id2, crl.clone()))).await,
        make_node_tls(3, &shutdown, Some((&id3, crl.clone()))).await,
    ];

    assert!(
        grow_one_to_three(&nodes, 100).await,
        "mTLS voters did not reach {{1,2,3}}"
    );

    shutdown_all(&shutdown, nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raft_rpc_refuses_a_peer_without_a_client_cert() {
    // Leader requires mTLS; the followers dial plaintext. Their handshakes
    // fail, so the council never grows past the leader.
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let id1 = tls.identity("n1", 10);

    let nodes = [
        make_node_tls(1, &shutdown, Some((&id1, CrlHandle::default()))).await,
        make_node(2, &shutdown).await,
        make_node(3, &shutdown).await,
    ];

    assert!(
        !grow_one_to_three(&nodes, 25).await,
        "plaintext peers must not join an mTLS Raft listener"
    );

    shutdown_all(&shutdown, nodes).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raft_rpc_refuses_a_revoked_node_cert() {
    // All three speak mTLS, but the leader's CRL revokes node 2's serial (11).
    // Its client cert is rejected at handshake, so the council can't form.
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let (id1, id2, id3) = (
        tls.identity("n1", 10),
        tls.identity("n2", 11),
        tls.identity("n3", 12),
    );

    let nodes = [
        make_node_tls(1, &shutdown, Some((&id1, revoked_crl(11)))).await,
        make_node_tls(2, &shutdown, Some((&id2, CrlHandle::default()))).await,
        make_node_tls(3, &shutdown, Some((&id3, CrlHandle::default()))).await,
    ];

    assert!(
        !grow_one_to_three(&nodes, 25).await,
        "a revoked node cert must not be admitted to the council"
    );

    shutdown_all(&shutdown, nodes).await;
}
