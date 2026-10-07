//! The node-side executor for task arrays.
//!
//! A task array's tasks are short and numerous, so they skip everything
//! the ordinary job path does per job: no owner helper, no checkpoint
//! rewrite, no per-job log files. A [`TaskPool`] runs the tasks of the
//! chunks this node holds through a shared set of slots (a semaphore),
//! retries failures inside the pool, and hands back per-task
//! [`TaskRecord`]s plus a per-chunk [`ChunkResult`] for the leader.
//!
//! Running a task is behind the [`TaskRunner`] trait, with two
//! implementations: [`ProcessRunner`] spawns a real host process, and
//! [`FakeRunner`] computes an outcome from the index, for tests and
//! in-process benchmarks.
//!
//! Nothing here is wired into the agent yet; see the million-jobs plan.

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::meat::index_set::IndexRangeSet;
use crate::meat::task_array::{ChunkId, TaskArraySpec, expand_argv, task_env};
use crate::meat::task_array_state::ChunkResult;

/// Bytes of output kept from the start of a task's stdout and stderr
/// together, and again from the end.
pub const OUTPUT_KEEP_BYTES: usize = 2048;

/// How long a cancelled task gets between SIGTERM and SIGKILL.
pub const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// One attempt of one task, fully resolved: what to run and as whom.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskInvocation {
    /// Container template, when execution uses an owned runtime.
    pub template: Option<Box<crate::config::job::JobSpec>>,
    /// The task's index.
    pub index: u32,
    /// Attempt number, starting at 1.
    pub attempt: u8,
    /// Host binary to run.
    pub program: PathBuf,
    /// Arguments with `{index}` already expanded.
    pub args: Vec<String>,
    /// Environment: the template's plain variables plus the task identity.
    pub env: Vec<(String, String)>,
}

/// How one attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The process exited with this code; 0 is success.
    Exited { code: i32 },
    /// The process was killed by a signal it didn't choose.
    Signalled { signal: i32 },
    /// The attempt ran past the task timeout and was killed.
    TimedOut,
    /// The process couldn't be started. Retrying can't fix that.
    SpawnFailed { reason: String },
    /// Execution may have occurred, but its exit status could not be established.
    Unknown { reason: String },
    /// The array was cancelled while the attempt ran.
    Cancelled,
}

impl AttemptOutcome {
    /// Whether the attempt succeeded.
    pub fn succeeded(&self) -> bool {
        matches!(self, Self::Exited { code: 0 })
    }

    /// Whether another attempt could turn out differently.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Exited { .. } | Self::Signalled { .. } | Self::TimedOut | Self::Unknown { .. }
        ) && !self.succeeded()
    }
}

/// The first and last bytes a task wrote, interleaved from stdout and
/// stderr as they arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedOutput {
    /// Up to [`OUTPUT_KEEP_BYTES`] from the start.
    pub head: Vec<u8>,
    /// Up to [`OUTPUT_KEEP_BYTES`] from the end, when there was more than
    /// fit in `head`.
    pub tail: Vec<u8>,
    /// Total bytes written.
    pub total_bytes: u64,
}

impl CapturedOutput {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.total_bytes += bytes.len() as u64;
        let room = OUTPUT_KEEP_BYTES.saturating_sub(self.head.len());
        let (into_head, rest) = bytes.split_at(room.min(bytes.len()));
        self.head.extend_from_slice(into_head);
        self.tail.extend_from_slice(rest);
        if self.tail.len() > OUTPUT_KEEP_BYTES {
            let excess = self.tail.len() - OUTPUT_KEEP_BYTES;
            self.tail.drain(..excess);
        }
    }
}

/// The result of one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// How it ended.
    pub outcome: AttemptOutcome,
    /// What it wrote.
    pub output: CapturedOutput,
}

/// Runs one attempt of one task.
///
/// A trait because there are two real implementations: the process
/// runner the node uses, and the fake the tests and benchmarks use.
pub trait TaskRunner: Send + Sync + 'static {
    /// Run `task`, killing it after a nonzero `timeout` or when `cancel` fires.
    fn run(
        &self,
        task: &TaskInvocation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Attempt> + Send;
}

/// A zero duration preserves jobs with no implicit wall-clock deadline.
pub(crate) async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

/// Spawns each attempt as a host process, directly: no shell, no owner
/// helper, stdin closed, output captured to a bounded head and tail.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    /// Time between SIGTERM and SIGKILL on cancel.
    pub cancel_grace: Duration,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            cancel_grace: CANCEL_GRACE,
        }
    }
}

impl TaskRunner for ProcessRunner {
    async fn run(
        &self,
        task: &TaskInvocation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Attempt {
        let mut command = tokio::process::Command::new(&task.program);
        command
            .args(&task.args)
            .envs(task.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Attempt {
                    outcome: AttemptOutcome::SpawnFailed {
                        reason: error.to_string(),
                    },
                    output: CapturedOutput::default(),
                };
            }
        };
        let output = Arc::new(tokio::sync::Mutex::new(CapturedOutput::default()));
        let mut readers = JoinSet::new();
        if let Some(stdout) = child.stdout.take() {
            readers.spawn(capture(stdout, Arc::clone(&output)));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.spawn(capture(stderr, Arc::clone(&output)));
        }

        let outcome = tokio::select! {
            status = child.wait() => match status {
                Ok(status) => outcome_of(status),
                Err(error) => AttemptOutcome::SpawnFailed { reason: error.to_string() },
            },
            () = wait_deadline((!timeout.is_zero()).then(|| tokio::time::Instant::now() + timeout)) => {
                kill_now(&mut child).await;
                AttemptOutcome::TimedOut
            }
            () = cancel.cancelled() => {
                terminate(&mut child, self.cancel_grace).await;
                AttemptOutcome::Cancelled
            }
        };
        // The pipes close when the process exits; a grandchild holding
        // them open mustn't hold the slot, so the drain is bounded too.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while readers.join_next().await.is_some() {}
        })
        .await;
        readers.abort_all();
        let output = output.lock().await.clone();
        Attempt { outcome, output }
    }
}

