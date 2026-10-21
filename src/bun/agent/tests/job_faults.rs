//! Faults against job and task-array attempts (#704).
//!
//! Attempts don't live in the supervisor, so these tests list them in the
//! agent's live-attempt list the way the task runtime does. A reusable
//! executor's command is reached through its task cgroup's `cgroup.procs`;
//! here that is a plain file listing a real sleeping process, which is all
//! the agent reads.

use super::*;
use crate::bun::live_attempts::{AttemptHandle, LiveAttempt, LiveAttempts};
use crate::bun::task_executor::{
    Attempt, AttemptOutcome, CapturedOutput, ChunkWork, PoolConfig, TaskFinal, TaskInvocation,
    TaskPool, TaskRunner,
};
use crate::smoker::types::{FaultRequest, FaultSummary, FaultType, WorkloadKind};

/// A stand-in for a task cgroup whose one process is asleep.
struct SleepingTaskGroup {
    dir: tempfile::TempDir,
    child: std::process::Child,
}

impl SleepingTaskGroup {
    fn new() -> Self {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup.procs"), format!("{}\n", child.id())).unwrap();
        Self { dir, child }
    }

    fn path(&self) -> std::path::PathBuf {
        self.dir.path().to_path_buf()
    }

    /// Wait up to five seconds for the process to end; its exit status if
    /// it did.
    async fn ended(&mut self) -> Option<std::process::ExitStatus> {
        for _ in 0..500 {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }
}

impl Drop for SleepingTaskGroup {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn attempt(job: &str, index: u32, handle: AttemptHandle, shared: bool) -> LiveAttempt {
    LiveAttempt {
        namespace: "default".into(),
        job: job.into(),
        batch_id: 42,
        task_count: 4,
        index,
        attempt: 1,
        handle,
        cgroup: None,
        shared_executor: shared.then(|| InstanceId("default/executor-abc-0".into())),
    }
}

/// A workload fault as a caller sends it: the caller doesn't say whether
/// the target is an app or a job, and `--instance` is a plain string.
fn request(fault_type: FaultType, target: &str, instance: Option<&str>) -> FaultRequest {
    FaultRequest {
        workload: WorkloadKind::App,
        task_index: None,
        fault_type,
        target_service: target.into(),
        namespace: Some("default".into()),
        target_instance: instance.map(str::to_string),
        target_node: None,
        duration: std::time::Duration::from_secs(30),
        injected_by: "test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    }
}

/// Inject through the agent, finishing a signal fault's task if it started
/// one, and return the answer.
async fn inject(
    agent: &mut TestAgent,
    request: FaultRequest,
) -> Result<FaultSummary, crate::bun::BunError> {
    let (response, answer) = oneshot::channel();
    agent
        .handle_command(AgentCommand::InjectFault {
            reservation: None,
            replica_evidence: None,
            request,
            response,
        })
        .await;
    if let Some(done) = agent.follow_ups.join_next_with_id().await {
        agent.apply_follow_up(done).await;
    }
    answer.await.unwrap()
}

#[tokio::test]
async fn fault_targets_include_a_running_array_attempt() {
    let (agent, _tx, _shutdown) = test_agent();
    let group = SleepingTaskGroup::new();
    let _listing = agent.live_attempts().register(attempt(
        "render",
        3,
        AttemptHandle::TaskGroup(group.path()),
        true,
    ));
    let mut rule = fault_rule(FaultType::Kill { count: 0 });
    rule.target_service = "render".into();
    rule.namespace = Some("default".into());
    rule.workload = WorkloadKind::TaskArray;

    let targets = agent.fault_targets(&rule);

    assert_eq!(
        targets,
        vec![signal_faults::SignalTarget::TaskGroup(group.path())]
    );
}

#[tokio::test]
async fn kill_of_one_task_index_hits_only_that_attempt() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let mut zero = SleepingTaskGroup::new();
    let mut one = SleepingTaskGroup::new();
    let attempts = agent.live_attempts();
    let _zero = attempts.register(attempt(
        "render",
        0,
        AttemptHandle::TaskGroup(zero.path()),
        true,
    ));
    let _one = attempts.register(attempt(
        "render",
        1,
        AttemptHandle::TaskGroup(one.path()),
        true,
    ));

    // `--count 0` kills every attempt it matches. For an app with one
    // replica the replica rail would refuse that; a task is meant to end.
    let summary = inject(
        &mut agent,
        request(FaultType::Kill { count: 0 }, "render", Some("1")),
    )
    .await
    .expect("a job's attempts are not held to the replica rail");

    assert_eq!(summary.task_index, Some(1));
    assert!(one.ended().await.is_some(), "task 1 was not killed");
    assert!(zero.running(), "task 0 was killed too");
}

/// Runs each task's first attempt as a sleeping process it lists in the
/// agent's attempts, as the task runtime would, and lets later attempts
/// succeed at once.
struct ListingRunner {
    attempts: Arc<LiveAttempts>,
}

impl TaskRunner for ListingRunner {
    async fn run(
        &self,
        task: &TaskInvocation,
        _timeout: std::time::Duration,
        _cancel: &CancellationToken,
    ) -> Attempt {
        let done = |outcome| Attempt {
            outcome,
            output: CapturedOutput::default(),
            ran: None,
        };
        if task.attempt > 1 {
            return done(AttemptOutcome::Exited { code: 0 });
        }
        let mut group = SleepingTaskGroup::new();
        let run = task.run.clone().unwrap();
        let _listing = self.attempts.register(LiveAttempt {
            batch_id: run.batch_id,
            task_count: run.task_count,
            attempt: task.attempt,
            ..attempt(
                run.job_name.as_deref().unwrap(),
                task.index,
                AttemptHandle::TaskGroup(group.path()),
                false,
            )
        });
        use std::os::unix::process::ExitStatusExt;
        match group.ended().await.map(|status| status.signal()) {
            Some(Some(signal)) => done(AttemptOutcome::Signalled { signal }),
            _ => done(AttemptOutcome::Exited { code: 1 }),
        }
    }
}

#[tokio::test]
async fn killed_attempt_is_retried_per_policy() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let attempts = agent.live_attempts();
    let pool = Arc::new(TaskPool::new(
        Arc::new(ListingRunner {
            attempts: attempts.clone(),
        }),
        PoolConfig {
            concurrency: 1,
            backoff_base: std::time::Duration::from_millis(1),
            backoff_cap: std::time::Duration::from_millis(4),
        },
    ));
    let work = ChunkWork {
        template: None,
        batch_id: 42,
        spec: crate::meat::task_array::TaskArraySpec {
            max_attempts: 2,
            ..crate::meat::task_array::TaskArraySpec::with_count(1)
        },
        chunk: crate::meat::task_array::ChunkId(0),
        grant_attempt: 1,
        replay_unknown: true,
        program: "/bin/true".into(),
        args: Vec::new(),
        env: vec![("RELIABURGER_JOB_NAME".into(), "migrate".into())],
    };
    let running = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.run_chunk(&work, &CancellationToken::new()).await })
    };
    for _ in 0..500 {
        if !attempts.snapshot().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    inject(
        &mut agent,
        request(FaultType::Kill { count: 1 }, "migrate", None),
    )
    .await
    .expect("the job's attempt is a fault target");

    let outcome = running.await.unwrap();
    let record = &outcome.records[0];
    assert_eq!(record.outcome, TaskFinal::Succeeded, "{record:?}");
    assert_eq!(record.attempts, 2, "the killed attempt was not retried");
}

