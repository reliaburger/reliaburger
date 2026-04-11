//! ProcessManager — process workloads with isolation.
//!
//! Wraps `ProcessGrill` with binary allowlist validation, script temp
//! file lifecycle, and (on Linux) mount namespace isolation. The Grill
//! trait stays clean — ProcessManager adds isolation on top.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::process_workloads::ProcessWorkloadsConfig;

/// Manages the lifecycle of process workloads (exec/script).
///
/// Validates binaries against the allowlist, creates temp files for
/// inline scripts, and configures isolation. Does not implement `Grill`
/// directly — the supervisor calls `prepare_exec` or `prepare_script`
/// to get a command + args, then passes them to ProcessGrill.
#[derive(Debug, Clone)]
pub struct ProcessManager {
    config: ProcessWorkloadsConfig,
}

/// A prepared process workload, ready to be passed to ProcessGrill.
#[derive(Debug, Clone)]
pub struct PreparedWorkload {
    /// The binary to execute.
    pub binary: PathBuf,
    /// Arguments to pass.
    pub args: Vec<String>,
    /// Temp file to clean up after execution (for scripts).
    pub temp_file: Option<PathBuf>,
}

/// Errors from process workload operations.
#[derive(Debug, thiserror::Error)]
pub enum ProcessWorkloadError {
    #[error("binary {path:?} is not in the allowlist")]
    BinaryNotAllowed { path: PathBuf },

    #[error("binary {path:?} not found")]
    BinaryNotFound { path: PathBuf },

    #[error("failed to create script temp file: {0}")]
    ScriptCreateFailed(std::io::Error),

    #[error("failed to clean up script temp file {path:?}: {source}")]
    ScriptCleanupFailed {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl ProcessManager {
    /// Create a new ProcessManager with the given config.
    pub fn new(config: ProcessWorkloadsConfig) -> Self {
        Self { config }
    }

    /// Prepare an exec workload (host binary).
    ///
    /// Validates the binary against the allowlist. Returns a
    /// `PreparedWorkload` with the binary path and no temp file.
    pub fn prepare_exec(&self, binary: &Path) -> Result<PreparedWorkload, ProcessWorkloadError> {
        // Check allowlist
        if !self.config.is_binary_allowed(binary) {
            return Err(ProcessWorkloadError::BinaryNotAllowed {
                path: binary.to_path_buf(),
            });
        }

        Ok(PreparedWorkload {
            binary: binary.to_path_buf(),
            args: Vec::new(),
            temp_file: None,
        })
    }

    /// Prepare a script workload (inline script content).
    ///
    /// Writes the script to a temp file in the configured script dir,
    /// makes it executable, and returns a `PreparedWorkload` that
    /// executes it via `/bin/sh`. The temp file path is returned in
    /// `PreparedWorkload.temp_file` for cleanup after execution.
    pub fn prepare_script(
        &self,
        script_content: &str,
        name: &str,
    ) -> Result<PreparedWorkload, ProcessWorkloadError> {
        // Ensure script directory exists
        std::fs::create_dir_all(&self.config.script_dir)
            .map_err(ProcessWorkloadError::ScriptCreateFailed)?;

        // Create temp file with a predictable name for debugging
        let script_path = self
            .config
            .script_dir
            .join(format!("reliaburger-{name}-{}.sh", std::process::id()));

        // Write script content
        let mut file = std::fs::File::create(&script_path)
            .map_err(ProcessWorkloadError::ScriptCreateFailed)?;
        file.write_all(script_content.as_bytes())
            .map_err(ProcessWorkloadError::ScriptCreateFailed)?;

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&script_path, perms)
                .map_err(ProcessWorkloadError::ScriptCreateFailed)?;
        }

        Ok(PreparedWorkload {
            binary: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script_path.to_string_lossy().to_string()],
            temp_file: Some(script_path),
        })
    }

    /// Clean up a prepared workload's temp files.
    ///
    /// Call after the workload completes (success or failure).
    pub fn cleanup(&self, workload: &PreparedWorkload) -> Result<(), ProcessWorkloadError> {
        if let Some(ref temp_file) = workload.temp_file
            && temp_file.exists()
        {
            std::fs::remove_file(temp_file).map_err(|e| {
                ProcessWorkloadError::ScriptCleanupFailed {
                    path: temp_file.clone(),
                    source: e,
                }
            })?;
        }
        Ok(())
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
            script_dir: std::env::temp_dir().join("reliaburger-test-scripts"),
        })
    }

    #[test]
    fn prepare_exec_allowed_binary() {
        let mgr = manager_with_allowlist(vec![PathBuf::from("/usr/bin/python3")]);
        let workload = mgr.prepare_exec(Path::new("/usr/bin/python3")).unwrap();
        assert_eq!(workload.binary, PathBuf::from("/usr/bin/python3"));
        assert!(workload.temp_file.is_none());
    }

    #[test]
    fn prepare_exec_empty_allowlist_allows_all() {
        let mgr = manager_with_allowlist(vec![]);
        let workload = mgr.prepare_exec(Path::new("/any/binary")).unwrap();
        assert_eq!(workload.binary, PathBuf::from("/any/binary"));
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

    #[test]
    fn prepare_script_creates_temp_file() {
        let mgr = manager_with_allowlist(vec![]);
        let workload = mgr.prepare_script("echo hello world", "test-app").unwrap();

        assert_eq!(workload.binary, PathBuf::from("/bin/sh"));
        assert_eq!(workload.args[0], "-c");
        assert!(workload.temp_file.is_some());

        let temp_path = workload.temp_file.as_ref().unwrap();
        assert!(temp_path.exists());
        let content = std::fs::read_to_string(temp_path).unwrap();
        assert_eq!(content, "echo hello world");

        // Cleanup
        mgr.cleanup(&workload).unwrap();
        assert!(!temp_path.exists());
    }

    #[test]
    fn prepare_script_temp_file_is_executable() {
        let mgr = manager_with_allowlist(vec![]);
        let workload = mgr
            .prepare_script("#!/bin/sh\necho ok", "exec-test")
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let temp_path = workload.temp_file.as_ref().unwrap();
            let perms = std::fs::metadata(temp_path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o700, 0o700);
        }

        mgr.cleanup(&workload).unwrap();
    }

    #[test]
    fn cleanup_nonexistent_file_succeeds() {
        let mgr = manager_with_allowlist(vec![]);
        let workload = PreparedWorkload {
            binary: PathBuf::from("/bin/sh"),
            args: vec![],
            temp_file: Some(PathBuf::from("/tmp/nonexistent-reliaburger-script")),
        };
        // Should succeed — file doesn't exist, nothing to clean up
        mgr.cleanup(&workload).unwrap();
    }

    #[test]
    fn cleanup_no_temp_file_succeeds() {
        let mgr = manager_with_allowlist(vec![]);
        let workload = PreparedWorkload {
            binary: PathBuf::from("/usr/bin/python3"),
            args: vec![],
            temp_file: None,
        };
        mgr.cleanup(&workload).unwrap();
    }
}
