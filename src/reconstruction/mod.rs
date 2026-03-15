/// State reconstruction after leader election.
///
/// When a new Raft leader is elected, it knows the desired state (from
/// the log) but not what's actually running. The reconstruction controller
/// runs a learning period: collect StateReports until 95% of nodes have
/// reported (or a timeout fires), then diff desired vs actual and compute
/// corrections.
pub mod controller;
pub mod diff;
pub mod types;

pub use controller::ReconstructionController;
pub use diff::compute_diff;
pub use types::{Correction, LearningOutcome, ReconstructionPhase, ReconstructionResult};

/// Errors from state reconstruction.
#[derive(Debug, thiserror::Error)]
pub enum ReconstructionError {
    #[error("not the leader")]
    NotLeader,

    #[error("reconstruction already in progress")]
    AlreadyRunning,
}
