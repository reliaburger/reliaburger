//! Council membership growth over the REAL TCP Raft network, in isolation
//! (no gossip, no reconciler). Mirrors the in-memory `single_node_grows_to_three`
//! but over `TcpRaftNetworkFactory` + `serve_raft_rpc`, so it guards the
//! transport/openraft membership path independently of the cluster glue.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio_util::sync::CancellationToken;

use reliaburger::council::fence::RecoveryFence;
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
            trust: reliaburger::sesame::trust::TrustSet::single(
                self.hierarchy.node.ca.certificate_der.clone(),
                self.hierarchy.root.ca.certificate_der.clone(),
            ),
            node_ca_der: self.hierarchy.node.ca.certificate_der.clone(),
            root_ca_der: self.hierarchy.root.ca.certificate_der.clone(),
            not_before: now,
            not_after: now + Duration::from_secs(365 * 24 * 3600),
        }
    }
}

fn revoked_crl(serial: u64) -> CrlHandle {
    CrlHandle::new(revoked(serial))
}

/// A CRL revoking one Node CA serial.
fn revoked(serial: u64) -> reliaburger::sesame::types::Crl {
    reliaburger::sesame::types::Crl {
        retired_nodes: Default::default(),
        entries: vec![CrlEntry {
            serial: SerialNumber(serial),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "test".to_string(),
            expires_at: None,
        }],
        version: 1,
        updated_at: SystemTime::now(),
    }
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
    let contact = council.leader_contact();
    let sd = shutdown.clone();
    tokio::spawn(async move {
        reliaburger::council::network::serve_raft_rpc_with_node_gate(
            listener,
            raft,
            contact,
            sd,
            acceptor,
            RecoveryFence::serving(0),
            reliaburger::smoker::node_fault::NodeTransportGate::new(),
            None,
            reliaburger::council::network::RaftBlocklist::default(),
        )
        .await;
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

/// The TCP server notes a leader's AppendEntries on the follower, which is
/// what ends a returning node's hold on its elections (#1524).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leaders_append_entries_over_tcp_reaches_the_followers_contact() {
    let shutdown = CancellationToken::new();
    let nodes = [
        make_node(1, &shutdown).await,
        make_node(2, &shutdown).await,
        make_node(3, &shutdown).await,
    ];
    assert!(nodes.iter().all(|(node, _)| !node.leader_contact().heard()));
    assert!(
        grow_one_to_three(&nodes, 100).await,
        "voters did not reach {{1,2,3}}"
    );
    assert!(
        nodes[1].0.leader_contact().heard() && nodes[2].0.leader_contact().heard(),
        "a follower's contact wasn't noted"
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

/// O6: the accept loop holds a semaphore permit for the lifetime of each
/// connection, so a permit that isn't released would wedge the listener after
/// `max_connections` peers — silently, and only under load.
///
/// Serve with a cap of one and drive three connections through in sequence.
/// Each sends a frame the server can't decode, which is the shortest path to
/// "connection handled, permit dropped". If the permit leaked, the second
/// connection would be accepted by the kernel but never served, and the read
/// below would block until the test's timeout.
#[tokio::test]
async fn raft_rpc_connection_permits_are_released() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let shutdown = CancellationToken::new();
    let council = CouncilNode::new(
        1,
        CouncilConfig::default(),
        TcpRaftNetworkFactory::new(1),
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();

    let listener = tokio::net::TcpListener::bind(addr(0)).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let raft = council.raft().clone();
    let serving = shutdown.clone();
    tokio::spawn(async move {
        reliaburger::council::network::serve_raft_rpc_with_limit(
            listener,
            raft,
            serving,
            None,
            RecoveryFence::serving(0),
            1,
        )
        .await;
    });

    for attempt in 0..3 {
        let mut stream = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(bound),
        )
        .await
        .unwrap_or_else(|_| panic!("connect {attempt} timed out — a permit was never released"))
        .unwrap();

        let body = b"not a raft rpc";
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(body).await.unwrap();

        // The server drops an undecodable frame, so we expect EOF. What
        // matters is that it happens promptly rather than never.
        let mut sink = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut sink))
            .await
            .unwrap_or_else(|_| {
                panic!("connection {attempt} was never served — a permit was never released")
            })
            .unwrap();
    }

    shutdown.cancel();
}

// ---------------------------------------------------------------------------
// Persistent connections (#928)
// ---------------------------------------------------------------------------

/// One stamped vote request, framed as the transport sends it.
fn vote_frame() -> Vec<u8> {
    let envelope = reliaburger::council::network::RaftRpcEnvelope {
        compatibility: reliaburger::compatibility::CURRENT,
        sender_recovery_epoch: 0,
        sender_recovery_id: None,
        rpc: reliaburger::council::network::RaftRpc::Vote(openraft::raft::VoteRequest::new(
            openraft::Vote::new(1, 2),
            None,
        )),
    };
    let body = serde_json::to_vec(&envelope).unwrap();
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    frame
}

/// Send one vote and read the answer. `None` means the server closed the
/// connection instead of answering.
async fn ask_for_vote<S>(stream: &mut S) -> Option<serde_json::Value>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    stream.write_all(&vote_frame()).await.ok()?;
    stream.flush().await.ok()?;
    let read = async {
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).await.ok()?;
        let mut body = vec![0u8; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut body).await.ok()?;
        serde_json::from_slice::<serde_json::Value>(&body).ok()
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the server neither answered nor closed the connection")
}

