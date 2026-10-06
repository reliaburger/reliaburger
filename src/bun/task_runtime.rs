//! Container attempts use the same durable runtime owner as applications.
//! Image layers are cached by the runtime. A slot remains occupied during
//! cancellation and uncertain retirement; accepting SIGKILL is not proof of exit.
use super::task_executor::{
    Attempt, AttemptOutcome, CapturedOutput, OUTPUT_KEEP_BYTES, TaskInvocation, TaskRunner,
};
use crate::grill::state::ContainerState;
use crate::grill::{AnyGrill, Grill};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Owned execution backend; host execution retains its separate admission policy.
pub struct OwnedRunner<G: Grill + Clone> {
    runtime: G,
    slots: Mutex<VecDeque<u32>>,
    prefix: String,
    available: Notify,
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    namespace_policy: Option<std::sync::Arc<super::task_namespace::TaskNamespacePolicy>>,
}
impl<G: Grill + Clone> OwnedRunner<G> {
    /// Share the exact runtime, ownership inventory and image cache of the agent.
    pub fn new(runtime: G) -> Self {
        Self::with_prefix(runtime, format!("{:032x}", rand::random::<u128>()))
    }
    fn with_prefix(runtime: G, prefix: String) -> Self {
        Self {
            runtime,
            slots: Mutex::new((0..256).collect()),
            prefix,
            available: Notify::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            namespace_policy: None,
        }
    }
    /// A smaller executor pool for constrained nodes and real-runtime tests.
    pub fn with_slot_count(runtime: G, slots: u32) -> Self {
        let runner = Self::new(runtime);
        *runner.slots.lock().expect("executor slots poisoned") = (0..slots.clamp(1, 256)).collect();
        runner
    }
    /// Persist a private executor identity so runtime artifacts remain bounded
    /// by pool size across node restarts. The agent retires old launches before
    /// task admission starts; runtime create replaces only retired generations.
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
        matches!(&self.runtime, AnyGrill::Process(_))
    }
    pub fn supports_containers(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let AnyGrill::Runc(runtime) = &self.runtime {
            return !runtime.is_rootless();
        }
        false
    }
}
impl<G: Grill + Clone + 'static> TaskRunner for OwnedRunner<G> {
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
            };
        };
        let Some(slot) = self.slot(cancel).await else {
            return Attempt {
                outcome: AttemptOutcome::Cancelled,
                output: CapturedOutput::default(),
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
        let mut spec = template.as_ref().clone();
        spec.command = Some(task.args.clone());
        spec.env = task
            .env
            .iter()
            .map(|(k, v)| (k.clone(), crate::config::types::EnvValue::Plain(v.clone())))
            .collect();
        // Omitted requests have concrete conservative defaults, including limits.
        spec.cpu.get_or_insert(crate::config::types::ResourceRange {
            request: 1000,
            limit: 1000,
        });
        spec.memory
            .get_or_insert(crate::config::types::ResourceRange {
                request: 64 << 20,
                limit: 64 << 20,
            });
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
        if template.image.is_some() {
            oci.process.rlimits.push(crate::grill::oci::OciRlimit {
                kind: "RLIMIT_FSIZE".into(),
                hard: 1 << 20,
                soft: 1 << 20,
            });
        }
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
        let deadline = tokio::time::Instant::now() + timeout;
        let interrupted = cancel.child_token();
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        let mut namespace_lease = None;
        let launch_outcome = {
            let launch = async {
                #[cfg(all(feature = "ebpf", target_os = "linux"))]
                if let Some(policy) = &self.namespace_policy {
                    namespace_lease =
                        Some(policy.acquire(namespace, &cgroup).await.map_err(|error| {
                            crate::grill::GrillError::StartFailed {
                                instance: id.clone(),
                                reason: error.to_string(),
                            }
                        })?);
                }
                self.runtime.create(&id, &oci).await?;
                if interrupted.is_cancelled() || tokio::time::Instant::now() >= deadline {
                    return Ok::<_, crate::grill::GrillError>(false);
                }
                #[cfg(all(feature = "ebpf", target_os = "linux"))]
                if let Some(policy) = &self.namespace_policy {
                    policy.check(namespace).await.map_err(|error| {
                        crate::grill::GrillError::StartFailed {
                            instance: id.clone(),
                            reason: error.to_string(),
                        }
                    })?;
                }
                self.runtime.start(&id).await?;
                Ok(true)
            };
            tokio::pin!(launch);
            tokio::select! {
                biased;
                () = cancel.cancelled() => { interrupted.cancel(); let _ = launch.await; Some(AttemptOutcome::Cancelled) },
                () = tokio::time::sleep_until(deadline) => { interrupted.cancel(); let _ = launch.await; Some(AttemptOutcome::TimedOut) },
                result = &mut launch => match result {
                    Ok(true) => None,
                    Ok(false) => Some(AttemptOutcome::Cancelled),
                    Err(error) => Some(AttemptOutcome::SpawnFailed { reason: error.to_string() }),
                },
            }
        };
        let outcome = if let Some(outcome) = launch_outcome {
            outcome
        } else {
            let observe = async {
                loop {
                    #[cfg(all(feature = "ebpf", target_os = "linux"))]
                    if let Some(policy) = &self.namespace_policy {
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
                () = tokio::time::sleep_until(deadline) => AttemptOutcome::TimedOut,
                result = observe => match result {
                    Ok(Some(code)) => AttemptOutcome::Exited { code },
                    Ok(None) => AttemptOutcome::SpawnFailed { reason: "runtime confirmed exit but has no exit status".into() },
                    Err(error) => AttemptOutcome::SpawnFailed { reason: error.to_string() },
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
        if let AttemptOutcome::SpawnFailed { reason } = &outcome {
            output.push(reason.as_bytes());
            output.push(b"\n");
        }
        if !outcome.succeeded()
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
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(lease) = namespace_lease {
            lease.retired().await;
        }
        self.slots
            .lock()
            .expect("executor slots poisoned")
            .push_back(slot);
        self.available.notify_one();
        Attempt { outcome, output }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invocation() -> TaskInvocation {
        TaskInvocation {
            template: Some(Box::new(
                toml::from_str("image='fixture:v1'\nnamespace='tenant-a'").unwrap(),
            )),
            index: 0,
            attempt: 1,
            program: "/unused".into(),
            args: vec!["worker".into()],
            env: vec![],
        }
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
