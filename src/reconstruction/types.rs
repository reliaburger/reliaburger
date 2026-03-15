/// Types for state reconstruction.
///
/// The reconstruction process has five phases, produces a set of
/// corrections (desired vs actual mismatches), and records whether
/// the learning period ended by threshold or timeout.
use crate::patty::types::{AppId, NodeId};

/// Phase of the state reconstruction process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconstructionPhase {
    /// No leader election in progress. Normal operations.
    Idle,
    /// Leader just elected. Announcing via gossip.
    Announcing,
    /// Collecting StateReports. No scheduling or new deploys.
    Learning,
    /// Diffing desired vs actual state and computing corrections.
    Reconciling,
    /// Reconstruction complete. Normal operations resumed.
    Active,
}

/// A mismatch between desired and actual state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Correction {
    /// App should be running on this node but isn't.
    MissingApp { app_id: AppId, node_id: NodeId },
    /// App is running on this node but shouldn't be.
    ExtraApp { app_id: AppId, node_id: NodeId },
    /// Node didn't report during the learning period.
    UnknownNode { node_id: NodeId },
}

/// How the learning period ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningOutcome {
    /// Threshold was met (>= report_threshold_percent of nodes reported).
    ThresholdMet { reported: usize, total: usize },
    /// Timeout fired before the threshold was reached.
    TimedOut { reported: usize, total: usize },
}

/// Full result of a reconstruction cycle.
#[derive(Debug, Clone)]
pub struct ReconstructionResult {
    /// How the learning period ended.
    pub outcome: LearningOutcome,
    /// Corrections computed by the diff engine.
    pub corrections: Vec<Correction>,
    /// Nodes that did not report and are marked STATE_UNKNOWN.
    pub unknown_nodes: Vec<NodeId>,
    /// Nodes that reported and are eligible for scheduling.
    pub reported_nodes: Vec<NodeId>,
}
