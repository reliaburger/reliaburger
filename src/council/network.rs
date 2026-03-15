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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maximum Raft RPC payload size (64 MiB — snapshots can be large).
const MAX_RAFT_RPC_SIZE: usize = 64 * 1024 * 1024;

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

/// Read a length-prefixed bincode frame from a TCP stream.
async fn read_frame(stream: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_RAFT_RPC_SIZE {
        return None;
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

/// Write a length-prefixed bincode frame to a TCP stream.
async fn write_frame(stream: &mut tokio::net::TcpStream, data: &[u8]) -> Result<(), String> {
    let len_bytes = (data.len() as u32).to_be_bytes();
    stream
        .write_all(&len_bytes)
        .await
        .map_err(|e| e.to_string())?;
    stream.write_all(data).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Serve Raft RPCs over TCP.
///
/// Accepts connections on `listener`, reads one RPC per connection,
/// dispatches to the local `raft` instance, and writes the response.
/// Runs until `shutdown` is cancelled.
pub async fn serve_raft_rpc(
    listener: tokio::net::TcpListener,
    raft: Raft<TypeConfig>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            result = listener.accept() => {
                match result {
                    Ok((stream, _peer)) => {
                        let raft = raft.clone();
                        tokio::spawn(handle_raft_rpc(stream, raft));
                    }
                    Err(_) => continue,
                }
            }
        }
    }
}

/// Handle a single Raft RPC connection.
async fn handle_raft_rpc(mut stream: tokio::net::TcpStream, raft: Raft<TypeConfig>) {
    let Some(payload) = read_frame(&mut stream).await else {
        return;
    };
    let Ok(rpc) = bincode::deserialize::<RaftRpc>(&payload) else {
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

    if let Ok(bytes) = bincode::serialize(&response) {
        let _ = write_frame(&mut stream, &bytes).await;
    }
}

/// Creates TCP-based Raft network connections.
#[derive(Debug, Clone)]
pub struct TcpRaftNetworkFactory {
    #[allow(dead_code)]
    source_id: u64,
}

impl TcpRaftNetworkFactory {
    /// Create a new factory for a specific source node.
    pub fn new(source_id: u64) -> Self {
        Self { source_id }
    }
}

impl RaftNetworkFactory<TypeConfig> for TcpRaftNetworkFactory {
    type Network = TcpRaftNetwork;

    async fn new_client(&mut self, _target: u64, node: &CouncilNodeInfo) -> Self::Network {
        TcpRaftNetwork {
            target_addr: node.addr,
        }
    }
}

/// A single TCP connection to a Raft peer.
#[derive(Debug)]
pub struct TcpRaftNetwork {
    target_addr: SocketAddr,
}

impl TcpRaftNetwork {
    /// Send an RPC and read the response.
    async fn rpc(&self, rpc: RaftRpc) -> Result<RaftRpcResponse, Unreachable> {
        let payload = bincode::serialize(&rpc)
            .map_err(|e| Unreachable::new(&RouterError(format!("serialize: {e}"))))?;

        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::TcpStream::connect(self.target_addr),
        )
        .await
        .map_err(|_| Unreachable::new(&RouterError("connect timeout".into())))?
        .map_err(|e| Unreachable::new(&RouterError(format!("connect: {e}"))))?;

        write_frame(&mut stream, &payload)
            .await
            .map_err(|e| Unreachable::new(&RouterError(format!("write: {e}"))))?;

        let resp_payload = read_frame(&mut stream)
            .await
            .ok_or_else(|| Unreachable::new(&RouterError("read response failed".into())))?;

        bincode::deserialize(&resp_payload)
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
}
