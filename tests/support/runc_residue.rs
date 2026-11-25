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
//! - [`TestNamespaces`] is a guard over namespaces a test makes itself,
//!   outside any runtime root's pool, such as `owned_network`'s. Its `Drop`
//!   deletes them, and its registry entry names its owning process and that
//!   process's start time, as [`test_cgroups`]'s does.
//! - Creating either guard first sweeps what a test killed outright (by a
//!   timeout's SIGKILL, or a panic before its guard existed) left behind:
//!   roots and namespace claims whose owner has gone, and `rb-rbtest-*`
//!   namespaces that nothing claims and that are older than [`STALE_AFTER`].
//!   Only names tests own are touched.
//!
//! A runc test's cgroups are [`test_cgroups`]'s: the sweep here runs its
//! sweep too, and [`RuncRoot`] claims the cgroups named after its directory
//! (#2322).
//!
//! A root's guard proves its owner is alive with a lock on a file in its
//! root, which the kernel releases when the owner dies, however it dies.
//!
//! A namespace nobody claims is never deleted just because nobody claims
//! it: tests outside nextest's `host-network` group sweep too (the
//! leader-loss test on host processes, under `cgroup_`), so a sweep can run
//! while another test is half way through making its namespace (#2332).
//! The kernel stamps a namespace's inode when `ip netns add` mounts it, and
//! only a namespace unclaimed for longer than any test takes to claim one is
//! left behind.

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

/// Where each live [`TestNamespaces`] lists the namespaces it claims, one
/// file per guard, named as [`test_cgroups::register_in`] names them.
const NAMESPACE_REGISTRY: &str = "/run/reliaburger-tests/netns";

/// How old a namespace no guard claims must be before the sweep counts it as
/// left behind. A test claims its namespaces before it makes them, so a
/// young unclaimed one belongs to a test that has no guard, still running.
pub const STALE_AFTER: Duration = test_cgroups::STALE_AFTER;

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

/// A guard over network namespaces a test makes itself, outside a runtime
/// root's address pool. Dropping it deletes them and their host veths.
pub struct TestNamespaces {
    names: Vec<String>,
    entry: Option<PathBuf>,
}

impl TestNamespaces {
    /// Sweep stale test leftovers, then claim the namespaces `names` (such
    /// as `rb-rbtest-owned-net-<pid>`). Claim them before making them.
    pub fn new(names: impl IntoIterator<Item = String>) -> Self {
        sweep_stale();
        let names: Vec<String> = names.into_iter().collect();
        let entry = test_cgroups::register_in(Path::new(NAMESPACE_REGISTRY), &names);
        Self { names, entry }
    }

    /// Claim the namespace of test instance `instance`.
    pub fn instance(instance: &InstanceId) -> Self {
        Self::new([format!("rb-{}", instance.0)])
    }

    /// Give the namespaces up the way a SIGKILLed test does: the entry
    /// stays but names an owner that no longer runs, and nothing is
    /// released. Only for testing the sweep.
    pub fn abandon(mut self) {
        if let Some(entry) = self.entry.take() {
            let name = entry.file_name().unwrap().to_string_lossy().into_owned();
            let mut parts = name.splitn(3, '-');
            let (pid, _, n) = (parts.next(), parts.next(), parts.next());
            // No process started at tick 0 is still this pid's.
            let dead = format!("{}-0-{}", pid.unwrap(), n.unwrap());
            std::fs::rename(&entry, entry.with_file_name(dead)).unwrap();
        }
        std::mem::forget(self);
    }
}

impl Drop for TestNamespaces {
    fn drop(&mut self) {
        for name in &self.names {
            if namespace_exists(name) {
                release_network(name);
            }
        }
        if let Some(entry) = &self.entry {
            let _ = std::fs::remove_file(entry);
        }
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

/// Release the roots and namespace claims of tests that died without their
/// guard's `Drop`, the test namespaces nothing claims that are older than
/// [`STALE_AFTER`], and the empty test cgroups no live guard claims.
pub fn sweep_stale() {
    sweep_stale_namespaces(STALE_AFTER);
    test_cgroups::sweep_stale();
}

/// [`sweep_stale`]'s namespace sweep, counting an unclaimed namespace as
/// left behind once it is `age` old. Only for testing the sweep, which
/// stands in this way for one that runs [`STALE_AFTER`] later.
pub fn sweep_stale_namespaces(age: Duration) {
    let mut live = claimed_namespaces();
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
        if !live.contains(&name) && older_than(&name, age) {
            eprintln!("runc residue: releasing stale test namespace {name}");
            release_network(&name);
        }
    }
}

/// The namespaces live [`TestNamespaces`] guards claim. The namespaces of
/// guards whose owner has gone are released, and their entries removed.
fn claimed_namespaces() -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    let registry = Path::new(NAMESPACE_REGISTRY);
    for entry in std::fs::read_dir(registry).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if test_cgroups::entry_alive(&name) {
            live.extend(body.lines().map(str::to_owned));
            continue;
        }
        for namespace in body.lines() {
            if namespace.starts_with(TEST_NAMESPACE_PREFIX) && namespace_exists(namespace) {
                eprintln!("runc residue: releasing namespace {namespace} of a dead test");
                release_network(namespace);
            }
        }
        let _ = std::fs::remove_file(entry.path());
    }
    live
}

/// Whether network namespace `name` exists.
fn namespace_exists(name: &str) -> bool {
    Path::new("/var/run/netns").join(name).exists()
}

/// Whether namespace `name` was made at least `age` ago. Its mount carries
/// the namespace inode's time, which the kernel sets when the namespace is
/// first mounted and never changes. One whose age can't be read is young.
fn older_than(name: &str, age: Duration) -> bool {
    std::fs::metadata(Path::new("/var/run/netns").join(name))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|made| made.elapsed().ok())
        .is_some_and(|elapsed| elapsed >= age)
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
