/// Process-based container runtime.
///
/// Implements the `Grill` trait by spawning child processes via
/// `tokio::process::Command`. Each "container" is a child process.
/// Works on macOS and Linux — the cross-platform fallback when
/// neither `runc` nor Apple's `container` CLI is available.
///
/// The 0.1.0 contract requires foreground workloads whose children remain in
/// the supervised process group. Daemonising, detached groups/sessions and
/// hand-off to external service managers require Linux container mode instead.
/// Process groups are cooperative supervision, not a security boundary.
///
/// Two capture modes:
/// - **In-memory** (default, `new()`): stdout/stderr are piped into
///   buffers. Simple, but nothing survives a bun restart.
/// - **File-backed** (`with_log_dir`): stdout/stderr append to
///   `{log_dir}/{instance}.stdout` / `.stderr`. Workloads keep writing
///   through a self-upgrade `exec()` (a pipe would go with the old
///   process's reader tasks — and a SIGPIPE would kill the workload),
///   and a fresh bun can *adopt* them from their instance records.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use super::oci::OciSpec;
use super::process_control::ProcessControl;
use super::process_owner::{GateReport, OwnerPhase, OwnerRecord};
use super::records::{self, InstanceRecord};
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// Variables a host command inherits from Bun, plus every `LC_*`.
///
/// Anything else in Bun's environment (cloud credentials, `RELIABURGER_*`
/// settings) stays private to Bun. The workload's own `env` is applied on top.
pub const HOST_ENVIRONMENT_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "LANG", "LANGUAGE", "TZ", "USER", "LOGNAME", "SHELL", "TMPDIR",
];

/// Whether a host command inherits this variable from Bun.
pub fn inherited_by_host_commands(key: &str) -> bool {
    HOST_ENVIRONMENT_ALLOWLIST.contains(&key) || key.starts_with("LC_")
}

/// The complete environment of a host command: Bun's allowlisted variables
/// overlaid with the workload's `KEY=value` entries.
///
/// Every host backend (in-memory, owned and native executors) uses this, so a
/// command sees the same environment wherever it runs.
pub fn host_environment(workload: &[String]) -> std::collections::BTreeMap<String, String> {
    host_environment_from(std::env::vars_os(), workload)
}

/// The variables of Bun's own environment a host command inherits, read
/// now. A caller that starts many commands reads them once and overlays
/// each command's entries with [`host_environment_over`].
pub fn inherited_host_environment() -> std::collections::BTreeMap<String, String> {
    inherited_from(std::env::vars_os())
}

/// `inherited` overlaid with the workload's `KEY=value` entries: the same
/// environment [`host_environment`] builds, from variables read earlier.
pub fn host_environment_over(
    mut inherited: std::collections::BTreeMap<String, String>,
    workload: &[String],
) -> std::collections::BTreeMap<String, String> {
    inherited.extend(
        workload
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned())),
    );
    inherited
}

fn inherited_from(
    inherited: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> std::collections::BTreeMap<String, String> {
    inherited
        .into_iter()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| inherited_by_host_commands(key))
        .collect()
}

fn host_environment_from(
    inherited: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    workload: &[String],
) -> std::collections::BTreeMap<String, String> {
    host_environment_over(inherited_from(inherited), workload)
}

