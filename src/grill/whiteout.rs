//! OCI whiteouts, applied to an unpacked rootfs.
//!
//! A layer deletes something from the layers below it with a marker entry:
//! `dir/.wh.name` removes `dir/name`, and `dir/.wh..wh..opq` (an *opaque*
//! whiteout) empties `dir`. The [OCI image spec] adds one rule: a whiteout
//! only hides lower layers, never entries of its own layer.
//!
//! Two things make this more than a `remove_dir_all`:
//!
//! - **Symlinks in the rootfs belong to the image.** A lower layer can make
//!   `x` a symlink to `/etc`, so resolving `rootfs/x/.wh.passwd` with
//!   ordinary path calls would reach the host's `/etc/passwd`. Every step
//!   here goes through a directory descriptor with "don't follow symlinks"
//!   flags, starting at the rootfs, so a whiteout only reaches what is
//!   really beneath it. A whiteout whose parent is a symlink, a file or
//!   missing has nothing to remove.
//! - **Order inside a layer is not fixed.** Builders usually put a whiteout
//!   before its siblings, but nothing forces them to. The unpacker records
//!   the paths the current layer has written ([`LayerEntries`]) and a
//!   whiteout leaves those alone, so it removes the same things whether it
//!   comes first or last. containerd's applier does the same.
//!
//! [OCI image spec]: https://github.com/opencontainers/image-spec/blob/main/layer.md#whiteouts

use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::ops::Bound;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat};
use nix::sys::stat::{Mode, SFlag, fstatat};
use nix::unistd::{UnlinkatFlags, unlinkat};

/// The file name of an opaque whiteout.
const OPAQUE: &[u8] = b".wh..wh..opq";

/// The prefix of a plain whiteout's file name.
const PREFIX: &[u8] = b".wh.";

/// A whiteout: what a layer deletes from the layers below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Whiteout {
    /// Remove `directory/name` from the lower layers.
    Remove { directory: PathBuf, name: CString },
    /// Remove everything under `directory` that the lower layers put there.
    Opaque { directory: PathBuf },
}

/// What a layer entry is, judged from its path alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerEntry {
    /// A file, directory or link to unpack.
    Ordinary,
    /// A whiteout to apply.
    Whiteout(Whiteout),
    /// A whiteout whose path climbs with `..`, or that names `.` or `..`:
    /// the unpacker skips it.
    Unsafe,
}

impl LayerEntry {
    /// Classify a layer entry by its path.
    pub fn classify(path: &Path) -> Self {
        let Some(file_name) = path.file_name() else {
            return LayerEntry::Ordinary;
        };
        let bytes = file_name.as_bytes();
        let name = if bytes == OPAQUE {
            None
        } else if let Some(name) = bytes.strip_prefix(PREFIX) {
            Some(name)
        } else {
            return LayerEntry::Ordinary;
        };
        let Some(directory) = path.parent().and_then(relative_path) else {
            return LayerEntry::Unsafe;
        };
        let whiteout = match name {
            None => Whiteout::Opaque { directory },
            Some(b"" | b"." | b"..") => return LayerEntry::Unsafe,
            Some(name) => match CString::new(name) {
                Ok(name) => Whiteout::Remove { directory, name },
                Err(_) => return LayerEntry::Unsafe,
            },
        };
        LayerEntry::Whiteout(whiteout)
    }
}

/// Normalise a layer entry's path to plain components under the rootfs.
///
/// `./a//b` and `/a/b` both become `a/b`, as the `tar` crate's `unpack_in`
/// reads them. A path with `..` is refused (`None`).
pub fn relative_path(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            Component::ParentDir => return None,
        }
    }
    Some(out)
}

/// The paths the layer being unpacked has written so far, relative to the
/// rootfs. A whiteout in the same layer leaves these alone.
#[derive(Debug, Default)]
pub struct LayerEntries {
    paths: BTreeSet<PathBuf>,
}

impl LayerEntries {
    /// Record an entry the current layer has unpacked.
    pub fn insert(&mut self, path: PathBuf) {
        self.paths.insert(path);
    }

    /// Whether this layer wrote `path` itself.
    fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }

    /// Whether this layer wrote `path` or anything beneath it.
    fn covers(&self, path: &Path) -> bool {
        // `PathBuf` orders component by component, so everything beneath
        // `path` sorts directly after it.
        self.paths
            .range::<Path, _>((Bound::Included(path), Bound::Unbounded))
            .next()
            .is_some_and(|first| first.starts_with(path))
    }
}

