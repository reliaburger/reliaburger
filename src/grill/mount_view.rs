//! The filesystem a host workload sees under `[process_workloads]
//! mount_isolation` (#714).
//!
//! A host workload (an `exec` or `script` app, a host job, the native host
//! executor) runs an allowlisted binary as root. Isolation gives it a mount
//! namespace of its own: the host's root, minus Bun's own paths, with a
//! private `/tmp`, `/var/tmp` and `/dev/shm`. The process owner's execution
//! gate builds it after joining the workload's cgroup and before `exec`, and
//! then drops the capabilities that would undo it.
//!
//! This module holds the view's description, which Bun computes once from
//! `node.toml` and every owner record carries, and the Linux system calls
//! that build it. It isn't a sandbox: see manual 13 for what the view stops
//! and what it doesn't.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Size of the one tmpfs a workload's `/tmp` and `/var/tmp` share.
pub const TMP_BYTES: u64 = 256 * 1024 * 1024;
/// Size of a workload's private `/dev/shm`.
pub const SHM_BYTES: u64 = 64 * 1024 * 1024;

/// Bun's runtime directory: its sockets and the host executors' control
/// sockets.
pub const RUN_DIRECTORY: &str = "/run/reliaburger";
/// Where the pinned eBPF maps (Onion, Smoker) live.
pub const BPF_DIRECTORY: &str = "/sys/fs/bpf";
/// Bun's conventional configuration directory, hidden whatever `--config`
/// names.
pub const CONFIG_DIRECTORY: &str = "/etc/reliaburger";

/// Whether a hidden path is a directory, covered by an empty tmpfs, or a
/// file, covered by `/dev/null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    /// Masked with an empty, read-only tmpfs.
    Directory,
    /// Masked with a read-only bind of `/dev/null`.
    File,
}

/// One path the workload must not see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HiddenPath {
    /// The path, with symlinks resolved.
    pub path: PathBuf,
    /// How it's masked.
    pub kind: PathKind,
}

/// A path under a hidden one that the workload needs anyway: its own
/// cgroup, or the native host executor's binary and socket. The gate takes
/// a copy of it before masking and attaches the copy back afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindBack {
    /// Where it is on the host, and where it reappears in the view.
    pub path: PathBuf,
    /// Whether the workload may write through it.
    pub writable: bool,
}

/// Everything the gate needs to build one workload's view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MountView {
    /// Bun's paths, hidden from the workload. No entry is under another.
    pub hidden: Vec<HiddenPath>,
    /// Paths under `hidden` the workload keeps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bind_backs: Vec<BindBack>,
    /// Size of the tmpfs behind `/tmp` and `/var/tmp`.
    pub tmp_bytes: u64,
    /// Size of the tmpfs on `/dev/shm`.
    pub shm_bytes: u64,
}

/// What a host launch does about mounts, as its owner record says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum MountIsolation {
    /// Build a fresh view: an app, a job attempt, a host executor.
    Build {
        /// The view to build.
        view: MountView,
    },
    /// Join the namespace of the running workload whose execution gate had
    /// this PID: `relish exec` and stop hooks see what their app sees,
    /// `/tmp` included.
    Join {
        /// The workload's root process, held unreaped by the parent owner.
        pid: u32,
    },
}

impl MountView {
    /// The view for a node: its storage, security and configuration paths,
    /// `/run/reliaburger`, `/sys/fs/bpf` and Bun's cgroup tree, each with
    /// its symlinks resolved. `config_file` is the `--config` Bun read.
    pub fn for_node(config: &crate::config::NodeConfig, config_file: Option<&Path>) -> Self {
        let storage = &config.storage;
        let security = &config.security;
        let mut hidden = Vec::new();
        for directory in [
            &storage.data,
            &storage.images,
            &storage.logs,
            &storage.metrics,
            &storage.volumes,
        ] {
            hidden.push(HiddenPath {
                path: within_buns_directory(directory),
                kind: PathKind::Directory,
            });
        }
        hidden.push(HiddenPath {
            path: security
                .identity_dir
                .clone()
                .unwrap_or_else(|| crate::sesame::identity_store::identity_dir(&storage.data)),
            kind: PathKind::Directory,
        });
        for file in [
            security.master_key_path.as_deref(),
            security.bootstrap_path.as_deref(),
            config_file,
        ]
        .into_iter()
        .flatten()
        {
            hidden.push(file_or_its_directory(file));
        }
        for directory in [
            CONFIG_DIRECTORY,
            RUN_DIRECTORY,
            BPF_DIRECTORY,
            super::cgroup::CGROUP_ROOT,
        ] {
            hidden.push(HiddenPath {
                path: PathBuf::from(directory),
                kind: PathKind::Directory,
            });
        }
        Self {
            hidden: collapse(hidden),
            bind_backs: Vec::new(),
            tmp_bytes: TMP_BYTES,
            shm_bytes: SHM_BYTES,
        }
    }