#[cfg(test)]
struct CaptureGate {
    entered: tokio::sync::watch::Sender<usize>,
    release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl Default for CaptureGate {
    fn default() -> Self {
        Self {
            entered: tokio::sync::watch::channel(0).0,
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

#[cfg(test)]
struct ReleaseCaptureGate(Arc<CaptureGate>);

#[cfg(test)]
impl Drop for ReleaseCaptureGate {
    fn drop(&mut self) {
        self.0.release.add_permits(2);
    }
}

/// Child exit and pipe EOF are separate events. The entry owns both reader
/// lifetimes, including when every asynchronous logs caller is canceled.
struct CaptureTasks {
    instance: InstanceId,
    done: tokio::sync::watch::Sender<[bool; 2]>,
    readers: std::sync::Mutex<[Option<tokio::task::AbortHandle>; 2]>,
    drain_started: std::sync::atomic::AtomicBool,
    truncated: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    deadline_abort_issued: tokio::sync::Notify,
}

const CAPTURE_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

impl CaptureTasks {
    fn new(instance: &InstanceId) -> Arc<Self> {
        Arc::new(Self {
            instance: instance.clone(),
            done: tokio::sync::watch::channel([false; 2]).0,
            readers: std::sync::Mutex::new([None, None]),
            drain_started: std::sync::atomic::AtomicBool::new(false),
            truncated: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            deadline_abort_issued: tokio::sync::Notify::new(),
        })
    }

    async fn wait(&self) {
        let mut completion = self.done.subscribe();
        loop {
            if *completion.borrow_and_update() == [true; 2] {
                return;
            }
            if completion.changed().await.is_err() {
                return;
            }
        }
    }

    fn abort(&self) {
        for reader in self.readers.lock().unwrap().iter().flatten() {
            reader.abort();
        }
    }

    fn start_drain(self: &Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.drain_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let capture = self.clone();
        let deadline = tokio::time::Instant::now() + CAPTURE_DRAIN_TIMEOUT;
        tokio::spawn(async move {
            if tokio::time::timeout_at(deadline, capture.wait())
                .await
                .is_err()
            {
                capture.truncated.store(true, Ordering::Release);
                eprintln!(
                    "process {} log capture: pipe drain exceeded 2s after child exit; truncating inherited pipes and preserving captured bytes",
                    capture.instance
                );
                capture.abort();
                // Abort issuance is not task termination: a reader already
                // executing on another worker can still publish its chunk.
                // Only the owned CaptureCompletion guards acknowledge that
                // no reader can write any more bytes, even without a waiter.
                #[cfg(test)]
                capture.deadline_abort_issued.notify_one();
            }
        });
    }
}

struct CaptureCompletion {
    capture: Arc<CaptureTasks>,
    index: usize,
}

impl Drop for CaptureCompletion {
    fn drop(&mut self) {
        self.capture
            .done
            .send_modify(|done| done[self.index] = true);
    }
}

/// A child process managed by ProcessGrill.
struct ProcessEntry {
    spec: OciSpec,
    child: Option<tokio::process::Child>,
    /// A process started by a previous bun. Mutually exclusive with
    /// `child`: adopted processes have no handle, only a pid.
    adopted: Option<AdoptedProcess>,
    state: ContainerState,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    /// Base path for file-backed logs (`{stem}.stdout` / `{stem}.stderr`).
    log_stem: Option<PathBuf>,
    exit_code: Option<i32>,
    /// In-memory workloads have no adoption path, so dropping their last
    /// owner must not leave the process tree behind.
    cleanup_on_drop: bool,
    capture: Option<Arc<CaptureTasks>>,
}

/// The recorded identity of an adopted process.
#[derive(Debug, Clone, Copy)]
struct AdoptedProcess {
    pid: u32,
    /// Start time of the pid, to detect pid reuse (M23).
    started_at: u64,
}

impl Drop for ProcessEntry {
    fn drop(&mut self) {
        if let Some(capture) = &self.capture {
            capture.abort();
        }
        if !self.cleanup_on_drop {
            return;
        }

        if let Some(child) = self.child.as_mut()
            && let Some(pid) = child.id()
        {
            #[cfg(unix)]
            let pid = nix::unistd::Pid::from_raw(-(pid as i32));
            #[cfg(not(unix))]
            let pid = nix::unistd::Pid::from_raw(pid as i32);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            let _ = child.start_kill();
        } else if let Some(adopted) = self.adopted
            && records::process_matches(adopted.pid, adopted.started_at)
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(adopted.pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

use super::records::poll_adopted_process;

/// Signal only an adopted process whose recorded identity still matches.
fn signal_adopted_process(
    entry: &ProcessEntry,
    signal: nix::sys::signal::Signal,
) -> std::io::Result<bool> {
    let Some(adopted) = entry.adopted else {
        return Ok(false);
    };
    let nix_pid = nix::unistd::Pid::from_raw(adopted.pid as i32);
    if !records::process_matches(adopted.pid, adopted.started_at) {
        if nix::sys::signal::kill(nix_pid, None) == Err(nix::errno::Errno::ESRCH) {
            return Ok(false);
        }
        return Err(std::io::Error::other(
            "adopted process identity cannot be verified",
        ));
    }
    match nix::sys::signal::kill(nix_pid, signal) {
        Ok(()) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Record an owned child's observed exit before deciding whether to signal it.
fn observe_child_exit(entry: &mut ProcessEntry) -> std::io::Result<()> {
    if let Some(child) = entry.child.as_mut()
        && let Some(status) = child.try_wait()?
    {
        entry.exit_code = status.code();
        entry.state = ContainerState::Stopped;
        if let Some(capture) = &entry.capture {
            capture.start_drain();
        }
    }
    Ok(())
}

/// Signal an owned child, then its process group, after
/// `observe_child_exit` saw it still running.
///
/// The child goes first because its exit code is the one we record. macOS
/// delivers a group signal one member at a time, newest first, so a
/// group-only signal reaches a shell's children before the shell, which can
/// wake from `wait` and `exit 0` before its own signal lands: a killed
/// workload would read as a clean exit (#916). Signalled first, the child
/// reports the signal unless it had already exited on its own.
///
/// The child is unreaped, so its process identifier (and the group named
/// after it) can't be recycled while we hold it. A direct signal succeeds
/// while the child runs or exits, so an error from it is a genuine
/// permission refusal. The group signal can then still fail on macOS with
/// EPERM, because XNU skips zombies and exiting processes and reports EPERM
/// when no member of the group accepted the signal (#723). That refusal
/// only re-checks the child, recording its exit if it has one.
fn signal_child_group(
    entry: &mut ProcessEntry,
    signal: nix::sys::signal::Signal,
) -> std::io::Result<()> {
    let Some(pid) = entry.child.as_ref().and_then(|child| child.id()) else {
        return Ok(());
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    let group = nix::unistd::Pid::from_raw(-(pid as i32));
    #[cfg(not(unix))]
    let group = nix::unistd::Pid::from_raw(pid as i32);
    match nix::sys::signal::kill(group, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        #[cfg(target_os = "macos")]
        Err(nix::errno::Errno::EPERM) => observe_child_exit(entry),
        Err(error) => Err(error.into()),
    }
}

/// Why the process runtime can never launch `spec`, judged from the spec
/// alone: it runs an image, or it names nothing to run.
///
/// The process runtime runs host workloads (`exec` and `script` apps, and
/// the init containers of such an app that name no image of their own) and
/// nothing else (#914). It used to run an image app's `command` on the host,
/// or `sleep 86400` when it had none, so the app looked as though it ran.
fn spec_problem(spec: &OciSpec) -> Option<super::LaunchProblem> {
    use super::LaunchProblem;
    if !spec.host_process && spec.root.path != super::oci::HOST_ROOT {
        return Some(LaunchProblem::NeedsContainerRuntime(spec.root.path.clone()));
    }
    if let Some(problem) = spec.runtime_knobs_refusal("process") {
        return Some(problem);
    }
    spec.process
        .args
        .is_empty()
        .then_some(LaunchProblem::NoCommand)
}

/// Whether `program` names something this node can execute, resolving a bare
/// name on the workload's `PATH` the way `execvp` does.
///
/// Only answers that can't change by retrying are refusals. A path that
/// can't be inspected for another reason (a permission on a parent
/// directory, say) is left for the launch itself to report.
fn check_launchable(
    program: &str,
    environment: &std::collections::BTreeMap<String, String>,
) -> Result<(), super::LaunchProblem> {
    use super::LaunchProblem;
    if program.contains('/') {
        let path = PathBuf::from(program);
        return match executable(&path) {
            Some(true) => Ok(()),
            Some(false) => Err(LaunchProblem::BinaryNotExecutable(path)),
            None if absent(&path) => Err(LaunchProblem::BinaryNotFound(path)),
            None => Ok(()),
        };
    }
    // Bun clears the workload's environment, so only its own PATH counts.
    // Without one, `execvp` falls back to the default search path.
    let search = environment
        .get("PATH")
        .map(String::as_str)
        .unwrap_or("/usr/local/bin:/bin:/usr/bin");
    let mut found_unexecutable = None;
    for directory in search.split(':') {
        let directory = if directory.is_empty() { "." } else { directory };
        let candidate = Path::new(directory).join(program);
        match executable(&candidate) {
            Some(true) => return Ok(()),
            // Uncertain evidence never refuses a launch.
            None if !absent(&candidate) => return Ok(()),
            Some(false) if found_unexecutable.is_none() => found_unexecutable = Some(candidate),
            _ => {}
        }
    }
    Err(match found_unexecutable {
        Some(path) => LaunchProblem::BinaryNotExecutable(path),
        None => LaunchProblem::BinaryNotFound(PathBuf::from(program)),
    })
}

/// `Some(true)` for an executable file, `Some(false)` for a directory or a
/// file without any execute bit, `None` when the path can't be inspected.
fn executable(path: &Path) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Whether the path certainly doesn't exist: it, or one of its parents, is
/// missing or isn't a directory.
fn absent(path: &Path) -> bool {
    matches!(
        std::fs::metadata(path),
        Err(error) if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        )
    )
}

/// What a started generation's execution gate reported, as a start result.
///
/// A binary that vanished after `create`'s check is a refused launch, as one
/// missing at `create` is (#905). Another gate failure fails the start with
/// the gate's reason. Silence, or no report at all, means the command runs.
fn gate_result(instance: &InstanceId, record: &OwnerRecord) -> Result<(), GrillError> {
    let Some(report @ GateReport::Failed { reason, .. }) = &record.gate else {
        return Ok(());
    };
    // Loading the record proved its command non-empty.
    let program = record.command.first().map(String::as_str).unwrap_or("");
    match report.launch_problem(program) {
        Some(problem) => Err(GrillError::LaunchRefused {
            instance: instance.clone(),
            problem,
        }),
        None => Err(GrillError::StartFailed {
            instance: instance.clone(),
            reason: format!("execution gate failed: {reason}"),
        }),
    }
}

fn owner_error(instance: &InstanceId, error: impl std::fmt::Display) -> GrillError {
    GrillError::StateUnavailable {
        instance: instance.clone(),
        reason: error.to_string(),
    }
}

fn log_file(stem: &Path, suffix: &str) -> PathBuf {
    let mut name = stem
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".");
    name.push(suffix);
    stem.with_file_name(name)
}

/// Process-based Grill implementation.
///
/// Spawns OS processes instead of OCI containers. Useful for
/// development, testing, and platforms without container runtimes.
#[derive(Clone)]
pub struct ProcessGrill {
    processes: Arc<Mutex<HashMap<InstanceId, ProcessEntry>>>,
    /// When set, stdout/stderr go to files here instead of pipes.
    log_dir: Option<PathBuf>,
    /// Durable owner authority for persistent production workloads.
    control: Option<ProcessControl>,
    #[cfg(test)]
    capture_gate: Option<Arc<CaptureGate>>,
    #[cfg(test)]
    file_eof_gate: Option<Arc<CaptureGate>>,
    /// State checks that fail before answering, as an owner too busy to
    /// reply within its timeout fails.
    #[cfg(test)]
    state_failures: Arc<std::sync::atomic::AtomicUsize>,
}

impl ProcessGrill {
    /// Create a new ProcessGrill with in-memory log capture.
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: None,
            control: None,
            #[cfg(test)]
            capture_gate: None,
            #[cfg(test)]
            file_eof_gate: None,
            #[cfg(test)]
            state_failures: Arc::default(),
        }
    }

    /// Create a ProcessGrill that writes workload output to files under
    /// `log_dir`, enabling adoption across bun restarts and upgrades.
    pub fn with_log_dir(log_dir: PathBuf) -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: Some(log_dir),
            control: None,
            #[cfg(test)]
            capture_gate: None,
            #[cfg(test)]
            file_eof_gate: None,
            #[cfg(test)]
            state_failures: Arc::default(),
        }
    }

    /// Create a persistent runtime backed by the foreground owner in `bun`.
    /// Every launch is discoverable before an agent adoption record exists.
    pub fn with_owner(log_dir: PathBuf, executable: PathBuf) -> Self {
        let control = ProcessControl::new(log_dir.join("process-owners"), executable);
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: Some(log_dir),
            control: Some(control),
            #[cfg(test)]
            capture_gate: None,
            #[cfg(test)]
            file_eof_gate: None,
            #[cfg(test)]
            state_failures: Arc::default(),
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn executor_directory(&self) -> Result<PathBuf, GrillError> {
        self.log_dir
            .as_ref()
            .map(|path| path.join("host-executors"))
            .ok_or_else(|| GrillError::StateUnavailable {
                instance: InstanceId("host-executor".into()),
                reason: "host executor requires durable ownership".into(),
            })
    }

    /// Owner-backed lifecycle operations that are still running, including
    /// ones whose caller dropped its future: cancellation never cancels a
    /// queued mutation. Zero means none can still change owner state. Always
    /// zero without an owner.
    pub fn owner_operations_in_flight(&self) -> usize {
        self.control
            .as_ref()
            .map_or(0, ProcessControl::operations_in_flight)
    }

    /// Get captured stdout for an instance.
    pub async fn stdout(&self, instance: &InstanceId) -> Result<Vec<u8>, GrillError> {
        self.read_stream(instance, true).await
    }

    /// Get captured stderr for an instance.
    pub async fn stderr(&self, instance: &InstanceId) -> Result<Vec<u8>, GrillError> {
        self.read_stream(instance, false).await
    }

    pub(crate) async fn tail_snapshot(
        &self,
        instance: &InstanceId,
    ) -> super::capture::TailSnapshot {
        if let Some(stem) = super::Grill::log_stem(self, instance).await {
            return super::capture::TailSnapshot::files(&stem).await;
        }
        let buffers = {
            let procs = self.processes.lock().await;
            let Some(entry) = procs.get(instance) else {
                return Default::default();
            };
            [entry.stdout_buf.clone(), entry.stderr_buf.clone()]
        };
        let mut snapshot = super::capture::TailSnapshot::default();
        for (suffix, buffer) in ["stdout", "stderr"].into_iter().zip(buffers) {
            let bytes = buffer.lock().await;
            let start = bytes.len().saturating_sub(1024 * 1024);
            snapshot.add(
                PathBuf::from(format!("{}.{suffix}", instance.0)),
                &bytes[start..],
                start as u64,
                None,
            );
        }
        snapshot
    }

    async fn read_stream(
        &self,
        instance: &InstanceId,
        stdout: bool,
    ) -> Result<Vec<u8>, GrillError> {
        if let Some(control) = &self.control {
            let stem = control
                .log_stem(instance)
                .map_err(|error| owner_error(instance, error))?;
            control
                .record(instance)
                .await
                .map_err(|error| owner_error(instance, error))?;
            let path = log_file(&stem, if stdout { "stdout" } else { "stderr" });
            return tokio::task::spawn_blocking(move || {
                match super::capture::read_retained(&path) {
                    Ok(bytes) => Ok(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                    Err(error) => Err(error),
                }
            })
            .await
            .map_err(|error| owner_error(instance, error))?
            .map_err(|error| owner_error(instance, error));
        }
        let (buffer, capture) = {
            let mut procs = self.processes.lock().await;
            let entry = procs
                .get_mut(instance)
                .ok_or_else(|| GrillError::NotFound {
                    instance: instance.clone(),
                })?;
            observe_child_exit(entry).map_err(|error| owner_error(instance, error))?;
            if let Some(stem) = &entry.log_stem {
                let path = log_file(stem, if stdout { "stdout" } else { "stderr" });
                return Ok(super::capture::read_retained(&path).unwrap_or_default());
            }
            let buffer = if stdout {
                entry.stdout_buf.clone()
            } else {
                entry.stderr_buf.clone()
            };
            let capture = if entry.state == ContainerState::Stopped {
                entry.capture.clone()
            } else {
                None
            };
            (buffer, capture)
        };
        if let Some(capture) = capture {
            capture.wait().await;
        }
        Ok(buffer.lock().await.clone())
    }
}

impl Default for ProcessGrill {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Grill for ProcessGrill {
    /// The owned execution gate joins the spec's cgroup before it runs the
    /// command, on rootful cgroup v2 Linux.
    fn places_host_processes(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.host_executor_runtime().is_some();
        #[cfg(not(target_os = "linux"))]
        false
    }

    #[cfg(target_os = "linux")]
    fn host_executor_runtime(&self) -> Option<Self> {
        (self.control.is_some()
            && nix::unistd::geteuid().is_root()
            && Path::new("/sys/fs/cgroup/cgroup.controllers").exists())
        .then(|| self.clone())
    }

    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        // A launch that can never work fails here, before any generation
        // exists, so the caller can fail the instance with the reason.
        let refused = |problem| GrillError::LaunchRefused {
            instance: instance.clone(),
            problem,
        };
        if let Some(problem) = spec_problem(spec) {
            return Err(refused(problem));
        }
        // A missing binary would only show once the owner's gate tried to
        // exec it, after start had already succeeded (#881).
        let Some(program) = spec.process.args.first().cloned() else {
            return Err(refused(super::LaunchProblem::NoCommand));
        };
        let environment = host_environment(&spec.process.env);
        let checked = program.clone();
        let check = tokio::task::spawn_blocking(move || check_launchable(&checked, &environment))
            .await
            .map_err(|error| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("binary check for {program} did not finish: {error}"),
            })?;
        if let Err(problem) = check {
            return Err(refused(problem));
        }
        if let Some(control) = &self.control {
            return control.prepare(instance, spec).await.map_err(|error| {
                GrillError::StartFailed {
                    instance: instance.clone(),
                    reason: error.to_string(),
                }
            });
        }
        let mut procs = self.processes.lock().await;
        // Allow re-creation of stopped instances (needed for restart)
        if let Some(existing) = procs.get(instance)
            && existing.state != ContainerState::Stopped
        {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "instance already exists".to_string(),
            });
        }
        procs.insert(
            instance.clone(),
            ProcessEntry {
                spec: spec.clone(),
                child: None,
                adopted: None,
                state: ContainerState::Pending,
                stdout_buf: Arc::new(Mutex::new(Vec::new())),
                stderr_buf: Arc::new(Mutex::new(Vec::new())),
                log_stem: None,
                exit_code: None,
                cleanup_on_drop: self.log_dir.is_none(),
                capture: None,
            },
        );
        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        if let Some(control) = &self.control {
            let record =
                control
                    .start(instance)
                    .await
                    .map_err(|error| GrillError::StartFailed {
                        instance: instance.clone(),
                        reason: error.to_string(),
                    })?;
            return gate_result(instance, &record);
        }
        let mut procs = self.processes.lock().await;
        let entry = procs
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;

        if entry.child.is_some() || entry.adopted.is_some() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "already started".to_string(),
            });
        }

        // `create` refused a spec with nothing to run.
        let effective_args = &entry.spec.process.args;
        if effective_args.is_empty() {
            return Err(GrillError::LaunchRefused {
                instance: instance.clone(),
                problem: super::LaunchProblem::NoCommand,
            });
        }

        let mut cmd = Command::new(&effective_args[0]);
        cmd.kill_on_drop(entry.cleanup_on_drop);
        if effective_args.len() > 1 {
            cmd.args(&effective_args[1..]);
        }
        // A workload may be a shell that starts grandchildren. Giving each
        // workload its own process group lets stop/kill signal children that
        // follow the foreground contract. Detached groups are unsupported.
        #[cfg(unix)]
        cmd.process_group(0);

        cmd.env_clear();
        cmd.envs(host_environment(&entry.spec.process.env));

        // File-backed mode: append to log files that outlive this process
        // (they must survive a self-upgrade exec). In-memory mode: pipes.
        let log_stem = self.log_dir.as_ref().map(|dir| dir.join(&instance.0));
        if let Some(stem) = &log_stem {
            if let Some(dir) = stem.parent() {
                std::fs::create_dir_all(dir).map_err(|e| GrillError::StartFailed {
                    instance: instance.clone(),
                    reason: format!("failed to create log dir: {e}"),
                })?;
            }
            let open = |suffix: &str| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(log_file(stem, suffix))
            };
            let stdout_file = open("stdout").map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to open stdout log: {e}"),
            })?;
            let stderr_file = open("stderr").map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to open stderr log: {e}"),
            })?;
            cmd.stdout(std::process::Stdio::from(stdout_file));
            cmd.stderr(std::process::Stdio::from(stderr_file));
        } else {
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
        }

        let mut child = cmd.spawn().map_err(|error| {
            // The binary can vanish after `create`'s check (#905).
            let program = PathBuf::from(&effective_args[0]);
            let problem = match error.kind() {
                std::io::ErrorKind::NotFound => Some(super::LaunchProblem::BinaryNotFound(program)),
                std::io::ErrorKind::PermissionDenied => {
                    Some(super::LaunchProblem::BinaryNotExecutable(program))
                }
                _ => None,
            };
            match problem {
                Some(problem) => GrillError::LaunchRefused {
                    instance: instance.clone(),
                    problem,
                },
                None => GrillError::StartFailed {
                    instance: instance.clone(),
                    reason: error.to_string(),
                },
            }
        })?;

        let capture = if self.log_dir.is_none() {
            Some(CaptureTasks::new(instance))
        } else {
            None
        };
        // Spawn tasks to capture stdout/stderr (in-memory mode only —
        // file-backed mode has no pipes to read).
        let stdout_buf = entry.stdout_buf.clone();
        if let Some(stdout) = child.stdout.take() {
            #[cfg(test)]
            let gate = self.capture_gate.clone();
            let capture = capture.as_ref().unwrap().clone();
            let completion = CaptureCompletion {
                capture: capture.clone(),
                index: 0,
            };
            let reader_task = tokio::spawn(async move {
                let _completion = completion;
                let mut reader = stdout;
                #[cfg(test)]
                let mut gated = false;
                let mut buf = vec![0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            #[cfg(test)]
                            if !gated {
                                gated = true;
                                if let Some(gate) = &gate {
                                    gate.entered.send_modify(|count| *count += 1);
                                    gate.release.acquire().await.unwrap().forget();
                                }
                            }
                            let mut out = stdout_buf.lock().await;
                            out.extend_from_slice(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
            });
            capture.readers.lock().unwrap()[0] = Some(reader_task.abort_handle());
        }

        let stderr_buf = entry.stderr_buf.clone();
        if let Some(stderr) = child.stderr.take() {
            #[cfg(test)]
            let gate = self.capture_gate.clone();
            let capture = capture.as_ref().unwrap().clone();
            let completion = CaptureCompletion {
                capture: capture.clone(),
                index: 1,
            };
            let reader_task = tokio::spawn(async move {
                let _completion = completion;
                let mut reader = stderr;
                #[cfg(test)]
                let mut gated = false;
                let mut buf = vec![0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            #[cfg(test)]
                            if !gated {
                                gated = true;
                                if let Some(gate) = &gate {
                                    gate.entered.send_modify(|count| *count += 1);
                                    gate.release.acquire().await.unwrap().forget();
                                }
                            }
                            let mut out = stderr_buf.lock().await;
                            out.extend_from_slice(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
            });
            capture.readers.lock().unwrap()[1] = Some(reader_task.abort_handle());
        }

        entry.capture = capture;
        entry.child = Some(child);
        entry.log_stem = log_stem;
        entry.state = ContainerState::Running;
        Ok(())
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        if let Some(control) = &self.control {
            return control
                .signal(instance, false)
                .await
                .map_err(|error| GrillError::StopFailed {
                    instance: instance.clone(),
                    reason: error.to_string(),
                });
        }
        let mut procs = self.processes.lock().await;
        let entry = procs
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;
        let error = |error: std::io::Error| GrillError::StopFailed {
            instance: instance.clone(),
            reason: error.to_string(),
        };
        observe_child_exit(entry).map_err(error)?;
        if entry.state == ContainerState::Stopped {
            return Ok(());
        }
        // The app's own stop signal (#1121), SIGTERM unless it names one.
        let signal = entry.spec.stop_signal().to_nix();
        if entry.child.as_ref().and_then(|child| child.id()).is_some() {
            signal_child_group(entry, signal).map_err(error)?;
            if entry.state != ContainerState::Stopped {
                entry.state = ContainerState::Stopping;
            }
        } else if entry.adopted.is_some() {
            entry.state = if signal_adopted_process(entry, signal).map_err(error)? {
                ContainerState::Stopping
            } else {
                ContainerState::Stopped
            };
        } else {
            entry.state = ContainerState::Stopped;
        }
        Ok(())
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        if let Some(control) = &self.control {
            return control
                .signal(instance, true)
                .await
                .map_err(|error| GrillError::StopFailed {
                    instance: instance.clone(),
                    reason: error.to_string(),
                });
        }
        let mut procs = self.processes.lock().await;
        let entry = procs
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;
        let error = |error: std::io::Error| GrillError::StopFailed {
            instance: instance.clone(),
            reason: error.to_string(),
        };
        observe_child_exit(entry).map_err(error)?;
        if entry.state == ContainerState::Stopped {
            return Ok(());
        }
        signal_child_group(entry, nix::sys::signal::Signal::SIGKILL).map_err(error)?;
        if entry.state == ContainerState::Stopped {
            return Ok(());
        }
        if let Some(ref mut child) = entry.child {
            entry.state = ContainerState::Stopping;
            child.start_kill().map_err(error)?;
            let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
                .await
                .map_err(|_| GrillError::StopFailed {
                    instance: instance.clone(),
                    reason: "process did not exit after force-kill".into(),
                })?
                .map_err(error)?;
            entry.exit_code = status.code();
            entry.state = ContainerState::Stopped;
            if let Some(capture) = &entry.capture {
                capture.start_drain();
            }
        } else if entry.adopted.is_some() {
            entry.state = if signal_adopted_process(entry, nix::sys::signal::Signal::SIGKILL)
                .map_err(error)?
            {
                ContainerState::Stopping
            } else {
                ContainerState::Stopped
            };
        } else {
            entry.state = ContainerState::Stopped;
        }
        Ok(())
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        #[cfg(test)]
        if self
            .state_failures
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            self.state_failures
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(owner_error(instance, "process owner request timed out"));
        }
        if let Some(control) = &self.control {
            let record = control
                .status_if_present(instance)
                .await
                .map_err(|error| owner_error(instance, error))?
                .ok_or_else(|| GrillError::NotFound {
                    instance: instance.clone(),
                })?;
            return Ok(match record.phase {
                OwnerPhase::Prepared => ContainerState::Pending,
                OwnerPhase::Retiring { .. } => ContainerState::Stopping,
                OwnerPhase::Running { .. } => ContainerState::Running,
                OwnerPhase::Retired { .. } | OwnerPhase::Cancelled => ContainerState::Stopped,
            });
        }
        let mut procs = self.processes.lock().await;
        let entry = procs
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;

        // Check if the process has exited
        if entry.child.is_some() {
            observe_child_exit(entry).map_err(|error| GrillError::StateUnavailable {
                instance: instance.clone(),
                reason: error.to_string(),
            })?;
        } else if let Some(adopted) = entry.adopted {
            // Adopted process: no handle, poll (and reap) by pid. This
            // doubles as the zombie reaper — the supervisor polls state
            // regularly, so exited adoptees get waitpid'd here.
            if entry.state != ContainerState::Stopped {
                let (running, exit_code) = poll_adopted_process(adopted.pid, adopted.started_at)
                    .map_err(|error| GrillError::StateUnavailable {
                        instance: instance.clone(),
                        reason: error.to_string(),
                    })?;
                if !running {
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = exit_code;
                }
            }
        }

        Ok(entry.state)
    }

    async fn forget_retired_generation(
        &self,
        instance: &InstanceId,
        generation: &super::RuntimeGeneration,
    ) -> Result<(), GrillError> {
        // Without an owner journal there is no generation to compare, and
        // `launch_generation` never handed one out.
        let Some(control) = &self.control else {
            return Ok(());
        };
        control
            .forget_retired(instance, Some(generation))
            .await
            .map_err(|error| owner_error(instance, error))
    }

    async fn launch_generation(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<super::RuntimeGeneration>, GrillError> {
        let Some(control) = &self.control else {
            return Ok(None);
        };
        control
            .generation(instance)
            .await
            .map_err(|error| owner_error(instance, error))
    }

    async fn forget_retired(&self, instance: &InstanceId) -> Result<(), GrillError> {
        if let Some(control) = &self.control {
            control
                .forget_retired(instance, None)
                .await
                .map_err(|error| owner_error(instance, error))?;
        }
        let mut processes = self.processes.lock().await;
        if processes
            .get(instance)
            .is_some_and(|entry| entry.state == ContainerState::Stopped)
        {
            processes.remove(instance);
        }
        Ok(())
    }

    async fn launch_inventory(&self) -> Result<Option<Vec<super::RuntimeLaunch>>, GrillError> {
        let Some(control) = &self.control else {
            return Ok(None);
        };
        control
            .inventory()
            .await
            .map(Some)
            .map_err(|error| GrillError::InventoryUnavailable {
                reason: error.to_string(),
            })
    }

    async fn adopt(
        &self,
        instance: &InstanceId,
        record: &InstanceRecord,
    ) -> Result<bool, GrillError> {
        if let Some(control) = &self.control {
            let owner = control
                .status(instance)
                .await
                .map_err(|error| owner_error(instance, error))?;
            if owner.launch.spec != record.oci_spec {
                return Err(owner_error(
                    instance,
                    "process launch conflicts with adoption record",
                ));
            }
            return match owner.phase {
                OwnerPhase::Running { .. } => Ok(true),
                OwnerPhase::Retired { .. } | OwnerPhase::Cancelled => Ok(false),
                OwnerPhase::Prepared | OwnerPhase::Retiring { .. } => Err(owner_error(
                    instance,
                    "unactivated process preparation requires recovery",
                )),
            };
        }
        // A process recorded in an earlier boot is gone, whatever now holds its pid.
        if !records::from_this_boot(record) {
            return Ok(false);
        }
        let (running, _) =
            poll_adopted_process(record.pid, record.pid_started_at).map_err(|error| {
                GrillError::StateUnavailable {
                    instance: instance.clone(),
                    reason: error.to_string(),
                }
            })?;
        if !running {
            return Ok(false);
        }
        let mut procs = self.processes.lock().await;
        procs.insert(
            instance.clone(),
            ProcessEntry {
                spec: record.oci_spec.clone(),
                child: None,
                adopted: Some(AdoptedProcess {
                    pid: record.pid,
                    started_at: record.pid_started_at,
                }),
                state: ContainerState::Running,
                stdout_buf: Arc::new(Mutex::new(Vec::new())),
                stderr_buf: Arc::new(Mutex::new(Vec::new())),
                log_stem: record.log_stem.clone(),
                exit_code: None,
                cleanup_on_drop: self.log_dir.is_none(),
                capture: None,
            },
        );
        Ok(true)
    }

    async fn pid(&self, instance: &InstanceId) -> Result<Option<u32>, GrillError> {
        if let Some(control) = &self.control {
            // An owner that didn't answer leaves the pid unknown, never
            // "no process" (#358).
            let record = control
                .status_if_present(instance)
                .await
                .map_err(|error| owner_error(instance, error))?
                .ok_or_else(|| GrillError::NotFound {
                    instance: instance.clone(),
                })?;
            return Ok(match record.phase {
                OwnerPhase::Running { pid } => Some(pid),
                _ => None,
            });
        }
        let procs = self.processes.lock().await;
        Ok(procs.get(instance).and_then(|entry| {
            entry
                .child
                .as_ref()
                .and_then(|c| c.id())
                .or(entry.adopted.map(|adopted| adopted.pid))
        }))
    }

    async fn log_stem(&self, instance: &InstanceId) -> Option<PathBuf> {
        if let Some(control) = &self.control {
            return control.log_stem(instance).ok();
        }
        let procs = self.processes.lock().await;
        procs.get(instance).and_then(|entry| {
            entry
                .log_stem
                .clone()
                .or_else(|| self.log_dir.as_ref().map(|dir| dir.join(&instance.0)))
        })
    }

    async fn exit_code(&self, instance: &InstanceId) -> Result<Option<i32>, GrillError> {
        if let Some(control) = &self.control {
            // An owner that didn't answer leaves the exit code unknown,
            // never "hasn't exited" (#389).
            let record = control
                .status_if_present(instance)
                .await
                .map_err(|error| owner_error(instance, error))?
                .ok_or_else(|| GrillError::NotFound {
                    instance: instance.clone(),
                })?;
            return Ok(match record.phase {
                OwnerPhase::Retired { exit_code } => exit_code,
                _ => None,
            });
        }
        let mut procs = self.processes.lock().await;
        let Some(entry) = procs.get_mut(instance) else {
            return Ok(None);
        };
        observe_child_exit(entry).map_err(|error| owner_error(instance, error))?;
        Ok(entry.exit_code)
    }

    /// Both streams, stdout then stderr, as runc answers: the two capture
    /// files carry no times, so this can't interleave them. The log store,
    /// which ingests each line as it arrives, keeps the order.
    async fn logs(&self, instance: &InstanceId) -> Result<String, GrillError> {
        let mut logs = String::from_utf8_lossy(&self.stdout(instance).await?).into_owned();
        let stderr = self.stderr(instance).await?;
        if !stderr.is_empty() {
            if !logs.is_empty() && !logs.ends_with('\n') {
                logs.push('\n');
            }
            logs.push_str(&String::from_utf8_lossy(&stderr));
        }
        Ok(logs)
    }

    async fn exec(
        &self,
        instance: &InstanceId,
        command: &[String],
    ) -> Result<super::ExecOutput, GrillError> {
        // Verify the instance exists and is running
        if let Some(control) = &self.control {
            return control
                .exec(instance, command)
                .await
                .map_err(|error| owner_error(instance, error));
        }
        // The same environment as the workload itself (see `host_environment`).
        let environment = {
            let procs = self.processes.lock().await;
            let entry = procs.get(instance).ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;
            if entry.state != ContainerState::Running {
                return Err(GrillError::StartFailed {
                    instance: instance.clone(),
                    reason: format!("instance is not running (state: {})", entry.state),
                });
            }
            host_environment(&entry.spec.process.env)
        };

        if command.is_empty() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "no command specified".to_string(),
            });
        }

        // Spawn the command directly (no namespace entry for ProcessGrill)
        // A caller's timeout drops this future (a pre-stop command's bound,
        // #1121); the command mustn't outlive it.
        let output = Command::new(&command[0])
            .args(&command[1..])
            .env_clear()
            .envs(environment)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("exec failed: {e}"),
            })?;

        let mut result = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&stderr);
        }
        Ok(super::ExecOutput {
            output: result,
            // A signal-killed command has no code; report a plain failure.
            exit_code: output.status.code().unwrap_or(1),
        })
    }

    async fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: tokio::sync::mpsc::Sender<crate::ketchup::types::CapturedLine>,
        resume: &crate::ketchup::types::CaptureOffsets,
    ) {
        use crate::ketchup::types::LogStream;

        // Snapshot how this instance's logs are captured: two files, or two
        // in-memory buffers, one per stream.
        let (buffers, log_stem, capture) = if let Some(control) = &self.control {
            let empty = || Arc::new(Mutex::new(Vec::new()));
            ([empty(), empty()], control.log_stem(instance).ok(), None)
        } else {
            let procs = self.processes.lock().await;
            match procs.get(instance) {
                Some(entry) => (
                    [entry.stdout_buf.clone(), entry.stderr_buf.clone()],
                    entry.log_stem.clone(),
                    entry.capture.clone(),
                ),
                None => return,
            }
        };

        // One reader per stream, each with its own offset, as runc follows
        // its two files. Lines keep their order within a stream; across the
        // two, the store orders them by when they were read.
        let mut readers = Vec::with_capacity(2);
        for ((stream, suffix), buffer) in
            [(LogStream::Stdout, "stdout"), (LogStream::Stderr, "stderr")]
                .into_iter()
                .zip(buffers)
        {
            let reader = match &log_stem {
                Some(stem) => {
                    crate::grill::capture::CaptureReader::resume(
                        stream,
                        log_file(stem, suffix),
                        resume,
                    )
                    .await
                }
                None => crate::grill::capture::CaptureReader::memory_at(
                    stream,
                    resume
                        .get(&PathBuf::from(format!("{}.{suffix}", instance.0)))
                        .unwrap_or(0),
                ),
            };
            readers.push((reader, buffer));
        }

        let mut drained = false;
        let mut watch =
            crate::grill::capture::SourceWatch::new(crate::grill::capture::UNKNOWN_SOURCE_LIMIT);
        #[cfg(test)]
        let mut eof_gated = false;
        loop {
            // New bytes since the last poll, from each file or buffer, at
            // most one bounded chunk per stream at a time: a capture with no
            // checkpoint replays from byte 0, and one long synchronous step
            // would hold a runtime worker for as long as it took.
            let mut no_new_data = true;
            let mut backlog = false;
            for (reader, buffer) in &mut readers {
                let new_data = if reader.file().is_some() {
                    reader.read_chunk().await.unwrap_or_default()
                } else {
                    let offset = usize::try_from(reader.read_offset()).unwrap_or(usize::MAX);
                    let buf = buffer.lock().await;
                    let end = buf
                        .len()
                        .min(offset.saturating_add(crate::grill::capture::CAPTURE_CHUNK_BYTES));
                    buf.get(offset..end).unwrap_or_default().to_vec()
                };
                no_new_data &= new_data.is_empty();
                backlog |= new_data.len() == crate::grill::capture::CAPTURE_CHUNK_BYTES;
                for line in reader.push(&new_data) {
                    if lines_tx.send(line).await.is_err() {
                        return;
                    }
                }
            }
            if backlog {
                // The in-memory buffer never awaits; hand the worker back.
                tokio::task::yield_now().await;
                continue;
            }

            #[cfg(test)]
            if no_new_data
                && !eof_gated
                && log_stem.is_some()
                && let Some(gate) = &self.file_eof_gate
            {
                eof_gated = true;
                gate.entered.send_modify(|count| *count += 1);
                gate.release.acquire().await.unwrap().forget();
            }

            if !no_new_data {
                watch.confirm();
            }

            // Check if the process has exited and no more data is coming. A
            // state that can't be read is a busy owner, not an ended process:
            // leaving now would freeze this instance's logs for good (#1493).
            let exited = match self.state(instance).await {
                Ok(state) => {
                    watch.confirm();
                    state == ContainerState::Stopped
                }
                Err(GrillError::NotFound { .. }) => return,
                Err(error) => {
                    if watch.check_failed(tokio::time::Instant::now()) {
                        eprintln!("ketchup: no longer following {instance}: {error}");
                        return;
                    }
                    false
                }
            };
            if exited && no_new_data {
                if !drained {
                    if let Some(capture) = &capture {
                        capture.wait().await;
                    }
                    drained = true;
                    // Re-scan after confirmed exit/completion. Both a file
                    // writer and an owned pipe reader can publish final bytes
                    // between the earlier empty read and state observation.
                    continue;
                }
                for (reader, _) in &mut readers {
                    if let Some(line) = reader.finish() {
                        let _ = lines_tx.send(line).await;
                    }
                }
                return;
            }

            tokio::select! {
                _ = lines_tx.closed() => return,
                _ = tokio::time::sleep(crate::grill::capture::FOLLOW_POLL) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::Grill;
    use crate::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
    use crate::grill::records::{self, InstanceRecord, RuntimeKind};

    /// A host workload's spec: the only kind the process runtime runs.
    fn spec_with_args(args: Vec<String>) -> OciSpec {
        OciSpec {
            reusable_executor: false,
            host_process: true,
            initialiser: false,
            port_mapping: None,
            stop: None,
            root: OciRoot {
                path: "/tmp/test".to_string(),
                readonly: false,
            },
            process: OciProcess {
                rlimits: Vec::new(),
                args,
                env: vec!["TEST_VAR=hello".to_string()],
                cwd: "/".to_string(),
                user: OciUser { uid: 0, gid: 0 },
                capabilities: None,
                oom_score_adj: None,
                overrides: None,
            },
            mounts: vec![],
            linux: OciLinux {
                namespaces: vec![],
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
                sysctl: Default::default(),
                devices: Vec::new(),
            },
            runtime_knobs: None,
        }
    }

    fn echo_spec(msg: &str) -> OciSpec {
        spec_with_args(vec!["echo".to_string(), msg.to_string()])
    }

    fn sleep_spec(secs: &str) -> OciSpec {
        spec_with_args(vec!["sleep".to_string(), secs.to_string()])
    }

    async fn wait_for_state(grill: &ProcessGrill, instance: &InstanceId, expected: ContainerState) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if grill.state(instance).await.unwrap() == expected {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{instance:?} did not reach {expected:?}"));
    }

    fn record_for(instance: &InstanceId, pid: u32, started_at: u64) -> InstanceRecord {
        InstanceRecord {
            schema: crate::grill::records::RECORD_SCHEMA,
            instance_id: instance.0.clone(),
            namespace: "default".to_string(),
            app_name: "test".to_string(),
            replica_index: 0,
            image: String::new(),
            runtime: RuntimeKind::Process,
            pid,
            pid_started_at: started_at,
            boot_id: crate::grill::records::current_boot(),
            runc_container_id: None,
            log_stem: None,
            host_port: None,
            app_spec: None,
            oci_spec: sleep_spec("60"),
            rootless_network: None,
        }
    }

    /// Wait until the child `pid` has exited, without reaping it.
    ///
    /// WNOWAIT observes the exit but leaves the zombie in place, preserving
    /// the exact zombie-group state that macOS refuses to signal.
    async fn wait_until_exited_unreaped(pid: u32) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let mut info = std::mem::MaybeUninit::<nix::libc::siginfo_t>::zeroed();
                // SAFETY: pid belongs to a child this test spawned; info points
                // to correctly sized, initialised storage. WNOHANG bounds the
                // call and WNOWAIT preserves ownership.
                let result = unsafe {
                    nix::libc::waitid(
                        nix::libc::P_PID,
                        pid as nix::libc::id_t,
                        info.as_mut_ptr(),
                        nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::WNOWAIT,
                    )
                };
                assert_eq!(
                    result,
                    0,
                    "waitid failed: {}",
                    std::io::Error::last_os_error()
                );
                // SAFETY: the POD buffer was zero-initialised and waitid
                // succeeded; si_pid reads the process-event member.
                let observed_pid = unsafe { info.assume_init().si_pid() };
                if observed_pid == pid as nix::libc::pid_t {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stop_and_kill_reap_already_exited_children_without_signalling_zombies() {
        for operation in ["stop", "kill"] {
            for exit in [0, 7] {
                let grill = ProcessGrill::new();
                let id = InstanceId(format!("completed-{operation}-{exit}"));
                let spec =
                    spec_with_args(vec!["/bin/sh".into(), "-c".into(), format!("exit {exit}")]);
                grill.create(&id, &spec).await.unwrap();
                grill.start(&id).await.unwrap();
                let pid = grill.pid(&id).await.unwrap().unwrap();
                wait_until_exited_unreaped(pid).await;
                let result = if operation == "stop" {
                    grill.stop(&id).await
                } else {
                    grill.kill(&id).await
                };
                assert!(
                    result.is_ok(),
                    "{operation} on exited child failed: {result:?}"
                );
                assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
                assert_eq!(grill.exit_code(&id).await.unwrap(), Some(exit));
            }
        }
    }

    /// `stop` and `kill` first look for an exit, then signal the group. A
    /// child that exits between those two steps is an unreaped zombie when
    /// the signal goes out, and macOS refuses a group of zombies with EPERM
    /// rather than ESRCH (#723). That window can't be hit on demand through
    /// `stop` or `kill`, so this drives their signalling step directly on a
    /// zombie whose exit nobody has observed yet.
    #[tokio::test]
    async fn signalling_a_child_that_exited_after_the_exit_check_is_not_an_error() {
        for signal in [
            nix::sys::signal::Signal::SIGTERM,
            nix::sys::signal::Signal::SIGKILL,
        ] {
            for exit in [0, 7] {
                let grill = ProcessGrill::new();
                let id = InstanceId(format!("raced-{signal}-{exit}"));
                let spec =
                    spec_with_args(vec!["/bin/sh".into(), "-c".into(), format!("exit {exit}")]);
                grill.create(&id, &spec).await.unwrap();
                grill.start(&id).await.unwrap();
                let pid = grill.pid(&id).await.unwrap().unwrap();
                wait_until_exited_unreaped(pid).await;

                let result = {
                    let mut procs = grill.processes.lock().await;
                    signal_child_group(procs.get_mut(&id).unwrap(), signal)
                };
                assert!(
                    result.is_ok(),
                    "{signal} to an exited child failed: {result:?}"
                );
                // Whether the platform delivered the signal to the zombie or
                // refused it, the next look reports the child's real exit.
                assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
                assert_eq!(grill.exit_code(&id).await.unwrap(), Some(exit));
            }
        }
    }

    #[tokio::test]
    async fn create_stores_spec() {
        let grill = ProcessGrill::new();
        let id = InstanceId("test-0".to_string());
        let spec = echo_spec("hello");

        grill.create(&id, &spec).await.unwrap();
        let state = grill.state(&id).await.unwrap();
        assert_eq!(state, ContainerState::Pending);
    }

    #[tokio::test]
    async fn start_spawns_process() {
        let grill = ProcessGrill::new();
        let id = InstanceId("test-0".to_string());
        let spec = sleep_spec("10");

        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();

        let state = grill.state(&id).await.unwrap();
        assert_eq!(state, ContainerState::Running);

        grill.kill(&id).await.unwrap();
    }

    #[tokio::test]
    async fn state_returns_running_while_alive() {
        let grill = ProcessGrill::new();
        let id = InstanceId("test-0".to_string());
        let spec = sleep_spec("10");

        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();

        let state = grill.state(&id).await.unwrap();
        assert_eq!(state, ContainerState::Running);

        // Clean up
        grill.kill(&id).await.unwrap();
    }

    #[tokio::test]
    async fn stop_sends_sigterm() {
        let grill = ProcessGrill::new();
        let id = InstanceId("test-0".to_string());
        let spec = sleep_spec("60");

        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();
        grill.stop(&id).await.unwrap();

        wait_for_state(&grill, &id, ContainerState::Stopped).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_terminates_shell_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        // Write the pid beside the file and rename it into place: `>` creates
        // the file before `echo` fills it, and the poll below could read it
        // empty in between.
        let script = format!(
            "sleep 60 & echo $! > {path}.tmp && mv {path}.tmp {path}; wait",
            path = pid_file.display()
        );
        let grill = ProcessGrill::new();
        let id = InstanceId("process-tree-0".to_string());

        grill
            .create(
                &id,
                &spec_with_args(vec!["sh".to_string(), "-c".to_string(), script]),
            )
            .await
            .unwrap();
        grill.start(&id).await.unwrap();

        let descendant_pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    .map_err(|_| ())
                    .and_then(|contents| contents.trim().parse::<u32>().map_err(|_| ()))
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("shell did not report its child pid");

        grill.stop(&id).await.unwrap();
        wait_for_state(&grill, &id, ContainerState::Stopped).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while records::process_start_time(descendant_pid).is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("stopping the workload left its shell descendant alive");
    }

    /// A killed shell that was waiting on its children must record the kill,
    /// not the `exit 0` it runs if a group signal reaches its children first.
    /// macOS signals a group one member at a time, newest first, so the
    /// kill goes to the shell before its group (#916).
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_records_a_shell_waiting_on_killed_children_as_killed() {
        for round in 0..30 {
            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("ready");
            let script = format!(
                "i=0; while [ $i -lt 400 ]; do sleep 60 & i=$((i+1)); done; \
                 sleep 60 & touch '{}'; wait $!; exit 0",
                ready.display()
            );
            let grill = ProcessGrill::new();
            let id = InstanceId(format!("killed-shell-{round}"));
            grill
                .create(
                    &id,
                    &spec_with_args(vec!["sh".to_string(), "-c".to_string(), script]),
                )
                .await
                .unwrap();
            grill.start(&id).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                while !ready.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("shell never started its children");
            grill.kill(&id).await.unwrap();
            wait_for_state(&grill, &id, ContainerState::Stopped).await;
            assert_eq!(grill.exit_code(&id).await.unwrap(), None, "round {round}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn panicking_owner_terminates_in_memory_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        // Write then rename: the redirection creates the file before `echo` fills
        // it, and the wait below only checks that the file exists.
        let script = format!(
            "sleep 60 & echo $! > {0}.tmp && mv {0}.tmp {0}; wait",
            pid_file.display()
        );
        let task = tokio::spawn(async move {
            let grill = ProcessGrill::new();
            let id = InstanceId("panic-cleanup-0".to_string());
            grill
                .create(
                    &id,
                    &spec_with_args(vec!["sh".to_string(), "-c".to_string(), script]),
                )
                .await
                .unwrap();
            grill.start(&id).await.unwrap();

            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !pid_file.is_file() {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("shell did not report its child pid");
            panic!("exercise unwind cleanup");
        });

        let error = task.await.expect_err("fixture task should panic");
        assert!(error.is_panic());
        let descendant_pid = std::fs::read_to_string(dir.path().join("child.pid"))
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while records::process_start_time(descendant_pid).is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("panicking fixture owner left its process tree alive");
    }

    #[tokio::test]
    async fn kill_sends_sigkill() {
        let grill = ProcessGrill::new();
        let id = InstanceId("test-0".to_string());
        let spec = sleep_spec("60");

        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();
        grill.kill(&id).await.unwrap();

        let state = grill.state(&id).await.unwrap();
        assert_eq!(state, ContainerState::Stopped);
    }

    #[tokio::test]
    async fn start_before_create_errors() {
        let grill = ProcessGrill::new();
        let id = InstanceId("test-0".to_string());

        let err = grill.start(&id).await.unwrap_err();
        assert!(matches!(err, GrillError::NotFound { .. }));
    }

    #[tokio::test]
    async fn state_after_natural_exit_returns_stopped() {
        let grill = ProcessGrill::new();
        let id = InstanceId("test-0".to_string());
        let spec = echo_spec("done");

        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();

        wait_for_state(&grill, &id, ContainerState::Stopped).await;
    }

    #[tokio::test]
    async fn double_start_errors() {
        let grill = ProcessGrill::new();
        let id = InstanceId("test-0".to_string());
        let spec = sleep_spec("10");

        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();

        let err = grill.start(&id).await.unwrap_err();
        assert!(matches!(err, GrillError::StartFailed { .. }));

        grill.kill(&id).await.unwrap();
    }

    // ---- file-backed capture and adoption ----

    #[tokio::test]
    async fn file_backed_mode_writes_logs_to_files() {
        let dir = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(dir.path().to_path_buf());
        let id = InstanceId("test-0".to_string());

        grill.create(&id, &echo_spec("to file")).await.unwrap();
        grill.start(&id).await.unwrap();
        let logged = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let logs = grill.logs(&id).await.unwrap();
                if logs.contains("to file") {
                    return logs;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("file-backed log was not written");
        assert!(logged.contains("to file"), "got {logged:?}");
        assert!(dir.path().join("test-0.stdout").is_file());
    }

    /// Read the first line `follow_logs` produces for `id`, as a fresh
    /// forwarder would after an agent restart.
    async fn first_followed_line(
        grill: &ProcessGrill,
        id: &InstanceId,
    ) -> crate::ketchup::types::CapturedLine {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        let follower = grill.clone();
        let follow_id = id.clone();
        let task = tokio::spawn(async move {
            follower
                .follow_logs(&follow_id, sender, &Default::default())
                .await
        });
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
            .await
            .expect("no line followed")
            .expect("follow ended without a line");
        drop(receiver);
        task.abort();
        line
    }

    /// V02 soak regression: every agent restart re-follows adopted
    /// instances from the start of their capture files. The replayed lines
    /// must carry the same positions, so the log store recognises them and
    /// doesn't store the instance's whole history again as new lines.
    #[tokio::test]
    async fn refollowing_a_capture_file_replays_the_same_positions_and_the_store_keeps_one_copy() {
        let dir = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(dir.path().to_path_buf());
        let id = InstanceId("test-0".to_string());
        grill.create(&id, &echo_spec("ACK 1")).await.unwrap();
        grill.start(&id).await.unwrap();

        let before_restart = first_followed_line(&grill, &id).await;
        let after_restart = first_followed_line(&grill, &id).await;
        assert_eq!(before_restart.line, "ACK 1");
        assert_eq!(
            before_restart.position,
            Some(crate::ketchup::types::CapturePosition {
                file: dir.path().join("test-0.stdout"),
                end_offset: "ACK 1\n".len() as u64,
                identity: Some(crate::ketchup::types::CaptureFileIdentity::of(
                    &std::fs::metadata(dir.path().join("test-0.stdout")).unwrap(),
                )),
            })
        );
        assert_eq!(after_restart, before_restart);

        let store_dir = tempfile::tempdir().unwrap();
        let record =
            |captured: crate::ketchup::types::CapturedLine| crate::ketchup::types::LogRecord {
                app: "echo".to_string(),
                namespace: "default".to_string(),
                instance: id.0.clone(),
                stream: captured.stream,
                line: captured.line,
                position: captured.position,
            };
        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
        assert!(store.ingest(&record(before_restart)));
        store.flush().await.unwrap();
        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
        assert!(!store.ingest(&record(after_restart)));
        let stored = store
            .query("echo", "default", None, None, None, None)
            .await
            .unwrap();
        assert_eq!(stored.len(), 1);
        grill.kill(&id).await.unwrap();
    }

    /// F07 part 2: a process's stderr reaches the log store labelled as
    /// stderr. It was written to `<instance>.stderr` all along, but only the
    /// `.stdout` file was followed, so it never arrived.
    #[tokio::test]
    async fn follow_logs_carries_stderr_as_stderr() {
        use crate::ketchup::types::LogStream;
        let dir = tempfile::tempdir().unwrap();
        for grill in [
            ProcessGrill::with_log_dir(dir.path().to_path_buf()),
            ProcessGrill::new(),
        ] {
            let id = InstanceId("both-0".to_string());
            let spec = spec_with_args(vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo to-out; echo to-err >&2".to_string(),
            ]);
            grill.create(&id, &spec).await.unwrap();
            grill.start(&id).await.unwrap();

            let mut lines: Vec<(LogStream, String)> = followed_lines(&grill, &id, 2)
                .await
                .into_iter()
                .map(|captured| (captured.stream, captured.line))
                .collect();
            lines.sort_by(|a, b| a.1.cmp(&b.1));
            assert_eq!(
                lines,
                [
                    (LogStream::Stderr, "to-err".to_string()),
                    (LogStream::Stdout, "to-out".to_string()),
                ]
            );
            let _ = grill.kill(&id).await;
            let _ = std::fs::remove_file(dir.path().join("both-0.stdout"));
            let _ = std::fs::remove_file(dir.path().join("both-0.stderr"));
        }
    }

    /// `logs()` answers with both streams, stdout first, as runc's does.
    #[tokio::test]
    async fn logs_include_stderr() {
        let grill = ProcessGrill::new();
        let id = InstanceId("both-1".to_string());
        let spec = spec_with_args(vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo to-out; echo to-err >&2".to_string(),
        ]);
        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();
        wait_for_state(&grill, &id, ContainerState::Stopped).await;
        let logs = grill.logs(&id).await.unwrap();
        assert!(logs.contains("to-out"), "{logs:?}");
        assert!(logs.contains("to-err"), "{logs:?}");
    }

    #[test]
    fn host_environment_keeps_the_allowlist_and_lets_the_workload_override_it() {
        let inherited = [
            ("PATH", "/usr/local/bin:/usr/bin"),
            ("HOME", "/root"),
            ("LC_ALL", "C.UTF-8"),
            ("AWS_SECRET_ACCESS_KEY", "bun-only"),
            ("RELIABURGER_JOIN_TOKEN", "bun-only"),
        ]
        .map(|(key, value)| (key.into(), value.into()));
        let environment =
            super::host_environment_from(inherited, &["PATH=/opt/job/bin".into(), "JOB=1".into()]);
        assert_eq!(
            environment.into_iter().collect::<Vec<_>>(),
            [
                ("HOME", "/root"),
                ("JOB", "1"),
                ("LC_ALL", "C.UTF-8"),
                ("PATH", "/opt/job/bin"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
    }

    /// One variable in this test process's environment that host commands
    /// must not inherit (nextest sets `CARGO_*`, sudo sets `SUDO_*`).
    pub(crate) fn private_variable() -> String {
        std::env::vars_os()
            .filter_map(|(key, _)| key.into_string().ok())
            .find(|key| !super::inherited_by_host_commands(key))
            .expect("the test environment has a variable outside the allowlist")
    }

    #[tokio::test]
    async fn exec_gets_the_workloads_environment_and_none_of_buns() {
        use std::os::unix::fs::PermissionsExt;
        let private = private_variable();
        let root = tempfile::tempdir().unwrap();
        let tool = root.path().join("rb-exec-tool");
        std::fs::write(&tool, "#!/bin/sh\nprintf found\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:/usr/bin:/bin", root.path().display());
        let grill = ProcessGrill::new();
        let id = InstanceId("exec-env-1".to_string());
        let mut spec = sleep_spec("30");
        spec.process.env = vec!["JOB=1".into(), format!("PATH={path}")];
        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();
        let env = grill.exec(&id, &["env".to_string()]).await.unwrap().output;
        let resolved = grill.exec(&id, &["rb-exec-tool".to_string()]).await;
        grill.kill(&id).await.unwrap();
        assert!(env.lines().any(|line| line == "JOB=1"), "{env}");
        assert!(
            env.lines().any(|line| line == format!("PATH={path}")),
            "{env}"
        );
        assert!(
            !env.lines()
                .any(|line| line.starts_with(&format!("{private}="))),
            "{private} leaked: {env}"
        );
        assert_eq!(resolved.unwrap().output.trim(), "found");
    }

    #[tokio::test]
    async fn host_commands_see_the_allowlisted_environment_only() {
        let private = private_variable();
        let grill = ProcessGrill::new();
        let id = InstanceId("env-1".to_string());
        let mut spec = spec_with_args(vec!["env".to_string()]);
        spec.process.env = vec!["JOB=1".into()];
        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();
        wait_for_state(&grill, &id, ContainerState::Stopped).await;
        let logs = grill.logs(&id).await.unwrap();
        let path = std::env::var("PATH").unwrap();
        assert!(
            logs.lines().any(|line| line == format!("PATH={path}")),
            "{logs}"
        );
        assert!(logs.lines().any(|line| line == "JOB=1"), "{logs}");
        assert!(
            !logs
                .lines()
                .any(|line| line.starts_with(&format!("{private}="))),
            "{private} leaked: {logs}"
        );
    }

    /// Read the first `count` lines `follow_logs` produces for `id`, as a
    /// fresh forwarder would.
    async fn followed_lines(
        grill: &ProcessGrill,
        id: &InstanceId,
        count: usize,
    ) -> Vec<crate::ketchup::types::CapturedLine> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        let follower = grill.clone();
        let follow_id = id.clone();
        let task = tokio::spawn(async move {
            follower
                .follow_logs(&follow_id, sender, &Default::default())
                .await
        });
        let mut lines = Vec::new();
        while lines.len() < count {
            let line = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
                .await
                .expect("follow stalled")
                .expect("follow ended early");
            lines.push(line);
        }
        drop(receiver);
        task.abort();
        lines
    }

    fn client_record(
        instance: &InstanceId,
        captured: crate::ketchup::types::CapturedLine,
    ) -> crate::ketchup::types::LogRecord {
        crate::ketchup::types::LogRecord {
            app: "client".to_string(),
            namespace: "default".to_string(),
            instance: instance.0.clone(),
            stream: captured.stream,
            line: captured.line,
            position: captured.position,
        }
    }

    async fn client_lines(store: &crate::ketchup::log_store::LogStore) -> Vec<String> {
        store
            .query("client", "default", None, None, None, None)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.line)
            .collect()
    }

    /// Every line `follow_logs` produces for `id` when it resumes at
    /// `resume`, until the capture goes quiet: what a forwarder reads.
    async fn lines_read_resuming(
        grill: &ProcessGrill,
        id: &InstanceId,
        resume: crate::ketchup::types::CaptureOffsets,
    ) -> Vec<crate::ketchup::types::CapturedLine> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        let follower = grill.clone();
        let follow_id = id.clone();
        let task =
            tokio::spawn(async move { follower.follow_logs(&follow_id, sender, &resume).await });
        let mut lines = Vec::new();
        while let Ok(Some(line)) =
            tokio::time::timeout(std::time::Duration::from_millis(800), receiver.recv()).await
        {
            lines.push(line);
        }
        task.abort();
        lines
    }

    /// #1493: one owner request timing out on a loaded node ended the
    /// forwarder for good. The instance kept running, but its lines stopped
    /// reaching the store, and `relish logs --tail` froze on the last ones
    /// stored. A failed check says nothing about the workload.
    #[tokio::test]
    async fn a_failed_state_check_keeps_a_running_instance_in_the_log_store() {
        let dir = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(dir.path().to_path_buf());
        let id = InstanceId("client-0".to_string());
        grill.create(&id, &sleep_spec("60")).await.unwrap();
        grill.start(&id).await.unwrap();
        let capture = dir.path().join("client-0.stdout");
        let mut capture_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&capture)
            .unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());

        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        let follower = grill.clone();
        let follow_id = id.clone();
        let task = tokio::spawn(async move {
            follower
                .follow_logs(&follow_id, sender, &Default::default())
                .await
        });

        // Each round is one flush, so the tail has to cross Parquet files.
        let mut written = 0;
        for round in 0..4 {
            if round > 0 {
                grill
                    .state_failures
                    .store(1, std::sync::atomic::Ordering::SeqCst);
                while grill
                    .state_failures
                    .load(std::sync::atomic::Ordering::SeqCst)
                    > 0
                {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
            for _ in 0..10 {
                written += 1;
                std::io::Write::write_all(&mut capture_file, format!("ACK {written}\n").as_bytes())
                    .unwrap();
            }
            for _ in 0..10 {
                let line = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
                    .await
                    .expect("the follower went quiet with lines still unread")
                    .expect("the follower ended while its instance ran");
                assert!(store.ingest(&client_record(&id, line)));
            }
            store.flush().await.unwrap();
        }

        let tail: Vec<String> = store
            .query("client", "default", None, None, None, Some(15))
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.line)
            .collect();
        let newest: Vec<String> = (26..=40).map(|n| format!("ACK {n}")).collect();
        assert_eq!(tail, newest);

        task.abort();
        grill.kill(&id).await.unwrap();
    }

    /// #308: after a Bun restart, a forwarder resumes the capture file at the
    /// store's checkpoint. Only the lines written since are read and
    /// ingested; a truncated capture falls back to byte 0.
    #[tokio::test]
    async fn a_restarted_forwarder_reads_only_the_lines_past_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(dir.path().to_path_buf());
        let id = InstanceId("client-0".to_string());
        grill.create(&id, &sleep_spec("60")).await.unwrap();
        grill.start(&id).await.unwrap();
        let capture = dir.path().join("client-0.stdout");
        std::fs::write(&capture, "ACK 1\nACK 2\nACK 3\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let open_store =
            || crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());

        let mut store = open_store();
        let first = lines_read_resuming(&grill, &id, store.capture_offsets()).await;
        assert_eq!(first.len(), 3);
        for line in first {
            assert!(store.ingest(&client_record(&id, line)));
        }
        store.flush().await.unwrap();
        drop(store);

        // Bun restarts while the instance keeps writing.
        let mut capture_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&capture)
            .unwrap();
        std::io::Write::write_all(&mut capture_file, b"ACK 4\nACK 5\n").unwrap();
        let mut store = open_store();
        let resumed = lines_read_resuming(&grill, &id, store.capture_offsets()).await;
        let read: Vec<&str> = resumed.iter().map(|line| line.line.as_str()).collect();
        assert_eq!(read, ["ACK 4", "ACK 5"], "the forwarder re-read old lines");
        for line in resumed {
            assert!(store.ingest(&client_record(&id, line)));
        }
        store.flush().await.unwrap();
        assert_eq!(
            client_lines(&store).await,
            ["ACK 1", "ACK 2", "ACK 3", "ACK 4", "ACK 5"]
        );
        drop(store);

        // Truncated while Bun was down: everything in it is new.
        std::fs::write(&capture, "ACK 6\n").unwrap();
        let mut store = open_store();
        let after_truncation = lines_read_resuming(&grill, &id, store.capture_offsets()).await;
        let read: Vec<&str> = after_truncation
            .iter()
            .map(|line| line.line.as_str())
            .collect();
        assert_eq!(read, ["ACK 6"]);
        for line in after_truncation {
            assert!(store.ingest(&client_record(&id, line)));
        }
        grill.kill(&id).await.unwrap();
    }

    /// #1418: in the 0.2.3 day run the soak's log spammer, about 200 lines a
    /// second, grew its capture file by 40 MiB an hour for 24 hours, and
    /// filled most of node 1's disk. Once the store holds the lines durably,
    /// the capture keeps only its last mebibyte allocated, while the process
    /// writes on and `stdout` still reads whole lines.
    #[tokio::test]
    async fn a_chatty_instance_s_capture_stays_bounded_once_its_lines_are_stored() {
        use std::os::unix::fs::MetadataExt;
        const LINE: &str = "spam the quick brown fox jumps over the lazy dog";
        const BYTES: u64 = 8 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(dir.path().to_path_buf());
        let id = InstanceId("spammer-0".to_string());
        grill
            .create(
                &id,
                &spec_with_args(vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("yes '{LINE}' | head -c {BYTES}; sleep 60"),
                ]),
            )
            .await
            .unwrap();
        grill.start(&id).await.unwrap();
        let capture = dir.path().join("spammer-0.stdout");
        let expected = BYTES / (LINE.len() as u64 + 1);

        let (sender, mut receiver) = tokio::sync::mpsc::channel(1024);
        let follower = grill.clone();
        let follow_id = id.clone();
        let task = tokio::spawn(async move {
            follower
                .follow_logs(&follow_id, sender, &Default::default())
                .await
        });
        let store_dir = tempfile::tempdir().unwrap();
        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
        let mut read = 0;
        while read < expected {
            let line = tokio::time::timeout(std::time::Duration::from_secs(30), receiver.recv())
                .await
                .expect("the follower went quiet")
                .unwrap();
            assert!(store.ingest(&client_record(&id, line)));
            read += 1;
        }
        task.abort();
        let allocated = || std::fs::metadata(&capture).unwrap().blocks() * 512;
        assert!(
            allocated() >= BYTES,
            "the whole output is on disk before a flush"
        );

        store.flush().await.unwrap();

        assert_eq!(std::fs::metadata(&capture).unwrap().len(), BYTES);
        assert!(
            allocated() <= super::super::capture::RETAINED_CAPTURE_BYTES + 256 * 1024,
            "{} bytes of the capture are still allocated",
            allocated()
        );
        let stdout = grill.stdout(&id).await.unwrap();
        assert!(!stdout.contains(&0), "reclaimed bytes read back as zeros");
        let text = String::from_utf8(stdout).unwrap();
        let mut lines = text.split('\n');
        assert_eq!(lines.next(), Some(LINE), "the first line read is whole");
        grill.kill(&id).await.unwrap();
    }

    fn printf_spec(output: &str) -> OciSpec {
        spec_with_args(vec!["printf".to_string(), output.to_string()])
    }

    /// V02 soak blocker (candidate 3fcb1fd): after a SIGKILL, Bun re-follows
    /// every adopted instance's capture file from byte 0. The soak's log
    /// spammer had written about a million lines in 80 minutes, and the
    /// forwarder split them in one synchronous call on a runtime worker. On a
    /// two-vCPU node that starved startup adoption for 11 minutes, until the
    /// next instance's 10 s adoption deadline expired and Bun exited.
    ///
    /// Replaying a backlog must hand the runtime back between bounded chunks,
    /// so a concurrent task (here a 1 ms timer, standing in for adoption)
    /// keeps running on a single-threaded runtime.
    #[tokio::test(flavor = "current_thread")]
    async fn replaying_a_large_capture_backlog_does_not_hold_the_runtime() {
        const LINE: &str = "spam the quick brown fox jumps\n";
        const LINES: usize = 128 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(dir.path().to_path_buf());
        let id = InstanceId("spammer-0".to_string());
        grill.create(&id, &sleep_spec("60")).await.unwrap();
        grill.start(&id).await.unwrap();
        let capture = dir.path().join("spammer-0.stdout");
        std::fs::write(&capture, LINE.repeat(LINES)).unwrap();

        let (sender, mut receiver) = tokio::sync::mpsc::channel(256);
        let follower = grill.clone();
        let follow_id = id.clone();
        let task = tokio::spawn(async move {
            follower
                .follow_logs(&follow_id, sender, &Default::default())
                .await
        });
        let consumer = tokio::spawn(async move {
            let mut last = None;
            for _ in 0..LINES {
                last = receiver.recv().await;
            }
            last
        });

        let mut longest_stall = std::time::Duration::ZERO;
        let replay_started = std::time::Instant::now();
        while !consumer.is_finished() {
            let tick = std::time::Instant::now();
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            longest_stall = longest_stall.max(tick.elapsed());
            assert!(
                replay_started.elapsed() < std::time::Duration::from_secs(60),
                "the backlog was not replayed within 60 s"
            );
        }
        let last = consumer.await.unwrap().expect("replay ended early");
        task.abort();
        grill.kill(&id).await.unwrap();

        assert_eq!(
            last.position.unwrap().end_offset,
            (LINE.len() * LINES) as u64,
            "every line of the backlog is replayed, in order"
        );
        assert!(
            longest_stall < std::time::Duration::from_secs(1),
            "replaying the backlog held the runtime for {longest_stall:?}"
        );
    }

    /// V02 soak follow-up: after a graceful whole-cluster stop and start, a
    /// retired instance's capture file is still on disk next to its
    /// replacement's. Re-following both after the restart must not store the
    /// retired instance's lines again as the newest.
    #[tokio::test]
    async fn graceful_restart_does_not_reingest_a_retired_instances_capture_file() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(dir.path().to_path_buf());
        let retired = InstanceId("client-old".to_string());
        let current = InstanceId("client-new".to_string());
        grill
            .create(&retired, &printf_spec("INCR 1\\nINCR 2\\nINCR 3\\n"))
            .await
            .unwrap();
        grill.start(&retired).await.unwrap();
        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
        for line in followed_lines(&grill, &retired, 3).await {
            assert!(store.ingest(&client_record(&retired, line)));
        }
        grill.kill(&retired).await.unwrap();
        grill
            .create(&current, &printf_spec("INCR 4\\n"))
            .await
            .unwrap();
        grill.start(&current).await.unwrap();
        for line in followed_lines(&grill, &current, 1).await {
            assert!(store.ingest(&client_record(&current, line)));
        }
        let shared = std::sync::Arc::new(tokio::sync::RwLock::new(store));
        crate::ketchup::log_store::flush_shared(&shared)
            .await
            .unwrap();
        drop(shared);

        // Bun comes back and follows every capture file it finds from byte 0.
        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
        for (id, count) in [(&retired, 3), (&current, 1)] {
            for line in followed_lines(&grill, id, count).await {
                assert!(!store.ingest(&client_record(id, line)), "{id} re-ingested");
            }
        }
        assert_eq!(
            client_lines(&store).await,
            vec!["INCR 1", "INCR 2", "INCR 3", "INCR 4"]
        );
        grill.kill(&current).await.unwrap();
    }

    /// A graceful stop between two periodic flushes: the lines exist only in
    /// the buffer. The shutdown flush must persist them and their offsets, so
    /// the restart neither loses nor duplicates them.
    #[tokio::test]
    async fn graceful_stop_keeps_lines_that_were_only_buffered() {
        let dir = tempfile::tempdir().unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(dir.path().to_path_buf());
        let id = InstanceId("client-0".to_string());
        grill
            .create(&id, &printf_spec("INCR 1\\nINCR 2\\n"))
            .await
            .unwrap();
        grill.start(&id).await.unwrap();
        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
        for line in followed_lines(&grill, &id, 2).await {
            store.ingest(&client_record(&id, line));
        }
        assert_eq!(store.buffer_len(), 2, "nothing flushed before the stop");
        let shared = std::sync::Arc::new(tokio::sync::RwLock::new(store));
        crate::ketchup::log_store::flush_shared(&shared)
            .await
            .unwrap();
        drop(shared);

        let mut store = crate::ketchup::log_store::LogStore::new(store_dir.path().to_path_buf());
        for line in followed_lines(&grill, &id, 2).await {
            assert!(!store.ingest(&client_record(&id, line)));
        }
        assert_eq!(client_lines(&store).await, vec!["INCR 1", "INCR 2"]);
        grill.kill(&id).await.unwrap();
    }

    #[tokio::test]
    async fn adopts_live_process_and_reports_running() {
        // A process spawned outside the grill entirely stands in for a
        // workload started by a previous bun.
        let mut external = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = external.id();
        let started_at = records::process_start_time(pid).unwrap();

        let grill = ProcessGrill::new();
        let id = InstanceId("adopted-0".to_string());
        let adopted = grill
            .adopt(&id, &record_for(&id, pid, started_at))
            .await
            .unwrap();

        assert!(adopted);
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Running);
        assert_eq!(grill.pid(&id).await.unwrap(), Some(pid));

        external.kill().unwrap();
        external.wait().unwrap();
    }

    #[tokio::test]
    async fn adoption_refuses_invalid_process_ids_without_claiming_absence() {
        let grill = ProcessGrill::new();
        let id = InstanceId("invalid-adoption-0".into());
        for pid in [0, u32::MAX, i32::MAX as u32 + 1] {
            assert!(
                grill.adopt(&id, &record_for(&id, pid, 1000)).await.is_err(),
                "invalid pid {pid} was treated as a dead workload"
            );
        }
    }

    #[tokio::test]
    async fn adopt_returns_false_for_dead_pid() {
        let mut external = std::process::Command::new("true").spawn().unwrap();
        let pid = external.id();
        external.wait().unwrap();

        let grill = ProcessGrill::new();
        let id = InstanceId("adopted-0".to_string());
        let adopted = grill.adopt(&id, &record_for(&id, pid, 1000)).await.unwrap();

        assert!(!adopted);
        assert!(grill.state(&id).await.is_err());
    }

    async fn stale_adopted_owner_is_not_signalled(operation: &str) {
        let mut external = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = external.id();
        let started_at = records::process_start_time(pid).unwrap();
        let grill = ProcessGrill::new();
        let id = InstanceId("stale-adoptee".into());
        assert!(
            grill
                .adopt(&id, &record_for(&id, pid, started_at))
                .await
                .unwrap()
        );
        // Model a persisted owner that no longer matches the live PID. This
        // avoids depending on the operating system actually recycling a PID.
        grill
            .processes
            .lock()
            .await
            .get_mut(&id)
            .unwrap()
            .adopted
            .as_mut()
            .unwrap()
            .started_at = started_at + 3600;
        let refused = match operation {
            "stop" => grill.stop(&id).await.is_err(),
            "kill" => grill.kill(&id).await.is_err(),
            "drop" => true,
            _ => unreachable!(),
        };
        drop(grill);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let survived = external.try_wait().unwrap().is_none();
        let _ = external.kill();
        let _ = external.wait();
        assert!(refused, "{operation} accepted an unverified adopted owner");
        assert!(
            survived,
            "{operation} signalled a process with a different recorded identity"
        );
    }

    #[tokio::test]
    async fn stop_refuses_a_stale_adopted_owner() {
        stale_adopted_owner_is_not_signalled("stop").await;
    }

    #[tokio::test]
    async fn kill_refuses_a_stale_adopted_owner() {
        stale_adopted_owner_is_not_signalled("kill").await;
    }

    #[tokio::test]
    async fn drop_preserves_a_stale_adopted_owner() {
        stale_adopted_owner_is_not_signalled("drop").await;
    }

    #[tokio::test]
    async fn state_does_not_claim_exit_when_the_child_cannot_be_observed() {
        let root = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::with_log_dir(root.path().join("logs"));
        let id = InstanceId("lost-wait-owner".into());
        grill.create(&id, &sleep_spec("0.01")).await.unwrap();
        grill.start(&id).await.unwrap();
        let pid = grill.pid(&id).await.unwrap().unwrap();
        // Consume the kernel wait result outside the Child handle. The
        // runtime can no longer obtain its own exit evidence and must refuse.
        tokio::task::spawn_blocking(move || {
            nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid as i32), None).unwrap();
        })
        .await
        .unwrap();
        assert!(grill.state(&id).await.is_err());
    }

    #[tokio::test]
    async fn stop_kills_adopted_instance() {
        let mut external = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = external.id();
        let started_at = records::process_start_time(pid).unwrap();

        let grill = ProcessGrill::new();
        let id = InstanceId("adopted-0".to_string());
        assert!(
            grill
                .adopt(&id, &record_for(&id, pid, started_at))
                .await
                .unwrap()
        );

        grill.stop(&id).await.unwrap();
        // Reap via the parent handle (this test process is the real parent).
        external.wait().unwrap();
        assert!(records::process_start_time(pid).is_none());
    }

    #[tokio::test]
    async fn state_detects_adopted_instance_exit() {
        // The adopted process is a child of THIS process, mirroring the
        // exec() case where adoptees are still children — waitpid reaps.
        let external = std::process::Command::new("sleep")
            .arg("0.2")
            .spawn()
            .unwrap();
        let pid = external.id();
        let started_at = records::process_start_time(pid).unwrap();
        // Deliberately do not wait() on `external`: state() must reap it.

        let grill = ProcessGrill::new();
        let id = InstanceId("adopted-0".to_string());
        assert!(
            grill
                .adopt(&id, &record_for(&id, pid, started_at))
                .await
                .unwrap()
        );
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Running);

        wait_for_state(&grill, &id, ContainerState::Stopped).await;
        assert_eq!(grill.exit_code(&id).await.unwrap(), Some(0));
        std::mem::forget(external); // already reaped via waitpid
    }

    #[tokio::test]
    async fn adopted_instance_reads_logs_from_recorded_files() {
        let dir = tempfile::tempdir().unwrap();
        let stem = dir.path().join("adopted-0");
        std::fs::write(log_file(&stem, "stdout"), "written before the swap\n").unwrap();

        let mut external = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = external.id();
        let started_at = records::process_start_time(pid).unwrap();

        let grill = ProcessGrill::new();
        let id = InstanceId("adopted-0".to_string());
        let mut record = record_for(&id, pid, started_at);
        record.log_stem = Some(stem);
        assert!(grill.adopt(&id, &record).await.unwrap());

        let logs = grill.logs(&id).await.unwrap();
        assert!(logs.contains("written before the swap"));

        external.kill().unwrap();
        external.wait().unwrap();
    }
    async fn child_after_os_exit_with_gated_capture(
        observe: bool,
    ) -> (
        ProcessGrill,
        InstanceId,
        Arc<CaptureGate>,
        ReleaseCaptureGate,
    ) {
        let gate = Arc::new(CaptureGate::default());
        let mut entered = gate.entered.subscribe();
        let mut grill = ProcessGrill::new();
        grill.capture_gate = Some(gate.clone());
        let release = ReleaseCaptureGate(gate.clone());
        let id = InstanceId("gated-final-output".into());
        grill
            .create(
                &id,
                &spec_with_args(vec![
                    "sh".into(),
                    "-c".into(),
                    "echo to-out; echo to-err >&2".into(),
                ]),
            )
            .await
            .unwrap();
        grill.start(&id).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while *entered.borrow_and_update() < 2 {
                entered.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        // Actual OS exit is confirmed while both final chunks remain owned by
        // their readers, before either can publish its bytes to the buffers.
        grill
            .processes
            .lock()
            .await
            .get_mut(&id)
            .unwrap()
            .child
            .as_mut()
            .unwrap()
            .wait()
            .await
            .unwrap();
        if observe {
            assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
        }
        (grill, id, gate, release)
    }

    async fn stopped_child_with_gated_capture() -> (
        ProcessGrill,
        InstanceId,
        Arc<CaptureGate>,
        ReleaseCaptureGate,
    ) {
        child_after_os_exit_with_gated_capture(true).await
    }

    #[tokio::test]
    async fn final_log_snapshot_waits_for_reader_publication_after_child_exit() {
        let (grill, id, gate, _release) = stopped_child_with_gated_capture().await;
        let read = grill.logs(&id);
        tokio::pin!(read);
        assert!(
            futures_util::poll!(read.as_mut()).is_pending(),
            "Stopped was reported before pipe capture published its final bytes"
        );
        gate.release.add_permits(2);
        let logs = tokio::time::timeout(std::time::Duration::from_secs(5), read)
            .await
            .unwrap()
            .unwrap();
        assert!(
            logs.contains("to-out") && logs.contains("to-err"),
            "{logs:?}"
        );
    }

    #[tokio::test]
    async fn following_logs_waits_for_reader_publication_after_child_exit() {
        let (grill, id, gate, _release) = stopped_child_with_gated_capture().await;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let offsets = Default::default();
        let follow = grill.follow_logs(&id, sender, &offsets);
        tokio::pin!(follow);
        assert!(
            futures_util::poll!(follow.as_mut()).is_pending(),
            "following ended while final pipe chunks were still reader-owned"
        );
        gate.release.add_permits(2);
        tokio::time::timeout(std::time::Duration::from_secs(5), follow)
            .await
            .unwrap();
        let mut lines = Vec::new();
        while let Some(line) = receiver.recv().await {
            lines.push(line.line);
        }
        lines.sort();
        assert_eq!(lines, ["to-err", "to-out"]);
    }
    #[tokio::test]
    async fn canceling_a_final_snapshot_waiter_does_not_cancel_owned_capture() {
        let (grill, id, gate, _release) = stopped_child_with_gated_capture().await;
        let capture = grill
            .processes
            .lock()
            .await
            .get(&id)
            .unwrap()
            .capture
            .clone()
            .unwrap();
        let mut read = Box::pin(grill.logs(&id));
        assert!(futures_util::poll!(read.as_mut()).is_pending());
        drop(read);
        gate.release.add_permits(2);
        tokio::time::timeout(std::time::Duration::from_secs(5), capture.wait())
            .await
            .unwrap();
        assert!(!capture.truncated.load(std::sync::atomic::Ordering::Acquire));
        let logs = grill.logs(&id).await.unwrap();
        assert!(
            logs.contains("to-out") && logs.contains("to-err"),
            "{logs:?}"
        );
    }

    #[tokio::test]
    async fn dropping_the_last_process_owner_aborts_gated_pipe_readers() {
        let (grill, id, _gate, _release) = stopped_child_with_gated_capture().await;
        let capture = grill
            .processes
            .lock()
            .await
            .get(&id)
            .unwrap()
            .capture
            .clone()
            .unwrap();
        drop(grill);
        tokio::time::timeout(std::time::Duration::from_secs(5), capture.wait())
            .await
            .unwrap();
        assert_eq!(*capture.done.borrow(), [true; 2]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn descendant_held_pipes_are_bounded_after_exit_even_without_a_logs_waiter() {
        struct KillMatching {
            pid: u32,
            started_at: u64,
        }
        impl Drop for KillMatching {
            fn drop(&mut self) {
                if records::process_matches(self.pid, self.started_at) {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(self.pid as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("descendant.pid");
        let script = format!(
            "sleep 30 & echo $! > {}; echo to-out; echo to-err >&2; exit",
            pid_file.display()
        );
        let grill = ProcessGrill::new();
        let id = InstanceId("inherited-capture-pipes".into());
        grill
            .create(&id, &spec_with_args(vec!["sh".into(), "-c".into(), script]))
            .await
            .unwrap();
        grill.start(&id).await.unwrap();
        grill
            .processes
            .lock()
            .await
            .get_mut(&id)
            .unwrap()
            .child
            .as_mut()
            .unwrap()
            .wait()
            .await
            .unwrap();
        let pid: u32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _kill = KillMatching {
            pid,
            started_at: records::process_start_time(pid).unwrap(),
        };
        // exit_code is also an exit observer: no logs waiter is needed to own
        // or start the deadline. The child is gone, but its descendant still
        // holds both pipe write ends open.
        assert_eq!(grill.exit_code(&id).await.unwrap(), Some(0));
        let capture = grill
            .processes
            .lock()
            .await
            .get(&id)
            .unwrap()
            .capture
            .clone()
            .unwrap();
        assert!(
            capture
                .drain_started
                .load(std::sync::atomic::Ordering::Acquire)
        );
        tokio::time::timeout(
            CAPTURE_DRAIN_TIMEOUT + std::time::Duration::from_secs(3),
            capture.wait(),
        )
        .await
        .unwrap();
        assert!(capture.truncated.load(std::sync::atomic::Ordering::Acquire));
        let logs = grill.logs(&id).await.unwrap();
        assert!(
            logs.contains("to-out") && logs.contains("to-err"),
            "{logs:?}"
        );
    }
    #[tokio::test]
    async fn every_child_exit_observer_starts_the_owned_capture_drain() {
        use std::sync::atomic::Ordering;
        for observer in 0..4 {
            let (grill, id, gate, _release) = child_after_os_exit_with_gated_capture(false).await;
            let capture = grill
                .processes
                .lock()
                .await
                .get(&id)
                .unwrap()
                .capture
                .clone()
                .unwrap();
            assert!(!capture.drain_started.load(Ordering::Acquire));
            match observer {
                0 => assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped),
                1 => grill.stop(&id).await.unwrap(),
                2 => grill.kill(&id).await.unwrap(),
                3 => assert_eq!(grill.exit_code(&id).await.unwrap(), Some(0)),
                _ => unreachable!(),
            }
            assert!(capture.drain_started.load(Ordering::Acquire));
            assert_ne!(*capture.done.borrow(), [true; 2]);
            gate.release.add_permits(2);
            tokio::time::timeout(std::time::Duration::from_secs(5), capture.wait())
                .await
                .unwrap();
            assert!(!capture.truncated.load(Ordering::Acquire));
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_deadline_abort_waits_for_actual_reader_termination_acknowledgement() {
        struct DropGate {
            entered: tokio::sync::Notify,
            released: std::sync::Mutex<bool>,
            release: std::sync::Condvar,
        }
        struct HoldTermination {
            gate: Arc<DropGate>,
            _completion: CaptureCompletion,
        }
        impl Drop for HoldTermination {
            fn drop(&mut self) {
                self.gate.entered.notify_one();
                let mut released = self.gate.released.lock().unwrap();
                while !*released {
                    released = self.gate.release.wait(released).unwrap();
                }
                // CaptureCompletion drops only after this destructor returns.
            }
        }
        struct ReleaseTermination(Arc<DropGate>);
        impl Drop for ReleaseTermination {
            fn drop(&mut self) {
                *self.0.released.lock().unwrap() = true;
                self.0.release.notify_all();
            }
        }
        let gate = Arc::new(DropGate {
            entered: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            release: std::sync::Condvar::new(),
        });
        let _release_on_failure = ReleaseTermination(gate.clone());
        let capture = CaptureTasks::new(&InstanceId("termination-ack".into()));
        let held = HoldTermination {
            gate: gate.clone(),
            _completion: CaptureCompletion {
                capture: capture.clone(),
                index: 0,
            },
        };
        let first = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        });
        let second_completion = CaptureCompletion {
            capture: capture.clone(),
            index: 1,
        };
        let second = tokio::spawn(async move {
            let _completion = second_completion;
            std::future::pending::<()>().await;
        });
        *capture.readers.lock().unwrap() =
            [Some(first.abort_handle()), Some(second.abort_handle())];
        capture.start_drain();
        tokio::time::timeout(
            CAPTURE_DRAIN_TIMEOUT + std::time::Duration::from_secs(5),
            async {
                gate.entered.notified().await;
                capture.deadline_abort_issued.notified().await;
            },
        )
        .await
        .unwrap();
        // Abort has been issued at the deadline, but reader zero is still
        // terminating. Final snapshots must remain pending until its guard
        // confirms that it can no longer publish any bytes.
        let mut wait = Box::pin(capture.wait());
        assert!(
            futures_util::poll!(wait.as_mut()).is_pending(),
            "abort issuance was mistaken for reader termination"
        );
        *gate.released.lock().unwrap() = true;
        gate.release.notify_all();
        tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .unwrap();
        assert_eq!(*capture.done.borrow(), [true; 2]);
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(second.await.unwrap_err().is_cancelled());
    }

    async fn assert_file_follow_rescans_after_empty_read_then_child_exit(resuming: bool) {
        let directory = tempfile::tempdir().unwrap();
        let release_child = directory.path().join("release-child");
        let id = InstanceId("gated-file-final-output".into());
        let stem = directory.path().join(&id.0);
        let stdout = log_file(&stem, "stdout");
        let stderr = log_file(&stem, "stderr");
        let prefix = if resuming {
            b"already-seen\n".as_slice()
        } else {
            b"".as_slice()
        };
        std::fs::write(&stdout, prefix).unwrap();
        std::fs::write(&stderr, prefix).unwrap();
        let gate = Arc::new(CaptureGate::default());
        let mut entered = gate.entered.subscribe();
        let _release_on_failure = ReleaseCaptureGate(gate.clone());
        let mut grill = ProcessGrill::with_log_dir(directory.path().to_path_buf());
        grill.file_eof_gate = Some(gate.clone());
        let script = format!(
            "while [ ! -f '{}' ]; do sleep 0.01; done; printf 'final-out\\n'; printf 'final-err\\n' >&2",
            release_child.display()
        );
        grill
            .create(&id, &spec_with_args(vec!["sh".into(), "-c".into(), script]))
            .await
            .unwrap();
        // This test owns a gated foreground child. Unlike production file
        // capture, it must be killed if an assertion unwinds before release.
        grill
            .processes
            .lock()
            .await
            .get_mut(&id)
            .unwrap()
            .cleanup_on_drop = true;
        grill.start(&id).await.unwrap();
        let mut resume = crate::ketchup::types::CaptureOffsets::default();
        resume.0.insert(stdout.clone(), prefix.len() as u64);
        resume.0.insert(stderr.clone(), prefix.len() as u64);
        // The child is still gated. Bind each checkpointed prefix to the
        // actual descriptor that supplies its bytes, as the log store does.
        for file in [&stdout, &stderr] {
            let mut input = std::fs::File::open(file).unwrap();
            let mut observed = Vec::new();
            std::io::Read::read_to_end(&mut input, &mut observed).unwrap();
            assert_eq!(observed, prefix);
            resume.1.insert(
                file.clone(),
                crate::ketchup::types::CaptureFileIdentity::of(&input.metadata().unwrap()),
            );
        }
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let follow = grill.follow_logs(&id, sender, &resume);
        tokio::pin!(follow);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while *entered.borrow_and_update() == 0 {
                tokio::select! {
                    _ = follow.as_mut() => panic!("file follow returned before its empty-scan gate"),
                    change = entered.changed() => change.unwrap(),
                }
            }
        }).await.unwrap();
        assert!(
            receiver.try_recv().is_err(),
            "fixture emitted before the empty scan"
        );
        // The first file reads have already returned EOF. Only now does the
        // real child publish both final writes and exit; the follower is
        // still held before its state observation.
        std::fs::write(&release_child, "release").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            grill
                .processes
                .lock()
                .await
                .get_mut(&id)
                .unwrap()
                .child
                .as_mut()
                .unwrap()
                .wait()
                .await
                .unwrap();
        })
        .await
        .unwrap();
        gate.release.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(5), follow.as_mut())
            .await
            .unwrap();
        let mut captured = Vec::new();
        while let Some(line) = receiver.recv().await {
            captured.push(line);
        }
        assert_eq!(
            captured.len(),
            2,
            "file follower treated its pre-exit EOF as final"
        );
        for (line, stream, file, text) in [
            (
                &captured[0],
                crate::ketchup::types::LogStream::Stdout,
                &stdout,
                "final-out",
            ),
            (
                &captured[1],
                crate::ketchup::types::LogStream::Stderr,
                &stderr,
                "final-err",
            ),
        ] {
            assert_eq!(line.stream, stream);
            assert_eq!(line.line, text);
            let position = line.position.as_ref().unwrap();
            assert_eq!(&position.file, file);
            assert_eq!(position.end_offset, prefix.len() as u64 + 10);
        }
    }

    #[tokio::test]
    async fn file_follow_rescans_after_eof_before_actual_child_exit() {
        assert_file_follow_rescans_after_empty_read_then_child_exit(false).await;
    }

    #[tokio::test]
    async fn resumed_file_follow_keeps_offsets_across_eof_before_actual_child_exit() {
        assert_file_follow_rescans_after_empty_read_then_child_exit(true).await;
    }

    /// #881: the error a create returns for a binary nobody can run.
    fn refused_launch(error: GrillError) -> super::super::LaunchProblem {
        match error {
            GrillError::LaunchRefused { problem, .. } => problem,
            other => panic!("expected a refused launch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_refuses_a_missing_binary_as_binary_not_found() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("not-installed");
        let grill = ProcessGrill::new();
        let id = InstanceId("missing-0".to_string());
        let spec = spec_with_args(vec![missing.display().to_string()]);

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(
            problem.to_string(),
            format!("binary not found: {}", missing.display())
        );
    }

    #[tokio::test]
    async fn create_refuses_a_binary_without_execute_permission() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("not-executable");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let grill = ProcessGrill::new();
        let id = InstanceId("not-executable-0".to_string());
        let spec = spec_with_args(vec![script.display().to_string()]);

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(
            problem.to_string(),
            format!("binary not executable: {}", script.display())
        );
    }

    #[tokio::test]
    async fn create_refuses_a_directory_as_a_binary() {
        let root = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::new();
        let id = InstanceId("directory-0".to_string());
        let spec = spec_with_args(vec![root.path().display().to_string()]);

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(
            problem.to_string(),
            format!("binary not executable: {}", root.path().display())
        );
    }

    #[tokio::test]
    async fn create_looks_a_bare_command_up_on_the_workloads_path() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let tool = root.path().join("rb-path-tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let grill = ProcessGrill::new();
        let id = InstanceId("path-0".to_string());
        let mut spec = spec_with_args(vec!["rb-path-tool".to_string()]);
        spec.process.env = vec![format!("PATH={}", root.path().display())];

        grill.create(&id, &spec).await.unwrap();
    }

    #[tokio::test]
    async fn create_refuses_a_bare_command_missing_from_the_workloads_path() {
        let root = tempfile::tempdir().unwrap();
        let grill = ProcessGrill::new();
        let id = InstanceId("path-missing-0".to_string());
        let mut spec = spec_with_args(vec!["rb-no-such-tool".to_string()]);
        spec.process.env = vec![format!("PATH={}", root.path().display())];

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(problem.to_string(), "binary not found: rb-no-such-tool");
    }

    /// #905: a binary deleted after the pre-launch check fails the start
    /// as "binary not found", not as a generic spawn error.
    #[tokio::test]
    async fn start_refuses_a_binary_deleted_after_create_as_binary_not_found() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let tool = root.path().join("vanishing-tool");
        std::fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let grill = ProcessGrill::new();
        let id = InstanceId("vanishing-0".to_string());
        let spec = spec_with_args(vec![tool.display().to_string()]);
        grill.create(&id, &spec).await.unwrap();
        std::fs::remove_file(&tool).unwrap();

        let problem = refused_launch(grill.start(&id).await.unwrap_err());
        assert_eq!(
            problem,
            super::super::LaunchProblem::BinaryNotFound(tool.clone())
        );
    }

    #[tokio::test]
    async fn owned_create_refuses_a_missing_binary_before_preparing_a_generation() {
        let root = tempfile::tempdir().unwrap();
        let owners = root.path().join("owners");
        let missing = root.path().join("not-installed");
        let grill = ProcessGrill::with_owner(owners.clone(), "/bin/false".into());
        let id = InstanceId("owned-missing-0".to_string());
        let spec = spec_with_args(vec![missing.display().to_string()]);

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(
            problem,
            super::super::LaunchProblem::BinaryNotFound(missing.clone())
        );
        assert!(
            !owners.join(&id.0).exists(),
            "a refused launch must not leave a prepared generation behind"
        );
    }

    /// The OCI spec Bun builds for `app`, as a fresh deploy does.
    fn app_spec(name: &str, app: &str) -> OciSpec {
        let app: crate::config::app::AppSpec = toml::from_str(app).unwrap();
        crate::grill::oci::generate_oci_spec(
            name,
            "default",
            &app,
            &format!("default__{name}-0"),
            None,
            &format!("/reliaburger/default/{name}/0"),
            None,
            None,
        )
    }

    /// #914: an app that names an image needs a container runtime. The
    /// process runtime refuses it, instead of running its `command` on the
    /// host as though the image were a placeholder.
    #[tokio::test]
    async fn create_refuses_an_image_app_as_needing_a_container_runtime() {
        let grill = ProcessGrill::new();
        let id = InstanceId("default__web-0".to_string());
        let spec = app_spec(
            "web",
            "image = \"nginx:1.27\"\ncommand = [\"sleep\", \"60\"]",
        );

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(
            problem,
            super::super::LaunchProblem::NeedsContainerRuntime("nginx:1.27".to_string())
        );
        assert_eq!(
            problem.to_string(),
            "needs a container runtime to run image nginx:1.27, and this node's runtime is process"
        );
        assert!(
            matches!(grill.start(&id).await, Err(GrillError::NotFound { .. })),
            "a refused image app must leave nothing to start"
        );
    }

    /// #1122: a spec carrying runtime knobs is refused, not run without
    /// them. Validation keeps knobs off host apps, so this guards a spec
    /// that reaches the wrong runtime some other way.
    #[tokio::test]
    async fn create_refuses_a_spec_with_runtime_knobs_instead_of_ignoring_them() {
        let grill = ProcessGrill::new();
        let id = InstanceId("default__lb-0".to_string());
        let mut spec = echo_spec("hello");
        spec.runtime_knobs = Some(toml::from_str("capabilities = [\"IPC_OWNER\"]\n").unwrap());

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(
            problem.to_string(),
            "runtime.capabilities CAP_IPC_OWNER can't be applied: runtime knobs need rootful \
             runc, and this node's runtime is process"
        );
        assert!(matches!(
            grill.start(&id).await,
            Err(GrillError::NotFound { .. })
        ));
    }

    /// #914: an image app without a command used to run `sleep 86400` as a
    /// stand-in, and looked healthy while doing nothing.
    #[tokio::test]
    async fn create_refuses_an_image_app_without_a_command_instead_of_sleeping() {
        let grill = ProcessGrill::new();
        let id = InstanceId("default__idle-0".to_string());
        let spec = app_spec("idle", "image = \"busybox:1.36\"");

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(
            problem,
            super::super::LaunchProblem::NeedsContainerRuntime("busybox:1.36".to_string())
        );
    }

    /// #914: an init container that names its own image needs a container
    /// runtime too, even before an exec app. One that names none runs on the
    /// host, like its app.
    #[tokio::test]
    async fn create_refuses_an_init_container_image_but_runs_a_host_apps_init() {
        let grill = ProcessGrill::new();
        let app = app_spec("tool", "exec = \"/bin/sleep\"\ncommand = [\"60\"]");

        let id = InstanceId("default__tool-0-init-0".to_string());
        let init: crate::config::app::InitContainerSpec =
            toml::from_str("image = \"migrate:2\"\ncommand = [\"migrate\"]").unwrap();
        let spec = crate::grill::oci::generate_init_oci_spec(&init, &app);
        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(
            problem.to_string(),
            "needs a container runtime to run image migrate:2, and this node's runtime is process"
        );

        let id = InstanceId("default__tool-0-init-1".to_string());
        let init: crate::config::app::InitContainerSpec =
            toml::from_str("command = [\"echo\", \"migrating\"]").unwrap();
        let spec = crate::grill::oci::generate_init_oci_spec(&init, &app);
        grill.create(&id, &spec).await.unwrap();
    }

    #[tokio::test]
    async fn owned_create_refuses_an_image_app_before_preparing_a_generation() {
        let root = tempfile::tempdir().unwrap();
        let owners = root.path().join("owners");
        let grill = ProcessGrill::with_owner(owners.clone(), "/bin/false".into());
        let id = InstanceId("default__web-0".to_string());
        let spec = app_spec(
            "web",
            "image = \"nginx:1.27\"\ncommand = [\"sleep\", \"60\"]",
        );

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert!(
            matches!(
                problem,
                super::super::LaunchProblem::NeedsContainerRuntime(_)
            ),
            "{problem:?}"
        );
        assert!(
            !owners.join(&id.0).exists(),
            "a refused launch must not leave a prepared generation behind"
        );
    }

    /// #914: host workloads are what the process runtime is for. An `exec`
    /// app and a `script` app still run.
    #[tokio::test]
    async fn host_exec_and_script_apps_still_run_under_the_process_runtime() {
        let grill = ProcessGrill::new();
        for (name, app) in [
            ("tool", "exec = \"/bin/sleep\"\ncommand = [\"60\"]"),
            ("job", "script = \"sleep 60\""),
        ] {
            let id = InstanceId(format!("default__{name}-0"));
            let spec = app_spec(name, app);
            assert!(spec.host_process, "{name} must be a host workload");

            grill.create(&id, &spec).await.unwrap();
            grill.start(&id).await.unwrap();
            assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Running);
            grill.kill(&id).await.unwrap();
        }
    }

    /// #914: the `sleep 86400` stand-in is gone. A host spec with nothing to
    /// run is refused, not kept alive doing nothing.
    #[tokio::test]
    async fn create_refuses_a_host_spec_with_no_command() {
        let grill = ProcessGrill::new();
        let id = InstanceId("empty-0".to_string());
        let mut spec = spec_with_args(Vec::new());
        spec.host_process = true;

        let problem = refused_launch(grill.create(&id, &spec).await.unwrap_err());
        assert_eq!(problem, super::super::LaunchProblem::NoCommand);
        assert_eq!(problem.to_string(), "no command to run");
    }
}
