//! Cleanup for the cgroups Linux tests make (#2322).
//!
//! A cgroup v2 directory outlives the processes in it: when the last one
//! exits, the directory stays until someone removes it. Tests that start
//! real workloads make cgroups under Reliaburger's hierarchy
//! (`/sys/fs/cgroup/reliaburger/<namespace>/<app>/...`) or straight under
//! `/sys/fs/cgroup`, and Bun's own cleanup only removes the leaves it knows
//! about. A passing `make test-linux` left about 29 empty cgroups behind, and
//! a long-lived test VM had gathered hundreds.
//!
//! Two things clean them up here:
//!
//! - [`TestCgroups`] is a guard over the cgroups one test makes. Its `Drop`
//!   removes each one, children first, on a panic as on a pass. A claim is
//!   one cgroup tree, or a prefix ending in `*`: `/sys/fs/cgroup/rbtest-x*`
//!   covers `/sys/fs/cgroup/rbtest-x-unit` too, and `Drop` removes every
//!   top-level, namespace or app cgroup a prefix covers. A cgroup
//!   that still has processes stays: the kernel refuses to remove it, and
//!   we wait only a moment for its processes to finish exiting.
//! - Creating a guard first sweeps empty test-owned cgroups that no live
//!   guard claims and that haven't changed for [`STALE_AFTER`], which is
//!   what a test killed outright, or one without a guard, left behind.
//!
//! Only test-owned names are ever swept: a namespace or top-level cgroup
//! called `rbtest-*` or `cap-tmp*`, or an app called `rbtest-*` in any
//! namespace. A guard may also claim other paths it made, such as an
//! executor's slot groups in `default` (`.../default/executor-<prefix>`);
//! its `Drop` removes those, and the sweep never looks at them.
//!
//! A guard proves its owner is alive with a registry entry naming the owning
//! process and its start time, so a reused process ID doesn't count.
//!
//! Included with `#[path = "support/test_cgroups.rs"] mod test_cgroups;` from
//! a test binary, or `#[path = "../tests/support/test_cgroups.rs"]` under
//! `#[cfg(test)]` from the library's unit tests. It uses only `std`.

#![allow(dead_code)]

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The cgroup v2 mount.
pub const CGROUP_MOUNT: &str = "/sys/fs/cgroup";

/// Reliaburger's workload hierarchy, `CGROUP_ROOT` in `grill::cgroup`.
pub const HIERARCHY: &str = "/sys/fs/cgroup/reliaburger";

/// Where each live guard lists the cgroups it claims, one file per guard.
const REGISTRY: &str = "/run/reliaburger-tests/cgroups";

/// The name prefixes tests give the namespaces and cgroups they own.
const TEST_PREFIXES: [&str; 2] = ["rbtest-", "cap-tmp"];

/// How long a test-owned cgroup tree must have gone unchanged before the
/// sweep counts it as left behind. A test that makes a group without a
/// guard puts a process in it within moments; until then the group is
/// empty, and this keeps the sweep from removing it under the test.
pub const STALE_AFTER: Duration = Duration::from_secs(120);

/// How long `Drop` waits for a cgroup's last processes to exit.
const EMPTY_DEADLINE: Duration = Duration::from_secs(5);

/// Tells this process's guards apart in the registry.
static NEXT_GUARD: AtomicU64 = AtomicU64::new(0);

/// A guard that removes the cgroups one test makes when dropped.
pub struct TestCgroups {
    paths: Vec<PathBuf>,
    entry: Option<PathBuf>,
}

