/// Hierarchical reporting tree.
///
/// Non-council nodes send `StateReport` to their assigned council
/// member every few seconds. Council members aggregate reports and
/// expose the latest cluster view via a `tokio::sync::watch` channel.
///
/// Parent assignment is deterministic: `council[hash(node_id) % council_size]`
/// with the council list sorted by `NodeId` for cross-node consistency.
pub mod aggregator;
pub mod assignment;
pub mod transport;
pub mod types;
pub mod worker;

pub use aggregator::{AggregatedState, ReportAggregator};
pub use assignment::assign_parent;
pub use transport::{InMemoryReportingNetwork, InMemoryReportingTransport, ReportingTransport};
pub use types::{
    AppResourceUsage, EventKind, NodeEvent, ReportHealthStatus, ReportingMessage, ResourceUsage,
    RunningApp, StateReport,
};
pub use worker::{AgentSnapshot, CollectSnapshotRequest, InstanceSnapshot, ReportWorker};

/// Errors from reporting tree operations.
#[derive(Debug, thiserror::Error)]
pub enum ReportingError {
    #[error("send failed: {reason}")]
    SendFailed { reason: String },

    #[error("connection lost to parent {parent}")]
    ConnectionLost { parent: String },

    #[error("serialisation error: {0}")]
    Serialisation(String),

    #[error("deserialisation error: {0}")]
    Deserialisation(String),

    #[error("report too large: {size} bytes (max {max})")]
    ReportTooLarge { size: usize, max: usize },
}
