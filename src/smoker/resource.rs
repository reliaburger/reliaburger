/// Resource fault injection via cgroups.
///
/// CPU stress, memory pressure, and disk I/O throttling use the same
/// cgroup hierarchy that Bun already manages for container isolation.
/// The fault functions are Linux-only; restoring a workload's recorded
/// limits is plain file I/O, so it builds (and is tested) everywhere.
use std::path::Path;

use crate::grill::oci::OciResources;

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

/// The major and minor numbers packed into a `st_dev`, unpacked as
/// glibc's `gnu_dev_major` and `gnu_dev_minor` do.
pub fn device_numbers(dev: u64) -> (u32, u32) {
    let major = ((dev >> 32) & 0xffff_f000) | ((dev >> 8) & 0xfff);
    let minor = ((dev >> 12) & 0xffff_ff00) | (dev & 0xff);
    (major as u32, minor as u32)
}

/// The whole disk (`major:minor`) that device `major:minor` belongs to, as
/// `sys_dev_block` (normally `/sys/dev/block`) describes it: a partition's
/// parent disk, or the device itself when it isn't a partition (a disk,
/// or a device-mapper volume such as LVM or LUKS).
///
/// cgroup v2 `io.max` accepts only whole devices, and refuses a partition
/// with `ENODEV` (#2463). `None` means there's no block device: Btrfs,
/// overlayfs and tmpfs report an anonymous device with major 0.
pub fn whole_disk_of(sys_dev_block: &Path, major: u32, minor: u32) -> Option<String> {
    let entry = sys_dev_block
        .join(format!("{major}:{minor}"))
        .canonicalize()
        .ok()?;
    let disk = if entry.join("partition").exists() {
        entry.parent()?
    } else {
        &entry
    };
    let dev = std::fs::read_to_string(disk.join("dev")).ok()?;
    Some(dev.trim().to_string())
}

/// The device an `io.max` throttle for files under `directory` names, or
/// why there isn't one.
pub fn throttle_device_of(directory: &Path) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(directory).map_err(|error| {
        format!(
            "disk-io fault: cannot stat {}: {error}",
            directory.display()
        )
    })?;
    let (major, minor) = device_numbers(metadata.dev());
    whole_disk_of(Path::new("/sys/dev/block"), major, minor).ok_or_else(|| {
        format!(
            "disk-io fault: {} is not on a block device this node can throttle \
             (device {major}:{minor}; Btrfs, overlayfs and tmpfs have none)",
            directory.display()
        )
    })
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

/// How many cores a workload is entitled to today, given its current
/// `cpu.max` and the host's core count (O17).
///
/// A `cpu.max` of `"max 100000"` means unlimited, so the baseline is every
/// core on the box. A quota of `"200000 100000"` means two cores. Anything
/// unparseable falls back to one core — the conservative reading, since
/// over-estimating the baseline would make a stress fault weaker than asked
/// for, and a chaos experiment that quietly does less than it claims is
/// worse than one that does slightly more.
pub fn baseline_cores(current_cpu_max: &str, host_cores: u32) -> u32 {
    let mut parts = current_cpu_max.split_whitespace();
    let (Some(quota), Some(period)) = (parts.next(), parts.next()) else {
        return 1;
    };
    if quota == "max" {
        return host_cores.max(1);
    }
    match (quota.parse::<u64>(), period.parse::<u64>()) {
        (Ok(quota), Ok(period)) if period > 0 => ((quota / period) as u32).max(1),
        _ => 1,
    }
}