/// Apply one whiteout to the rootfs at `rootfs`, sparing `layer`'s own
/// entries.
///
/// An entry the unpacker isn't allowed to remove (`EACCES` or `EPERM`) is
/// kept with a warning; any other error fails the whiteout.
pub fn apply(rootfs: &Path, whiteout: &Whiteout, layer: &LayerEntries) -> std::io::Result<()> {
    let directory = match whiteout {
        Whiteout::Remove { directory, .. } | Whiteout::Opaque { directory } => directory,
    };
    let parent = match open_beneath(rootfs, directory) {
        Ok(Some(parent)) => parent,
        Ok(None) => return Ok(()),
        Err(errno) => return kept(directory, errno),
    };
    let pending = match whiteout {
        Whiteout::Remove { name, .. } => {
            vec![Pending::new(&Rc::new(parent), name.clone(), directory)]
        }
        Whiteout::Opaque { .. } => match children(parent, directory) {
            Ok(pending) => pending,
            Err(errno) => return kept(directory, errno),
        },
    };
    remove_lower(pending, layer)
}

/// The flags for walking to a directory without following a symlink in
/// its last component. Paths are opened one component at a time, so no
/// component anywhere is followed.
///
/// On Linux `O_PATH` opens a directory for `*at` calls only, which needs
/// the same search (`x`) permission a path lookup does, not read. macOS has
/// no `O_PATH`, so it opens the directory for reading.
fn walk_flags() -> OFlag {
    #[cfg(target_os = "linux")]
    let access = OFlag::O_PATH;
    #[cfg(not(target_os = "linux"))]
    let access = OFlag::O_RDONLY;
    access | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

/// Open `relative` under `rootfs`, one component at a time, never following
/// a symlink. `None` when a component is missing or isn't a real directory:
/// then the image has nothing there to remove.
fn open_beneath(rootfs: &Path, relative: &Path) -> Result<Option<OwnedFd>, Errno> {
    let mut directory = open(rootfs, walk_flags(), Mode::empty())?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        directory = match openat(&directory, name, walk_flags(), Mode::empty()) {
            Ok(next) => next,
            Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => return Ok(None),
            Err(errno) => return Err(errno),
        };
    }
    Ok(Some(directory))
}

/// One name under an open directory, waiting to be removed.
struct Pending {
    /// The directory holding `name`. `Rc` (a reference-counted pointer)
    /// lets siblings share it; it closes when the last one is done.
    parent: Rc<OwnedFd>,
    name: CString,
    /// `name`'s path relative to the rootfs, for [`LayerEntries`] lookups.
    path: PathBuf,
    /// Set once the children are queued: on the second visit they have
    /// been handled and the directory itself can go.
    emptied: bool,
}

impl Pending {
    fn new(parent: &Rc<OwnedFd>, name: CString, directory: &Path) -> Self {
        let path = directory.join(OsStr::from_bytes(name.as_bytes()));
        Pending {
            parent: Rc::clone(parent),
            name,
            path,
            emptied: false,
        }
    }
}

/// Queue every entry of the open `directory`, which sits at `path` relative
/// to the rootfs. Listing needs read permission on the directory itself.
fn children(directory: OwnedFd, path: &Path) -> Result<Vec<Pending>, Errno> {
    let mut names = Vec::new();
    let read = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let mut listing = Dir::openat(&directory, c".", read, Mode::empty())?;
    for entry in listing.iter() {
        let name = entry?.file_name().to_owned();
        if name.as_bytes() != b"." && name.as_bytes() != b".." {
            names.push(name);
        }
    }
    let directory = Rc::new(directory);
    Ok(names
        .into_iter()
        .map(|name| Pending::new(&directory, name, path))
        .collect())
}

