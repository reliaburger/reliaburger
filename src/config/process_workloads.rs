//! Process workloads configuration.
//!
//! Controls which host binaries are allowed to run as workloads
//! and what isolation is applied.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Configuration for process workloads (exec/script apps and jobs).
///
/// Process workloads run host binaries or inline scripts as first-class
/// workloads, in a mount view that hides Bun's paths unless
/// `mount_isolation` is off.
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

    /// Start host workloads in a mount view of their own (Linux only, #714).
    ///
    /// When true, every `exec` and `script` app, host job and native host
    /// executor sees the host's root minus Bun's own paths, with a private
    /// `/tmp`, `/var/tmp` and `/dev/shm`, and runs without `CAP_SYS_ADMIN`,
    /// `CAP_SYS_PTRACE`, `CAP_SYS_MODULE`, `CAP_BPF` and `CAP_SYS_BOOT`
    /// (see [`crate::grill::mount_view`]). A node that can't build the view
    /// (not Linux, or a non-root Bun) refuses host workloads instead and
    /// doesn't advertise them. On by default on Linux; a host workload that
    /// mounts, traces or loads BPF needs it `false`.
    #[serde(skip_serializing_if = "is_default_mount_isolation")]
    pub mount_isolation: bool,
}

// Both defaults depend on the machine that reads the file. When relish on a
// Mac writes a Linux guest's node.toml, writing them out would pin the Mac's
// temp directory and its "no mount isolation" onto the guest. Leaving them
// out lets each node compute its own.
fn is_default_mount_isolation(value: &bool) -> bool {
    *value == ProcessWorkloadsConfig::default().mount_isolation
}

// Not derived: `mount_isolation` defaults to true on Linux, which clippy on
// another platform can't see.
#[allow(clippy::derivable_impls)]
impl Default for ProcessWorkloadsConfig {
    fn default() -> Self {
        Self {
            allowed_binaries: Vec::new(),
            mount_isolation: cfg!(target_os = "linux"),
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
        "#;
        let config: ProcessWorkloadsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.allowed_binaries.len(), 2);
        assert!(config.mount_isolation);
    }

    #[test]
    fn parses_empty_toml() {
        let config: ProcessWorkloadsConfig = toml::from_str("").unwrap();
        assert!(config.allowed_binaries.is_empty());
    }

    #[test]
    fn host_dependent_defaults_are_left_for_the_reading_node() {
        let written = toml::to_string(&ProcessWorkloadsConfig::default()).unwrap();
        assert!(!written.contains("mount_isolation"), "{written}");
        let explicit = ProcessWorkloadsConfig {
            mount_isolation: !ProcessWorkloadsConfig::default().mount_isolation,
            ..Default::default()
        };
        let written = toml::to_string(&explicit).unwrap();
        let back: ProcessWorkloadsConfig = toml::from_str(&written).unwrap();
        assert_eq!(back.mount_isolation, explicit.mount_isolation);
    }
}
