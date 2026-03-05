/// Cgroup v2 parameter computation.
///
/// Computes paths and parameter values for cgroup v2 resource limits.
/// All functions are pure computation — no filesystem access. The
/// actual cgroup directory creation and file writes are deferred to
/// integration tests when running on Linux with root access.
use std::path::PathBuf;

use crate::config::types::ResourceRange;

/// The cgroup v2 root directory for Reliaburger workloads.
const CGROUP_ROOT: &str = "/sys/fs/cgroup/reliaburger";

/// The default cgroup CPU period in microseconds.
const CGROUP_PERIOD_US: u64 = 100_000;

/// Computed cgroup v2 parameters for a workload instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupParams {
    /// Path to the cgroup directory,
    /// e.g. `/sys/fs/cgroup/reliaburger/default/web/0`.
    pub path: PathBuf,
    /// `cpu.max` value: `"{quota_us} {period_us}"`,
    /// e.g. `"50000 100000"` for 500 millicores.
    pub cpu_max: Option<String>,
    /// `cpu.weight` value (1-10000), derived from CPU request.
    pub cpu_weight: Option<u32>,
    /// `memory.max` in bytes (hard limit, triggers OOM kill).
    pub memory_max: Option<u64>,
    /// `memory.high` in bytes (soft limit, triggers reclaim pressure).
    pub memory_high: Option<u64>,
}

/// Build the cgroup directory path for a workload instance.
pub fn cgroup_path(namespace: &str, app_name: &str, instance: u32) -> PathBuf {
    PathBuf::from(format!("{CGROUP_ROOT}/{namespace}/{app_name}/{instance}"))
}

/// Convert millicores to a `cpu.max` string.
///
/// 500 millicores means 50ms of CPU time per 100ms period.
/// The formula is: quota_us = millicores * period_us / 1000.
pub fn cpu_max_from_millicores(millicores: u64) -> String {
    let quota_us = millicores * CGROUP_PERIOD_US / 1000;
    format!("{quota_us} {CGROUP_PERIOD_US}")
}

/// Convert a CPU request in millicores to a `cpu.weight` value.
///
/// cgroup v2 weight range is 1-10000, where 100 is the default
/// (representing roughly 1000 millicores of proportional share).
/// We scale linearly: weight = millicores / 10, clamped to 1-10000.
pub fn cpu_weight_from_millicores(millicores: u64) -> u32 {
    let weight = millicores / 10;
    (weight as u32).clamp(1, 10000)
}

/// Compute all cgroup parameters from resource ranges.
pub fn compute_cgroup_params(
    namespace: &str,
    app_name: &str,
    instance: u32,
    cpu: Option<&ResourceRange>,
    memory: Option<&ResourceRange>,
) -> CgroupParams {
    let path = cgroup_path(namespace, app_name, instance);

    let (cpu_max, cpu_weight) = match cpu {
        Some(range) => (
            Some(cpu_max_from_millicores(range.limit)),
            Some(cpu_weight_from_millicores(range.request)),
        ),
        None => (None, None),
    };

    let (memory_max, memory_high) = match memory {
        Some(range) => (Some(range.limit), Some(range.request)),
        None => (None, None),
    };

    CgroupParams {
        path,
        cpu_max,
        cpu_weight,
        memory_max,
        memory_high,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- cgroup_path ----------------------------------------------------------

    #[test]
    fn cgroup_path_default_namespace() {
        let path = cgroup_path("default", "web", 0);
        assert_eq!(
            path,
            PathBuf::from("/sys/fs/cgroup/reliaburger/default/web/0")
        );
    }

    #[test]
    fn cgroup_path_custom_namespace() {
        let path = cgroup_path("team-backend", "api", 3);
        assert_eq!(
            path,
            PathBuf::from("/sys/fs/cgroup/reliaburger/team-backend/api/3")
        );
    }

    // -- cpu_max_from_millicores ----------------------------------------------

    #[test]
    fn cpu_max_from_100m() {
        assert_eq!(cpu_max_from_millicores(100), "10000 100000");
    }

    #[test]
    fn cpu_max_from_500m() {
        assert_eq!(cpu_max_from_millicores(500), "50000 100000");
    }

    #[test]
    fn cpu_max_from_1000m() {
        assert_eq!(cpu_max_from_millicores(1000), "100000 100000");
    }

    #[test]
    fn cpu_max_from_2000m() {
        assert_eq!(cpu_max_from_millicores(2000), "200000 100000");
    }

    // -- cpu_weight_from_millicores -------------------------------------------

    #[test]
    fn cpu_weight_from_500m() {
        assert_eq!(cpu_weight_from_millicores(500), 50);
    }

    #[test]
    fn cpu_weight_from_1000m() {
        assert_eq!(cpu_weight_from_millicores(1000), 100);
    }

    #[test]
    fn cpu_weight_minimum_is_one() {
        assert_eq!(cpu_weight_from_millicores(0), 1);
        assert_eq!(cpu_weight_from_millicores(5), 1);
    }

    #[test]
    fn cpu_weight_maximum_is_10000() {
        assert_eq!(cpu_weight_from_millicores(200_000), 10000);
    }

    // -- compute_cgroup_params ------------------------------------------------

    #[test]
    fn compute_with_all_resources() {
        let cpu = ResourceRange {
            request: 500,
            limit: 1000,
        };
        let memory = ResourceRange {
            request: 128 * 1024 * 1024,
            limit: 512 * 1024 * 1024,
        };
        let params = compute_cgroup_params("default", "web", 0, Some(&cpu), Some(&memory));

        assert_eq!(
            params.path,
            PathBuf::from("/sys/fs/cgroup/reliaburger/default/web/0")
        );
        assert_eq!(params.cpu_max, Some("100000 100000".to_string()));
        assert_eq!(params.cpu_weight, Some(50));
        assert_eq!(params.memory_max, Some(512 * 1024 * 1024));
        assert_eq!(params.memory_high, Some(128 * 1024 * 1024));
    }

    #[test]
    fn compute_with_no_cpu() {
        let memory = ResourceRange {
            request: 256 * 1024 * 1024,
            limit: 256 * 1024 * 1024,
        };
        let params = compute_cgroup_params("default", "web", 0, None, Some(&memory));
        assert!(params.cpu_max.is_none());
        assert!(params.cpu_weight.is_none());
        assert_eq!(params.memory_max, Some(256 * 1024 * 1024));
    }

    #[test]
    fn compute_with_no_memory() {
        let cpu = ResourceRange {
            request: 100,
            limit: 500,
        };
        let params = compute_cgroup_params("default", "web", 0, Some(&cpu), None);
        assert_eq!(params.cpu_max, Some("50000 100000".to_string()));
        assert!(params.memory_max.is_none());
        assert!(params.memory_high.is_none());
    }

    #[test]
    fn compute_with_no_resources() {
        let params = compute_cgroup_params("default", "web", 0, None, None);
        assert!(params.cpu_max.is_none());
        assert!(params.cpu_weight.is_none());
        assert!(params.memory_max.is_none());
        assert!(params.memory_high.is_none());
    }
}