/// Compute the `cpu.max` quota string that leaves a workload
/// `100 - percentage` of the CPU it is entitled to.
///
/// A CPU-stress fault steals CPU by *capping* the target's quota rather than
/// burning cycles in Bun's own cgroup. At 80% stress across two cores the
/// workload keeps 20% of two cores: `(100 - 80) * 2 * period / 100 = 40000`.
///
/// `cores` is the fault's `--cores`, which used to be accepted and silently
/// ignored while the maths assumed exactly one core (O17). On a 4-core node
/// that turned "80% stress" into "you now have 20% of one core" — a 95% cut,
/// not an 80% one. `None` means "as many cores as the workload has", which
/// the caller supplies via [`baseline_cores`].
///
/// A percentage of 100 is clamped to a 1% floor so the process never wedges
/// completely (that would be a Kill, not a stress). The returned string is
/// written verbatim to the target cgroup's `cpu.max`.
pub fn cpu_stress_quota(percentage: u8, cores: u32) -> String {
    let remaining = 100u64.saturating_sub(percentage as u64).max(1);
    let quota_us = remaining
        .saturating_mul(CPU_PERIOD_US)
        .saturating_mul(cores.max(1) as u64)
        / 100;
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
///
/// `cores` is the fault's `--cores`; `None` stresses every core the workload
/// currently has, read from its own `cpu.max` (O17).
#[cfg(target_os = "linux")]
pub fn apply_cpu_stress(
    cgroup_path: &Path,
    percentage: u8,
    cores: Option<u32>,
) -> Result<(), ResourceFaultError> {
    let cores = match cores {
        Some(cores) => cores,
        None => {
            let current = read_cpu_max(cgroup_path)?;
            let host = std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1);
            baseline_cores(&current, host)
        }
    };
    let cpu_max_path = cgroup_path.join("cpu.max");
    std::fs::write(
        &cpu_max_path,
        cpu_stress_quota(percentage, cores).as_bytes(),
    )?;
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
pub fn apply_cpu_stress(
    _cgroup_path: &Path,
    _percentage: u8,
    _cores: Option<u32>,
) -> Result<(), ResourceFaultError> {
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

/// The `cpu.max` a workload's runtime gives it from its recorded resources:
/// its CPU limit as a quota, or unlimited when it declares none.
pub fn recorded_cpu_max(resources: Option<&OciResources>) -> String {
    match resources.and_then(|resources| resources.cpu.as_ref()) {
        Some(cpu) if cpu.quota > 0 => format!("{} {}", cpu.quota, cpu.period),
        Some(cpu) => format!("max {}", cpu.period),
        None => format!("max {CPU_PERIOD_US}"),
    }
}

/// The `memory.high` a workload's runtime gives it from its recorded
/// resources: unlimited, unless the recorded specification sets one. Grill
/// never derives it from a memory request (#1955).
pub fn recorded_memory_high(resources: Option<&OciResources>) -> String {
    resources
        .and_then(|resources| resources.unified.get("memory.high"))
        .cloned()
        .unwrap_or_else(|| "max".to_string())
}

/// Put a workload's cgroup back to the limits its recorded specification
/// gives it, undoing any CPU, memory or disk-I/O fault an earlier Bun left.
///
/// Bun keeps a fault's saved values only in memory, so a Bun that crashes
/// or upgrades mid-fault can't restore them. The recorded specification is
/// the source of truth instead: `cpu.max` and `memory.high` are rewritten
/// from it, and every `io.max` throttle is lifted, since Bun never sets one.
/// A file the cgroup doesn't have (its controller isn't enabled) is skipped.
pub fn restore_recorded_limits(
    cgroup_path: &Path,
    resources: Option<&OciResources>,
) -> Result<(), ResourceFaultError> {
    write_if_present(&cgroup_path.join("cpu.max"), &recorded_cpu_max(resources))?;
    write_if_present(
        &cgroup_path.join("memory.high"),
        &recorded_memory_high(resources),
    )?;
    let io_max = cgroup_path.join("io.max");
    let throttles = match std::fs::read_to_string(&io_max) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    // cgroup v2 takes one device per write.
    for device in throttles
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        std::fs::write(
            &io_max,
            format!("{device} rbps=max wbps=max riops=max wiops=max"),
        )?;
    }
    Ok(())
}

/// Write `value` to an existing cgroup file; a missing file is left alone.
fn write_if_present(path: &Path, value: &str) -> Result<(), ResourceFaultError> {
    // Opening without `create` keeps a missing controller file missing
    // instead of leaving a stray regular file behind.
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    std::io::Write::write_all(&mut file, value.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- restoring recorded limits (portable: plain files) --------------------

    fn recorded_resources() -> OciResources {
        OciResources {
            pids: None,
            cpu: Some(crate::grill::oci::OciCpuResources {
                quota: 50_000,
                period: 100_000,
            }),
            memory: Some(crate::grill::oci::OciMemoryResources {
                limit: 128 * 1024 * 1024,
            }),
            unified: std::collections::BTreeMap::from([(
                "memory.high".to_string(),
                "33554432".to_string(),
            )]),
            devices: Vec::new(),
        }
    }

    #[test]
    fn restoring_recorded_limits_rewrites_cpu_max_and_memory_high() {
        let dir = tempfile::tempdir().unwrap();
        // What a CPU and a memory fault left behind.
        std::fs::write(dir.path().join("cpu.max"), "1000 100000").unwrap();
        std::fs::write(dir.path().join("memory.high"), "1024").unwrap();

        restore_recorded_limits(dir.path(), Some(&recorded_resources())).unwrap();

        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("cpu.max"), "50000 100000");
        assert_eq!(read("memory.high"), "33554432");
    }

    #[test]
    fn a_workload_without_limits_is_restored_to_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cpu.max"), "1000 100000").unwrap();
        std::fs::write(dir.path().join("memory.high"), "1024").unwrap();

        restore_recorded_limits(dir.path(), None).unwrap();

        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("cpu.max"), "max 100000");
        assert_eq!(read("memory.high"), "max");
    }

    #[test]
    fn restoring_recorded_limits_lifts_the_io_throttle() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("io.max"),
            "8:0 rbps=1048576 wbps=1048576 riops=max wiops=max\n",
        )
        .unwrap();

        restore_recorded_limits(dir.path(), None).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("io.max")).unwrap(),
            "8:0 rbps=max wbps=max riops=max wiops=max"
        );
    }

    #[test]
    fn restoring_recorded_limits_skips_files_the_cgroup_lacks() {
        let dir = tempfile::tempdir().unwrap();
        restore_recorded_limits(dir.path(), Some(&recorded_resources())).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
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

    // -- cpu_stress_quota (pure, portable) ------------------------------------

    #[test]
    fn cpu_stress_80_percent_leaves_a_fifth_of_a_core() {
        // 80% stolen -> 20% of one 100ms period remains -> 20000us quota.
        assert_eq!(cpu_stress_quota(80, 1), "20000 100000");
    }

    #[test]
    fn cpu_stress_50_percent_leaves_half_a_core() {
        assert_eq!(cpu_stress_quota(50, 1), "50000 100000");
    }

    #[test]
    fn cpu_stress_100_percent_clamps_to_a_one_percent_floor() {
        // A full 100% would wedge the process; we keep a 1% floor so the
        // workload can still make (very slow) progress — that's a stress,
        // not a Kill.
        assert_eq!(cpu_stress_quota(100, 1), "1000 100000");
    }

    #[test]
    fn cpu_stress_zero_percent_leaves_a_full_core() {
        assert_eq!(cpu_stress_quota(0, 1), "100000 100000");
    }

    /// O17: `--cores` was parsed and ignored while the maths assumed one
    /// core, so on a 4-core node "80% stress" actually took 95% of the
    /// workload's CPU.
    #[test]
    fn cpu_stress_scales_with_the_cores_it_is_told_to_stress() {
        // 80% across 2 cores: 20% of 2 cores remains = 40000us.
        assert_eq!(cpu_stress_quota(80, 2), "40000 100000");
        // …and across 4: 20% of 4 cores = 80000us, not 20000.
        assert_eq!(cpu_stress_quota(80, 4), "80000 100000");
        // Zero cores is nonsense; treat it as one rather than divide by it.
        assert_eq!(cpu_stress_quota(80, 0), "20000 100000");
    }

    #[test]
    fn baseline_cores_reads_the_workloads_current_entitlement() {
        // Unlimited: every core on the box.
        assert_eq!(baseline_cores("max 100000", 8), 8);
        // A two-core quota.
        assert_eq!(baseline_cores("200000 100000", 8), 2);
        // Sub-core quotas still count as one — you can't stress less than a
        // core's worth of scheduling.
        assert_eq!(baseline_cores("50000 100000", 8), 1);
        // Garbage falls back to one: under-reading the baseline makes the
        // fault weaker than asked, which is the wrong way to be wrong.
        assert_eq!(baseline_cores("", 8), 1);
        assert_eq!(baseline_cores("not a quota", 8), 1);
        assert_eq!(baseline_cores("100000 0", 8), 1);
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
    #[ignore = "requires linux cgroup-v2 write access (RELIABURGER_CGROUP_TESTS=1); run with make test-linux"]
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

        apply_cpu_stress(&dir, 80, Some(1)).unwrap();
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
    #[ignore = "requires linux cgroup-v2 write access (RELIABURGER_CGROUP_TESTS=1); run with make test-linux"]
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

    /// A fake `/sys/dev/block`: disk `vda` (253:0) with partition `vda1`
    /// (253:1), and device-mapper `dm-0` (252:0), which has no partitions.
    fn fake_sys_dev_block() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let devices = dir.path().join("devices");
        for (path, dev) in [("vda", "253:0"), ("vda/vda1", "253:1"), ("dm-0", "252:0")] {
            std::fs::create_dir_all(devices.join(path)).unwrap();
            std::fs::write(devices.join(path).join("dev"), format!("{dev}\n")).unwrap();
            std::os::unix::fs::symlink(devices.join(path), dir.path().join(dev)).unwrap();
        }
        std::fs::write(devices.join("vda/vda1/partition"), "1\n").unwrap();
        dir
    }

    /// #2463: cgroup v2 `io.max` takes whole disks only, so a volumes
    /// directory on a partition is throttled through its disk.
    #[test]
    fn a_partition_resolves_to_its_whole_disk() {
        let sys = fake_sys_dev_block();
        assert_eq!(whole_disk_of(sys.path(), 253, 1).as_deref(), Some("253:0"));
        assert_eq!(whole_disk_of(sys.path(), 253, 0).as_deref(), Some("253:0"));
        assert_eq!(whole_disk_of(sys.path(), 252, 0).as_deref(), Some("252:0"));
        // Btrfs, overlayfs and tmpfs report an anonymous device (major 0),
        // which has no block device to throttle.
        assert_eq!(whole_disk_of(sys.path(), 0, 42), None);
    }

    /// `st_dev` packs major and minor the way glibc's `makedev` does,
    /// with the high bits of each above the classic 8:8 layout.
    #[test]
    fn st_dev_unpacks_major_and_minor_like_glibc() {
        fn makedev(major: u64, minor: u64) -> u64 {
            ((major & 0xfff) << 8)
                | ((major & 0xffff_f000) << 32)
                | (minor & 0xff)
                | ((minor & 0xffff_ff00) << 12)
        }
        assert_eq!(device_numbers(0xfd01), (253, 1));
        for (major, minor) in [(8, 1), (259, 300), (0x1_2345, 0x10_0000)] {
            assert_eq!(
                device_numbers(makedev(major, minor)),
                (major as u32, minor as u32)
            );
        }
    }

    /// #2463: a directory with no block device under it (tmpfs, for one)
    /// is refused with the reason, not written as a bad `io.max` line.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_directory_on_tmpfs_has_no_device_to_throttle() {
        let error = throttle_device_of(Path::new("/proc")).unwrap_err();
        assert!(error.contains("/proc"), "{error}");
        assert!(error.contains("not on a block device"), "{error}");
    }

    /// #2463: what the throttle writes is accepted by a real cgroup's
    /// `io.max`, for the device under the temp directory, which on most
    /// hosts (the Lima VM, cloud images) is a partition.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires linux cgroup-v2 write access (RELIABURGER_CGROUP_TESTS=1); run with make test-linux"]
    fn cgroup_disk_io_throttle_is_accepted_by_a_real_cgroup() {
        assert!(
            std::env::var("RELIABURGER_CGROUP_TESTS").is_ok(),
            "run via `make test-linux` (needs RELIABURGER_CGROUP_TESTS=1)"
        );
        let root = Path::new("/sys/fs/cgroup");
        let controllers = std::fs::read_to_string(root.join("cgroup.subtree_control")).unwrap();
        if !controllers.split_whitespace().any(|name| name == "io") {
            std::fs::write(root.join("cgroup.subtree_control"), "+io").unwrap();
        }
        let cgroup = root.join(format!("rb-io-test-{}", std::process::id()));
        std::fs::create_dir_all(&cgroup).unwrap();
        let device = throttle_device_of(&std::env::temp_dir()).unwrap();

        let applied = apply_disk_io_throttle(&cgroup, 1024 * 1024, false, &device);
        let io_max = std::fs::read_to_string(cgroup.join("io.max")).unwrap_or_default();
        let removed = remove_disk_io_throttle(&cgroup, &device);
        let after = std::fs::read_to_string(cgroup.join("io.max")).unwrap_or_default();
        std::fs::remove_dir(&cgroup).ok();

        applied.unwrap_or_else(|e| panic!("io.max refused {device}: {e}"));
        assert!(
            io_max.contains(&format!("{device} rbps=1048576 wbps=1048576")),
            "{io_max}"
        );
        removed.unwrap();
        assert!(!after.contains("rbps=1048576"), "{after}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires linux cgroup-v2 write access (RELIABURGER_CGROUP_TESTS=1); run with make test-linux"]
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
