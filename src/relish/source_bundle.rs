//! The source bundle: the build, test and read set this binary was built
//! from, as one tarball.
//!
//! `build.rs` packs the files named in [`pack::ALLOWLIST`] into
//! `$OUT_DIR/source.tar.zst` and records its digest and dirty flag; this
//! module reads it back. Release builds and builds with the `embed-source`
//! feature embed it. Other builds pack the checkout they were built in when
//! asked, so editing the book or a test doesn't rebuild the crate.
//! `relish source` browses it, and the CI job `source bundle builds`
//! extracts it and builds the result, which is what makes "this binary
//! carries its source" true.

#[cfg(any(test, not(source_bundle_embedded)))]
pub mod pack;

use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// The compressed tarball, compiled in at build time.
#[cfg(source_bundle_embedded)]
static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/source.tar.zst"));

/// What a source bundle operation can fail with.
#[derive(Debug, thiserror::Error)]
pub enum SourceBundleError {
    /// The bundle couldn't be decompressed, packed or read as a tarball.
    #[error("failed to read the source bundle: {0}")]
    Read(#[source] io::Error),

    /// A build that doesn't embed the bundle lost the checkout it reads.
    #[error(
        "this build reads its source from {}, which is gone; build with --release or --features embed-source to embed it",
        .0.display()
    )]
    CheckoutMissing(PathBuf),

    /// An entry would land outside the extraction directory.
    #[error("source bundle entry {0:?} escapes the extraction directory")]
    UnsafePath(String),

    /// Extraction refuses a directory that already holds something.
    #[error("{} is not empty; extract the source into a new or empty directory", .0.display())]
    NotEmpty(PathBuf),

    /// Writing the extracted tree failed.
    #[error("failed to write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Where this binary's source comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Compiled into the binary.
    Embedded,
    /// Packed at run time from the checkout this binary was built in.
    Checkout(PathBuf),
}

/// One file in the bundle.
#[derive(Debug, Clone)]
pub struct SourceFile {
    /// Repository-relative path with `/` separators, e.g. `src/lib.rs`.
    pub path: String,
    pub contents: Vec<u8>,
}

