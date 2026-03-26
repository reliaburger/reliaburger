/// eBPF program loader and lifecycle management.
///
/// Loads compiled BPF object files via aya, attaches programs to
/// cgroup hooks, and provides handles for the BPF maps. Checks
/// kernel prerequisites (version, BTF, cgroup v2) before loading.
use std::path::{Path, PathBuf};

use super::super::types::OnionError;

/// Handle to loaded eBPF programs.
///
/// Holds the aya `Bpf` objects for the connect and DNS programs.
/// Dropping this detaches the programs from the cgroup.
pub struct OnionEbpf {
    /// Path to the cgroup v2 mount point.
    _cgroup_path: PathBuf,
    // TODO(Phase 3, Step 2b): Add aya::Bpf handles when aya
    // dependency is enabled. For now this is a placeholder
    // that compiles without the aya crate.
}

impl OnionEbpf {
    /// Load eBPF programs from compiled object files.
    ///
    /// Looks for `onion_connect.bpf.o` and `onion_dns.bpf.o` in the
    /// given directory (typically `target/bpf/` or the build output dir).
    pub fn load(program_dir: &Path, cgroup_path: &Path) -> Result<Self, OnionError> {
        check_prerequisites()?;

        if !program_dir.join("onion_connect.bpf.o").exists() {
            return Err(OnionError::EbpfLoadFailed {
                reason: format!("onion_connect.bpf.o not found in {}", program_dir.display()),
            });
        }

        // TODO(Phase 3, Step 2b): Load via aya::Bpf::load_file(),
        // attach to cgroup. For now, return a placeholder.
        Ok(Self {
            _cgroup_path: cgroup_path.to_path_buf(),
        })
    }

    /// Check if eBPF programs are currently attached.
    pub fn is_attached(&self) -> bool {
        // TODO(Phase 3, Step 2b): Check via aya program handles
        false
    }

    /// Detach eBPF programs from the cgroup.
    pub fn detach(&mut self) {
        // TODO(Phase 3, Step 2b): Detach via aya
    }
}

/// Check kernel prerequisites for eBPF.
fn check_prerequisites() -> Result<(), OnionError> {
    check_kernel_version()?;
    check_cgroup_v2()?;
    Ok(())
}

/// Verify the kernel version is 5.7+ (required for cgroup socket hooks).
fn check_kernel_version() -> Result<(), OnionError> {
    let version =
        std::fs::read_to_string("/proc/version").map_err(|e| OnionError::EbpfLoadFailed {
            reason: format!("failed to read /proc/version: {e}"),
        })?;

    // Parse "Linux version X.Y.Z ..."
    let parts: Vec<&str> = version.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(OnionError::EbpfLoadFailed {
            reason: "cannot parse kernel version".to_string(),
        });
    }

    let version_str = parts[2];
    let nums: Vec<u32> = version_str
        .split('.')
        .take(2)
        .filter_map(|s| s.parse().ok())
        .collect();

    if nums.len() >= 2 && (nums[0] > 5 || (nums[0] == 5 && nums[1] >= 7)) {
        Ok(())
    } else {
        Err(OnionError::EbpfLoadFailed {
            reason: format!("kernel {version_str} is too old; Onion requires 5.7+"),
        })
    }
}

/// Verify cgroup v2 is mounted.
fn check_cgroup_v2() -> Result<(), OnionError> {
    let cgroup_path = Path::new("/sys/fs/cgroup");
    if !cgroup_path.exists() {
        return Err(OnionError::EbpfLoadFailed {
            reason: "/sys/fs/cgroup does not exist".to_string(),
        });
    }

    // cgroup v2 has a "cgroup.controllers" file at the root
    let controllers = cgroup_path.join("cgroup.controllers");
    if !controllers.exists() {
        return Err(OnionError::EbpfLoadFailed {
            reason: "cgroup v2 not mounted (no cgroup.controllers at /sys/fs/cgroup)".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_kernel_version_on_test_host() {
        // This test just verifies the function doesn't panic.
        // On macOS (where /proc/version doesn't exist), it will
        // return an error, which is expected.
        let result = check_kernel_version();
        if cfg!(target_os = "linux") {
            // On Linux CI, this should pass (kernel 5.7+)
            assert!(result.is_ok(), "kernel version check failed: {result:?}");
        } else {
            // On macOS, /proc/version doesn't exist
            assert!(result.is_err());
        }
    }

    #[test]
    fn check_cgroup_v2_on_test_host() {
        let result = check_cgroup_v2();
        if cfg!(target_os = "linux") {
            // Most modern Linux CI has cgroup v2
            // Don't assert — some CI environments might not
            let _ = result;
        } else {
            assert!(result.is_err());
        }
    }
}
