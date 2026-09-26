/// Bun — the per-node agent.
///
/// Manages workload instances on a single node: deploying containers,
/// supervising their lifecycle, running health checks, computing restart
/// backoff, detecting GPU hardware, and serving a local HTTP API.
pub mod agent;
pub mod api;
pub mod authz;
pub mod batch;
pub mod build_runner;
pub mod capabilities;
pub mod consumer_owners;
pub mod deploy_operations;
pub mod diagnostics;
pub mod discovery_owners;
pub mod disk_pressure;
mod egress_owners;
pub mod events;
pub mod gpu;
pub mod health;
pub(crate) mod jobs;
pub mod probe;
pub mod readiness;
pub mod restart;
mod schedules;
pub mod snapshot_worker;
pub mod supervisor;
pub mod testapp;
pub mod top;

pub use gpu::{GpuDetector, GpuInfo, NvidiaGpuDetector, StubGpuDetector};
pub use health::{HealthCheckConfig, HealthChecker, HealthCounters, HealthStatus, evaluate_result};
pub use restart::RestartPolicy;
pub use supervisor::{WorkloadInstance, WorkloadSupervisor};

use crate::grill::port::PortError;
use crate::grill::state::InvalidTransition;
use crate::grill::{GrillError, InstanceId};

/// Errors from Bun agent operations.
#[derive(Debug, thiserror::Error)]
pub enum BunError {
    /// Durable job execution evidence cannot be established.
    #[error("job state is unavailable: {0}")]
    JobState(String),

    /// A schedule mutation cannot establish durable ownership.
    #[error("scheduled-job state is unavailable: {0}")]
    ScheduleState(String),

    /// Startup cannot establish the complete ownership inventory.
    #[error("cannot restore workload ownership: {0}")]
    AdoptionState(String),

    /// Runtime exit was confirmed, but durable ownership cleanup must retry.
    #[error("cannot retire artifacts for {instance_id}: {reason}")]
    RetirementState {
        instance_id: InstanceId,
        reason: String,
    },

    /// The leader hasn't confirmed a producer release yet: its request is
    /// still in flight, or other nodes haven't confirmed the endpoint's
    /// withdrawal. Asking again shortly is expected to succeed.
    #[error("cannot retire artifacts for {instance_id} yet: {reason}")]
    ProducerReleasePending {
        instance_id: InstanceId,
        reason: &'static str,
    },

    /// The cluster catalogue and routing views could not be confirmed together.
    #[error("cluster discovery publication failed: {0}")]
    ClusterPublication(String),

    /// Backend registration or publication failed, so deployment cannot report completion.
    #[error("cannot publish backend for {service}: {reason}")]
    BackendPublication {
        service: crate::onion::service_id::ServiceId,
        reason: String,
    },

    /// Kernel service withdrawal failed, so its workload ownership must remain.
    #[error("cannot retire backend for {service}: {reason}")]
    BackendRetirement {
        service: crate::onion::service_id::ServiceId,
        reason: String,
    },

    /// Destination permissions remain owned until their removal is confirmed.
    #[error("cannot retire destination grants for {service}: {reason}")]
    DestinationRetirement {
        service: crate::onion::service_id::ServiceId,
        reason: String,
    },

    /// An error from the container runtime.
    #[error(transparent)]
    Grill(#[from] GrillError),

    /// A port allocation error.
    #[error("port allocation failed: {0}")]
    Port(#[from] PortError),

    /// An invalid state transition was attempted.
    #[error("invalid state transition: {0}")]
    InvalidTransition(#[from] InvalidTransition),

    /// The requested workload instance does not exist.
    #[error("instance not found: {instance_id}")]
    InstanceNotFound { instance_id: InstanceId },

    /// The requested app does not exist in the given namespace.
    #[error("app {app_name:?} not found in namespace {namespace:?}")]
    AppNotFound { app_name: String, namespace: String },

    /// A deployment still owns mutations for the requested workload.
    #[error(
        "workload {namespace}/{app_name} is still owned by deploy {operation_id}; wait or cancel the deploy before stopping"
    )]
    WorkloadBusy {
        app_name: String,
        namespace: String,
        operation_id: deploy_operations::DeployOperationId,
    },

    /// Runtime exit could not be confirmed within the stop deadline.
    #[error("stop not confirmed for instance {instance_id}: {reason}")]
    StopUnconfirmed {
        instance_id: InstanceId,
        reason: &'static str,
    },

    /// A stop this request joined, or was waiting on, did not complete.
    #[error("stop did not complete: {reason}")]
    StopIncomplete { reason: String },

    /// An `exec` did not finish within its deadline.
    #[error("exec timed out after {seconds}s")]
    ExecTimeout { seconds: u64 },

    /// A health check was configured but the app has no port to probe.
    #[error("app {app_name:?} has a health check but no port")]
    NoPortForHealthCheck { app_name: String },

    /// Deploy was rejected (e.g. process workload binary not in allowlist).
    #[error("deploy failed for {app_name:?}: {reason}")]
    DeployFailed { app_name: String, reason: String },

    /// A fault injection was rejected (safety rail, or unsupported on
    /// this platform / without the eBPF feature).
    #[error("fault rejected: {reason}")]
    FaultRejected { reason: String },

    /// A volume snapshot operation failed (unsupported filesystem,
    /// missing snapshot, running app on restore, btrfs failure).
    #[error("snapshot: {0}")]
    Snapshot(#[from] crate::grill::snapshot::SnapshotError),

    /// The workload has exceeded its restart limit.
    #[error(
        "instance {instance_id} exceeded restart limit: {restart_count}/{max_restarts} restarts"
    )]
    RestartLimitExceeded {
        instance_id: InstanceId,
        restart_count: u32,
        max_restarts: u32,
    },

    /// An init container failed during startup.
    #[error("init container {init_index} failed for instance {instance_id}: {reason}")]
    InitContainerFailed {
        instance_id: InstanceId,
        init_index: usize,
        /// How it failed, with the runtime's captured stderr tail when it has one.
        reason: String,
    },

    /// A security or identity operation failed.
    #[error("security error: {reason}")]
    SecurityError { reason: String },

    /// All bounded connectivity-trace execution slots are occupied.
    #[error("too many connectivity traces are already running on this node")]
    TraceBusy,

    /// A self-upgrade operation failed.
    #[error(transparent)]
    Upgrade(#[from] crate::upgrade::UpgradeError),

    /// Self-upgrade is not configured on this node.
    #[error("self-upgrade is not available on this node (no upgrade manager)")]
    UpgradesUnavailable,
}

#[cfg(test)]
mod job_lifecycle_tests;
