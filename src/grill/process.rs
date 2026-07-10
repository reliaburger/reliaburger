/// Process-based container runtime.
///
/// Implements the `Grill` trait by spawning child processes via
/// `tokio::process::Command`. Each "container" is a child process.
/// Works on macOS and Linux — the cross-platform fallback when
/// neither `runc` nor Apple's `container` CLI is available.
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
use super::records::InstanceRecord;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// A child process managed by ProcessGrill.
struct ProcessEntry {
    spec: OciSpec,
    child: Option<tokio::process::Child>,
    /// Pid of an adopted process (started by a previous bun). Mutually
    /// exclusive with `child`: adopted processes have no handle, only a pid.
    adopted_pid: Option<u32>,
    state: ContainerState,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    /// Base path for file-backed logs (`{stem}.stdout` / `{stem}.stderr`).
    log_stem: Option<PathBuf>,
    exit_code: Option<i32>,
}

use super::records::poll_adopted_process;

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
}

impl ProcessGrill {
    /// Create a new ProcessGrill with in-memory log capture.
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: None,
        }
    }

    /// Create a ProcessGrill that writes workload output to files under
    /// `log_dir`, enabling adoption across bun restarts and upgrades.
    pub fn with_log_dir(log_dir: PathBuf) -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: Some(log_dir),
        }
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
                adopted_pid: None,
                state: ContainerState::Pending,
                stdout_buf: Arc::new(Mutex::new(Vec::new())),
                stderr_buf: Arc::new(Mutex::new(Vec::new())),
                log_stem: None,
                exit_code: None,
            },
        );
        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let mut procs = self.processes.lock().await;
        let entry = procs
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;

        if entry.child.is_some() || entry.adopted_pid.is_some() {
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
        if effective_args.len() > 1 {
            cmd.args(&effective_args[1..]);
        }

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
        let mut procs = self.processes.lock().await;
        let entry = procs
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;

        let pid = entry
            .child
            .as_ref()
            .and_then(|c| c.id())
            .or(entry.adopted_pid);
        if let Some(pid) = pid {
            let pid = nix::unistd::Pid::from_raw(pid as i32);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        }
        entry.state = ContainerState::Stopping;
        Ok(())
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let mut procs = self.processes.lock().await;
        let entry = procs
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;

        if let Some(ref mut child) = entry.child {
            let _ = child.kill().await;
        } else if let Some(pid) = entry.adopted_pid {
            let pid = nix::unistd::Pid::from_raw(pid as i32);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
        }
        entry.state = ContainerState::Stopped;
        Ok(())
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        let mut procs = self.processes.lock().await;
        let entry = procs
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;

        // Check if the process has exited
        if let Some(ref mut child) = entry.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = status.code();
                }
                Ok(None) => {
                    // Still running — keep current state
                }
                Err(_) => {
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = None;
                }
            }
        } else if let Some(pid) = entry.adopted_pid {
            // Adopted process: no handle, poll (and reap) by pid. This
            // doubles as the zombie reaper — the supervisor polls state
            // regularly, so exited adoptees get waitpid'd here.
            if entry.state != ContainerState::Stopped {
                let (running, exit_code) = poll_adopted_process(pid);
                if !running {
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = exit_code;
                }
            }
        }

        Ok(entry.state)
    }

    async fn adopt(
        &self,
        instance: &InstanceId,
        record: &InstanceRecord,
    ) -> Result<bool, GrillError> {
        if !super::records::is_live(record) {
            return Ok(false);
        }
        let mut procs = self.processes.lock().await;
        procs.insert(
            instance.clone(),
            ProcessEntry {
                spec: record.oci_spec.clone(),
                child: None,
                adopted_pid: Some(record.pid),
                state: ContainerState::Running,
                stdout_buf: Arc::new(Mutex::new(Vec::new())),
                stderr_buf: Arc::new(Mutex::new(Vec::new())),
                log_stem: record.log_stem.clone(),
                exit_code: None,
            },
        );
        Ok(true)
    }

    async fn pid(&self, instance: &InstanceId) -> Option<u32> {
        let procs = self.processes.lock().await;
        let entry = procs.get(instance)?;
        entry
            .child
            .as_ref()
            .and_then(|c| c.id())
            .or(entry.adopted_pid)
    }

    async fn log_stem(&self, instance: &InstanceId) -> Option<PathBuf> {
        let procs = self.processes.lock().await;
        procs.get(instance).and_then(|e| e.log_stem.clone())
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
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
        {
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
        lines_tx: tokio::sync::mpsc::Sender<String>,
    ) {
        // Snapshot how this instance's logs are captured.
        let (stdout_buf, log_stem) = {
            let procs = self.processes.lock().await;
            match procs.get(instance) {
                Some(entry) => (entry.stdout_buf.clone(), entry.log_stem.clone()),
                None => return,
            }
        };

        let mut offset = 0usize;
        let mut partial_line = String::new();

        loop {
            // New bytes since the last poll, from the file or the buffer.
            let new_data = if let Some(stem) = &log_stem {
                let contents = std::fs::read(log_file(stem, "stdout")).unwrap_or_default();
                if offset < contents.len() {
                    let data = contents[offset..].to_vec();
                    offset = contents.len();
                    Some(data)
                } else {
                    None
                }
            } else {
                let buf = stdout_buf.lock().await;
                if offset < buf.len() {
                    let data = buf[offset..].to_vec();
                    offset = buf.len();
                    Some(data)
                } else {
                    None
                }
            };

            let no_new_data = new_data.is_none();
            if let Some(data) = new_data {
                partial_line.push_str(&String::from_utf8_lossy(&data));

                // Send all complete lines
                while let Some(newline_pos) = partial_line.find('\n') {
                    let line = partial_line[..newline_pos].to_string();
                    partial_line = partial_line[newline_pos + 1..].to_string();
                    if lines_tx.send(line).await.is_err() {
                        return;
                    }
                }
            }

            // Check if the process has exited and no more data is coming
            {
                let procs = self.processes.lock().await;
                if let Some(entry) = procs.get(instance) {
                    let exited = entry.state == ContainerState::Stopped
                        || entry.state == ContainerState::Stopping;
                    if exited && no_new_data {
                        if !partial_line.is_empty() {
                            let _ = lines_tx.send(std::mem::take(&mut partial_line)).await;
                        }
                        return;
                    }
                } else {
                    return;
                }
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

    fn record_for(instance: &InstanceId, pid: u32, started_at: u64) -> InstanceRecord {
        InstanceRecord {
            schema: 1,
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

        // After SIGTERM, process should exit (sleep responds to signals)
        // Give it a moment
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let state = grill.state(&id).await.unwrap();
        assert!(
            state == ContainerState::Stopped || state == ContainerState::Stopping,
            "expected stopped or stopping, got {state}"
        );
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

        // Wait for the short-lived process to exit
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let state = grill.state(&id).await.unwrap();
        assert_eq!(state, ContainerState::Stopped);
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
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let logged = grill.logs(&id).await.unwrap();
        assert!(logged.contains("to file"), "got {logged:?}");
        assert!(dir.path().join("test-0.stdout").is_file());
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

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
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
