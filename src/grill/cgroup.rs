/// Cgroup v2 parameter computation.
///
/// Computes paths and parameter values for cgroup v2 resource limits.
/// All functions are pure computation — no filesystem access. The
/// actual cgroup directory creation and file writes are deferred to
/// integration tests when running on Linux with root access.
use std::path::PathBuf;

use crate::config::types::ResourceRange;

/// The cgroup v2 root directory for Reliaburger workloads.
pub(crate) const CGROUP_ROOT: &str = "/sys/fs/cgroup/reliaburger";

/// The default cgroup CPU period in microseconds.
pub(crate) const CGROUP_PERIOD_US: u64 = 100_000;

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
}

/// Build the cgroup directory path for a workload instance.
pub fn cgroup_path(namespace: &str, app_name: &str, instance: u32) -> PathBuf {
    PathBuf::from(format!("{CGROUP_ROOT}/{namespace}/{app_name}/{instance}"))
}

/// Build the host cgroup path for an exact workload instance.
/// The deployment generation remains part of the leaf name, so retiring an
/// old container cannot remove its successor's cgroup. Invalid identities refuse.
pub fn instance_cgroup_path(
    namespace: &str,
    app_name: &str,
    instance: &super::InstanceId,
) -> Result<PathBuf, super::GrillError> {
    let invalid = || super::GrillError::StartFailed {
        instance: instance.clone(),
        reason: "instance identity does not match its cgroup owner".into(),
    };
    let valid_label = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    };
    if !valid_label(namespace) || !valid_label(app_name) {
        return Err(invalid());
    }
    let prefix = format!("{namespace}__{app_name}-");
    let suffix = instance.0.strip_prefix(&prefix).ok_or_else(invalid)?;
    let canonical = if let Some((generation, ordinal)) = suffix
        .strip_prefix('g')
        .and_then(|value| value.split_once('-'))
    {
        let generation = generation.parse::<u64>().map_err(|_| invalid())?;
        let ordinal = ordinal.parse::<u32>().map_err(|_| invalid())?;
        format!("g{generation}-{ordinal}")
    } else {
        suffix.parse::<u32>().map_err(|_| invalid())?.to_string()
    };
    if suffix != canonical {
        return Err(invalid());
    }
    Ok(PathBuf::from(CGROUP_ROOT)
        .join(namespace)
        .join(app_name)
        .join(canonical))
}

/// The CFS quota, in microseconds per [`CGROUP_PERIOD_US`], for a CPU limit
/// in millicores, or `None` when it's too large to compute.
///
/// 500 millicores means 50ms of CPU time per 100ms period.
/// The formula is: quota_us = millicores * period_us / 1000.
pub(crate) fn cpu_quota_us(millicores: u64) -> Option<u64> {
    millicores
        .checked_mul(CGROUP_PERIOD_US)
        .map(|product| product / 1000)
}

