/// Raft consensus for the council (3–7 nodes).
///
/// The council is a small group of nodes that replicate desired state
/// (app specs, scheduling decisions, configuration) using the Raft
/// protocol. While Mustard gossip runs on all nodes for membership
/// and failure detection, Raft runs only on council members for
/// strong consistency.
///
/// We wrap the `openraft` crate with three adapter implementations:
/// - `MemLogStore` — in-memory Raft log storage
/// - `CouncilStateMachine` — applies entries to desired cluster state
/// - `InMemoryRaftRouter` — in-memory network for testing
pub mod log_store;
pub mod state_machine;
pub mod types;

pub use types::{
    CouncilConfig, CouncilNodeInfo, CouncilResponse, DesiredState, RaftRequest, TypeConfig,
};

/// Errors from the council subsystem.
#[derive(Debug, thiserror::Error)]
pub enum CouncilError {
    #[error("raft fatal: {0}")]
    Fatal(String),
    #[error("not leader, leader is node {leader:?}")]
    ForwardToLeader { leader: Option<u64> },
    #[error("raft initialisation failed: {0}")]
    InitError(String),
    #[error("raft write failed: {0}")]
    WriteFailed(String),
    #[error("network error: {0}")]
    Network(String),
}
