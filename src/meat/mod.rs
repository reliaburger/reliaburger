/// Meat scheduler.
///
/// Handles multi-node workload placement decisions. The scheduler runs
/// on the leader node and uses a four-phase pipeline (Filter → Score →
/// Select → Commit) to place replicas across the cluster.
///
/// This module is built incrementally across Phase 2. Currently it
/// exports the shared cluster types; the placement logic arrives in
/// later PRs.
pub mod types;

// TODO(Phase 2): placement, scoring, cluster_state, quota modules

pub use types::{AppId, NodeCapacity, NodeId, Placement, Resources, SchedulingDecision};
