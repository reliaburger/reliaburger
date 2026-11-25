//! The app and namespace cgroups under [`CGROUP_ROOT`] that this Bun puts
//! workloads in, and their removal once the last workload in one has gone
//! (#2324).
//!
//! A workload's cgroup is `<root>/<namespace>/<group>/<leaf>`. The group is
//! the app's name, or an executor's `executor-<prefix>[-reuse|-host]`. The
//! runtime removes the leaf when it retires the workload (runc on `delete`,
//! the process owner before it reports `Retired`, the executor pool when it
//! retires a slot), but nothing removed the group or the namespace above
//! it. A node that ran many short-lived namespaces, such as `relish test`'s
//! `rbtest-*` runs, gathered one empty namespace cgroup per run.
//!
//! Removing a parent cgroup races a launch into it. runc and the executor
//! pool make a workload's cgroup one directory at a time and enable
//! controllers in each parent on the way down; if the namespace directory
//! is removed between two of those steps, the launch fails. So every
//! launcher takes a [`WorkloadGroupHold`] on its `(namespace, group)` before
//! it creates anything, and keeps it until its workload's leaf has gone.
//! Releasing the last hold on a group removes the group's empty tree,
//! children first; releasing the last hold in a namespace then removes the
//! namespace directory too. Both happen under the same lock `hold` takes, so
//! a launch either sees the directories gone or keeps them.
//!
//! The kernel refuses to remove a cgroup that still has processes or child
//! cgroups, so a busy group, or one another process made, is never removed:
//! the `rmdir` fails and the directory stays.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use super::cgroup::CGROUP_ROOT;

/// The namespace every node always has. Its directory stays: it's never
/// "deleted", and keeping it means a removal can't race the many launches
/// into it, including other Buns' on a shared test host.
const PERMANENT_NAMESPACE: &str = "default";

/// The process-wide registry for the real cgroup hierarchy. The agent, the
/// job runner and the executor pools launch into the same namespaces, so
/// they must share one.
static SHARED: LazyLock<Arc<WorkloadGroups>> = LazyLock::new(|| WorkloadGroups::new(CGROUP_ROOT));

/// One app or executor group: `(namespace, group)`.
type GroupKey = (String, String);

/// Who in this process still launches workloads into each group, and
/// which released groups couldn't be removed yet.
#[derive(Debug)]
pub struct WorkloadGroups {
    root: PathBuf,
    // A std mutex: `release` runs from `Drop`, which can't await. Every
    // critical section is a map update plus a few `rmdir`s on cgroupfs,
    // an in-memory kernel interface, and never holds the lock across an await.
    state: Mutex<GroupState>,
}

#[derive(Debug, Default)]
struct GroupState {
    users: BTreeMap<GroupKey, usize>,
    /// Groups whose last hold went while their tree was still busy, for
    /// example a leaf whose last process hadn't been reaped. The last
    /// release in their namespace tries them again.
    unremoved: BTreeSet<GroupKey>,
}