/// Remove each pending entry and, for a directory, everything beneath it,
/// except what the current layer wrote.
///
/// An explicit stack instead of recursion: an image can nest directories
/// deeply, and that should fail with an error, not overflow Bun's stack.
fn remove_lower(mut pending: Vec<Pending>, layer: &LayerEntries) -> std::io::Result<()> {
    while let Some(mut item) = pending.pop() {
        if item.emptied {
            // The layer wrote something inside, so the directory stays.
            if !layer.covers(&item.path) {
                unlink(&item, UnlinkatFlags::RemoveDir)?;
            }
            continue;
        }
        let stat = match fstatat(
            &*item.parent,
            item.name.as_c_str(),
            AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => continue,
            Err(errno) => return Err(errno.into()),
        };
        let is_directory =
            SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR;
        if !is_directory {
            // A symlink is removed as a link: its target is never touched.
            if !layer.contains(&item.path) {
                unlink(&item, UnlinkatFlags::NoRemoveDir)?;
            }
            continue;
        }
        let queued = openat(
            &*item.parent,
            item.name.as_c_str(),
            walk_flags(),
            Mode::empty(),
        )
        .and_then(|directory| children(directory, &item.path));
        let queued = match queued {
            Ok(queued) => queued,
            Err(Errno::ENOENT) => continue,
            // Unreadable: the directory and what's in it stay.
            Err(errno) => {
                kept(&item.path, errno)?;
                continue;
            }
        };
        item.emptied = true;
        pending.push(item);
        pending.extend(queued);
    }
    Ok(())
}

fn unlink(item: &Pending, flags: UnlinkatFlags) -> std::io::Result<()> {
    match unlinkat(&*item.parent, item.name.as_c_str(), flags) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        // A child was kept (and warned about), so its directory stays too.
        Err(Errno::ENOTEMPTY) if matches!(flags, UnlinkatFlags::RemoveDir) => Ok(()),
        Err(errno) => kept(&item.path, errno),
    }
}

/// Decide what an error removing `path` means.
///
/// Root removes anything, but a rootless Bun unpacks as an ordinary user,
/// and an image may make a lower directory read-only (`0555`) or
/// unreadable. Then the entry stays in the rootfs, as it would with a
/// failed `rm`, and the pull carries on: the leftover is still inside the
/// rootfs, and failing would refuse an image other runtimes run. Any other
/// error (a full disk, an I/O error) fails the unpack.
fn kept(path: &Path, errno: Errno) -> std::io::Result<()> {
    match errno {
        Errno::EACCES | Errno::EPERM => {
            eprintln!(
                "image: whiteout kept {} in the rootfs: {errno}",
                path.display()
            );
            Ok(())
        }
        errno => Err(errno.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_whiteout_names_its_directory_and_target() {
        assert_eq!(
            LayerEntry::classify(Path::new("./etc/.wh.passwd")),
            LayerEntry::Whiteout(Whiteout::Remove {
                directory: PathBuf::from("etc"),
                name: CString::new("passwd").unwrap(),
            })
        );
    }

    #[test]
    fn opaque_whiteout_names_its_directory() {
        assert_eq!(
            LayerEntry::classify(Path::new("var/cache/.wh..wh..opq")),
            LayerEntry::Whiteout(Whiteout::Opaque {
                directory: PathBuf::from("var/cache"),
            })
        );
    }

    #[test]
    fn absolute_whiteouts_are_read_relative_to_the_rootfs() {
        assert_eq!(
            LayerEntry::classify(Path::new("/etc/.wh.passwd")),
            LayerEntry::Whiteout(Whiteout::Remove {
                directory: PathBuf::from("etc"),
                name: CString::new("passwd").unwrap(),
            })
        );
    }

    #[test]
    fn ordinary_entries_are_not_whiteouts() {
        assert_eq!(
            LayerEntry::classify(Path::new("etc/passwd")),
            LayerEntry::Ordinary
        );
        assert_eq!(
            LayerEntry::classify(Path::new("etc/")),
            LayerEntry::Ordinary
        );
    }

    #[test]
    fn whiteouts_with_parent_components_are_refused() {
        for path in ["../etc/.wh.passwd", "a/../../.wh..wh..opq", "etc/.wh..."] {
            assert_eq!(
                LayerEntry::classify(Path::new(path)),
                LayerEntry::Unsafe,
                "{path}"
            );
        }
    }

    #[test]
    fn layer_entries_cover_their_ancestors_only() {
        let mut layer = LayerEntries::default();
        layer.insert(PathBuf::from("a/b/c"));
        assert!(layer.covers(Path::new("a")));
        assert!(layer.covers(Path::new("a/b")));
        assert!(layer.covers(Path::new("a/b/c")));
        assert!(!layer.covers(Path::new("a/b/c2")));
        assert!(!layer.covers(Path::new("a-b")));
        assert!(!layer.covers(Path::new("b")));
        assert!(layer.contains(Path::new("a/b/c")));
        assert!(!layer.contains(Path::new("a/b")));
    }
}