async fn capture(
    mut stream: impl AsyncRead + Unpin,
    output: Arc<tokio::sync::Mutex<CapturedOutput>>,
) {
    let mut buffer = [0u8; 4096];
    while let Ok(read) = stream.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        output.lock().await.push(&buffer[..read]);
    }
}

#[cfg(unix)]
fn outcome_of(status: std::process::ExitStatus) -> AttemptOutcome {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => AttemptOutcome::Exited { code },
        (None, Some(signal)) => AttemptOutcome::Signalled { signal },
        (None, None) => AttemptOutcome::Exited { code: -1 },
    }
}

#[cfg(not(unix))]
fn outcome_of(status: std::process::ExitStatus) -> AttemptOutcome {
    AttemptOutcome::Exited {
        code: status.code().unwrap_or(-1),
    }
}

async fn kill_now(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

/// SIGTERM, then SIGKILL after `grace` if the process is still there.
async fn terminate(child: &mut tokio::process::Child, grace: Duration) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
        if tokio::time::timeout(grace, child.wait()).await.is_ok() {
            return;
        }
    }
    kill_now(child).await;
}

/// A runner that doesn't start processes: each attempt's outcome comes
/// from a function of the invocation, after an optional fixed delay. It
/// also records how many attempts ran at once, which is how the pool's
/// concurrency limit is tested. For tests and benchmarks.
pub struct FakeRunner {
    outcome: Box<dyn Fn(&TaskInvocation) -> AttemptOutcome + Send + Sync>,
    delay: Duration,
    running: AtomicU32,
    peak: AtomicU32,
    attempts: AtomicU64,
}

impl FakeRunner {
    /// A fake whose attempts end as `outcome` says, each taking `delay`.
    pub fn new(
        delay: Duration,
        outcome: impl Fn(&TaskInvocation) -> AttemptOutcome + Send + Sync + 'static,
    ) -> Self {
        Self {
            outcome: Box::new(outcome),
            delay,
            running: AtomicU32::new(0),
            peak: AtomicU32::new(0),
            attempts: AtomicU64::new(0),
        }
    }

    /// A fake where every attempt succeeds at once.
    pub fn always_succeeds() -> Self {
        Self::new(Duration::ZERO, |_| AttemptOutcome::Exited { code: 0 })
    }

    /// Most attempts that were running at the same moment.
    pub fn peak_concurrency(&self) -> u32 {
        self.peak.load(Ordering::SeqCst)
    }

    /// Attempts run so far.
    pub fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl TaskRunner for FakeRunner {
    async fn run(
        &self,
        task: &TaskInvocation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Attempt {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        let outcome = if self.delay.is_zero() {
            // Yield once so concurrent attempts really overlap.
            tokio::task::yield_now().await;
            (self.outcome)(task)
        } else {
            tokio::select! {
                () = tokio::time::sleep(if timeout.is_zero() { self.delay } else { self.delay.min(timeout) }) => {
                    if !timeout.is_zero() && self.delay > timeout {
                        AttemptOutcome::TimedOut
                    } else {
                        (self.outcome)(task)
                    }
                }
                () = cancel.cancelled() => AttemptOutcome::Cancelled,
            }
        };
        self.running.fetch_sub(1, Ordering::SeqCst);
        Attempt {
            outcome,
            output: CapturedOutput::default(),
        }
    }
}

/// What the pool needs to run one chunk.
#[derive(Debug, Clone)]
pub struct ChunkWork {
    /// Container template carried with the committed assignment.
    pub template: Option<Box<crate::config::job::JobSpec>>,
    /// The array's batch id (goes into each task's environment).
    pub batch_id: u64,
    /// Count and policy.
    pub spec: TaskArraySpec,
    /// Which chunk.
    pub chunk: ChunkId,
    /// The leader's grant attempt, echoed back in the result.
    pub grant_attempt: u64,
    /// Explicit permission to repeat an attempt whose outcome is ambiguous.
    pub replay_unknown: bool,
    /// Host binary to run.
    pub program: PathBuf,
    /// Argument template, with `{index}` placeholders.
    pub args: Vec<String>,
    /// The template's plain environment variables.
    pub env: Vec<(String, String)>,
}

/// How a task ended after all its attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFinal {
    /// An attempt exited 0.
    Succeeded,
    /// Every allowed attempt failed, or the binary couldn't be started.
    Failed,
    /// The array was cancelled before the task finished.
    NotRun,
}

/// The durable outcome of one task, as the ledger stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// Durable leader grant generation; distinct from per-task retry count.
    pub grant_attempt: u64,
    /// The task's index.
    pub index: u32,
    /// Attempts made (0 if the task never started).
    pub attempts: u8,
    /// How it ended.
    pub outcome: TaskFinal,
    /// The last attempt's exit code, or the negated signal; `None` when
    /// there was no exit status (not run, timed out, spawn failure).
    pub exit_code: Option<i32>,
    /// Wall time of the last attempt, in milliseconds.
    pub run_ms: u32,
    /// The last failed attempt's output; kept only for failures.
    pub output: Option<CapturedOutput>,
}

