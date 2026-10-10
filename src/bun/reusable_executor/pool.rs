//! Bounded owned containers, with one independently limited command per slot.
use super::timings::{Phase, TimingSnapshot, Timings};
use super::{ExecutorError, ExecutorKey, ExecutorProfile, HELPER_MEMORY_BYTES, protocol};
use crate::bun::execution_budget::{ExecutionBudget, ResourceLease};
use crate::bun::task_executor::{Attempt, AttemptOutcome, CapturedOutput, TaskInvocation};
use crate::config::job::JobSpec;
use crate::grill::kernel::GroupKill;
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
/// signal, and a runtime call can hang with it; past this, the slot and its
/// lease stay quarantined while the eviction loop keeps retrying, and the
/// caller gets its own outcome back.
const RETIREMENT_DEADLINE: Duration = Duration::from_secs(10);
/// How long one eviction tick spends on each executor, so one that won't
/// retire can't hold up the others behind it.
const EVICTION_BUDGET: Duration = Duration::from_secs(1);
pub(crate) struct CommandReporting<'a> {
    pub sink: Option<&'a tokio::sync::mpsc::Sender<crate::ketchup::types::LogRecord>>,
    pub singletons: &'a std::sync::Mutex<
        std::collections::BTreeMap<u64, Arc<crate::bun::task_runtime::SingletonRuntime>>,
    >,
}