async fn lone_council(id: u64) -> CouncilNode {
    CouncilNode::new(
        id,
        CouncilConfig::default(),
        TcpRaftNetworkFactory::new(id),
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap()
}

/// The server side of #928: one connection carries request after request,
/// so a peer that keeps its connection pays the handshake once.
#[tokio::test]
async fn raft_rpc_serves_many_requests_on_one_connection() {
    let shutdown = CancellationToken::new();
    let council = lone_council(1).await;
    let listener = tokio::net::TcpListener::bind(addr(0)).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let raft = council.raft().clone();
    let serving = shutdown.clone();
    tokio::spawn(async move {
        serve_raft_rpc(listener, raft, serving, None, RecoveryFence::serving(0)).await;
    });

    let mut stream = tokio::net::TcpStream::connect(bound).await.unwrap();
    for request in 0..5 {
        let answer = ask_for_vote(&mut stream)
            .await
            .unwrap_or_else(|| panic!("request {request} got no answer on the open connection"));
        assert!(answer["response"].get("Vote").is_some(), "{answer}");
    }

    shutdown.cancel();
}

/// A connection outlives its handshake, so the server re-checks the peer's
/// certificate against the live CRL before each request: a peer revoked
/// while connected is cut off at its next request, as it was when every
/// request paid a fresh handshake.
#[tokio::test]
async fn raft_rpc_closes_an_open_connection_once_its_peer_is_revoked() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let server_identity = tls.identity("n1", 10);
    let client_identity = tls.identity("n2", 11);
    let crl = CrlHandle::default();
    let acceptor = tokio_rustls::TlsAcceptor::from(
        reliaburger::sesame::mtls::build_mtls_server_config(&server_identity, crl.clone()).unwrap(),
    );
    let connector = tokio_rustls::TlsConnector::from(
        reliaburger::sesame::mtls::build_mtls_client_config(&client_identity, CrlHandle::default())
            .unwrap(),
    );

    let council = lone_council(1).await;
    let listener = tokio::net::TcpListener::bind(addr(0)).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let raft = council.raft().clone();
    let contact = council.leader_contact();
    let serving = shutdown.clone();
    let serving_crl = crl.clone();
    tokio::spawn(async move {
        reliaburger::council::network::serve_raft_rpc_with_node_gate(
            listener,
            raft,
            contact,
            serving,
            Some(acceptor),
            RecoveryFence::serving(0),
            reliaburger::smoker::node_fault::NodeTransportGate::new(),
            Some(serving_crl),
            reliaburger::council::network::RaftBlocklist::default(),
        )
        .await;
    });

    let tcp = tokio::net::TcpStream::connect(bound).await.unwrap();
    let server_name = rustls::pki_types::ServerName::IpAddress(bound.ip().into());
    let mut stream = connector.connect(server_name, tcp).await.unwrap();
    assert!(ask_for_vote(&mut stream).await.is_some());

    crl.update(revoked(11));
    assert!(
        ask_for_vote(&mut stream).await.is_none(),
        "a revoked peer's open connection must not be served"
    );

    shutdown.cancel();
}

/// A council partition must hold in both directions (#1810). The partition
/// fills the target's Raft blocklist, which used to stop only its own RPCs:
/// the leader's AppendEntries still reached it and it kept applying the log.
/// The server now names the sender of each RPC from its vote, finds that
/// node's Raft address in the membership, and closes the connection without
/// an answer when the blocklist holds it.
#[tokio::test]
async fn raft_rpc_refuses_a_peer_its_partition_blocks() {
    let shutdown = CancellationToken::new();
    let council = lone_council(1).await;
    let peer = addr(1);
    council
        .initialize(BTreeMap::from([
            (
                1,
                CouncilNodeInfo {
                    addr: addr(2),
                    name: "n1".into(),
                },
            ),
            (
                2,
                CouncilNodeInfo {
                    addr: peer,
                    name: "n2".into(),
                },
            ),
        ]))
        .await
        .unwrap();
    let blocklist = reliaburger::council::network::RaftBlocklist::default();
    blocklist.write().await.insert(peer);

    let listener = tokio::net::TcpListener::bind(addr(0)).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let raft = council.raft().clone();
    let serving = shutdown.clone();
    let contact = council.leader_contact();
    let serving_blocklist = blocklist.clone();
    tokio::spawn(async move {
        reliaburger::council::network::serve_raft_rpc_with_node_gate(
            listener,
            raft,
            contact,
            serving,
            None,
            RecoveryFence::serving(0),
            reliaburger::smoker::node_fault::NodeTransportGate::new(),
            None,
            serving_blocklist,
        )
        .await;
    });

    // `vote_frame` is a vote from node 2.
    let mut stream = tokio::net::TcpStream::connect(bound).await.unwrap();
    assert!(
        ask_for_vote(&mut stream).await.is_none(),
        "a peer the partition blocks must not be served"
    );

    blocklist.write().await.clear();
    let mut stream = tokio::net::TcpStream::connect(bound).await.unwrap();
    assert!(
        ask_for_vote(&mut stream).await.is_some(),
        "a healed partition serves the peer again"
    );

    shutdown.cancel();
}