    /// This view with `extra` bound back as well, for one launch.
    pub fn with_bind_backs(mut self, extra: &[BindBack]) -> Self {
        self.bind_backs.extend(extra.iter().cloned());
        self
    }

    /// The bind-backs the gate attaches, with `own_cgroup` (the workload
    /// cgroup it joined) among them: only those a mask hides, and none
    /// under another, which already brings it back. Making a mountpoint
    /// inside a bound-back cgroup would create a real cgroup.
    pub fn bind_backs_to_attach(&self, own_cgroup: Option<&Path>) -> Vec<BindBack> {
        let own = own_cgroup.map(|path| BindBack {
            path: path.to_path_buf(),
            writable: true,
        });
        let mut backs: Vec<BindBack> = self
            .bind_backs
            .iter()
            .chain(own.iter())
            .filter(|back| self.hides(&back.path))
            .cloned()
            .collect();
        backs.sort_by(|left, right| left.path.cmp(&right.path));
        let mut kept: Vec<BindBack> = Vec::new();
        for back in backs {
            if !kept
                .iter()
                .any(|parent| back.path.starts_with(&parent.path))
            {
                kept.push(back);
            }
        }
        kept
    }

    /// Whether `path`, symlinks resolved, is hidden by this view.
    pub fn hides(&self, path: &Path) -> bool {
        let path = crate::bun::host_paths::resolve(path);
        self.hidden
            .iter()
            .any(|hidden| path.starts_with(&hidden.path))
    }
}

/// A storage directory, or its parent when that's Bun's own `reliaburger`
/// directory: the default layout hides `/var/lib/reliaburger` whole.
fn within_buns_directory(directory: &Path) -> PathBuf {
    match directory.parent() {
        Some(parent) if parent.file_name().is_some_and(|name| name == "reliaburger") => {
            parent.to_path_buf()
        }
        _ => directory.to_path_buf(),
    }
}

/// A file's directory when that's a `reliaburger` directory, which holds
/// nothing else; otherwise the file alone, since its directory may be one
/// the workload needs, such as an operator's home.
fn file_or_its_directory(file: &Path) -> HiddenPath {
    match file.parent() {
        Some(parent) if parent.file_name().is_some_and(|name| name == "reliaburger") => {
            HiddenPath {
                path: parent.to_path_buf(),
                kind: PathKind::Directory,
            }
        }
        _ => HiddenPath {
            path: file.to_path_buf(),
            kind: PathKind::File,
        },
    }
}

/// Resolve every path, then drop duplicates and any path under a hidden
/// directory, which its parent's mask already covers.
fn collapse(hidden: Vec<HiddenPath>) -> Vec<HiddenPath> {
    let mut resolved: Vec<HiddenPath> = hidden
        .into_iter()
        .map(|entry| HiddenPath {
            path: crate::bun::host_paths::resolve(&entry.path),
            kind: entry.kind,
        })
        .collect();
    resolved.sort_by(|left, right| left.path.cmp(&right.path));
    resolved.dedup_by(|later, earlier| later.path == earlier.path);
    let mut kept: Vec<HiddenPath> = Vec::new();
    for entry in resolved {
        let covered = kept.iter().any(|parent| {
            parent.kind == PathKind::Directory && entry.path.starts_with(&parent.path)
        });
        if !covered {
            kept.push(entry);
        }
    }
    kept
}

/// Whether this Bun can build the view: Linux, as root. A node that can't
/// refuses host workloads under `mount_isolation` rather than run them
/// without it.
pub fn mount_isolation_available() -> bool {
    cfg!(target_os = "linux") && nix::unistd::geteuid().is_root()
}

/// Why a node refuses host workloads under `mount_isolation`.
pub const UNAVAILABLE: &str = "this node can't provide [process_workloads] mount_isolation \
     (it needs Linux and a root Bun); set mount_isolation = false to run host workloads \
     unisolated, or give the workload an image";

