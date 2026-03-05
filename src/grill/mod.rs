/// Container runtime interface.
///
/// Grill abstracts the container runtime (containerd/runc), providing
/// container state management, port allocation, cgroup configuration,
/// and OCI spec generation. The actual containerd gRPC integration
/// is deferred to the integration test phase.
pub mod cgroup;
pub mod oci;
pub mod port;
pub mod state;

use std::fmt;

pub use cgroup::{CgroupParams, cgroup_path, compute_cgroup_params, cpu_max_from_millicores};
pub use oci::{OciSpec, generate_oci_spec};
pub use port::{PortAllocator, PortError};
pub use state::ContainerState;

/// Unique identifier for a workload instance on this node.
///
/// Format: `"{app_name}-{replica_index}"`, e.g. `"web-3"`.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct InstanceId(pub String);

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Errors from Grill operations.
#[derive(Debug, thiserror::Error)]
pub enum GrillError {
    #[error("invalid state transition: {0}")]
    InvalidTransition(#[from] state::InvalidTransition),

    #[error("port allocation failed: {0}")]
    Port(#[from] PortError),

    #[error("container {instance} failed to start: {reason}")]
    StartFailed {
        instance: InstanceId,
        reason: String,
    },

    #[error("container {instance} not found")]
    NotFound { instance: InstanceId },
    // TODO(Phase 1): add containerd client errors when integration is implemented
}

/// The container runtime interface.
///
/// Abstracts the underlying container runtime (containerd/runc).
/// The real implementation talks to containerd over gRPC.
pub trait Grill: Send + Sync {
    /// Create a container from an OCI spec. Does not start it.
    fn create(
        &self,
        instance: &InstanceId,
        spec: &OciSpec,
    ) -> impl std::future::Future<Output = Result<(), GrillError>> + Send;

    /// Start a previously created container.
    fn start(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<(), GrillError>> + Send;

    /// Send SIGTERM to the container. Returns immediately.
    fn stop(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<(), GrillError>> + Send;

    /// Send SIGKILL to the container.
    fn kill(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<(), GrillError>> + Send;

    /// Get the current state of a container.
    fn state(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<ContainerState, GrillError>> + Send;
}
