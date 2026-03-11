/// Container runtime interface.
///
/// Grill abstracts the container runtime (runc, Apple Container, or
/// a simple process fallback), providing container state management,
/// port allocation, cgroup configuration, and OCI spec generation.
#[cfg(target_os = "macos")]
pub mod apple;
pub mod cgroup;
pub mod image;
#[cfg(test)]
pub mod mock;
pub mod oci;
pub mod port;
pub mod process;
#[cfg(target_os = "linux")]
pub mod rootless;
#[cfg(target_os = "linux")]
pub mod runc;
pub mod state;

use std::fmt;

use tokio::sync::mpsc;

pub use cgroup::{CgroupParams, cgroup_path, compute_cgroup_params, cpu_max_from_millicores};
pub use image::ImageStore;
pub use oci::{OciSpec, generate_job_oci_spec, generate_oci_spec};
pub use port::{PortAllocator, PortError};
pub use process::ProcessGrill;
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

    #[error("image pull failed: {0}")]
    ImagePull(#[from] image::ImageError),
}

/// The container runtime interface.
///
/// Abstracts the underlying container runtime. Implementations exist
/// for `runc` (Linux), Apple Container (macOS), and plain OS processes
/// (cross-platform fallback).
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

    /// Get the OS process ID for an instance, if available.
    ///
    /// Returns `None` for runtimes where the PID isn't directly visible
    /// (e.g. containers running inside VMs).
    fn pid(&self, instance: &InstanceId) -> impl std::future::Future<Output = Option<u32>> + Send {
        let _ = instance;
        std::future::ready(None)
    }

    /// Get the exit code of a stopped instance.
    ///
    /// Returns `None` if the instance hasn't exited, doesn't exist,
    /// or the runtime doesn't track exit codes.
    fn exit_code(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Option<i32>> + Send {
        let _ = instance;
        std::future::ready(None)
    }

    /// Get captured logs for an instance.
    ///
    /// Returns whatever output the runtime has captured. The default
    /// returns an empty string for runtimes that don't capture logs.
    fn logs(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<String, GrillError>> + Send {
        let _ = instance;
        std::future::ready(Ok(String::new()))
    }

    /// Stream logs for an instance.
    ///
    /// Sends new log lines over the channel as they are produced.
    /// The default does nothing (stream closes immediately). Runtimes
    /// that support streaming override this.
    fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: mpsc::Sender<String>,
    ) -> impl std::future::Future<Output = ()> + Send {
        let _ = (instance, lines_tx);
        std::future::ready(())
    }
}

/// Runtime-selected Grill implementation.
///
/// Since `Grill` uses `impl Future` return types (not `dyn`-safe),
/// we can't use trait objects. This enum dispatches to the concrete
/// implementation selected at startup.
pub enum AnyGrill {
    /// Cross-platform process-based runtime.
    Process(ProcessGrill),
    /// Linux runc-based container runtime.
    #[cfg(target_os = "linux")]
    Runc(runc::RuncGrill),
    /// macOS Apple Container runtime.
    #[cfg(target_os = "macos")]
    Apple(apple::AppleContainerGrill),
}

impl Grill for AnyGrill {
    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        match self {
            AnyGrill::Process(g) => g.create(instance, spec).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.create(instance, spec).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.create(instance, spec).await,
        }
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        match self {
            AnyGrill::Process(g) => g.start(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.start(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.start(instance).await,
        }
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        match self {
            AnyGrill::Process(g) => g.stop(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.stop(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.stop(instance).await,
        }
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        match self {
            AnyGrill::Process(g) => g.kill(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.kill(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.kill(instance).await,
        }
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        match self {
            AnyGrill::Process(g) => g.state(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.state(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.state(instance).await,
        }
    }

    async fn pid(&self, instance: &InstanceId) -> Option<u32> {
        match self {
            AnyGrill::Process(g) => g.pid(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.pid(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.pid(instance).await,
        }
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        match self {
            AnyGrill::Process(g) => g.exit_code(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.exit_code(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.exit_code(instance).await,
        }
    }

    async fn logs(&self, instance: &InstanceId) -> Result<String, GrillError> {
        match self {
            AnyGrill::Process(g) => g.logs(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.logs(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.logs(instance).await,
        }
    }

    async fn follow_logs(&self, instance: &InstanceId, lines_tx: mpsc::Sender<String>) {
        match self {
            AnyGrill::Process(g) => g.follow_logs(instance, lines_tx).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.follow_logs(instance, lines_tx).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.follow_logs(instance, lines_tx).await,
        }
    }
}

/// Auto-detect the best available runtime.
///
/// Checks for platform-specific runtimes first, falls back to ProcessGrill.
/// On Linux, detects rootless mode and configures paths accordingly.
pub async fn detect_runtime() -> AnyGrill {
    #[cfg(target_os = "macos")]
    {
        if which_exists("container").await {
            return AnyGrill::Apple(apple::AppleContainerGrill::new());
        }
    }

    #[cfg(target_os = "linux")]
    {
        if which_exists("runc").await {
            let is_rootless = rootless::is_rootless();

            let (bundle_base, image_store, state_dir) = if is_rootless {
                let base = dirs::data_local_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp/reliaburger"))
                    .join("reliaburger");
                (
                    base.join("bundles"),
                    ImageStore::new(base.join("images")),
                    rootless::rootless_state_dir(),
                )
            } else {
                let base = std::path::PathBuf::from("/var/lib/reliaburger");
                (
                    base.join("bundles"),
                    ImageStore::new(base.join("images")),
                    std::path::PathBuf::from("/run/reliaburger/runc"),
                )
            };

            return AnyGrill::Runc(runc::RuncGrill::new(
                bundle_base,
                image_store,
                is_rootless,
                state_dir,
            ));
        }
    }

    AnyGrill::Process(ProcessGrill::new())
}

/// Check if a binary exists in PATH.
async fn which_exists(name: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(name)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}
