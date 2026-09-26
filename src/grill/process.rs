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
use super::process_owner::OwnerPhase;
use super::records::{self, InstanceRecord};
use super::state::ContainerState;
use super::{GrillError, InstanceId};

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
    }
    Ok(())
}

/// Signal the group while the unreaped Child still owns its process identifier.
fn signal_child_group(pid: u32, signal: nix::sys::signal::Signal) -> std::io::Result<()> {
    #[cfg(unix)]
    let pid = nix::unistd::Pid::from_raw(-(pid as i32));
    #[cfg(not(unix))]
    let pid = nix::unistd::Pid::from_raw(pid as i32);
    match nix::sys::signal::kill(pid, signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(error.into()),
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
}

impl ProcessGrill {
    /// Create a new ProcessGrill with in-memory log capture.
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: None,
            control: None,
        }
    }

    /// Create a ProcessGrill that writes workload output to files under
    /// `log_dir`, enabling adoption across bun restarts and upgrades.
    pub fn with_log_dir(log_dir: PathBuf) -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: Some(log_dir),
            control: None,
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
        }
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
            return tokio::task::spawn_blocking(move || match std::fs::read(path) {
                Ok(bytes) => Ok(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(error) => Err(error),
            })
            .await
            .map_err(|error| owner_error(instance, error))?
            .map_err(|error| owner_error(instance, error));
        }
        let procs = self.processes.lock().await;
        let entry = procs.get(instance).ok_or_else(|| GrillError::NotFound {
            instance: instance.clone(),
        })?;
        if let Some(stem) = &entry.log_stem {
            let path = log_file(stem, if stdout { "stdout" } else { "stderr" });
            return Ok(std::fs::read(path).unwrap_or_default());
        }
        let buf = if stdout {
            entry.stdout_buf.lock().await
        } else {
            entry.stderr_buf.lock().await
        };
        Ok(buf.clone())
    }
}

