/// eBPF program loader and lifecycle management.
///
/// When the `ebpf` feature is enabled, loads compiled BPF object
/// files via aya, attaches programs to cgroup hooks, and provides
/// handles for the BPF maps. Without the feature, provides stubs
/// that always report "not attached".
use std::path::{Path, PathBuf};

use super::super::types::OnionError;

/// Every map the loaded `onion_connect.bpf.o` object must define. A
/// missing map used to surface as a panic at first use, deep inside the
/// agent (NET8); now the load fails up front with the full list.
pub const REQUIRED_MAPS: [&str; 9] = [
    "backend_map",
    "firewall_map",
    "cgroup_namespace_map",
    "egress_map",
    "egress6_map",
    "egress_cidr4_map",
    "egress_cidr6_map",
    "egress_enabled_map",
    "fault_connect_map",
];

/// Every program the object must define.
pub const REQUIRED_PROGRAMS: [&str; 2] = ["onion_connect", "onion_connect6"];

/// Which required names are absent from the loaded object? Pure, so the
/// validation logic is testable without a kernel.
pub fn missing_names(required: &[&'static str], present: &[String]) -> Vec<&'static str> {
    required
        .iter()
        .filter(|name| !present.iter().any(|p| p == *name))
        .copied()
        .collect()
}

/// Handle to loaded eBPF programs.
pub struct OnionEbpf {
    _cgroup_path: PathBuf,
    #[cfg(feature = "ebpf")]
    _connect_link_id: aya::programs::cgroup_sock_addr::CgroupSockAddrLinkId,
    #[cfg(feature = "ebpf")]
    _connect6_link_id: Option<aya::programs::cgroup_sock_addr::CgroupSockAddrLinkId>,
    #[cfg(feature = "ebpf")]
    pub bpf: aya::Ebpf,
    attached: bool,
    connect6_attached: bool,
}

impl OnionEbpf {
    /// Load and attach the connect eBPF programs.
    ///
    /// Loads `onion_connect.bpf.o` from `program_dir`, validates that
    /// every required map and program exists in the object, and attaches
    /// the `onion_connect` (IPv4) and `onion_connect6` (IPv6) programs
    /// to the root cgroup v2. A kernel that refuses the connect6 attach
    /// is logged and reported via `connect6_attached()` — deploys with
    /// an egress allowlist then fail closed rather than run a policy
    /// that IPv6 walks straight past.
    #[cfg(feature = "ebpf")]
    pub fn load(program_dir: &Path, cgroup_path: &Path) -> Result<Self, OnionError> {
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

        // Validate the object in one place instead of panicking at first
        // map use later (NET8).
        let map_names: Vec<String> = bpf.maps().map(|(name, _)| name.to_string()).collect();
        let program_names: Vec<String> = bpf.programs().map(|(name, _)| name.to_string()).collect();
        let mut missing = missing_names(&REQUIRED_MAPS, &map_names);
        missing.extend(missing_names(&REQUIRED_PROGRAMS, &program_names));
        if !missing.is_empty() {
            return Err(OnionError::EbpfLoadFailed {
                reason: format!(
                    "loaded object {} is missing required maps/programs: {}",
                    obj_path.display(),
                    missing.join(", ")
                ),
            });
        }

        let cgroup_fd =
            std::fs::File::open(cgroup_path).map_err(|e| OnionError::EbpfLoadFailed {
                reason: format!("failed to open cgroup {}: {e}", cgroup_path.display()),
            })?;

        // Attach the connect4 program to the root cgroup.
        let link_id = Self::attach_connect_program(&mut bpf, "onion_connect", &cgroup_fd)?;

        // Attach connect6 wherever connect4 attaches. An old kernel
        // refusing the hook keeps the log-and-continue load policy; the
        // agent then refuses deploys that need egress enforcement.
        let (connect6_link_id, connect6_attached) =
            match Self::attach_connect_program(&mut bpf, "onion_connect6", &cgroup_fd) {
                Ok(id) => (Some(id), true),
                Err(e) => {
                    eprintln!(
                        "onion: connect6 attach failed ({e}); IPv6 egress policy unavailable \
                         — deploys with an egress allowlist will be refused"
                    );
                    (None, false)
                }
            };

        Ok(Self {
            _cgroup_path: cgroup_path.to_path_buf(),
            _connect_link_id: link_id,
            _connect6_link_id: connect6_link_id,
            bpf,
            attached: true,
            connect6_attached,
        })
    }

    /// Load and attach one `cgroup/connect*` program by name.
    #[cfg(feature = "ebpf")]
    fn attach_connect_program(
        bpf: &mut aya::Ebpf,
        name: &str,
        cgroup_fd: &std::fs::File,
    ) -> Result<aya::programs::cgroup_sock_addr::CgroupSockAddrLinkId, OnionError> {
        use aya::programs::CgroupSockAddr;

        let prog: &mut CgroupSockAddr = bpf
            .program_mut(name)
            .ok_or_else(|| OnionError::EbpfLoadFailed {
                reason: format!("{name} program not found in object file"),
            })?
            .try_into()
            .map_err(|e| OnionError::EbpfLoadFailed {
                reason: format!("wrong program type for {name}: {e}"),
            })?;

        prog.load().map_err(|e| OnionError::EbpfLoadFailed {
            reason: format!("failed to load {name} into kernel: {e}"),
        })?;

        prog.attach(cgroup_fd, aya::programs::CgroupAttachMode::Single)
            .map_err(|e| OnionError::EbpfLoadFailed {
                reason: format!("failed to attach {name} to cgroup: {e}"),
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
            connect6_attached: false,
        })
    }

    /// Check if eBPF programs are currently attached.
    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// Whether the IPv6 connect hook is attached. Without it, an egress
    /// allowlist is decorative — anything dual-stack bypasses it.
    pub fn connect6_attached(&self) -> bool {
        self.connect6_attached
    }

    /// Detach eBPF programs from the cgroup.
    pub fn detach(&mut self) {
        self.attached = false;
        self.connect6_attached = false;
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
    fn missing_names_reports_absent_maps() {
        let present = vec!["backend_map".to_string(), "egress_map".to_string()];
        let missing = missing_names(&REQUIRED_MAPS, &present);
        assert!(missing.contains(&"egress6_map"));
        assert!(missing.contains(&"egress_cidr4_map"));
        assert!(missing.contains(&"egress_cidr6_map"));
        assert!(!missing.contains(&"backend_map"));
    }

    #[test]
    fn missing_names_empty_when_all_present() {
        let present: Vec<String> = REQUIRED_MAPS.iter().map(|s| s.to_string()).collect();
        assert!(missing_names(&REQUIRED_MAPS, &present).is_empty());
    }

    #[test]
    fn required_programs_include_both_connect_hooks() {
        assert!(REQUIRED_PROGRAMS.contains(&"onion_connect"));
        assert!(REQUIRED_PROGRAMS.contains(&"onion_connect6"));
    }

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
