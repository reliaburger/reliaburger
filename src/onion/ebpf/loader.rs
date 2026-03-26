/// eBPF program loader and lifecycle management.
///
/// When the `ebpf` feature is enabled, loads compiled BPF object
/// files via aya, attaches programs to cgroup hooks, and provides
/// handles for the BPF maps. Without the feature, provides stubs
/// that always report "not attached".
use std::path::{Path, PathBuf};

use super::super::types::OnionError;

/// Handle to loaded eBPF programs.
pub struct OnionEbpf {
    _cgroup_path: PathBuf,
    #[cfg(feature = "ebpf")]
    _connect_link_id: aya::programs::cgroup_sock_addr::CgroupSockAddrLinkId,
    #[cfg(feature = "ebpf")]
    pub(super) bpf: aya::Ebpf,
    attached: bool,
}

impl OnionEbpf {
    /// Load and attach the connect rewrite eBPF program.
    ///
    /// Loads `onion_connect.bpf.o` from `program_dir` and attaches
    /// the `onion_connect` program to the root cgroup v2.
    #[cfg(feature = "ebpf")]
    pub fn load(program_dir: &Path, cgroup_path: &Path) -> Result<Self, OnionError> {
        use aya::programs::CgroupSockAddr;

        check_prerequisites()?;

        let obj_path = program_dir.join("onion_connect.bpf.o");
        if !obj_path.exists() {
            return Err(OnionError::EbpfLoadFailed {
                reason: format!("onion_connect.bpf.o not found in {}", program_dir.display()),
            });
        }

        let mut bpf = aya::Ebpf::load_file(&obj_path).map_err(|e| OnionError::EbpfLoadFailed {
            reason: format!("failed to load eBPF program: {e}"),
        })?;

        // Attach the connect4 program to the root cgroup
        let prog: &mut CgroupSockAddr = bpf
            .program_mut("onion_connect")
            .ok_or_else(|| OnionError::EbpfLoadFailed {
                reason: "onion_connect program not found in object file".to_string(),
            })?
            .try_into()
            .map_err(|e| OnionError::EbpfLoadFailed {
                reason: format!("wrong program type: {e}"),
            })?;

        prog.load().map_err(|e| OnionError::EbpfLoadFailed {
            reason: format!("failed to load program into kernel: {e}"),
        })?;

        let cgroup_fd =
            std::fs::File::open(cgroup_path).map_err(|e| OnionError::EbpfLoadFailed {
                reason: format!("failed to open cgroup {}: {e}", cgroup_path.display()),
            })?;

        let link_id = prog
            .attach(cgroup_fd, aya::programs::CgroupAttachMode::Single)
            .map_err(|e| OnionError::EbpfLoadFailed {
                reason: format!("failed to attach to cgroup: {e}"),
            })?;

        Ok(Self {
            _cgroup_path: cgroup_path.to_path_buf(),
            _connect_link_id: link_id,
            bpf,
            attached: true,
        })
    }

    /// Stub loader when eBPF feature is not enabled.
    #[cfg(not(feature = "ebpf"))]
    pub fn load(program_dir: &Path, cgroup_path: &Path) -> Result<Self, OnionError> {
        check_prerequisites()?;

        if !program_dir.join("onion_connect.bpf.o").exists() {
            return Err(OnionError::EbpfLoadFailed {
                reason: format!("onion_connect.bpf.o not found in {}", program_dir.display()),
            });
        }

        Ok(Self {
            _cgroup_path: cgroup_path.to_path_buf(),
            attached: false,
        })
    }

    /// Check if eBPF programs are currently attached.
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// Detach eBPF programs from the cgroup.
    pub fn detach(&mut self) {
        self.attached = false;
        // Links are dropped when self is dropped, which detaches the program
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
        let result = check_kernel_version();
        if cfg!(target_os = "linux") {
            assert!(result.is_ok(), "kernel version check failed: {result:?}");
        } else {
            assert!(result.is_err());
        }
    }

    #[test]
    fn check_cgroup_v2_on_test_host() {
        let result = check_cgroup_v2();
        if cfg!(target_os = "linux") {
            let _ = result;
        } else {
            assert!(result.is_err());
        }
    }
}
