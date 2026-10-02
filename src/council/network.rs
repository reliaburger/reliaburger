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

use super::fence::{Admission, RecoveryFence};
use super::types::{CouncilNodeInfo, TypeConfig};

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

/// How long the accept side waits for a peer to finish its mTLS handshake and
/// deliver a complete frame before dropping the connection (CP11). A peer that
/// connects and stalls (half-open handshake, partial length prefix) must not
/// pin the task forever.
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
    Some((fence.admit(envelope.sender_recovery_epoch), envelope.rpc))
}

/// Raft RPC response envelope, serialised over TCP.
#[derive(Serialize, Deserialize)]
pub enum RaftRpcResponse {
    AppendEntries(AppendEntriesResponse<u64>),
    Vote(VoteResponse<u64>),
    InstallSnapshot(InstallSnapshotResponse<u64>),
    /// The peer refused the RPC under the recovery fence. `recovery_epoch` is
    /// the newest epoch it knows; a sender holding an older one fences itself.
    Fenced {
        recovery_epoch: u64,
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
    let len_bytes = (data.len() as u32).to_be_bytes();
    stream
        .write_all(&len_bytes)
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(data).await.map_err(|e| e.to_string())?;
    // TLS buffers until flushed; a plain TCP stream flushes on write.
    stream.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Serve Raft RPCs over TCP, optionally wrapped in mTLS.
///
/// Accepts connections on `listener`, reads one RPC per connection,
/// dispatches to the local `raft` instance, and writes the response.
/// When `tls` is `Some`, every connection must complete an mTLS handshake
/// (the peer presents a client certificate) before its frame is read.
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
    )
    .await
}

/// Serve Raft RPCs while observing the reversible node-level fault gate.
pub async fn serve_raft_rpc_with_node_gate(
    listener: tokio::net::TcpListener,
    raft: Raft<TypeConfig>,
    shutdown: tokio_util::sync::CancellationToken,
    tls: Option<tokio_rustls::TlsAcceptor>,
    fence: RecoveryFence,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
) {
    serve_raft_rpc_with_limit_and_node_gate(
        listener,
        raft,
        shutdown,
        tls,
        fence,
        MAX_RAFT_CONNECTIONS,
        node_gate,
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
    )
    .await
}

/// Connection-limited Raft server with a shared node-level fault gate.
pub async fn serve_raft_rpc_with_limit_and_node_gate(
    listener: tokio::net::TcpListener,
    raft: Raft<TypeConfig>,
    shutdown: tokio_util::sync::CancellationToken,
    tls: Option<tokio_rustls::TlsAcceptor>,
    fence: RecoveryFence,
    max_connections: usize,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
) {
    // O6: hold a permit for the whole lifetime of a connection. Acquiring
    // *before* accepting means excess peers queue in the kernel's backlog
    // instead of each costing a task and a buffer. Every permit is released
    // within RAFT_ACCEPT_DEADLINE, so a stalled peer can't hold one forever.
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
                        let raft = raft.clone();
                        let fence = fence.clone();
                        let connection_gate = node_gate.clone();
                        match tls.clone() {
                            Some(acceptor) => {
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    // A failed handshake (no/invalid/revoked
                                    // client cert) is dropped silently — a
                                    // rejected peer must not learn why. The
                                    // whole handshake + frame exchange is under
                                    // one deadline so a stalled peer can't pin
                                    // the task (CP11).
                                    let _ = tokio::time::timeout(RAFT_ACCEPT_DEADLINE, async {
                                        if let Ok(tls_stream) = acceptor.accept(stream).await
                                            && !connection_gate.is_quiesced()
                                        {
                                            handle_raft_rpc(tls_stream, raft, fence)
                                                .await;
                                        }
                                    })
                                    .await;
                                });
                            }
                            None => {
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if !connection_gate.is_quiesced() {
                                        let _ = tokio::time::timeout(
                                            RAFT_ACCEPT_DEADLINE,
                                            handle_raft_rpc(stream, raft, fence),
                                        )
                                        .await;
                                    }
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

/// Handle a single Raft RPC connection over any byte stream.
async fn handle_raft_rpc<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    raft: Raft<TypeConfig>,
    fence: RecoveryFence,
) {
    let Some(payload) = read_frame(&mut stream).await else {
        return;
    };
    // JSON, not bincode: AppendEntries carries log entries whose
    // `AppSpec` payload uses `deserialize_any` config types (see
    // durable_log). bincode is not self-describing and corrupts them,
    // so a replicated AppSpec entry would never deserialise on the
    // follower and the write would hang forever.
    //
    // The frame is a `RaftRpcEnvelope`; the recovery fence (C5, #424) decides
    // whether it reaches Raft at all.
    let Some((admission, rpc)) = decode_and_admit(&payload, &fence) else {
        return;
    };
    match admission {
        Admission::Serve => {}
        Admission::Adopted => {
            // A fresh node joining a council: make the adopted epoch durable
            // before acting on it, or a crash could bring it back claiming
            // nothing with Raft state on disk.
            if let Err(error) = fence.persist().await {
                eprintln!("raft: could not persist the adopted recovery epoch: {error}");
                return;
            }
        }
        Admission::Refuse { newest_epoch } => {
            write_response(
                &mut stream,
                RaftRpcResponse::Fenced {
                    recovery_epoch: newest_epoch,
                },
            )
            .await;
            return;
        }
    }

    let response = match rpc {
        RaftRpc::AppendEntries(req) => match raft.append_entries(req).await {
            Ok(resp) => RaftRpcResponse::AppendEntries(resp),
            Err(_) => return,
        },
        RaftRpc::Vote(req) => match raft.vote(req).await {
            Ok(resp) => RaftRpcResponse::Vote(resp),
            Err(_) => return,
        },
        RaftRpc::InstallSnapshot(req) => match raft.install_snapshot(req).await {
            Ok(resp) => RaftRpcResponse::InstallSnapshot(resp),
            Err(_) => return,
        },
    };

    write_response(&mut stream, response).await;
}

