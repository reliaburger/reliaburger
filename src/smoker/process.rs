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

/// Errors from process fault injection.
#[derive(Debug, thiserror::Error)]
pub enum ProcessFaultError {
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

    #[cfg(unix)]
    #[test]
    fn resume_nonexistent_process_returns_error() {
        let result = resume_process(999_999_999);
        assert!(result.is_err());
    }
}