// ---------------------------------------------------------------------------
// Council membership check on the Raft port
// ---------------------------------------------------------------------------

/// One stamped RPC, framed as the transport sends it.
fn rpc_frame(epoch: u64, rpc: reliaburger::council::network::RaftRpc) -> Vec<u8> {
    let envelope = reliaburger::council::network::RaftRpcEnvelope {
        compatibility: reliaburger::compatibility::CURRENT,
        sender_recovery_epoch: epoch,
        sender_recovery_id: None,
        rpc,
    };
    let body = serde_json::to_vec(&envelope).unwrap();
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    frame
}

/// A vote request from candidate `id`.
fn vote_from(id: u64) -> reliaburger::council::network::RaftRpc {
    reliaburger::council::network::RaftRpc::Vote(openraft::raft::VoteRequest::new(
        openraft::Vote::new(1, id),
        None,
    ))
}

/// A heartbeat from a leader `id` at term 9.
fn append_from(id: u64) -> reliaburger::council::network::RaftRpc {
    reliaburger::council::network::RaftRpc::AppendEntries(openraft::raft::AppendEntriesRequest {
        vote: openraft::Vote::new_committed(9, id),
        prev_log_id: None,
        entries: Vec::new(),
        leader_commit: None,
    })
}

/// A snapshot chunk from a leader `id` at term 9.
fn snapshot_from(id: u64) -> reliaburger::council::network::RaftRpc {
    reliaburger::council::network::RaftRpc::InstallSnapshot(
        openraft::raft::InstallSnapshotRequest {
            vote: openraft::Vote::new_committed(9, id),
            meta: Default::default(),
            offset: 0,
            data: Vec::new(),
            done: false,
        },
    )
}

/// Send one frame and read the answer. `None` means the server closed the
/// connection instead of answering.
async fn ask<S>(stream: &mut S, frame: &[u8]) -> Option<serde_json::Value>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    stream.write_all(frame).await.ok()?;
    stream.flush().await.ok()?;
    let read = async {
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).await.ok()?;
        let mut body = vec![0u8; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut body).await.ok()?;
        serde_json::from_slice::<serde_json::Value>(&body).ok()
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the server neither answered nor closed the connection")
}

/// Open an mTLS connection to `address` presenting `identity`.
async fn connect_as(
    identity: &NodeIdentity,
    address: SocketAddr,
) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
    let connector = tokio_rustls::TlsConnector::from(
        reliaburger::sesame::mtls::build_mtls_client_config(identity, CrlHandle::default())
            .unwrap(),
    );
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let server_name = rustls::pki_types::ServerName::IpAddress(address.ip().into());
    connector.connect(server_name, tcp).await.unwrap()
}

/// Node `n1` over mTLS, its council initialised with `members` (id and node
/// name; an empty map leaves it a fresh node with no council), serving Raft
/// with `fence`. Returns the node and its Raft address.
async fn council_node_over_mtls(
    tls: &TlsSetup,
    members: &[(u64, &str)],
    fence: RecoveryFence,
    shutdown: &CancellationToken,
) -> (CouncilNode, SocketAddr) {
    let identity = tls.identity("n1", 10);
    let acceptor = tokio_rustls::TlsAcceptor::from(
        reliaburger::sesame::mtls::build_mtls_server_config(&identity, CrlHandle::default())
            .unwrap(),
    );
    let council = lone_council(1).await;
    if !members.is_empty() {
        council
            .initialize(
                members
                    .iter()
                    .map(|(id, name)| {
                        (
                            *id,
                            CouncilNodeInfo {
                                addr: addr(1),
                                name: (*name).to_string(),
                            },
                        )
                    })
                    .collect(),
            )
            .await
            .unwrap();
    }
    let listener = tokio::net::TcpListener::bind(addr(0)).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let raft = council.raft().clone();
    let contact = council.leader_contact();
    let serving = shutdown.clone();
    tokio::spawn(async move {
        reliaburger::council::network::serve_raft_rpc_with_node_gate(
            listener,
            raft,
            contact,
            serving,
            Some(acceptor),
            fence,
            reliaburger::smoker::node_fault::NodeTransportGate::new(),
            None,
            reliaburger::council::network::RaftBlocklist::default(),
        )
        .await;
    });
    (council, bound)
}