struct Slot {
    busy: bool,
    /// The run whose caller has this slot checked out with resources behind
    /// it (a charged executor, or an admitted reservation), until release.
    /// A caller still waiting for admission holds nothing, so isn't counted.
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
    /// The retirement probe still in flight from an abandoned attempt. It is
    /// awaited again rather than started again, so a runtime call that never
    /// returns leaves one stuck task behind, not one per retry.
    probe: Option<tokio::task::JoinHandle<bool>>,
    /// Filesystem cleanup may continue after the caller times out. Keep its
    /// handle until completion before releasing or reusing the slot.
    cleanup: Option<tokio::task::JoinHandle<bool>>,
    #[cfg(feature = "ebpf")]
    namespace: Option<crate::bun::task_namespace::NamespaceLease>,
}
/// A fixed set of executor slots. A dropped [`ReusablePool::run`] future
/// hands its executor to the eviction loop to retire (see [`Checkout`]).
pub(crate) struct ReusablePool<G> {
    lifecycle: G,
    runtime: Runtime,
    /// How retirement empties a task group on this kernel.
    group_kill: GroupKill,
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
            crate::grill::kernel::executor_support().group_kill,
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
            crate::grill::kernel::executor_support().group_kill,
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
        group_kill: GroupKill,
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
            group_kill,
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
            if self.retire(&mut context, EVICTION_BUDGET).await {
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
            self.retire_and_release(index, context, EVICTION_BUDGET)
                .await;
        }
    }
    async fn release(&self, index: usize, context: Option<Context>) {
        vacate(&mut self.slots.lock().await[index], context);
        self.changed.notify_waiters();
    }
    /// Slots this run's callers hold with resources charged. Unlike
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
            let selected = warm.or_else(|| choose_cold_slot(&slots));
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
                if lease.is_some() || slots[index].context.is_some() {
                    slots[index].holder = holder;
                }
                slots[index].key = Some(key);
                return Some((index, slots[index].context.take(), lease));
            }
            drop(slots);
            tokio::select! { biased; () = cancel.cancelled() => return None, () = changed => {} }
        }
    }
    /// Wait for the reservation of a slot checked out without one. The slot
    /// counts towards [`Self::busy_slots`] only from here: until admission
    /// its caller holds nothing a command could run on.
    async fn admit(
        &self,
        index: usize,
        holder: Option<u64>,
        reservation: crate::meat::Resources,
        cancel: &CancellationToken,
    ) -> Option<ResourceLease> {
        let lease = self.budget.acquire(reservation, cancel).await?;
        self.slots.lock().await[index].holder = holder;
        Some(lease)
    }
    /// One retirement attempt: true once the helper has stopped and its task
    /// group is empty. Cancel-safe: an unfinished probe stays in the context.
    async fn retirement_step(&self, context: &mut Context) -> bool {
        context.connection.take();
        let probe = context.probe.get_or_insert_with(|| {
            let runtime = self.runtime.clone();
            let lifecycle = self.lifecycle.clone();
            let id = context.id.clone();
            tokio::spawn(async move {
                match runtime.state(&id).await {
                    Ok(ContainerState::Stopped)
                    | Err(crate::grill::GrillError::NotFound { .. }) => true,
                    _ => {
                        let _ = lifecycle.kill(&id).await;
                        false
                    }
                }
            })
        });
        let stopped = probe.await.unwrap_or(false);
        context.probe = None;
        if !stopped {
            return false;
        }
        // The runtime owns helper retirement; Bun owns the sibling task group.
        // Removing/reusing a group requires emptiness, independently of PID 1.
        // cgroupfs is an in-memory kernel interface, so this never waits on a
        // disk (see wait_empty_task).
        crate::grill::kernel::kill_group(&context.base.join("task"), self.group_kill).is_ok()
    }
    /// Retire within `budget`, every runtime call and file removal included;
    /// false leaves the executor, its lease and its namespace binding held.
    async fn retire(&self, context: &mut Context, budget: Duration) -> bool {
        let retired = tokio::time::timeout(budget, async {
            while !self.retirement_step(context).await {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.remove_files(context).await
        })
        .await
        .unwrap_or(false);
        if !retired {
            return false;
        }
        // Releasing the binding takes a lock and must not be cut off halfway,
        // so it runs on its own once retirement is proven.
        #[cfg(feature = "ebpf")]
        if let Some(namespace) = context.namespace.take() {
            tokio::spawn(namespace.retired());
        }
        context.lease.confirm_retired();
        true
    }
    /// Keep one cleanup operation across timeouts. Blocking filesystem work
    /// cannot be cancelled by dropping its async wait; a retry must await it
    /// before the fixed slot paths can be reused for another executor.
    async fn remove_files(&self, context: &mut Context) -> bool {
        let cleanup = context.cleanup.get_or_insert_with(|| {
            let base = context.base.clone();
            let directory = context.directory.clone();
            let socket_path = context.socket_path.clone();
            let id = context.id.clone();
            tokio::task::spawn_blocking(move || {
                // The task group must really be gone, even after cgroup.kill.
                // Removing a surviving group then reusing it is unsafe (see
                // prepare_cgroups). Missing paths are valid after a retry.
                let removed = |result: std::io::Result<()>, path: &Path| match result {
                    Ok(()) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => {
                        eprintln!(
                            "executor {}: cannot remove {}: {error}",
                            id.0,
                            path.display()
                        );
                        false
                    }
                };
                for path in [base.join("task"), base.join("helper"), base] {
                    if !removed(std::fs::remove_dir(&path), &path) {
                        return false;
                    }
                }
                if let Some(path) = socket_path
                    && !removed(std::fs::remove_file(&path), &path)
                {
                    return false;
                }
                removed(std::fs::remove_dir_all(&directory), &directory)
            })
        });
        // Await by reference so the outer retirement timeout leaves the
        // handle in the context, alongside the quarantined reservation.
        let removed = cleanup.await.unwrap_or(false);
        context.cleanup = None;
        removed
    }
    /// Retire within `budget` and free the slot, or quarantine it with its
    /// lease for the eviction loop to retry.
    async fn retire_and_release(&self, index: usize, mut context: Context, budget: Duration) {
        if self.retire(&mut context, budget).await {
            self.release(index, None).await;
        } else {
            self.quarantine(index, context, budget).await;
        }
    }
    async fn quarantine(&self, index: usize, context: Context, budget: Duration) {
        let mut slots = self.slots.lock().await;
        quarantine(&mut slots[index], context, budget);
    }
    /// Start a native host executor. Gives up at its next safe point once
    /// `stop` is cancelled; the caller then retires the context.
    async fn start_host(
        &self,
        context: &mut Context,
        template: &JobSpec,
        profile: ExecutorProfile,
        stop: &CancellationToken,
    ) -> Result<(), ExecutorError> {
        use sha2::{Digest, Sha256};
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
        let directory = context.directory.clone();
        blocking(move || {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&directory)?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        })
        .await?;
        let helper = context.directory.join("helper");
        install_helper(helper.clone(), HOST_HELPER).await?;
        // Socket credentials and the durable owner's unreaped helper identity
        // authenticate this short address. Hash the executor directory too,
        // so two Buns on one host never share a name.
        let mut name = Sha256::new();
        name.update(context.directory.as_os_str().as_encoded_bytes());
        let name = hex::encode(name.finalize())[..32].to_string();
        let socket = blocking(move || -> std::io::Result<PathBuf> {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let socket = host_socket_directory()?.join(name);
            match std::fs::symlink_metadata(&socket) {
                Ok(metadata) => {
                    if !metadata.file_type().is_socket()
                        || metadata.uid() != crate::grill::userns::EXECUTOR_HOST_UID
                    {
                        return Err(std::io::Error::other("foreign host executor socket"));
                    }
                    std::fs::remove_file(&socket)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            Ok(socket)
        })
        .await?;
        let listener = UnixListener::bind(&socket)?;
        context.socket_path = Some(socket.clone());
        let owned = socket.clone();
        blocking(move || {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&owned, std::fs::Permissions::from_mode(0o600))?;
            std::os::unix::fs::lchown(
                &owned,
                Some(crate::grill::userns::EXECUTOR_HOST_UID),
                Some(crate::grill::userns::EXECUTOR_HOST_UID),
            )
        })
        .await?;
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
        let base = context.base.clone();
        blocking(move || prepare_cgroups(&base, profile)).await?;
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
        let mut connection = self.launch(&context.id, &spec, &listener, stop).await?;
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
        tokio::fs::remove_file(&socket).await?;
        context.socket_path = None;
        let native = connection.into_std()?;
        let task = tokio::fs::File::open(context.base.join("task"))
            .await?
            .into_std()
            .await;
        protocol::send_directory(&native, &task)?;
        context.connection = Some(UnixStream::from_std(native)?);
        Ok(())
    }
    /// Create and start the helper, then wait for it to connect. `stop`
    /// is checked between runtime calls, never during one, and interrupts
    /// the wait for the connection.
    async fn launch(
        &self,
        id: &InstanceId,
        spec: &crate::grill::oci::OciSpec,
        listener: &UnixListener,
        stop: &CancellationToken,
    ) -> Result<UnixStream, ExecutorError> {
        let interrupted = || ExecutorError::Configuration("executor preparation interrupted");
        if stop.is_cancelled() {
            return Err(interrupted());
        }
        self.lifecycle.create(id, spec).await?;
        if stop.is_cancelled() {
            return Err(interrupted());
        }
        self.lifecycle.start(id).await?;
        tokio::select! { biased;
            () = stop.cancelled() => Err(interrupted()),
            accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()) => {
                let (connection, _) = accepted
                    .map_err(|_| ExecutorError::Protocol("helper connection timed out".into()))??;
                Ok(connection)
            }
        }
    }
    /// Start an executor container. Gives up at its next safe point once
    /// `stop` is cancelled; the caller then retires the context.
    async fn start(
        &self,
        context: &mut Context,
        template: &JobSpec,
        profile: ExecutorProfile,
        stop: &CancellationToken,
    ) -> Result<(), ExecutorError> {
        if self.runtime.host() {
            return self.start_host(context, template, profile, stop).await;
        }
        let Runtime::Container(runtime) = &self.runtime else {
            return Err(ExecutorError::Configuration("container backend missing"));
        };
        use std::os::fd::AsRawFd;
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
        let directory = context.directory.clone();
        let (bootstrap, source) = blocking(move || {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&directory)?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
            std::os::unix::fs::lchown(
                &directory,
                Some(crate::grill::userns::EXECUTOR_HOST_UID),
                Some(crate::grill::userns::EXECUTOR_HOST_UID),
            )?;
            let bootstrap = directory.join("bootstrap");
            std::fs::create_dir(&bootstrap)?;
            std::fs::set_permissions(&bootstrap, std::fs::Permissions::from_mode(0o755))?;
            Ok((bootstrap, std::fs::File::open(&directory)?))
        })
        .await?;
        let helper = bootstrap.join("helper");
        install_helper(helper.clone(), HELPER).await?;
        // A /proc/fd alias keeps AF_UNIX's address bounded, independent of the
        // configured data-directory length. The source descriptor stays open.
        let socket_path = format!("/proc/self/fd/{}/control", source.as_raw_fd());
        let listener = UnixListener::bind(&socket_path)?;
        let control = context.directory.join("control");
        blocking(move || {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o600))?;
            std::os::unix::fs::lchown(
                &control,
                Some(crate::grill::userns::EXECUTOR_HOST_UID),
                Some(crate::grill::userns::EXECUTOR_HOST_UID),
            )
        })
        .await?;
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
        let base = context.base.clone();
        blocking(move || prepare_cgroups(&base, profile)).await?;
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
        spec.mounts.extend(scratch_mounts());
        let mut connection = self.launch(&context.id, &spec, &listener, stop).await?;
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
        tokio::fs::remove_file(context.directory.join("control")).await?;
        let native = connection.into_std()?;
        let task = tokio::fs::File::open(context.base.join("task"))
            .await?
            .into_std()
            .await;
        protocol::send_directory(&native, &task)?;
        context.connection = Some(UnixStream::from_std(native)?);
        Ok(())
    }
    pub(crate) async fn run<F>(
        self: &Arc<Self>,
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
        // From here every exit hands the slot back through `checkout`, and a
        // dropped future hands it to the eviction loop.
        let mut checkout = Checkout {
            pool: self.clone(),
            index,
            context: existing,
            returned: false,
        };
        if checkout
            .context
            .as_ref()
            .is_some_and(|old| old.key != key || self.budget.has_waiters())
            && !checkout.retire_previous().await
        {
            return failed(ExecutorError::Configuration(
                "the slot's previous executor has not retired",
            ));
        }
        // Match fresh execution: waiting for compatible slots or admission is
        // not command run time. Cold preparation starts its clock only once the
        // complete profile is charged; warm work starts after borrowing its slot.
        let mut deadline = (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout);
        if checkout.context.is_none() {
            let lease = match reservation {
                Some(lease) => Some(lease),
                None => {
                    self.admit(
                        index,
                        task.run.as_ref().map(|run| run.batch_id),
                        profile.reservation,
                        cancel,
                    )
                    .await
                }
            };
            let Some(lease) = lease else {
                checkout.release().await;
                return Attempt {
                    outcome: AttemptOutcome::Cancelled,
                    output: CapturedOutput::default(),
                    ran: None,
                };
            };
            deadline = (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout);
            drop(admission_timer.take());
            let startup_timer = self.timings.start(Phase::Startup);
            // Cold preparation can take as long as an image pull, so cancel
            // and the deadline interrupt it, as they do a fresh launch.
            let interrupted = || async {
                tokio::select! { biased;
                    () = cancel.cancelled() => AttemptOutcome::Cancelled,
                    () = crate::bun::task_executor::wait_deadline(deadline) => AttemptOutcome::TimedOut,
                }
            };
            let image = match &self.runtime {
                Runtime::Host(_) => None,
                Runtime::Container(runtime) => {
                    // The pull runs as its own task, so an interrupted attempt
                    // leaves it to finish filling the image cache instead of
                    // dropping it halfway through unpacking a layer.
                    let store = runtime.image_store().clone();
                    let reference = template.image.clone().unwrap_or_default();
                    let pull = tokio::spawn(async move { store.pull_and_unpack(&reference).await });
                    let pulled = tokio::select! { biased;
                        outcome = interrupted() => Err(Attempt {
                            outcome,
                            output: CapturedOutput::default(),
                            ran: None,
                        }),
                        pulled = pull => match pulled {
                            Ok(Ok(image)) => Ok(image),
                            Ok(Err(error)) => Err(failed(ExecutorError::Protocol(error.to_string()))),
                            Err(error) => Err(failed(ExecutorError::Protocol(error.to_string()))),
                        },
                    };
                    match pulled {
                        Ok(image) => Some(image),
                        Err(attempt) => {
                            checkout.release().await;
                            return attempt;
                        }
                    }
                }
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
                    checkout.release().await;
                    return failed(error.into());
                }
            };
            let directory = match self.runtime.directory() {
                Ok(directory) => directory.join(&id.0),
                Err(error) => {
                    checkout.release().await;
                    return failed(error);
                }
            };
            let new = checkout.context.insert(Context {
                key,
                directory,
                id,
                base,
                image,
                socket_path: None,
                connection: None,
                lease: lease.quarantine_on_drop(),
                sequence: 0,
                idle_since: tokio::time::Instant::now(),
                probe: None,
                cleanup: None,
                #[cfg(feature = "ebpf")]
                namespace: None,
            });
            // `stop` asks start to give up at its next safe point. It never
            // abandons a runtime call halfway, so retirement sees whatever
            // that call left behind.
            let stop = CancellationToken::new();
            let stopped = {
                let started = self.start(new, &template, profile, &stop);
                tokio::pin!(started);
                tokio::select! { biased;
                    outcome = interrupted() => {
                        stop.cancel();
                        let _ = (&mut started).await;
                        Some(Attempt { outcome, output: CapturedOutput::default(), ran: None })
                    }
                    result = &mut started => result.err().map(failed),
                }
            };
            if let Some(attempt) = stopped {
                checkout.retire().await;
                return attempt;
            }
            drop(startup_timer);
        }
        drop(admission_timer.take());
        let Some(context) = checkout.context.as_mut() else {
            checkout.release().await;
            return failed(ExecutorError::Configuration("missing executor context"));
        };
        if let Some(refresh) = refresh {
            let refreshed = refresh().await.ok().filter(|template| {
                ExecutorKey::new(template).is_ok_and(|live| live == context.key)
            });
            let Some(refreshed) = refreshed else {
                checkout.retire().await;
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
                checkout.retire().await;
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
            checkout.retire().await;
        } else {
            if let Some(context) = checkout.context.as_mut() {
                context.sequence = sequence;
                context.idle_since = tokio::time::Instant::now();
            }
            checkout.release().await;
        }
        Attempt {
            outcome,
            output,
            ran,
        }
    }
}

