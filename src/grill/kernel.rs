//! What the reusable command executors need from the running kernel.
//!
//! Bun probes once, the first time a runtime is asked for an executor
//! backend, and keeps the answer for the life of the process. A kernel
//! without a feature the executors can't do without gets no executor pool,
//! so jobs take the fresh path (or refuse with a reason) rather than leaking
//! slots. A kernel without `cgroup.kill` (added in 5.14) keeps its pool and
//! retires task groups by freezing them and killing every member instead.
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

/// The oldest kernel Reliaburger supports, as `(major, minor)`.
///
/// eBPF CO-RE and the boot-time clock helper set it at 5.8. The executors
/// need `clone3` with `CLONE_INTO_CGROUP` (5.7) and the cgroup v2 freezer
/// (5.2), both inside it, so there is one number for the whole node.
pub const MINIMUM_KERNEL: (u32, u32) = (5, 8);

/// Where the probe makes its scratch cgroup. Instance cgroups live here too.
const PROBE_PARENT: &str = "/sys/fs/cgroup/reliaburger";

/// How retirement empties a task cgroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKill {
    /// Write `cgroup.kill`, which kills every member atomically (5.14+).
    CgroupKill,
    /// Freeze the group so nobody in it can fork, then SIGKILL each pid in
    /// `cgroup.procs`. Retirement repeats this until the group is empty.
    FreezeAndKill,
}

/// The executor features the running kernel offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutorSupport {
    /// The kernel is at least [`MINIMUM_KERNEL`], so the helper's
    /// `clone3(CLONE_INTO_CGROUP)` launch works.
    pub clone_into_cgroup: bool,
    /// `/proc/<pid>/task/<tid>/children` exists (`CONFIG_PROC_CHILDREN`).
    /// The host helper walks it to kill its commands' descendants.
    pub proc_children: bool,
    /// How retirement empties a task group on this kernel.
    pub group_kill: GroupKill,
}

impl ExecutorSupport {
    /// Whether shared-runc executors can run here.
    pub fn shared_runc(&self) -> bool {
        self.clone_into_cgroup
    }

    /// Whether native host executors can run here.
    pub fn host(&self) -> bool {
        self.clone_into_cgroup && self.proc_children
    }
}

/// The running kernel's executor support, probed once per process.
pub fn executor_support() -> ExecutorSupport {
    static SUPPORT: OnceLock<ExecutorSupport> = OnceLock::new();
    *SUPPORT.get_or_init(|| {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok();
        let support = probe(
            release.as_deref(),
            Path::new("/proc/thread-self/children"),
            Path::new(PROBE_PARENT),
        );
        if !support.shared_runc() {
            eprintln!(
                "kernel {} is older than {}.{}: reusable executors are off, jobs use fresh containers",
                release.as_deref().unwrap_or("unknown").trim(),
                MINIMUM_KERNEL.0,
                MINIMUM_KERNEL.1
            );
        } else if !support.proc_children {
            eprintln!(
                "kernel lacks CONFIG_PROC_CHILDREN: native host executors are off, host jobs use fresh processes"
            );
        }
        if support.group_kill == GroupKill::FreezeAndKill {
            eprintln!(
                "kernel has no usable cgroup.kill: executors retire task groups by freezing and killing each member"
            );
        }
        support
    })
}

/// Probe for executor support. `release` is the kernel release string,
/// `children` the calling thread's children file, and `cgroup_parent` a
/// cgroup v2 directory Bun may create a scratch child in.
fn probe(release: Option<&str>, children: &Path, cgroup_parent: &Path) -> ExecutorSupport {
    ExecutorSupport {
        clone_into_cgroup: release
            .and_then(parse_release)
            .is_some_and(|version| version >= MINIMUM_KERNEL),
        proc_children: children.exists(),
        group_kill: probe_group_kill(cgroup_parent),
    }
}