impl WorkloadGroups {
    /// A registry for the hierarchy at `root`. Tests point it at a
    /// temporary directory; Bun uses [`WorkloadGroups::shared`].
    pub fn new(root: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            root: root.into(),
            state: Mutex::new(GroupState::default()),
        })
    }

    /// The registry for `/sys/fs/cgroup/reliaburger`, shared by everything
    /// in this process that launches workloads there.
    pub fn shared() -> Arc<Self> {
        Arc::clone(&SHARED)
    }

    /// The hierarchy this registry removes groups from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Hold `namespace`'s `group` open while a workload is launched into it
    /// and runs. Take it before creating any of the workload's cgroup.
    ///
    /// A name that isn't a workload label can't be one of our groups; its
    /// hold does nothing.
    pub fn hold(self: &Arc<Self>, namespace: &str, group: &str) -> WorkloadGroupHold {
        let key = (crate::config::valid_workload_label(namespace)
            && crate::config::valid_workload_label(group))
        .then(|| (namespace.to_string(), group.to_string()));
        if let Some(key) = &key {
            let mut state = self.lock();
            *state.users.entry(key.clone()).or_default() += 1;
            state.unremoved.remove(key);
        }
        WorkloadGroupHold {
            groups: Arc::clone(self),
            key,
        }
    }

    /// Remove `namespace`'s `group`, and the namespace after it, if nothing
    /// in this process holds them. For a workload this Bun adopted from an
    /// earlier run, which never took a hold.
    pub fn remove_if_idle(self: &Arc<Self>, namespace: &str, group: &str) {
        drop(self.hold(namespace, group));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GroupState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn release(&self, key: &GroupKey) {
        let mut state = self.lock();
        match state.users.get_mut(key) {
            Some(users) if *users > 1 => {
                *users -= 1;
                return;
            }
            Some(_) => {
                state.users.remove(key);
            }
            None => return,
        }
        let (namespace, group) = key;
        let namespace_dir = self.root.join(namespace);
        if !remove_empty_tree(&namespace_dir.join(group)) {
            state.unremoved.insert(key.clone());
        }
        if namespace == PERMANENT_NAMESPACE || state.users.keys().any(|(held, _)| held == namespace)
        {
            return;
        }
        let waiting: Vec<GroupKey> = state
            .unremoved
            .iter()
            .filter(|(held, _)| held == namespace)
            .cloned()
            .collect();
        for key in waiting {
            if remove_empty_tree(&namespace_dir.join(&key.1)) {
                state.unremoved.remove(&key);
            }
        }
        // Not the whole tree: a group another process made is its own to
        // remove. This only succeeds once the namespace has no group left.
        let _ = std::fs::remove_dir(&namespace_dir);
    }
}

/// A launcher's claim on one app or executor group. Dropping the last one
/// for a group removes the group's empty cgroups, and the namespace's once
/// no group in it is held.
#[derive(Debug)]
pub struct WorkloadGroupHold {
    groups: Arc<WorkloadGroups>,
    key: Option<GroupKey>,
}

impl Drop for WorkloadGroupHold {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.groups.release(&key);
        }
    }
}