/// The council is `{1, 2}`. A node outside it (`n9`) holds a valid node
/// certificate, but none of its Raft RPCs reach Raft: the server closes the
/// connection without an answer, whichever RPC it sends.
#[tokio::test]
async fn raft_rpc_refuses_every_kind_from_a_node_outside_the_council() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let (_council, bound) = council_node_over_mtls(
        &tls,
        &[(1, "n1"), (2, "n2")],
        RecoveryFence::serving(0),
        &shutdown,
    )
    .await;
    let outsider = tls.identity("n9", 19);

    for (kind, rpc) in [
        ("vote", vote_from(9)),
        ("append", append_from(9)),
        ("snapshot", snapshot_from(9)),
    ] {
        let mut stream = connect_as(&outsider, bound).await;
        assert!(
            ask(&mut stream, &rpc_frame(0, rpc)).await.is_none(),
            "a {kind} from outside the council must not be served"
        );
    }

    shutdown.cancel();
}

/// A certificate speaks only for its own node: `n9` naming council member 2
/// as the leader or candidate is refused too.
#[tokio::test]
async fn raft_rpc_refuses_a_node_speaking_for_a_council_member() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let (_council, bound) = council_node_over_mtls(
        &tls,
        &[(1, "n1"), (2, "n2")],
        RecoveryFence::serving(0),
        &shutdown,
    )
    .await;
    let outsider = tls.identity("n9", 19);

    for (kind, rpc) in [
        ("vote", vote_from(2)),
        ("append", append_from(2)),
        ("snapshot", snapshot_from(2)),
    ] {
        let mut stream = connect_as(&outsider, bound).await;
        assert!(
            ask(&mut stream, &rpc_frame(0, rpc)).await.is_none(),
            "a {kind} naming another node must not be served"
        );
    }

    shutdown.cancel();
}

/// A council member's RPCs are still served over mTLS.
#[tokio::test]
async fn raft_rpc_serves_a_council_member() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let (_council, bound) = council_node_over_mtls(
        &tls,
        &[(1, "n1"), (2, "n2")],
        RecoveryFence::serving(0),
        &shutdown,
    )
    .await;
    let member = tls.identity("n2", 11);

    let mut stream = connect_as(&member, bound).await;
    let answer = ask(&mut stream, &rpc_frame(0, vote_from(2)))
        .await
        .expect("a member's vote is answered");
    assert!(answer["response"].get("Vote").is_some(), "{answer}");
    let answer = ask(&mut stream, &rpc_frame(0, append_from(2)))
        .await
        .expect("a member's heartbeat is answered");
    assert!(
        answer["response"].get("AppendEntries").is_some(),
        "{answer}"
    );

    shutdown.cancel();
}

/// The recovery epoch on the envelope moves the fence only for a council
/// member. A node outside the council claiming a newer epoch leaves the
/// fence where it was; a member claiming it still fences the node, as a
/// recovered council's member must.
#[tokio::test]
async fn a_newer_recovery_epoch_fences_a_member_only_when_a_member_sends_it() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let fence = RecoveryFence::serving(0);
    let (_council, bound) =
        council_node_over_mtls(&tls, &[(1, "n1"), (2, "n2")], fence.clone(), &shutdown).await;

    let outsider = tls.identity("n9", 19);
    for rpc in [vote_from(9), append_from(9), snapshot_from(9)] {
        let mut stream = connect_as(&outsider, bound).await;
        assert!(ask(&mut stream, &rpc_frame(1, rpc)).await.is_none());
    }
    assert!(
        !fence.is_fenced(),
        "a newer epoch from outside the council must not fence the node"
    );

    let member = tls.identity("n2", 11);
    let mut stream = connect_as(&member, bound).await;
    let answer = ask(&mut stream, &rpc_frame(1, vote_from(2)))
        .await
        .expect("the refusal is answered");
    assert!(answer["response"].get("Fenced").is_some(), "{answer}");
    assert_eq!(fence.snapshot().fenced_by(), Some(1));

    shutdown.cancel();
}

