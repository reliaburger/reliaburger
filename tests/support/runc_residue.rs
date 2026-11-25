//! Cleanup for tests that run rootful runc containers (#2315).
//!
//! Every rootful runc test runtime starts with an empty address pool, so
//! its first container always takes the same address: the node's first
//! container address, routed to that container's host veth. A test that
//! panics before it kills its container leaves the container running, and
//! the running container keeps its network namespace, its veth and that
//! route. Every later runc test on the host then fails to add the route
//! with `RTNETLINK answers: File exists`. One panic turned into fourteen
//! failures that way.
//!
//! Two things stop that here:
//!
//! - [`RuncResidue`] is a guard over one runtime root. Its `Drop` kills the
//!   root's runc containers and deletes every network namespace and veth
//!   its address pool reserved, on a panic as on a pass. [`RuncRoot`] is a
//!   temporary directory that carries one.
//! - Creating a guard first sweeps what a test killed outright (by a
//!   timeout's SIGKILL, or a panic before this guard existed) left behind:
//!   roots whose owner has gone, and `rb-rbtest-*` namespaces that no live
//!   guard's pool names. Only names tests own are touched.
//!
//! A runc test's cgroups are [`test_cgroups`]'s: the sweep here runs its
//! sweep too, and [`RuncRoot`] claims the cgroups named after its directory
//! (#2322).
//!
//! A guard proves its owner is alive with a lock on a file in its root,
//! which the kernel releases when the owner dies, however it dies. Runc
//! tests run one at a time (nextest's `host-network` group), so a sweep
//! never meets another test's half-made namespace.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use reliaburger::file_lock::{FileLock, FileLockError};
use reliaburger::grill::InstanceId;
use reliaburger::grill::netns::host_veth_name;

#[path = "test_cgroups.rs"]
pub mod test_cgroups;
use test_cgroups::TestCgroups;

/// Where each live guard names its root, one file per root.
const REGISTRY: &str = "/run/reliaburger-tests/runc-roots";

/// The file in a root whose lock says its test is still running.
const OWNER_LOCK: &str = ".runc-test-owner.lock";

/// The network namespace prefix of every test instance (`rb-` + `rbtest-`).
const TEST_NAMESPACE_PREFIX: &str = "rb-rbtest-";

/// How long a deleted namespace's veth may take to disappear.
const VETH_DEADLINE: Duration = Duration::from_secs(5);

/// A guard that releases one runtime root's containers and networks when
/// dropped. The root holds the runtime's `bundles` and `state` directories.
pub struct RuncResidue {
    root: PathBuf,
    entry: PathBuf,
    lock: Option<FileLock>,
}

impl RuncResidue {
    /// Sweep stale test leftovers, then guard `root`.
    pub fn new(root: &Path) -> Self {
        sweep_stale();
        std::fs::create_dir_all(root).unwrap();
        std::fs::create_dir_all(REGISTRY).unwrap();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(OWNER_LOCK))
            .unwrap();
        let lock = FileLock::try_lock(file).unwrap();
        let entry = Path::new(REGISTRY).join(entry_name(root));
        std::fs::write(&entry, root.to_string_lossy().as_bytes()).unwrap();
        Self {
            root: root.to_path_buf(),
            entry,
            lock: Some(lock),
        }
    }

    /// Give the root up the way a SIGKILLed test does: its lock goes, and
    /// nothing is cleaned up. Only for testing the sweep.
    pub fn abandon(mut self) {
        drop(self.lock.take());
        std::mem::forget(self);
    }
}

impl Drop for RuncResidue {
    fn drop(&mut self) {
        release_root(&self.root);
        let _ = std::fs::remove_file(&self.entry);
    }
}

/// A temporary runtime root guarded by a [`RuncResidue`]: dropping it
/// releases the containers and networks first, then deletes the directory.
pub struct RuncRoot {
    // Fields drop in declaration order, so the residue goes before the
    // directory that names it, and the containers are gone before their
    // cgroups are removed.
    residue: RuncResidue,
    cgroups: TestCgroups,
    directory: tempfile::TempDir,
}

