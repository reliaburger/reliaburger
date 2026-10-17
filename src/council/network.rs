//! In-memory Raft network for testing.
//!
//! Routes RPCs directly between `openraft::Raft` handles without
//! TCP, enabling fast deterministic tests with partition simulation.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, Unreachable};
use openraft::network::RPCOption;
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::{Raft, RaftNetwork, RaftNetworkFactory};
use tokio::sync::Mutex;

use super::connection_pool::{
    PeerKey, RaftConnection, RaftConnectionPool, ReuseCheck, SERVER_CONNECTION_MAX_AGE,
    SERVER_IDLE_TIMEOUT,
};
use super::fence::{Admission, RecoveryFence};
use super::types::{CouncilNodeInfo, TypeConfig};
use super::types::{RecoveryClaim, RecoveryId};

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/// Thin wrapper to make a string into an `Error`.
#[derive(Debug)]
struct RouterError(String);

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RouterError {}

// ---------------------------------------------------------------------------
// InMemoryRaftRouter
// ---------------------------------------------------------------------------

/// Routes Raft RPCs between in-memory nodes.
///
/// Each node registers its `Raft<TypeConfig>` handle. Sends look up
/// the target's handle and call its method directly. Partitions
/// silently return `Unreachable`.
#[derive(Clone, Default)]
pub struct InMemoryRaftRouter {
    rafts: Arc<Mutex<HashMap<u64, Raft<TypeConfig>>>>,
    partitions: Arc<Mutex<HashSet<(u64, u64)>>>,
}

impl fmt::Debug for InMemoryRaftRouter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryRaftRouter")
            .field("num_rafts", &"<opaque>")
            .field("partitions", &self.partitions)
            .finish()
    }
}

impl InMemoryRaftRouter {
    /// Create a new empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a Raft instance with the router.
    pub async fn register(&self, id: u64, raft: Raft<TypeConfig>) {
        self.rafts.lock().await.insert(id, raft);
    }

    /// Simulate a network partition between `a` and `b` (bidirectional).
    pub async fn partition(&self, a: u64, b: u64) {
        let mut parts = self.partitions.lock().await;
        parts.insert((a, b));
        parts.insert((b, a));
    }

    /// Heal all partitions.
    pub async fn heal(&self) {
        self.partitions.lock().await.clear();
    }

    /// Check if a message from `from` to `to` would be dropped.
    pub async fn is_partitioned(&self, from: u64, to: u64) -> bool {
        self.partitions.lock().await.contains(&(from, to))
    }

    /// Look up a Raft handle, returning Unreachable if partitioned or not found.
    async fn lookup(&self, from: u64, target: u64) -> Result<Raft<TypeConfig>, Unreachable> {
        if self.is_partitioned(from, target).await {
            return Err(Unreachable::new(&RouterError(format!(
                "partitioned: {} -> {}",
                from, target
            ))));
        }
        let rafts = self.rafts.lock().await;
        rafts
            .get(&target)
            .cloned()
            .ok_or_else(|| Unreachable::new(&RouterError(format!("unknown target: {}", target))))
    }
}

// ---------------------------------------------------------------------------
// InMemoryRaftNetworkFactory
// ---------------------------------------------------------------------------

/// Creates `InMemoryRaftNetwork` instances for each target node.
#[derive(Debug, Clone)]
pub struct InMemoryRaftNetworkFactory {
    source_id: u64,
    router: InMemoryRaftRouter,
}

impl InMemoryRaftNetworkFactory {
    /// Create a new factory for a specific source node.
    pub fn new(source_id: u64, router: InMemoryRaftRouter) -> Self {
        Self { source_id, router }
    }
}

impl RaftNetworkFactory<TypeConfig> for InMemoryRaftNetworkFactory {
    type Network = InMemoryRaftNetwork;

