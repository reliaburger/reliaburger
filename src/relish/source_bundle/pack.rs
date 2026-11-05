//! Packing the source bundle: the deterministic tarball `build.rs` embeds.
//!
//! `build.rs` compiles this file with `#[path]`, so it may use only `std`
//! and the crates that are both build and normal dependencies (`tar` and
//! `sha2`). The library compiles it too: a build that doesn't embed the
//! bundle packs the checkout with it at run time, and the tests use it.

use std::io;
use std::path::Path;
use std::process::Command;

/// What the bundle carries, relative to the repository root: everything
/// `cargo build` reads, plus the tests and the docs an agent reads to
/// understand the code. Directories are taken whole.
///
/// The CI job `source bundle builds` extracts the bundle into an empty
/// directory and builds it there, so a new `include_str!` of a file outside
/// this list fails CI rather than a user.
pub const ALLOWLIST: &[&str] = &[
    // The build set: everything `cargo build --bins` reads.
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "clippy.toml",
    "LICENSE",
    "src",
    "ebpf",
    "benches",
    "brioche/dist",
    "docs/manual",
    "examples",
    "scripts/release/guest-images.json",
    "scripts/release/qualify-sustained.sh",
    "scripts/release/sustained/soak-jobs.toml",
    "scripts/release/sustained/soak-workloads.toml",
    // The test set.
    "tests",
    "proptest-regressions",
    "Makefile",
    ".config",
    // The read set.
    "CLAUDE.md",
    "docs/design",
    "docs/book",
    "docs/whitepaper.md",
    "docs/roadmap.md",
    "docs/release-log.md",
    "docs/releasing.md",
    "docs/testing.md",
    "docs/flakes.md",
];

/// Every entry's modification time: the Unix epoch, so the bytes never
/// depend on when the files were checked out.
pub const MTIME: u64 = 0;

/// Names a directory walk skips: they're in `.gitignore`, so a checkout
/// listed by git never has them either.
const SKIPPED_NAMES: &[&str] = &[".DS_Store", "__pycache__", "target"];

/// The files under `allowlist`, repository-relative with `/` separators,
/// sorted.
///
/// In a git checkout whose top level is `root`, git lists them: tracked
/// files plus untracked ones that `.gitignore` doesn't exclude, so a build
/// output lying in `examples/` stays out. Anywhere else (an extracted
/// bundle) a directory walk lists them. Every allowlisted path must exist.
pub fn list_files(root: &Path, allowlist: &[&str]) -> io::Result<Vec<String>> {
    for entry in allowlist {
        if !root.join(entry).exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("allowlisted path {entry} is missing"),
            ));
        }
    }
    let mut files = match git_files(root, allowlist) {
        Some(files) => files,
        None => walk_files(root, allowlist)?,
    };
    files.sort();
    files.dedup();
    Ok(files)
}

/// Whether git reports changes under `allowlist`: `None` when `root` isn't
/// the top level of a git checkout, or git can't say.
pub fn dirty(root: &Path, allowlist: &[&str]) -> Option<bool> {
    if !is_git_top_level(root) {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "--"])
        .args(allowlist)
        .output()
        .ok()?;
    output.status.success().then_some(!output.stdout.is_empty())
}

/// A tar archive of `files` (repository-relative, as [`list_files`]
/// returns them) read from `root`. The same files give the same bytes:
/// entries come in the order given, with a fixed time, owner and mode.
pub fn write_tar(root: &Path, files: &[String]) -> io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for path in files {
        let full = root.join(path);
        let data = std::fs::read(&full)?;
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(data.len() as u64);
        header.set_mode(if is_executable(&full)? { 0o755 } else { 0o644 });
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(MTIME);
        // append_data writes the path (with a GNU long-name entry when it
        // doesn't fit in the header) and the checksum.
        builder.append_data(&mut header, path, data.as_slice())?;
    }
    builder.into_inner()
}

/// The lower-case hex SHA-256 of `bytes`.
pub fn digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn is_git_top_level(root: &Path) -> bool {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let top = String::from_utf8_lossy(&output.stdout);
    match (
        std::fs::canonicalize(top.trim()),
        std::fs::canonicalize(root),
    ) {
        (Ok(top), Ok(root)) => top == root,
        _ => false,
    }
}

fn git_files(root: &Path, allowlist: &[&str]) -> Option<Vec<String>> {
    if !is_git_top_level(root) {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ])
        .args(allowlist)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let listing = String::from_utf8(output.stdout).ok()?;
    let files = listing
        .split('\0')
        .filter(|path| !path.is_empty())
        // A tracked file deleted in the working tree is still in the index.
        .filter(|path| is_regular_file(&root.join(path)))
        .map(str::to_string)
        .collect();
    Some(files)
}

fn walk_files(root: &Path, allowlist: &[&str]) -> io::Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in allowlist {
        walk(root, entry, &mut files)?;
    }
    Ok(files)
}

fn walk(root: &Path, relative: &str, files: &mut Vec<String>) -> io::Result<()> {
    let full = root.join(relative);
    let metadata = std::fs::symlink_metadata(&full)?;
    if metadata.is_file() {
        files.push(relative.to_string());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for child in std::fs::read_dir(&full)? {
        let name = child?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if SKIPPED_NAMES.contains(&name) {
            continue;
        }
        walk(root, &format!("{relative}/{name}"), files)?;
    }
    Ok(())
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> io::Result<bool> {
    Ok(false)
}