/// A node outside the council that holds an older epoch (an old voter
/// talking to a recovered council) is still told the newer one, so it can
/// fence itself; the recovered node's own fence doesn't move.
#[tokio::test]
async fn an_older_epoch_from_outside_the_council_is_told_the_newer_one() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let fence = RecoveryFence::serving(1);
    let (_council, bound) =
        council_node_over_mtls(&tls, &[(1, "n1")], fence.clone(), &shutdown).await;

    let old_voter = tls.identity("n2", 11);
    let mut stream = connect_as(&old_voter, bound).await;
    let answer = ask(&mut stream, &rpc_frame(0, append_from(2)))
        .await
        .expect("an old voter is told the newer epoch");
    assert_eq!(
        answer["response"]["Fenced"]["recovery_epoch"],
        serde_json::json!(1),
        "{answer}"
    );
    assert_eq!(fence.snapshot(), RecoveryFence::serving(1).snapshot());

    shutdown.cancel();
}

/// A fresh node has no council to check against: the first leader to
/// replicate to it is how it joins. It still serves any node certificate,
/// and adopts the epoch of the council that reaches it.
#[tokio::test]
async fn a_fresh_node_serves_the_council_that_adds_it() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let fence = RecoveryFence::new(
        reliaburger::council::fence::FenceSnapshot {
            epoch: 0,
            recovery_id: None,
            state: reliaburger::council::fence::FenceState::Unclaimed,
        },
        None,
    );
    let (_council, bound) = council_node_over_mtls(&tls, &[], fence.clone(), &shutdown).await;

    let leader = tls.identity("n5", 15);
    let mut stream = connect_as(&leader, bound).await;
    let answer = ask(&mut stream, &rpc_frame(2, append_from(5)))
        .await
        .expect("a fresh node serves the leader that adds it");
    assert!(
        answer["response"].get("AppendEntries").is_some(),
        "{answer}"
    );
    assert_eq!(fence.snapshot().claimed_epoch(), Some(2));

    shutdown.cancel();
}

/// A node with no council yet that already holds an epoch (a survivor
/// `relish council recover` stamped, before it re-forms its council) can't
/// be fenced by a newer epoch from a node it has never heard of.
#[tokio::test]
async fn a_newer_epoch_does_not_fence_a_node_that_has_no_council_yet() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let fence = RecoveryFence::serving(1);
    let (_council, bound) = council_node_over_mtls(&tls, &[], fence.clone(), &shutdown).await;

    let outsider = tls.identity("n9", 19);
    let mut stream = connect_as(&outsider, bound).await;
    assert!(
        ask(&mut stream, &rpc_frame(2, append_from(9)))
            .await
            .is_none()
    );
    assert!(!fence.is_fenced());

    shutdown.cancel();
}

/// Membership changes keep working under the council membership check: a
/// learner added from outside the council receives replication, a promoted
/// voter takes part, and a leader that removes itself still commits its own
/// removal (its followers have already appended the entry that drops it)
/// before the remaining voters elect a new leader among themselves.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_council_replaces_its_leader_over_mtls() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();
    let crl = CrlHandle::default();
    let identities = [
        tls.identity("n1", 10),
        tls.identity("n2", 11),
        tls.identity("n3", 12),
        tls.identity("n4", 13),
    ];
    let nodes = [
        make_node_tls(1, &shutdown, Some((&identities[0], crl.clone()))).await,
        make_node_tls(2, &shutdown, Some((&identities[1], crl.clone()))).await,
        make_node_tls(3, &shutdown, Some((&identities[2], crl.clone()))).await,
    ];
    let fourth = make_node_tls(4, &shutdown, Some((&identities[3], crl.clone()))).await;
    assert!(
        grow_one_to_three(&nodes, 100).await,
        "mTLS voters did not reach {{1,2,3}}"
    );

    let leader = &nodes[0].0;
    let ten_s = Duration::from_secs(10);
    tokio::time::timeout(ten_s, leader.add_learner(4, info_at(4, fourth.1)))
        .await
        .expect("adding the learner timed out")
        .expect("adding the learner failed");
    tokio::time::timeout(
        ten_s,
        leader.change_membership_evicting(BTreeSet::from([2, 3, 4])),
    )
    .await
    .expect("replacing the leader timed out")
    .expect("replacing the leader failed");

    let expected = BTreeSet::from([2, 3, 4]);
    let survivors = [&nodes[1].0, &nodes[2].0, &fourth.0];
    let mut converged = false;
    for _ in 0..100 {
        let applied = survivors
            .iter()
            .all(|node| applied_voters(node) == expected);
        let mut led = false;
        for node in survivors {
            led |= node.is_leader().await;
        }
        if applied && led {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        converged,
        "the council {{2,3,4}} did not converge on a new leader"
    );

    shutdown.cancel();
    for (council, _) in nodes.into_iter().chain([fourth]) {
        council.shutdown().await.ok();
    }
}

