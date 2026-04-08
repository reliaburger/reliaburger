/// Meat scheduler.
///
/// Handles multi-node workload placement decisions. The scheduler runs
/// on the leader node and uses a four-phase pipeline (Filter → Score →
/// Select → Commit) to place replicas across the cluster.
pub mod cluster_state;
pub mod deploy_types;
pub mod filter;
pub mod orchestrator;
pub mod quota;
pub mod scheduler;
pub mod score;
pub mod types;

pub use cluster_state::{ClusterStateCache, SchedulerNodeState};
pub use quota::{NamespaceQuota, NamespaceUsage, QuotaError, check_quota};
pub use scheduler::{ScheduleError, Scheduler};
pub use types::{AppId, NodeCapacity, NodeId, Placement, Resources, SchedulingDecision};
