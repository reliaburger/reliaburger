/// Resource fault injection via cgroups.
///
/// CPU stress, memory pressure, and disk I/O throttling use the same
/// cgroup hierarchy that Bun already manages for container isolation.
/// All functions in this module are Linux-only.
use std::path::Path;

/// Errors from resource fault injection.
#[derive(Debug, thiserror::Error)]
pub enum ResourceFaultError {
    #[error("cgroup operation failed: {0}")]
    CgroupError(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("resource faults require linux with cgroups v2")]
    UnsupportedPlatform,
}

/// Apply disk I/O throttle to a container via blkio cgroup.
///
/// Uses cgroupv2 `io.max` to set read/write bandwidth limits.
#[cfg(target_os = "linux")]
pub fn apply_disk_io_throttle(
    cgroup_path: &Path,
    bytes_per_sec: u64,
    write_only: bool,
    device_major_minor: &str,
) -> Result<(), ResourceFaultError> {
    let io_max_path = cgroup_path.join("io.max");

    let value = if write_only {
        format!("{device_major_minor} rbps=max wbps={bytes_per_sec}")
    } else {
        format!("{device_major_minor} rbps={bytes_per_sec} wbps={bytes_per_sec}")
    };

    std::fs::write(&io_max_path, value.as_bytes())?;
    Ok(())
}

/// Remove disk I/O throttle (restore unlimited).
#[cfg(target_os = "linux")]
pub fn remove_disk_io_throttle(
    cgroup_path: &Path,
    device_major_minor: &str,
) -> Result<(), ResourceFaultError> {
    let io_max_path = cgroup_path.join("io.max");
    let value = format!("{device_major_minor} rbps=max wbps=max");
    std::fs::write(&io_max_path, value.as_bytes())?;
    Ok(())
}

/// The default cgroup v2 CPU period in microseconds. A `cpu.max` value is
/// `"{quota_us} {period_us}"`; we keep the period fixed and vary the quota.
const CPU_PERIOD_US: u64 = 100_000;

/// Compute the `cpu.max` quota string that leaves a workload only
/// `100 - percentage` of one core.
///
/// A CPU-stress fault steals CPU by *capping* the target's quota rather than
/// burning cycles in Bun's own cgroup. At 80% stress the workload keeps 20%
/// of a core: `quota_us = (100 - 80) * period / 100 = 20000`. A percentage of
/// 100 is clamped to a 1% floor so the process never wedges completely (that
/// would be a Kill, not a stress). The returned string is written verbatim to
/// the target cgroup's `cpu.max`.
pub fn cpu_stress_quota(percentage: u8) -> String {
    let remaining = 100u64.saturating_sub(percentage as u64).max(1);
    let quota_us = remaining * CPU_PERIOD_US / 100;
    format!("{quota_us} {CPU_PERIOD_US}")
}