impl Default for ProcessGrill {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Grill for ProcessGrill {
    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
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
            },
        );
        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        if let Some(control) = &self.control {
            return control
                .start(instance)
                .await
                .map_err(|error| GrillError::StartFailed {
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

        if entry.child.is_some() || entry.adopted.is_some() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "already started".to_string(),
            });
        }

        let args = &entry.spec.process.args;

        // If no command specified, use a long sleep as a placeholder.
        // Real containers get their entrypoint from the image; ProcessGrill
        // doesn't have images, so we fall back to keeping the process alive.
        let default_args;
        let effective_args = if args.is_empty() {
            default_args = vec!["sleep".to_string(), "86400".to_string()];
            &default_args
        } else {
            args
        };

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

        // Set environment variables from OCI spec
        for env_str in &entry.spec.process.env {
            if let Some((key, value)) = env_str.split_once('=') {
                cmd.env(key, value);
            }
        }

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

        let mut child = cmd.spawn().map_err(|e| GrillError::StartFailed {
            instance: instance.clone(),
            reason: e.to_string(),
        })?;

        // Spawn tasks to capture stdout/stderr (in-memory mode only —
        // file-backed mode has no pipes to read).
        let stdout_buf = entry.stdout_buf.clone();
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let mut reader = stdout;
                let mut buf = vec![0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut out = stdout_buf.lock().await;
                            out.extend_from_slice(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        let stderr_buf = entry.stderr_buf.clone();
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = stderr;
                let mut buf = vec![0u8; 4096];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let mut out = stderr_buf.lock().await;
                            out.extend_from_slice(&buf[..n]);
                        }
                        Err(_) => break,
                    }
                }
            });
        }

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
        if let Some(pid) = entry.child.as_ref().and_then(|child| child.id()) {
            signal_child_group(pid, nix::sys::signal::Signal::SIGTERM).map_err(error)?;
            entry.state = ContainerState::Stopping;
        } else if entry.adopted.is_some() {
            entry.state = if signal_adopted_process(entry, nix::sys::signal::Signal::SIGTERM)
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
        if let Some(ref mut child) = entry.child {
            if let Some(pid) = child.id() {
                signal_child_group(pid, nix::sys::signal::Signal::SIGKILL).map_err(error)?;
            }
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
        if let Some(control) = &self.control {
            let record = control
                .status(instance)
                .await
                .map_err(|error| owner_error(instance, error))?;
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
            },
        );
        Ok(true)
    }

    async fn pid(&self, instance: &InstanceId) -> Option<u32> {
        if let Some(control) = &self.control {
            return match control.status(instance).await.ok()?.phase {
                OwnerPhase::Running { pid } => Some(pid),
                _ => None,
            };
        }
        let procs = self.processes.lock().await;
        let entry = procs.get(instance)?;
        entry
            .child
            .as_ref()
            .and_then(|c| c.id())
            .or(entry.adopted.map(|adopted| adopted.pid))
    }

    async fn log_stem(&self, instance: &InstanceId) -> Option<PathBuf> {
        if let Some(control) = &self.control {
            return control.log_stem(instance).ok();
        }
        let procs = self.processes.lock().await;
        procs.get(instance).and_then(|e| e.log_stem.clone())
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        if let Some(control) = &self.control {
            return match control.status(instance).await.ok()?.phase {
                OwnerPhase::Retired { exit_code } => exit_code,
                _ => None,
            };
        }
        let procs = self.processes.lock().await;
        let entry = procs.get(instance)?;
        entry.exit_code
    }

    async fn logs(&self, instance: &InstanceId) -> Result<String, GrillError> {
        let stdout = self.stdout(instance).await?;
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    async fn exec(&self, instance: &InstanceId, command: &[String]) -> Result<String, GrillError> {
        // Verify the instance exists and is running
        if let Some(control) = &self.control {
            return control
                .exec(instance, command)
                .await
                .map_err(|error| owner_error(instance, error));
        } else {
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
        }

        if command.is_empty() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "no command specified".to_string(),
            });
        }

        // Spawn the command directly (no namespace entry for ProcessGrill)
        let output = Command::new(&command[0])
            .args(&command[1..])
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
        Ok(result)
    }

    async fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: tokio::sync::mpsc::Sender<crate::ketchup::types::CapturedLine>,
    ) {
        // Snapshot how this instance's logs are captured.
        let (stdout_buf, log_stem) = if let Some(control) = &self.control {
            (
                Arc::new(Mutex::new(Vec::new())),
                control.log_stem(instance).ok(),
            )
        } else {
            let procs = self.processes.lock().await;
            match procs.get(instance) {
                Some(entry) => (entry.stdout_buf.clone(), entry.log_stem.clone()),
                None => return,
            }
        };

        let mut reader = crate::grill::capture::CaptureReader::new(
            crate::ketchup::types::LogStream::Stdout,
            log_stem.as_ref().map(|stem| log_file(stem, "stdout")),
        );

        loop {
            // New bytes since the last poll, from the file or the buffer.
            let offset = usize::try_from(reader.read_offset()).unwrap_or(usize::MAX);
            let new_data = if let Some(file) = reader.file() {
                let contents = std::fs::read(file).unwrap_or_default();
                contents.get(offset..).unwrap_or_default().to_vec()
            } else {
                let buf = stdout_buf.lock().await;
                buf.get(offset..).unwrap_or_default().to_vec()
            };

            let no_new_data = new_data.is_empty();
            for line in reader.push(&new_data) {
                if lines_tx.send(line).await.is_err() {
                    return;
                }
            }

            // Check if the process has exited and no more data is coming
            let exited = if self.control.is_some() {
                match self.state(instance).await {
                    Ok(ContainerState::Stopped) => true,
                    Err(_) => return,
                    _ => false,
                }
            } else {
                let procs = self.processes.lock().await;
                let Some(entry) = procs.get(instance) else {
                    return;
                };
                entry.state == ContainerState::Stopped || entry.state == ContainerState::Stopping
            };
            if exited && no_new_data {
                if let Some(line) = reader.finish() {
                    let _ = lines_tx.send(line).await;
                }
                return;
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::Grill;
    use crate::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
    use crate::grill::records::{self, InstanceRecord, RuntimeKind};

    fn spec_with_args(args: Vec<String>) -> OciSpec {
        OciSpec {
            port_mapping: None,
            root: OciRoot {
                path: "/tmp/test".to_string(),
                readonly: false,
            },
            process: OciProcess {
                args,
                env: vec!["TEST_VAR=hello".to_string()],
                cwd: "/".to_string(),
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
            schema: 2,
            instance_id: instance.0.clone(),
            namespace: "default".to_string(),
            app_name: "test".to_string(),
            replica_index: 0,
            is_job: false,
            image: String::new(),
            runtime: RuntimeKind::Process,
            pid,
            pid_started_at: started_at,
            runc_container_id: None,
            log_stem: None,
            host_port: None,
            app_spec: None,
            oci_spec: sleep_spec("60"),
            rootless_network: None,
        }
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
                let pid = grill.pid(&id).await.unwrap();
                // WNOWAIT observes exit without reaping the child, preserving
                // the exact zombie-group state that macOS refuses to signal.
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        let mut info = std::mem::MaybeUninit::<nix::libc::siginfo_t>::zeroed();
                        // SAFETY: pid belongs to the child just spawned above;
                        // info points to correctly sized, initialised storage.
                        // WNOHANG bounds the call and WNOWAIT preserves ownership.
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
                assert_eq!(grill.exit_code(&id).await, Some(exit));
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
        let script = format!("sleep 60 & echo $! > {}; wait", pid_file.display());
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
                if let Ok(contents) = std::fs::read_to_string(&pid_file) {
                    break contents.trim().parse::<u32>().unwrap();
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

    #[cfg(unix)]
    #[tokio::test]
    async fn panicking_owner_terminates_in_memory_process_tree() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let script = format!("sleep 60 & echo $! > {}; wait", pid_file.display());
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
        let task = tokio::spawn(async move { follower.follow_logs(&follow_id, sender).await });
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
        let task = tokio::spawn(async move { follower.follow_logs(&follow_id, sender).await });
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

    fn printf_spec(output: &str) -> OciSpec {
        spec_with_args(vec!["printf".to_string(), output.to_string()])
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
        assert_eq!(grill.pid(&id).await, Some(pid));

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
        let pid = grill.pid(&id).await.unwrap();
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
        assert_eq!(grill.exit_code(&id).await, Some(0));
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
}