/// Where [`files`], [`read`] and [`extract`] get the source from.
pub fn origin() -> Origin {
    #[cfg(source_bundle_embedded)]
    {
        Origin::Embedded
    }
    #[cfg(not(source_bundle_embedded))]
    {
        Origin::Checkout(PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    }
}

/// The uncompressed tarball: the embedded one, or the checkout packed now.
pub fn tarball() -> Result<Vec<u8>, SourceBundleError> {
    #[cfg(source_bundle_embedded)]
    {
        zstd::stream::decode_all(BUNDLE).map_err(SourceBundleError::Read)
    }
    #[cfg(not(source_bundle_embedded))]
    {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        if !root.join("Cargo.toml").is_file() {
            return Err(SourceBundleError::CheckoutMissing(root));
        }
        let files = pack::list_files(&root, pack::ALLOWLIST).map_err(SourceBundleError::Read)?;
        pack::write_tar(&root, &files).map_err(SourceBundleError::Read)
    }
}

/// The SHA-256 of the uncompressed tarball, in lower-case hex. Builds of the
/// same tree report the same digest, whatever they compressed it with.
pub fn digest() -> Result<String, SourceBundleError> {
    #[cfg(source_bundle_embedded)]
    {
        Ok(env!("RELIABURGER_SOURCE_DIGEST").to_string())
    }
    #[cfg(not(source_bundle_embedded))]
    {
        Ok(digest_of(&tarball()?))
    }
}

/// The SHA-256 of an uncompressed tarball, in lower-case hex: what
/// [`digest`] reports for the bundle that tarball came from.
pub fn digest_of(tar: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(tar)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether the tree had uncommitted changes in the allowlisted paths: when
/// the binary was built, for an embedded bundle, or now, for a checkout.
/// `None` when git can't say (no git, or not a checkout).
pub fn dirty() -> Option<bool> {
    #[cfg(source_bundle_embedded)]
    {
        parse_dirty(env!("RELIABURGER_SOURCE_DIRTY"))
    }
    #[cfg(not(source_bundle_embedded))]
    {
        pack::dirty(Path::new(env!("CARGO_MANIFEST_DIR")), pack::ALLOWLIST)
    }
}

/// Every file in the bundle, sorted by path.
pub fn files() -> Result<Vec<SourceFile>, SourceBundleError> {
    files_in(&tarball()?)
}

/// One file's contents, or `None` when the bundle doesn't carry it.
pub fn read(path: &str) -> Result<Option<Vec<u8>>, SourceBundleError> {
    Ok(files()?
        .into_iter()
        .find(|file| file.path == path)
        .map(|file| file.contents))
}

/// Write the bundle's tree into `dir`, which must be new or empty, and
/// return the number of files written.
pub fn extract(dir: &Path) -> Result<usize, SourceBundleError> {
    extract_from(&tarball()?, dir)
}

// Only an embedded bundle carries its dirty flag as text.
#[cfg_attr(not(any(test, source_bundle_embedded)), allow(dead_code))]
fn parse_dirty(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The files in an uncompressed tarball, sorted by path.
pub fn files_in(tar: &[u8]) -> Result<Vec<SourceFile>, SourceBundleError> {
    let mut archive = tar::Archive::new(tar);
    let mut files = Vec::new();
    for entry in archive.entries().map_err(SourceBundleError::Read)? {
        let mut entry = entry.map_err(SourceBundleError::Read)?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let path = entry
            .path()
            .map_err(SourceBundleError::Read)?
            .to_string_lossy()
            .into_owned();
        let mut contents = Vec::new();
        entry
            .read_to_end(&mut contents)
            .map_err(SourceBundleError::Read)?;
        files.push(SourceFile { path, contents });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

/// Unpack an uncompressed tarball into `dir`, which must be new or empty,
/// and return the number of files written.
pub fn extract_from(tar: &[u8], dir: &Path) -> Result<usize, SourceBundleError> {
    let write_error = |source| SourceBundleError::Write {
        path: dir.to_path_buf(),
        source,
    };
    if dir.exists() {
        let mut entries = std::fs::read_dir(dir).map_err(write_error)?;
        if entries.next().is_some() {
            return Err(SourceBundleError::NotEmpty(dir.to_path_buf()));
        }
    }
    std::fs::create_dir_all(dir).map_err(write_error)?;

    let mut archive = tar::Archive::new(tar);
    // The extracted files get the time they're written, so Cargo's
    // freshness checks behave as they would in a fresh checkout.
    archive.set_preserve_mtime(false);
    let mut written = 0;
    for entry in archive.entries().map_err(SourceBundleError::Read)? {
        let mut entry = entry.map_err(SourceBundleError::Read)?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let path = entry
            .path()
            .map_err(SourceBundleError::Read)?
            .to_string_lossy()
            .into_owned();
        // unpack_in refuses (returns false for) a path with `..` or a root.
        let unpacked = entry
            .unpack_in(dir)
            .map_err(|source| SourceBundleError::Write {
                path: dir.join(&path),
                source,
            })?;
        if !unpacked {
            return Err(SourceBundleError::UnsafePath(path));
        }
        written += 1;
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The checkout this test binary was built from.
    fn checkout() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// The bundle passing this many bytes at release level breaks the
    /// promise that it costs under 1% of the binary.
    const SIZE_CEILING: usize = 6 * 1024 * 1024;

    fn write(root: &Path, path: &str, contents: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    /// A small tree outside any git checkout, so pack walks it.
    fn sample_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"sample\"\n");
        write(dir.path(), "src/lib.rs", "pub fn hello() {}\n");
        write(dir.path(), "src/bin/tool.rs", "fn main() {}\n");
        // Longer than the 100 bytes a tar header holds.
        write(
            dir.path(),
            &format!("src/{}/deep.rs", "nested".repeat(20)),
            "// deep\n",
        );
        dir
    }

    const SAMPLE_ALLOWLIST: &[&str] = &["Cargo.toml", "src"];

    /// The sample tree's tarball, and the tarball compressed as build.rs
    /// compresses a dev build's.
    fn sample_bundle(root: &Path) -> (Vec<u8>, Vec<u8>) {
        let files = pack::list_files(root, SAMPLE_ALLOWLIST).unwrap();
        let tar = pack::write_tar(root, &files).unwrap();
        let bundle = zstd::stream::encode_all(tar.as_slice(), 3).unwrap();
        (tar, bundle)
    }

    #[test]
    fn bundle_carries_the_manifest_lockfile_build_script_and_entry_points() {
        let paths: Vec<String> = files().unwrap().into_iter().map(|file| file.path).collect();
        for expected in [
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
            "src/lib.rs",
            "src/bin/relish.rs",
            "src/bin/bun.rs",
            "src/bin/testapp.rs",
            "src/relish/source_bundle.rs",
            "ebpf/onion_connect.bpf.c",
            "docs/manual/08_five-minute-tour.md",
            "tests/suite/main.rs",
        ] {
            assert!(
                paths.iter().any(|path| path == expected),
                "{expected} is missing from the bundle"
            );
        }
    }

    #[test]
    fn a_file_read_back_equals_the_checkouts() {
        for path in ["Cargo.toml", "Cargo.lock", "src/relish/source_bundle.rs"] {
            let embedded = read(path).unwrap().expect("in the bundle");
            let on_disk = std::fs::read(checkout().join(path)).unwrap();
            assert_eq!(embedded, on_disk, "{path} differs from the checkout");
        }
        assert!(read("no/such/file.rs").unwrap().is_none());
    }

    #[test]
    fn paths_are_repository_relative_and_inside_the_allowlist() {
        let files = files().unwrap();
        assert!(
            files.len() > 500,
            "expected the full tree, got {}",
            files.len()
        );
        for file in &files {
            let path = &file.path;
            assert!(!path.starts_with('/'), "{path} is absolute");
            assert!(!path.contains('\\'), "{path} uses a backslash");
            assert!(
                !path.split('/').any(|part| part == ".." || part == "."),
                "{path} has a relative component"
            );
            assert!(
                pack::ALLOWLIST
                    .iter()
                    .any(|entry| path == entry || path.starts_with(&format!("{entry}/"))),
                "{path} is outside the allowlist"
            );
        }
        let sorted = files.windows(2).all(|pair| pair[0].path < pair[1].path);
        assert!(sorted, "files come sorted, without duplicates");
    }

    #[test]
    fn the_digest_is_the_tarballs() {
        let digest = digest().unwrap();
        assert_eq!(digest.len(), 64);
        assert_eq!(pack::digest(&tarball().unwrap()), digest);
        assert_eq!(digest_of(&tarball().unwrap()), digest);
    }

    #[test]
    fn the_origin_says_whether_the_bundle_is_embedded() {
        let expected = if cfg!(source_bundle_embedded) {
            Origin::Embedded
        } else {
            Origin::Checkout(checkout())
        };
        assert_eq!(origin(), expected);
    }

    /// The embedded bundle's size, if this build embedded it at level 19.
    #[cfg(source_bundle_embedded)]
    fn embedded_at_release_level() -> Option<usize> {
        (env!("RELIABURGER_SOURCE_LEVEL") == "19").then_some(BUNDLE.len())
    }

    #[cfg(not(source_bundle_embedded))]
    fn embedded_at_release_level() -> Option<usize> {
        None
    }

    #[test]
    fn the_bundle_stays_under_six_megabytes_at_release_level() {
        // Only a release build embeds at level 19; measure what one would
        // embed, whatever this build did.
        let size = embedded_at_release_level().unwrap_or_else(|| {
            zstd::stream::encode_all(tarball().unwrap().as_slice(), 19)
                .unwrap()
                .len()
        });
        assert!(
            size <= SIZE_CEILING,
            "the source bundle is {size} bytes at level 19, over the {SIZE_CEILING} ceiling: \
             drop something from pack::ALLOWLIST or raise the ceiling on purpose"
        );
    }

    #[test]
    fn the_digest_is_stable_across_two_packs_of_the_same_tree() {
        let tree = sample_tree();
        let (first_tar, first_bundle) = sample_bundle(tree.path());
        // Rewriting a file with the same bytes changes its modification time,
        // and nothing else.
        write(tree.path(), "src/lib.rs", "pub fn hello() {}\n");
        let (second_tar, second_bundle) = sample_bundle(tree.path());
        assert_eq!(pack::digest(&first_tar), pack::digest(&second_tar));
        assert_eq!(first_bundle, second_bundle);

        write(tree.path(), "src/lib.rs", "pub fn goodbye() {}\n");
        let (changed, _) = sample_bundle(tree.path());
        assert_ne!(pack::digest(&first_tar), pack::digest(&changed));
    }

    #[test]
    fn a_walk_lists_sorted_relative_paths_and_skips_ignored_names() {
        let tree = sample_tree();
        write(tree.path(), "src/.DS_Store", "junk");
        write(tree.path(), "src/__pycache__/x.pyc", "junk");
        write(tree.path(), "outside.txt", "not allowlisted");
        let files = pack::list_files(tree.path(), SAMPLE_ALLOWLIST).unwrap();
        let deep = format!("src/{}/deep.rs", "nested".repeat(20));
        assert_eq!(
            files,
            vec![
                "Cargo.toml".to_string(),
                "src/bin/tool.rs".to_string(),
                "src/lib.rs".to_string(),
                deep,
            ]
        );
    }

    #[test]
    fn a_missing_allowlisted_path_fails_the_pack() {
        let tree = sample_tree();
        let error = pack::list_files(tree.path(), &["Cargo.toml", "docs/gone.md"]).unwrap_err();
        assert!(error.to_string().contains("docs/gone.md"), "{error}");
    }

    #[test]
    fn a_tree_outside_git_has_no_dirty_flag() {
        let tree = sample_tree();
        assert_eq!(pack::dirty(tree.path(), SAMPLE_ALLOWLIST), None);
        assert_eq!(parse_dirty("true"), Some(true));
        assert_eq!(parse_dirty("false"), Some(false));
        assert_eq!(parse_dirty("unknown"), None);
    }

    #[test]
    fn extraction_reproduces_the_packed_tree() {
        let tree = sample_tree();
        let (tar, bundle) = sample_bundle(tree.path());
        // What build.rs compresses, the binary decompresses unchanged.
        assert_eq!(zstd::stream::decode_all(bundle.as_slice()).unwrap(), tar);
        let target = tempfile::tempdir().unwrap();
        let out = target.path().join("source");
        let written = extract_from(&tar, &out).unwrap();
        assert_eq!(written, 4);
        for file in files_in(&tar).unwrap() {
            let extracted = std::fs::read(out.join(&file.path)).unwrap();
            assert_eq!(
                extracted,
                std::fs::read(tree.path().join(&file.path)).unwrap()
            );
        }
        // Packing the extracted copy gives the same tarball.
        let (repacked, _) = sample_bundle(&out);
        assert_eq!(pack::digest(&repacked), pack::digest(&tar));
    }

    #[cfg(unix)]
    #[test]
    fn extraction_keeps_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tree = sample_tree();
        write(tree.path(), "src/run.sh", "#!/bin/sh\n");
        let script = tree.path().join("src/run.sh");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (tar, _) = sample_bundle(tree.path());
        let target = tempfile::tempdir().unwrap();
        extract_from(&tar, target.path()).unwrap();
        let mode = |path: &str| {
            std::fs::metadata(target.path().join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("src/run.sh"), 0o755);
        assert_eq!(mode("src/lib.rs"), 0o644);
    }

    #[test]
    fn extraction_refuses_a_directory_that_is_not_empty() {
        let tree = sample_tree();
        let (tar, _) = sample_bundle(tree.path());
        let target = tempfile::tempdir().unwrap();
        std::fs::write(target.path().join("keep.txt"), "mine").unwrap();
        let error = extract_from(&tar, target.path()).unwrap_err();
        assert!(matches!(error, SourceBundleError::NotEmpty(_)), "{error}");
        assert_eq!(
            std::fs::read_to_string(target.path().join("keep.txt")).unwrap(),
            "mine"
        );
    }
}