/// What running one chunk produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkOutcome {
    /// The report for the leader.
    pub result: ChunkResult,
    /// One record per task, in index order, for the ledger.
    pub records: Vec<TaskRecord>,
}

/// Pool settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolConfig {
    /// Most attempts running at once on this node.
    pub concurrency: u32,
    /// First retry delay; doubles per attempt.
    pub backoff_base: Duration,
    /// Longest retry delay.
    pub backoff_cap: Duration,
}

impl PoolConfig {
    /// The node defaults: 100 ms doubling up to 5 s.
    pub fn with_concurrency(concurrency: u32) -> Self {
        Self {
            concurrency: concurrency.max(1),
            backoff_base: Duration::from_millis(100),
            backoff_cap: Duration::from_secs(5),
        }
    }

    /// Delay before attempt `next_attempt` (2 or later) of task `index`.
    /// Doubling from the base, capped, plus up to about 44% extra spread
    /// by index, so tasks that failed together don't retry together.
    /// Deterministic, so tests can predict it.
    pub fn retry_delay(&self, index: u32, next_attempt: u8) -> Duration {
        let doublings = u32::from(next_attempt.saturating_sub(2)).min(16);
        let delay = self
            .backoff_base
            .saturating_mul(1 << doublings)
            .min(self.backoff_cap);
        delay + delay * (index % 8) / 16
    }
}

/// Live counters for a pool, readable while it runs.
#[derive(Debug, Default)]
pub struct PoolCounters {
    /// Attempts started.
    pub attempts_started: AtomicU64,
    /// Attempts running now.
    pub running: AtomicU64,
    /// Tasks that succeeded.
    pub succeeded: AtomicU64,
    /// Tasks that failed for good.
    pub failed: AtomicU64,
    /// Retries (attempts after the first).
    pub retried: AtomicU64,
    /// An ambiguous attempt is waiting for an operator decision.
    pub unknown: std::sync::atomic::AtomicBool,
}

/// Runs the tasks of any number of chunks through one shared set of
/// slots. Several chunks can run at once; the slots, not the chunks,
/// bound the work, so the tail of one chunk overlaps the head of the next.
pub struct TaskPool<R: TaskRunner> {
    runner: Arc<R>,
    config: PoolConfig,
    slots: Arc<Semaphore>,
    node_slots: Arc<Semaphore>,
    counters: Arc<PoolCounters>,
    budget: Option<Arc<super::execution_budget::ExecutionBudget>>,
    resources: crate::meat::Resources,
}

impl<R: TaskRunner> TaskPool<R> {
    /// A pool running attempts through `runner`.
    pub fn new(runner: Arc<R>, config: PoolConfig) -> Self {
        Self::with_node_slots(
            runner,
            config,
            Arc::new(Semaphore::new(config.concurrency.max(1) as usize)),
        )
    }

    /// An array pool sharing the node's admission limit with other arrays.
    pub fn with_node_slots(runner: Arc<R>, config: PoolConfig, node_slots: Arc<Semaphore>) -> Self {
        let permits = config.concurrency.max(1) as usize;
        Self {
            runner,
            config,
            slots: Arc::new(Semaphore::new(permits)),
            node_slots,
            counters: Arc::new(PoolCounters::default()),
            budget: None,
            resources: crate::meat::Resources::default(),
        }
    }

    /// Account each running attempt against the shared application/task budget.
    pub fn with_budget(
        mut self,
        budget: Arc<super::execution_budget::ExecutionBudget>,
        resources: crate::meat::Resources,
    ) -> Self {
        self.budget = Some(budget);
        self.resources = resources;
        self
    }

    /// The live counters.
    pub fn counters(&self) -> &Arc<PoolCounters> {
        &self.counters
    }

    /// Run every task in the chunk and account for each one exactly once.
    /// On cancel, tasks not yet started count as not run and running ones
    /// are stopped.
    pub async fn run_chunk(&self, work: &ChunkWork, cancel: &CancellationToken) -> ChunkOutcome {
        self.resume_chunk(work, Vec::new(), cancel).await
    }

    /// Finish a chunk some of whose tasks already ended in an earlier run
    /// (read back from the ledger after a restart). Only the other tasks
    /// run. The result counts every task in the chunk; `records` holds
    /// only the new ones, since the earlier ones are already durable.
    /// `finished` should hold terminal records (succeeded or failed) only.
    pub async fn resume_chunk(
        &self,
        work: &ChunkWork,
        finished: Vec<TaskRecord>,
        cancel: &CancellationToken,
    ) -> ChunkOutcome {
        self.resume_chunk_streaming(work, finished, cancel, None)
            .await
    }