/// Read the current `cpu.max` value of a cgroup, so it can be restored when
/// the fault clears. Returns `"max 100000"` (unlimited) when the file is
/// absent, matching cgroup v2's default.
#[cfg(target_os = "linux")]
pub fn read_cpu_max(cgroup_path: &Path) -> Result<String, ResourceFaultError> {
    let cpu_max_path = cgroup_path.join("cpu.max");
    match std::fs::read_to_string(&cpu_max_path) {
        Ok(content) => Ok(content.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("max 100000".to_string()),
        Err(e) => Err(ResourceFaultError::Io(e)),
    }
}

/// Apply a CPU-stress fault by capping the target cgroup's `cpu.max` quota.
#[cfg(target_os = "linux")]
pub fn apply_cpu_stress(cgroup_path: &Path, percentage: u8) -> Result<(), ResourceFaultError> {
    let cpu_max_path = cgroup_path.join("cpu.max");
    std::fs::write(&cpu_max_path, cpu_stress_quota(percentage).as_bytes())?;
    Ok(())
}

/// Restore a previously saved `cpu.max` value (removes the stress cap).
#[cfg(target_os = "linux")]
pub fn restore_cpu_max(cgroup_path: &Path, saved: &str) -> Result<(), ResourceFaultError> {
    let cpu_max_path = cgroup_path.join("cpu.max");
    std::fs::write(&cpu_max_path, saved.as_bytes())?;
    Ok(())
}

/// Read the current `memory.high` soft-limit value, so it can be restored
/// when a memory-pressure fault clears. Returns `"max"` (unlimited) when the
/// file holds no numeric limit.
#[cfg(target_os = "linux")]
pub fn read_memory_high(cgroup_path: &Path) -> Result<String, ResourceFaultError> {
    let high_path = cgroup_path.join("memory.high");
    match std::fs::read_to_string(&high_path) {
        Ok(content) => Ok(content.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("max".to_string()),
        Err(e) => Err(ResourceFaultError::Io(e)),
    }
}

/// Apply memory pressure by lowering the target cgroup's `memory.high` soft
/// limit to `percentage` of its hard `memory.max`.
///
/// `memory.high` is the throttling limit: the kernel forces the workload into
/// reclaim (and, past its working set, heavy swapping/allocation stalls) as it
/// approaches it, without the outright OOM kill a lowered `memory.max` would
/// cause. We derive the target from the hard limit so a 90%-pressure fault
/// leaves a 10% headroom band. A cgroup with no hard limit can't be squeezed
/// this way, so we reject it rather than silently no-op.
#[cfg(target_os = "linux")]
pub fn apply_memory_pressure(cgroup_path: &Path, percentage: u8) -> Result<(), ResourceFaultError> {
    let limit = read_cgroup_memory_max(cgroup_path)?;
    if limit == u64::MAX {
        return Err(ResourceFaultError::CgroupError(
            "no memory limit set on cgroup, cannot apply memory pressure".into(),
        ));
    }
    let high = (limit * percentage as u64) / 100;
    let high_path = cgroup_path.join("memory.high");
    std::fs::write(&high_path, high.to_string().as_bytes())?;
    Ok(())
}

/// Restore a previously saved `memory.high` value (lifts the pressure).
#[cfg(target_os = "linux")]
pub fn restore_memory_high(cgroup_path: &Path, saved: &str) -> Result<(), ResourceFaultError> {
    let high_path = cgroup_path.join("memory.high");
    std::fs::write(&high_path, saved.as_bytes())?;
    Ok(())
}

/// Read the current memory usage of a cgroup in bytes.
#[cfg(target_os = "linux")]
pub fn read_cgroup_memory_current(cgroup_path: &Path) -> Result<u64, ResourceFaultError> {
    let current_path = cgroup_path.join("memory.current");
    let content = std::fs::read_to_string(&current_path)?;
    content.trim().parse::<u64>().map_err(|e| {
        ResourceFaultError::CgroupError(format!("failed to parse memory.current: {e}"))
    })
}

/// Read the memory limit of a cgroup in bytes.
#[cfg(target_os = "linux")]
pub fn read_cgroup_memory_max(cgroup_path: &Path) -> Result<u64, ResourceFaultError> {
    let max_path = cgroup_path.join("memory.max");
    let content = std::fs::read_to_string(&max_path)?;
    let trimmed = content.trim();
    if trimmed == "max" {
        // No limit set — return a large sentinel
        return Ok(u64::MAX);
    }
    trimmed
        .parse::<u64>()
        .map_err(|e| ResourceFaultError::CgroupError(format!("failed to parse memory.max: {e}")))
}

/// Calculate how many bytes to allocate for a memory pressure fault.
///
/// Returns the number of bytes to `mlock` to push the container to
/// the target percentage of its memory limit.
#[cfg(target_os = "linux")]
pub fn calculate_memory_pressure_bytes(
    cgroup_path: &Path,
    target_percent: u8,
    oom: bool,
) -> Result<u64, ResourceFaultError> {
    let limit = read_cgroup_memory_max(cgroup_path)?;
    if limit == u64::MAX {
        return Err(ResourceFaultError::CgroupError(
            "no memory limit set on cgroup, cannot calculate pressure target".into(),
        ));
    }

    if oom {
        // Allocate beyond the limit to trigger OOM
        Ok(limit + 64 * 1024 * 1024) // limit + 64 MiB
    } else {
        let current = read_cgroup_memory_current(cgroup_path)?;
        let target_usage = (limit * target_percent as u64) / 100;
        if target_usage <= current {
            Ok(0) // already at or above target
        } else {
            Ok(target_usage - current)
        }
    }
}

/// Non-Linux stub — returns UnsupportedPlatform for all operations.
#[cfg(not(target_os = "linux"))]
pub fn apply_disk_io_throttle(
    _cgroup_path: &Path,
    _bytes_per_sec: u64,
    _write_only: bool,
    _device_major_minor: &str,
) -> Result<(), ResourceFaultError> {
    Err(ResourceFaultError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn remove_disk_io_throttle(
    _cgroup_path: &Path,
    _device_major_minor: &str,
) -> Result<(), ResourceFaultError> {
    Err(ResourceFaultError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn read_cpu_max(_cgroup_path: &Path) -> Result<String, ResourceFaultError> {
    Err(ResourceFaultError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn apply_cpu_stress(_cgroup_path: &Path, _percentage: u8) -> Result<(), ResourceFaultError> {
    Err(ResourceFaultError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn restore_cpu_max(_cgroup_path: &Path, _saved: &str) -> Result<(), ResourceFaultError> {
    Err(ResourceFaultError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn read_memory_high(_cgroup_path: &Path) -> Result<String, ResourceFaultError> {
    Err(ResourceFaultError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn apply_memory_pressure(
    _cgroup_path: &Path,
    _percentage: u8,
) -> Result<(), ResourceFaultError> {
    Err(ResourceFaultError::UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
pub fn restore_memory_high(_cgroup_path: &Path, _saved: &str) -> Result<(), ResourceFaultError> {
    Err(ResourceFaultError::UnsupportedPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn disk_io_throttle_unsupported_on_non_linux() {
        let result = apply_disk_io_throttle(Path::new("/tmp"), 1024, false, "8:0");
        assert!(matches!(
            result,
            Err(ResourceFaultError::UnsupportedPlatform)
        ));
    }

    // -- cpu_stress_quota (pure, portable) ------------------------------------

    #[test]
    fn cpu_stress_80_percent_leaves_a_fifth_of_a_core() {
        // 80% stolen -> 20% of one 100ms period remains -> 20000us quota.
        assert_eq!(cpu_stress_quota(80), "20000 100000");
    }

    #[test]
    fn cpu_stress_50_percent_leaves_half_a_core() {
        assert_eq!(cpu_stress_quota(50), "50000 100000");
    }

    #[test]
    fn cpu_stress_100_percent_clamps_to_a_one_percent_floor() {
        // A full 100% would wedge the process; we keep a 1% floor so the
        // workload can still make (very slow) progress — that's a stress,
        // not a Kill.
        assert_eq!(cpu_stress_quota(100), "1000 100000");
    }

    #[test]
    fn cpu_stress_zero_percent_leaves_a_full_core() {
        assert_eq!(cpu_stress_quota(0), "100000 100000");
    }

    // -- Linux cgroup effect + reversal (gated) -------------------------------

    /// Applying and restoring a `cpu.max` cap against a real cgroup-v2 file.
    ///
    /// This writes and reads the actual cgroup file, so it needs a Linux host
    /// with cgroup v2. The Make target `make test-linux` provides it via
    /// `RELIABURGER_CGROUP_TESTS=1`; without the gate the first assertion fails
    /// loudly rather than manufacturing a pass.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires linux cgroup-v2 write access (RELIABURGER_CGROUP_TESTS=1)"]
    fn cgroup_cpu_stress_applies_and_restores() {
        assert!(
            std::env::var("RELIABURGER_CGROUP_TESTS").is_ok(),
            "run via `make test-linux` (needs RELIABURGER_CGROUP_TESTS=1)"
        );
        // A writable temp dir stands in for the cgroup directory: the fault
        // functions only ever read/write `cpu.max` within it.
        let dir = std::env::temp_dir().join(format!("rb-cgroup-cpu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cpu.max"), b"max 100000").unwrap();

        let saved = read_cpu_max(&dir).unwrap();
        assert_eq!(saved, "max 100000");

        apply_cpu_stress(&dir, 80).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("cpu.max")).unwrap().trim(),
            "20000 100000"
        );

        restore_cpu_max(&dir, &saved).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("cpu.max")).unwrap().trim(),
            "max 100000"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires linux cgroup-v2 write access (RELIABURGER_CGROUP_TESTS=1)"]
    fn cgroup_memory_pressure_applies_and_restores() {
        assert!(
            std::env::var("RELIABURGER_CGROUP_TESTS").is_ok(),
            "run via `make test-linux` (needs RELIABURGER_CGROUP_TESTS=1)"
        );
        let dir = std::env::temp_dir().join(format!("rb-cgroup-mem-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 100 MiB hard limit, unlimited soft limit to start.
        let limit = 100 * 1024 * 1024u64;
        std::fs::write(dir.join("memory.max"), limit.to_string()).unwrap();
        std::fs::write(dir.join("memory.high"), b"max").unwrap();

        let saved = read_memory_high(&dir).unwrap();
        assert_eq!(saved, "max");

        apply_memory_pressure(&dir, 90).unwrap();
        let applied: u64 = std::fs::read_to_string(dir.join("memory.high"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(applied, limit * 90 / 100);

        restore_memory_high(&dir, &saved).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("memory.high"))
                .unwrap()
                .trim(),
            "max"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires linux cgroup-v2 write access (RELIABURGER_CGROUP_TESTS=1)"]
    fn cgroup_disk_io_throttle_applies_and_removes() {
        assert!(
            std::env::var("RELIABURGER_CGROUP_TESTS").is_ok(),
            "run via `make test-linux` (needs RELIABURGER_CGROUP_TESTS=1)"
        );
        let dir = std::env::temp_dir().join(format!("rb-cgroup-io-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("io.max"), b"").unwrap();

        apply_disk_io_throttle(&dir, 1024 * 1024, false, "8:0").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("io.max")).unwrap().trim(),
            "8:0 rbps=1048576 wbps=1048576"
        );

        remove_disk_io_throttle(&dir, "8:0").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("io.max")).unwrap().trim(),
            "8:0 rbps=max wbps=max"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
