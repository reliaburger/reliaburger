//! Bounded owned containers, with one independently limited command per slot.
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
const IDLE: Duration = Duration::from_secs(1);
pub(crate) struct CommandReporting<'a> {
    pub sink: Option<&'a tokio::sync::mpsc::Sender<crate::ketchup::types::LogRecord>>,
    pub singletons: &'a std::sync::Mutex<
        std::collections::BTreeMap<u64, Arc<crate::bun::task_runtime::SingletonRuntime>>,
    >,
}

struct Slot {
    busy: bool,
    active_run: Option<u64>,
    key: Option<ExecutorKey>,
    context: Option<Context>,
}
struct Context {
    key: ExecutorKey,
    id: InstanceId,
    directory: PathBuf,
    base: PathBuf,
    image: crate::grill::image::PulledImage,
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
    runtime: RuncGrill,
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
        let pool = Arc::new(Self {
            runtime,
            lifecycle,
            prefix,
            budget,
            slots: Mutex::new(
                (0..count.clamp(1, super::MAX_EXECUTOR_SLOTS))
                    .map(|_| Slot {
                        busy: false,
                        active_run: None,
                        key: None,
                        context: None,
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
    async fn evict_idle(&self) {
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
            let Some((index, Some(mut context))) = victim else {
                break;
            };
            self.retire(&mut context).await;
            self.release(index, None).await;
        }
    }
    async fn release(&self, index: usize, context: Option<Context>) {
        let mut slots = self.slots.lock().await;
        slots[index].key = context.as_ref().map(|context| context.key);
        slots[index].context = context;
        slots[index].busy = false;
        slots[index].active_run = None;
        self.changed.notify_waiters();
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
                slots[index].key = Some(key);
                return Some((index, slots[index].context.take(), lease));
            }
            drop(slots);
            tokio::select! { biased; () = cancel.cancelled() => return None, () = changed => {} }
        }
    }
    async fn retire(&self, context: &mut Context) {
        context.connection.take();
        loop {
            match self.runtime.state(&context.id).await {
                Ok(ContainerState::Stopped) | Err(crate::grill::GrillError::NotFound { .. }) => {
                    break;
                }
                _ => {
                    let _ = self.lifecycle.kill(&context.id).await;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // The runtime owns helper retirement; Bun owns the sibling task group.
        // Removing/reusing a group requires emptiness, independently of PID 1.
        loop {
            match empty_task(&context.base.join("task")) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        #[cfg(feature = "ebpf")]
        if let Some(namespace) = context.namespace.take() {
            namespace.retired().await;
        }
        for directory in [
            context.base.join("task"),
            context.base.join("helper"),
            context.base.clone(),
        ] {
            let _ = tokio::fs::remove_dir(directory).await;
        }
        let _ = tokio::fs::remove_dir_all(&context.directory).await;
        context.lease.confirm_retired();
    }
    async fn start(
        &self,
        context: &mut Context,
        template: &JobSpec,
        profile: ExecutorProfile,
    ) -> Result<(), ExecutorError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o555)
            .open(&helper)?;
        std::io::Write::write_all(&mut file, HELPER)?;
        file.sync_all()?;
        drop(file);
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
        self.runtime.authenticate_executor(&context.id, pid).await?;
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
        let Some((index, existing, reservation)) =
            self.slot(key, profile.reservation, cancel).await
        else {
            return Attempt {
                outcome: AttemptOutcome::Cancelled,
                output: CapturedOutput::default(),
            };
        };
        let mut context = match existing {
            Some(mut old) if old.key != key || self.budget.has_waiters() => {
                self.retire(&mut old).await;
                drop(old);
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
                };
            };
            deadline = (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout);
            let image = match self
                .runtime
                .image_store()
                .pull_and_unpack(template.image.as_deref().unwrap_or_default())
                .await
            {
                Ok(image) => image,
                Err(error) => {
                    self.release(index, None).await;
                    return failed(ExecutorError::Protocol(error.to_string()));
                }
            };
            let namespace = template.namespace.as_deref().unwrap_or("default");
            let app = format!("executor-{}-reuse", self.prefix);
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
                directory: self.runtime.executor_directory().join(&id.0),
                id,
                base,
                image,
                connection: None,
                lease: lease.quarantine_on_drop(),
                sequence: 0,
                idle_since: tokio::time::Instant::now(),
                #[cfg(feature = "ebpf")]
                namespace: None,
            };
            let start = self.start(&mut new, &template, profile).await;
            if let Err(error) = start {
                self.retire(&mut new).await;
                self.release(index, None).await;
                return failed(error);
            }
            context = Some(new);
        }
        let Some(mut context) = context else {
            self.release(index, None).await;
            return failed(ExecutorError::Configuration("missing executor context"));
        };
        if let Some(refresh) = refresh {
            let refreshed = refresh().await.ok().filter(|template| {
                ExecutorKey::new(template).is_ok_and(|live| live == context.key)
            });
            let Some(refreshed) = refreshed else {
                self.retire(&mut context).await;
                self.release(index, None).await;
                return failed(ExecutorError::Configuration(
                    "namespace credentials no longer authorise this command",
                ));
            };
            template = refreshed;
        }
        let mut output = CapturedOutput::default();
        template.command = Some(task.args.clone());
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
        let preparation = crate::grill::image_config::resolve_process(
            &mut process,
            &context.image.config,
            &context.image.rootfs,
        )
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
                self.retire(&mut context).await;
                self.release(index, None).await;
                return failed(error);
            }
        };
        let run_id = task
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == "RELIABURGER_BATCH_ID")
            .and_then(|(_, value)| value.parse().ok());
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
            socket.write_all(&bytes).await?;
            // The encoded buffer holds live secrets only during submission.
            bytes.fill(0);
            let mut started = false;
            let mut logs = CommandLogs::new(task, &template, sink);
            if task
                .env
                .iter()
                .rev()
                .find(|(key, _)| key == "RELIABURGER_TASK_COUNT")
                .is_some_and(|(_, value)| value == "1")
                && let Some(run) = task
                    .env
                    .iter()
                    .rev()
                    .find(|(key, _)| key == "RELIABURGER_BATCH_ID")
                    .and_then(|(_, value)| value.parse().ok())
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
                        self.slots.lock().await[index].active_run = run_id;
                    }
                    protocol::Event::Output { stream, bytes } if started => {
                        output.push(&bytes);
                        logs.push(stream, &bytes).await;
                    }
                    protocol::Event::Exited(code) if started => {
                        logs.finish().await;
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
            self.retire(&mut context).await;
            self.release(index, None).await;
        } else {
            context.sequence = sequence;
            context.idle_since = tokio::time::Instant::now();
            self.release(index, Some(context)).await;
        }
        Attempt { outcome, output }
    }
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
    for child in ["helper", "task"] {
        std::fs::create_dir_all(base.join(child))?;
    }
    let task = base.join("task");
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

async fn wait_empty_task(task: &Path) -> std::io::Result<()> {
    loop {
        let events = std::fs::read_to_string(task.join("cgroup.events"))?;
        if events.lines().any(|line| line == "populated 0") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
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
        let field = |name| {
            task.env
                .iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        Self {
            live: None,
            sink: sink.filter(|_| field("RELIABURGER_TASK_COUNT").as_deref() == Some("1")),
            app: field("RELIABURGER_JOB_NAME").unwrap_or_else(|| "job".into()),
            namespace: template
                .namespace
                .clone()
                .unwrap_or_else(|| "default".into()),
            instance: format!("run-{}", field("RELIABURGER_BATCH_ID").unwrap_or_default()),
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