/// Hand a slot back, holding `context` warm or empty.
fn vacate(slot: &mut Slot, context: Option<Context>) {
    slot.key = context.as_ref().map(|context| context.key);
    slot.context = context;
    slot.busy = false;
    slot.holder = None;
    slot.active_run = None;
}

/// Leave a busy slot's executor for the eviction loop to retire. Its
/// reservation stays charged until it does.
fn quarantine(slot: &mut Slot, context: Context, budget: Duration) {
    eprintln!(
        "executor {} did not retire within {budget:?}; its slot and reservation stay quarantined until it does",
        context.id.0
    );
    slot.holder = None;
    slot.active_run = None;
    slot.retiring = Some(context);
}

/// The slot a caller with no warm match takes: an empty one first, so a
/// warm executor of another profile isn't thrown away while there's room,
/// then the executor that has been idle longest.
fn choose_cold_slot(slots: &[Slot]) -> Option<usize> {
    let free = || slots.iter().enumerate().filter(|(_, slot)| !slot.busy);
    free()
        .find(|(_, slot)| slot.context.is_none())
        .or_else(|| free().min_by_key(|(_, slot)| slot.context.as_ref().map(|c| c.idle_since)))
        .map(|(index, _)| index)
}

/// A slot checked out by [`ReusablePool::run`], with the executor it holds.
///
/// Every normal exit hands the slot back through [`Checkout::release`] or
/// [`Checkout::retire`]. If the run future is dropped first, dropping this
/// moves the executor into the slot's `retiring` place, so the eviction loop
/// retires it and frees the slot and its lease without a Bun restart. A slot
/// with no executor yet is simply released.
struct Checkout<G: Grill + Clone + 'static> {
    pool: Arc<ReusablePool<G>>,
    index: usize,
    context: Option<Context>,
    returned: bool,
}
impl<G: Grill + Clone + 'static> Checkout<G> {
    /// Hand the slot back, keeping its executor (if any) warm.
    async fn release(mut self) {
        let mut slots = self.pool.slots.lock().await;
        vacate(&mut slots[self.index], self.context.take());
        self.returned = true;
        drop(slots);
        self.pool.changed.notify_waiters();
    }
    /// Retire the executor and free the slot, or quarantine both.
    async fn retire(mut self) {
        let retired = match self.context.as_mut() {
            Some(context) => self.pool.retire(context, RETIREMENT_DEADLINE).await,
            None => true,
        };
        let mut slots = self.pool.slots.lock().await;
        match self.context.take() {
            Some(context) if !retired => {
                quarantine(&mut slots[self.index], context, RETIREMENT_DEADLINE)
            }
            // A retired context's lease returns its reservation as it drops.
            _ => vacate(&mut slots[self.index], None),
        }
        self.returned = true;
        drop(slots);
        self.pool.changed.notify_waiters();
    }
    /// Retire the incompatible executor a caller found in its slot, keeping
    /// the slot, or quarantine it and give the slot up. Its reservation goes
    /// with it, so the slot stops counting towards
    /// [`ReusablePool::busy_slots`] until the caller's own is admitted.
    async fn retire_previous(&mut self) -> bool {
        let Some(old) = self.context.as_mut() else {
            return true;
        };
        let retired = self.pool.retire(old, RETIREMENT_DEADLINE).await;
        let mut slots = self.pool.slots.lock().await;
        let Some(old) = self.context.take() else {
            return true;
        };
        if retired {
            slots[self.index].holder = None;
            drop(old);
        } else {
            quarantine(&mut slots[self.index], old, RETIREMENT_DEADLINE);
            self.returned = true;
        }
        retired
    }
}
impl<G: Grill + Clone + 'static> Drop for Checkout<G> {
    fn drop(&mut self) {
        if self.returned {
            return;
        }
        let pool = self.pool.clone();
        let index = self.index;
        let context = self.context.take();
        // Drop can't wait for the slot lock, so a task finishes the hand-over.
        let handoff = async move {
            let mut slots = pool.slots.lock().await;
            match context {
                Some(context) => {
                    let slot = &mut slots[index];
                    slot.holder = None;
                    slot.active_run = None;
                    slot.retiring = Some(context);
                }
                None => vacate(&mut slots[index], None),
            }
            drop(slots);
            pool.changed.notify_waiters();
        };
        // Outside a runtime (only when the runtime itself is shutting down)
        // the context drops here, and its lease stays quarantined as before.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(handoff);
        }
    }
}

