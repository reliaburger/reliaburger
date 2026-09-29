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
#[serde(default, deny_unknown_fields)]
pub struct ProcessWorkloadsConfig {
    /// Binaries allowed to run as process workloads.
    ///
    /// Host execution is **deny-by-default**: an empty or absent list
    /// refuses every `exec`/`script` workload. A binary runs only when its
    /// absolute path appears here. Container workloads (runc/apple) are
    /// unaffected — this list gates only ProcessGrill host execution.
    pub allowed_binaries: Vec<PathBuf>,

    /// Enable mount namespace isolation for process workloads (Linux only).
    ///
    /// When enabled, process workloads run in a separate mount namespace
    /// and cannot see `/var/lib/reliaburger` or other workloads' volumes.
    #[serde(skip_serializing_if = "is_default_mount_isolation")]
    pub mount_isolation: bool,

    /// Directory for temporary script files.
    ///
    /// Inline scripts are written here, made executable, and cleaned up
    /// after execution. Must not be inside any workload-visible path.
    #[serde(skip_serializing_if = "is_default_script_dir")]
    pub script_dir: PathBuf,
}

// Both defaults depend on the machine that reads the file. When relish on a
// Mac writes a Linux guest's node.toml, writing them out would pin the Mac's
// temp directory and its "no mount isolation" onto the guest. Leaving them
// out lets each node compute its own.
fn is_default_mount_isolation(value: &bool) -> bool {
    *value == ProcessWorkloadsConfig::default().mount_isolation
}

fn is_default_script_dir(value: &std::path::Path) -> bool {
    value == ProcessWorkloadsConfig::default().script_dir
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
    /// Check whether a binary is allowed to run as a host process workload.
    ///
    /// Deny-by-default: an empty allowlist refuses everything, and a
    /// non-empty one admits only an exact path match. Nothing runs on the
    /// host that an operator didn't name in `node.toml`.
    pub fn is_binary_allowed(&self, binary: &std::path::Path) -> bool {
        self.allowed_binaries.iter().any(|b| b == binary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_denies_all_host_binaries() {
        // Deny-by-default: with no allowlist configured, no host binary may
        // run as a process workload. This is the security posture the design
        // doc promised but the code used to invert.
        let config = ProcessWorkloadsConfig::default();
        assert!(config.allowed_binaries.is_empty());
        assert!(!config.is_binary_allowed(std::path::Path::new("/usr/bin/python3")));
        assert!(!config.is_binary_allowed(std::path::Path::new("/any/path")));
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
    fn host_dependent_defaults_are_left_for_the_reading_node() {
        let written = toml::to_string(&ProcessWorkloadsConfig::default()).unwrap();
        assert!(!written.contains("script_dir"), "{written}");
        assert!(!written.contains("mount_isolation"), "{written}");
        let explicit = ProcessWorkloadsConfig {
            script_dir: PathBuf::from("/srv/scripts"),
            mount_isolation: !ProcessWorkloadsConfig::default().mount_isolation,
            ..Default::default()
        };
        let written = toml::to_string(&explicit).unwrap();
        let back: ProcessWorkloadsConfig = toml::from_str(&written).unwrap();
        assert_eq!(back.script_dir, PathBuf::from("/srv/scripts"));
        assert_eq!(back.mount_isolation, explicit.mount_isolation);
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
