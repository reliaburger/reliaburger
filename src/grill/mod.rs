/// Container runtime interface.
///
/// Grill abstracts the container runtime (runc, Apple Container, or
/// a simple process fallback), providing container state management,
/// port allocation, cgroup configuration, and OCI spec generation.
#[cfg(target_os = "macos")]
pub mod apple;
pub mod btrfs;
pub mod capture;
pub mod cgroup;
pub mod command;
pub mod image;
pub mod image_config;
mod inventory;
// Also exposed under the `ebpf` feature: the Lima-gated integration
// tests drive the agent's pre-start egress programming through a mock
// grill (a runtime whose `pid()` is `None`), which unit tests can't.
#[cfg(any(test, feature = "ebpf"))]
pub mod mock;
#[cfg(target_os = "linux")]
pub mod netns;
#[cfg(target_os = "linux")]
mod network_leases;
pub mod oci;
pub(crate) mod oci_pull;
pub mod port;
pub mod portmap;
pub mod process;
mod process_control;
pub mod process_owner;
pub mod process_workload;
pub mod records;
#[cfg(target_os = "linux")]
mod rootfs;
#[cfg(target_os = "linux")]
pub mod rootless;
#[cfg(target_os = "linux")]
pub mod runc;
pub mod runc_intent;
pub mod snapshot;
pub mod state;
pub mod userns;
pub mod volume;

use std::fmt;

use tokio::sync::mpsc;

pub use cgroup::{CgroupParams, cgroup_path, compute_cgroup_params, cpu_max_from_millicores};
pub use image::{ImageMirrors, ImageStore};
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
#[derive(Debug, Clone, Hash, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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
/// contains a hyphen. Generation-like app suffixes can still produce the same
/// string as another app's canary (for example `worker-g1` and generation 1 of
/// `worker`). Allocation must check the inventory's structured owner before
/// claiming an ID; parsing the text alone cannot disambiguate those tuples.
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
    /// without the namespace prefix.
    fn app_suffix(&self) -> String {
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
    /// Returns `None` for a string that isn't in canonical form, such as a
    /// bare `{app}-{ordinal}` with no namespace prefix.
    ///
    /// The suffix is ambiguous when an app name's last hyphenated segment
    /// looks like `g{digits}` (e.g. an app literally named `worker-g5`).
    /// Adoption instead checks the canonical ID against the record's separate
    /// `namespace`/`app_name` fields, so this heuristic only matters for a
    /// bare id parse.
    pub fn parse(id: &str) -> Option<Self> {
        let (namespace, suffix) = id.split_once(NAMESPACE_SEPARATOR)?;
        Self::parse_suffix(suffix, namespace)
    }

    /// Parse the app-scoped suffix (`{app}-{ordinal}` or
    /// `{app}-g{generation}-{ordinal}`) of a canonical id. The app name may
    /// itself contain hyphens.
    fn parse_suffix(suffix: &str, namespace: &str) -> Option<Self> {
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

    /// A stop signal or exit wait could not be completed safely.
    #[error("container {instance} failed to stop: {reason}")]
    StopFailed {
        instance: InstanceId,
        reason: String,
    },

    /// Runtime inspection could not establish the instance's current state.
    #[error("container {instance} state unavailable: {reason}")]
    StateUnavailable {
        instance: InstanceId,
        reason: String,
    },

    /// Durable launch inventory could not be established.
    #[error("runtime launch inventory unavailable: {reason}")]
    InventoryUnavailable {
        /// The evidence that could not be read or validated.
        reason: String,
    },

    #[error("container {instance} not found")]
    NotFound { instance: InstanceId },

    #[error("image pull failed: {0}")]
    ImagePull(#[from] image::ImageError),
}

/// Non-secret identity of one original runtime execution generation.
#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct RuntimeGeneration(String);

impl TryFrom<String> for RuntimeGeneration {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err("invalid runtime generation fingerprint");
        }
        Ok(Self(value))
    }
}

