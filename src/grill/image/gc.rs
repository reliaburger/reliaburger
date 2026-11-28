//! Collection of unpacked rootfs generations, cached manifests and pin
//! metadata that no instance uses any more (#2423).
//!
//! Pickle's garbage collection expires compressed blobs. Everything an
//! unpack leaves beside them is removed here: the `gen-*` trees (several
//! times the size of their blobs), the manifests cached per tag and the
//! verified metadata that lets a pinned pull resolve without a registry.
//! The functions are synchronous: [`super::ImageStore::collect_rootfs_garbage`]
//! runs them on a blocking thread.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use oci_client::manifest::OciManifest;

/// What one collection pass removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootfsGcReport {
    /// Published generations (with a `.complete` marker) no instance used.
    pub generations_removed: usize,
    /// Marker-less trees an interrupted unpack left, older than the grace.
    pub partials_removed: usize,
    /// Cached manifests whose image has no unpacked tree left.
    pub manifests_removed: usize,
    /// Pin metadata files no remaining pinned image refers to.
    pub metadata_removed: usize,
}

/// Why a node's last image collection pass didn't do its work (#2524).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionProblem {
    /// The pass couldn't tell which generations instances use (a bundle's
    /// spec it couldn't read), so it removed nothing.
    Refused,
    /// The pass started and failed partway, on a filesystem error.
    Failed,
}

/// What `relish images` and `relish wtf` read about one node's image
/// store: where it is, and whether its collection passes keep failing.
/// Clones share one record.
///
/// A std mutex: it's held only to read or replace one small value, in
/// synchronous code, never across an `.await`.
#[derive(Debug, Clone, Default)]
pub struct ImageStoreStatus {
    root: Option<PathBuf>,
    problem: std::sync::Arc<std::sync::Mutex<Option<(CollectionProblem, u64)>>>,
}

// Two handles are equal when they share one record. Needed because the
// diagnostics' static evidence compares equal by value.
impl PartialEq for ImageStoreStatus {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.problem, &other.problem)
    }
}

impl Eq for ImageStoreStatus {}

impl ImageStoreStatus {
    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            root: Some(root),
            problem: std::sync::Arc::default(),
        }
    }

    /// Record how a pass at `now` (Unix seconds) went: a problem starts (or
    /// continues, keeping when it started) a record, `None` clears it.
    pub(super) fn record(&self, problem: Option<CollectionProblem>, now: u64) {
        let mut current = self
            .problem
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = match (problem, *current) {
            (None, _) => None,
            (Some(problem), Some((same, since))) if same == problem => Some((problem, since)),
            (Some(problem), _) => Some((problem, now)),
        };
    }

    /// The problem the passes have had since when, if the last one had one.
    pub fn problem(&self) -> Option<(CollectionProblem, u64)> {
        *self
            .problem
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Bytes on disk of each image unpacked here, keyed by the manifest
    /// digest it was pulled by (`sha256:…`), every generation of it
    /// summed. An image pulled by tag, and a generation published before
    /// sizes were recorded, aren't counted. Reads one marker per
    /// generation, on a blocking thread.
    pub async fn unpacked_sizes(&self) -> io::Result<HashMap<String, u64>> {
        let Some(root) = self.root.clone() else {
            return Ok(HashMap::new());
        };
        tokio::task::spawn_blocking(move || unpacked_sizes(&root.join("rootfs")))
            .await
            .map_err(io::Error::other)?
    }

    /// Total bytes of every generation whose size is recorded.
    pub async fn unpacked_total(&self) -> io::Result<u64> {
        Ok(self.unpacked_sizes().await?.values().sum())
    }
}

/// The marker a published generation gets: its unpacked size on the
/// second line, so listing sizes never walks a tree.
pub(super) fn marker_contents(bytes: u64) -> Vec<u8> {
    format!("complete\n{bytes}\n").into_bytes()
}

/// The size a generation's marker records, if it records one.
fn recorded_size(generation: &Path) -> Option<u64> {
    let marker = std::fs::read_to_string(generation.with_extension("complete")).ok()?;
    marker.lines().nth(1)?.trim().parse().ok()
}