/// `(major, minor)` from a release such as `5.10.0-23-amd64` or `6.8.0`.
fn parse_release(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.trim().split(['.', '-', '+']);
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Make a scratch cgroup and try writing its `cgroup.kill`. Any failure,
/// whether the file is missing (before 5.14) or the write is refused, means
/// retirement must not depend on it.
fn probe_group_kill(parent: &Path) -> GroupKill {
    let scratch = parent.join(format!("kernel_probe_{}", std::process::id()));
    let created =
        std::fs::create_dir_all(parent).and_then(|()| match std::fs::create_dir(&scratch) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            result => result,
        });
    let kill = match created.and_then(|()| write_existing(&scratch.join("cgroup.kill"), "1")) {
        Ok(()) => GroupKill::CgroupKill,
        Err(_) => GroupKill::FreezeAndKill,
    };
    // A cgroup with no members removes cleanly; a failure leaves an empty
    // directory that nothing else uses.
    let _ = std::fs::remove_dir(&scratch);
    kill
}

/// Write to a file that must already exist. cgroupfs refuses to create
/// files, so `std::fs::write` (which passes O_CREAT) fails with EACCES on
/// a missing interface file, which would hide the real cause.
fn write_existing(path: &Path, value: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(value.as_bytes())
}

/// Kill every member of the cgroup `group` and report whether it is empty.
///
/// Returns `Ok(())` when the group is empty or already gone, and an error
/// while members remain, so callers retry until it succeeds. The members
/// are already signalled when it returns, but they take a moment to exit.
pub fn kill_group(group: &Path, method: GroupKill) -> std::io::Result<()> {
    match std::fs::symlink_metadata(group) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    match method {
        GroupKill::CgroupKill => write_existing(&group.join("cgroup.kill"), "1")?,
        GroupKill::FreezeAndKill => {
            // Frozen tasks can't fork, but a fatal signal still kills them.
            write_existing(&group.join("cgroup.freeze"), "1")?;
            let members = std::fs::read_to_string(group.join("cgroup.procs"))?;
            for pid in members.lines().filter_map(|line| line.trim().parse().ok()) {
                match nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                ) {
                    Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    let events = std::fs::read_to_string(group.join("cgroup.events"))?;
    if !events.lines().any(|line| line == "populated 0") {
        return Err(std::io::Error::other("cgroup still populated"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn kernel_release_parses_distribution_suffixes() {
        assert_eq!(parse_release("5.10.0-23-amd64\n"), Some((5, 10)));
        assert_eq!(parse_release("6.8.0"), Some((6, 8)));
        assert_eq!(parse_release("5.7-rc1"), Some((5, 7)));
        assert_eq!(parse_release("garbage"), None);
        assert_eq!(parse_release(""), None);
    }

    #[test]
    fn preflight_reports_pool_unavailable_before_the_minimum_kernel() {
        let root = tempfile::tempdir().unwrap();
        let children = root.path().join("children");
        std::fs::write(&children, "").unwrap();
        for release in ["5.4.0-150-generic", "5.7.19", "4.19.0"] {
            let support = probe(Some(release), &children, root.path());
            assert!(!support.shared_runc(), "{release}");
            assert!(!support.host(), "{release}");
        }
        assert!(!probe(None, &children, root.path()).shared_runc());
        let support = probe(Some("5.8.0"), &children, root.path());
        assert!(support.shared_runc() && support.host());
    }

    #[test]
    fn preflight_reports_host_pool_unavailable_without_proc_children() {
        let root = tempfile::tempdir().unwrap();
        let support = probe(Some("6.1.0"), &root.path().join("missing"), root.path());
        assert!(support.shared_runc());
        assert!(!support.host());
    }

    #[test]
    fn preflight_falls_back_to_kill_and_reap_without_cgroup_kill() {
        // A plain directory has no interface files, like a pre-5.14 cgroup
        // has no cgroup.kill. The pool stays available.
        let root = tempfile::tempdir().unwrap();
        let children = root.path().join("children");
        std::fs::write(&children, "").unwrap();
        let support = probe(Some("5.10.0-23-amd64"), &children, root.path());
        assert_eq!(support.group_kill, GroupKill::FreezeAndKill);
        assert!(support.shared_runc() && support.host());
        let scratch = root
            .path()
            .join(format!("kernel_probe_{}", std::process::id()));
        assert!(!scratch.exists(), "the probe left its scratch cgroup");
        assert!(
            !scratch.join("cgroup.kill").exists(),
            "the probe created cgroup.kill"
        );
    }

    #[test]
    fn preflight_treats_any_cgroup_kill_write_error_as_unsupported() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root
            .path()
            .join(format!("kernel_probe_{}", std::process::id()));
        // A directory where the file should be: the write fails, but not
        // with NotFound.
        std::fs::create_dir_all(scratch.join("cgroup.kill")).unwrap();
        assert_eq!(probe_group_kill(root.path()), GroupKill::FreezeAndKill);
    }

    #[test]
    fn preflight_uses_cgroup_kill_when_the_kernel_has_it() {
        let root = tempfile::tempdir().unwrap();
        let scratch = root
            .path()
            .join(format!("kernel_probe_{}", std::process::id()));
        std::fs::create_dir(&scratch).unwrap();
        std::fs::write(scratch.join("cgroup.kill"), "").unwrap();
        assert_eq!(probe_group_kill(root.path()), GroupKill::CgroupKill);
    }

    /// A fake task group: a directory with the interface files a real
    /// cgroup has, listing `members`.
    fn fake_group(root: &Path, members: &[u32], populated: bool) -> std::path::PathBuf {
        let group = root.join("task");
        std::fs::create_dir(&group).unwrap();
        std::fs::write(group.join("cgroup.freeze"), "0").unwrap();
        let procs: String = members.iter().map(|pid| format!("{pid}\n")).collect();
        std::fs::write(group.join("cgroup.procs"), procs).unwrap();
        std::fs::write(
            group.join("cgroup.events"),
            format!("populated {}\nfrozen 0\n", u8::from(populated)),
        )
        .unwrap();
        group
    }

    #[test]
    fn kill_and_reap_freezes_the_group_and_kills_every_member() {
        let root = tempfile::tempdir().unwrap();
        let mut members: Vec<_> = (0..2)
            .map(|_| {
                std::process::Command::new("sleep")
                    .arg("60")
                    .spawn()
                    .unwrap()
            })
            .collect();
        let pids: Vec<u32> = members.iter().map(|child| child.id()).collect();
        let group = fake_group(root.path(), &pids, true);
        // Still populated: the caller must retry.
        assert!(kill_group(&group, GroupKill::FreezeAndKill).is_err());
        assert_eq!(
            std::fs::read_to_string(group.join("cgroup.freeze")).unwrap(),
            "1"
        );
        assert!(!group.join("cgroup.kill").exists());
        for member in &mut members {
            assert_eq!(member.wait().unwrap().signal(), Some(libc_sigkill()));
        }
        // The kernel reports the group empty once they've gone.
        std::fs::write(group.join("cgroup.events"), "populated 0\nfrozen 1\n").unwrap();
        assert!(kill_group(&group, GroupKill::FreezeAndKill).is_ok());
    }

    #[test]
    fn kill_group_succeeds_for_a_group_that_is_already_gone() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("task");
        assert!(kill_group(&missing, GroupKill::CgroupKill).is_ok());
        assert!(kill_group(&missing, GroupKill::FreezeAndKill).is_ok());
    }

    #[test]
    fn kill_group_never_creates_a_missing_interface_file() {
        let root = tempfile::tempdir().unwrap();
        let group = fake_group(root.path(), &[], false);
        assert!(kill_group(&group, GroupKill::CgroupKill).is_err());
        assert!(!group.join("cgroup.kill").exists());
    }

    fn libc_sigkill() -> i32 {
        nix::sys::signal::Signal::SIGKILL as i32
    }
}