/// Remove `path` and the cgroups below it, children first, and say whether
/// `path` is gone. A busy cgroup, or one with a busy child, stays, because
/// the kernel refuses the `rmdir`. A symbolic link is never followed.
fn remove_empty_tree(path: &Path) -> bool {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                remove_empty_tree(&entry.path());
            }
        }
    }
    match std::fs::remove_dir(path) {
        Ok(()) => true,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in hierarchy. A plain file inside a directory makes `rmdir`
    /// fail the way a process does in a real cgroup.
    fn hierarchy() -> (tempfile::TempDir, Arc<WorkloadGroups>) {
        let root = tempfile::tempdir().unwrap();
        let groups = WorkloadGroups::new(root.path());
        (root, groups)
    }

    fn make(root: &Path, path: &str) -> PathBuf {
        let path = root.join(path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn busy(leaf: &Path) {
        std::fs::write(leaf.join("cgroup.procs"), "4242\n").unwrap();
    }

    #[test]
    fn removing_the_last_app_removes_its_empty_namespace_cgroup() {
        let (root, groups) = hierarchy();
        let hold = groups.hold("rbtest-run-00", "web");
        // The runtime already removed the leaf; its parents are left.
        make(root.path(), "rbtest-run-00/web");
        drop(hold);
        assert!(!root.path().join("rbtest-run-00").exists());
    }

    #[test]
    fn a_namespace_with_another_app_keeps_its_cgroup() {
        let (root, groups) = hierarchy();
        let web = groups.hold("shop", "web");
        let api = groups.hold("shop", "api");
        make(root.path(), "shop/web");
        let api_leaf = make(root.path(), "shop/api/g1-0");
        busy(&api_leaf);
        drop(web);
        assert!(!root.path().join("shop/web").exists());
        assert!(api_leaf.exists());
        // The api app's leaf retires; its last hold then takes the rest.
        std::fs::remove_file(api_leaf.join("cgroup.procs")).unwrap();
        drop(api);
        assert!(!root.path().join("shop").exists());
    }

    #[test]
    fn a_group_with_another_hold_keeps_its_cgroup() {
        let (root, groups) = hierarchy();
        let old = groups.hold("shop", "web");
        let new = groups.hold("shop", "web");
        make(root.path(), "shop/web");
        drop(old);
        assert!(root.path().join("shop/web").exists());
        drop(new);
        assert!(!root.path().join("shop").exists());
    }

    #[test]
    fn a_busy_child_keeps_its_group_and_namespace_until_it_empties() {
        let (root, groups) = hierarchy();
        let hold = groups.hold("batch", "executor-abc-host");
        make(root.path(), "batch/executor-abc-host/0/helper");
        let task = make(root.path(), "batch/executor-abc-host/0/task");
        busy(&task);
        drop(hold);
        // Empty siblings go, children first; the busy leaf and its
        // ancestors stay.
        assert!(
            !root
                .path()
                .join("batch/executor-abc-host/0/helper")
                .exists()
        );
        assert!(task.exists());
        // Once it empties, the last release in the namespace retries it.
        std::fs::remove_file(task.join("cgroup.procs")).unwrap();
        let other = groups.hold("batch", "report");
        drop(other);
        assert!(!root.path().join("batch").exists());
    }

    #[test]
    fn a_group_made_by_someone_else_keeps_the_namespace() {
        let (root, groups) = hierarchy();
        let hold = groups.hold("shop", "web");
        make(root.path(), "shop/web");
        // Another Bun on this host, say, which this process never held.
        let foreign = make(root.path(), "shop/executor-other");
        drop(hold);
        assert!(!root.path().join("shop/web").exists());
        assert!(foreign.exists());
    }

    #[test]
    fn the_default_namespace_cgroup_stays() {
        let (root, groups) = hierarchy();
        let hold = groups.hold("default", "web");
        make(root.path(), "default/web/g1-0");
        drop(hold);
        assert!(!root.path().join("default/web").exists());
        assert!(root.path().join("default").exists());
    }

    #[test]
    fn an_adopted_workload_without_a_hold_is_removed_when_idle() {
        let (root, groups) = hierarchy();
        make(root.path(), "rbtest-run-01/cfg-app");
        groups.remove_if_idle("rbtest-run-01", "cfg-app");
        assert!(!root.path().join("rbtest-run-01").exists());

        // But never while a launcher holds it.
        let launching = groups.hold("rbtest-run-02", "cfg-app");
        make(root.path(), "rbtest-run-02/cfg-app");
        groups.remove_if_idle("rbtest-run-02", "cfg-app");
        assert!(root.path().join("rbtest-run-02/cfg-app").exists());
        drop(launching);
    }

    #[test]
    fn a_launch_into_the_namespace_holds_it_against_removal() {
        // The race the lock closes: the last app in a namespace retires
        // while another app's launch is between creating the namespace
        // directory and its own.
        let (root, groups) = hierarchy();
        let leaving = groups.hold("shop", "web");
        make(root.path(), "shop/web");
        let joining = groups.hold("shop", "api");
        make(root.path(), "shop");
        drop(leaving);
        assert!(root.path().join("shop").exists(), "namespace kept for api");
        drop(joining);
        assert!(!root.path().join("shop").exists());
    }

    #[test]
    fn names_that_are_not_workload_labels_are_never_touched() {
        let (root, groups) = hierarchy();
        let outside = make(root.path(), "victim");
        drop(groups.hold("..", "victim"));
        drop(groups.hold("shop", "../../victim"));
        groups.remove_if_idle("", "victim");
        assert!(outside.exists());
    }

    #[test]
    fn symbolic_links_are_not_followed() {
        let (root, groups) = hierarchy();
        let elsewhere = tempfile::tempdir().unwrap();
        let precious = make(elsewhere.path(), "precious");
        make(root.path(), "shop/web");
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("shop/web/link")).unwrap();
        drop(groups.hold("shop", "web"));
        assert!(precious.exists());
    }
}
