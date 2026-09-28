/// Meat scheduler.
///
/// Handles multi-node workload placement decisions. The scheduler runs
/// on the leader node and uses a four-phase pipeline (Filter → Score →
/// Select → Commit) to place replicas across the cluster.
pub mod admission;
pub mod autoscaler;
pub mod batch;
pub(crate) mod batch_execution;
pub mod batch_tracker;
pub mod cluster_state;
pub mod cron;
pub mod deploy_types;
pub mod filter;
pub mod index_set;
pub mod latency_histogram;
pub mod quota;
pub mod scheduler;
pub mod score;
pub mod task_array;
pub mod task_array_state;
pub mod types;

pub use cluster_state::{ClusterStateCache, SchedulerNodeState};
pub use quota::{NamespaceQuota, NamespaceUsage, QuotaError, check_quota};
pub use scheduler::{ScheduleError, Scheduler};
pub use types::{AppId, NodeCapacity, NodeId, Placement, Resources, SchedulingDecision};