impl TestCgroups {
    /// Sweep stale test cgroups, then claim `paths`: absolute cgroup
    /// directories this test will make, or prefixes of their paths ending in
    /// `*`. Each claimed tree is removed with everything under it when the
    /// guard drops, as is every top-level, namespace or app cgroup a prefix
    /// covers. A claim must be unique to this test.
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        sweep_stale();
        let paths: Vec<PathBuf> = paths.into_iter().collect();
        let entry = register(&paths);
        Self { paths, entry }
    }

    /// Claim the namespace `name` in Reliaburger's hierarchy.
    pub fn namespace(name: &str) -> Self {
        Self::new([Path::new(HIERARCHY).join(name)])
    }

    /// Claim one more path.
    pub fn claim(&mut self, path: PathBuf) {
        self.paths.push(path);
        // The new entry exists before the old one goes, so the claim never
        // lapses.
        let previous = std::mem::replace(&mut self.entry, register(&self.paths));
        if let Some(previous) = previous {
            let _ = std::fs::remove_file(previous);
        }
    }

    /// Give the cgroups up the way a SIGKILLed test does: the claim goes,
    /// and nothing is removed. Only for testing the sweep.
    pub fn abandon(mut self) {
        if let Some(entry) = self.entry.take() {
            let _ = std::fs::remove_file(entry);
        }
        std::mem::forget(self);
    }
}

impl Drop for TestCgroups {
    fn drop(&mut self) {
        let covered = claimable_cgroups()
            .into_iter()
            .filter(|cgroup| self.paths.iter().any(|claim| covers(claim, cgroup)));
        let literal = self.paths.iter().filter(|claim| !is_prefix(claim)).cloned();
        let mut targets: Vec<PathBuf> = literal.chain(covered).collect();
        // Deepest first, so a claimed group inside another goes before it.
        targets.sort_by(|a, b| (b.components().count(), b).cmp(&(a.components().count(), a)));
        targets.dedup();
        for path in &targets {
            remove_tree(path, EMPTY_DEADLINE);
        }
        if let Some(entry) = &self.entry {
            let _ = std::fs::remove_file(entry);
        }
    }
}

/// Write a registry entry claiming `paths` for this process. `None` when the
/// registry can't be written, as on a host without `/run` access; the guard
/// still removes its cgroups on drop.
fn register(paths: &[PathBuf]) -> Option<PathBuf> {
    std::fs::create_dir_all(REGISTRY).ok()?;
    let pid = std::process::id();
    let started = process_start(pid)?;
    let name = format!(
        "{pid}-{started}-{}",
        NEXT_GUARD.fetch_add(1, Ordering::Relaxed)
    );
    let body: String = paths
        .iter()
        .map(|path| format!("{}\n", path.display()))
        .collect();
    // Write under a hidden name and rename, so a sweep never reads half an
    // entry and takes the guard for a dead one.
    let partial = Path::new(REGISTRY).join(format!(".{name}"));
    std::fs::write(&partial, body).ok()?;
    let entry = Path::new(REGISTRY).join(name);
    std::fs::rename(&partial, &entry).ok()?;
    Some(entry)
}

/// The start time of process `pid` in clock ticks since boot, field 22 of
/// `/proc/<pid>/stat`.
fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name in parentheses may hold spaces; the fields after its
    // closing parenthesis don't.
    let fields = stat.rsplit_once(')')?.1;
    fields.split_whitespace().nth(19)?.parse().ok()
}

/// Whether the process an entry named `pid-start-n` belongs to still runs.
fn entry_alive(name: &str) -> bool {
    let mut parts = name.split('-');
    let (Some(pid), Some(started)) = (parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(pid), Ok(started)) = (pid.parse::<u32>(), started.parse::<u64>()) else {
        return false;
    };
    process_start(pid) == Some(started)
}

/// The paths live guards claim. Entries of dead owners are removed.
fn live_claims() -> Vec<PathBuf> {
    let mut claims = Vec::new();
    for entry in std::fs::read_dir(REGISTRY).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if !entry_alive(&name) {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(entry.path()) {
            claims.extend(body.lines().map(PathBuf::from));
        }
    }
    claims
}

/// Remove the empty test-owned cgroups that no live guard claims and that
/// haven't changed for [`STALE_AFTER`].
pub fn sweep_stale() {
    let claims = live_claims();
    for cgroup in test_owned_cgroups() {
        // A claim on the cgroup, on a group inside it, or on a group around
        // it means a live test uses it.
        let claimed = claims
            .iter()
            .any(|claim| covers(claim, &cgroup) || claim.starts_with(&cgroup));
        if claimed || populated(&cgroup) || !unchanged_for(&cgroup, STALE_AFTER) {
            continue;
        }
        if remove_tree(&cgroup, Duration::ZERO) {
            eprintln!("test cgroups: removed stale {}", cgroup.display());
        }
    }
}