    async fn new_client(&mut self, target: u64, _node: &CouncilNodeInfo) -> Self::Network {
        InMemoryRaftNetwork {
            source_id: self.source_id,
            target,
            router: self.router.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// InMemoryRaftNetwork
// ---------------------------------------------------------------------------

/// A single connection in the in-memory Raft network.
#[derive(Debug)]
pub struct InMemoryRaftNetwork {
    source_id: u64,
    target: u64,
    router: InMemoryRaftRouter,
}

impl RaftNetwork<TypeConfig> for InMemoryRaftNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, CouncilNodeInfo, RaftError<u64>>> {
        let raft = self
            .router
            .lookup(self.source_id, self.target)
            .await
            .map_err(RPCError::Unreachable)?;
        raft.append_entries(rpc)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, CouncilNodeInfo, RaftError<u64, InstallSnapshotError>>,
    > {
        let raft = self
            .router
            .lookup(self.source_id, self.target)
            .await
            .map_err(RPCError::Unreachable)?;
        raft.install_snapshot(rpc)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, CouncilNodeInfo, RaftError<u64>>> {
        let raft = self
            .router
            .lookup(self.source_id, self.target)
            .await
            .map_err(RPCError::Unreachable)?;
        raft.vote(rpc)
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))
    }
}

// ---------------------------------------------------------------------------
// TCP Raft transport for production
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum Raft RPC payload size (64 MiB — snapshots can be large).
const MAX_RAFT_RPC_SIZE: usize = 64 * 1024 * 1024;

/// How long the accept side waits for a peer to finish its mTLS handshake,
/// and how long it may spend serving one request, before dropping the
/// connection (CP11). A peer that connects and stalls (half-open handshake)
/// must not pin the task forever. Waiting for the next request on an open
/// connection is bounded separately, by `SERVER_IDLE_TIMEOUT` (#928).
const RAFT_ACCEPT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// How much of a frame we read (and therefore allocate) at a time.
///
/// The length prefix is attacker-controlled up to [`MAX_RAFT_RPC_SIZE`], so
/// allocating it up front let one connection claim 64 MiB by sending four
/// bytes. Reading in chunks means the buffer only grows as bytes actually
/// arrive (O6).
const FRAME_CHUNK_SIZE: usize = 64 * 1024;

/// How many Raft RPC connections may be in flight at once.
///
/// Each one is already bounded in time by [`RAFT_ACCEPT_DEADLINE`] and in
/// size by [`MAX_RAFT_RPC_SIZE`]; this bounds them in *number*, so a peer
/// opening connections in a loop can't fan out unbounded tasks and buffers.
/// A real council is a handful of nodes, so this is generous (O6).
const MAX_RAFT_CONNECTIONS: usize = 64;

/// Raft RPC request envelope, serialised over TCP.
#[derive(Serialize, Deserialize)]
pub enum RaftRpc {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    Vote(VoteRequest<u64>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
}

/// A Raft RPC stamped with the sender's recovery epoch (C5, #424).
///
/// Disaster recovery (`relish council recover`) bumps a node's recovery epoch
/// and wipes the old Raft log, minting a fresh single-voter council. Stamping
/// the epoch on every RPC lets each side learn the other's. A node that hears
/// of a newer epoch than its own fences itself (see [`super::fence`]), and a
/// node that refuses an RPC says which epoch it knows, so the news reaches
/// every old voter that tries to talk to the recovered council or to an
/// already fenced peer.
#[derive(Serialize, Deserialize)]
pub struct RaftRpcEnvelope {
    /// Protocol and state formats required before this RPC may reach Raft.
    pub compatibility: crate::compatibility::Compatibility,
    /// The sending node's recovery epoch.
    pub sender_recovery_epoch: u64,
    /// The recovery that started the sender's epoch (#694), `None` at epoch
    /// 0. A receiver on the same epoch with a different id fences itself
    /// as a split and names its own id in the refusal.
    pub sender_recovery_id: Option<RecoveryId>,
    /// Versioned request field also makes development decoders refuse this frame.
    #[serde(rename = "request")]
    pub rpc: RaftRpc,
}

/// Decode a stamped Raft RPC frame and run it past the recovery fence.
///
/// Returns `None` (drop the connection) for a malformed or incompatible
/// frame, and otherwise the fence's verdict with the inner RPC. Kept free of
/// I/O so the fence is unit-tested without a `Raft` instance.
fn decode_and_admit(payload: &[u8], fence: &RecoveryFence) -> Option<(Admission, RaftRpc)> {
    let envelope = serde_json::from_slice::<RaftRpcEnvelope>(payload).ok()?;
    envelope.compatibility.require_current().ok()?;
    let sender = RecoveryClaim {
        epoch: envelope.sender_recovery_epoch,
        id: envelope.sender_recovery_id,
    };
    Some((fence.admit(sender), envelope.rpc))
}

/// Raft RPC response envelope, serialised over TCP.
#[derive(Serialize, Deserialize)]
pub enum RaftRpcResponse {
    AppendEntries(AppendEntriesResponse<u64>),
    Vote(VoteResponse<u64>),
    InstallSnapshot(InstallSnapshotResponse<u64>),
    /// The peer refused the RPC under the recovery fence. `recovery_epoch` is
    /// the newest epoch it knows; a sender holding an older one fences itself.
    /// `recovery_id` is the peer's own recovery id when that epoch is its
    /// own: a sender on that epoch with a different id is a split (#694).
    Fenced {
        recovery_epoch: u64,
        recovery_id: Option<RecoveryId>,
    },
}

#[derive(Serialize, Deserialize)]
struct RaftResponseEnvelope {
    compatibility: crate::compatibility::Compatibility,
    response: RaftRpcResponse,
}

/// Read a length-prefixed frame from any byte stream (plain TCP or TLS).
async fn read_frame<S: AsyncRead + Unpin>(stream: &mut S) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_RAFT_RPC_SIZE {
        eprintln!("raft: dropping oversized frame: {len} bytes (max {MAX_RAFT_RPC_SIZE})");
        return None;
    }
    // O6: grow the buffer as bytes arrive rather than trusting `len`. A peer
    // that declares 64 MiB and then sends nothing costs one chunk, not 64 MiB.
    let mut payload = Vec::new();
    let mut chunk = vec![0u8; FRAME_CHUNK_SIZE.min(len.max(1))];
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(chunk.len());
        stream.read_exact(&mut chunk[..want]).await.ok()?;
        payload.extend_from_slice(&chunk[..want]);
        remaining -= want;
    }
    Some(payload)
}

/// Write a length-prefixed frame to any byte stream (plain TCP or TLS).
async fn write_frame<S: AsyncWrite + Unpin>(stream: &mut S, data: &[u8]) -> Result<(), String> {
    // One write for prefix and payload: two small writes on a reused
    // connection can become two TLS records and two TCP segments (#928).
    let mut frame = Vec::with_capacity(4 + data.len());
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(data);
    stream.write_all(&frame).await.map_err(|e| e.to_string())?;
    // TLS buffers until flushed; a plain TCP stream flushes on write.
    stream.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Serve Raft RPCs over TCP, optionally wrapped in mTLS.
///
/// Accepts connections on `listener` and serves requests on each one in
/// turn, dispatching each to the local `raft` instance and writing its
/// response, until the peer closes the connection, leaves it idle for
/// `SERVER_IDLE_TIMEOUT` or it reaches `SERVER_CONNECTION_MAX_AGE` (#928).
/// When `tls` is `Some`, every connection must complete an mTLS handshake
/// (the peer presents a client certificate) before its first frame is read.
/// Runs until `shutdown` is cancelled.
pub async fn serve_raft_rpc(
    listener: tokio::net::TcpListener,
    raft: Raft<TypeConfig>,
    shutdown: tokio_util::sync::CancellationToken,
    tls: Option<tokio_rustls::TlsAcceptor>,
    fence: RecoveryFence,
) {
    serve_raft_rpc_with_limit_and_node_gate(
        listener,
        raft,
        shutdown,
        tls,
        fence,
        MAX_RAFT_CONNECTIONS,
        crate::smoker::node_fault::NodeTransportGate::new(),
        None,
    )
    .await
}

/// Serve Raft RPCs while observing the reversible node-level fault gate.
///
/// `crl` is the live CRL the TLS acceptor checks at the handshake. With it,
/// each request on an open connection re-checks the peer's certificate, so
/// a revocation takes effect at the peer's next RPC rather than when its
/// connection closes.
pub async fn serve_raft_rpc_with_node_gate(
    listener: tokio::net::TcpListener,
    raft: Raft<TypeConfig>,
    shutdown: tokio_util::sync::CancellationToken,
    tls: Option<tokio_rustls::TlsAcceptor>,
    fence: RecoveryFence,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
    crl: Option<crate::sesame::mtls::CrlHandle>,
) {
    serve_raft_rpc_with_limit_and_node_gate(
        listener,
        raft,
        shutdown,
        tls,
        fence,
        MAX_RAFT_CONNECTIONS,
        node_gate,
        crl,
    )
    .await
}

/// [`serve_raft_rpc`] with an injectable connection cap, so the bound can be
/// exercised without opening `MAX_RAFT_CONNECTIONS` sockets.
pub async fn serve_raft_rpc_with_limit(
    listener: tokio::net::TcpListener,
    raft: Raft<TypeConfig>,
    shutdown: tokio_util::sync::CancellationToken,
    tls: Option<tokio_rustls::TlsAcceptor>,
    fence: RecoveryFence,
    max_connections: usize,
) {
    serve_raft_rpc_with_limit_and_node_gate(
        listener,
        raft,
        shutdown,
        tls,
        fence,
        max_connections,
        crate::smoker::node_fault::NodeTransportGate::new(),
        None,
    )
    .await
}

/// Connection-limited Raft server with a shared node-level fault gate and an
/// optional live CRL (see [`serve_raft_rpc_with_node_gate`]).
#[allow(clippy::too_many_arguments)]
pub async fn serve_raft_rpc_with_limit_and_node_gate(
    listener: tokio::net::TcpListener,
    raft: Raft<TypeConfig>,
    shutdown: tokio_util::sync::CancellationToken,
    tls: Option<tokio_rustls::TlsAcceptor>,
    fence: RecoveryFence,
    max_connections: usize,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
    crl: Option<crate::sesame::mtls::CrlHandle>,
) {
    // O6: hold a permit for the whole lifetime of a connection. Acquiring
    // *before* accepting means excess peers queue in the kernel's backlog
    // instead of each costing a task and a buffer. A connection now serves
    // many requests (#928), but every permit is still released in bounded
    // time: the handshake and each request have RAFT_ACCEPT_DEADLINE, the
    // wait between requests has SERVER_IDLE_TIMEOUT, and no connection
    // outlives SERVER_CONNECTION_MAX_AGE by more than one request.
    let connections = std::sync::Arc::new(tokio::sync::Semaphore::new(max_connections));
    loop {
        let permit = tokio::select! {
            _ = shutdown.cancelled() => break,
            permit = connections.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                // Only returned once the semaphore is closed, which we never do.
                Err(_) => break,
            },
        };
        tokio::select! {
            _ = shutdown.cancelled() => break,
            result = listener.accept() => {
                match result {
                    Ok((stream, _peer)) => {
                        if node_gate.is_quiesced() {
                            continue;
                        }
                        // Requests and responses are small and waited on:
                        // send each at once instead of letting Nagle's
                        // algorithm hold it for an earlier segment's ACK.
                        // Failing to set it only costs latency.
                        let _ = stream.set_nodelay(true);
                        let raft = raft.clone();
                        let fence = fence.clone();
                        let connection_gate = node_gate.clone();
                        match tls.clone() {
                            Some(acceptor) => {
                                let crl = crl.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    // A failed handshake (no/invalid/revoked
                                    // client cert) is dropped silently — a
                                    // rejected peer must not learn why. A
                                    // stalled handshake can't pin the task
                                    // (CP11).
                                    let Ok(Ok(tls_stream)) =
                                        tokio::time::timeout(RAFT_ACCEPT_DEADLINE, acceptor.accept(stream))
                                            .await
                                    else {
                                        return;
                                    };
                                    // The handshake checked the peer's chain
                                    // against the CRL once; re-check it
                                    // before every later request (#928).
                                    let still_admitted =
                                        |stream: &tokio_rustls::server::TlsStream<tokio::net::TcpStream>| {
                                            match &crl {
                                                Some(crl) => stream
                                                    .get_ref()
                                                    .1
                                                    .peer_certificates()
                                                    .is_some_and(|chain| crl.admits(chain)),
                                                None => true,
                                            }
                                        };
                                    serve_connection(
                                        tls_stream,
                                        raft,
                                        fence,
                                        connection_gate,
                                        still_admitted,
                                    )
                                    .await;
                                });
                            }
                            None => {
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    serve_connection(stream, raft, fence, connection_gate, |_| true)
                                        .await;
                                });
                            }
                        }
                    }
                    // Nothing was accepted, so release the permit rather than
                    // leaking it on a transient accept error.
                    Err(_) => continue,
                }
            }
        }
    }
}

