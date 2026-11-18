//! ProcessManager — process workloads with isolation.
//!
//! Wraps `ProcessGrill` with binary allowlist validation. The Grill trait
//! stays clean — ProcessManager adds the policy on top. An inline script
//! runs as `/bin/sh -c` through the same allowlist, with no file written.

use std::path::{Path, PathBuf};

use crate::config::process_workloads::ProcessWorkloadsConfig;

/// Manages the lifecycle of process workloads (exec/script).
///
/// Validates binaries against the allowlist. Does not implement `Grill`
/// directly — the supervisor calls `prepare_exec` to get a command + args,
/// then passes them to ProcessGrill.
#[derive(Debug, Clone)]
pub struct ProcessManager {
    config: ProcessWorkloadsConfig,
}

/// The interpreter host scripts run through. An inline `script` workload is
/// host execution of this binary, so it must appear in the allowlist just
/// like any `exec` binary before a script may run.
pub const SCRIPT_INTERPRETER: &str = "/bin/sh";

/// A prepared process workload, ready to be passed to ProcessGrill.
#[derive(Debug, Clone)]
pub struct PreparedWorkload {
    /// The binary to execute.
    pub binary: PathBuf,
    /// Arguments to pass.
    pub args: Vec<String>,
}

/// Errors from process workload operations.
#[derive(Debug, thiserror::Error)]
pub enum ProcessWorkloadError {
    #[error("binary {path:?} is not in the allowlist")]
    BinaryNotAllowed { path: PathBuf },

    #[error("binary {path:?} not found")]
    BinaryNotFound { path: PathBuf },
}

impl ProcessManager {
    /// Create a new ProcessManager with the given config.
    pub fn new(config: ProcessWorkloadsConfig) -> Self {
        Self { config }
    }

    /// Prepare an exec workload (host binary).
    ///
    /// Enforces the deny-by-default allowlist: the binary runs only when its
    /// absolute path is named in `node.toml`. Returns a `PreparedWorkload`
    /// with the binary path and no temp file.
    pub fn prepare_exec(&self, binary: &Path) -> Result<PreparedWorkload, ProcessWorkloadError> {
        if !self.config.is_binary_allowed(binary) {
            return Err(ProcessWorkloadError::BinaryNotAllowed {
                path: binary.to_path_buf(),
            });
        }

        Ok(PreparedWorkload {
            binary: binary.to_path_buf(),
            args: Vec::new(),
        })
    }

    /// Whether inline scripts may run on this node: true only when the
    /// interpreter (`/bin/sh`) is in the allowlist. Lets the supervisor
    /// refuse a script deploy up front.
    pub fn is_script_allowed(&self) -> bool {
        self.config
            .is_binary_allowed(std::path::Path::new(SCRIPT_INTERPRETER))
    }

    /// Get the process workloads config.
    pub fn config(&self) -> &ProcessWorkloadsConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with_allowlist(binaries: Vec<PathBuf>) -> ProcessManager {
        ProcessManager::new(ProcessWorkloadsConfig {
            allowed_binaries: binaries,
            mount_isolation: false,
        })
    }

    #[test]
    fn prepare_exec_allowed_binary() {
        let mgr = manager_with_allowlist(vec![PathBuf::from("/usr/bin/python3")]);
        let workload = mgr.prepare_exec(Path::new("/usr/bin/python3")).unwrap();
        assert_eq!(workload.binary, PathBuf::from("/usr/bin/python3"));
        assert!(workload.args.is_empty());
    }

    #[test]
    fn prepare_exec_empty_allowlist_denies_all() {
        // Deny-by-default: an empty allowlist refuses every host binary.
        let mgr = manager_with_allowlist(vec![]);
        let result = mgr.prepare_exec(Path::new("/any/binary"));
        assert!(matches!(
            result,
            Err(ProcessWorkloadError::BinaryNotAllowed { .. })
        ));
    }

    #[test]
    fn prepare_exec_rejected_by_allowlist() {
        let mgr = manager_with_allowlist(vec![PathBuf::from("/usr/bin/python3")]);
        let result = mgr.prepare_exec(Path::new("/usr/bin/ruby"));
        assert!(matches!(
            result,
            Err(ProcessWorkloadError::BinaryNotAllowed { .. })
        ));
    }
}