#[cfg(target_os = "linux")]
pub(crate) use linux::{build, drop_capabilities, join};

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use nix::mount::{MntFlags, MsFlags, mount, umount2};
    use nix::sched::{CloneFlags, setns, unshare};

    use super::{BindBack, MountView, PathKind};

    // From <linux/mount.h>; the libc crate doesn't name them on every target.
    const OPEN_TREE_CLONE: nix::libc::c_uint = 1;
    const MOVE_MOUNT_F_EMPTY_PATH: nix::libc::c_uint = 0x4;

    /// The capabilities the gate drops from the bounding set before `exec`,
    /// from <linux/capability.h>: without them root can't unmount a mask,
    /// `setns` back to the host's namespace, read another process's root
    /// through `/proc`, load a module or BPF program, or reboot.
    const DROPPED_CAPABILITIES: [(nix::libc::c_ulong, &str); 5] = [
        (21, "CAP_SYS_ADMIN"),
        (19, "CAP_SYS_PTRACE"),
        (16, "CAP_SYS_MODULE"),
        (39, "CAP_BPF"),
        (22, "CAP_SYS_BOOT"),
    ];

    fn failed(step: impl std::fmt::Display) -> impl FnOnce(io::Error) -> io::Error {
        move |error| io::Error::other(format!("mount isolation: {step}: {error}"))
    }

    fn c_path(path: &Path) -> io::Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::other(format!("path {} contains NUL", path.display())))
    }

    /// `unshare(CLONE_NEWNS)` fails with `EINVAL` in a process whose threads
    /// share a filesystem context. The gate runs before any runtime starts,
    /// so this only catches a regression, with a clearer message.
    fn ensure_single_threaded() -> io::Result<()> {
        let threads = std::fs::read_dir("/proc/self/task")?.count();
        if threads != 1 {
            return Err(io::Error::other(format!(
                "the execution gate has {threads} threads; it must have one"
            )));
        }
        Ok(())
    }

    /// A detached copy of the mount at `path`, for attaching elsewhere.
    fn open_tree_clone(path: &Path) -> io::Result<OwnedFd> {
        let path = c_path(path)?;
        // SAFETY: open_tree reads a NUL-terminated path that outlives the
        // call and returns a new descriptor or -1; nothing else is touched.
        let fd = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_open_tree,
                nix::libc::AT_FDCWD,
                path.as_ptr(),
                OPEN_TREE_CLONE | nix::libc::O_CLOEXEC as nix::libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel just returned this descriptor and nothing else
        // owns it.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    /// Attach a detached mount at `target`.
    fn move_mount(tree: &OwnedFd, target: &Path) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let target = c_path(target)?;
        // SAFETY: both strings are NUL-terminated and outlive the call; the
        // descriptor is open for its duration.
        let result = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_move_mount,
                tree.as_raw_fd(),
                c"".as_ptr(),
                nix::libc::AT_FDCWD,
                target.as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn mount_tmpfs(target: &Path, flags: MsFlags, options: &str) -> io::Result<()> {
        mount(Some("tmpfs"), target, Some("tmpfs"), flags, Some(options)).map_err(io::Error::from)
    }

    fn remount_read_only(target: &Path) -> io::Result<()> {
        mount(
            None::<&str>,
            target,
            None::<&str>,
            MsFlags::MS_REMOUNT
                | MsFlags::MS_BIND
                | MsFlags::MS_RDONLY
                | MsFlags::MS_NOSUID
                | MsFlags::MS_NODEV,
            None::<&str>,
        )
        .map_err(io::Error::from)
    }

    fn is_directory(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
    }

    /// Build `view` in a new mount namespace for the calling process.
    /// `own_cgroup` is the workload cgroup the gate joined, which it keeps.
    ///
    /// The order matters: copies of the bind-backs are taken before the
    /// masks hide their sources, and attached after.
    pub(crate) fn build(view: &MountView, own_cgroup: Option<&Path>) -> io::Result<()> {
        ensure_single_threaded().map_err(failed("cannot start"))?;
        unshare(CloneFlags::CLONE_NEWNS)
            .map_err(io::Error::from)
            .map_err(failed("cannot create a mount namespace"))?;
        // Slave, not private: Bun's later unmounts still reach this
        // namespace, so a long-running workload doesn't pin every volume
        // and container root that existed when it started.
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_SLAVE,
            None::<&str>,
        )
        .map_err(io::Error::from)
        .map_err(failed("cannot make / a slave"))?;

        let backs = view.bind_backs_to_attach(own_cgroup);
        let mut copies = Vec::new();
        for back in &backs {
            let copy = open_tree_clone(&back.path)
                .map_err(failed(format!("cannot keep {}", back.path.display())))?;
            copies.push((back, copy, is_directory(&back.path)));
        }

        private_temp(view).map_err(failed("cannot make a private /tmp"))?;

        let mut masked = Vec::new();
        for hidden in &view.hidden {
            let Ok(metadata) = std::fs::symlink_metadata(&hidden.path) else {
                continue;
            };
            let step = failed(format!("cannot hide {}", hidden.path.display()));
            match hidden.kind {
                PathKind::Directory if metadata.is_dir() => {
                    let traversable = backs.iter().any(|back| back.path.starts_with(&hidden.path));
                    // Mode 000 keeps out everyone but root; a mask that
                    // bind-backs sit under lets others walk to them, as the
                    // host helper's unprivileged user must.
                    let mode = if traversable { "0711" } else { "000" };
                    mount_tmpfs(
                        &hidden.path,
                        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
                        &format!("mode={mode},size=64k"),
                    )
                    .map_err(step)?;
                    masked.push(hidden.path.clone());
                }
                _ => {
                    mount(
                        Some("/dev/null"),
                        &hidden.path,
                        None::<&str>,
                        MsFlags::MS_BIND,
                        None::<&str>,
                    )
                    .map_err(io::Error::from)
                    .and_then(|()| remount_read_only(&hidden.path))
                    .map_err(step)?;
                }
            }
        }

        for (back, copy, directory) in &copies {
            attach(back, copy, *directory)
                .map_err(failed(format!("cannot keep {}", back.path.display())))?;
        }
        for path in masked {
            remount_read_only(&path).map_err(failed(format!(
                "cannot seal the mask on {}",
                path.display()
            )))?;
        }
        Ok(())
    }

    /// Make a mountpoint for `back` inside its mask and attach the copy.
    fn attach(back: &BindBack, copy: &OwnedFd, directory: bool) -> io::Result<()> {
        if let Some(parent) = back.path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o711)
                .create(parent)?;
        }
        if directory {
            std::fs::DirBuilder::new().mode(0o711).create(&back.path)?;
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&back.path)?;
        }
        move_mount(copy, &back.path)?;
        if !back.writable {
            remount_read_only(&back.path)?;
        }
        Ok(())
    }

    /// One tmpfs, two directories: `/tmp` and `/var/tmp` share its cap.
    /// A staging mount on `/tmp` holds both until detached copies of them
    /// are taken, then goes; the copies keep the tmpfs alive.
    fn private_temp(view: &MountView) -> io::Result<()> {
        let staging = Path::new("/tmp");
        mount_tmpfs(
            staging,
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            &format!("mode=0700,size={}", view.tmp_bytes),
        )?;
        let mut copies: Vec<(PathBuf, OwnedFd)> = Vec::new();
        for (name, target) in [("var-tmp", "/var/tmp"), ("tmp", "/tmp")] {
            let directory = staging.join(name);
            std::fs::create_dir(&directory)?;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o1777))?;
            // A `/var/tmp` that is a symlink (to `/tmp`, say) already lands
            // in the private one.
            if target == "/tmp" || is_directory(Path::new(target)) {
                copies.push((PathBuf::from(target), open_tree_clone(&directory)?));
            }
        }
        umount2(staging, MntFlags::MNT_DETACH)?;
        for (target, copy) in &copies {
            move_mount(copy, target)?;
        }
        let shm = Path::new("/dev/shm");
        if is_directory(shm) {
            mount_tmpfs(
                shm,
                MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
                &format!("mode=1777,size={}", view.shm_bytes),
            )?;
        }
        Ok(())
    }

    /// Join the mount namespace of the workload whose root process is
    /// `pid`, through a pidfd, so a recycled PID can't redirect it.
    pub(crate) fn join(pid: u32) -> io::Result<()> {
        ensure_single_threaded().map_err(failed("cannot start"))?;
        let pid = nix::libc::pid_t::try_from(pid)
            .map_err(|_| io::Error::other("mount isolation: invalid workload PID"))?;
        // SAFETY: pidfd_open takes a PID and flags and returns a new
        // descriptor or -1.
        let fd = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(failed("cannot find the workload")(
                io::Error::last_os_error(),
            ));
        }
        // SAFETY: the kernel just returned this descriptor and nothing else
        // owns it.
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        setns(&pidfd, CloneFlags::CLONE_NEWNS)
            .map_err(io::Error::from)
            .map_err(failed("cannot join the workload's mount namespace"))
    }

    /// Drop the capabilities that would undo the view from the bounding
    /// set. They stay in this process until it `exec`s; root's next program
    /// can't regain them. A kernel that doesn't know one (`CAP_BPF` before
    /// 5.8) never grants it either.
    pub(crate) fn drop_capabilities() -> io::Result<()> {
        for (capability, name) in DROPPED_CAPABILITIES {
            // SAFETY: PR_CAPBSET_DROP takes a capability number and changes
            // only this thread's bounding set.
            let result =
                unsafe { nix::libc::prctl(nix::libc::PR_CAPBSET_DROP, capability, 0, 0, 0) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(nix::libc::EINVAL) {
                    return Err(failed(format!("cannot drop {name}"))(error));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NodeConfig;

    fn resolved(path: &Path) -> PathBuf {
        crate::bun::host_paths::resolve(path)
    }

    fn paths(view: &MountView) -> Vec<PathBuf> {
        view.hidden
            .iter()
            .map(|hidden| hidden.path.clone())
            .collect()
    }

    #[test]
    fn mount_view_hides_every_storage_directory_and_the_config_directory() {
        let roots = [
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        ];
        let mut config = NodeConfig::default();
        config.storage.data = roots[0].path().join("data");
        config.storage.images = roots[1].path().join("images");
        config.storage.logs = roots[1].path().join("logs");
        config.storage.metrics = roots[2].path().join("metrics");
        config.storage.volumes = roots[2].path().join("volumes");
        let view = MountView::for_node(&config, None);
        let hidden = paths(&view);
        for directory in [
            &config.storage.data,
            &config.storage.images,
            &config.storage.logs,
            &config.storage.metrics,
            &config.storage.volumes,
        ] {
            assert!(
                hidden.contains(&resolved(directory)),
                "{directory:?} in {hidden:?}"
            );
        }
        for directory in [
            "/etc/reliaburger",
            "/run/reliaburger",
            "/sys/fs/bpf",
            "/sys/fs/cgroup/reliaburger",
        ] {
            assert!(
                hidden.contains(&resolved(Path::new(directory))),
                "{directory} in {hidden:?}"
            );
        }
        // The identity directory sits under data, which already hides it.
        assert!(view.hides(&config.storage.data.join("identity")));
        assert_eq!(view.tmp_bytes, 256 * 1024 * 1024);
        assert_eq!(view.shm_bytes, 64 * 1024 * 1024);
        assert!(view.bind_backs.is_empty());
    }

    #[test]
    fn the_default_layout_hides_buns_whole_directory() {
        let view = MountView::for_node(&NodeConfig::default(), None);
        let hidden = paths(&view);
        assert!(
            hidden.contains(&resolved(Path::new("/var/lib/reliaburger"))),
            "{hidden:?}"
        );
        assert!(!hidden.contains(&resolved(Path::new("/var/lib/reliaburger/data"))));
        assert!(view.hides(Path::new(
            "/var/lib/reliaburger/data/instances/a/owner.json"
        )));
        assert!(!view.hides(Path::new("/var/lib/shipper")));
        assert!(!view.hides(Path::new("/usr/bin/restic")));
    }

    #[test]
    fn mount_view_resolves_symlinks_before_it_records_a_path() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real-data");
        std::fs::create_dir(&real).unwrap();
        let link = root.path().join("data");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut config = NodeConfig::default();
        config.storage.data = link.clone();
        let view = MountView::for_node(&config, None);
        let hidden = paths(&view);
        assert!(
            hidden.contains(&std::fs::canonicalize(&real).unwrap()),
            "{hidden:?}"
        );
        assert!(!hidden.contains(&link), "{hidden:?}");
    }

    #[test]
    fn mount_view_collapses_paths_under_a_hidden_parent() {
        let root = tempfile::tempdir().unwrap();
        let mut config = NodeConfig::default();
        config.storage.data = root.path().join("data");
        config.security.identity_dir = Some(root.path().join("data/keys/identity"));
        config.storage.volumes = root.path().join("data/volumes");
        let view = MountView::for_node(&config, None);
        let hidden = paths(&view);
        let data = resolved(&config.storage.data);
        assert!(hidden.contains(&data), "{hidden:?}");
        assert!(
            hidden
                .iter()
                .all(|path| path == &data || !path.starts_with(&data)),
            "{hidden:?}"
        );
        // No entry appears twice.
        let mut unique = hidden.clone();
        unique.dedup();
        assert_eq!(unique.len(), hidden.len());
    }

    #[test]
    fn a_config_file_outside_etc_reliaburger_is_hidden_as_a_file() {
        let home = tempfile::tempdir().unwrap();
        let file = home.path().join("node.toml");
        std::fs::write(&file, "").unwrap();
        let view = MountView::for_node(&NodeConfig::default(), Some(&file));
        let entry = view
            .hidden
            .iter()
            .find(|hidden| hidden.path == resolved(&file))
            .expect("the config file is hidden");
        assert_eq!(entry.kind, PathKind::File);
        // Its directory isn't: it may be one the workload needs.
        assert!(!view.hides(&home.path().join("other")));

        let default = MountView::for_node(
            &NodeConfig::default(),
            Some(Path::new("/etc/reliaburger/node.toml")),
        );
        assert!(
            !default
                .hidden
                .iter()
                .any(|hidden| hidden.kind == PathKind::File),
            "{:?}",
            default.hidden
        );
    }

    #[test]
    fn a_master_key_beside_other_files_is_hidden_alone() {
        let home = tempfile::tempdir().unwrap();
        let mut config = NodeConfig::default();
        config.security.master_key_path = Some(home.path().join("master.key"));
        let view = MountView::for_node(&config, None);
        assert!(view.hides(&home.path().join("master.key")));
        assert!(!view.hides(&home.path().join("notes.txt")));
    }

    #[test]
    fn a_launch_adds_its_own_bind_backs() {
        let view = MountView::for_node(&NodeConfig::default(), None).with_bind_backs(&[BindBack {
            path: "/run/reliaburger/host-executors/abc".into(),
            writable: true,
        }]);
        assert_eq!(view.bind_backs.len(), 1);
        assert!(view.bind_backs[0].writable);
    }

    #[test]
    fn only_hidden_outermost_bind_backs_are_attached() {
        let mut config = NodeConfig::default();
        config.storage.data = "/srv/rb/data".into();
        let view = MountView::for_node(&config, None).with_bind_backs(&[
            BindBack {
                path: "/sys/fs/cgroup/reliaburger/executors/a".into(),
                writable: true,
            },
            BindBack {
                path: "/usr/share/visible-anyway".into(),
                writable: false,
            },
        ]);
        let attached = view.bind_backs_to_attach(Some(Path::new(
            "/sys/fs/cgroup/reliaburger/executors/a/helper",
        )));
        let paths: Vec<_> = attached.iter().map(|back| back.path.clone()).collect();
        assert_eq!(
            paths,
            vec![PathBuf::from("/sys/fs/cgroup/reliaburger/executors/a")],
            "the nested own cgroup and the unhidden path are left out"
        );
        assert_eq!(view.bind_backs_to_attach(None).len(), 1);
    }

    #[test]
    fn an_isolation_record_round_trips() {
        let view = MountView::for_node(&NodeConfig::default(), None).with_bind_backs(&[BindBack {
            path: "/var/lib/reliaburger/data/instances/host-executors/x".into(),
            writable: false,
        }]);
        for isolation in [
            MountIsolation::Build { view },
            MountIsolation::Join { pid: 42 },
        ] {
            let json = serde_json::to_string(&isolation).unwrap();
            let back: MountIsolation = serde_json::from_str(&json).unwrap();
            assert_eq!(back, isolation, "{json}");
        }
    }

    #[test]
    fn isolation_is_unavailable_off_linux_or_without_root() {
        let expected = cfg!(target_os = "linux") && nix::unistd::geteuid().is_root();
        assert_eq!(mount_isolation_available(), expected);
        assert!(UNAVAILABLE.contains("mount_isolation"));
    }

    /// The gate runs before any runtime starts. A threaded process can't
    /// `unshare` its mount namespace, so building refuses one by name
    /// before touching anything.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_execution_gate_refuses_to_build_a_view_with_threads() {
        let (stop, wait) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        let error = build(&MountView::for_node(&NodeConfig::default(), None), None).unwrap_err();
        let _ = stop.send(());
        thread.join().unwrap();
        assert!(error.to_string().contains("threads"), "{error}");
    }
}