/// Serve requests on one connection until it should close (#928).
///
/// The connection closes when the peer closes it, sends nothing for
/// `SERVER_IDLE_TIMEOUT`, sends something this side can't serve, is no
/// longer `admitted` (its certificate was revoked), or has been open for
/// `SERVER_CONNECTION_MAX_AGE`. The node fault gate is checked per request,
/// so quiescing a node cuts its open connections too.
///
/// `admitted` is a closure: `F: Fn(&S) -> bool` means any function or
/// closure that borrows the stream and returns a `bool`. The TLS path passes
/// one that reads the peer's certificates out of the TLS session; the plain
/// path passes `|_| true`.
async fn serve_connection<S, F>(
    mut stream: S,
    raft: Raft<TypeConfig>,
    fence: RecoveryFence,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
    admitted: F,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Fn(&S) -> bool,
{
    let opened = tokio::time::Instant::now();
    loop {
        let Ok(Some(payload)) =
            tokio::time::timeout(SERVER_IDLE_TIMEOUT, read_frame(&mut stream)).await
        else {
            return;
        };
        if node_gate.is_quiesced() || !admitted(&stream) {
            return;
        }
        let served = tokio::time::timeout(
            RAFT_ACCEPT_DEADLINE,
            handle_raft_frame(&mut stream, &payload, &raft, &fence),
        )
        .await;
        if !matches!(served, Ok(true)) || opened.elapsed() >= SERVER_CONNECTION_MAX_AGE {
            return;
        }
    }
}

/// Serve one request frame and write its response. Returns whether the
/// connection may carry another request.
async fn handle_raft_frame<S: AsyncWrite + Unpin>(
    stream: &mut S,
    payload: &[u8],
    raft: &Raft<TypeConfig>,
    fence: &RecoveryFence,
) -> bool {
    // JSON, not bincode: AppendEntries carries log entries whose
    // `AppSpec` payload uses `deserialize_any` config types (see
    // durable_log). bincode is not self-describing and corrupts them,
    // so a replicated AppSpec entry would never deserialise on the
    // follower and the write would hang forever.
    //
    // The frame is a `RaftRpcEnvelope`; the recovery fence (C5, #424) decides
    // whether it reaches Raft at all.
    let Some((admission, rpc)) = decode_and_admit(payload, fence) else {
        return false;
    };
    match admission {
        Admission::Serve => {}
        Admission::Adopted => {
            // A fresh node joining a council: make the adopted epoch durable
            // before acting on it, or a crash could bring it back claiming
            // nothing with Raft state on disk.
            if let Err(error) = fence.persist().await {
                eprintln!("raft: could not persist the adopted recovery epoch: {error}");
                return false;
            }
        }
        Admission::Refuse { newest } => {
            // Answer, then close: a fenced peer has nothing more to say here.
            write_response(
                stream,
                RaftRpcResponse::Fenced {
                    recovery_epoch: newest.epoch,
                    recovery_id: newest.id,
                },
            )
            .await;
            return false;
        }
    }

    let response = match rpc {
        RaftRpc::AppendEntries(req) => match raft.append_entries(req).await {
            Ok(resp) => RaftRpcResponse::AppendEntries(resp),
            Err(_) => return false,
        },
        RaftRpc::Vote(req) => match raft.vote(req).await {
            Ok(resp) => RaftRpcResponse::Vote(resp),
            Err(_) => return false,
        },
        RaftRpc::InstallSnapshot(req) => match raft.install_snapshot(req).await {
            Ok(resp) => RaftRpcResponse::InstallSnapshot(resp),
            Err(_) => return false,
        },
    };

    write_response(stream, response).await
}

/// Write one enveloped response frame. Returns whether it was written; a
/// failed write only loses this RPC and closes the connection.
async fn write_response<S: AsyncWrite + Unpin>(stream: &mut S, response: RaftRpcResponse) -> bool {
    match serde_json::to_vec(&RaftResponseEnvelope {
        compatibility: crate::compatibility::CURRENT,
        response,
    }) {
        Ok(bytes) => write_frame(stream, &bytes).await.is_ok(),
        Err(_) => false,
    }
}

/// The material needed to build a per-target, node-id-bound mTLS connector
/// (PKI3): this node's identity (CA pins + client cert/key) and the shared,
/// live CRL handle. Held by the factory so `new_client` can bind each peer
/// connection to the specific node id it is dialling.
#[derive(Clone)]
pub struct RaftTlsMaterial {
    identity: crate::sesame::credentials::LiveNodeIdentity,
    crl: crate::sesame::mtls::CrlHandle,
}

impl RaftTlsMaterial {
    /// Bundle a node identity and CRL handle for node-id-bound dialling.
    pub fn new(
        identity: crate::sesame::credentials::LiveNodeIdentity,
        crl: crate::sesame::mtls::CrlHandle,
    ) -> Self {
        Self { identity, crl }
    }

    /// Build a connector that binds the handshake to `expected_node_id`.
    fn connector_for(&self, expected_node_id: &str) -> Option<tokio_rustls::TlsConnector> {
        let config = crate::sesame::mtls::build_live_mtls_client_config(
            &self.identity,
            self.crl.clone(),
            Some(expected_node_id),
        )
        .ok()?;
        Some(tokio_rustls::TlsConnector::from(config))
    }
}

/// Creates TCP-based Raft network connections.
///
/// Supports a runtime blocklist for chaos testing.
#[derive(Clone)]
pub struct TcpRaftNetworkFactory {
    #[allow(dead_code)]
    source_id: u64,
    blocklist: std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<SocketAddr>>>,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
    /// When set, peers are dialled over mTLS with an unbound connector (CA-pin
    /// + CRL only). Kept for callers that don't thread an identity through.
    tls: Option<tokio_rustls::TlsConnector>,
    /// When set, each peer connection is dialled over a node-id-bound
    /// connector built from this material (PKI3). Takes precedence over `tls`.
    tls_material: Option<RaftTlsMaterial>,
    /// This node's recovery fence: the epoch stamped on every outgoing RPC,
    /// and whether this node may send Raft RPCs at all (C5, #424).
    fence: RecoveryFence,
    /// Established connections to peers, shared by every client this
    /// factory hands out (#928).
    pool: RaftConnectionPool,
}

impl fmt::Debug for TcpRaftNetworkFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpRaftNetworkFactory")
            .field("source_id", &self.source_id)
            .field("tls", &(self.tls.is_some() || self.tls_material.is_some()))
            .finish()
    }
}