/// Run blocking filesystem work on the blocking pool, not an async worker.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, ExecutorError> {
    Ok(tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| ExecutorError::Protocol(error.to_string()))??)
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

/// The scratch filesystems a shared executor container declares.
///
/// The helper mounts a fresh tmpfs over `/tmp` and `/dev/shm` for every
/// command, which needs both directories to exist. The root is read-only,
/// so declaring them here is what makes runc create the mount points in
/// images that ship without them (`FROM scratch` and static images).
fn scratch_mounts() -> [crate::grill::oci::OciMount; 2] {
    let tmpfs = |destination: &str, mut options: Vec<String>| {
        options.extend(["mode=1777".into(), "size=16m".into()]);
        crate::grill::oci::OciMount {
            destination: destination.into(),
            source: None,
            mount_type: Some("tmpfs".into()),
            options,
        }
    };
    [
        tmpfs("/tmp", vec!["nosuid".into(), "nodev".into()]),
        tmpfs(
            "/dev/shm",
            vec!["nosuid".into(), "nodev".into(), "noexec".into()],
        ),
    ]
}

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

/// After the helper's cleanup receipt the group is normally already empty, so
/// the first read usually succeeds. Back off from 1 ms rather than spin, and
/// give up at [`RETIREMENT_DEADLINE`] so a job without a timeout can't wait
/// forever; the caller then retires the executor.
async fn wait_empty_task(task: &Path) -> std::io::Result<()> {
    let deadline = tokio::time::Instant::now() + RETIREMENT_DEADLINE;
    let mut pause = Duration::from_millis(1);
    loop {
        // cgroupfs is an in-memory kernel interface, like /proc: reading it
        // never waits on a disk. This runs once per command, and handing it to
        // the blocking pool cost about 3% of host-job throughput.
        let events = std::fs::read_to_string(task.join("cgroup.events"))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
    use crate::grill::{GrillError, ProcessGrill};
    use crate::meat::Resources;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A runtime whose `kill` never returns, as when it waits on a task in
    /// uninterruptible sleep.
    #[derive(Clone, Default)]
    struct HangingKill {
        kills: Arc<AtomicUsize>,
    }
    impl Grill for HangingKill {
        async fn create(&self, _: &InstanceId, _: &OciSpec) -> Result<(), GrillError> {
            Ok(())
        }
        async fn start(&self, _: &InstanceId) -> Result<(), GrillError> {
            Ok(())
        }
        async fn stop(&self, _: &InstanceId) -> Result<(), GrillError> {
            Ok(())
        }
        async fn kill(&self, _: &InstanceId) -> Result<(), GrillError> {
            self.kills.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
        async fn state(&self, _: &InstanceId) -> Result<ContainerState, GrillError> {
            Ok(ContainerState::Running)
        }
    }

    fn reservation() -> Resources {
        Resources::new(100, 16 << 20, 0)
    }

    fn pool(
        process: &ProcessGrill,
        lifecycle: HangingKill,
        budget: &Arc<ExecutionBudget>,
    ) -> Arc<ReusablePool<HangingKill>> {
        pool_with(process, lifecycle, budget, GroupKill::CgroupKill, 2)
    }

    fn pool_with<L: Grill + Clone + 'static>(
        process: &ProcessGrill,
        lifecycle: L,
        budget: &Arc<ExecutionBudget>,
        group_kill: GroupKill,
        slots: usize,
    ) -> Arc<ReusablePool<L>> {
        ReusablePool::with_runtime(
            Runtime::Host(process.clone()),
            group_kill,
            lifecycle,
            "rbtest".into(),
            slots,
            budget.clone(),
            #[cfg(feature = "ebpf")]
            None,
        )
    }

    /// A slot checked out of `pool` the way `run` checks one out.
    async fn checkout(
        pool: &Arc<ReusablePool<HangingKill>>,
        context: Option<Context>,
    ) -> Checkout<HangingKill> {
        let cancel = CancellationToken::new();
        let (index, _, lease) = pool
            .slot(key(1), reservation(), Some(7), &cancel)
            .await
            .unwrap();
        drop(lease);
        Checkout {
            pool: pool.clone(),
            index,
            context,
            returned: false,
        }
    }

    async fn wait_for_free_slot(pool: &Arc<ReusablePool<HangingKill>>, index: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while pool.slots.lock().await[index].busy {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the slot was never handed back");
    }

    /// An executor whose helper is a real `sleep` the in-memory backend
    /// reports as running.
    async fn running(process: &ProcessGrill, name: &str) -> InstanceId {
        let id = InstanceId(name.into());
        let spec = OciSpec {
            reusable_executor: false,
            host_process: false,
            port_mapping: None,
            root: OciRoot {
                path: "/".into(),
                readonly: false,
            },
            process: OciProcess {
                rlimits: Vec::new(),
                args: vec!["sleep".into(), "60".into()],
                env: vec![],
                cwd: "/".into(),
                user: OciUser { uid: 0, gid: 0 },
                capabilities: None,
                overrides: None,
            },
            mounts: vec![],
            linux: OciLinux {
                namespaces: vec![],
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
            },
        };
        process.create(&id, &spec).await.unwrap();
        process.start(&id).await.unwrap();
        id
    }

    fn context(budget: &Arc<ExecutionBudget>, id: InstanceId, root: &Path) -> Context {
        Context {
            key: ExecutorKey([0; 32]),
            directory: root.join(&id.0),
            base: root.join(format!("{}-cgroup", id.0)),
            id,
            image: None,
            socket_path: None,
            connection: None,
            lease: budget
                .try_acquire_executor(reservation())
                .unwrap()
                .quarantine_on_drop(),
            sequence: 0,
            idle_since: tokio::time::Instant::now(),
            probe: None,
            cleanup: None,
            #[cfg(feature = "ebpf")]
            namespace: None,
        }
    }

    #[test]
    fn executor_declares_tmp_and_shm_so_images_without_them_still_run() {
        let mounts = scratch_mounts();
        for destination in ["/tmp", "/dev/shm"] {
            let mount = mounts
                .iter()
                .find(|mount| mount.destination == Path::new(destination))
                .unwrap_or_else(|| panic!("{destination} is not declared"));
            assert_eq!(mount.mount_type.as_deref(), Some("tmpfs"));
            assert!(mount.source.is_none());
            for option in ["nosuid", "nodev", "mode=1777", "size=16m"] {
                assert!(
                    mount.options.iter().any(|o| o == option),
                    "{destination}: {option}"
                );
            }
        }
    }

    #[tokio::test]
    async fn timed_out_cleanup_keeps_its_slot_and_reservation_until_the_same_operation_finishes() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        let mut old = context(&budget, InstanceId("rbtest-cleaning".into()), root.path());
        std::fs::create_dir(&old.directory).unwrap();
        let directory = old.directory.clone();
        let (resume, gate) = std::sync::mpsc::channel();
        let started = Arc::new(AtomicUsize::new(0));
        let calls = started.clone();
        // A cleanup that has already started owns a fixed slot path. Blocking
        // filesystem work must survive timeout without losing its obligation.
        old.cleanup = Some(tokio::task::spawn_blocking(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            if gate.recv().is_err() {
                return false;
            }
            std::fs::remove_dir_all(directory).unwrap();
            true
        }));
        let handle = old.cleanup.as_ref().unwrap().id();
        for _ in 0..2 {
            assert!(!pool.retire(&mut old, Duration::from_millis(20)).await);
            assert_eq!(old.cleanup.as_ref().unwrap().id(), handle);
            assert_ne!(budget.available(), budget.capacity());
            assert!(old.directory.exists());
        }
        assert_eq!(started.load(Ordering::SeqCst), 1);
        {
            let mut slots = pool.slots.lock().await;
            slots[0].busy = true;
            slots[0].retiring = Some(old);
            slots[1].busy = true;
            slots[1].retiring = Some(context(
                &budget,
                InstanceId("rbtest-already-gone".into()),
                root.path(),
            ));
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while pool.slots.lock().await[1].busy {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            pool.slots.lock().await[0].busy,
            "unfinished cleanup allowed slot reuse"
        );
        assert_eq!(started.load(Ordering::SeqCst), 1);
        assert_eq!(
            budget.available(),
            budget.capacity().saturating_sub(&reservation())
        );
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while pool.slots.lock().await[0].busy {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(budget.available(), budget.capacity());
        // Only after positive cleanup can the path be used by a new executor.
        let directory = root.path().join("rbtest-cleaning");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("new-generation"), "alive").unwrap();
        tokio::task::yield_now().await;
        assert!(directory.join("new-generation").exists());
    }

    #[tokio::test]
    async fn failed_filesystem_cleanup_keeps_capacity_until_a_successful_retry() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        let mut old = context(
            &budget,
            InstanceId("rbtest-cleanup-error".into()),
            root.path(),
        );
        // A nonempty directory at the socket path cannot be unlinked as a
        // socket. Successful cgroup removal alone must not release capacity.
        let socket = root.path().join("bad-socket");
        std::fs::create_dir(&socket).unwrap();
        std::fs::write(socket.join("still-present"), "owned").unwrap();
        old.socket_path = Some(socket.clone());
        assert!(!pool.retire(&mut old, Duration::from_secs(5)).await);
        assert_ne!(budget.available(), budget.capacity());
        assert!(
            old.cleanup.is_none(),
            "completed failed cleanup was retained"
        );
        std::fs::remove_dir_all(&socket).unwrap();
        assert!(pool.retire(&mut old, Duration::from_secs(5)).await);
        // Retirement proves that dropping the lease is safe; releasing the
        // context actually returns its reservation to the execution budget.
        drop(old);
        assert_eq!(budget.available(), budget.capacity());
    }

    #[tokio::test]
    async fn retirement_gives_up_at_its_budget_when_a_runtime_call_never_returns() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let process = ProcessGrill::new();
        let lifecycle = HangingKill::default();
        let pool = pool(&process, lifecycle.clone(), &budget);
        let id = running(&process, "rbtest-hanging").await;
        let mut stuck = context(&budget, id.clone(), root.path());
        for _ in 0..2 {
            let retired = tokio::time::timeout(
                Duration::from_secs(5),
                pool.retire(&mut stuck, Duration::from_millis(200)),
            )
            .await
            .expect("retirement outlived its budget");
            assert!(!retired);
        }
        assert_eq!(
            lifecycle.kills.load(Ordering::SeqCst),
            1,
            "a retry started a second kill instead of waiting for the first"
        );
        drop(stuck);
        assert_ne!(
            budget.available(),
            budget.capacity(),
            "an unretired executor returned its reservation"
        );
        process.kill(&id).await.unwrap();
    }

    #[tokio::test]
    async fn eviction_retires_other_executors_past_one_whose_runtime_hangs() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let process = ProcessGrill::new();
        let pool = pool(&process, HangingKill::default(), &budget);
        let id = running(&process, "rbtest-hanging").await;
        // The second executor has already exited and left nothing behind.
        let gone = InstanceId("rbtest-gone".into());
        {
            let mut slots = pool.slots.lock().await;
            for (slot, id) in slots.iter_mut().zip([id.clone(), gone]) {
                slot.busy = true;
                slot.retiring = Some(context(&budget, id, root.path()));
            }
        }
        let one_held = budget.capacity().saturating_sub(&reservation());
        tokio::time::timeout(Duration::from_secs(5), async {
            while budget.available() != one_held {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hanging executor held up eviction of the other");
        let slots = pool.slots.lock().await;
        assert!(slots[0].busy, "the hanging executor's slot was freed");
        assert!(!slots[1].busy, "the retired executor's slot stayed busy");
        drop(slots);
        process.kill(&id).await.unwrap();
    }

    fn key(byte: u8) -> ExecutorKey {
        ExecutorKey([byte; 32])
    }

    #[tokio::test]
    async fn a_slot_waiting_for_admission_counts_as_busy_only_once_admitted() {
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        let app = budget.try_acquire(budget.capacity()).unwrap();
        let cancel = CancellationToken::new();
        let (index, existing, lease) = pool
            .slot(key(1), reservation(), Some(7), &cancel)
            .await
            .unwrap();
        assert!(existing.is_none() && lease.is_none());
        assert_eq!(pool.busy_slots(7).await, 0, "counted before admission");
        let admitted = tokio::spawn({
            let pool = pool.clone();
            let cancel = cancel.clone();
            async move { pool.admit(index, Some(7), reservation(), &cancel).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(pool.busy_slots(7).await, 0, "counted while queued");
        drop(app);
        let lease = tokio::time::timeout(Duration::from_secs(5), admitted)
            .await
            .unwrap()
            .unwrap();
        assert!(lease.is_some());
        assert_eq!(pool.busy_slots(7).await, 1);
        pool.release(index, None).await;
        assert_eq!(pool.busy_slots(7).await, 0);
    }

    #[tokio::test]
    async fn a_cancelled_admission_never_counts_as_busy() {
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        let _app = budget.try_acquire(budget.capacity()).unwrap();
        let cancel = CancellationToken::new();
        let (index, _, _) = pool
            .slot(key(1), reservation(), Some(7), &cancel)
            .await
            .unwrap();
        cancel.cancel();
        assert!(
            pool.admit(index, Some(7), reservation(), &cancel)
                .await
                .is_none()
        );
        assert_eq!(pool.busy_slots(7).await, 0);
    }

    #[tokio::test]
    async fn a_slot_admitted_at_checkout_counts_before_its_executor_starts() {
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        let cancel = CancellationToken::new();
        let (_, existing, lease) = pool
            .slot(key(1), reservation(), Some(7), &cancel)
            .await
            .unwrap();
        assert!(existing.is_none() && lease.is_some());
        assert_eq!(pool.busy_slots(7).await, 1);
    }

    #[tokio::test]
    async fn retiring_another_profiles_executor_stops_the_slot_counting_until_admitted() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        // One slot, so the caller has to take the other profile's executor.
        let pool = pool_with(
            &ProcessGrill::new(),
            HangingKill::default(),
            &budget,
            GroupKill::CgroupKill,
            1,
        );
        // An idle executor of another profile, already exited, holds the slot's
        // reservation; an app holds everything else.
        let mut old = context(&budget, InstanceId("rbtest-gone".into()), root.path());
        old.key = key(2);
        pool.release(0, Some(old)).await;
        let app = budget.try_acquire(budget.available()).unwrap();
        let cancel = CancellationToken::new();
        let (index, existing, lease) = pool
            .slot(key(1), reservation(), Some(7), &cancel)
            .await
            .unwrap();
        assert!(lease.is_none());
        assert_eq!(
            pool.busy_slots(7).await,
            1,
            "the old executor is still charged"
        );
        let mut checkout = Checkout {
            pool: pool.clone(),
            index,
            context: existing,
            returned: false,
        };
        assert!(checkout.retire_previous().await);
        assert!(checkout.context.is_none());
        assert_eq!(pool.busy_slots(7).await, 0, "counted with nothing charged");
        // Only the retired executor's reservation is free, which is all it needs.
        drop(app);
        assert!(
            pool.admit(index, Some(7), reservation(), &cancel)
                .await
                .is_some()
        );
        assert_eq!(pool.busy_slots(7).await, 1);
    }

    #[tokio::test]
    async fn empty_slot_is_chosen_before_evicting_a_warm_executor() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        let mut warm = context(&budget, InstanceId("rbtest-warm".into()), root.path());
        warm.key = key(2);
        pool.release(0, Some(warm)).await;
        let cancel = CancellationToken::new();
        let (index, existing, _) = pool
            .slot(key(1), reservation(), Some(7), &cancel)
            .await
            .unwrap();
        assert_eq!(index, 1, "took the warm slot while an empty one was free");
        assert!(existing.is_none());
        assert!(
            pool.slots.lock().await[0]
                .context
                .as_ref()
                .is_some_and(|context| context.key == key(2)),
            "the other profile's warm executor was disturbed"
        );
    }

    #[tokio::test]
    async fn the_longest_idle_executor_is_evicted_when_no_slot_is_empty() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        let now = tokio::time::Instant::now();
        for (index, idle) in [(0, 10), (1, 500)] {
            let mut warm = context(
                &budget,
                InstanceId(format!("rbtest-warm-{index}")),
                root.path(),
            );
            warm.key = key(2 + index as u8);
            warm.idle_since = now - Duration::from_millis(idle);
            pool.release(index, Some(warm)).await;
        }
        let cancel = CancellationToken::new();
        let (index, existing, _) = pool
            .slot(key(1), reservation(), Some(7), &cancel)
            .await
            .unwrap();
        assert_eq!(index, 1);
        assert!(existing.is_some_and(|context| context.key == key(3)));
    }

    #[tokio::test]
    async fn dropped_run_future_hands_its_slot_to_retirement() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        // An executor that has already exited and left nothing behind, held
        // by a run whose future is dropped mid-command.
        let held = context(&budget, InstanceId("rbtest-dropped".into()), root.path());
        let checkout = checkout(&pool, Some(held)).await;
        let index = checkout.index;
        assert_ne!(budget.available(), budget.capacity());
        drop(checkout);
        wait_for_free_slot(&pool, index).await;
        assert_eq!(
            budget.available(),
            budget.capacity(),
            "the dropped run's lease stayed quarantined"
        );
        let slot = &pool.slots.lock().await[index];
        assert!(slot.context.is_none() && slot.retiring.is_none());
        assert_eq!(slot.holder, None);
    }

    #[tokio::test]
    async fn dropped_run_future_without_an_executor_releases_its_slot() {
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool(&ProcessGrill::new(), HangingKill::default(), &budget);
        let checkout = checkout(&pool, None).await;
        let index = checkout.index;
        drop(checkout);
        wait_for_free_slot(&pool, index).await;
        assert_eq!(budget.available(), budget.capacity());
    }

    #[tokio::test]
    async fn a_dropped_run_whose_executor_hangs_stays_quarantined() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let process = ProcessGrill::new();
        let pool = pool(&process, HangingKill::default(), &budget);
        let id = running(&process, "rbtest-dropped-hanging").await;
        let checkout = checkout(&pool, Some(context(&budget, id.clone(), root.path()))).await;
        let index = checkout.index;
        drop(checkout);
        // Give the hand-over and a couple of eviction ticks time to run.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(pool.slots.lock().await[index].busy);
        assert_ne!(budget.available(), budget.capacity());
        process.kill(&id).await.unwrap();
    }

    #[tokio::test]
    async fn retirement_falls_back_to_kill_and_reap_without_cgroup_kill() {
        let root = tempfile::tempdir().unwrap();
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool_with(
            &ProcessGrill::new(),
            HangingKill::default(),
            &budget,
            GroupKill::FreezeAndKill,
            1,
        );
        // The helper is gone; one command survives in a task group that,
        // as before Linux 5.14, has no cgroup.kill.
        let mut context = context(&budget, InstanceId("rbtest-no-kill".into()), root.path());
        let mut survivor = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let task = context.base.join("task");
        std::fs::create_dir_all(&task).unwrap();
        std::fs::write(task.join("cgroup.freeze"), "0").unwrap();
        std::fs::write(task.join("cgroup.procs"), format!("{}\n", survivor.id())).unwrap();
        std::fs::write(task.join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(
            !pool.retirement_step(&mut context).await,
            "retired a populated group"
        );
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            survivor.wait().unwrap().signal(),
            Some(nix::sys::signal::Signal::SIGKILL as i32)
        );
        assert_eq!(
            std::fs::read_to_string(task.join("cgroup.freeze")).unwrap(),
            "1"
        );
        assert!(!task.join("cgroup.kill").exists());
        std::fs::write(task.join("cgroup.events"), "populated 0\nfrozen 1\n").unwrap();
        assert!(pool.retirement_step(&mut context).await);
    }

    /// A lifecycle whose helper never connects, so a cold start waits on
    /// the full accept timeout unless something interrupts it.
    #[derive(Clone, Default)]
    struct SilentHelper;
    impl Grill for SilentHelper {
        async fn create(&self, _: &InstanceId, _: &OciSpec) -> Result<(), GrillError> {
            Ok(())
        }
        async fn start(&self, _: &InstanceId) -> Result<(), GrillError> {
            Ok(())
        }
        async fn stop(&self, _: &InstanceId) -> Result<(), GrillError> {
            Ok(())
        }
        async fn kill(&self, _: &InstanceId) -> Result<(), GrillError> {
            Ok(())
        }
        async fn state(&self, id: &InstanceId) -> Result<ContainerState, GrillError> {
            Err(GrillError::NotFound {
                instance: id.clone(),
            })
        }
    }

    /// Run one host command through a pool whose helper never connects, and
    /// check the interruption came well before the 10 s accept timeout and
    /// left nothing held.
    async fn interrupt_cold_start(
        namespace: &str,
        timeout: Duration,
        cancel_after: Option<Duration>,
    ) -> Attempt {
        let root = tempfile::Builder::new()
            .prefix("rb-cold-interrupt-")
            .tempdir()
            .unwrap();
        let process = ProcessGrill::with_owner(root.path().join("owners"), "/bin/false".into());
        let budget = ExecutionBudget::new(Resources::new(1000, 1 << 30, 0));
        let pool = pool_with(
            &process,
            SilentHelper,
            &budget,
            crate::grill::kernel::executor_support().group_kill,
            1,
        );
        let template: JobSpec = toml::from_str(
            &format!(
            "runtime='process'\nexec='/bin/true'\nnamespace='{namespace}'\ncpu='100m-1000m'\nmemory='32Mi'"
        ),
        )
        .unwrap();
        let task = TaskInvocation {
            template: Some(Box::new(template.clone())),
            index: 0,
            attempt: 1,
            program: "/bin/true".into(),
            args: vec![],
            env: vec![],
            run: None,
        };
        let cancel = CancellationToken::new();
        if let Some(after) = cancel_after {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                cancel.cancel();
            });
        }
        let singletons = std::sync::Mutex::new(std::collections::BTreeMap::new());
        let started = std::time::Instant::now();
        let attempt = pool
            .run(
                &task,
                template,
                timeout,
                &cancel,
                None::<fn() -> std::future::Ready<Result<JobSpec, String>>>,
                CommandReporting {
                    sink: None,
                    singletons: &singletons,
                },
            )
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cold start ran to its accept timeout: {attempt:?}"
        );
        assert!(!pool.slots.lock().await[0].busy, "the slot stayed busy");
        assert!(pool.slots.lock().await[0].context.is_none());
        assert_eq!(budget.available(), budget.capacity());
        attempt
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires root and cgroup v2; run with make test-linux"]
    async fn cgroup_cancel_during_cold_preparation_retires_the_context() {
        let attempt = interrupt_cold_start(
            "rbtest-cold-cancel",
            Duration::from_secs(60),
            Some(Duration::from_millis(300)),
        )
        .await;
        assert_eq!(attempt.outcome, AttemptOutcome::Cancelled);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires root and cgroup v2; run with make test-linux"]
    async fn cgroup_deadline_during_cold_preparation_retires_the_context() {
        let attempt =
            interrupt_cold_start("rbtest-cold-deadline", Duration::from_millis(300), None).await;
        assert_eq!(attempt.outcome, AttemptOutcome::TimedOut);
    }
}