impl RuntimeGeneration {
    /// Stable fingerprint for correlation, never an execution capability.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn process(token: &str) -> Self {
        Self::fingerprint(b"reliaburger/runtime-generation/process/v1\0", token)
    }

    pub(crate) fn runc(token: &str) -> Self {
        Self::fingerprint(b"reliaburger/runtime-generation/runc/v1\0", token)
    }

    fn fingerprint(domain: &[u8], token: &str) -> Self {
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        digest.update(domain);
        digest.update(token.as_bytes());
        Self(hex::encode(digest.finish().as_ref()))
    }
}

/// Original execution behind a reported or published workload endpoint.
#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeExecution {
    /// Exact canonical runtime instance name, including deployment generation.
    pub instance_id: InstanceId,
    /// Non-secret fingerprint from that instance's original runtime intent.
    pub generation: RuntimeGeneration,
}

/// A durable runtime launch, written before user code can execute.
#[derive(Debug, Clone)]
pub struct RuntimeLaunch {
    /// Identity derived from the original private runtime intent.
    pub generation: RuntimeGeneration,
    /// Canonical workload identity.
    pub instance_id: InstanceId,
    /// Specification committed for this generation.
    pub spec: OciSpec,
    /// Original address hold or release receipt from the same durable runtime intent.
    /// Process runtimes have no reusable container address and return None.
    pub network_reference: Option<runc_intent::NetworkReferenceState>,
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

