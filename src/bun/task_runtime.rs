//! Container attempts use the same durable runtime owner as applications.
//! Image layers are cached by the runtime. A slot remains occupied during
//! cancellation and uncertain retirement; accepting SIGKILL is not proof of exit.
use super::task_executor::{
    Attempt, AttemptOutcome, CapturedOutput, OUTPUT_KEEP_BYTES, TaskInvocation, TaskRunner,
};
use crate::grill::state::ContainerState;
use crate::grill::{AnyGrill, Grill};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(crate) struct SingletonRuntime {
    pub(crate) instance: crate::grill::InstanceId,
    pub(crate) accepting: std::sync::atomic::AtomicBool,
    pub(crate) followers: std::sync::atomic::AtomicUsize,
    pub(crate) changed: Notify,
    pub(crate) retired: CancellationToken,
    pub(crate) command_logs: Option<std::sync::Arc<super::reusable_executor::CommandLogStream>>,
}

/// A follower is bound to one run, never to a subsequent occupant of its slot.
/// Cleanup allows readers to drain, then cancels stalled readers before reuse.
pub(crate) struct SingletonLogBinding(std::sync::Arc<SingletonRuntime>);
impl SingletonLogBinding {
    pub(crate) fn retired(&self) -> CancellationToken {
        self.0.retired.clone()
    }
    pub(crate) async fn follow<G: Grill>(
        self,
        runtime: &G,
        instance: &crate::grill::InstanceId,
        lines: tokio::sync::mpsc::Sender<crate::ketchup::types::CapturedLine>,
        offsets: &crate::ketchup::types::CaptureOffsets,
    ) {
        if let Some(stream) = &self.0.command_logs {
            stream.follow(lines, &self.0.retired).await;
        } else {
            tokio::select! {
                biased;
                () = self.0.retired.cancelled() => {},
                () = runtime.follow_logs(instance, lines, offsets) => {},
            }
        }
    }
}
impl Drop for SingletonLogBinding {
    fn drop(&mut self) {
        self.0
            .followers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}

/// Owned execution backend; host execution retains its separate admission policy.
pub struct OwnedRunner<G: Grill + Clone> {
    runtime: G,
    #[cfg(target_os = "linux")]
    reusable: std::sync::OnceLock<std::sync::Arc<super::reusable_executor::ReusablePool<G>>>,
    #[cfg(target_os = "linux")]
    host: std::sync::OnceLock<std::sync::Arc<super::reusable_executor::ReusablePool<G>>>,
    #[cfg(target_os = "linux")]
    host_runtime: Option<crate::grill::ProcessGrill>,
    budget: Mutex<std::sync::Arc<super::execution_budget::ExecutionBudget>>,
    slots: Mutex<VecDeque<u32>>,
    prefix: String,
    singletons: Mutex<BTreeMap<u64, std::sync::Arc<SingletonRuntime>>>,
    available: Notify,
    secrets: Option<(std::sync::Arc<crate::council::CouncilNode>, [u8; 32])>,
    log_sink: Option<tokio::sync::mpsc::Sender<crate::ketchup::types::LogRecord>>,
    capture_offsets: std::sync::Arc<crate::ketchup::types::CaptureOffsets>,
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    namespace_policy: Option<std::sync::Arc<super::task_namespace::TaskNamespacePolicy>>,
}
impl<G: Grill + Clone> OwnedRunner<G> {
    /// Share the exact runtime, ownership inventory and image cache of the agent.
    pub fn new(runtime: G) -> Self {
        Self::with_prefix(runtime, format!("{:032x}", rand::random::<u128>()))
    }
    fn with_prefix(runtime: G, prefix: String) -> Self {
        #[cfg(target_os = "linux")]
        let host_runtime = runtime.host_executor_runtime();
        Self {
            runtime,
            #[cfg(target_os = "linux")]
            reusable: std::sync::OnceLock::new(),
            #[cfg(target_os = "linux")]
            host: std::sync::OnceLock::new(),
            #[cfg(target_os = "linux")]
            host_runtime,
            budget: Mutex::new(super::execution_budget::ExecutionBudget::new(
                crate::meat::Resources::new(256_000, u64::MAX, 0),
            )),
            slots: Mutex::new((0..256).collect()),
            prefix,
            singletons: Mutex::new(BTreeMap::new()),
            available: Notify::new(),
            secrets: None,
            log_sink: None,
            capture_offsets: Default::default(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            namespace_policy: None,
        }
    }
    async fn resolve_template(
        &self,
        template: &crate::config::job::JobSpec,
    ) -> Result<crate::config::job::JobSpec, String> {
        let resolved = template.clone();
        if !resolved.env.values().any(|value| value.is_encrypted()) {
            return Ok(resolved);
        }
        let identities = match &self.secrets {
            Some((council, ikm)) => crate::sesame::secret::namespace_identities(
                &council.security_state().await,
                resolved.namespace.as_deref().unwrap_or("default"),
                ikm,
            ),
            None => Vec::new(),
        };
        tokio::task::spawn_blocking(move || decrypt_template(resolved, identities))
            .await
            .map_err(|_| "namespace secret resolution stopped".to_string())?
    }
    /// Whether host jobs can use owned executors with enforced Linux limits.
    pub fn supports_host_limits(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.host_runtime.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
    /// Cumulative phase distributions, bounded independently of completed jobs.
    /// Callers compare snapshots from the same pool lifetime and binary.
    #[cfg(target_os = "linux")]
    pub fn executor_timings(
        &self,
        runtime: crate::config::job::JobRuntime,
    ) -> Option<super::reusable_executor::timings::TimingSnapshot>
    where
        G: 'static,
    {
        match runtime {
            crate::config::job::JobRuntime::Process => self.host.get(),
            crate::config::job::JobRuntime::SharedRunc => self.reusable.get(),
            _ => None,
        }
        .map(|pool| pool.timings())
    }
    /// Compatible idle contexts already own their complete resource request.
    #[cfg(target_os = "linux")]
    pub(crate) async fn reusable_capacity(&self, template: &crate::config::job::JobSpec) -> u32
    where
        G: 'static,
    {
        use super::reusable_executor::{ExecutorKey, ExecutorProfile, MAX_EXECUTOR_SLOTS};
        let Ok(profile) = ExecutorProfile::new(template) else {
            return 0;
        };
        let pool = if template.runtime == crate::config::job::JobRuntime::Process {
            self.host.get()
        } else {
            self.reusable.get()
        };
        if let Some(pool) = pool {
            let key = self
                .resolve_template(template)
                .await
                .ok()
                .and_then(|resolved| ExecutorKey::new(&resolved).ok());
            return pool.available_slots(key, profile.reservation).await;
        }
        let budget = self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if budget.has_waiters() {
            return 0;
        }
        let available = budget.available();
        (available.cpu_millicores / profile.reservation.cpu_millicores)
            .min(available.memory_bytes / profile.reservation.memory_bytes)
            .min(MAX_EXECUTOR_SLOTS as u64) as u32
    }
    /// Share application and idle executor commitments on this node.
    pub fn with_budget(
        self,
        budget: std::sync::Arc<super::execution_budget::ExecutionBudget>,
    ) -> Self {
        self.set_budget(budget);
        self
    }
    pub(crate) fn set_budget(
        &self,
        budget: std::sync::Arc<super::execution_budget::ExecutionBudget>,
    ) {
        *self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = budget;
    }
    /// A smaller executor pool for constrained nodes and real-runtime tests.
    pub fn with_slot_count(runtime: G, slots: u32) -> Self {
        let runner = Self::new(runtime);
        *runner.slots.lock().expect("executor slots poisoned") = (0..slots.clamp(1, 256)).collect();
        runner
    }
    /// Persist a private executor identity so each namespace reuses its bounded
    /// slot identities across restarts. Historical namespace journals still need
    /// retention accounting; this is not a global metadata bound. The agent
    /// retires old launches before admission; create replaces retired generations.
    pub fn for_data_dir(runtime: G, data_dir: &std::path::Path) -> std::io::Result<Self> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = data_dir.join("batch-executor-id");
        let prefix = match std::fs::read_to_string(&path) {
            Ok(prefix) => prefix,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let prefix = format!("{:032x}", rand::random::<u128>());
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                file.write_all(prefix.as_bytes())?;
                file.sync_all()?;
                std::fs::File::open(data_dir)?.sync_all()?;
                prefix
            }
            Err(error) => return Err(error),
        };
        if prefix.len() != 32 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(std::io::Error::other("invalid batch executor identity"));
        }
        Ok(Self::with_prefix(runtime, prefix))
    }
    /// Use the agent's kernel map to enforce the delegated source namespace.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub fn with_namespace_policy(
        mut self,
        policy: std::sync::Arc<super::task_namespace::TaskNamespacePolicy>,
    ) -> Self {
        self.namespace_policy = Some(policy);
        self
    }
    /// Resolve live namespace keys at execution, keeping plaintext outside replicated state.
    pub fn with_secrets(
        mut self,
        council: std::sync::Arc<crate::council::CouncilNode>,
        ikm: [u8; 32],
    ) -> Self {
        self.secrets = Some((council, ikm));
        self
    }
    /// Singleton jobs retain the ordinary live log stream and ingest checkpoints.
    pub fn with_log_sink(
        mut self,
        sink: Option<tokio::sync::mpsc::Sender<crate::ketchup::types::LogRecord>>,
        offsets: std::sync::Arc<crate::ketchup::types::CaptureOffsets>,
    ) -> Self {
        self.log_sink = sink;
        self.capture_offsets = offsets;
        self
    }

    /// Resolve an active singleton without exposing a reusable slot as its public identity.
    pub fn singleton_instance(&self, run: u64) -> Option<crate::grill::InstanceId> {
        self.singletons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .map(|active| active.instance.clone())
    }
    /// Bind a reader before cleanup closes admission to this physical generation.
    pub(crate) fn singleton_runtime(
        &self,
        run: u64,
    ) -> Option<(G, crate::grill::InstanceId, SingletonLogBinding)> {
        let map = self
            .singletons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let active = map.get(&run)?;
        if !active.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        active
            .followers
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Some((
            self.runtime.clone(),
            active.instance.clone(),
            SingletonLogBinding(active.clone()),
        ))
    }
    async fn retire_singleton(&self, run: u64) {
        let active = {
            let map = self
                .singletons
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let active = map.get(&run).cloned();
            if let Some(active) = &active {
                active
                    .accepting
                    .store(false, std::sync::atomic::Ordering::Release);
            }
            active
        };
        if let Some(active) = active {
            let drain = async {
                loop {
                    let changed = active.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    if active.followers.load(std::sync::atomic::Ordering::Acquire) == 0 {
                        break;
                    }
                    changed.await;
                }
            };
            let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
            active.retired.cancel();
        }
        self.singletons
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&run);
    }

    async fn slot(&self, cancel: &CancellationToken) -> Option<u32> {
        loop {
            let available = self.available.notified();
            tokio::pin!(available);
            available.as_mut().enable();
            if cancel.is_cancelled() {
                return None;
            }
            if let Some(id) = self
                .slots
                .lock()
                .expect("executor slots poisoned")
                .pop_front()
            {
                return Some(id);
            }
            tokio::select! { biased; () = cancel.cancelled() => return None, () = available => {} }
        }
    }
}
impl OwnedRunner<AnyGrill> {
    /// Container isolation and cgroup limits require rootful Linux execution.
    pub fn supports_host(&self) -> bool {
        self.runtime
            .supports_runtime(crate::grill::records::RuntimeKind::Process)
    }
    /// A singleton preserves the configured runtime's existing workload contract.
    pub fn supports_singleton_image(&self, template: &crate::config::job::JobSpec) -> bool {
        #[cfg(not(target_os = "linux"))]
        let _ = template;
        #[cfg(target_os = "linux")]
        let limits = template.cpu.is_some() || template.memory.is_some();
        match &self.runtime {
            AnyGrill::Process(_) => false,
            #[cfg(target_os = "linux")]
            AnyGrill::Runc(runtime) => !runtime.is_rootless() || !limits,
            #[cfg(target_os = "linux")]
            AnyGrill::Mixed(runtime) => {
                (!runtime.container().is_rootless() && template.image.is_some()) || !limits
            }
            #[cfg(target_os = "macos")]
            AnyGrill::Apple(_) => true,
        }
    }
    pub fn supports_containers(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(runtime) = self.runtime.runc_runtime() {
            return !runtime.is_rootless();
        }
        false
    }
}
impl<G: Grill + Clone + 'static> TaskRunner for OwnedRunner<G> {
    async fn active_commands(
        &self,
        batch_id: u64,
        template: Option<&crate::config::job::JobSpec>,
    ) -> Option<u64> {
        let template = template?;
        #[cfg(target_os = "linux")]
        if (template.runtime == crate::config::job::JobRuntime::SharedRunc
            && self.runtime.reusable_runtime().is_some())
            || (template.runtime == crate::config::job::JobRuntime::Process
                && self.supports_host_limits())
        {
            let pool = if template.runtime == crate::config::job::JobRuntime::Process {
                self.host.get()
            } else {
                self.reusable.get()
            };
            return Some(match pool {
                Some(pool) => pool.active_commands(batch_id).await,
                None => 0,
            });
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (batch_id, template);
        None
    }
    fn owns_admission(&self, task: &TaskInvocation) -> bool {
        task.template.as_ref().is_some_and(|job| {
            job.runtime == crate::config::job::JobRuntime::SharedRunc
                || (job.runtime == crate::config::job::JobRuntime::Process
                    && self.supports_host_limits())
        })
    }
    async fn run(
        &self,
        task: &TaskInvocation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Attempt {
        let Some(template) = task.template.as_ref() else {
            return Attempt {
                outcome: AttemptOutcome::SpawnFailed {
                    reason: "owned tasks require a runtime template".into(),
                },
                output: CapturedOutput::default(),
                ran: None,
            };
        };
        let resolved = match self.resolve_template(template).await {
            Ok(resolved) => resolved,
            Err(_) => {
                return Attempt {
                    outcome: AttemptOutcome::SpawnFailed {
                        reason: "cannot decrypt this job's namespace secrets".into(),
                    },
                    output: CapturedOutput::default(),
                    ran: None,
                };
            }
        };
        let backend = match resolved.runtime {
            crate::config::job::JobRuntime::Process => crate::grill::records::RuntimeKind::Process,
            crate::config::job::JobRuntime::Runc | crate::config::job::JobRuntime::SharedRunc => {
                crate::grill::records::RuntimeKind::Runc
            }
        };
        if let Err(reason) = resolved.validate_runtime() {
            return Attempt {
                outcome: AttemptOutcome::SpawnFailed {
                    reason: reason.into(),
                },
                output: CapturedOutput::default(),
                ran: None,
            };
        }
        if !self.runtime.supports_runtime(backend) {
            return Attempt {
                outcome: AttemptOutcome::SpawnFailed {
                    reason: "selected job runtime is unavailable on this node".into(),
                },
                output: CapturedOutput::default(),
                ran: None,
            };
        }
        if resolved.runtime == crate::config::job::JobRuntime::SharedRunc
            || (resolved.runtime == crate::config::job::JobRuntime::Process
                && self.supports_host_limits())
        {
            #[cfg(target_os = "linux")]
            if let Some(pool) = {
                let count = self
                    .slots
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len()
                    .clamp(1, 32);
                let budget = self
                    .budget
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if resolved.runtime == crate::config::job::JobRuntime::Process {
                    self.host_runtime.as_ref().map(|runtime| {
                        self.host.get_or_init(|| {
                            super::reusable_executor::ReusablePool::new_host(
                                runtime.clone(),
                                self.runtime.clone(),
                                self.prefix.clone(),
                                count,
                                budget,
                                #[cfg(feature = "ebpf")]
                                self.namespace_policy.clone(),
                            )
                        })
                    })
                } else {
                    self.runtime.reusable_runtime().map(|runtime| {
                        self.reusable.get_or_init(|| {
                            super::reusable_executor::ReusablePool::new(
                                runtime,
                                self.runtime.clone(),
                                self.prefix.clone(),
                                count,
                                budget,
                                #[cfg(feature = "ebpf")]
                                self.namespace_policy.clone(),
                            )
                        })
                    })
                }
            } {
                let result = pool
                    .run(
                        task,
                        resolved,
                        timeout,
                        cancel,
                        template
                            .env
                            .values()
                            .any(|value| value.is_encrypted())
                            .then_some(|| self.resolve_template(template)),
                        super::reusable_executor::CommandReporting {
                            sink: self.log_sink.as_ref(),
                            singletons: &self.singletons,
                        },
                    )
                    .await;
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
                    self.retire_singleton(run).await;
                }
                return result;
            }
            return Attempt {
                outcome: AttemptOutcome::SpawnFailed {
                    reason: "shared-runc requires the owned rootful Linux runtime".into(),
                },
                output: CapturedOutput::default(),
                ran: None,
            };
        }
        let Some(slot) = self.slot(cancel).await else {
            return Attempt {
                outcome: AttemptOutcome::Cancelled,
                output: CapturedOutput::default(),
                ran: None,
            };
        };
        let namespace = template.namespace.as_deref().unwrap_or("default");
        let id = crate::grill::InstanceIdentity::new(
            namespace,
            format!("executor-{}", self.prefix),
            slot,
        )
        .instance_id();
        // Every task gets a fresh runtime generation inside a reusable slot.
        // Only retired slots return to the pool. Abandoned futures quarantine
        // their slot until startup retirement proves the previous owner absent.
        let mut spec = resolved;
        spec.command = Some(task.args.clone());
        for (key, value) in &task.env {
            spec.env.insert(
                key.clone(),
                crate::config::types::EnvValue::Plain(value.clone()),
            );
        }
        if let Some(script) = &mut spec.script {
            *script = script.replace("{index}", &task.index.to_string());
        }
        let singleton = task
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == "RELIABURGER_TASK_COUNT")
            .is_some_and(|(_, value)| value == "1");
        let run_id = singleton
            .then(|| {
                task.env
                    .iter()
                    .rev()
                    .find(|(key, _)| key == "RELIABURGER_BATCH_ID")
                    .and_then(|(_, value)| value.parse::<u64>().ok())
            })
            .flatten();
        // Omitted requests have concrete conservative defaults, including limits.
        if template.image.is_some() && (!singleton || self.runtime.honours_cgroup_path()) {
            spec.cpu.get_or_insert(crate::config::types::ResourceRange {
                request: 1000,
                limit: 1000,
            });
            spec.memory
                .get_or_insert(crate::config::types::ResourceRange {
                    request: 64 << 20,
                    limit: 64 << 20,
                });
        }
        let cgroup = crate::grill::cgroup::instance_cgroup_path(
            namespace,
            &format!("executor-{}", self.prefix),
            &id,
        )
        .expect("validated executor identity");
        let mut oci = crate::grill::oci::generate_job_oci_spec(
            "task",
            namespace,
            &spec,
            cgroup.to_str().expect("executor cgroup is UTF-8"),
            None,
        );
        if template.image.is_some() && !singleton {
            oci.process.rlimits.push(crate::grill::oci::OciRlimit {
                kind: "RLIMIT_FSIZE".into(),
                hard: 1 << 20,
                soft: 1 << 20,
            });
        }
        if template.image.is_some() && !singleton {
            // Reusing a runtime slot must not carry writable files between tasks.
            oci.root.readonly = true;
            oci.mounts.push(crate::grill::oci::OciMount {
                destination: "/tmp".into(),
                source: None,
                mount_type: Some("tmpfs".into()),
                options: vec![
                    "nosuid".into(),
                    "nodev".into(),
                    "mode=1777".into(),
                    "size=16m".into(),
                ],
            });
        }
        let deadline = (!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout);
        let interrupted = cancel.child_token();
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        let mut namespace_lease = None;
        let captures_ready = std::sync::atomic::AtomicBool::new(false);
        let execution_authorised = std::sync::atomic::AtomicBool::new(false);
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        let namespace_enforced = self.runtime.honours_cgroup_path_for(&oci);
        let launch_outcome = {
            let launch = async {
                #[cfg(all(feature = "ebpf", target_os = "linux"))]
                if namespace_enforced && let Some(policy) = &self.namespace_policy {
                    namespace_lease =
                        Some(policy.acquire(namespace, &cgroup).await.map_err(|error| {
                            crate::grill::GrillError::StartFailed {
                                instance: id.clone(),
                                reason: error.to_string(),
                            }
                        })?);
                }
                self.runtime.create(&id, &oci).await?;
                if let Some(stem) = self.runtime.log_stem(&id).await {
                    tokio::task::spawn_blocking(move || reset_captures(&stem))
                        .await
                        .map_err(std::io::Error::other)
                        .and_then(|result| result)
                        .map_err(|error| crate::grill::GrillError::StartFailed {
                            instance: id.clone(),
                            reason: format!("cannot prepare private job capture: {error}"),
                        })?;
                }
                captures_ready.store(true, std::sync::atomic::Ordering::Release);

                if interrupted.is_cancelled()
                    || deadline.is_some_and(|at| tokio::time::Instant::now() >= at)
                {
                    return Ok::<_, crate::grill::GrillError>(false);
                }
                #[cfg(all(feature = "ebpf", target_os = "linux"))]
                if namespace_enforced && let Some(policy) = &self.namespace_policy {
                    policy.check(namespace).await.map_err(|error| {
                        crate::grill::GrillError::StartFailed {
                            instance: id.clone(),
                            reason: error.to_string(),
                        }
                    })?;
                }
                if template.env.values().any(|value| value.is_encrypted()) {
                    self.resolve_template(template).await.map_err(|_| {
                        crate::grill::GrillError::StartFailed {
                            instance: id.clone(),
                            reason: "namespace credentials no longer authorise this command".into(),
                        }
                    })?;
                }
                execution_authorised.store(true, std::sync::atomic::Ordering::Relaxed);
                self.runtime.start(&id).await?;
                if let Some(run) = run_id {
                    self.singletons
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(
                            run,
                            std::sync::Arc::new(SingletonRuntime {
                                command_logs: None,
                                instance: id.clone(),
                                accepting: std::sync::atomic::AtomicBool::new(true),
                                followers: std::sync::atomic::AtomicUsize::new(0),
                                changed: Notify::new(),
                                retired: CancellationToken::new(),
                            }),
                        );
                }

                Ok(true)
            };
            tokio::pin!(launch);
            tokio::select! {
                biased;
                () = cancel.cancelled() => { interrupted.cancel(); let _ = launch.await; Some(AttemptOutcome::Cancelled) },
                () = super::task_executor::wait_deadline(deadline) => { interrupted.cancel(); let _ = launch.await; Some(AttemptOutcome::TimedOut) },
                result = &mut launch => match result {
                    Ok(true) => None,
                    Ok(false) => Some(AttemptOutcome::Cancelled),
                    Err(error) => Some(if execution_authorised.load(std::sync::atomic::Ordering::Relaxed) {
                        AttemptOutcome::Unknown { reason: error.to_string() }
                    } else { AttemptOutcome::SpawnFailed { reason: error.to_string() } }),
                },
            }
        };
        let log_forwarder = if singleton && launch_outcome.is_none() {
            self.log_sink.clone().map(|sink| {
                let runtime = self.runtime.clone();
                let id = id.clone();
                let app = task
                    .env
                    .iter()
                    .rev()
                    .find(|(key, _)| key == "RELIABURGER_JOB_NAME")
                    .map(|(_, value)| value.clone())
                    .unwrap_or_else(|| "job".into());
                let log_instance = task
                    .env
                    .iter()
                    .rev()
                    .find(|(key, _)| key == "RELIABURGER_BATCH_ID")
                    .and_then(|(_, value)| value.parse::<u64>().ok())
                    .map_or_else(|| id.0.clone(), |id| format!("run-{id}"));
                let namespace = namespace.to_string();
                let offsets = self.capture_offsets.clone();
                tokio::spawn(async move {
                    let (sender, mut receiver) =
                        tokio::sync::mpsc::channel::<crate::ketchup::types::CapturedLine>(256);
                    let producer = runtime.follow_logs(&id, sender, &offsets);
                    let consumer = async {
                        while let Some(line) = receiver.recv().await {
                            if sink
                                .send(crate::ketchup::types::LogRecord {
                                    app: app.clone(),
                                    namespace: namespace.clone(),
                                    instance: log_instance.clone(),
                                    stream: line.stream,
                                    line: line.line,
                                    position: line.position,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    };
                    tokio::join!(producer, consumer);
                })
            })
        } else {
            None
        };
        let outcome = if let Some(outcome) = launch_outcome {
            outcome
        } else {
            let observe = async {
                loop {
                    #[cfg(all(feature = "ebpf", target_os = "linux"))]
                    if namespace_enforced && let Some(policy) = &self.namespace_policy {
                        policy.check(namespace).await.map_err(|error| {
                            crate::grill::GrillError::StateUnavailable {
                                instance: id.clone(),
                                reason: error.to_string(),
                            }
                        })?;
                    }
                    if self.runtime.has_exited(&id).await? {
                        return self.runtime.exit_code(&id).await;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => AttemptOutcome::Cancelled,
                () = super::task_executor::wait_deadline(deadline) => AttemptOutcome::TimedOut,
                result = observe => match result {
                    Ok(Some(code)) => AttemptOutcome::Exited { code },
                    Ok(None) => AttemptOutcome::Unknown { reason: "runtime confirmed exit but has no exit status".into() },
                    Err(error) => AttemptOutcome::Unknown { reason: error.to_string() },
                },
            }
        };
        // Cleanup can outlast timeout when ownership is uncertain. Holding the
        // attempt here prevents a replacement from consuming the same resources.
        let mut cleanup_round = 0u64;
        loop {
            match self.runtime.state(&id).await {
                Ok(ContainerState::Stopped) | Err(crate::grill::GrillError::NotFound { .. }) => {
                    break;
                }
                _ => {
                    if let Err(error) = self.runtime.kill(&id).await
                        && cleanup_round.is_multiple_of(600)
                    {
                        eprintln!(
                            "batch retirement failed for {id}: {error}; capacity remains held"
                        );
                    }
                }
            }
            cleanup_round = cleanup_round.saturating_add(1);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let mut output = CapturedOutput::default();
        if let AttemptOutcome::SpawnFailed { reason } | AttemptOutcome::Unknown { reason } =
            &outcome
        {
            output.push(reason.as_bytes());
            output.push(b"\n");
        }
        if captures_ready.load(std::sync::atomic::Ordering::Acquire)
            && (singleton || !outcome.succeeded())
            && let Some(stem) = self.runtime.log_stem(&id).await
        {
            for suffix in ["stdout", "stderr"] {
                if let Ok(mut file) = tokio::fs::File::open(stem.with_extension(suffix)).await {
                    let mut head = vec![0; OUTPUT_KEEP_BYTES];
                    if let Ok(n) = file.read(&mut head).await {
                        output.push(&head[..n]);
                    }
                    if let Ok(metadata) = file.metadata().await {
                        let length = metadata.len();
                        if length > OUTPUT_KEEP_BYTES as u64 {
                            let start = (OUTPUT_KEEP_BYTES as u64)
                                .max(length.saturating_sub(OUTPUT_KEEP_BYTES as u64));
                            if file.seek(std::io::SeekFrom::Start(start)).await.is_ok() {
                                let mut tail = vec![0; OUTPUT_KEEP_BYTES];
                                if let Ok(n) = file.read(&mut tail).await {
                                    output.push(&tail[..n]);
                                }
                            }
                            output.total_bytes = output
                                .total_bytes
                                .saturating_add(start.saturating_sub(OUTPUT_KEEP_BYTES as u64));
                        }
                    }
                }
            }
        }
        // Process adapters used without a capture directory retain output in
        // memory. Read it only after positive exit; logs() drains both readers.
        if captures_ready.load(std::sync::atomic::Ordering::Acquire)
            && (singleton || !outcome.succeeded())
            && self.runtime.log_stem(&id).await.is_none()
            && let Ok(text) = self.runtime.logs(&id).await
        {
            output.push(text.as_bytes());
        }
        if let Some(forwarder) = log_forwarder {
            let _ = forwarder.await;
        }
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(lease) = namespace_lease {
            lease.retired().await;
        }
        if let Some(run) = run_id {
            self.retire_singleton(run).await;
        }
        self.slots
            .lock()
            .expect("executor slots poisoned")
            .push_back(slot);
        self.available.notify_one();
        Attempt {
            outcome,
            output,
            ran: None,
        }
    }
}

// Reusable process identities otherwise append to their predecessor's capture.
// Replacing nonempty files also gives checkpoint readers a new inode. Fresh OCI
// generations have no prior bytes, so they incur no extra publication writes.
fn reset_captures(stem: &std::path::Path) -> std::io::Result<()> {
    for suffix in ["stdout", "stderr"] {
        let path = stem.with_extension(suffix);
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > 0 => {
                crate::sesame::identity::atomic_write_mode(&path, b"", Some(0o600))?
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn decrypt_template(
    mut spec: crate::config::job::JobSpec,
    identities: Vec<age::x25519::Identity>,
) -> Result<crate::config::job::JobSpec, String> {
    for (key, value) in &mut spec.env {
        if let crate::config::types::EnvValue::Encrypted(sealed) = value {
            let plain = identities
                .iter()
                .find_map(|identity| crate::sesame::secret::decrypt_secret(sealed, identity).ok())
                .ok_or_else(|| format!("no live namespace key can decrypt {key}"))?;
            *value = crate::config::types::EnvValue::Plain(plain);
        }
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invocation() -> TaskInvocation {
        TaskInvocation {
            template: Some(Box::new(
                toml::from_str("runtime='process'\nexec='/unused'\nnamespace='tenant-a'").unwrap(),
            )),
            index: 0,
            attempt: 1,
            program: "/unused".into(),
            args: vec!["worker".into()],
            env: vec![],
        }
    }
    #[tokio::test]
    async fn a_singleton_maps_its_run_to_the_active_runtime_and_retires_the_mapping() {
        let runtime = crate::grill::ProcessGrill::new();
        let runner = std::sync::Arc::new(OwnedRunner::new(runtime));
        let mut task = invocation();
        task.template = Some(Box::new(
            toml::from_str("runtime='process'\nexec='/bin/sh'\nnamespace='tenant-a'").unwrap(),
        ));
        task.args = vec!["-c".into(), "printf live; exec sleep 30".into()];
        task.env = vec![
            ("RELIABURGER_TASK_COUNT".into(), "1".into()),
            ("RELIABURGER_BATCH_ID".into(), "42".into()),
        ];
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker = runner.clone();
        let work =
            tokio::spawn(async move { worker.run(&task, Duration::ZERO, &worker_cancel).await });
        let id = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(id) = runner.singleton_instance(42)
                    && runner.runtime.pid(&id).await.unwrap().is_some()
                {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(runner.runtime.pid(&id).await.unwrap().is_some());
        assert!(runner.singleton_instance(41).is_none());
        cancel.cancel();
        let result = work.await.unwrap();
        assert_eq!(result.outcome, AttemptOutcome::Cancelled);
        assert!(runner.singleton_instance(42).is_none());
        assert_eq!(
            runner.runtime.state(&id).await.unwrap(),
            ContainerState::Stopped
        );
    }

    #[tokio::test]
    async fn a_live_log_binding_fences_slot_reuse_until_the_reader_retires() {
        let runner = std::sync::Arc::new(OwnedRunner::with_slot_count(
            crate::grill::ProcessGrill::new(),
            1,
        ));
        let mut task = invocation();
        task.template = Some(Box::new(
            toml::from_str("runtime='process'\nexec='/bin/sh'\nnamespace='tenant-a'").unwrap(),
        ));
        task.args = vec!["-c".into(), "echo original; exec sleep 30".into()];
        task.env = vec![
            ("RELIABURGER_TASK_COUNT".into(), "1".into()),
            ("RELIABURGER_BATCH_ID".into(), "42".into()),
        ];
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker = runner.clone();
        let mut work =
            tokio::spawn(async move { worker.run(&task, Duration::ZERO, &worker_cancel).await });
        let binding = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(binding) = runner.singleton_runtime(42)
                    && binding.0.pid(&binding.1).await.ok().flatten().is_some()
                {
                    break binding;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            while runner.runtime.state(&binding.1).await.unwrap() != ContainerState::Stopped {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut work)
                .await
                .is_err(),
            "a reusable physical slot was released while its original log binding was live"
        );
        assert!(runner.singleton_instance(42).is_some());
        drop(binding);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), work)
                .await
                .unwrap()
                .unwrap()
                .outcome,
            AttemptOutcome::Cancelled
        );
        assert!(runner.singleton_instance(42).is_none());
    }

    #[tokio::test]
    async fn a_reused_process_slot_cannot_retain_another_runs_capture() {
        let logs = tempfile::tempdir().unwrap();
        let runner = OwnedRunner::with_slot_count(
            crate::grill::ProcessGrill::with_log_dir(logs.path().into()),
            1,
        );
        for id in [1, 2] {
            let mut task = invocation();
            task.template = Some(Box::new(
                toml::from_str("runtime='process'\nexec='/bin/sh'\nnamespace='tenant-a'").unwrap(),
            ));
            task.args = vec!["-c".into(), format!("echo output-{id}")];
            task.env = vec![
                ("RELIABURGER_TASK_COUNT".into(), "1".into()),
                ("RELIABURGER_BATCH_ID".into(), id.to_string()),
            ];
            let attempt = runner
                .run(&task, Duration::ZERO, &CancellationToken::new())
                .await;
            assert_eq!(attempt.outcome, AttemptOutcome::Exited { code: 0 });
            assert_eq!(
                String::from_utf8(attempt.output.head).unwrap(),
                format!("output-{id}\n")
            );
        }
    }

    #[tokio::test]
    async fn active_singleton_logs_follow_its_owned_runtime() {
        use crate::grill::Grill;
        let runner = std::sync::Arc::new(OwnedRunner::new(crate::grill::ProcessGrill::new()));
        let mut task = invocation();
        task.template = Some(Box::new(
            toml::from_str("runtime='process'\nexec='/bin/sh'\nnamespace='tenant-a'").unwrap(),
        ));
        task.args = vec!["-c".into(), "printf 'live-output\n'; sleep 30".into()];
        task.env = vec![
            ("RELIABURGER_TASK_COUNT".into(), "1".into()),
            ("RELIABURGER_BATCH_ID".into(), "42".into()),
        ];
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker = runner.clone();
        let work =
            tokio::spawn(async move { worker.run(&task, Duration::ZERO, &worker_cancel).await });
        let (runtime, id, binding) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pair) = runner.singleton_runtime(42)
                    && pair.0.pid(&pair.1).await.unwrap().is_some()
                {
                    break pair;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let follow =
            tokio::spawn(
                async move { binding.follow(&runtime, &id, tx, &Default::default()).await },
            );
        let line = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line.line, "live-output");
        cancel.cancel();
        let _ = work.await.unwrap();
        follow.await.unwrap();
    }

    #[tokio::test]
    async fn singleton_logs_identify_the_run_instead_of_a_reusable_executor_slot() {
        let (sink, mut logs) = tokio::sync::mpsc::channel(16);
        let runtime = crate::grill::ProcessGrill::new();
        let runner = OwnedRunner::new(runtime).with_log_sink(Some(sink), Default::default());
        let mut task = invocation();
        task.template = Some(Box::new(toml::from_str("runtime='process'\nexec='/bin/sh'\nnamespace='tenant-a'\ncommand=['-c','echo output']").unwrap()));
        task.args = vec!["-c".into(), "echo output".into()];
        task.env = vec![
            ("RELIABURGER_TASK_COUNT".into(), "1".into()),
            ("RELIABURGER_BATCH_ID".into(), "42".into()),
            ("RELIABURGER_JOB_NAME".into(), "migrate".into()),
        ];
        let result = runner
            .run(&task, Duration::from_secs(5), &CancellationToken::new())
            .await;
        assert!(result.outcome.succeeded(), "{:?}", result.outcome);
        let record = logs.recv().await.unwrap();
        assert_eq!(record.instance, "run-42");
        assert_eq!(record.app, "migrate");
        assert_eq!(record.namespace, "tenant-a");
    }

    #[test]
    fn encrypted_templates_decrypt_only_with_the_execution_namespaces_keys() {
        let identity = age::x25519::Identity::generate();
        let other = age::x25519::Identity::generate();
        let sealed = crate::sesame::secret::encrypt_secret(
            "private-token",
            &identity.to_public().to_string(),
        )
        .unwrap();
        let mut spec: crate::config::job::JobSpec =
            toml::from_str("runtime='process'\nexec='/bin/true'\nnamespace='team-a'").unwrap();
        spec.env.insert(
            "TOKEN".into(),
            crate::config::types::EnvValue::Encrypted(sealed),
        );
        assert!(decrypt_template(spec.clone(), vec![other]).is_err());
        assert!(decrypt_template(spec.clone(), vec![]).is_err());
        let resolved = decrypt_template(spec.clone(), vec![identity]).unwrap();
        assert_eq!(
            resolved.env["TOKEN"],
            crate::config::types::EnvValue::Plain("private-token".into())
        );
        assert!(
            !serde_json::to_string(&spec)
                .unwrap()
                .contains("private-token")
        );
    }

    #[tokio::test]
    async fn task_runtime_launch_errors_are_preserved_in_failed_output() {
        let runtime = crate::grill::mock::MockGrill::new();
        runtime.set_fail_create(true);
        let runner = OwnedRunner::new(runtime);
        let result = runner
            .run(
                &invocation(),
                Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await;
        let AttemptOutcome::SpawnFailed { reason } = result.outcome else {
            panic!("expected runtime refusal");
        };
        assert!(String::from_utf8_lossy(&result.output.head).contains(&reason));
        assert_eq!(runner.slots.lock().unwrap().len(), 256);
    }

    #[tokio::test]
    async fn task_runtime_cancellation_waits_for_mutating_create_before_retirement() {
        let runtime = crate::grill::mock::MockGrill::new();
        runtime.block_creates();
        let runner = std::sync::Arc::new(OwnedRunner::new(runtime.clone()));
        let cancel = CancellationToken::new();
        let running = {
            let runner = runner.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                runner
                    .run(&invocation(), Duration::from_secs(30), &cancel)
                    .await
            })
        };
        runtime.wait_for_creates(1).await;
        cancel.cancel();
        tokio::task::yield_now().await;
        assert!(
            !running.is_finished(),
            "lost track of an in-flight runtime mutation"
        );
        assert_eq!(runner.slots.lock().unwrap().len(), 255);
        runtime.release_creates(1);
        let result = tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.outcome, AttemptOutcome::Cancelled);
        assert!(
            !runtime
                .calls()
                .iter()
                .any(|(operation, _)| operation == "start")
        );
        assert!(
            runtime
                .calls()
                .iter()
                .any(|(_, id)| id.0.starts_with("tenant-a__"))
        );
        assert_eq!(runner.slots.lock().unwrap().len(), 256);
    }
    #[tokio::test]
    async fn task_runtime_uncertain_exit_holds_the_slot_until_positive_retirement() {
        let runtime = crate::grill::mock::MockGrill::new();
        runtime.set_ignore_kill(true);
        runtime.block_creates();
        let runner = std::sync::Arc::new(OwnedRunner::new(runtime.clone()));
        let cancel = CancellationToken::new();
        let running = {
            let runner = runner.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                runner
                    .run(&invocation(), Duration::from_secs(30), &cancel)
                    .await
            })
        };
        runtime.wait_for_creates(1).await;
        let id = runtime
            .calls()
            .into_iter()
            .find(|(operation, _)| operation == "create")
            .unwrap()
            .1;
        runtime.set_state(&id, ContainerState::Running);
        runtime.release_creates(1);
        cancel.cancel();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(!running.is_finished());
        assert_eq!(runner.slots.lock().unwrap().len(), 255);
        runtime.set_state(&id, ContainerState::Stopped);
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runner.slots.lock().unwrap().len(), 256);
    }
}
