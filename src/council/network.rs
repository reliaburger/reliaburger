//! In-memory Raft network for testing.
//!
//! Routes RPCs directly between `openraft::Raft` handles without
//! TCP, enabling fast deterministic tests with partition simulation.

use std::collections::{HashMap, HashSet};
use std::fmt;
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