impl TcpRaftNetworkFactory {
    /// Create a new plaintext factory for a specific source node. It serves
    /// recovery epoch 0; use [`with_fence`](Self::with_fence) to share the
    /// node's real recovery fence.
    pub fn new(source_id: u64) -> Self {
        Self {
            source_id,
            blocklist: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            node_gate: crate::smoker::node_fault::NodeTransportGate::new(),
            tls: None,
            tls_material: None,
            fence: RecoveryFence::serving(0),
            pool: RaftConnectionPool::default(),
        }
    }

    /// Create a factory that dials peers over mTLS with a single shared
    /// (unbound) connector.
    pub fn new_tls(source_id: u64, connector: tokio_rustls::TlsConnector) -> Self {
        Self {
            source_id,
            blocklist: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            node_gate: crate::smoker::node_fault::NodeTransportGate::new(),
            tls: Some(connector),
            tls_material: None,
            fence: RecoveryFence::serving(0),
            pool: RaftConnectionPool::default(),
        }
    }

    /// Create a factory that dials each peer over a node-id-bound mTLS
    /// connector (PKI3), built from this node's identity + the live CRL.
    pub fn new_tls_bound(source_id: u64, material: RaftTlsMaterial) -> Self {
        Self {
            source_id,
            blocklist: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            node_gate: crate::smoker::node_fault::NodeTransportGate::new(),
            tls: None,
            tls_material: Some(material),
            fence: RecoveryFence::serving(0),
            pool: RaftConnectionPool::default(),
        }
    }

    /// Share this node's recovery fence (C5, #424): its epoch is stamped on
    /// every RPC, a fenced or probing node sends none, and a peer's fenced
    /// reply naming a newer epoch fences this node.
    pub fn with_fence(mut self, fence: RecoveryFence) -> Self {
        self.fence = fence;
        self
    }

    /// Share the reversible node-fault gate used by every cluster transport.
    pub fn with_node_gate(
        mut self,
        node_gate: crate::smoker::node_fault::NodeTransportGate,
    ) -> Self {
        self.node_gate = node_gate;
        self
    }

    /// Get a handle to the blocklist for chaos injection.
    pub fn blocklist(
        &self,
    ) -> std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<SocketAddr>>> {
        std::sync::Arc::clone(&self.blocklist)
    }
}

impl RaftNetworkFactory<TypeConfig> for TcpRaftNetworkFactory {
    type Network = TcpRaftNetwork;

    async fn new_client(&mut self, _target: u64, node: &CouncilNodeInfo) -> Self::Network {
        // Prefer a node-id-bound connector built for this exact peer (PKI3);
        // fall back to the shared unbound connector, then plaintext.
        let tls = match &self.tls_material {
            Some(material) => material.connector_for(&node.name),
            None => self.tls.clone(),
        };
        TcpRaftNetwork {
            target_addr: node.addr,
            peer: PeerKey {
                address: node.addr,
                node_name: node.name.clone(),
            },
            blocklist: std::sync::Arc::clone(&self.blocklist),
            node_gate: self.node_gate.clone(),
            tls,
            identity: self
                .tls_material
                .as_ref()
                .map(|material| material.identity.clone()),
            crl: self
                .tls_material
                .as_ref()
                .map(|material| material.crl.clone()),
            fence: self.fence.clone(),
            pool: self.pool.clone(),
        }
    }
}

/// A client for one Raft peer (plain TCP or mTLS). It sends each RPC over a
/// pooled connection when one is fit for reuse, and dials otherwise (#928).
pub struct TcpRaftNetwork {
    target_addr: SocketAddr,
    peer: PeerKey,
    blocklist: std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<SocketAddr>>>,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
    tls: Option<tokio_rustls::TlsConnector>,
    /// This node's live identity, when dialling with one: a pooled connection
    /// made under an older identity is retired.
    identity: Option<crate::sesame::credentials::LiveNodeIdentity>,
    /// The live CRL, when dialling with one: a pooled connection to a peer
    /// whose certificate is now revoked is retired.
    crl: Option<crate::sesame::mtls::CrlHandle>,
    /// This node's recovery fence (C5, #424).
    fence: RecoveryFence,
    /// The factory's connection pool.
    pool: RaftConnectionPool,
}