/// Serve node `id`'s Raft over mTLS as `identity`, with the council membership check's
/// vouching dialling peers as that identity too.
async fn serve_with_vouching(
    council: &CouncilNode,
    id: u64,
    identity: &NodeIdentity,
    shutdown: &CancellationToken,
) -> SocketAddr {
    let acceptor = tokio_rustls::TlsAcceptor::from(
        reliaburger::sesame::mtls::build_mtls_server_config(identity, CrlHandle::default())
            .unwrap(),
    );
    let connector = tokio_rustls::TlsConnector::from(
        reliaburger::sesame::mtls::build_mtls_client_config(identity, CrlHandle::default())
            .unwrap(),
    );
    let admission = reliaburger::council::admission::RaftAdmission::new(
        TcpRaftNetworkFactory::new_tls(id, connector),
    );
    let listener = tokio::net::TcpListener::bind(addr(0)).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let raft = council.raft().clone();
    let contact = council.leader_contact();
    let serving = shutdown.clone();
    tokio::spawn(async move {
        reliaburger::council::network::serve_raft_rpc_with_admission(
            listener,
            raft,
            contact,
            serving,
            Some(acceptor),
            RecoveryFence::serving(0),
            reliaburger::smoker::node_fault::NodeTransportGate::new(),
            None,
            reliaburger::council::network::RaftBlocklist::default(),
            admission,
        )
        .await;
    });
    bound
}

/// Send `frame` on a fresh connection as `identity` until it is answered or
/// `attempts` × 200 ms have passed. Returns whether it was ever answered.
async fn answered_within(
    identity: &NodeIdentity,
    address: SocketAddr,
    frame: &[u8],
    attempts: u32,
) -> bool {
    for _ in 0..attempts {
        let mut stream = connect_as(identity, address).await;
        if ask(&mut stream, frame).await.is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// A member whose view of the council is behind (it knows `{1, 2}`, while
/// the council is `{1, 2, 3}` and node 3 leads) must not lock out the
/// leader it has never heard of. It asks voter 1, which names node 3, and
/// then serves node 3's replication. A node no voter names (`n9`) stays
/// refused, and so does a certificate speaking for the vouched node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_whose_view_is_behind_admits_a_leader_its_voters_name() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();

    let voucher = lone_council(1).await;
    let voucher_address =
        serve_with_vouching(&voucher, 1, &tls.identity("n1", 10), &shutdown).await;
    voucher
        .initialize(BTreeMap::from([
            (1, info_at(1, voucher_address)),
            (2, info_at(2, addr(1))),
            (3, info_at(3, addr(1))),
        ]))
        .await
        .unwrap();

    let behind = lone_council(2).await;
    let behind_address = serve_with_vouching(&behind, 2, &tls.identity("n2", 11), &shutdown).await;
    behind
        .initialize(BTreeMap::from([
            (1, info_at(1, voucher_address)),
            (2, info_at(2, behind_address)),
        ]))
        .await
        .unwrap();

    let leader = tls.identity("n3", 12);
    let mut stream = connect_as(&leader, behind_address).await;
    assert!(
        ask(&mut stream, &rpc_frame(0, append_from(3)))
            .await
            .is_none(),
        "a leader this node has never heard of isn't served before a voter names it"
    );
    assert!(
        answered_within(&leader, behind_address, &rpc_frame(0, append_from(3)), 50).await,
        "the leader voter 1 names must be served"
    );

    let outsider = tls.identity("n9", 19);
    assert!(
        !answered_within(&outsider, behind_address, &rpc_frame(0, append_from(9)), 20).await,
        "a node no voter names must stay refused"
    );
    assert!(
        !answered_within(&outsider, behind_address, &rpc_frame(0, append_from(3)), 5).await,
        "a vouched node's id doesn't admit another node's certificate"
    );

    shutdown.cancel();
}

/// A vote request from candidate `id` whose log reaches `index` at term 9.
fn vote_reaching(id: u64, index: u64) -> reliaburger::council::network::RaftRpc {
    reliaburger::council::network::RaftRpc::Vote(openraft::raft::VoteRequest::new(
        openraft::Vote::new(9, id),
        Some(openraft::LogId::new(
            openraft::CommittedLeaderId::new(9, id),
            index,
        )),
    ))
}

/// A stranger's vote is held to the same rule as its replication: refused
/// until a voter this node knows names it, then served. A candidate no
/// voter names stays refused however long its log claims to be.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_strangers_vote_is_served_only_once_a_voter_names_it() {
    let shutdown = CancellationToken::new();
    let tls = TlsSetup::new();

    let voucher = lone_council(1).await;
    let voucher_address =
        serve_with_vouching(&voucher, 1, &tls.identity("n1", 10), &shutdown).await;
    voucher
        .initialize(BTreeMap::from([
            (1, info_at(1, voucher_address)),
            (2, info_at(2, addr(1))),
            (3, info_at(3, addr(1))),
        ]))
        .await
        .unwrap();

    let behind = lone_council(2).await;
    let behind_address = serve_with_vouching(&behind, 2, &tls.identity("n2", 11), &shutdown).await;
    behind
        .initialize(BTreeMap::from([
            (1, info_at(1, voucher_address)),
            (2, info_at(2, behind_address)),
        ]))
        .await
        .unwrap();

    let outsider = tls.identity("n9", 19);
    assert!(
        !answered_within(
            &outsider,
            behind_address,
            &rpc_frame(0, vote_reaching(9, 100)),
            20
        )
        .await,
        "a candidate no voter names must stay refused"
    );

    let candidate = tls.identity("n3", 12);
    let mut stream = connect_as(&candidate, behind_address).await;
    assert!(
        ask(&mut stream, &rpc_frame(0, vote_reaching(3, 100)))
            .await
            .is_none(),
        "a candidate this node has never heard of isn't served before a voter names it"
    );
    assert!(
        answered_within(
            &candidate,
            behind_address,
            &rpc_frame(0, vote_reaching(3, 100)),
            50
        )
        .await,
        "the candidate voter 1 names must be served"
    );

    shutdown.cancel();
}