#[tokio::test]
async fn cpu_fault_on_a_shared_executor_task_is_refused() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let group = SleepingTaskGroup::new();
    let _listing = agent.live_attempts().register(attempt(
        "render",
        0,
        AttemptHandle::TaskGroup(group.path()),
        true,
    ));

    let error = inject(
        &mut agent,
        request(
            FaultType::CpuStress {
                percentage: 50,
                cores: None,
            },
            "render",
            Some("0"),
        ),
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(error.contains("reusable executor"), "{error}");
    assert!(error.contains("default/executor-abc-0"), "{error}");
    assert_eq!(agent.fault_registry.iter().count(), 0);
}

#[tokio::test]
async fn delay_from_a_shared_executor_task_is_refused() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let group = SleepingTaskGroup::new();
    let _listing = agent.live_attempts().register(attempt(
        "render",
        0,
        AttemptHandle::TaskGroup(group.path()),
        true,
    ));

    let error = inject(
        &mut agent,
        request(
            FaultType::Delay {
                delay_ns: 100_000_000,
                jitter_ns: 0,
                source_app: Some("render".into()),
            },
            "redis",
            None,
        ),
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(error.contains("reusable executor"), "{error}");
    assert_eq!(agent.fault_registry.iter().count(), 0);
}

#[tokio::test]
async fn array_attempt_as_caller_gets_the_delay() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let id = InstanceId("default/executor-abc-3".into());
    let _listing = agent.live_attempts().register(attempt(
        "render",
        3,
        AttemptHandle::Instance(id.clone()),
        false,
    ));
    let rule = agent.fault_registry.insert(&request(
        FaultType::Delay {
            delay_ns: 100_000_000,
            jitter_ns: 0,
            source_app: Some("render".into()),
        },
        "redis",
        None,
    ));

    let callers = agent.local_callers().await;

    let caller = callers
        .iter()
        .find(|caller| caller.instance_id == id.0)
        .expect("the attempt is a caller on this node");
    assert_eq!(caller.app, "render");
    assert!(crate::smoker::network::applies_to_caller(
        &rule,
        &caller.app,
        &caller.namespace
    ));
}
