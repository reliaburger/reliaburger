//! Bounded node-scoped CPU and memory pressure.
//!
//! Workload CPU and memory faults edit an existing workload cgroup. That
//! cannot exercise a whole node. This controller instead owns a dedicated
//! cgroup and a child copy of `bun`, so the experiment consumes real capacity
//! without moving Bun's control-plane process into the pressured cgroup.
//!
//! The primitive is deliberately opt-in. Both percentages are constrained by
//! server-owned `[testing]` limits, which default to zero. Linux cgroup v2 is
//! required; Apple Container and rootless nodes report the capability as
//! unavailable.
//!
//! Control ordering matters: the memory ceiling is written before the helper
//! spawns (it bounds the ballast allocation), but the CPU quota is written
//! only after the helper reports ready, so startup page-faulting is never
//! throttled against the readiness timeout.

use std::path::{Path, PathBuf};

use crate::smoker::types::FaultId;

/// The cgroup subtree owned exclusively by node-pressure helpers.
pub const NODE_PRESSURE_CGROUP_ROOT: &str = "/sys/fs/cgroup/reliaburger-chaos";
const CPU_PERIOD_US: u64 = 100_000;
#[cfg(target_os = "linux")]
const HELPER_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

/// Operator-owned bounds for node pressure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodePressureLimits {
    /// Largest total-node CPU percentage a helper may consume.
    pub max_cpu_percentage: u8,
    /// Largest total-node memory-usage target a helper may request.
    pub max_memory_percentage: u8,
}

impl NodePressureLimits {
    /// Whether the operator enabled at least one pressure dimension.
    pub fn enabled(self) -> bool {
        self.max_cpu_percentage > 0 || self.max_memory_percentage > 0
    }
}

/// Validate a pressure request against server-owned limits.
pub fn validate_request(
    cpu_percentage: u8,
    memory_percentage: u8,
    limits: NodePressureLimits,
) -> Result<(), String> {
    if cpu_percentage == 0 && memory_percentage == 0 {
        return Err("node pressure must request CPU, memory, or both".to_string());
    }
    if cpu_percentage > limits.max_cpu_percentage {
        return Err(format!(
            "node CPU pressure {cpu_percentage}% exceeds the configured maximum of {}%",
            limits.max_cpu_percentage
        ));
    }
    if memory_percentage > limits.max_memory_percentage {
        return Err(format!(
            "node memory pressure {memory_percentage}% exceeds the configured maximum of {}%",
            limits.max_memory_percentage
        ));
    }
    Ok(())
}

/// CPU quota for a helper consuming `percentage` of every available core.
pub fn node_cpu_max(percentage: u8, cores: u32) -> String {
    let quota = CPU_PERIOD_US
        .saturating_mul(cores.max(1) as u64)
        .saturating_mul(percentage as u64)
        / 100;
    format!("{} {CPU_PERIOD_US}", quota.max(1))
}

/// Bytes a helper must make resident to bring total node usage to a target.
///
/// `available_bytes` comes from Linux `MemAvailable`, which accounts for
/// reclaimable cache. If the node is already at or above the requested usage,
/// no extra memory is allocated.
pub fn memory_bytes_to_target(
    total_bytes: u64,
    available_bytes: u64,
    target_percentage: u8,
) -> u64 {
    let used = total_bytes.saturating_sub(available_bytes.min(total_bytes));
    let target = total_bytes.saturating_mul(target_percentage as u64) / 100;
    target.saturating_sub(used)
}

/// Hard cgroup ceiling for a requested share of physical node memory.
///
/// This ceiling is independent of the node's momentary usage. The helper
/// recalculates how much ballast it needs after joining the cgroup, so deriving
/// the ceiling from an earlier delta would race with that calculation.
pub fn node_memory_max(total_bytes: u64, target_percentage: u8) -> u64 {
    total_bytes.saturating_mul(target_percentage as u64) / 100
}