    /// Read the complete durable launch inventory, including launches without
    /// agent adoption records. `None` means this runtime cannot establish that
    /// inventory; it must never be interpreted as an empty inventory.
    fn launch_inventory(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Vec<RuntimeLaunch>>, GrillError>> + Send
    {
        std::future::ready(Ok(None))
    }

    /// Hold a rootful address before discovery publication. Unsupported adapters
    /// return None; the production recovery profile must require this capability.
    fn retain_network_reference(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<Option<runc_intent::NetworkReference>, GrillError>>
    + Send {
        let _ = instance;
        std::future::ready(Ok(None))
    }

    /// Read an outstanding discovery reference independently of execution state.
    fn network_reference(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<Option<runc_intent::NetworkReference>, GrillError>>
    + Send {
        let _ = instance;
        std::future::ready(Ok(None))
    }

    /// Confirm withdrawal of every route naming this exact original address.
    /// Callers must retain the reference on any failed or uncertain withdrawal.
    fn release_network_reference(
        &self,
        reference: &runc_intent::NetworkReference,
    ) -> impl std::future::Future<Output = Result<(), GrillError>> + Send {
        std::future::ready(Err(GrillError::StateUnavailable {
            instance: reference.instance_id.clone(),
            reason: "runtime cannot release a discovery reference".into(),
        }))
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

    /// Snapshot rootless userspace-network ownership for an adoption record.
    fn rootless_network_record(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Option<records::RootlessNetworkRecord>> + Send {
        let _ = instance;
        std::future::ready(None)
    }

    /// Return the verified live workload's cgroup v2 identity, when supported.
    /// A launcher PID is not a workload cgroup. Unavailable or conflicting
    /// ownership returns an error; unsupported runtimes return `None`.
    fn workload_cgroup(
        &self,
        instance: &InstanceId,
    ) -> impl std::future::Future<Output = Result<Option<u64>, GrillError>> + Send {
        let _ = instance;
        std::future::ready(Ok(None))
    }

    /// Get the OS process ID for an instance, if available.
    ///
    /// Returns `None` for runtimes where the PID isn't directly visible
    /// (e.g. containers running inside VMs). Runc reports its owned launcher
    /// here; use `workload_cgroup` for verified container network attribution.
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
    /// Sends every captured line over the channel, from the start of the
    /// instance's output, then new lines as they are produced. File-backed
    /// runtimes tag each line with its capture position so the log store can
    /// skip lines it already ingested. The default does nothing (stream
    /// closes immediately). Runtimes that support streaming override this.
    fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: mpsc::Sender<crate::ketchup::types::CapturedLine>,
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
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.adopt(instance, record).await,
        }
    }

    async fn launch_inventory(&self) -> Result<Option<Vec<RuntimeLaunch>>, GrillError> {
        match self {
            AnyGrill::Process(g) => g.launch_inventory().await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.launch_inventory().await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.launch_inventory().await,
        }
    }

    async fn retain_network_reference(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<runc_intent::NetworkReference>, GrillError> {
        match self {
            AnyGrill::Process(runtime) => runtime.retain_network_reference(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(runtime) => runtime.retain_network_reference(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(runtime) => runtime.retain_network_reference(instance).await,
        }
    }

    async fn network_reference(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<runc_intent::NetworkReference>, GrillError> {
        match self {
            AnyGrill::Process(runtime) => runtime.network_reference(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(runtime) => runtime.network_reference(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(runtime) => runtime.network_reference(instance).await,
        }
    }

    async fn release_network_reference(
        &self,
        reference: &runc_intent::NetworkReference,
    ) -> Result<(), GrillError> {
        match self {
            AnyGrill::Process(runtime) => runtime.release_network_reference(reference).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(runtime) => runtime.release_network_reference(reference).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(runtime) => runtime.release_network_reference(reference).await,
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

    async fn rootless_network_record(
        &self,
        instance: &InstanceId,
    ) -> Option<records::RootlessNetworkRecord> {
        match self {
            AnyGrill::Process(g) => g.rootless_network_record(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.rootless_network_record(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.rootless_network_record(instance).await,
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

    async fn workload_cgroup(&self, instance: &InstanceId) -> Result<Option<u64>, GrillError> {
        match self {
            AnyGrill::Process(g) => g.workload_cgroup(instance).await,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(g) => g.workload_cgroup(instance).await,
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(g) => g.workload_cgroup(instance).await,
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

    async fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: mpsc::Sender<crate::ketchup::types::CapturedLine>,
    ) {
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

/// Which runtime `detect_runtime` found on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedRuntime {
    /// Native processes; no container runtime is available.
    Process,
    /// Runc is installed; `rootless` says whether Bun lacks root.
    #[cfg(target_os = "linux")]
    Runc {
        /// Whether Bun runs without root and needs user namespaces.
        rootless: bool,
    },
}

/// Auto-detect the best available runtime.
///
/// On Linux, selects runc when installed. Otherwise selects native processes.
/// For 0.1.0, macOS containers use managed Linux VMs; the direct Apple adapter
/// is excluded pending daemon-command recovery. The caller builds the runtime
/// with its configured storage and owner executable.
pub async fn detect_runtime() -> DetectedRuntime {
    #[cfg(target_os = "linux")]
    if which_exists("runc").await {
        return DetectedRuntime::Runc {
            rootless: rootless::is_rootless(),
        };
    }
    DetectedRuntime::Process
}

/// Check if a binary exists in PATH.
#[cfg(target_os = "linux")]
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
    #[test]
    fn runtime_generation_wire_values_require_canonical_fingerprints() {
        let original = RuntimeGeneration::process("private-generation");
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(
            serde_json::from_str::<RuntimeGeneration>(&json).unwrap(),
            original
        );
        for invalid in [
            String::new(),
            "private-owner-token".into(),
            "A".repeat(64),
            "g".repeat(64),
            "a".repeat(65),
        ] {
            assert!(
                serde_json::from_value::<RuntimeGeneration>(serde_json::json!(invalid)).is_err()
            );
        }
    }

    #[test]
    fn runtime_generation_fingerprints_never_expose_or_confuse_owner_tokens() {
        let token = "1234567890abcdef1234567890abcdef";
        let process = super::RuntimeGeneration::process(token);
        let runc = super::RuntimeGeneration::runc(token);
        assert_ne!(process.as_str(), token);
        assert_ne!(runc.as_str(), token);
        assert_ne!(process, runc);
        assert_eq!(process, super::RuntimeGeneration::process(token));
        assert_ne!(
            process,
            super::RuntimeGeneration::process("abcdef1234567890abcdef1234567890")
        );
    }

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
    fn parse_rejects_bare_id_without_namespace() {
        assert!(InstanceIdentity::parse("api-0").is_none());
    }
}
