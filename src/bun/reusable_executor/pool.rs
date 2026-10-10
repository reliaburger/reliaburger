//! Bounded owned containers, with one independently limited command per slot.
use super::timings::{Phase, TimingSnapshot, Timings};
use super::{ExecutorError, ExecutorKey, ExecutorProfile, HELPER_MEMORY_BYTES, protocol};
use crate::bun::execution_budget::{ExecutionBudget, ResourceLease};
use crate::bun::task_executor::{Attempt, AttemptOutcome, CapturedOutput, TaskInvocation};
use crate::config::job::JobSpec;
use crate::grill::runc::RuncGrill;
use crate::grill::{ContainerState, Grill, InstanceId, InstanceIdentity};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rb-executor-helper"));
const HOST_HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rb-host-executor-helper"));
#[derive(Clone)]
enum Runtime {
    Container(RuncGrill),
    Host(crate::grill::ProcessGrill),
}
impl Runtime {
    async fn state(&self, id: &InstanceId) -> Result<ContainerState, crate::grill::GrillError> {
        match self {
            Self::Container(runtime) => runtime.state(id).await,
            Self::Host(runtime) => runtime.state(id).await,
        }
    }
    fn directory(&self) -> Result<PathBuf, ExecutorError> {
        match self {
            Self::Container(runtime) => Ok(runtime.executor_directory()),
            Self::Host(runtime) => Ok(runtime.executor_directory()?),
        }
    }
    fn host(&self) -> bool {
        matches!(self, Self::Host(_))
    }
}
const IDLE: Duration = Duration::from_secs(1);
/// How long a caller waits for an executor to retire. A task stuck in an
/// uninterruptible kernel wait (NFS, FUSE, a hung disk) can outlive any
/// signal; past this, the slot and its lease stay quarantined while the
/// eviction loop keeps retrying, and the caller gets its own outcome back.
const RETIREMENT_DEADLINE: Duration = Duration::from_secs(10);
pub(crate) struct CommandReporting<'a> {
    pub sink: Option<&'a tokio::sync::mpsc::Sender<crate::ketchup::types::LogRecord>>,
    pub singletons: &'a std::sync::Mutex<
        std::collections::BTreeMap<u64, Arc<crate::bun::task_runtime::SingletonRuntime>>,
    >,
}