/// Write one enveloped response frame; a failed write only loses this RPC.
async fn write_response<S: AsyncWrite + Unpin>(stream: &mut S, response: RaftRpcResponse) {
    if let Ok(bytes) = serde_json::to_vec(&RaftResponseEnvelope {
        compatibility: crate::compatibility::CURRENT,
        response,
    }) {
        let _ = write_frame(stream, &bytes).await;
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
            blocklist: std::sync::Arc::clone(&self.blocklist),
            node_gate: self.node_gate.clone(),
            tls,
            fence: self.fence.clone(),
        }
    }
}

/// A single connection to a Raft peer (plain TCP or mTLS).
pub struct TcpRaftNetwork {
    target_addr: SocketAddr,
    blocklist: std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<SocketAddr>>>,
    node_gate: crate::smoker::node_fault::NodeTransportGate,
    tls: Option<tokio_rustls::TlsConnector>,
    /// This node's recovery fence (C5, #424).
    fence: RecoveryFence,
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
    /// The entire operation (connect + write + read) is wrapped in a
    /// 10-second timeout to prevent hangs from slow or stalled peers.
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

        let Some(epoch) = self.fence.outbound_epoch() else {
            return Err(Unreachable::new(&RouterError(
                "recovery fence: this node is not serving raft".into(),
            )));
        };

        // Wrap the entire RPC in a timeout — covers connect, write, and read.
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.rpc_inner(rpc, epoch),
        )
        .await
        .map_err(|_| Unreachable::new(&RouterError("rpc timeout (10s)".into())))??;
        if let RaftRpcResponse::Fenced { recovery_epoch } = response {
            if self.fence.observe_peer_epoch(recovery_epoch) {
                eprintln!(
                    "raft: fenced: peer {} serves recovery epoch {recovery_epoch}, newer than \
                     this node's {epoch}; this node's council was replaced by a recovery",
                    self.target_addr
                );
            }
            return Err(Unreachable::new(&RouterError(format!(
                "peer refused the rpc under the recovery fence (its epoch {recovery_epoch})"
            ))));
        }
        Ok(response)
    }

    /// Inner RPC implementation (called within a timeout).
    async fn rpc_inner(&self, rpc: RaftRpc, epoch: u64) -> Result<RaftRpcResponse, Unreachable> {
        // JSON to match handle_raft_rpc (self-describing; see there). The RPC is
        // wrapped in a `RaftRpcEnvelope` stamped with this node's recovery epoch
        // (C5) so the accept side can fence off a different-epoch peer.
        let envelope = RaftRpcEnvelope {
            compatibility: crate::compatibility::CURRENT,
            sender_recovery_epoch: epoch,
            rpc,
        };
        let payload = serde_json::to_vec(&envelope)
            .map_err(|e| Unreachable::new(&RouterError(format!("serialize: {e}"))))?;

        let tcp = tokio::net::TcpStream::connect(self.target_addr)
            .await
            .map_err(|e| Unreachable::new(&RouterError(format!("connect: {e}"))))?;

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
                Self::exchange(stream, &payload).await
            }
            None => Self::exchange(tcp, &payload).await,
        }
    }

    /// Write one frame and read the response over an established stream.
    async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
        mut stream: S,
        payload: &[u8],
    ) -> Result<RaftRpcResponse, Unreachable> {
        write_frame(&mut stream, payload)
            .await
            .map_err(|e| Unreachable::new(&RouterError(format!("write: {e}"))))?;

        let resp_payload = read_frame(&mut stream)
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
        let network = TcpRaftNetwork {
            target_addr: "127.0.0.1:9".parse().unwrap(),
            blocklist: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            node_gate: gate.clone(),
            tls: None,
            fence: RecoveryFence::serving(0),
        };
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
        assert_eq!(admission, Admission::Refuse { newest_epoch: 1 });
        assert!(!recovered.is_fenced());
    }

    #[test]
    fn an_old_voter_contacted_by_a_recovered_node_fences_itself() {
        let payload = serde_json::to_vec(&vote_envelope(1)).unwrap();
        let old = RecoveryFence::serving(0);
        let (admission, _) = decode_and_admit(&payload, &old).unwrap();
        assert_eq!(admission, Admission::Refuse { newest_epoch: 1 });
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
            let Admission::Refuse { newest_epoch } = admission else {
                panic!("a fenced peer must refuse, got {admission:?}");
            };
            write_response(
                &mut stream,
                RaftRpcResponse::Fenced {
                    recovery_epoch: newest_epoch,
                },
            )
            .await;
        });

        let caller = RecoveryFence::serving(0);
        let network = TcpRaftNetwork {
            target_addr: address,
            blocklist: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            node_gate: crate::smoker::node_fault::NodeTransportGate::new(),
            tls: None,
            fence: caller.clone(),
        };
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
}
