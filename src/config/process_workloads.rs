//! Process workloads configuration.
//!
//! Controls which host binaries are allowed to run as workloads
//! and what isolation is applied.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration for process workloads (exec/script apps and jobs).
///
/// Process workloads run host binaries or inline scripts as first-class
/// workloads with optional isolation (mount namespace, cgroup limits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProcessWorkloadsConfig {
    /// Binaries allowed to run as process workloads.
    ///
    /// If empty, all binaries are allowed (opt-in restriction).
    /// Paths must be absolute.
    pub allowed_binaries: Vec<PathBuf>,

    /// Enable mount namespace isolation for process workloads (Linux only).
    ///
    /// When enabled, process workloads run in a separate mount namespace
    /// and cannot see `/var/lib/reliaburger` or other workloads' volumes.
    pub mount_isolation: bool,

    /// Directory for temporary script files.
    ///
    /// Inline scripts are written here, made executable, and cleaned up
    /// after execution. Must not be inside any workload-visible path.
    pub script_dir: PathBuf,
}

impl Default for ProcessWorkloadsConfig {
    fn default() -> Self {
        Self {
            allowed_binaries: Vec::new(),
            mount_isolation: cfg!(target_os = "linux"),
            script_dir: std::env::temp_dir().join("reliaburger-scripts"),
        }
    }
}

impl ProcessWorkloadsConfig {
    /// Check whether a binary is allowed by the allowlist.
    ///
    /// Returns `true` if the allowlist is empty (all allowed) or
    /// if the binary path is in the list.
    pub fn is_binary_allowed(&self, binary: &std::path::Path) -> bool {
        self.allowed_binaries.is_empty() || self.allowed_binaries.iter().any(|b| b == binary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allows_all_binaries() {
        let config = ProcessWorkloadsConfig::default();
        assert!(config.allowed_binaries.is_empty());
        assert!(config.is_binary_allowed(std::path::Path::new("/usr/bin/python3")));
        assert!(config.is_binary_allowed(std::path::Path::new("/any/path")));
    }

    #[test]
    fn allowlist_accepts_listed_binary() {
        let config = ProcessWorkloadsConfig {
            allowed_binaries: vec![
                PathBuf::from("/usr/bin/python3"),
                PathBuf::from("/usr/local/bin/node"),
            ],
            ..Default::default()
        };
        assert!(config.is_binary_allowed(std::path::Path::new("/usr/bin/python3")));
        assert!(config.is_binary_allowed(std::path::Path::new("/usr/local/bin/node")));
    }

    #[test]
    fn allowlist_rejects_unlisted_binary() {
        let config = ProcessWorkloadsConfig {
            allowed_binaries: vec![PathBuf::from("/usr/bin/python3")],
            ..Default::default()
        };
        assert!(!config.is_binary_allowed(std::path::Path::new("/usr/bin/ruby")));
        assert!(!config.is_binary_allowed(std::path::Path::new("/tmp/malicious")));
    }

    #[test]
    fn parses_from_toml() {
        let toml_str = r#"
            allowed_binaries = ["/usr/bin/python3", "/usr/local/bin/node"]
            mount_isolation = true
            script_dir = "/tmp/scripts"
        "#;
        let config: ProcessWorkloadsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.allowed_binaries.len(), 2);
        assert!(config.mount_isolation);
        assert_eq!(config.script_dir, PathBuf::from("/tmp/scripts"));
    }

    #[test]
    fn parses_empty_toml() {
        let config: ProcessWorkloadsConfig = toml::from_str("").unwrap();
        assert!(config.allowed_binaries.is_empty());
    }

    #[test]
    fn default_script_dir_is_temp() {
        let config = ProcessWorkloadsConfig::default();
        assert!(
            config
                .script_dir
                .to_string_lossy()
                .contains("reliaburger-scripts")
        );
    }
}