struct Slot {
    busy: bool,
    /// The run whose caller has this slot checked out, from checkout to
    /// release, including setup and cleanup.
    holder: Option<u64>,
    active_run: Option<u64>,
    key: Option<ExecutorKey>,
    context: Option<Context>,
    /// A busy slot whose executor missed [`RETIREMENT_DEADLINE`].
    retiring: Option<Context>,
}
struct Context {
    key: ExecutorKey,
    id: InstanceId,
    directory: PathBuf,
    base: PathBuf,
    image: Option<crate::grill::image::PulledImage>,
    socket_path: Option<PathBuf>,
    connection: Option<UnixStream>,
    lease: ResourceLease,
    sequence: u64,
    idle_since: tokio::time::Instant,
    #[cfg(feature = "ebpf")]
    namespace: Option<crate::bun::task_namespace::NamespaceLease>,
}
/// A dropped in-flight future leaves its slot and resource lease quarantined.
pub(crate) struct ReusablePool<G> {
    lifecycle: G,
    runtime: Runtime,
    timings: Timings,
    prefix: String,
    budget: Arc<ExecutionBudget>,
    slots: Mutex<Vec<Slot>>,
    changed: Notify,
    #[cfg(feature = "ebpf")]
    policy: Option<Arc<crate::bun::task_namespace::TaskNamespacePolicy>>,
}
impl<G: Grill + Clone + 'static> ReusablePool<G> {
    pub(crate) fn new(
        runtime: RuncGrill,
        lifecycle: G,
        prefix: String,
        count: usize,
        budget: Arc<ExecutionBudget>,
        #[cfg(feature = "ebpf")] policy: Option<
            Arc<crate::bun::task_namespace::TaskNamespacePolicy>,
        >,
    ) -> Arc<Self> {
        Self::with_runtime(
            Runtime::Container(runtime),
            lifecycle,
            prefix,
            count,
            budget,
            #[cfg(feature = "ebpf")]
            policy,
        )
    }
    pub(crate) fn new_host(
        runtime: crate::grill::ProcessGrill,
        lifecycle: G,
        prefix: String,
        count: usize,
        budget: Arc<ExecutionBudget>,
        #[cfg(feature = "ebpf")] policy: Option<
            Arc<crate::bun::task_namespace::TaskNamespacePolicy>,
        >,
    ) -> Arc<Self> {
        Self::with_runtime(
            Runtime::Host(runtime),
            lifecycle,
            prefix,
            count,
            budget,
            #[cfg(feature = "ebpf")]
            policy,
        )
    }
    fn with_runtime(
        runtime: Runtime,
        lifecycle: G,
        prefix: String,
        count: usize,
        budget: Arc<ExecutionBudget>,
        #[cfg(feature = "ebpf")] policy: Option<
            Arc<crate::bun::task_namespace::TaskNamespacePolicy>,
        >,
    ) -> Arc<Self> {
        let pool = Arc::new(Self {
            runtime,
            timings: Timings::default(),
            lifecycle,
            prefix,
            budget,
            slots: Mutex::new(
                (0..count.clamp(1, super::MAX_EXECUTOR_SLOTS))
                    .map(|_| Slot {
                        busy: false,
                        holder: None,
                        active_run: None,
                        key: None,
                        context: None,
                        retiring: None,
                    })
                    .collect(),
            ),
            changed: Notify::new(),
            #[cfg(feature = "ebpf")]
            policy,
        });
        let weak = Arc::downgrade(&pool);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let Some(pool) = weak.upgrade() else { break };
                pool.evict_idle().await;
            }
        });
        pool
    }
    pub(crate) fn timings(&self) -> TimingSnapshot {
        self.timings.snapshot()
    }
    async fn evict_idle(&self) {
        let stuck: Vec<(usize, Context)> = self
            .slots
            .lock()
            .await
            .iter_mut()
            .enumerate()
            .filter_map(|(index, slot)| Some((index, slot.retiring.take()?)))
            .collect();
        for (index, mut context) in stuck {
            if self.retirement_step(&mut context).await
                && self.finish_retirement(&mut context).await
            {
                self.release(index, None).await;
            } else {
                self.slots.lock().await[index].retiring = Some(context);
            }
        }
        loop {
            let victim = {
                let mut slots = self.slots.lock().await;
                slots.iter_mut().enumerate().find_map(|(index, slot)| {
                    if !slot.busy
                        && slot.context.as_ref().is_some_and(|context| {
                            context.idle_since.elapsed() >= IDLE || self.budget.has_waiters()
                        })
                    {
                        slot.busy = true;
                        Some((index, slot.context.take()))
                    } else {
                        None
                    }
                })
            };
            let Some((index, Some(context))) = victim else {
                break;
            };
            self.retire_and_release(index, context).await;
        }
    }
    async fn release(&self, index: usize, context: Option<Context>) {
        let mut slots = self.slots.lock().await;
        slots[index].key = context.as_ref().map(|context| context.key);
        slots[index].context = context;
        slots[index].busy = false;
        slots[index].holder = None;
        slots[index].active_run = None;
        self.changed.notify_waiters();
    }
    /// Slots this run's callers have checked out. Unlike
    /// [`Self::active_commands`], it doesn't miss commands that start and exit
    /// between samples, and unlike the executor's own running count, it leaves
    /// out callers still waiting for a slot or for admission.
    pub(crate) async fn busy_slots(&self, run: u64) -> u64 {
        self.slots
            .lock()
            .await
            .iter()
            .filter(|slot| slot.holder == Some(run))
            .count() as u64
    }
    pub(crate) async fn active_commands(&self, run: u64) -> u64 {
        self.slots
            .lock()
            .await
            .iter()
            .filter(|slot| slot.active_run == Some(run))
            .count() as u64
    }
    pub(crate) async fn available_slots(
        &self,
        key: Option<ExecutorKey>,
        reservation: crate::meat::Resources,
    ) -> u32 {
        let slots = self.slots.lock().await;
        if self.budget.has_waiters() {
            return 0;
        }
        let idle = slots.iter().filter(|slot| !slot.busy).count();
        let warm = slots
            .iter()
            .filter(|slot| {
                !slot.busy
                    && key.is_some_and(|key| {
                        slot.context
                            .as_ref()
                            .is_some_and(|context| context.key == key)
                    })
            })
            .count();
        let available = self.budget.available();
        let cold = (available.cpu_millicores / reservation.cpu_millicores)
            .min(available.memory_bytes / reservation.memory_bytes)
            .min((idle - warm) as u64);
        (warm as u64 + cold) as u32
    }
    async fn slot(
        &self,
        key: ExecutorKey,
        reservation: crate::meat::Resources,
        holder: Option<u64>,
        cancel: &CancellationToken,
    ) -> Option<(usize, Option<Context>, Option<ResourceLease>)> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if cancel.is_cancelled() {
                return None;
            }
            let mut slots = self.slots.lock().await;
            let warm = (!self.budget.has_waiters())
                .then(|| {
                    slots.iter().position(|slot| {
                        !slot.busy
                            && slot
                                .context
                                .as_ref()
                                .is_some_and(|context| context.key == key)
                    })
                })
                .flatten();
            let selected = warm.or_else(|| slots.iter().position(|slot| !slot.busy));
            if let Some(index) = selected {
                // Charge an empty context before image I/O can let another
                // caller spend the same apparent capacity. If a compatible
                // context is already charged or preparing, wait for it rather
                // than enqueueing a self-inflicted budget waiter that evicts it.
                let lease = if slots[index].context.is_none() {
                    match self.budget.try_acquire_executor(reservation) {
                        Some(lease) => Some(lease),
                        None if slots.iter().any(|slot| slot.busy && slot.key == Some(key)) => {
                            drop(slots);
                            tokio::select! { biased; () = cancel.cancelled() => return None, () = changed => {} }
                            continue;
                        }
                        None => None,
                    }
                } else {
                    None
                };
                slots[index].busy = true;
                slots[index].holder = holder;
                slots[index].key = Some(key);
                return Some((index, slots[index].context.take(), lease));
            }
            drop(slots);
            tokio::select! { biased; () = cancel.cancelled() => return None, () = changed => {} }
        }
    }
    /// One non-blocking retirement attempt: true once the helper has stopped
    /// and its task group is empty.
    async fn retirement_step(&self, context: &mut Context) -> bool {
        context.connection.take();
        match self.runtime.state(&context.id).await {
            Ok(ContainerState::Stopped) | Err(crate::grill::GrillError::NotFound { .. }) => {}
            _ => {
                let _ = self.lifecycle.kill(&context.id).await;
                return false;
            }
        }
        // The runtime owns helper retirement; Bun owns the sibling task group.
        // Removing/reusing a group requires emptiness, independently of PID 1.
        match empty_task(&context.base.join("task")) {
            Ok(()) => true,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    }
    /// Retire within [`RETIREMENT_DEADLINE`]; false leaves the executor running.
    async fn retire(&self, context: &mut Context) -> bool {
        let deadline = tokio::time::Instant::now() + RETIREMENT_DEADLINE;
        while !self.retirement_step(context).await {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.finish_retirement(context).await
    }
    /// Remove the emptied groups and files, then return the lease. The task
    /// group must really be gone: one that survived `cgroup.kill` must never
    /// be reused (see `prepare_cgroups`).
    async fn finish_retirement(&self, context: &mut Context) -> bool {
        #[cfg(feature = "ebpf")]
        if let Some(namespace) = context.namespace.take() {
            namespace.retired().await;
        }
        for directory in [
            context.base.join("task"),
            context.base.join("helper"),
            context.base.clone(),
        ] {
            match tokio::fs::remove_dir(&directory).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    eprintln!(
                        "executor {}: cannot remove {}: {error}",
                        context.id.0,
                        directory.display()
                    );
                    return false;
                }
            }
        }
        if let Some(path) = context.socket_path.take() {
            let _ = tokio::fs::remove_file(path).await;
        }
        let _ = tokio::fs::remove_dir_all(&context.directory).await;
        context.lease.confirm_retired();
        true
    }
    /// Retire and free the slot, or quarantine it with its lease for the
    /// eviction loop to retry.
    async fn retire_and_release(&self, index: usize, mut context: Context) {
        if self.retire(&mut context).await {
            self.release(index, None).await;
        } else {
            self.quarantine(index, context).await;
        }
    }
    async fn quarantine(&self, index: usize, context: Context) {
        eprintln!(
            "executor {} did not retire within {}s; its slot and reservation stay quarantined until it does",
            context.id.0,
            RETIREMENT_DEADLINE.as_secs()
        );
        let mut slots = self.slots.lock().await;
        slots[index].holder = None;
        slots[index].retiring = Some(context);
    }
    async fn start_host(
        &self,
        context: &mut Context,
        template: &JobSpec,
        profile: ExecutorProfile,
    ) -> Result<(), ExecutorError> {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::PermissionsExt;
        match self.runtime.state(&context.id).await {
            Ok(ContainerState::Stopped) | Err(crate::grill::GrillError::NotFound { .. }) => {}
            _ => {
                return Err(ExecutorError::Configuration(
                    "old host executor has not retired",
                ));
            }
        }
        match tokio::fs::remove_dir_all(&context.directory).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::create_dir_all(&context.directory)?;
        std::fs::set_permissions(&context.directory, std::fs::Permissions::from_mode(0o700))?;
        let helper = context.directory.join("helper");
        install_helper(helper.clone(), HOST_HELPER).await?;
        // Socket credentials and the durable owner's unreaped helper identity
        // authenticate this short address. Hash the executor directory too,
        // so two Buns on one host never share a name.
        let mut name = Sha256::new();
        name.update(context.directory.as_os_str().as_encoded_bytes());
        let socket = host_socket_directory()?.join(&hex::encode(name.finalize())[..32]);
        match std::fs::symlink_metadata(&socket) {
            Ok(metadata) => {
                use std::os::unix::fs::{FileTypeExt, MetadataExt};
                if !metadata.file_type().is_socket()
                    || metadata.uid() != crate::grill::userns::EXECUTOR_HOST_UID
                {
                    return Err(ExecutorError::Configuration("foreign host executor socket"));
                }
                std::fs::remove_file(&socket)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(&socket)?;
        context.socket_path = Some(socket.clone());
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        std::os::unix::fs::lchown(
            &socket,
            Some(crate::grill::userns::EXECUTOR_HOST_UID),
            Some(crate::grill::userns::EXECUTOR_HOST_UID),
        )?;
        #[cfg(feature = "ebpf")]
        if let Some(policy) = &self.policy {
            context.namespace = Some(
                policy
                    .acquire(
                        template.namespace.as_deref().unwrap_or("default"),
                        &context.base,
                    )
                    .await?,
            );
        }
        prepare_cgroups(&context.base, profile)?;
        let mut spec = crate::grill::oci::generate_job_oci_spec(
            "executor",
            template.namespace.as_deref().unwrap_or("default"),
            template,
            "/unused",
            None,
        );
        spec.reusable_executor = false;
        spec.linux.resources = None;
        spec.process.env.clear();
        spec.process.args = vec![
            helper.to_string_lossy().into_owned(),
            socket.to_string_lossy().into_owned(),
            context
                .base
                .join("helper/cgroup.procs")
                .to_string_lossy()
                .into_owned(),
        ];
        spec.process.overrides = None;
        self.lifecycle.create(&context.id, &spec).await?;
        self.lifecycle.start(&context.id).await?;
        let (mut connection, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .map_err(|_| ExecutorError::Protocol("host helper connection timed out".into()))??;
        let peer = connection.peer_cred()?;
        let peer_pid = peer.pid().and_then(|pid| u32::try_from(pid).ok());
        let Runtime::Host(runtime) = &self.runtime else {
            return Err(ExecutorError::Configuration("host backend missing"));
        };
        if peer.uid() != crate::grill::userns::EXECUTOR_HOST_UID
            || peer_pid.is_none()
            || runtime.pid(&context.id).await? != peer_pid
        {
            return Err(ExecutorError::Protocol(
                "host helper socket owner mismatch".into(),
            ));
        }
        let mut magic = [0; 8];
        connection.read_exact(&mut magic).await?;
        if &magic != b"RBEX0001" {
            return Err(ExecutorError::Protocol(
                "host helper protocol mismatch".into(),
            ));
        }
        std::fs::remove_file(&socket)?;
        context.socket_path = None;
        let native = connection.into_std()?;
        protocol::send_directory(&native, &std::fs::File::open(context.base.join("task"))?)?;
        context.connection = Some(UnixStream::from_std(native)?);
        Ok(())
    }
    async fn start(
        &self,
        context: &mut Context,
        template: &JobSpec,
        profile: ExecutorProfile,
    ) -> Result<(), ExecutorError> {
        if self.runtime.host() {
            return self.start_host(context, template, profile).await;
        }
        let Runtime::Container(runtime) = &self.runtime else {
            return Err(ExecutorError::Configuration("container backend missing"));
        };
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        match self.runtime.state(&context.id).await {
            Ok(ContainerState::Stopped) | Err(crate::grill::GrillError::NotFound { .. }) => {}
            _ => {
                return Err(ExecutorError::Configuration(
                    "old executor ownership has not retired",
                ));
            }
        }
        match tokio::fs::remove_dir_all(&context.directory).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        tokio::fs::create_dir_all(&context.directory).await?;
        std::fs::set_permissions(&context.directory, std::fs::Permissions::from_mode(0o700))?;
        std::os::unix::fs::lchown(
            &context.directory,
            Some(crate::grill::userns::EXECUTOR_HOST_UID),
            Some(crate::grill::userns::EXECUTOR_HOST_UID),
        )?;
        let bootstrap = context.directory.join("bootstrap");
        std::fs::create_dir(&bootstrap)?;
        std::fs::set_permissions(&bootstrap, std::fs::Permissions::from_mode(0o755))?;
        let helper = bootstrap.join("helper");
        install_helper(helper.clone(), HELPER).await?;
        let source = std::fs::File::open(&context.directory)?;
        // A /proc/fd alias keeps AF_UNIX's address bounded, independent of the
        // configured data-directory length. The source descriptor stays open.
        let socket_path = format!("/proc/self/fd/{}/control", source.as_raw_fd());
        let listener = UnixListener::bind(&socket_path)?;
        std::fs::set_permissions(
            context.directory.join("control"),
            std::fs::Permissions::from_mode(0o600),
        )?;
        std::os::unix::fs::lchown(
            context.directory.join("control"),
            Some(crate::grill::userns::EXECUTOR_HOST_UID),
            Some(crate::grill::userns::EXECUTOR_HOST_UID),
        )?;
        #[cfg(feature = "ebpf")]
        if let Some(policy) = &self.policy {
            context.namespace = Some(
                policy
                    .acquire(
                        template.namespace.as_deref().unwrap_or("default"),
                        &context.base,
                    )
                    .await?,
            );
        }
        prepare_cgroups(&context.base, profile)?;
        let path = context.base.join("helper");
        let mut spec = crate::grill::oci::generate_job_oci_spec(
            "executor",
            template.namespace.as_deref().unwrap_or("default"),
            template,
            path.to_str()
                .ok_or(ExecutorError::Configuration("non-UTF-8 cgroup"))?,
            None,
        );
        spec.reusable_executor = true;
        spec.root.readonly = true;
        spec.process.env.clear();
        spec.process.args = vec![
            "/run/rb-bootstrap/helper".into(),
            "/run/rb-executor/control".into(),
        ];
        spec.process.overrides = Some(crate::grill::oci::ProcessOverrides {
            command: spec.process.args.clone(),
            working_dir: Some("/".into()),
            user: Some(0),
            group: Some(0),
            ..Default::default()
        });
        let mut helper_profile = template.clone();
        helper_profile.cpu = Some(crate::config::types::ResourceRange {
            request: super::HELPER_CPU_REQUEST,
            limit: 100,
        });
        helper_profile.memory = Some(crate::config::types::ResourceRange {
            request: HELPER_MEMORY_BYTES,
            limit: HELPER_MEMORY_BYTES,
        });
        spec.linux.resources = crate::grill::oci::generate_job_oci_spec(
            "helper",
            "default",
            &helper_profile,
            path.to_str()
                .ok_or(ExecutorError::Configuration("non-UTF-8 helper cgroup"))?,
            None,
        )
        .linux
        .resources;
        // File targets exist before the private directory binds. A helper
        // file bind at the image root raced concurrent runc initialisations.
        // runc must stat the bootstrap before installing process capabilities;
        // expose only that public static binary, retaining the control directory
        // as mode 0700 owned by the protected helper uid.
        for (destination, source) in [
            ("/run/rb-bootstrap", bootstrap),
            ("/run/rb-executor", context.directory.clone()),
        ] {
            spec.mounts.push(crate::grill::oci::OciMount {
                destination: destination.into(),
                source: Some(source),
                mount_type: Some("bind".into()),
                options: vec!["bind".into(), "ro".into(), "nosuid".into(), "nodev".into()],
            });
        }
        spec.mounts.push(crate::grill::oci::OciMount {
            destination: "/dev/shm".into(),
            source: None,
            mount_type: Some("tmpfs".into()),
            options: vec![
                "nosuid".into(),
                "nodev".into(),
                "noexec".into(),
                "mode=1777".into(),
                "size=16m".into(),
            ],
        });
        self.lifecycle.create(&context.id, &spec).await?;
        self.lifecycle.start(&context.id).await?;
        let (mut connection, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .map_err(|_| ExecutorError::Protocol("helper connection timed out".into()))??;
        let peer = connection.peer_cred()?;
        if peer.uid() != crate::grill::userns::EXECUTOR_HOST_UID {
            return Err(ExecutorError::Protocol(
                "helper socket credentials mismatch".into(),
            ));
        }
        let pid = peer
            .pid()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or(ExecutorError::Configuration("missing helper peer pid"))?;
        runtime.authenticate_executor(&context.id, pid).await?;
        let mut magic = [0; 8];
        connection.read_exact(&mut magic).await?;
        if &magic != b"RBEX0001" {
            return Err(ExecutorError::Protocol("helper protocol mismatch".into()));
        }
        std::fs::remove_file(context.directory.join("control"))?;
        let native = connection.into_std()?;
        protocol::send_directory(&native, &std::fs::File::open(context.base.join("task"))?)?;
        context.connection = Some(UnixStream::from_std(native)?);
        Ok(())
    }
    pub(crate) async fn run<F>(
        &self,
        task: &TaskInvocation,
        mut template: JobSpec,
        timeout: Duration,
        cancel: &CancellationToken,
        refresh: Option<impl Fn() -> F + Send + Sync>,
        reporting: CommandReporting<'_>,
    ) -> Attempt
    where
        F: std::future::Future<Output = Result<JobSpec, String>> + Send,
    {
        let CommandReporting { sink, singletons } = reporting;
        let failed = |error: ExecutorError| Attempt {
            outcome: AttemptOutcome::SpawnFailed {
                reason: error.to_string(),
            },
            output: CapturedOutput::default(),
            ran: None,
        };
        let key = match ExecutorKey::new(&template) {
            Ok(key) => key,
            Err(error) => return failed(error),
        };
        let profile = match ExecutorProfile::new(&template) {
            Ok(profile) => profile,
            Err(error) => return failed(error),
        };
        if !self.budget.capacity().fits(&profile.reservation) {
            return failed(ExecutorError::Configuration(
                "profile plus helper exceeds node capacity",
            ));
        }
        let mut admission_timer = Some(self.timings.start(Phase::Admission));
        let Some((index, existing, reservation)) = self
            .slot(
                key,
                profile.reservation,
                task.run.as_ref().map(|run| run.batch_id),
                cancel,
            )
            .await
        else {
            return Attempt {
                outcome: AttemptOutcome::Cancelled,
                output: CapturedOutput::default(),
                ran: None,
            };
        };
        let mut context = match existing {
            Some(mut old) if old.key != key || self.budget.has_waiters() => {
                if !self.retire(&mut old).await {
                    self.quarantine(index, old).await;
                    return failed(ExecutorError::Configuration(
                        "the slot's previous executor has not retired",
                    ));
                }
                None
            }
            other => other,
        };
        // Match fresh execution: waiting for compatible slots or admission is
        // not command run time. Cold preparation starts its clock only once the
        // complete profile is charged; warm work starts after borrowing its slot.
        let mut deadline = (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout);
        if context.is_none() {
            let lease = match reservation {
                Some(lease) => Some(lease),
                None => self.budget.acquire(profile.reservation, cancel).await,
            };
            let Some(lease) = lease else {
                self.release(index, None).await;
                return Attempt {
                    outcome: AttemptOutcome::Cancelled,
                    output: CapturedOutput::default(),
                    ran: None,
                };
            };
            deadline = (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout);
            drop(admission_timer.take());
            let startup_timer = self.timings.start(Phase::Startup);
            let image = match &self.runtime {
                Runtime::Host(_) => None,
                Runtime::Container(runtime) => match runtime
                    .image_store()
                    .pull_and_unpack(template.image.as_deref().unwrap_or_default())
                    .await
                {
                    Ok(image) => Some(image),
                    Err(error) => {
                        self.release(index, None).await;
                        return failed(ExecutorError::Protocol(error.to_string()));
                    }
                },
            };
            let namespace = template.namespace.as_deref().unwrap_or("default");
            let app = format!(
                "executor-{}-{}",
                self.prefix,
                if self.runtime.host() { "host" } else { "reuse" }
            );
            let id = InstanceIdentity::new(namespace, &app, index as u32).instance_id();
            let base = match crate::grill::cgroup::instance_cgroup_path(namespace, &app, &id) {
                Ok(base) => base,
                Err(error) => {
                    self.release(index, None).await;
                    return failed(error.into());
                }
            };
            let mut new = Context {
                key,
                directory: match self.runtime.directory() {
                    Ok(directory) => directory.join(&id.0),
                    Err(error) => {
                        self.release(index, None).await;
                        return failed(error);
                    }
                },
                id,
                base,
                image,
                socket_path: None,
                connection: None,
                lease: lease.quarantine_on_drop(),
                sequence: 0,
                idle_since: tokio::time::Instant::now(),
                #[cfg(feature = "ebpf")]
                namespace: None,
            };
            let start = self.start(&mut new, &template, profile).await;
            if let Err(error) = start {
                self.retire_and_release(index, new).await;
                return failed(error);
            }
            context = Some(new);
            drop(startup_timer);
        }
        drop(admission_timer.take());
        let Some(mut context) = context else {
            self.release(index, None).await;
            return failed(ExecutorError::Configuration("missing executor context"));
        };
        if let Some(refresh) = refresh {
            let refreshed = refresh().await.ok().filter(|template| {
                ExecutorKey::new(template).is_ok_and(|live| live == context.key)
            });
            let Some(refreshed) = refreshed else {
                self.retire_and_release(index, context).await;
                return failed(ExecutorError::Configuration(
                    "namespace credentials no longer authorise this command",
                ));
            };
            template = refreshed;
        }
        let mut output = CapturedOutput::default();
        template.command = Some(task.args.clone());
        if let Some(script) = &mut template.script {
            *script = script.replace("{index}", &task.index.to_string());
        }
        for (key, value) in &task.env {
            template.env.insert(
                key.clone(),
                crate::config::types::EnvValue::Plain(value.clone()),
            );
        }
        let mut process = crate::grill::oci::generate_job_oci_spec(
            "task",
            "default",
            &template,
            "/sys/fs/cgroup/unused",
            None,
        )
        .process;
        if self.runtime.host() {
            // Preserve the existing owned host contract: commands run as Bun's
            // effective user, while the helper has a separate protected identity.
            process.user.uid = nix::unistd::geteuid().as_raw();
            process.user.gid = nix::unistd::getegid().as_raw();
            // The helper execs with exactly this environment.
            process.env = crate::grill::process::host_environment(&process.env)
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
        }
        let preparation = match &context.image {
            Some(image) => crate::grill::image_config::resolve_process(
                &mut process,
                &image.config,
                &image.rootfs,
            ),
            None => Ok(()),
        }
        .map_err(|error| ExecutorError::Protocol(error.to_string()))
        .and_then(|()| {
            context
                .sequence
                .checked_add(1)
                .ok_or(ExecutorError::Configuration("executor sequence exhausted"))
        })
        .and_then(|sequence| protocol::command(sequence, &process).map(|bytes| (sequence, bytes)));
        let (sequence, mut bytes) = match preparation {
            Ok(encoded) => encoded,
            Err(error) => {
                self.retire_and_release(index, context).await;
                return failed(error);
            }
        };
        let run_id = task.run.as_ref().map(|run| run.batch_id);
        // Only the command's own run time, from the helper's start receipt to
        // its exit: not slot queueing, admission, image pulls or cleanup.
        let mut ran = None;
        let observe = async {
            let socket = context
                .connection
                .as_mut()
                .ok_or(ExecutorError::Configuration("executor connection missing"))?;
            #[cfg(feature = "ebpf")]
            if let Some(policy) = &self.policy {
                policy
                    .check(template.namespace.as_deref().unwrap_or("default"))
                    .await?;
            }
            let mut command_timer = Some(self.timings.start(Phase::Command));
            socket.write_all(&bytes).await?;
            // The encoded buffer holds live secrets only during submission.
            bytes.fill(0);
            let mut started = false;
            let mut began = tokio::time::Instant::now();
            let mut logs = CommandLogs::new(task, &template, sink);
            if let Some(run) = task
                .run
                .as_ref()
                .filter(|run| run.is_singleton())
                .map(|run| run.batch_id)
            {
                let stream = super::CommandLogStream::new();
                logs.live = Some(stream.clone());
                singletons
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        run,
                        Arc::new(crate::bun::task_runtime::SingletonRuntime {
                            instance: context.id.clone(),
                            accepting: std::sync::atomic::AtomicBool::new(true),
                            followers: std::sync::atomic::AtomicUsize::new(0),
                            changed: Notify::new(),
                            retired: CancellationToken::new(),
                            command_logs: Some(stream),
                        }),
                    );
            }
            loop {
                let event = protocol::receive(socket, sequence).await?;
                match event {
                    protocol::Event::Started if !started => {
                        started = true;
                        began = tokio::time::Instant::now();
                        self.slots.lock().await[index].active_run = run_id;
                    }
                    protocol::Event::Output { stream, bytes } if started => {
                        output.push(&bytes);
                        logs.push(stream, &bytes).await;
                    }
                    protocol::Event::Exited(code) if started => {
                        ran = Some(began.elapsed());
                        logs.finish().await;
                        drop(command_timer.take());
                        let _cleanup_timer = self.timings.start(Phase::Cleanup);
                        protocol::cleanup(socket, sequence).await?;
                        if protocol::receive(socket, sequence).await? != protocol::Event::Ready {
                            return Err(ExecutorError::Protocol("missing cleanup receipt".into()));
                        }
                        wait_empty_task(&context.base.join("task")).await?;
                        return Ok(if code < 0 {
                            AttemptOutcome::Signalled { signal: -code }
                        } else {
                            AttemptOutcome::Exited { code }
                        });
                    }
                    protocol::Event::SpawnFailed(error) if !started => {
                        drop(command_timer.take());
                        let _cleanup_timer = self.timings.start(Phase::Cleanup);
                        protocol::cleanup(socket, sequence).await?;
                        if protocol::receive(socket, sequence).await? != protocol::Event::Ready {
                            return Err(ExecutorError::Protocol(
                                "missing failed-spawn cleanup".into(),
                            ));
                        }
                        wait_empty_task(&context.base.join("task")).await?;
                        return Ok(AttemptOutcome::SpawnFailed {
                            reason: format!(
                                "atomic cgroup launch: {}",
                                std::io::Error::from_raw_os_error(error as i32)
                            ),
                        });
                    }
                    _ => {
                        return Err(ExecutorError::Protocol(
                            "unexpected executor lifecycle frame".into(),
                        ));
                    }
                }
            }
        };
        let policy_monitor = async {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                #[cfg(feature = "ebpf")]
                if let Some(policy) = &self.policy {
                    policy
                        .check(template.namespace.as_deref().unwrap_or("default"))
                        .await?;
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), std::io::Error>(())
        };
        let outcome = tokio::select! { biased;
            () = cancel.cancelled() => AttemptOutcome::Cancelled,
            () = crate::bun::task_executor::wait_deadline(deadline) => AttemptOutcome::TimedOut,
            result = policy_monitor => AttemptOutcome::Unknown { reason: result.err().map_or_else(|| "namespace supervision stopped".into(), |error| error.to_string()) },
            result = observe => match result { Ok(outcome) => outcome, Err(error) => AttemptOutcome::Unknown { reason: error.to_string() } },
        };
        bytes.fill(0);
        if matches!(
            outcome,
            AttemptOutcome::Cancelled | AttemptOutcome::TimedOut | AttemptOutcome::Unknown { .. }
        ) || self.budget.has_waiters()
        {
            self.retire_and_release(index, context).await;
        } else {
            context.sequence = sequence;
            context.idle_since = tokio::time::Instant::now();
            self.release(index, Some(context)).await;
        }
        Attempt {
            outcome,
            output,
            ran,
        }
    }
}

/// Write an executor helper binary and make it and its directory entry
/// durable. The write and both fsyncs are blocking file I/O, so they run on
/// the blocking pool, not an async worker thread.
async fn install_helper(path: PathBuf, bytes: &'static [u8]) -> Result<(), ExecutorError> {
    use std::os::unix::fs::OpenOptionsExt;
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o555)
            .open(&path)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })
    .await
    .map_err(|error| ExecutorError::Protocol(error.to_string()))??;
    Ok(())
}

