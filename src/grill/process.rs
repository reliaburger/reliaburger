/// Process-based container runtime.
///
/// Implements the `Grill` trait by spawning child processes via
/// `tokio::process::Command`. Each "container" is a child process.
/// Works on macOS and Linux — the cross-platform fallback when
/// neither `runc` nor Apple's `container` CLI is available.
use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use super::oci::OciSpec;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// A child process managed by ProcessGrill.
struct ProcessEntry {
    spec: OciSpec,
    child: Option<tokio::process::Child>,
    state: ContainerState,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    exit_code: Option<i32>,
}

/// Process-based Grill implementation.
///
/// Spawns OS processes instead of OCI containers. Useful for
/// development, testing, and platforms without container runtimes.
pub struct ProcessGrill {
    processes: Arc<Mutex<HashMap<InstanceId, ProcessEntry>>>,
}

impl ProcessGrill {
    /// Create a new ProcessGrill.
    pub fn new() -> Self {
        Self {
            processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get captured stdout for an instance.
    pub async fn stdout(&self, instance: &InstanceId) -> Result<Vec<u8>, GrillError> {
        let procs = self.processes.lock().await;
        let entry = procs.get(instance).ok_or_else(|| GrillError::NotFound {
            instance: instance.clone(),
        })?;
        let buf = entry.stdout_buf.lock().await;
        Ok(buf.clone())
    }

    /// Get captured stderr for an instance.
    pub async fn stderr(&self, instance: &InstanceId) -> Result<Vec<u8>, GrillError> {
        let procs = self.processes.lock().await;
        let entry = procs.get(instance).ok_or_else(|| GrillError::NotFound {
            instance: instance.clone(),
        })?;
        let buf = entry.stderr_buf.lock().await;
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
                state: ContainerState::Pending,
                stdout_buf: Arc::new(Mutex::new(Vec::new())),
                stderr_buf: Arc::new(Mutex::new(Vec::new())),
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

        if entry.child.is_some() {
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

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| GrillError::StartFailed {
            instance: instance.clone(),
            reason: e.to_string(),
        })?;

        // Spawn tasks to capture stdout/stderr
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

        if let Some(ref child) = entry.child
            && let Some(pid) = child.id()
        {
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
        }

        Ok(entry.state)
    }

    async fn pid(&self, instance: &InstanceId) -> Option<u32> {
        let procs = self.processes.lock().await;
        let entry = procs.get(instance)?;
        entry.child.as_ref().and_then(|c| c.id())
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        let procs = self.processes.lock().await;
        let entry = procs.get(instance)?;
        entry.exit_code
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::Grill;
    use crate::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};

    fn echo_spec(msg: &str) -> OciSpec {
        OciSpec {
            root: OciRoot {
                path: "/tmp/test".to_string(),
                readonly: false,
            },
            process: OciProcess {
                args: vec!["echo".to_string(), msg.to_string()],
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

    fn sleep_spec(secs: &str) -> OciSpec {
        OciSpec {
            root: OciRoot {
                path: "/tmp/test".to_string(),
                readonly: false,
            },
            process: OciProcess {
                args: vec!["sleep".to_string(), secs.to_string()],
                env: vec![],
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
}