/// Bytes of file content under `tree`, not following symlinks.
pub(super) fn tree_bytes(tree: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    let mut pending = vec![tree.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

fn unpacked_sizes(rootfs: &Path) -> io::Result<HashMap<String, u64>> {
    let mut sizes = HashMap::new();
    for generation in generations(rootfs)? {
        let Some(size) = recorded_size(&generation) else {
            continue;
        };
        // `rootfs_path` encodes the digest's colon.
        let Some(digest) = generation
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(|name| name.replace("%3A", ":"))
            .filter(|name| name.starts_with("sha256:"))
        else {
            continue;
        };
        *sizes.entry(digest).or_insert(0) += size;
    }
    Ok(sizes)
}

/// Whether a directory name is one [`super::ImageStore::rootfs_generation_path`]
/// makes: `gen-` and 16 hex digits, optionally `-owner-` and a number. The
/// pattern is strict so a repository called `gen-tools` is never mistaken
/// for a generation and removed.
pub(super) fn is_generation_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("gen-") else {
        return false;
    };
    let (hash, owner) = match rest.split_once("-owner-") {
        Some((hash, owner)) => (hash, Some(owner)),
        None => (rest, None),
    };
    let hex = hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit());
    let owner = owner
        .is_none_or(|owner| !owner.is_empty() && owner.bytes().all(|byte| byte.is_ascii_digit()));
    hex && owner
}

/// How long ago `path` was last modified. An unreadable time counts as old:
/// the caller has already ruled the entry out of every live set.
fn age(path: &Path) -> Duration {
    std::fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .unwrap_or(Duration::MAX)
}

fn remove_tree(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Every generation directory under `rootfs`, at whatever depth the
/// registry and repository put it. Generation trees are never entered.
pub(super) fn generations(rootfs: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![rootfs.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if entry.file_name().to_str().is_some_and(is_generation_name) {
                found.push(path);
            } else {
                pending.push(path);
            }
        }
    }
    Ok(found)
}

/// Take a generation out of publication by removing its `.complete`
/// marker, the only thing a pull checks before handing a generation out.
/// `true` when there was one. The caller holds the hand-out record's lock,
/// so no pull can claim the generation between its check and this.
pub(super) fn retire_marker(generation: &Path) -> io::Result<bool> {
    let marker = generation.with_extension("complete");
    if !marker.is_file() {
        return Ok(false);
    }
    remove_file(&marker)?;
    Ok(true)
}

/// Remove a generation's tree once its marker is gone. A crash before this
/// leaves a marker-less tree that the next unpack or the next pass clears,
/// never a marker over nothing. A tree that never had a marker (an unpack
/// that died) goes only once it is older than `grace`. `true` if removed.
pub(super) fn remove_tree_after(
    generation: &Path,
    published: bool,
    grace: Duration,
) -> io::Result<bool> {
    if !published && age(generation) < grace {
        return Ok(false);
    }
    remove_tree(generation)?;
    Ok(true)
}

/// Remove `.complete` markers whose generation directory is gone. A marker
/// alone would make the next pull hand out a path with nothing in it.
fn remove_orphan_markers(rootfs: &Path) -> io::Result<()> {
    let mut pending = vec![rootfs.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if entry.file_type()?.is_dir() {
                if !is_generation_name(name) {
                    pending.push(path);
                }
            } else if let Some(stem) = name.strip_suffix(".complete")
                && is_generation_name(stem)
                && !path.with_extension("").is_dir()
            {
                remove_file(&path)?;
            }
        }
    }
    Ok(())
}

/// Remove directories under `root` (never `root` itself) that are empty and
/// older than `grace`. The age check spares a directory an unpack has just
/// created for a generation it is about to fill.
fn remove_empty_directories(root: &Path, grace: Duration) -> io::Result<()> {
    fn sweep(directory: &Path, grace: Duration, is_root: bool) -> io::Result<bool> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error),
        };
        let mut empty = true;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let generation = entry.file_name().to_str().is_some_and(is_generation_name);
            if entry.file_type()?.is_dir() && !generation {
                if !sweep(&path, grace, false)? {
                    empty = false;
                }
            } else {
                empty = false;
            }
        }
        if empty && !is_root && age(directory) >= grace {
            // Another writer may have just put something here: then the
            // directory is in use, not an error.
            return Ok(std::fs::remove_dir(directory).is_ok());
        }
        Ok(false)
    }
    sweep(root, grace, true).map(|_| ())
}

/// Where a cached manifest's image keeps its unpacked trees.
/// `manifests/{registry}/{repo…}/{tag}.json` maps to
/// `rootfs/{registry}/{repo…}/{tag}`, with the colons `rootfs_path` encodes.
fn manifest_rootfs(store_root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut components: Vec<String> = relative
        .iter()
        .map(|component| component.to_str().map(str::to_string))
        .collect::<Option<_>>()?;
    let tag = components.pop()?.strip_suffix(".json")?.replace(':', "%3A");
    let mut path = store_root.join("rootfs");
    for (index, component) in components.iter().enumerate() {
        if index == 0 {
            path.push(component.replace(':', "%3A"));
        } else {
            path.push(component);
        }
    }
    (components.len() >= 2).then(|| path.join(tag))
}