/// One node of a vouching council: its council, its Raft address, the
/// blocklist its own Raft client honours, and the token that stops its
/// listener. The council's factory and the vouching share one connector.
struct VouchingNode {
    council: Arc<CouncilNode>,
    address: SocketAddr,
    blocklist: reliaburger::council::network::RaftBlocklist,
    listener: CancellationToken,
}

/// Start node `id` over mTLS on `log` and `state` (pass clones of a
/// previous run's stores to restart it), listening on `at` (`addr(0)` for
/// any port).
async fn start_vouching_node(
    id: u64,
    identity: &NodeIdentity,
    log: MemLogStore,
    state: CouncilStateMachine,
    at: SocketAddr,
) -> VouchingNode {
    let connector = tokio_rustls::TlsConnector::from(
        reliaburger::sesame::mtls::build_mtls_client_config(identity, CrlHandle::default())
            .unwrap(),
    );
    let acceptor = tokio_rustls::TlsAcceptor::from(
        reliaburger::sesame::mtls::build_mtls_server_config(identity, CrlHandle::default())
            .unwrap(),
    );
    let factory = TcpRaftNetworkFactory::new_tls(id, connector);
    let blocklist = factory.blocklist();
    let admission = reliaburger::council::admission::RaftAdmission::new(factory.clone());
    let council = Arc::new(
        CouncilNode::new(id, CouncilConfig::default(), factory, log, state, None)
            .await
            .unwrap(),
    );
    // A restarted node takes its old port back; the old listener may take
    // a moment to let go of it.
    let mut bound = None;
    for _ in 0..50 {
        match tokio::net::TcpListener::bind(at).await {
            Ok(listener) => {
                bound = Some(listener);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let listener = bound.expect("the Raft port never came free");
    let address = listener.local_addr().unwrap();
    let token = CancellationToken::new();
    let raft = council.raft().clone();
    let contact = council.leader_contact();
    let serving = token.clone();
    let server_blocklist = blocklist.clone();
    tokio::spawn(async move {
        reliaburger::council::network::serve_raft_rpc_with_admission(
            listener,
            raft,
            contact,
            serving,
            Some(acceptor),
            RecoveryFence::serving(0),
            reliaburger::smoker::node_fault::NodeTransportGate::new(),
            None,
            server_blocklist,
            admission,
        )
        .await;
    });
    VouchingNode {
        council,
        address,
        blocklist,
        listener: token,
    }
}

/// Stop a node as a crash would, as far as its peers can tell: its
/// listener closes and its Raft stops.
async fn crash(node: &VouchingNode) {
    node.listener.cancel();
    node.council.shutdown().await.ok();
}

/// Wait up to `attempts` × 100 ms for `check` to hold.
async fn eventually<F, Fut>(attempts: u32, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..attempts {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// How the lagging-voter scenario plays out after the swap.
#[derive(Clone, Copy, PartialEq)]
enum AfterTheSwap {
    /// Node 3, evicted, keeps running and can vouch for node 4.
    EvictedVoterRuns,
    /// As `EvictedVoterRuns`, with node 2 restarted while it lags.
    LaggingVoterRestarts,
    /// Node 3 dies too, so node 2 can reach none of the voters it knows.
    EvictedVoterDies,
}

/// The council `{1, 2, 3}` swaps voter 3 for node 4 while voter 2 lags: the
/// joint and the final configs commit without it, so its membership still
/// says `{1, 2, 3}`. Then leader 1 crashes. Node 4 can only win with node
/// 2's vote, and node 2 has never heard of node 4, so it must ask voter 3
/// (evicted, but still running) before it votes. `after` picks the
/// variant: node 2 restarted while it lags, or voter 3 dead too, which is
/// the documented case with no leader until node 1 returns.
async fn lagging_voter_after_a_swap(after: AfterTheSwap) {
    let tls = TlsSetup::new();
    let identities: Vec<NodeIdentity> = (1..=4)
        .map(|id| tls.identity(&format!("n{id}"), 10 + id))
        .collect();
    let mut stores: Vec<(MemLogStore, CouncilStateMachine)> = (0..4)
        .map(|_| (MemLogStore::new(), CouncilStateMachine::new()))
        .collect();
    let mut nodes = Vec::new();
    for (index, identity) in identities.iter().enumerate() {
        let (log, state) = stores[index].clone();
        nodes.push(start_vouching_node(index as u64 + 1, identity, log, state, addr(0)).await);
    }
    let info = |nodes: &[VouchingNode], id: u64| info_at(id, nodes[id as usize - 1].address);

    let leader = Arc::clone(&nodes[0].council);
    leader
        .initialize(BTreeMap::from([(1, info(&nodes, 1))]))
        .await
        .unwrap();
    assert!(
        eventually(50, || leader.is_leader()).await,
        "node 1 never led"
    );
    let ten_s = Duration::from_secs(10);
    for id in [2, 3] {
        tokio::time::timeout(ten_s, leader.add_learner(id, info(&nodes, id)))
            .await
            .unwrap()
            .unwrap();
    }
    tokio::time::timeout(ten_s, leader.change_membership(BTreeSet::from([1, 2, 3])))
        .await
        .unwrap()
        .unwrap();
    let three = BTreeSet::from([1, 2, 3]);
    assert!(
        eventually(100, || async { applied_voters(&nodes[1].council) == three }).await,
        "node 2 never applied {{1,2,3}}"
    );

    // Node 2 falls behind: the leader stops reaching it.
    nodes[0].blocklist.write().await.insert(nodes[1].address);
    tokio::time::timeout(ten_s, leader.add_learner(4, info(&nodes, 4)))
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        ten_s,
        leader.change_membership_evicting(BTreeSet::from([1, 2, 4])),
    )
    .await
    .expect("swapping voter 3 for node 4 timed out")
    .expect("swapping voter 3 for node 4 failed");
    assert_eq!(
        applied_voters(&nodes[1].council),
        three,
        "node 2 was meant to miss the change"
    );

    match after {
        AfterTheSwap::EvictedVoterRuns => {}
        AfterTheSwap::LaggingVoterRestarts => {
            let address = nodes[1].address;
            crash(&nodes[1]).await;
            let (log, state) = stores[1].clone();
            nodes[1] = start_vouching_node(2, &identities[1], log, state, address).await;
            // openraft publishes the restored membership shortly after start.
            assert!(
                eventually(50, || async { applied_voters(&nodes[1].council) == three }).await,
                "the restarted node didn't come back with its old membership"
            );
        }
        AfterTheSwap::EvictedVoterDies => crash(&nodes[2]).await,
    }

    crash(&nodes[0]).await;
    let new_leader = Arc::clone(&nodes[3].council);
    let final_voters = BTreeSet::from([1, 2, 4]);
    if after == AfterTheSwap::EvictedVoterDies {
        // Every voter node 2 knows is gone, so nobody can vouch for node 4,
        // and node 4 can't win without node 2's vote: no leader. This is the
        // documented limit; the way out is to bring node 1 (or 3) back, or
        // recover the council.
        assert!(
            !eventually(100, || async {
                new_leader.is_leader().await || nodes[1].council.is_leader().await
            })
            .await,
            "nobody can vouch for node 4, so the council must stay without a leader"
        );
        let address = nodes[0].address;
        let (log, state) = stores[0].clone();
        nodes[0] = start_vouching_node(1, &identities[0], log, state, address).await;
        assert!(
            eventually(300, || async {
                nodes[0].council.is_leader().await || new_leader.is_leader().await
            })
            .await,
            "the council never elected a leader once node 1 came back"
        );
    } else {
        assert!(
            eventually(300, || new_leader.is_leader()).await,
            "node 4 never won an election with the lagging voter's vote"
        );
    }
    assert!(
        eventually(100, || async {
            applied_voters(&nodes[1].council) == final_voters
        })
        .await,
        "the lagging voter never caught up with the new leader"
    );

    for node in &nodes {
        crash(node).await;
    }
    stores.clear();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lagging_voter_votes_for_a_voter_added_while_it_was_behind() {
    lagging_voter_after_a_swap(AfterTheSwap::EvictedVoterRuns).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lagging_voter_restarted_while_behind_still_votes_for_the_new_voter() {
    lagging_voter_after_a_swap(AfterTheSwap::LaggingVoterRestarts).await;
}

/// The documented limit of the membership check: once leader 1 and the
/// swapped-out voter 3 are both gone, node 2 can reach none of the voters
/// it knows, nobody can vouch for node 4, and the council has no leader.
/// It recovers when node 1 comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lagging_voter_that_lost_every_voter_it_knows_waits_for_one_to_return() {
    lagging_voter_after_a_swap(AfterTheSwap::EvictedVoterDies).await;
}