/// Convert millicores to a `cpu.max` string.
///
/// A limit too large to compute is no limit (`max`). Config parsing refuses
/// one that large, so this only guards the arithmetic.
pub fn cpu_max_from_millicores(millicores: u64) -> String {
    match cpu_quota_us(millicores) {
        Some(quota_us) => format!("{quota_us} {CGROUP_PERIOD_US}"),
        None => format!("max {CGROUP_PERIOD_US}"),
    }
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

    // A memory request is a scheduling reservation, not a cgroup setting
    // (#1955): the limit is the only memory file we compute.
    let memory_max = memory.map(|range| range.limit);

    CgroupParams {
        path,
        cpu_max,
        cpu_weight,
        memory_max,
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

    /// A quota too large to compute is no limit, never a wrapped value (#2042).
    #[test]
    fn cpu_max_that_overflows_is_unlimited() {
        assert_eq!(cpu_max_from_millicores(u64::MAX), "max 100000");
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
    }

    #[test]
    fn compute_with_no_resources() {
        let params = compute_cgroup_params("default", "web", 0, None, None);
        assert!(params.cpu_max.is_none());
        assert!(params.cpu_weight.is_none());
        assert!(params.memory_max.is_none());
    }

    #[test]
    fn instance_paths_keep_deployment_generation_and_structured_owner() {
        use super::super::InstanceIdentity;
        let ordinary = InstanceIdentity::new("default", "web", 0).instance_id();
        let canary = InstanceIdentity::canary("default", "web", 7, 0).instance_id();
        assert_eq!(
            instance_cgroup_path("default", "web", &ordinary).unwrap(),
            cgroup_path("default", "web", 0)
        );
        assert_eq!(
            instance_cgroup_path("default", "web", &canary).unwrap(),
            PathBuf::from("/sys/fs/cgroup/reliaburger/default/web/g7-0")
        );
        // An ordinary app whose name resembles a generation is a different owner.
        assert_ne!(
            instance_cgroup_path("default", "web-g7", &canary).unwrap(),
            instance_cgroup_path("default", "web", &canary).unwrap()
        );
    }

    #[test]
    fn instance_paths_reject_traversal_and_mismatched_or_noncanonical_ids() {
        use super::super::InstanceId;
        for id in [
            "other__web-0",
            "default__other-0",
            "default__web-../0",
            "default__web-g1-../0",
            "default__web-00",
            "default__web-g01-0",
        ] {
            assert!(
                instance_cgroup_path("default", "web", &InstanceId(id.into())).is_err(),
                "{id}"
            );
        }
        assert!(
            instance_cgroup_path("../default", "web", &InstanceId("../default__web-0".into()))
                .is_err()
        );
    }

    // -- test cgroup cleanup (#2322) -----------------------------------------

    use crate::test_cgroups::{self, TestCgroups};
    use std::path::Path;

    /// Start a process in the cgroup at `path`. It runs `sleep seconds`.
    fn occupy(path: &Path, seconds: &str) -> std::process::Child {
        let child = std::process::Command::new("sleep")
            .arg(seconds)
            .spawn()
            .unwrap();
        std::fs::write(path.join("cgroup.procs"), child.id().to_string()).unwrap();
        child
    }

    fn assert_root_with_cgroup_v2() {
        assert!(nix::unistd::geteuid().is_root(), "run as root");
        assert!(Path::new("/sys/fs/cgroup/cgroup.controllers").is_file());
    }

    #[test]
    #[ignore = "requires root and cgroup v2; run with make test-linux"]
    fn cgroup_guard_removes_its_tree_children_first_once_empty() {
        assert_root_with_cgroup_v2();
        let namespace = format!("rbtest-guarded-{}", std::process::id());
        let top = Path::new(CGROUP_ROOT).join(&namespace);
        let leaf = top.join("executor-reuse/0/task");
        let guard = TestCgroups::namespace(&namespace);
        std::fs::create_dir_all(&leaf).unwrap();
        // A process still exiting when the test ends: the guard waits for
        // it rather than leaving the tree behind.
        let mut child = occupy(&leaf, "0.5");
        drop(guard);
        let _ = child.wait();
        assert!(!top.exists(), "{} was left behind", top.display());
    }

    #[test]
    #[ignore = "requires root and cgroup v2; run with make test-linux"]
    fn cgroup_guard_removes_every_test_cgroup_its_prefix_claims() {
        assert_root_with_cgroup_v2();
        let prefix = format!("rbtest-prefix-{}", std::process::id());
        let top = Path::new(test_cgroups::CGROUP_MOUNT).join(&prefix);
        let unit = Path::new(test_cgroups::CGROUP_MOUNT).join(format!("{prefix}-unit"));
        let app = Path::new(CGROUP_ROOT).join("default").join(&prefix);
        let default_existed = Path::new(CGROUP_ROOT).join("default").exists();
        let guard = TestCgroups::new([PathBuf::from(format!("{}*", top.display())), app.clone()]);
        for path in [&top, &unit, &app.join("0")] {
            std::fs::create_dir_all(path).unwrap();
        }
        drop(guard);
        if !default_existed {
            let _ = std::fs::remove_dir(Path::new(CGROUP_ROOT).join("default"));
        }
        for path in [&top, &unit, &app] {
            assert!(!path.exists(), "{} was left behind", path.display());
        }
    }

    #[test]
    #[ignore = "requires root and cgroup v2; run with make test-linux"]
    fn cgroup_sweep_removes_only_stale_empty_test_cgroups() {
        assert_root_with_cgroup_v2();
        let id = std::process::id();
        let root = Path::new(CGROUP_ROOT);
        let mount = Path::new(test_cgroups::CGROUP_MOUNT);
        let default_existed = root.join("default").exists();
        // A controller newly enabled in a parent adds its files to every
        // child group, which moves each child's modification time and undoes
        // `age` (#2356). runc enables every controller a parent offers when
        // it makes a container's cgroup, and Bun enables three, so a test
        // running beside this one could do that between `age` and the sweep.
        // Enable them all first; a later enable of the same ones adds nothing.
        std::fs::create_dir_all(root.join("default")).unwrap();
        for parent in [mount, root, &root.join("default")] {
            enable_every_controller(parent);
        }
        // Left by a test that died: empty, test-owned, claimed by nobody and
        // untouched for an hour.
        let stale = [
            root.join(format!("rbtest-stale-{id}")),
            root.join(format!("cap-tmpstale{id}")),
            root.join(format!("default/rbtest-stale-{id}")),
            mount.join(format!("rbtest-stale-{id}")),
        ];
        for (top, leaf) in stale.iter().zip(["executor-host/3", "app/0", "0", ""]) {
            std::fs::create_dir_all(top.join(leaf)).unwrap();
            age(top);
        }
        let abandoned = root.join(format!("rbtest-abandoned-{id}"));
        let guard = TestCgroups::new([abandoned.clone()]);
        std::fs::create_dir_all(abandoned.join("0")).unwrap();
        age(&abandoned);
        guard.abandon();
        // Kept: a test cgroup with a process in it, one a live guard claims,
        // one made moments ago, and a namespace whose name no test owns.
        let busy = root.join(format!("rbtest-busy-{id}"));
        std::fs::create_dir_all(busy.join("app/0")).unwrap();
        let mut child = occupy(&busy.join("app/0"), "30");
        age(&busy);
        let claimed = root.join(format!("rbtest-claimed-{id}"));
        let live = TestCgroups::new([claimed.clone()]);
        std::fs::create_dir_all(claimed.join("0")).unwrap();
        age(&claimed);
        let fresh = root.join(format!("rbtest-fresh-{id}"));
        std::fs::create_dir_all(fresh.join("app/0")).unwrap();
        let product = root.join(format!("sweep-keeps-{id}"));
        std::fs::create_dir_all(product.join("app/0")).unwrap();
        age(&product);

        test_cgroups::sweep_stale();

        let busy_kept = busy.join("app/0").exists();
        let claimed_kept = claimed.join("0").exists();
        let fresh_kept = fresh.join("app/0").exists();
        let product_kept = product.join("app/0").exists();
        let _ = child.kill();
        let _ = child.wait();
        drop(live);
        let wait = std::time::Duration::from_secs(5);
        for path in [&busy, &fresh, &product] {
            test_cgroups::remove_tree(path, wait);
        }
        if !default_existed {
            let _ = std::fs::remove_dir(root.join("default"));
        }
        for path in &stale {
            assert!(!path.exists(), "stale {} was kept", path.display());
        }
        assert!(!abandoned.exists(), "an abandoned claim kept its cgroup");
        assert!(busy_kept, "a cgroup with a process in it was swept");
        assert!(claimed_kept, "a live guard's cgroup was swept");
        assert!(fresh_kept, "a cgroup made moments ago was swept");
        assert!(product_kept, "a cgroup no test owns was swept");
        assert!(!claimed.exists(), "the live guard's drop left its cgroup");
    }

    #[test]
    #[ignore = "requires root and cgroup v2; run with make test-linux"]
    fn cgroup_controller_enabled_in_a_parent_moves_its_children_mtime_only_once() {
        assert_root_with_cgroup_v2();
        let parent = Path::new(test_cgroups::CGROUP_MOUNT)
            .join(format!("rbtest-controllers-{}", std::process::id()));
        let child = parent.join("app");
        let _guard = TestCgroups::new([parent.clone()]);
        std::fs::create_dir_all(&child).unwrap();
        let offered = std::fs::read_to_string(parent.join("cgroup.controllers")).unwrap();
        let controller = offered.split_whitespace().next().expect("a controller");
        let enable = || {
            std::fs::write(
                parent.join("cgroup.subtree_control"),
                format!("+{controller}"),
            )
            .unwrap()
        };
        let untouched = |path: &Path| {
            let modified = std::fs::metadata(path).unwrap().modified().unwrap();
            modified.elapsed().unwrap() > std::time::Duration::from_secs(1800)
        };

        age(&child);
        enable();
        assert!(
            !untouched(&child),
            "enabling {controller} in the parent left the child's mtime alone"
        );
        age(&child);
        enable();
        assert!(
            untouched(&child),
            "enabling {controller} again moved the child's mtime"
        );
    }

    /// Enable in `path`'s children every controller `path` offers, as runc
    /// does in each parent of a container's cgroup.
    fn enable_every_controller(path: &Path) {
        let offered = std::fs::read_to_string(path.join("cgroup.controllers")).unwrap();
        let enable: Vec<String> = offered
            .split_whitespace()
            .map(|controller| format!("+{controller}"))
            .collect();
        std::fs::write(path.join("cgroup.subtree_control"), enable.join(" ")).unwrap();
    }

    /// Make the cgroup tree at `path` look untouched for an hour.
    fn age(path: &Path) {
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                age(&entry.path());
            }
        }
        std::fs::File::open(path)
            .unwrap()
            .set_modified(hour_ago)
            .unwrap();
    }
}
