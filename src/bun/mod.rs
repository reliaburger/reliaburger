/// Bun — the per-node agent.
///
/// Manages workload instances on a single node: deploying containers,
/// supervising their lifecycle, running health checks, computing restart
/// backoff, detecting GPU hardware, and serving a local HTTP API.
pub mod agent;
pub mod api;
pub mod gpu;
pub mod health;
pub mod probe;
pub mod restart;
pub mod supervisor;
pub mod testapp;

pub use gpu::{GpuDetector, GpuInfo, StubGpuDetector};
pub use health::{HealthCheckConfig, HealthChecker, HealthCounters, HealthStatus, evaluate_result};
pub use restart::RestartPolicy;
pub use supervisor::{WorkloadInstance, WorkloadSupervisor};

use crate::grill::port::PortError;
use crate::grill::state::InvalidTransition;
use crate::grill::{GrillError, InstanceId};

/// Errors from Bun agent operations.
#[derive(Debug, thiserror::Error)]
pub enum BunError {
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

    /// A health check was configured but the app has no port to probe.
    #[error("app {app_name:?} has a health check but no port")]
    NoPortForHealthCheck { app_name: String },

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
    #[error("init container {init_index} failed for instance {instance_id}")]
    InitContainerFailed {
        instance_id: InstanceId,
        init_index: usize,
    },
}