/// Whether `claim` names a prefix (it ends in `*`) rather than one tree.
fn is_prefix(claim: &Path) -> bool {
    claim.to_string_lossy().ends_with('*')
}

/// Whether `claim` covers `path`: `path` is the claimed group or inside it,
/// or, for a prefix claim, `path`'s text starts with the prefix.
fn covers(claim: &Path, path: &Path) -> bool {
    let text = claim.to_string_lossy();
    match text.strip_suffix('*') {
        Some(prefix) => path.to_string_lossy().starts_with(prefix),
        None => path.starts_with(claim),
    }
}

/// Whether `name` is a namespace or cgroup name only tests use.
pub fn test_owned_name(name: &str) -> bool {
    TEST_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// The host's test-owned cgroups: `rbtest-*` and `cap-tmp*` namespaces in
/// Reliaburger's hierarchy and directly under the mount, and `rbtest-*` apps
/// in any namespace.
pub fn test_owned_cgroups() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for parent in [CGROUP_MOUNT, HIERARCHY] {
        for (name, path) in child_groups(Path::new(parent)) {
            if test_owned_name(&name) {
                found.push(path);
            }
        }
    }
    for (namespace, path) in child_groups(Path::new(HIERARCHY)) {
        if test_owned_name(&namespace) {
            continue;
        }
        for (app, path) in child_groups(&path) {
            if app.starts_with("rbtest-") {
                found.push(path);
            }
        }
    }
    found
}

/// The cgroups a claim's prefix can cover: those directly under the mount,
/// and the namespaces and apps of Reliaburger's hierarchy. Only a guard's
/// own claims pick from these, so an executor's slot groups in `default`
/// can be claimed by the test that made them without the sweep ever
/// touching such a name.
fn claimable_cgroups() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = child_groups(Path::new(CGROUP_MOUNT))
        .into_iter()
        .map(|(_, path)| path)
        .collect();
    for (_, namespace) in child_groups(Path::new(HIERARCHY)) {
        found.extend(child_groups(&namespace).into_iter().map(|(_, path)| path));
        found.push(namespace);
    }
    found
}

/// The child cgroups of `path`, by name.
fn child_groups(path: &Path) -> Vec<(String, PathBuf)> {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .collect()
}

/// Whether a process runs anywhere in the cgroup tree at `path`. A tree
/// whose `cgroup.events` can't be read counts as populated, so it stays.
fn populated(path: &Path) -> bool {
    match std::fs::read_to_string(path.join("cgroup.events")) {
        Ok(events) => events.lines().any(|line| line == "populated 1"),
        Err(error) => error.kind() != ErrorKind::NotFound,
    }
}

/// Whether nothing in the cgroup tree at `path` has changed for `age`. A
/// group's modification time moves when a child group is made in it.
fn unchanged_for(path: &Path, age: Duration) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    if modified.elapsed().map_or(true, |elapsed| elapsed < age) {
        return false;
    }
    child_groups(path)
        .iter()
        .all(|(_, child)| unchanged_for(child, age))
}

/// Remove the cgroup tree at `path`, children first, waiting up to
/// `deadline` for processes still in it to exit. Returns whether `path` is
/// gone. A group something still holds stays: `rmdir` on a cgroup with
/// processes fails with `EBUSY`, so nothing in use is ever removed.
pub fn remove_tree(path: &Path, deadline: Duration) -> bool {
    let until = Instant::now() + deadline;
    loop {
        if remove_children_first(path) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn remove_children_first(path: &Path) -> bool {
    for (_, child) in child_groups(path) {
        remove_children_first(&child);
    }
    match std::fs::remove_dir(path) {
        Ok(()) => true,
        Err(error) => error.kind() == ErrorKind::NotFound,
    }
}
