/// Container runtime interface.
///
/// Grill abstracts the container runtime (runc, Apple Container, or
/// a simple process fallback), providing container state management,
/// port allocation, cgroup configuration, and OCI spec generation.
#[cfg(target_os = "macos")]
pub mod apple;
pub mod btrfs;
pub mod cgroup;
pub mod image;
// Also exposed under the `ebpf` feature: the Lima-gated integration
// tests drive the agent's pre-start egress programming through a mock
// grill (a runtime whose `pid()` is `None`), which unit tests can't.
#[cfg(any(test, feature = "ebpf"))]
pub mod mock;
#[cfg(target_os = "linux")]
pub mod netns;
pub mod oci;
pub mod port;
pub mod portmap;
pub mod process;
pub mod process_workload;
pub mod records;
#[cfg(target_os = "linux")]
pub mod rootless;
#[cfg(target_os = "linux")]
pub mod runc;
pub mod snapshot;
pub mod state;
pub mod volume;

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
/// The wrapped string is the instance's *canonical* form, which is
/// namespace-qualified so two apps of the same name in different
/// namespaces never collide (DEP1). Build it through
/// [`InstanceIdentity::instance_id`] rather than formatting by hand — the
/// raw string is opaque to everything that keys on it (the supervisor
/// map, on-disk records, container ids, netns names, identity dirs).
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct InstanceId(pub String);

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The structured identity behind an [`InstanceId`].
///
/// An instance is uniquely identified by four parts:
///
/// - `namespace` — the namespace the app runs in (`default`, `payments`, …);
/// - `app` — the app (or job) name;
/// - `generation` — `Some(n)` for a canary/blue-green instance from deploy
///   generation `n`, or `None` for a steady-state instance;
/// - `ordinal` — the replica index within the app (`0`, `1`, …).
///
/// The canonical string form is
/// `{namespace}__{app}-{ordinal}` (steady state) or
/// `{namespace}__{app}-g{generation}-{ordinal}` (canary). The `__`
/// separator can never appear inside a namespace or app name — both are
/// DNS-1123 labels, which allow only `[a-z0-9-]` — so parsing the
/// namespace back out is unambiguous even when the app name itself
/// contains a hyphen.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct InstanceIdentity {
    /// Namespace the app runs in.
    pub namespace: String,
    /// App (or job) name.
    pub app: String,
    /// Deploy generation for a canary instance, or `None` for steady state.
    pub generation: Option<u64>,
    /// Replica index within the app.
    pub ordinal: u32,
}

/// The separator between the namespace prefix and the app-scoped suffix in
/// a canonical instance id. Two underscores because a single underscore is
/// already illegal in a DNS-1123 label, so this can't collide with any
/// real app or namespace name.
const NAMESPACE_SEPARATOR: &str = "__";

impl InstanceIdentity {
    /// A steady-state (non-canary) instance identity.
    pub fn new(namespace: impl Into<String>, app: impl Into<String>, ordinal: u32) -> Self {
        Self {
            namespace: namespace.into(),
            app: app.into(),
            generation: None,
            ordinal,
        }
    }

    /// A canary instance identity carrying its deploy generation.
    pub fn canary(
        namespace: impl Into<String>,
        app: impl Into<String>,
        generation: u64,
        ordinal: u32,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            app: app.into(),
            generation: Some(generation),
            ordinal,
        }
    }

    /// The app-scoped suffix (`{app}-{ordinal}` or `{app}-g{gen}-{ordinal}`),
    /// without the namespace prefix. This is the *legacy* string form, kept
    /// only for parsing pre-theme records and container names.
    pub fn app_suffix(&self) -> String {
        match self.generation {
            Some(generation) => format!("{}-g{generation}-{}", self.app, self.ordinal),
            None => format!("{}-{}", self.app, self.ordinal),
        }
    }

    /// The canonical, namespace-qualified [`InstanceId`].
    pub fn instance_id(&self) -> InstanceId {
        InstanceId(format!(
            "{}{NAMESPACE_SEPARATOR}{}",
            self.namespace,
            self.app_suffix()
        ))
    }

    /// Parse a canonical instance id back into its structured identity.
    ///
    /// Returns `None` for a string that isn't in canonical form (for
    /// example a legacy `{app}-{ordinal}` id that predates this theme) —
    /// use [`InstanceIdentity::parse_legacy`] for those.
    pub fn parse(id: &str) -> Option<Self> {
        let (namespace, suffix) = id.split_once(NAMESPACE_SEPARATOR)?;
        let mut ident = Self::parse_legacy(suffix, namespace)?;
        ident.namespace = namespace.to_string();
        Some(ident)
    }

    /// Parse a legacy, namespace-less suffix (`{app}-{ordinal}` or
    /// `{app}-g{generation}-{ordinal}`) with the namespace supplied
    /// separately — the shape an old instance record or container name
    /// carries. The app name may itself contain hyphens.
    ///
    /// The legacy format is inherently ambiguous when an app name's last
    /// hyphenated segment looks like `g{digits}` (e.g. an app literally
    /// named `worker-g5`). Adoption doesn't rely on this: it rebuilds the
    /// identity from the record's separate `namespace`/`app_name` fields,
    /// so this heuristic only ever matters for a bare container-name parse.
    pub fn parse_legacy(suffix: &str, namespace: &str) -> Option<Self> {
        let (head, ordinal_part) = suffix.rsplit_once('-')?;
        let ordinal: u32 = ordinal_part.parse().ok()?;

        // Canary form: the segment before the ordinal is `g{generation}`.
        if let Some((app, gen_part)) = head.rsplit_once('-')
            && let Some(gen_str) = gen_part.strip_prefix('g')
            && let Ok(generation) = gen_str.parse::<u64>()
        {
            return Some(Self {
                namespace: namespace.to_string(),
                app: app.to_string(),
                generation: Some(generation),
                ordinal,
            });
        }

        Some(Self {
            namespace: namespace.to_string(),
            app: head.to_string(),
            generation: None,
            ordinal,
        })
    }
}