/// Owns any live node-pressure helper on this Bun process.
#[derive(Default)]
pub struct NodePressureController {
    limits: NodePressureLimits,
    executable: Option<PathBuf>,
    available: bool,
    #[cfg(target_os = "linux")]
    active: std::collections::HashMap<FaultId, NodePressureHandle>,
}

#[cfg(target_os = "linux")]
struct NodePressureHandle {
    child: tokio::process::Child,
    cgroup: PathBuf,
}

impl NodePressureController {
    /// Configure the controller and clean leftovers owned by a crashed Bun.
    ///
    /// Returns whether the primitive is usable on this node. A false result is
    /// capability evidence, not a fault: the default policy intentionally
    /// disables pressure.
    pub fn configure(&mut self, limits: NodePressureLimits, executable: PathBuf) -> bool {
        self.limits = limits;
        self.executable = Some(executable);
        self.available = prepare_controller(limits).is_ok();
        self.available
    }

    /// Whether configuration and the host cgroup hierarchy support pressure.
    pub fn available(&self) -> bool {
        self.available
    }

    /// Start one bounded helper. Concurrent pressure faults are refused so
    /// independent requests cannot add up past the configured ceiling.
    pub async fn apply(
        &mut self,
        id: FaultId,
        cpu_percentage: u8,
        memory_percentage: u8,
    ) -> Result<(), String> {
        validate_request(cpu_percentage, memory_percentage, self.limits)?;
        if !self.available {
            return Err(
                "node pressure requires an enabled rootful Linux cgroup-v2 controller".to_string(),
            );
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Err("node pressure requires Linux cgroup v2".to_string())
        }

        #[cfg(target_os = "linux")]
        {
            if !self.active.is_empty() {
                return Err(
                    "a node-pressure fault is already active; clear it before starting another"
                        .to_string(),
                );
            }
            let executable = self
                .executable
                .as_ref()
                .ok_or_else(|| "node-pressure helper executable is unavailable".to_string())?;
            let cgroup = Path::new(NODE_PRESSURE_CGROUP_ROOT).join(id.to_string());
            let (_memory_ceiling_bytes, cores) = prepare_fault_cgroup(&cgroup, memory_percentage)?;

            let mut command = tokio::process::Command::new(executable);
            command
                .arg("__node-pressure-helper")
                .arg("--cgroup")
                .arg(&cgroup)
                .arg("--parent-pid")
                .arg(std::process::id().to_string())
                .arg("--memory-percentage")
                .arg(memory_percentage.to_string())
                .arg("--cpu-workers")
                .arg(if cpu_percentage > 0 { cores } else { 0 }.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);

            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    let _ = remove_fault_cgroup(&cgroup);
                    return Err(format!("failed to start node-pressure helper: {error}"));
                }
            };
            let Some(stdout) = child.stdout.take() else {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = remove_fault_cgroup(&cgroup);
                return Err("node-pressure helper stdout was not captured".to_string());
            };
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut ready = String::new();
            let started = tokio::time::timeout(
                HELPER_START_TIMEOUT,
                tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut ready),
            )
            .await;
            if !matches!(&started, Ok(Ok(_))) || ready.trim() != "ready" {
                let reason = if started.is_err() {
                    "node-pressure helper did not become ready within 4 seconds".to_string()
                } else {
                    let stderr = child.stderr.take().map(tokio::io::BufReader::new).map(
                        |mut stderr| async move {
                            let mut message = String::new();
                            let _ =
                                tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut message)
                                    .await;
                            message
                        },
                    );
                    let stderr_message = match stderr {
                        Some(stderr) => {
                            tokio::time::timeout(std::time::Duration::from_millis(100), stderr)
                                .await
                                .unwrap_or_default()
                        }
                        None => String::new(),
                    };
                    let status = child
                        .try_wait()
                        .ok()
                        .flatten()
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "still running".to_string());
                    let memory_events =
                        std::fs::read_to_string(cgroup.join("memory.events")).unwrap_or_default();
                    let memory_current =
                        std::fs::read_to_string(cgroup.join("memory.current")).unwrap_or_default();
                    let memory_peak =
                        std::fs::read_to_string(cgroup.join("memory.peak")).unwrap_or_default();
                    let memory_max =
                        std::fs::read_to_string(cgroup.join("memory.max")).unwrap_or_default();
                    format!(
                        "node-pressure helper failed before readiness \
                         (status: {status}; stderr: {stderr_message:?}; \
                         memory.current: {:?}; memory.peak: {:?}; memory.max: {:?}; \
                         memory.events: {:?})",
                        memory_current.trim(),
                        memory_peak.trim(),
                        memory_max.trim(),
                        memory_events.trim(),
                    )
                };
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = remove_fault_cgroup(&cgroup);
                return Err(reason);
            }

            // Throttle only after the helper is resident and ready. The spin
            // workers run unthrottled for at most this one round-trip; the
            // memory ceiling has bounded the allocation since before spawn.
            if cpu_percentage > 0
                && let Err(error) = write_control(
                    &cgroup,
                    "cpu.max",
                    &node_cpu_max(cpu_percentage, cores as u32),
                )
            {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = remove_fault_cgroup(&cgroup);
                return Err(error);
            }

            self.active.insert(id, NodePressureHandle { child, cgroup });
            Ok(())
        }
    }

    /// Terminate a helper and remove its owned cgroup.
    pub async fn clear(&mut self, id: FaultId) -> Result<(), String> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Ok(())
        }

        #[cfg(target_os = "linux")]
        {
            let Some(mut handle) = self.active.remove(&id) else {
                return Ok(());
            };
            let _ = handle.child.kill().await;
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(2), handle.child.wait()).await;
            if let Err(error) = remove_fault_cgroup(&handle.cgroup) {
                self.active.insert(id, handle);
                return Err(error);
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn prepare_controller(limits: NodePressureLimits) -> Result<(), String> {
    if !limits.enabled() {
        return Err("node pressure is disabled by policy".to_string());
    }
    if crate::grill::rootless::is_rootless() {
        return Err("rootless Bun has no delegated pressure cgroup".to_string());
    }
    let root = Path::new(NODE_PRESSURE_CGROUP_ROOT);
    std::fs::create_dir_all(root)
        .map_err(|error| format!("failed to create {}: {error}", root.display()))?;
    cleanup_stale_cgroups(root)?;

    let controllers = std::fs::read_to_string(root.join("cgroup.controllers"))
        .map_err(|error| format!("failed to read cgroup controllers: {error}"))?;
    for required in ["cpu", "memory"] {
        if !controllers.split_whitespace().any(|item| item == required) {
            return Err(format!("cgroup v2 {required} controller is unavailable"));
        }
    }
    let enabled = std::fs::read_to_string(root.join("cgroup.subtree_control")).unwrap_or_default();
    let missing: Vec<_> = ["cpu", "memory"]
        .into_iter()
        .filter(|required| !enabled.split_whitespace().any(|item| item == *required))
        .map(|item| format!("+{item}"))
        .collect();
    if !missing.is_empty() {
        std::fs::write(root.join("cgroup.subtree_control"), missing.join(" "))
            .map_err(|error| format!("failed to enable pressure cgroup controllers: {error}"))?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn prepare_controller(_limits: NodePressureLimits) -> Result<(), String> {
    Err("node pressure requires Linux cgroup v2".to_string())
}

#[cfg(target_os = "linux")]
fn cleanup_stale_cgroups(root: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(root)
        .map_err(|error| format!("failed to inspect {}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to inspect pressure cgroup: {error}"))?;
        if entry.file_name().to_string_lossy().starts_with("fault-") {
            remove_fault_cgroup(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn prepare_fault_cgroup(cgroup: &Path, memory_percentage: u8) -> Result<(u64, usize), String> {
    std::fs::create_dir(cgroup)
        .map_err(|error| format!("failed to create {}: {error}", cgroup.display()))?;
    let setup = (|| {
        let memory_ceiling = if memory_percentage == 0 {
            0
        } else {
            let (total, _) = read_linux_memory()?;
            node_memory_max(total, memory_percentage)
        };
        if memory_percentage > 0 {
            write_control(cgroup, "memory.max", &memory_ceiling.to_string())?;
            write_control_if_present(cgroup, "memory.swap.max", "0")?;
            write_control_if_present(cgroup, "memory.oom.group", "1")?;
        }

        // The CPU quota is deliberately NOT written here. The helper faults
        // its ballast in while inside this cgroup; a pre-applied quota would
        // throttle that startup work and race the readiness timeout. `apply`
        // writes `cpu.max` once the helper reports ready. The memory ceiling
        // must stay pre-spawn: it is the safety bound on the allocation.
        let cores = std::thread::available_parallelism()
            .map(|cores| cores.get())
            .unwrap_or(1);
        Ok((memory_ceiling, cores))
    })();
    if setup.is_err() {
        let _ = remove_fault_cgroup(cgroup);
    }
    setup
}

#[cfg(target_os = "linux")]
fn remove_fault_cgroup(cgroup: &Path) -> Result<(), String> {
    if !cgroup.exists() {
        return Ok(());
    }
    write_control_if_present(cgroup, "cgroup.kill", "1")?;
    for _ in 0..20 {
        match std::fs::remove_dir(cgroup) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::DirectoryNotEmpty
                        | std::io::ErrorKind::ResourceBusy
                        | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => {
                return Err(format!(
                    "failed to remove pressure cgroup {}: {error}",
                    cgroup.display()
                ));
            }
        }
    }
    Err(format!(
        "pressure cgroup {} remained populated after cleanup",
        cgroup.display()
    ))
}

#[cfg(target_os = "linux")]
fn write_control(cgroup: &Path, name: &str, value: &str) -> Result<(), String> {
    std::fs::write(cgroup.join(name), value)
        .map_err(|error| format!("failed to write {name} for {}: {error}", cgroup.display()))
}

#[cfg(target_os = "linux")]
fn write_control_if_present(cgroup: &Path, name: &str, value: &str) -> Result<(), String> {
    let path = cgroup.join(name);
    if path.exists() {
        std::fs::write(&path, value)
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_linux_memory() -> Result<(u64, u64), String> {
    let meminfo = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("failed to read /proc/meminfo: {error}"))?;
    parse_linux_meminfo(&meminfo)
}

/// Parse total and available memory from Linux `/proc/meminfo`.
pub fn parse_linux_meminfo(meminfo: &str) -> Result<(u64, u64), String> {
    fn value(meminfo: &str, key: &str) -> Option<u64> {
        meminfo.lines().find_map(|line| {
            let rest = line.strip_prefix(key)?;
            rest.split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .map(|kib| kib.saturating_mul(1024))
        })
    }
    let total =
        value(meminfo, "MemTotal:").ok_or_else(|| "MemTotal is absent from meminfo".to_string())?;
    let available = value(meminfo, "MemAvailable:")
        .ok_or_else(|| "MemAvailable is absent from meminfo".to_string())?;
    Ok((total, available))
}

/// Run inside the hidden helper subprocess.
#[cfg(target_os = "linux")]
pub fn run_helper(
    cgroup: &Path,
    parent_pid: u32,
    memory_percentage: u8,
    cpu_workers: usize,
) -> Result<(), String> {
    // SAFETY: PR_SET_PDEATHSIG only changes this process's parent-death
    // signal. SIGKILL has no handler and needs no shared memory invariants.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(format!(
            "failed to set parent-death signal: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Close the fork/exec race: if Bun died before prctl took effect, this
    // helper has already been reparented and must exit rather than pressuring
    // a node without an owner.
    // SAFETY: getppid reads process metadata and does not dereference memory.
    let actual_parent = unsafe { libc::getppid() } as u32;
    if actual_parent == 1 || actual_parent != parent_pid {
        return Err("node-pressure helper lost its Bun parent before startup".to_string());
    }
    std::fs::write(cgroup.join("cgroup.procs"), std::process::id().to_string())
        .map_err(|error| format!("failed to join pressure cgroup: {error}"))?;

    // Re-read after joining the cgroup. Starting the helper itself consumes
    // memory; calculating only in the parent would add that overhead on top
    // of the requested node target.
    let memory_bytes = if memory_percentage == 0 {
        0
    } else {
        let (total, available) = read_linux_memory()?;
        memory_bytes_to_target(total, available, memory_percentage)
    };
    let memory_len = usize::try_from(memory_bytes)
        .map_err(|_| "requested pressure memory does not fit this architecture".to_string())?;
    let mut ballast = Vec::new();
    ballast
        .try_reserve_exact(memory_len)
        .map_err(|error| format!("failed to reserve pressure memory: {error}"))?;
    // Fill by page-sized memcpy, not `resize(len, 0xa5)`: an unoptimised
    // build lowers the per-byte resize loop to ~100 MB/s, slow enough to
    // trip the parent's readiness timeout on multi-hundred-MB deltas.
    const PAGE: usize = 4096;
    let page = [0xa5u8; PAGE];
    while memory_len - ballast.len() >= PAGE {
        ballast.extend_from_slice(&page);
    }
    let tail = memory_len - ballast.len();
    ballast.extend_from_slice(&page[..tail]);

    let mut workers = Vec::with_capacity(cpu_workers);
    for _ in 0..cpu_workers {
        workers.push(std::thread::spawn(|| {
            let mut value = 1u64;
            loop {
                value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(value);
            }
        }));
    }

    use std::io::Write as _;
    println!("ready");
    std::io::stdout()
        .flush()
        .map_err(|error| format!("failed to report helper readiness: {error}"))?;

    // Keep both the resident pages and worker handles owned until Bun clears
    // the fault, its TTL expires, or PR_SET_PDEATHSIG terminates this process.
    std::hint::black_box(&ballast);
    std::hint::black_box(&workers);
    loop {
        std::thread::park();
    }
}

/// Non-Linux helper guard. The hidden subcommand exists for a stable CLI shape
/// but cannot apply pressure without cgroup v2.
#[cfg(not(target_os = "linux"))]
pub fn run_helper(
    _cgroup: &Path,
    _parent_pid: u32,
    _memory_percentage: u8,
    _cpu_workers: usize,
) -> Result<(), String> {
    Err("node pressure requires Linux cgroup v2".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_needs_work_and_stays_under_both_server_ceilings() {
        let limits = NodePressureLimits {
            max_cpu_percentage: 80,
            max_memory_percentage: 90,
        };
        assert!(validate_request(80, 90, limits).is_ok());
        assert!(validate_request(0, 90, limits).is_ok());
        assert!(validate_request(80, 0, limits).is_ok());
        assert!(
            validate_request(0, 0, limits)
                .unwrap_err()
                .contains("must request")
        );
        assert!(
            validate_request(81, 90, limits)
                .unwrap_err()
                .contains("CPU")
        );
        assert!(
            validate_request(80, 91, limits)
                .unwrap_err()
                .contains("memory")
        );
    }

    #[test]
    fn quota_scales_across_the_whole_node() {
        assert_eq!(node_cpu_max(80, 1), "80000 100000");
        assert_eq!(node_cpu_max(80, 4), "320000 100000");
    }

    #[test]
    fn memory_allocation_targets_total_usage_not_an_extra_percentage() {
        let gib = 1024 * 1024 * 1024;
        assert_eq!(memory_bytes_to_target(10 * gib, 6 * gib, 90), 5 * gib);
        assert_eq!(memory_bytes_to_target(10 * gib, gib, 90), 0);
        assert_eq!(node_memory_max(10 * gib, 90), 9 * gib);
    }

    #[test]
    fn linux_meminfo_parser_uses_kibibytes() {
        let (total, available) =
            parse_linux_meminfo("MemTotal:       1024 kB\nMemAvailable:    256 kB\n").unwrap();
        assert_eq!(total, 1024 * 1024);
        assert_eq!(available, 256 * 1024);
    }
}