/// Remove cached manifests older than `grace` whose image has no unpacked
/// tree left. Nothing reads these files back to run an image; they are a
/// record of the last pull.
fn remove_stale_manifests(store_root: &Path, grace: Duration) -> io::Result<usize> {
    let manifests = store_root.join("manifests");
    let mut removed = 0;
    let mut pending = vec![manifests.clone()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                pending.push(path);
                continue;
            }
            let Ok(relative) = path.strip_prefix(&manifests) else {
                continue;
            };
            let Some(rootfs) = manifest_rootfs(store_root, relative) else {
                continue;
            };
            if !rootfs.exists() && age(&path) >= grace {
                remove_file(&path)?;
                removed += 1;
            }
        }
    }
    remove_empty_directories(&manifests, grace)?;
    Ok(removed)
}

/// The sha256 hex digests of the pins that still have an unpacked tree:
/// each `rootfs/…/sha256%3A{hex}` directory.
fn pins_with_trees(rootfs: &Path) -> io::Result<Vec<String>> {
    let mut pins = Vec::new();
    let mut pending = vec![rootfs.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_generation_name(name) {
                continue;
            }
            match name.strip_prefix("sha256%3A") {
                Some(hex) => pins.push(hex.to_string()),
                None => pending.push(entry.path()),
            }
        }
    }
    Ok(pins)
}

/// The metadata a pin's chain names: the pin itself, the platform
/// manifests an index lists, and each manifest's config.
fn referenced_metadata(metadata: &Path, pins: Vec<String>) -> HashSet<String> {
    let mut referenced = HashSet::new();
    let mut pending = pins;
    while let Some(hex) = pending.pop() {
        if !referenced.insert(hex.clone()) {
            continue;
        }
        let Ok(bytes) = std::fs::read(metadata.join(&hex)) else {
            continue;
        };
        let digests: Vec<String> = match super::super::oci_pull::parse_manifest(&bytes) {
            Ok(OciManifest::Image(manifest)) => vec![manifest.config.digest],
            Ok(OciManifest::ImageIndex(index)) => index
                .manifests
                .into_iter()
                .map(|entry| entry.digest)
                .collect(),
            // A config, or bytes that aren't a manifest: no further links.
            Err(_) => Vec::new(),
        };
        pending.extend(
            digests
                .iter()
                .filter_map(|digest| digest.strip_prefix("sha256:"))
                .map(str::to_string),
        );
    }
    referenced
}

/// Remove metadata files older than `grace` that no pin with an unpacked
/// tree refers to. The grace spares what a pull in flight has just stored
/// ahead of its unpack.
fn remove_unreferenced_metadata(store_root: &Path, grace: Duration) -> io::Result<usize> {
    let metadata = store_root.join("metadata").join("sha256");
    let Ok(entries) = std::fs::read_dir(&metadata) else {
        return Ok(0);
    };
    let referenced = referenced_metadata(&metadata, pins_with_trees(&store_root.join("rootfs"))?);
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !entry.file_type()?.is_file() || referenced.contains(name) || age(&path) < grace {
            continue;
        }
        remove_file(&path)?;
        removed += 1;
    }
    Ok(removed)
}

/// After the generations are gone: orphan markers, emptied directories,
/// manifests and metadata. Adds what it removed to `report`.
pub(super) fn tidy_store(
    store_root: &Path,
    grace: Duration,
    report: &mut RootfsGcReport,
) -> io::Result<()> {
    let rootfs = store_root.join("rootfs");
    remove_orphan_markers(&rootfs)?;
    remove_empty_directories(&rootfs, grace)?;
    report.manifests_removed += remove_stale_manifests(store_root, grace)?;
    report.metadata_removed += remove_unreferenced_metadata(store_root, grace)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_names_the_store_makes_are_generations() {
        assert!(is_generation_name("gen-0123456789abcdef"));
        assert!(is_generation_name("gen-0123456789abcdef-owner-100000"));
        assert!(!is_generation_name("gen-tools"));
        assert!(!is_generation_name("gen-0123456789abcdef-owner-"));
        assert!(!is_generation_name("gen-0123456789abcdef.complete"));
        assert!(!is_generation_name("v1"));
    }

    #[test]
    fn a_manifest_maps_to_its_images_rootfs_directory() {
        let root = Path::new("/store");
        assert_eq!(
            manifest_rootfs(root, Path::new("localhost:5000/acme/web/sha256:ab.json")),
            Some(PathBuf::from(
                "/store/rootfs/localhost%3A5000/acme/web/sha256%3Aab"
            ))
        );
        assert_eq!(manifest_rootfs(root, Path::new("web.json")), None);
    }
}