impl fmt::Display for InstanceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.instance_id())
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

    /// Attempt to adopt a previously started instance from its on-disk
    /// record (after a bun restart or self-upgrade exec).
    ///
    /// Returns `Ok(true)` if the instance is live and is now tracked by
    /// this runtime, `Ok(false)` if it is gone (the caller should delete
    /// the record and reschedule through the normal path). The default
    /// declines: runtimes without adoption support never adopt.
    fn adopt(
        &self,
        instance: &InstanceId,
        record: &records::InstanceRecord,
    ) -> impl std::future::Future<Output = Result<bool, GrillError>> + Send {
        let _ = (instance, record);
        std::future::ready(Ok(false))
    }

    /// Which runtime kind this grill starts instances with. Recorded in
    /// instance records so adoption is routed to the right runtime.
    fn runtime_kind(&self) -> records::RuntimeKind {
        records::RuntimeKind::Process
    }

    /// Whether this runtime places workloads into the cgroup v2 path in
    /// the OCI spec (`cgroupsPath`). When true, the agent can create the
    /// cgroup directory itself, program the eBPF egress maps against its
    /// inode *before* `start`, and close the start-window during which a
    /// workload would otherwise run unpoliced. The default declines:
    /// process/VM runtimes run workloads elsewhere.
    fn honours_cgroup_path(&self) -> bool {
        false
    }

    /// Base path of an instance's on-disk log files (`{stem}.stdout` /
    /// `{stem}.stderr`), when the runtime captures to files. `None` for
    /// in-memory capture.
    fn log_stem(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Option<std::path::PathBuf>> + Send {
        let _ = instance;
        std::future::ready(None)
    }

    /// Get the OS process ID for an instance, if available.
    ///
    /// Returns `None` for runtimes where the PID isn't directly visible
    /// (e.g. containers running inside VMs).
    fn pid(&self, instance: &InstanceId) -> impl std::future::Future<Output = Option<u32>> + Send {
        let _ = instance;
        std::future::ready(None)
    }

    /// Get the runtime-assigned IP of a running instance's container, if any.
    ///
    /// Runtimes with per-container networking (runc netns, Apple container)
    /// return the address the workload actually listens on. The default
    /// returns `None`; callers then fall back to loopback, which is correct
    /// for process/host-networked runtimes.
    fn container_ip(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Option<std::net::Ipv4Addr>> + Send {
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

    /// Execute a command in the context of a running instance.
    ///
    /// Runs the given command and returns its combined stdout/stderr
    /// output. For process-based runtimes this spawns a new process;
    /// for container runtimes it enters the container's namespaces.
    fn exec(
        &self,
        instance: &InstanceId,
        command: &[String],
    ) -> impl std::future::Future<Output = Result<String, GrillError>> + Send {
        let _ = (instance, command);
        std::future::ready(Err(GrillError::NotFound {
            instance: InstanceId("exec not supported".to_string()),
        }))
    }
}

/// Runtime-selected Grill implementation.
///
/// Since `Grill` uses `impl Future` return types (not `dyn`-safe),
/// we can't use trait objects. This enum dispatches to the concrete
/// implementation selected at startup.
#[derive(Clone)]
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

impl AnyGrill {
    /// The runtime's image-store handle, when the runtime pulls OCI
    /// images itself (runc). Lets the binary install the cluster P2P
    /// image source after the cluster subsystems start — the runtime
    /// is selected long before the registry and catalog exist.
    pub fn image_store(&self) -> Option<ImageStore> {
        match self {
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => Some(g.image_store().clone()),
            _ => None,
        }
    }
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

    async fn adopt(
        &self,
        instance: &InstanceId,
        record: &records::InstanceRecord,
    ) -> Result<bool, GrillError> {
        match self {
            AnyGrill::Process(g) => g.adopt(instance, record).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.adopt(instance, record).await,
            // TODO(Phase 14 follow-up): Apple Container adoption.
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.adopt(instance, record).await,
        }
    }

    fn runtime_kind(&self) -> records::RuntimeKind {
        match self {
            AnyGrill::Process(_) => records::RuntimeKind::Process,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(_) => records::RuntimeKind::Runc,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(_) => records::RuntimeKind::Apple,
        }
    }

    fn honours_cgroup_path(&self) -> bool {
        match self {
            AnyGrill::Process(_) => false,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.honours_cgroup_path(),
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(_) => false,
        }
    }

    async fn log_stem(&self, instance: &InstanceId) -> Option<std::path::PathBuf> {
        match self {
            AnyGrill::Process(g) => g.log_stem(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.log_stem(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.log_stem(instance).await,
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

    async fn container_ip(&self, instance: &InstanceId) -> Option<std::net::Ipv4Addr> {
        match self {
            AnyGrill::Process(g) => g.container_ip(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.container_ip(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.container_ip(instance).await,
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

    async fn exec(&self, instance: &InstanceId, command: &[String]) -> Result<String, GrillError> {
        match self {
            AnyGrill::Process(g) => g.exec(instance, command).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.exec(instance, command).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.exec(instance, command).await,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_state_id_is_namespace_qualified() {
        let id = InstanceIdentity::new("default", "api", 0).instance_id();
        assert_eq!(id.0, "default__api-0");
    }

    #[test]
    fn canary_id_carries_the_generation() {
        let id = InstanceIdentity::canary("payments", "api", 5, 2).instance_id();
        assert_eq!(id.0, "payments__api-g5-2");
    }

    #[test]
    fn same_app_in_two_namespaces_does_not_collide() {
        let a = InstanceIdentity::new("default", "api", 0).instance_id();
        let b = InstanceIdentity::new("payments", "api", 0).instance_id();
        assert_ne!(a, b);
    }

    #[test]
    fn parse_round_trips_steady_state() {
        let ident = InstanceIdentity::new("default", "api", 3);
        let parsed = InstanceIdentity::parse(&ident.instance_id().0).expect("parses");
        assert_eq!(parsed, ident);
    }

    #[test]
    fn parse_round_trips_canary() {
        let ident = InstanceIdentity::canary("payments", "web", 12, 4);
        let parsed = InstanceIdentity::parse(&ident.instance_id().0).expect("parses");
        assert_eq!(parsed, ident);
    }

    #[test]
    fn parse_round_trips_hyphenated_app_name() {
        let ident = InstanceIdentity::new("team-a", "my-web-app", 1);
        let parsed = InstanceIdentity::parse(&ident.instance_id().0).expect("parses");
        assert_eq!(parsed, ident);
        assert_eq!(parsed.namespace, "team-a");
        assert_eq!(parsed.app, "my-web-app");
    }

    #[test]
    fn parse_rejects_legacy_bare_id() {
        // A pre-theme id with no namespace prefix isn't canonical.
        assert!(InstanceIdentity::parse("api-0").is_none());
    }

    #[test]
    fn parse_legacy_recovers_steady_state() {
        let ident = InstanceIdentity::parse_legacy("api-0", "default").expect("parses");
        assert_eq!(ident, InstanceIdentity::new("default", "api", 0));
    }

    #[test]
    fn parse_legacy_recovers_canary() {
        let ident = InstanceIdentity::parse_legacy("api-g1234-0", "prod").expect("parses");
        assert_eq!(ident, InstanceIdentity::canary("prod", "api", 1234, 0));
    }

    #[test]
    fn parse_legacy_recovers_hyphenated_app() {
        let ident = InstanceIdentity::parse_legacy("my-web-app-7", "default").expect("parses");
        assert_eq!(ident.app, "my-web-app");
        assert_eq!(ident.ordinal, 7);
        assert_eq!(ident.generation, None);
    }
}