impl RuncRoot {
    /// A fresh temporary root. It claims the cgroups tests name after it:
    /// `rbtest-owned-runc-<root>` at the top of the hierarchy and as an app
    /// in `default`, and the namespace `cap-<root>`.
    pub fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let residue = RuncResidue::new(directory.path());
        let suffix = directory
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('.')
            .to_owned();
        let instance = format!("rbtest-owned-runc-{suffix}");
        let hierarchy = Path::new(test_cgroups::HIERARCHY);
        // Tests add suffixes such as `-unit` and `-relocated`, so the top-level
        // claim is a prefix.
        let cgroups = TestCgroups::new([
            Path::new(test_cgroups::CGROUP_MOUNT).join(format!("{instance}*")),
            hierarchy.join("default").join(&instance),
            hierarchy.join(format!("cap-{}", suffix.to_lowercase())),
        ]);
        Self {
            residue,
            cgroups,
            directory,
        }
    }

    /// The root directory.
    pub fn path(&self) -> &Path {
        self.directory.path()
    }
}

impl Default for RuncRoot {
    fn default() -> Self {
        Self::new()
    }
}

/// Kill a root's runc containers, then delete the networks its address
/// pool reserved.
fn release_root(root: &Path) {
    let state = root.join("state");
    if state.is_dir()
        && let Ok(listed) = Command::new("runc")
            .arg("--root")
            .arg(&state)
            .args(["list", "--quiet"])
            .output()
    {
        for id in String::from_utf8_lossy(&listed.stdout).lines() {
            let _ = Command::new("runc")
                .arg("--root")
                .arg(&state)
                .args(["delete", "--force", id])
                .output();
        }
    }
    for instance in leased_instances(root).unwrap_or_default() {
        release_network(&format!("rb-{instance}"));
    }
}

/// The instances a root's address pool reserved addresses for. `None` when
/// the pool can't be read.
fn leased_instances(root: &Path) -> Option<Vec<String>> {
    let path = root.join("bundles/.network-leases.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let journal: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let allocations = journal.get("allocations")?.as_object()?;
    Some(allocations.keys().cloned().collect())
}

/// Kill what still runs in network namespace `name`, delete it and its host
/// veth, and wait for the veth (and so its route) to go.
fn release_network(name: &str) {
    let Some(instance) = name.strip_prefix("rb-") else {
        return;
    };
    let veth = host_veth_name(&InstanceId(instance.to_owned()));
    if let Ok(listed) = Command::new("ip").args(["netns", "pids", name]).output() {
        for pid in String::from_utf8_lossy(&listed.stdout).lines() {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }
    let _ = Command::new("ip").args(["link", "del", &veth]).output();
    let _ = Command::new("ip").args(["netns", "del", name]).output();
    let link = Path::new("/sys/class/net").join(&veth);
    let deadline = Instant::now() + VETH_DEADLINE;
    while link.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let _ = Command::new("ip").args(["link", "del", &veth]).output();
    }
}

/// Whether the test that guarded `root` still runs.
fn owner_alive(root: &Path) -> bool {
    let Ok(file) = std::fs::File::open(root.join(OWNER_LOCK)) else {
        return false;
    };
    matches!(FileLock::try_lock(file), Err(FileLockError::Busy))
}

/// Release the roots of tests that died without their guard's `Drop`, the
/// test namespaces that no live guard's pool names, and the empty test
/// cgroups no live guard claims.
pub fn sweep_stale() {
    sweep_stale_namespaces();
    test_cgroups::sweep_stale();
}

fn sweep_stale_namespaces() {
    let mut live = BTreeSet::new();
    let mut pools_known = true;
    for entry in std::fs::read_dir(REGISTRY).into_iter().flatten().flatten() {
        let Ok(root) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let root = PathBuf::from(root);
        if owner_alive(&root) {
            match leased_instances(&root) {
                Some(instances) => live.extend(instances.into_iter().map(|id| format!("rb-{id}"))),
                None => pools_known = false,
            }
        } else {
            release_root(&root);
            let _ = std::fs::remove_file(entry.path());
        }
    }
    // A live pool we can't read might name any namespace: leave them all.
    if !pools_known {
        return;
    }
    for name in test_namespaces() {
        if !live.contains(&name) {
            eprintln!("runc residue: releasing stale test namespace {name}");
            release_network(&name);
        }
    }
}

/// The host's `rb-rbtest-*` network namespaces.
pub fn test_namespaces() -> Vec<String> {
    std::fs::read_dir("/var/run/netns")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(TEST_NAMESPACE_PREFIX))
        .collect()
}

/// A registry file name for `root`.
fn entry_name(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}
