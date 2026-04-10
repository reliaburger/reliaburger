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

/// CPU burn loop parameters.
///
/// The burn loop runs a tight arithmetic loop inside the target's CPU
/// cgroup, competing with the application for CPU time. Each thread
/// alternates between burning and sleeping to achieve the target
/// percentage.
#[derive(Debug, Clone)]
pub struct CpuBurnConfig {
    /// Target CPU consumption (0-100).
    pub percentage: u8,
    /// Number of cores to stress (None = all in cgroup).
    pub cores: Option<u32>,
    /// Length of each burn/sleep window.
    pub window_us: u64,
}

impl CpuBurnConfig {
    /// Create a burn config for the given percentage.
    pub fn new(percentage: u8, cores: Option<u32>) -> Self {
        Self {
            percentage,
            cores,
            // 10ms windows: burn for (percentage/100)*10ms, sleep the rest
            window_us: 10_000,
        }
    }

    /// How long to burn per window.
    pub fn burn_duration_us(&self) -> u64 {
        (self.window_us * self.percentage as u64) / 100
    }

    /// How long to sleep per window.
    pub fn sleep_duration_us(&self) -> u64 {
        self.window_us - self.burn_duration_us()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_burn_config_durations() {
        let cfg = CpuBurnConfig::new(50, None);
        assert_eq!(cfg.burn_duration_us(), 5_000); // 5ms burn
        assert_eq!(cfg.sleep_duration_us(), 5_000); // 5ms sleep

        let cfg = CpuBurnConfig::new(100, None);
        assert_eq!(cfg.burn_duration_us(), 10_000); // 10ms burn
        assert_eq!(cfg.sleep_duration_us(), 0); // no sleep

        let cfg = CpuBurnConfig::new(0, None);
        assert_eq!(cfg.burn_duration_us(), 0);
        assert_eq!(cfg.sleep_duration_us(), 10_000);
    }

    #[test]
    fn cpu_burn_config_with_cores() {
        let cfg = CpuBurnConfig::new(75, Some(2));
        assert_eq!(cfg.cores, Some(2));
        assert_eq!(cfg.burn_duration_us(), 7_500);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn disk_io_throttle_unsupported_on_non_linux() {
        let result = apply_disk_io_throttle(Path::new("/tmp"), 1024, false, "8:0");
        assert!(matches!(
            result,
            Err(ResourceFaultError::UnsupportedPlatform)
        ));
    }
}