impl fmt::Debug for TcpRaftNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpRaftNetwork")
            .field("target_addr", &self.target_addr)
            .finish()
    }
}

impl TcpRaftNetwork {
    /// Send an RPC and read the response.
    ///
    /// The entire operation (any connect and handshake, write and read) is
    /// wrapped in a 10-second timeout to prevent hangs from slow or stalled
    /// peers. openraft also drops this future at its own, shorter deadlines;
    /// that is safe, because the connection in use is dropped with it.
    async fn rpc(&self, rpc: RaftRpc) -> Result<RaftRpcResponse, Unreachable> {
        if self.node_gate.is_quiesced() {
            return Err(Unreachable::new(&RouterError(
                "node transport quiesced".into(),
            )));
        }
        // Check blocklist for chaos testing
        if self.blocklist.read().await.contains(&self.target_addr) {
            return Err(Unreachable::new(&RouterError(
                "blocked by chaos partition".into(),
            )));
        }

        let Some(claim) = self.fence.outbound_claim() else {
            return Err(Unreachable::new(&RouterError(
                "recovery fence: this node is not serving raft".into(),
            )));
        };

        // Wrap the entire RPC in a timeout — covers connect, write, and read.
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.rpc_inner(rpc, claim),
        )
        .await
        .map_err(|_| Unreachable::new(&RouterError("rpc timeout (10s)".into())))??;
        if let RaftRpcResponse::Fenced {
            recovery_epoch,
            recovery_id,
        } = response
        {
            let peer = RecoveryClaim {
                epoch: recovery_epoch,
                id: recovery_id,
            };
            if self.fence.observe_peer(peer) {
                eprintln!(
                    "raft: fenced: peer {} serves recovery epoch {recovery_epoch}, and this \
                     node's council (epoch {}) is either older or a different recovery of the \
                     same epoch",
                    self.target_addr, claim.epoch
                );
            }
            return Err(Unreachable::new(&RouterError(format!(
                "peer refused the rpc under the recovery fence (its epoch {recovery_epoch})"
            ))));
        }
        Ok(response)
    }

    /// Inner RPC implementation (called within a timeout).
    ///
    /// Tries a pooled connection first. If that fails (most often because
    /// the peer closed it while it sat idle), the RPC is sent once more on a
    /// freshly dialled connection. Sending a Raft RPC twice is safe: append,
    /// vote and snapshot requests are all idempotent at the receiver.
    async fn rpc_inner(
        &self,
        rpc: RaftRpc,
        claim: RecoveryClaim,
    ) -> Result<RaftRpcResponse, Unreachable> {
        // JSON to match handle_raft_frame (self-describing; see there). The RPC
        // is wrapped in a `RaftRpcEnvelope` stamped with this node's recovery
        // epoch (C5) so the accept side can fence off a different-epoch peer.
        let envelope = RaftRpcEnvelope {
            compatibility: crate::compatibility::CURRENT,
            sender_recovery_epoch: claim.epoch,
            sender_recovery_id: claim.id,
            rpc,
        };
        let payload = serde_json::to_vec(&envelope)
            .map_err(|e| Unreachable::new(&RouterError(format!("serialize: {e}"))))?;

        let check = ReuseCheck {
            current_identity: self.identity.as_ref().map(|identity| identity.snapshot()),
            crl: self.crl.as_ref(),
        };
        if let Some(mut connection) = self.pool.take(&self.peer, &check).await
            && let Ok(response) = Self::exchange(&mut connection.stream, &payload).await
        {
            self.release(connection, &response).await;
            return Ok(response);
        }
        // No usable pooled connection, or it broke: it was dropped (and so
        // closed) above. Dial a new one.
        let mut connection = self.dial().await?;
        let response = Self::exchange(&mut connection.stream, &payload).await?;
        self.release(connection, &response).await;
        Ok(response)
    }

    /// Put a connection back after a complete request and response. A
    /// fenced peer closes the connection after refusing, so that one isn't
    /// kept.
    async fn release(&self, connection: RaftConnection, response: &RaftRpcResponse) {
        if !matches!(response, RaftRpcResponse::Fenced { .. }) {
            self.pool.put(&self.peer, connection).await;
        }
    }

    /// Open a new connection to the peer, with an mTLS handshake when the
    /// transport has a connector.
    async fn dial(&self) -> Result<RaftConnection, Unreachable> {
        // Note the identity before the handshake. If it rotates mid-handshake
        // the connection is marked with the old one and retired at its next
        // use, never kept with a certificate older than the mark.
        let presented = self.identity.as_ref().map(|identity| identity.snapshot());
        let tcp = tokio::net::TcpStream::connect(self.target_addr)
            .await
            .map_err(|e| Unreachable::new(&RouterError(format!("connect: {e}"))))?;
        // Small, latency-bound frames: don't let Nagle's algorithm hold one
        // back. Failing to set it only costs latency.
        let _ = tcp.set_nodelay(true);

        match &self.tls {
            Some(connector) => {
                // The pinned server verifier ignores the name, but rustls
                // still requires a syntactically valid one.
                let server_name =
                    rustls::pki_types::ServerName::IpAddress(self.target_addr.ip().into());
                let stream = connector
                    .connect(server_name, tcp)
                    .await
                    .map_err(|e| Unreachable::new(&RouterError(format!("tls: {e}"))))?;
                let peer_chain = stream
                    .get_ref()
                    .1
                    .peer_certificates()
                    .map(|chain| chain.to_vec())
                    .unwrap_or_default();
                Ok(RaftConnection::new(Box::new(stream), presented, peer_chain))
            }
            None => Ok(RaftConnection::new(Box::new(tcp), presented, Vec::new())),
        }
    }

    /// Write one frame and read the response over an established stream.
    ///
    /// The stream is borrowed (`&mut S`), not consumed, so the caller keeps
    /// the connection for its next RPC.
    async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        payload: &[u8],
    ) -> Result<RaftRpcResponse, Unreachable> {
        write_frame(stream, payload)
            .await
            .map_err(|e| Unreachable::new(&RouterError(format!("write: {e}"))))?;

        let resp_payload = read_frame(stream)
            .await
            .ok_or_else(|| Unreachable::new(&RouterError("read response failed".into())))?;

        let envelope: RaftResponseEnvelope = serde_json::from_slice(&resp_payload)
            .map_err(|e| Unreachable::new(&RouterError(format!("deserialize: {e}"))))?;
        envelope
            .compatibility
            .require_current()
            .map_err(|e| Unreachable::new(&RouterError(e.to_string())))?;
        Ok(envelope.response)
    }
}