    /// Send each outcome as it finishes, allowing group commit during execution.
    /// The bounded channel applies backpressure if the disk cannot keep up.
    pub async fn resume_chunk_streaming(
        &self,
        work: &ChunkWork,
        finished: Vec<TaskRecord>,
        cancel: &CancellationToken,
        outcomes: Option<tokio::sync::mpsc::Sender<TaskRecord>>,
    ) -> ChunkOutcome {
        let pending = Arc::new(Semaphore::new(1024));
        let mut done = IndexRangeSet::new();
        for record in &finished {
            done.insert(record.index);
        }
        // A chunk outside the array has no tasks, and so an empty result.
        let indices: Vec<u32> = work
            .spec
            .chunk_range(work.chunk)
            .into_iter()
            .flatten()
            .filter(|index| !done.contains(*index))
            .collect();
        let work = Arc::new(work.clone());
        let mut running = JoinSet::new();
        let mut records = Vec::with_capacity(indices.len());
        for index in indices {
            let permit = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                permit = Arc::clone(&self.slots).acquire_owned() => permit.ok(),
            };
            let Some(permit) = permit else {
                records.push(TaskRecord {
                    grant_attempt: work.grant_attempt,
                    ..not_run(index)
                });
                continue;
            };
            let pending_permit = Arc::clone(&pending)
                .acquire_owned()
                .await
                .expect("pending semaphore stays open");
            let task = TaskAttempts {
                runner: Arc::clone(&self.runner),
                config: self.config,
                slots: Arc::clone(&self.slots),
                node_slots: Arc::clone(&self.node_slots),
                counters: Arc::clone(&self.counters),
                budget: self.budget.clone(),
                resources: self.resources,
                work: Arc::clone(&work),
                cancel: cancel.clone(),
            };
            let outcomes = outcomes.clone();
            let task_grant = work.grant_attempt;
            running.spawn(async move {
                let mut record = task.run(index, permit).await;
                record.grant_attempt = task_grant;
                if let Some(sender) = outcomes {
                    // The writer owns capture from here. Keeping a second copy
                    // until a large chunk ends would multiply failure memory.
                    let output = record.output.take();
                    let _ = sender
                        .send(TaskRecord {
                            output,
                            ..record.clone()
                        })
                        .await;
                }
                drop(pending_permit);
                record
            });
            while let Some(done) = running.try_join_next() {
                records.extend(done.ok());
            }
        }
        // A task only fails to join if it panicked; its index then shows
        // up as missing, and the leader refuses the miscounted chunk
        // rather than retiring it.
        while let Some(done) = running.join_next().await {
            records.extend(done.ok());
        }
        records.sort_by_key(|record| record.index);
        let in_chunk = |record: &&TaskRecord| work.spec.chunk_of(record.index) == Some(work.chunk);
        ChunkOutcome {
            result: summarise(&work, finished.iter().filter(in_chunk).chain(&records)),
            records,
        }
    }
}

fn not_run(index: u32) -> TaskRecord {
    TaskRecord {
        grant_attempt: 1,
        index,
        attempts: 0,
        outcome: TaskFinal::NotRun,
        exit_code: None,
        run_ms: 0,
        output: None,
    }
}

fn summarise<'a>(work: &ChunkWork, records: impl Iterator<Item = &'a TaskRecord>) -> ChunkResult {
    let mut result = ChunkResult {
        duration_counts: [0; 16],
        chunk: work.chunk,
        attempt: work.grant_attempt,
        succeeded: 0,
        failed_count: 0,
        failed_indices: IndexRangeSet::new(),
        not_run: 0,
        retried: 0,
    };
    for record in records {
        match record.outcome {
            TaskFinal::Succeeded => result.succeeded += 1,
            TaskFinal::Failed => {
                result.failed_count += 1;
                if result.failed_indices.range_count()
                    < crate::meat::task_array_state::MAX_CHUNK_FAILED_RANGES
                {
                    result.failed_indices.insert(record.index);
                }
            }
            TaskFinal::NotRun => result.not_run += 1,
        }
        if record.outcome != TaskFinal::NotRun {
            let bucket =
                (32 - record.run_ms.max(1).saturating_sub(1).leading_zeros()).min(15) as usize;
            result.duration_counts[bucket] += 1;
        }
        result.retried += u32::from(record.attempts.saturating_sub(1));
    }
    result
}

/// One task's attempts, owned so it can run as its own tokio task.
struct TaskAttempts<R: TaskRunner> {
    runner: Arc<R>,
    config: PoolConfig,
    slots: Arc<Semaphore>,
    node_slots: Arc<Semaphore>,
    counters: Arc<PoolCounters>,
    budget: Option<Arc<super::execution_budget::ExecutionBudget>>,
    resources: crate::meat::Resources,
    work: Arc<ChunkWork>,
    cancel: CancellationToken,
}

