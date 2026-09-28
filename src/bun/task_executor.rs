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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInvocation {
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
            Self::Exited { .. } | Self::Signalled { .. } | Self::TimedOut
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
    fn push(&mut self, bytes: &[u8]) {
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
    /// Run `task`, killing it after `timeout` or when `cancel` fires.
    fn run(
        &self,
        task: &TaskInvocation,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Attempt> + Send;
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
            () = tokio::time::sleep(timeout) => {
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
                () = tokio::time::sleep(self.delay.min(timeout)) => {
                    if self.delay > timeout {
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
    /// The array's batch id (goes into each task's environment).
    pub batch_id: u64,
    /// Count and policy.
    pub spec: TaskArraySpec,
    /// Which chunk.
    pub chunk: ChunkId,
    /// The leader's grant attempt, echoed back in the result.
    pub grant_attempt: u8,
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
}

/// Runs the tasks of any number of chunks through one shared set of
/// slots. Several chunks can run at once; the slots, not the chunks,
/// bound the work, so the tail of one chunk overlaps the head of the next.
pub struct TaskPool<R: TaskRunner> {
    runner: Arc<R>,
    config: PoolConfig,
    slots: Arc<Semaphore>,
    counters: Arc<PoolCounters>,
}

impl<R: TaskRunner> TaskPool<R> {
    /// A pool running attempts through `runner`.
    pub fn new(runner: Arc<R>, config: PoolConfig) -> Self {
        let permits = config.concurrency.max(1) as usize;
        Self {
            runner,
            config,
            slots: Arc::new(Semaphore::new(permits)),
            counters: Arc::new(PoolCounters::default()),
        }
    }

    /// The live counters.
    pub fn counters(&self) -> &Arc<PoolCounters> {
        &self.counters
    }

    /// Run every task in the chunk and account for each one exactly once.
    /// On cancel, tasks not yet started count as not run and running ones
    /// are stopped.
    pub async fn run_chunk(&self, work: &ChunkWork, cancel: &CancellationToken) -> ChunkOutcome {
        // A chunk outside the array has no tasks, and so an empty result.
        let indices: Vec<u32> = work
            .spec
            .chunk_range(work.chunk)
            .into_iter()
            .flatten()
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
                records.push(not_run(index));
                continue;
            };
            let task = TaskAttempts {
                runner: Arc::clone(&self.runner),
                config: self.config,
                slots: Arc::clone(&self.slots),
                counters: Arc::clone(&self.counters),
                work: Arc::clone(&work),
                cancel: cancel.clone(),
            };
            running.spawn(task.run(index, permit));
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
        ChunkOutcome {
            result: summarise(&work, records.as_slice()),
            records,
        }
    }
}

fn not_run(index: u32) -> TaskRecord {
    TaskRecord {
        index,
        attempts: 0,
        outcome: TaskFinal::NotRun,
        exit_code: None,
        run_ms: 0,
        output: None,
    }
}

fn summarise(work: &ChunkWork, records: &[TaskRecord]) -> ChunkResult {
    let mut result = ChunkResult {
        chunk: work.chunk,
        attempt: work.grant_attempt,
        succeeded: 0,
        failed_indices: IndexRangeSet::new(),
        not_run: 0,
        retried: 0,
    };
    for record in records {
        match record.outcome {
            TaskFinal::Succeeded => result.succeeded += 1,
            TaskFinal::Failed => {
                result.failed_indices.insert(record.index);
            }
            TaskFinal::NotRun => result.not_run += 1,
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
    counters: Arc<PoolCounters>,
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
            let invocation = self.invocation(index, attempt);
            self.counters
                .attempts_started
                .fetch_add(1, Ordering::Relaxed);
            self.counters.running.fetch_add(1, Ordering::Relaxed);
            let started = Instant::now();
            let result = self.runner.run(&invocation, timeout, &self.cancel).await;
            let run_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            self.counters.running.fetch_sub(1, Ordering::Relaxed);
            drop(permit.take());

            let exit_code = match result.outcome {
                AttemptOutcome::Exited { code } => Some(code),
                AttemptOutcome::Signalled { signal } => Some(-signal),
                _ => None,
            };
            let record = TaskRecord {
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
            batch_id: 7,
            spec: TaskArraySpec {
                chunk_size,
                ..TaskArraySpec::with_count(count)
            },
            chunk: ChunkId(chunk),
            grant_attempt: 1,
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
            index: 3,
            attempt: 1,
            program: PathBuf::from(program),
            args: args.iter().map(|a| a.to_string()).collect(),
            env: vec![("GREETING".to_string(), "hello".to_string())],
        }
    }

    // -- M3.1: runners ---------------------------------------------------

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