impl RaftNetwork<TypeConfig> for TcpRaftNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, CouncilNodeInfo, RaftError<u64>>> {
        match self.rpc(RaftRpc::AppendEntries(rpc)).await {
            Ok(RaftRpcResponse::AppendEntries(resp)) => Ok(resp),
            Ok(_) => Err(RPCError::Unreachable(Unreachable::new(&RouterError(
                "unexpected response type".into(),
            )))),
            Err(e) => Err(RPCError::Unreachable(e)),
        }
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, CouncilNodeInfo, RaftError<u64, InstallSnapshotError>>,
    > {
        match self.rpc(RaftRpc::InstallSnapshot(rpc)).await {
            Ok(RaftRpcResponse::InstallSnapshot(resp)) => Ok(resp),
            Ok(_) => Err(RPCError::Unreachable(Unreachable::new(&RouterError(
                "unexpected response type".into(),
            )))),
            Err(e) => Err(RPCError::Unreachable(e)),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<u64>,
        _option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, CouncilNodeInfo, RaftError<u64>>> {
        match self.rpc(RaftRpc::Vote(rpc)).await {
            Ok(RaftRpcResponse::Vote(resp)) => Ok(resp),
            Ok(_) => Err(RPCError::Unreachable(Unreachable::new(&RouterError(
                "unexpected response type".into(),
            )))),
            Err(e) => Err(RPCError::Unreachable(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn router_registers_and_routes() {
        let router = InMemoryRaftRouter::new();

        // Unknown target returns Unreachable.
        let err = router.lookup(1, 99).await.err().unwrap();
        assert!(err.to_string().contains("unknown target"));
    }

    #[tokio::test]
    async fn partition_blocks_messages() {
        let router = InMemoryRaftRouter::new();
        router.partition(1, 2).await;

        assert!(router.is_partitioned(1, 2).await);
        assert!(router.is_partitioned(2, 1).await);
        assert!(!router.is_partitioned(1, 3).await);

        let err = router.lookup(1, 2).await.err().unwrap();
        assert!(err.to_string().contains("partitioned"));
    }

    #[tokio::test]
    async fn heal_restores_connectivity() {
        let router = InMemoryRaftRouter::new();
        router.partition(1, 2).await;
        assert!(router.is_partitioned(1, 2).await);

        router.heal().await;
        assert!(!router.is_partitioned(1, 2).await);
        assert!(!router.is_partitioned(2, 1).await);
    }

    #[tokio::test]
    async fn node_transport_gate_refuses_outbound_raft_rpc() {
        let gate = crate::smoker::node_fault::NodeTransportGate::new();
        let network = TcpRaftNetworkFactory::new(1)
            .with_node_gate(gate.clone())
            .new_client(2, &peer_info("127.0.0.1:9".parse().unwrap()))
            .await;
        gate.quiesce();
        let request = VoteRequest::new(openraft::Vote::new(1, 7), None);

        let error = match network.rpc(RaftRpc::Vote(request)).await {
            Ok(_) => panic!("quiesced node must not dial Raft peers"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("node transport quiesced"));
    }

    #[tokio::test]
    async fn unknown_target_returns_error() {
        let router = InMemoryRaftRouter::new();
        let result = router.lookup(1, 42).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_frame_never_completes_on_a_stalled_half_frame() {
        // CP11: a peer that sends 2 of the 4 length bytes then stalls would
        // hang read_frame forever without a deadline. Under a short timeout
        // (standing in for RAFT_ACCEPT_DEADLINE) the read is abandoned.
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(64);
        // Write half a length prefix, then hold the connection open by never
        // dropping `client` and never writing more.
        client.write_all(&[0u8, 0u8]).await.unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            read_frame(&mut server),
        )
        .await;
        assert!(
            result.is_err(),
            "a stalled half-frame read must be cut off by the deadline, not return"
        );
    }

    /// O6: a frame spanning several chunks must reassemble byte for byte —
    /// the whole point of the chunked read is that it can't lose or reorder
    /// anything while it grows.
    #[tokio::test]
    async fn read_frame_reassembles_a_multi_chunk_payload() {
        use tokio::io::AsyncWriteExt;

        let payload: Vec<u8> = (0..FRAME_CHUNK_SIZE * 2 + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        let (mut client, mut server) = tokio::io::duplex(4096);
        let writer = tokio::spawn(async move {
            client
                .write_all(&(payload.len() as u32).to_be_bytes())
                .await
                .unwrap();
            client.write_all(&payload).await.unwrap();
            payload
        });

        let read = read_frame(&mut server).await.expect("frame");
        let written = writer.await.unwrap();
        assert_eq!(read, written);
    }

    /// O6: the length prefix is attacker-controlled. Declaring the maximum
    /// and then sending almost nothing must fail on the short read rather
    /// than reserving 64 MiB per connection first.
    #[tokio::test]
    async fn read_frame_does_not_trust_a_declared_length() {
        use tokio::io::AsyncWriteExt;

        let (mut client, mut server) = tokio::io::duplex(1024);
        client
            .write_all(&(MAX_RAFT_RPC_SIZE as u32).to_be_bytes())
            .await
            .unwrap();
        client.write_all(b"ten bytes!").await.unwrap();
        drop(client); // EOF: the rest never arrives.

        assert!(read_frame(&mut server).await.is_none());
    }

    // -- recovery-epoch fence (C5) -------------------------------------------

    fn vote_envelope(sender_recovery_epoch: u64) -> RaftRpcEnvelope {
        RaftRpcEnvelope {
            compatibility: crate::compatibility::CURRENT,
            sender_recovery_epoch,
            sender_recovery_id: None,
            rpc: RaftRpc::Vote(VoteRequest::new(
                openraft::Vote::new(1, 7),
                Some(openraft::LogId::new(
                    openraft::CommittedLeaderId::new(1, 7),
                    0,
                )),
            )),
        }
    }

    #[test]
    fn decode_and_admit_refuses_incompatible_formats() {
        let envelope = vote_envelope(0);
        for field in ["protocol", "state"] {
            let mut wrong = serde_json::to_value(&envelope).unwrap();
            wrong["compatibility"][field] = serde_json::json!(99);
            let fence = RecoveryFence::serving(0);
            assert!(decode_and_admit(&serde_json::to_vec(&wrong).unwrap(), &fence).is_none());
        }
    }

    #[test]
    fn a_recovered_node_refuses_an_old_voter_and_names_its_epoch() {
        let payload = serde_json::to_vec(&vote_envelope(0)).unwrap();
        let recovered = RecoveryFence::serving(1);
        let (admission, _) = decode_and_admit(&payload, &recovered).unwrap();
        assert_eq!(
            admission,
            Admission::Refuse {
                newest: RecoveryClaim { epoch: 1, id: None }
            }
        );
        assert!(!recovered.is_fenced());
    }

    #[test]
    fn an_old_voter_contacted_by_a_recovered_node_fences_itself() {
        let payload = serde_json::to_vec(&vote_envelope(1)).unwrap();
        let old = RecoveryFence::serving(0);
        let (admission, _) = decode_and_admit(&payload, &old).unwrap();
        assert_eq!(
            admission,
            Admission::Refuse {
                newest: RecoveryClaim { epoch: 1, id: None }
            }
        );
        assert_eq!(old.snapshot().fenced_by(), Some(1));
    }

    #[test]
    fn same_epoch_rpcs_reach_raft() {
        let payload = serde_json::to_vec(&vote_envelope(0)).unwrap();
        let (admission, rpc) = decode_and_admit(&payload, &RecoveryFence::serving(0)).unwrap();
        assert_eq!(admission, Admission::Serve);
        assert!(matches!(rpc, RaftRpc::Vote(_)));
    }

    /// The #424 split brain, at the transport: an old voter whose peer has
    /// already been fenced must not get a vote out of it, and must come away
    /// fenced too.
    #[tokio::test]
    async fn a_fenced_peer_refuses_and_fences_the_caller() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fenced_peer = RecoveryFence::new(
            crate::council::fence::FenceSnapshot {
                epoch: 0,
                recovery_id: None,
                state: crate::council::fence::FenceState::Fenced { newer_epoch: 1 },
            },
            None,
        );
        let server_fence = fenced_peer.clone();
        // Serve the refusal by hand: a fenced peer never reaches Raft, so the
        // handler's refusal path needs no `Raft` instance.
        let refusal = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let payload = read_frame(&mut stream).await.unwrap();
            let (admission, _) = decode_and_admit(&payload, &server_fence).unwrap();
            let Admission::Refuse { newest } = admission else {
                panic!("a fenced peer must refuse, got {admission:?}");
            };
            write_response(
                &mut stream,
                RaftRpcResponse::Fenced {
                    recovery_epoch: newest.epoch,
                    recovery_id: newest.id,
                },
            )
            .await;
        });

        let caller = RecoveryFence::serving(0);
        let network = TcpRaftNetworkFactory::new(1)
            .with_fence(caller.clone())
            .new_client(2, &peer_info(address))
            .await;
        let request = VoteRequest::new(openraft::Vote::new(1, 7), None);
        assert!(network.rpc(RaftRpc::Vote(request)).await.is_err());
        refusal.await.unwrap();
        assert_eq!(caller.snapshot().fenced_by(), Some(1));

        // Once fenced, the caller sends nothing at all.
        let request = VoteRequest::new(openraft::Vote::new(1, 7), None);
        let error = match network.rpc(RaftRpc::Vote(request)).await {
            Ok(_) => panic!("a fenced node must not send raft rpcs"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("recovery fence"));
    }

    #[test]
    fn incompatible_or_missing_formats_are_refused_before_raft() {
        let request = serde_json::json!({
            "sender_recovery_epoch": 0,
            "rpc": RaftRpc::Vote(VoteRequest::new(openraft::Vote::new(1, 7), None)),
        });
        let fence = RecoveryFence::serving(0);
        assert!(decode_and_admit(&serde_json::to_vec(&request).unwrap(), &fence).is_none());
    }

    #[test]
    fn decode_and_admit_rejects_a_malformed_frame() {
        assert!(decode_and_admit(b"not json", &RecoveryFence::serving(0)).is_none());
    }

    // -----------------------------------------------------------------------
    // Connection reuse (#928)
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use crate::sesame::ca::{self, CaHierarchy};
    use crate::sesame::credentials::LiveNodeIdentity;
    use crate::sesame::identity_store::{self, NodeIdentity};
    use crate::sesame::mtls::CrlHandle;
    use crate::sesame::types::SerialNumber;

    fn node_identity(hierarchy: &CaHierarchy, node_id: &str, serial: u64) -> NodeIdentity {
        let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
            node_id,
            SerialNumber(serial),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        let now = SystemTime::now();
        NodeIdentity {
            node_id: node_id.to_string(),
            certificate_der,
            private_key_der,
            serial,
            ca_generation: 0,
            trust: crate::sesame::trust::TrustSet::single(
                hierarchy.node.ca.certificate_der.clone(),
                hierarchy.root.ca.certificate_der.clone(),
            ),
            node_ca_der: hierarchy.node.ca.certificate_der.clone(),
            root_ca_der: hierarchy.root.ca.certificate_der.clone(),
            not_before: now - Duration::from_secs(60),
            not_after: now + Duration::from_secs(24 * 3600),
        }
    }

    fn live_identity(
        hierarchy: &CaHierarchy,
        node_id: &str,
        serial: u64,
    ) -> (tempfile::TempDir, LiveNodeIdentity) {
        let directory = tempfile::tempdir().unwrap();
        identity_store::save(directory.path(), &node_identity(hierarchy, node_id, serial)).unwrap();
        let live = LiveNodeIdentity::load(directory.path()).unwrap();
        (directory, live)
    }

    /// A stand-in Raft peer that grants every vote and records what it saw.
    struct StubPeer {
        address: SocketAddr,
        /// Connections accepted (and, over mTLS, handshakes completed).
        connections: Arc<AtomicUsize>,
        /// The serial of the client certificate each mTLS handshake presented.
        client_serials: Arc<std::sync::Mutex<Vec<u64>>>,
    }

    /// How the stub treats each connection after accepting it.
    #[derive(Clone, Copy)]
    enum StubBehaviour {
        /// Answer every request until the client closes the connection.
        KeepOpen,
        /// Answer one request, then close the connection.
        OneRequestPerConnection,
        /// On the first connection only, answer the first request late.
        FirstAnswerLate(Duration),
    }

    async fn answer_votes<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        behaviour: StubBehaviour,
        first_connection: bool,
    ) {
        while let Some(payload) = read_frame(&mut stream).await {
            let (_, rpc) = decode_and_admit(&payload, &RecoveryFence::serving(0)).unwrap();
            let RaftRpc::Vote(request) = rpc else {
                panic!("the stub only answers votes");
            };
            if let (StubBehaviour::FirstAnswerLate(delay), true) = (behaviour, first_connection) {
                tokio::time::sleep(delay).await;
            }
            let response = VoteResponse::new(request.vote, None, true);
            write_response(&mut stream, RaftRpcResponse::Vote(response)).await;
            if matches!(behaviour, StubBehaviour::OneRequestPerConnection) {
                return;
            }
        }
    }

    async fn stub_peer(
        behaviour: StubBehaviour,
        tls: Option<(LiveNodeIdentity, CrlHandle)>,
    ) -> StubPeer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let client_serials = Arc::new(std::sync::Mutex::new(Vec::new()));
        let acceptor = tls.map(|(identity, crl)| {
            tokio_rustls::TlsAcceptor::from(
                crate::sesame::mtls::build_live_mtls_server_config(&identity, crl).unwrap(),
            )
        });
        let accepted = Arc::clone(&connections);
        let serials = Arc::clone(&client_serials);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let accepted = Arc::clone(&accepted);
                let serials = Arc::clone(&serials);
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor {
                        Some(acceptor) => {
                            let Ok(stream) = acceptor.accept(stream).await else {
                                return;
                            };
                            let leaf = stream.get_ref().1.peer_certificates().unwrap()[0].clone();
                            serials
                                .lock()
                                .unwrap()
                                .push(crate::sesame::cert::serial_from_der(&leaf).unwrap().0);
                            let first = accepted.fetch_add(1, Ordering::SeqCst) == 0;
                            answer_votes(stream, behaviour, first).await;
                        }
                        None => {
                            let first = accepted.fetch_add(1, Ordering::SeqCst) == 0;
                            answer_votes(stream, behaviour, first).await;
                        }
                    }
                });
            }
        });
        StubPeer {
            address,
            connections,
            client_serials,
        }
    }

    fn peer_info(address: SocketAddr) -> CouncilNodeInfo {
        CouncilNodeInfo {
            addr: address,
            name: "n2".into(),
        }
    }

    async fn vote(network: &mut TcpRaftNetwork) -> bool {
        let request = VoteRequest::new(openraft::Vote::new(1, 1), None);
        network
            .vote(request, RPCOption::new(Duration::from_secs(5)))
            .await
            .map(|response| response.vote_granted)
            .unwrap_or(false)
    }

    /// The #928 done-when: heartbeats and appends to one peer reuse one
    /// connection instead of paying a TCP and mTLS handshake each, and so
    /// does every client the factory hands out (openraft asks for a new one
    /// per replication stream and per vote).
    #[tokio::test]
    async fn many_rpcs_to_one_peer_make_one_handshake() {
        let hierarchy = ca::generate_ca_hierarchy("raft-reuse", b"ikm").unwrap();
        let (_server_dir, server) = live_identity(&hierarchy, "n2", 20);
        let (_client_dir, client) = live_identity(&hierarchy, "n1", 10);
        let peer = stub_peer(
            StubBehaviour::KeepOpen,
            Some((server, CrlHandle::default())),
        )
        .await;
        let mut factory = TcpRaftNetworkFactory::new_tls_bound(
            1,
            RaftTlsMaterial::new(client, CrlHandle::default()),
        );

        let mut network = factory.new_client(2, &peer_info(peer.address)).await;
        for _ in 0..20 {
            assert!(vote(&mut network).await);
        }
        let mut another = factory.new_client(2, &peer_info(peer.address)).await;
        assert!(vote(&mut another).await);

        assert_eq!(peer.connections.load(Ordering::SeqCst), 1);
    }

    /// The other #928 done-when: a renewed certificate is presented on the
    /// next connection, and the pool doesn't keep using one authenticated
    /// with the old certificate.
    #[tokio::test]
    async fn a_reconnect_after_rotation_presents_the_new_certificate() {
        let hierarchy = ca::generate_ca_hierarchy("raft-rotate", b"ikm").unwrap();
        let (_server_dir, server) = live_identity(&hierarchy, "n2", 20);
        let (_client_dir, client) = live_identity(&hierarchy, "n1", 10);
        let peer = stub_peer(
            StubBehaviour::KeepOpen,
            Some((server, CrlHandle::default())),
        )
        .await;
        let mut factory = TcpRaftNetworkFactory::new_tls_bound(
            1,
            RaftTlsMaterial::new(client.clone(), CrlHandle::default()),
        );
        let mut network = factory.new_client(2, &peer_info(peer.address)).await;
        assert!(vote(&mut network).await);
        assert!(vote(&mut network).await);

        let renewed = node_identity(&hierarchy, "n1", 11);
        let trust = renewed.trust.clone();
        client.replace(renewed, &trust).await.unwrap();
        assert!(vote(&mut network).await);
        assert!(vote(&mut network).await);

        assert_eq!(*peer.client_serials.lock().unwrap(), vec![10, 11]);
    }

    /// Revocation still takes effect at the next RPC: a pooled connection to
    /// a peer whose certificate is now on the CRL is not reused, and the new
    /// handshake refuses the peer.
    #[tokio::test]
    async fn a_pooled_connection_to_a_revoked_peer_is_not_reused() {
        let hierarchy = ca::generate_ca_hierarchy("raft-revoke", b"ikm").unwrap();
        let (_server_dir, server) = live_identity(&hierarchy, "n2", 20);
        let (_client_dir, client) = live_identity(&hierarchy, "n1", 10);
        let peer = stub_peer(
            StubBehaviour::KeepOpen,
            Some((server, CrlHandle::default())),
        )
        .await;
        let crl = CrlHandle::default();
        let mut factory =
            TcpRaftNetworkFactory::new_tls_bound(1, RaftTlsMaterial::new(client, crl.clone()));
        let mut network = factory.new_client(2, &peer_info(peer.address)).await;
        assert!(vote(&mut network).await);

        crl.update(crate::sesame::types::Crl {
            retired_nodes: Default::default(),
            entries: vec![crate::sesame::types::CrlEntry {
                serial: SerialNumber(20),
                issuer: crate::sesame::types::CaRole::Node,
                revoked_at: SystemTime::now(),
                reason: "test".to_string(),
                expires_at: None,
            }],
            version: 1,
            updated_at: SystemTime::now(),
        });
        assert!(
            !vote(&mut network).await,
            "a revoked peer must not be reached"
        );
        assert_eq!(peer.connections.load(Ordering::SeqCst), 1);
    }

    /// A pooled connection the peer has since closed costs a reconnect, not
    /// a failed RPC.
    #[tokio::test]
    async fn a_connection_the_peer_closed_is_replaced_transparently() {
        let peer = stub_peer(StubBehaviour::OneRequestPerConnection, None).await;
        let mut factory = TcpRaftNetworkFactory::new(1);
        let mut network = factory.new_client(2, &peer_info(peer.address)).await;
        for _ in 0..3 {
            assert!(vote(&mut network).await);
        }
        assert_eq!(peer.connections.load(Ordering::SeqCst), 3);
    }

    /// Cancellation safety: an RPC dropped while its request is in flight
    /// (openraft does this at its own deadline) must not hand that
    /// connection to the next RPC, which would then read the late answer
    /// meant for the cancelled one.
    #[tokio::test]
    async fn a_cancelled_rpc_does_not_return_its_connection_to_the_pool() {
        let peer = stub_peer(
            StubBehaviour::FirstAnswerLate(Duration::from_millis(300)),
            None,
        )
        .await;
        let mut factory = TcpRaftNetworkFactory::new(1);
        let mut network = factory.new_client(2, &peer_info(peer.address)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), vote(&mut network))
                .await
                .is_err(),
            "the first answer is late, so the first RPC is cancelled"
        );
        assert!(vote(&mut network).await);
        assert_eq!(peer.connections.load(Ordering::SeqCst), 2);
    }
}