impl<R: TaskRunner> TaskAttempts<R> {
    async fn run(self, index: u32, first_permit: tokio::sync::OwnedSemaphorePermit) -> TaskRecord {
        let spec = &self.work.spec;
        let timeout = Duration::from_secs(u64::from(spec.task_timeout_secs));
        let mut permit = Some(first_permit);
        let mut attempt = 0u8;
        loop {
            attempt += 1;
            if permit.is_none() {
                let delay = self.config.retry_delay(index, attempt);
                permit = tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => None,
                    permit = async {
                        tokio::time::sleep(delay).await;
                        Arc::clone(&self.slots).acquire_owned().await.ok()
                    } => permit,
                };
                if permit.is_none() {
                    return TaskRecord {
                        attempts: attempt - 1,
                        ..not_run(index)
                    };
                }
                self.counters.retried.fetch_add(1, Ordering::Relaxed);
            }
            let node_permit = tokio::select! {
                biased;
                () = self.cancel.cancelled() => None,
                permit = Arc::clone(&self.node_slots).acquire_owned() => permit.ok(),
            };
            let Some(node_permit) = node_permit else {
                return TaskRecord {
                    attempts: attempt - 1,
                    ..not_run(index)
                };
            };
            let mut resource_lease = match &self.budget {
                Some(budget) => match budget.acquire(self.resources, &self.cancel).await {
                    Some(lease) => Some(lease.quarantine_on_drop()),
                    None => {
                        return TaskRecord {
                            attempts: attempt - 1,
                            ..not_run(index)
                        };
                    }
                },
                None => None,
            };
            let invocation = self.invocation(index, attempt);
            self.counters
                .attempts_started
                .fetch_add(1, Ordering::Relaxed);
            self.counters.running.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let result = self.runner.run(&invocation, timeout, &self.cancel).await;
            let run_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            self.counters.running.fetch_sub(1, Ordering::Relaxed);
            if let Some(lease) = resource_lease.as_mut() {
                lease.confirm_retired();
            }
            drop(permit.take());
            drop(node_permit);
            drop(resource_lease);

            if !self.work.replay_unknown
                && matches!(
                    result.outcome,
                    AttemptOutcome::TimedOut | AttemptOutcome::Unknown { .. }
                )
            {
                self.counters.unknown.store(true, Ordering::Relaxed);
                self.cancel.cancelled().await;
                return TaskRecord {
                    attempts: attempt,
                    ..not_run(index)
                };
            }
            let exit_code = match result.outcome {
                AttemptOutcome::Exited { code } => Some(code),
                AttemptOutcome::Signalled { signal } => Some(-signal),
                _ => None,
            };
            let record = TaskRecord {
                grant_attempt: 1,
                index,
                attempts: attempt,
                outcome: TaskFinal::Failed,
                exit_code,
                run_ms,
                output: None,
            };
            if result.outcome.succeeded() {
                self.counters.succeeded.fetch_add(1, Ordering::Relaxed);
                return TaskRecord {
                    outcome: TaskFinal::Succeeded,
                    output: (self.work.spec.count == 1).then_some(result.output),
                    ..record
                };
            }
            if result.outcome == AttemptOutcome::Cancelled {
                return TaskRecord {
                    outcome: TaskFinal::NotRun,
                    ..record
                };
            }
            if !result.outcome.is_retryable() || attempt >= spec.max_attempts {
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
                return TaskRecord {
                    output: Some(result.output),
                    ..record
                };
            }
        }
    }

    fn invocation(&self, index: u32, attempt: u8) -> TaskInvocation {
        let work = &self.work;
        let mut env = work.env.clone();
        env.extend(
            task_env(work.batch_id, work.spec.count, index, attempt)
                .into_iter()
                .map(|(key, value)| (key.to_string(), value)),
        );
        TaskInvocation {
            template: work.template.clone(),
            index,
            attempt,
            program: work.program.clone(),
            args: expand_argv(&work.args, index),
            env,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(count: u32, chunk_size: u32, chunk: u32) -> ChunkWork {
        ChunkWork {
            template: None,
            batch_id: 7,
            spec: TaskArraySpec {
                chunk_size,
                ..TaskArraySpec::with_count(count)
            },
            chunk: ChunkId(chunk),
            grant_attempt: 1,
            replay_unknown: true,
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), "exit 0".to_string()],
            env: Vec::new(),
        }
    }

    fn fast(concurrency: u32) -> PoolConfig {
        PoolConfig {
            concurrency,
            backoff_base: Duration::from_millis(1),
            backoff_cap: Duration::from_millis(4),
        }
    }

    fn invocation(program: &str, args: &[&str]) -> TaskInvocation {
        TaskInvocation {
            template: None,
            index: 3,
            attempt: 1,
            program: PathBuf::from(program),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: vec![("GREETING".to_string(), "hello".to_string())],
        }
    }

    // -- M3.1: runners ---------------------------------------------------

    #[tokio::test]
    async fn an_ambiguous_singleton_attempt_waits_for_operator_replay() {
        let runner = FakeRunner::new(Duration::ZERO, |_| AttemptOutcome::TimedOut);
        let pool = Arc::new(TaskPool::new(Arc::new(runner), fast(1)));
        let mut work = work(1, 1, 0);
        work.replay_unknown = false;
        let cancel = CancellationToken::new();
        let running = {
            let pool = pool.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { pool.run_chunk(&work, &cancel).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !pool.counters().unknown.load(Ordering::Relaxed) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(pool.counters().attempts_started.load(Ordering::Relaxed), 1);
        assert!(!running.is_finished());
        cancel.cancel();
        running.await.unwrap();
    }

    #[tokio::test]
    async fn process_runner_reports_the_exit_code() {
        let runner = ProcessRunner::default();
        let cancel = CancellationToken::new();
        let ok = runner
            .run(
                &invocation("/bin/sh", &["-c", "exit 0"]),
                Duration::from_secs(10),
                &cancel,
            )
            .await;
        assert_eq!(ok.outcome, AttemptOutcome::Exited { code: 0 });
        let failed = runner
            .run(
                &invocation("/bin/sh", &["-c", "exit 3"]),
                Duration::from_secs(10),
                &cancel,
            )
            .await;
        assert_eq!(failed.outcome, AttemptOutcome::Exited { code: 3 });
    }

    #[tokio::test]
    async fn process_runner_passes_arguments_and_environment_and_captures_output() {
        let runner = ProcessRunner::default();
        let attempt = runner
            .run(
                &invocation(
                    "/bin/sh",
                    &["-c", "echo \"$GREETING $0\"; echo oops >&2", "task-3"],
                ),
                Duration::from_secs(10),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(attempt.outcome, AttemptOutcome::Exited { code: 0 });
        let text = String::from_utf8_lossy(&attempt.output.head).to_string();
        assert!(text.contains("hello task-3"), "{text:?}");
        assert!(text.contains("oops"), "{text:?}");
    }

    #[tokio::test]
    async fn zero_timeout_preserves_unbounded_job_execution_and_cancellation() {
        let runner = ProcessRunner::default();
        let outcome = runner
            .run(
                &invocation("/bin/sh", &["-c", "sleep 0.05; exit 0"]),
                Duration::ZERO,
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(outcome.outcome, AttemptOutcome::Exited { code: 0 });
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            runner.run(
                &invocation("/bin/sh", &["-c", "exec sleep 30"]),
                Duration::ZERO,
                &cancel,
            ),
        )
        .await
        .unwrap();
        assert_eq!(outcome.outcome, AttemptOutcome::Cancelled);
    }

    #[tokio::test]
    async fn process_runner_kills_an_attempt_past_its_timeout() {
        let runner = ProcessRunner::default();
        let started = Instant::now();
        let attempt = runner
            .run(
                &invocation("/bin/sh", &["-c", "exec sleep 30"]),
                Duration::from_millis(200),
                &CancellationToken::new(),
            )
            .await;
        assert_eq!(attempt.outcome, AttemptOutcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn process_runner_terminates_on_cancel() {
        let runner = ProcessRunner {
            cancel_grace: Duration::from_secs(5),
        };
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let attempt = runner
            .run(
                &invocation("/bin/sh", &["-c", "exec sleep 30"]),
                Duration::from_secs(60),
                &cancel,
            )
            .await;
        assert_eq!(attempt.outcome, AttemptOutcome::Cancelled);
        // SIGTERM ends `sleep` at once; no need to wait for the grace.
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn process_runner_reports_a_missing_binary_as_a_spawn_failure() {
        let runner = ProcessRunner::default();
        let attempt = runner
            .run(
                &invocation("/nonexistent/rb-task", &[]),
                Duration::from_secs(10),
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(
            attempt.outcome,
            AttemptOutcome::SpawnFailed { .. }
        ));
        assert!(!attempt.outcome.is_retryable());
    }

    #[tokio::test]
    async fn streaming_failures_transfer_output_without_retaining_it_for_the_whole_chunk() {
        struct LoudRunner;
        impl TaskRunner for LoudRunner {
            async fn run(&self, _: &TaskInvocation, _: Duration, _: &CancellationToken) -> Attempt {
                Attempt {
                    outcome: AttemptOutcome::Exited { code: 1 },
                    output: CapturedOutput {
                        head: vec![b'x'; OUTPUT_KEEP_BYTES],
                        tail: vec![b'y'; OUTPUT_KEEP_BYTES],
                        total_bytes: (OUTPUT_KEEP_BYTES * 2) as u64,
                    },
                }
            }
        }
        let pool = TaskPool::new(Arc::new(LoudRunner), fast(4));
        let mut work = work(64, 64, 0);
        work.spec.max_attempts = 1;
        let cancel = CancellationToken::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let consume = async {
            let mut count = 0;
            while let Some(record) = rx.recv().await {
                let record: TaskRecord = record;
                assert_eq!(record.output.unwrap().head.len(), OUTPUT_KEEP_BYTES);
                count += 1;
            }
            count
        };
        let (outcome, count) = tokio::join!(
            pool.resume_chunk_streaming(&work, vec![], &cancel, Some(tx)),
            consume
        );
        assert_eq!(count, 64);
        assert_eq!(outcome.result.failed_count, 64);
        assert!(outcome.records.iter().all(|record| record.output.is_none()));
    }

    #[tokio::test]
    async fn singleton_success_preserves_selected_output() {
        struct Writes;
        impl TaskRunner for Writes {
            async fn run(&self, _: &TaskInvocation, _: Duration, _: &CancellationToken) -> Attempt {
                Attempt {
                    outcome: AttemptOutcome::Exited { code: 0 },
                    output: CapturedOutput {
                        head: b"result".to_vec(),
                        tail: vec![],
                        total_bytes: 6,
                    },
                }
            }
        }
        let pool = TaskPool::new(Arc::new(Writes), fast(1));
        let result = pool
            .run_chunk(&work(1, 1, 0), &CancellationToken::new())
            .await;
        assert_eq!(result.records[0].output.as_ref().unwrap().head, b"result");
    }

    #[test]
    fn sparse_failures_keep_exact_counts_with_a_bounded_control_preview() {
        let work = ChunkWork {
            batch_id: 1,
            template: None,
            spec: TaskArraySpec {
                chunk_size: 65_536,
                ..TaskArraySpec::with_count(65_536)
            },
            chunk: ChunkId(0),
            grant_attempt: 1,
            replay_unknown: true,
            program: "/unused".into(),
            args: vec![],
            env: vec![],
        };
        let records: Vec<_> = (0..65_536)
            .map(|index| TaskRecord {
                grant_attempt: 1,
                index,
                attempts: 1,
                outcome: if index % 2 == 0 {
                    TaskFinal::Failed
                } else {
                    TaskFinal::Succeeded
                },
                exit_code: Some((index % 2 == 0) as i32),
                run_ms: 1,
                output: None,
            })
            .collect();
        let result = summarise(&work, records.iter());
        assert_eq!(result.failed_count, 32_768);
        assert_eq!(result.failed_indices.range_count(), 256);
        assert!(serde_json::to_vec(&result).unwrap().len() < 4096);
        let mut state = crate::meat::task_array_state::TaskArrayState::new(work.spec, 0).unwrap();
        let node = crate::meat::NodeId("worker".into());
        state
            .grant(&node, &IndexRangeSet::from_range(0..=0))
            .unwrap();
        state.complete(&node, &result).unwrap();
        assert_eq!(state.summary().failed, 32_768);
        assert_eq!(state.failed_overflow, 32_512);
    }

    #[test]
    fn captured_output_keeps_a_bounded_head_and_tail() {
        let mut output = CapturedOutput::default();
        let chunk = vec![b'a'; 1000];
        for _ in 0..10 {
            output.push(&chunk);
        }
        output.push(b"THE END");
        assert_eq!(output.total_bytes, 10_007);
        assert_eq!(output.head.len(), OUTPUT_KEEP_BYTES);
        assert_eq!(output.tail.len(), OUTPUT_KEEP_BYTES);
        assert!(output.tail.ends_with(b"THE END"));

        let mut short = CapturedOutput::default();
        short.push(b"hi");
        assert_eq!(short.head, b"hi");
        assert!(short.tail.is_empty());
    }

    #[tokio::test]
    async fn fake_runner_outcome_comes_from_the_invocation() {
        let runner = FakeRunner::new(Duration::ZERO, |task| AttemptOutcome::Exited {
            code: i32::from(task.index % 2 == 1),
        });
        let cancel = CancellationToken::new();
        let mut task = invocation("/unused", &[]);
        task.index = 4;
        assert!(
            runner
                .run(&task, Duration::from_secs(1), &cancel)
                .await
                .outcome
                .succeeded()
        );
        task.index = 5;
        assert!(
            !runner
                .run(&task, Duration::from_secs(1), &cancel)
                .await
                .outcome
                .succeeded()
        );
        assert_eq!(runner.attempts(), 2);
    }

    // -- M3.2: the pool --------------------------------------------------

    #[test]
    fn retry_delay_doubles_up_to_the_cap_with_spread_by_index() {
        let config = PoolConfig::with_concurrency(4);
        assert_eq!(config.retry_delay(0, 2), Duration::from_millis(100));
        assert_eq!(config.retry_delay(0, 3), Duration::from_millis(200));
        assert_eq!(config.retry_delay(0, 10), Duration::from_secs(5));
        assert_eq!(config.retry_delay(4, 2), Duration::from_millis(125));
        assert!(config.retry_delay(7, 10) <= Duration::from_millis(7200));
    }

    #[tokio::test]
    async fn every_task_in_the_chunk_runs_once_and_is_accounted_for() {
        let runner = Arc::new(FakeRunner::always_succeeds());
        let pool = TaskPool::new(Arc::clone(&runner), fast(8));
        let outcome = pool
            .run_chunk(&work(2500, 1000, 2), &CancellationToken::new())
            .await;
        assert_eq!(outcome.result.chunk, ChunkId(2));
        assert_eq!(outcome.result.succeeded, 500);
        assert!(outcome.result.failed_indices.is_empty());
        assert_eq!(outcome.records.len(), 500);
        let indices: Vec<u32> = outcome.records.iter().map(|r| r.index).collect();
        assert_eq!(indices, (2000..2500).collect::<Vec<_>>());
        assert_eq!(runner.attempts(), 500);
    }

    #[tokio::test]
    async fn resuming_a_chunk_runs_only_the_unfinished_tasks() {
        let runner = Arc::new(FakeRunner::always_succeeds());
        let pool = TaskPool::new(Arc::clone(&runner), fast(4));
        let earlier = |index, outcome| TaskRecord {
            grant_attempt: 1,
            index,
            attempts: 1,
            outcome,
            exit_code: Some(i32::from(outcome == TaskFinal::Failed)),
            run_ms: 1,
            output: None,
        };
        let finished = vec![
            earlier(100, TaskFinal::Succeeded),
            earlier(101, TaskFinal::Failed),
            earlier(150, TaskFinal::Succeeded),
            // Outside the chunk: ignored rather than miscounted.
            earlier(5, TaskFinal::Succeeded),
        ];
        let outcome = pool
            .resume_chunk(&work(200, 100, 1), finished, &CancellationToken::new())
            .await;
        assert_eq!(runner.attempts(), 97);
        assert_eq!(
            outcome.records.len(),
            97,
            "only new records go to the ledger"
        );
        assert!(
            outcome
                .records
                .iter()
                .all(|r| ![100, 101, 150].contains(&r.index))
        );
        assert_eq!(outcome.result.succeeded, 99);
        assert_eq!(
            outcome.result.failed_indices,
            IndexRangeSet::from_range(101..=101)
        );
    }

    #[tokio::test]
    async fn no_more_than_concurrency_attempts_run_at_once() {
        let runner = Arc::new(FakeRunner::new(Duration::from_millis(2), |_| {
            AttemptOutcome::Exited { code: 0 }
        }));
        let pool = TaskPool::new(Arc::clone(&runner), fast(4));
        let cancel = CancellationToken::new();
        let (first, second) = (work(200, 100, 0), work(200, 100, 1));
        // Two chunks at once share the same four slots.
        let (a, b) = tokio::join!(
            pool.run_chunk(&first, &cancel),
            pool.run_chunk(&second, &cancel),
        );
        assert_eq!(a.result.succeeded + b.result.succeeded, 200);
        assert!(
            runner.peak_concurrency() <= 4,
            "peak {}",
            runner.peak_concurrency()
        );
        assert!(runner.peak_concurrency() >= 2, "slots were never shared");
    }

    #[tokio::test]
    async fn failures_are_retried_up_to_max_attempts() {
        // Index % 10 == 1 fails once then succeeds; index % 10 == 2 always fails.
        let runner = Arc::new(FakeRunner::new(Duration::ZERO, |task| {
            let fails = match task.index % 10 {
                1 => task.attempt == 1,
                2 => true,
                _ => false,
            };
            AttemptOutcome::Exited {
                code: i32::from(fails),
            }
        }));
        let pool = TaskPool::new(Arc::clone(&runner), fast(4));
        let outcome = pool
            .run_chunk(&work(100, 100, 0), &CancellationToken::new())
            .await;
        let result = &outcome.result;
        assert_eq!(result.succeeded, 90);
        assert_eq!(result.failed_indices.len(), 10);
        assert!(result.failed_indices.contains(42));
        // 10 tasks retried once, 10 tasks retried twice (3 attempts).
        assert_eq!(result.retried, 10 + 20);
        assert_eq!(runner.attempts(), 80 + 10 * 2 + 10 * 3);
        let failed = outcome.records.iter().find(|r| r.index == 42).unwrap();
        assert_eq!(failed.attempts, 3);
        assert_eq!(failed.exit_code, Some(1));
        assert!(failed.output.is_some());
        let counters = pool.counters();
        assert_eq!(counters.succeeded.load(Ordering::Relaxed), 90);
        assert_eq!(counters.failed.load(Ordering::Relaxed), 10);
        assert_eq!(counters.retried.load(Ordering::Relaxed), 30);
    }

    #[tokio::test]
    async fn spawn_failures_are_not_retried() {
        let runner = Arc::new(FakeRunner::new(Duration::ZERO, |_| {
            AttemptOutcome::SpawnFailed {
                reason: "no such file".to_string(),
            }
        }));
        let pool = TaskPool::new(Arc::clone(&runner), fast(4));
        let outcome = pool
            .run_chunk(&work(10, 10, 0), &CancellationToken::new())
            .await;
        assert_eq!(outcome.result.failed_indices.len(), 10);
        assert_eq!(outcome.result.retried, 0);
        assert_eq!(runner.attempts(), 10);
    }

    #[tokio::test]
    async fn cancelling_stops_admission_and_accounts_for_every_task() {
        let runner = Arc::new(FakeRunner::new(Duration::from_millis(20), |_| {
            AttemptOutcome::Exited { code: 0 }
        }));
        let pool = TaskPool::new(Arc::clone(&runner), fast(2));
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let outcome = pool.run_chunk(&work(1000, 1000, 0), &cancel).await;
        let result = &outcome.result;
        assert!(result.not_run > 900, "not run: {}", result.not_run);
        assert_eq!(
            u64::from(result.succeeded) + result.failed_indices.len() + u64::from(result.not_run),
            1000
        );
        assert_eq!(outcome.records.len(), 1000);
    }

    #[tokio::test]
    async fn tasks_see_their_expanded_arguments_and_identity() {
        let runner = Arc::new(FakeRunner::new(Duration::ZERO, |task| {
            let index = task.index.to_string();
            let env_ok = task
                .env
                .contains(&("RELIABURGER_TASK_INDEX".to_string(), index.clone()))
                && task
                    .env
                    .contains(&("RELIABURGER_BATCH_ID".to_string(), "7".to_string()))
                && task.env.contains(&("PLAIN".to_string(), "yes".to_string()));
            let args_ok = task.args == vec!["square".to_string(), index];
            AttemptOutcome::Exited {
                code: i32::from(!(env_ok && args_ok)),
            }
        }));
        let pool = TaskPool::new(Arc::clone(&runner), fast(4));
        let mut chunk = work(64, 64, 0);
        chunk.args = vec!["square".to_string(), "{index}".to_string()];
        chunk.env = vec![("PLAIN".to_string(), "yes".to_string())];
        let outcome = pool.run_chunk(&chunk, &CancellationToken::new()).await;
        assert_eq!(
            outcome.result.succeeded, 64,
            "{:?}",
            outcome.result.failed_indices
        );
    }

    #[tokio::test]
    async fn a_real_process_chunk_runs_end_to_end() {
        let pool = TaskPool::new(Arc::new(ProcessRunner::default()), fast(4));
        let mut chunk = work(20, 20, 0);
        // Odd indices fail on their first attempt only.
        chunk.args = vec![
            "-c".to_string(),
            "[ $(( {index} % 2 )) -eq 0 ] || [ \"$RELIABURGER_TASK_ATTEMPT\" -gt 1 ]".to_string(),
        ];
        let outcome = pool.run_chunk(&chunk, &CancellationToken::new()).await;
        assert_eq!(
            outcome.result.succeeded, 20,
            "{:?}",
            outcome.result.failed_indices
        );
        assert_eq!(outcome.result.retried, 10);
    }
}
