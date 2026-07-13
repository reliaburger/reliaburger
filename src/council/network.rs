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

/// Raft RPC request envelope, serialised over TCP.
#[derive(Serialize, Deserialize)]
pub enum RaftRpc {
    AppendEntries(AppendEntriesRequest<TypeConfig>),
    Vote(VoteRequest<u64>),
    InstallSnapshot(InstallSnapshotRequest<TypeConfig>),
}

/// Raft RPC response envelope, serialised over TCP.
#[derive(Serialize, Deserialize)]
pub enum RaftRpcResponse {
    AppendEntries(AppendEntriesResponse<u64>),
    Vote(VoteResponse<u64>),
    InstallSnapshot(InstallSnapshotResponse<u64>),
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
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.ok()?;
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
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            result = listener.accept() => {
                match result {
                    Ok((stream, _peer)) => {
                        let raft = raft.clone();
                        match tls.clone() {
                            Some(acceptor) => {
                                tokio::spawn(async move {
                                    // A failed handshake (no/invalid/revoked
                                    // client cert) is dropped silently — a
                                    // rejected peer must not learn why. The
                                    // whole handshake + frame exchange is under
                                    // one deadline so a stalled peer can't pin
                                    // the task (CP11).
                                    let _ = tokio::time::timeout(RAFT_ACCEPT_DEADLINE, async {
                                        if let Ok(tls_stream) = acceptor.accept(stream).await {
                                            handle_raft_rpc(tls_stream, raft).await;
                                        }
                                    })
                                    .await;
                                });
                            }
                            None => {
                                tokio::spawn(async move {
                                    let _ = tokio::time::timeout(
                                        RAFT_ACCEPT_DEADLINE,
                                        handle_raft_rpc(stream, raft),
                                    )
                                    .await;
                                });
                            }
                        }
                    }
                    Err(_) => continue,
                }
            }
        }
    }
}

/// Handle a single Raft RPC connection over any byte stream.
async fn handle_raft_rpc<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, raft: Raft<TypeConfig>) {
    let Some(payload) = read_frame(&mut stream).await else {
        return;
    };
    // JSON, not bincode: AppendEntries carries log entries whose
    // `AppSpec` payload uses `deserialize_any` config types (see
    // durable_log). bincode is not self-describing and corrupts them,
    // so a replicated AppSpec entry would never deserialise on the
    // follower and the write would hang forever.
    let Ok(rpc) = serde_json::from_slice::<RaftRpc>(&payload) else {
        return;
    };

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

    if let Ok(bytes) = serde_json::to_vec(&response) {
        let _ = write_frame(&mut stream, &bytes).await;
    }
}

/// The material needed to build a per-target, node-id-bound mTLS connector
/// (PKI3): this node's identity (CA pins + client cert/key) and the shared,
/// live CRL handle. Held by the factory so `new_client` can bind each peer
/// connection to the specific node id it is dialling.
#[derive(Clone)]
pub struct RaftTlsMaterial {
    identity: std::sync::Arc<crate::sesame::identity_store::NodeIdentity>,
    crl: crate::sesame::mtls::CrlHandle,
}

impl RaftTlsMaterial {
    /// Bundle a node identity and CRL handle for node-id-bound dialling.
    pub fn new(
        identity: crate::sesame::identity_store::NodeIdentity,
        crl: crate::sesame::mtls::CrlHandle,
    ) -> Self {
        Self {
            identity: std::sync::Arc::new(identity),
            crl,
        }
    }

    /// Build a connector that binds the handshake to `expected_node_id`.
    fn connector_for(&self, expected_node_id: &str) -> Option<tokio_rustls::TlsConnector> {
        let config = crate::sesame::mtls::build_mtls_client_config_bound(
            &self.identity,
            self.crl.clone(),
            expected_node_id,
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
    /// When set, peers are dialled over mTLS with an unbound connector (CA-pin
    /// + CRL only). Kept for callers that don't thread an identity through.
    tls: Option<tokio_rustls::TlsConnector>,
    /// When set, each peer connection is dialled over a node-id-bound
    /// connector built from this material (PKI3). Takes precedence over `tls`.
    tls_material: Option<RaftTlsMaterial>,
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
    /// Create a new plaintext factory for a specific source node.
    pub fn new(source_id: u64) -> Self {
        Self {
            source_id,
            blocklist: std::sync::Arc::new(tokio::sync::RwLock::new(
                std::collections::HashSet::new(),
            )),
            tls: None,
            tls_material: None,
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
            tls: Some(connector),
            tls_material: None,
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
            tls: None,
            tls_material: Some(material),
        }
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
            tls,
        }
    }
}

/// A single connection to a Raft peer (plain TCP or mTLS).
pub struct TcpRaftNetwork {
    target_addr: SocketAddr,
    blocklist: std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<SocketAddr>>>,
    tls: Option<tokio_rustls::TlsConnector>,
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
        // Check blocklist for chaos testing
        if self.blocklist.read().await.contains(&self.target_addr) {
            return Err(Unreachable::new(&RouterError(
                "blocked by chaos partition".into(),
            )));
        }

        // Wrap the entire RPC in a timeout — covers connect, write, and read.
        tokio::time::timeout(std::time::Duration::from_secs(10), self.rpc_inner(rpc))
            .await
            .map_err(|_| Unreachable::new(&RouterError("rpc timeout (10s)".into())))?
    }

    /// Inner RPC implementation (called within a timeout).
    async fn rpc_inner(&self, rpc: RaftRpc) -> Result<RaftRpcResponse, Unreachable> {
        // JSON to match handle_raft_rpc (self-describing; see there).
        let payload = serde_json::to_vec(&rpc)
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

        serde_json::from_slice(&resp_payload)
            .map_err(|e| Unreachable::new(&RouterError(format!("deserialize: {e}"))))
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
}