/// Where host helpers' control sockets live. The helper's reserved uid can
/// traverse it but only root can create entries, so no other local user can
/// claim a socket name first and block that executor slot, as anyone could
/// in `/tmp`. Bun's 0700 data directory is closed to the helper, and a short
/// fixed path keeps sockets within the 108-byte `sun_path` limit.
const HOST_SOCKET_DIRECTORY: &str = "/run/reliaburger/host-executors";

fn host_socket_directory() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let directory = PathBuf::from(HOST_SOCKET_DIRECTORY);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o711)
        .create(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o711))?;
    for path in directory.ancestors().take(2) {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir()
            || metadata.uid() != nix::unistd::geteuid().as_raw()
            || metadata.mode() & 0o022 != 0
        {
            return Err(std::io::Error::other(format!(
                "{} must be a directory only Bun can write",
                path.display()
            )));
        }
    }
    Ok(directory)
}

fn prepare_cgroups(base: &Path, profile: ExecutorProfile) -> std::io::Result<()> {
    // Enable controllers only on empty ancestors. The helper and task are
    // sibling leaves; PID 1 never violates cgroup v2's internal-process rule.
    let mut ancestors = Vec::new();
    let mut current = Some(base);
    while let Some(path) = current {
        if path == Path::new("/sys/fs/cgroup") {
            break;
        }
        ancestors.push(path);
        current = path.parent();
    }
    for directory in ancestors.into_iter().rev() {
        std::fs::create_dir_all(directory)?;
        std::fs::write(
            directory.join("cgroup.subtree_control"),
            "+cpu +memory +pids",
        )?;
    }
    std::fs::create_dir_all(base.join("helper"))?;
    // Never reuse a task group. Retirement writes `cgroup.kill`, and on Linux
    // 6.8 a group keeps that kill sequence, so a later `CLONE_INTO_CGROUP`
    // child would be killed at birth. Remove a leftover (only possible when
    // empty) and always create a fresh directory.
    let task = base.join("task");
    match std::fs::remove_dir(&task) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir(&task)?;
    std::fs::write(
        task.join("cpu.max"),
        if profile.cpu.limit < 10 {
            format!("{} 1000000", profile.cpu.limit * 1000)
        } else {
            crate::grill::cpu_max_from_millicores(profile.cpu.limit)
        },
    )?;
    std::fs::write(
        task.join("cpu.weight"),
        crate::grill::cgroup::cpu_weight_from_millicores(profile.cpu.request).to_string(),
    )?;
    std::fs::write(task.join("memory.max"), profile.memory.limit.to_string())?;
    std::fs::write(task.join("memory.swap.max"), "0")?;
    std::fs::write(task.join("memory.high"), profile.memory.request.to_string())?;
    std::fs::write(task.join("memory.oom.group"), "1")?;
    std::fs::write(task.join("pids.max"), "256")?;
    std::fs::write(
        base.join("helper/cpu.max"),
        crate::grill::cpu_max_from_millicores(100),
    )?;
    std::fs::write(
        base.join("helper/cpu.weight"),
        crate::grill::cgroup::cpu_weight_from_millicores(super::HELPER_CPU_REQUEST).to_string(),
    )?;
    std::fs::write(
        base.join("helper/memory.high"),
        HELPER_MEMORY_BYTES.to_string(),
    )?;
    std::fs::write(base.join("helper/memory.swap.max"), "0")?;
    std::fs::write(base.join("helper/pids.max"), "16")?;
    std::fs::write(
        base.join("helper/memory.max"),
        HELPER_MEMORY_BYTES.to_string(),
    )?;
    let uid = Some(crate::grill::userns::EXECUTOR_HOST_UID);
    for path in [
        base.to_path_buf(),
        base.join("cgroup.procs"),
        task.clone(),
        task.join("cgroup.procs"),
        task.join("cgroup.threads"),
    ] {
        std::os::unix::fs::lchown(path, uid, uid)?;
    }
    Ok(())
}
fn empty_task(task: &Path) -> std::io::Result<()> {
    std::fs::write(task.join("cgroup.kill"), "1")?;
    let events = std::fs::read_to_string(task.join("cgroup.events"))?;
    if !events.lines().any(|line| line == "populated 0") {
        return Err(std::io::Error::other("task cgroup still populated"));
    }
    Ok(())
}

