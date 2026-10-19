//! Process-level fault injection via Unix signals.
//!
//! These faults send signals directly to a container's main process.
//! They work on all Unix platforms (no eBPF or cgroups required).

/// Send SIGKILL to a process, simulating a crash.
///
/// The Bun supervisor will detect the death and trigger the normal
/// restart/reschedule logic.
#[cfg(unix)]
pub fn kill_process(pid: i32) -> Result<(), ProcessFaultError> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .map_err(|e| ProcessFaultError::SignalFailed {
        signal: "SIGKILL",
        pid,
        source: e,
    })
}

/// Send SIGSTOP to freeze a process.
///
/// Health checks will fail after the configured timeout, triggering
/// the restart/reschedule logic. Use `resume_process` to unfreeze.
#[cfg(unix)]
pub fn pause_process(pid: i32) -> Result<(), ProcessFaultError> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGSTOP,
    )
    .map_err(|e| ProcessFaultError::SignalFailed {
        signal: "SIGSTOP",
        pid,
        source: e,
    })
}

/// Send SIGCONT to resume a previously paused process.
#[cfg(unix)]
pub fn resume_process(pid: i32) -> Result<(), ProcessFaultError> {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGCONT,
    )
    .map_err(|e| ProcessFaultError::SignalFailed {
        signal: "SIGCONT",
        pid,
        source: e,
    })
}

/// Send SIGCONT to every process in a cgroup, so none an earlier Bun's
/// pause fault froze stays frozen. Returns how many were signalled.
///
/// A cgroup without a `cgroup.procs` file (the workload has gone, or it
/// never had a cgroup of its own) has nothing to resume. A process that
/// exits between the read and the signal is skipped.
#[cfg(unix)]
pub fn resume_cgroup_processes(cgroup_path: &std::path::Path) -> Result<usize, ProcessFaultError> {
    let procs = cgroup_path.join("cgroup.procs");
    let listed = match std::fs::read_to_string(&procs) {
        Ok(listed) => listed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(ProcessFaultError::ReadProcesses {
                path: procs,
                source,
            });
        }
    };
    let mut resumed = 0;
    for pid in listed
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
    {
        if resume_process(pid).is_ok() {
            resumed += 1;
        }
    }
    Ok(resumed)
}

/// Errors from process fault injection.
#[derive(Debug, thiserror::Error)]
pub enum ProcessFaultError {
    #[error("failed to read {path}: {source}")]
    ReadProcesses {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to send {signal} to pid {pid}: {source}")]
    SignalFailed {
        signal: &'static str,
        pid: i32,
        #[source]
        source: nix::Error,
    },

    #[error("process {pid} not found")]
    ProcessNotFound { pid: i32 },
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    #[test]
    fn kill_nonexistent_process_returns_error() {
        // PID 0 is special (process group), use a very high PID unlikely to exist
        let result = kill_process(999_999_999);
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn pause_nonexistent_process_returns_error() {
        let result = pause_process(999_999_999);
        assert!(result.is_err());
    }

    /// Whether `ps` reports `pid` as stopped (state `T`).
    #[cfg(unix)]
    fn is_stopped(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with('T')
    }

    /// Wait up to five seconds for `pid` to reach the `stopped` state.
    #[cfg(unix)]
    fn wait_until_stopped(pid: u32, stopped: bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while is_stopped(pid) != stopped {
            assert!(
                std::time::Instant::now() < deadline,
                "pid {pid} never became stopped={stopped}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn resuming_a_cgroup_continues_every_stopped_process_it_lists() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        pause_process(child.id() as i32).unwrap();
        wait_until_stopped(child.id(), true);
        // A plain directory stands in for the cgroup: only `cgroup.procs`
        // is read.
        let cgroup = tempfile::tempdir().unwrap();
        std::fs::write(
            cgroup.path().join("cgroup.procs"),
            format!("{}\n999999999\n", child.id()),
        )
        .unwrap();

        let resumed = resume_cgroup_processes(cgroup.path()).unwrap();

        wait_until_stopped(child.id(), false);
        assert_eq!(resumed, 1, "the exited pid is skipped, not counted");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_cgroup_without_processes_has_nothing_to_resume() {
        let cgroup = tempfile::tempdir().unwrap();
        assert_eq!(resume_cgroup_processes(cgroup.path()).unwrap(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn resume_nonexistent_process_returns_error() {
        let result = resume_process(999_999_999);
        assert!(result.is_err());
    }
}