/// After the helper's cleanup receipt the group is normally already empty, so
/// the first read usually succeeds. Back off from 1 ms rather than spin, and
/// give up at [`RETIREMENT_DEADLINE`] so a job without a timeout can't wait
/// forever; the caller then retires the executor.
async fn wait_empty_task(task: &Path) -> std::io::Result<()> {
    let deadline = tokio::time::Instant::now() + RETIREMENT_DEADLINE;
    let mut pause = Duration::from_millis(1);
    loop {
        let events = tokio::fs::read_to_string(task.join("cgroup.events")).await?;
        if events.lines().any(|line| line == "populated 0") {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(std::io::Error::other(
                "task cgroup is still populated after cleanup",
            ));
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(50));
    }
}

/// Singleton logs retain public run identity; bulk output stays in bounded details.
struct CommandLogs<'a> {
    live: Option<Arc<super::CommandLogStream>>,
    sink: Option<&'a tokio::sync::mpsc::Sender<crate::ketchup::types::LogRecord>>,
    app: String,
    namespace: String,
    instance: String,
    pending: [Vec<u8>; 2],
}
impl<'a> CommandLogs<'a> {
    fn new(
        task: &TaskInvocation,
        template: &JobSpec,
        sink: Option<&'a tokio::sync::mpsc::Sender<crate::ketchup::types::LogRecord>>,
    ) -> Self {
        let run = task.run.as_ref();
        Self {
            live: None,
            sink: sink.filter(|_| run.is_some_and(|run| run.is_singleton())),
            app: run
                .and_then(|run| run.job_name.clone())
                .unwrap_or_else(|| "job".into()),
            namespace: template
                .namespace
                .clone()
                .unwrap_or_else(|| "default".into()),
            instance: format!(
                "run-{}",
                run.map(|run| run.batch_id.to_string()).unwrap_or_default()
            ),
            pending: Default::default(),
        }
    }
    async fn emit(&self, stream: usize, bytes: &[u8]) {
        if let Some(live) = &self.live {
            live.push(crate::ketchup::types::CapturedLine {
                stream: if stream == 0 {
                    crate::ketchup::types::LogStream::Stdout
                } else {
                    crate::ketchup::types::LogStream::Stderr
                },
                line: String::from_utf8_lossy(bytes).into(),
                position: None,
            });
        }
        if let Some(sink) = self.sink {
            let _ = sink
                .send(crate::ketchup::types::LogRecord {
                    app: self.app.clone(),
                    namespace: self.namespace.clone(),
                    instance: self.instance.clone(),
                    stream: if stream == 0 {
                        crate::ketchup::types::LogStream::Stdout
                    } else {
                        crate::ketchup::types::LogStream::Stderr
                    },
                    line: String::from_utf8_lossy(bytes).into(),
                    position: None,
                })
                .await;
        }
    }
    async fn push(&mut self, stream: u8, bytes: &[u8]) {
        if self.sink.is_none() && self.live.is_none() {
            return;
        }
        let index = usize::from(stream - 1);
        for byte in bytes {
            if *byte == b'\n' || self.pending[index].len() == 8192 {
                let line = std::mem::take(&mut self.pending[index]);
                self.emit(index, &line).await;
            }
            if *byte != b'\n' {
                self.pending[index].push(*byte);
            }
        }
    }
    async fn finish(&mut self) {
        for index in 0..2 {
            let line = std::mem::take(&mut self.pending[index]);
            if !line.is_empty() {
                self.emit(index, &line).await;
            }
        }
    }
}
