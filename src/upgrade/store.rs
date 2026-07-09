//! On-disk binary store: staging, atomic symlink activation, retention GC.
//!
//! Layout (see `docs/design/agent-bun.md` §4.5):
//!
//! ```text
//! {binary_dir}/
//!   bun                -> bun-v0.2.0     (symlink, the entry point)
//!   bun-v0.1.0                           (previous version, kept for rollback)
//!   bun-v0.2.0                           (current version)
//!   bun-v0.2.0.sig                       (detached signature envelope)
//! ```

use std::path::PathBuf;

use super::error::UpgradeError;
use super::signing::SignatureEnvelope;
use super::version::BinaryVersion;

/// Manages the versioned binaries and the entry symlink in one directory.
#[derive(Debug, Clone)]
pub struct BinaryStore {
    binary_dir: PathBuf,
    /// The symlink name and versioned-file prefix, e.g. `bun`.
    stem: String,
}

impl BinaryStore {
    /// Create a store over `binary_dir`, using `stem` for the symlink name
    /// and the `{stem}-vX.Y.Z` versioned file names.
    pub fn new(binary_dir: PathBuf, stem: String) -> Self {
        Self { binary_dir, stem }
    }

    /// The entry-point symlink path (`{binary_dir}/{stem}`).
    pub fn symlink_path(&self) -> PathBuf {
        self.binary_dir.join(&self.stem)
    }

    /// The versioned binary path for `version`.
    pub fn binary_path(&self, version: &BinaryVersion) -> PathBuf {
        self.binary_dir.join(version.file_name(&self.stem))
    }

    /// The signature envelope path for `version` (`{binary}.sig`).
    pub fn envelope_path(&self, version: &BinaryVersion) -> PathBuf {
        let mut name = version.file_name(&self.stem);
        name.push_str(".sig");
        self.binary_dir.join(name)
    }

    /// Write a verified binary and its envelope into the store.
    ///
    /// The binary is written to a temporary file, fsynced, made executable,
    /// and renamed into place, so a crash mid-write never leaves a truncated
    /// file under a versioned name. Does NOT touch the symlink.
    pub fn stage(
        &self,
        version: &BinaryVersion,
        bytes: &[u8],
        envelope: &SignatureEnvelope,
    ) -> Result<PathBuf, UpgradeError> {
        std::fs::create_dir_all(&self.binary_dir)?;

        let final_path = self.binary_path(version);
        let tmp_path = self.binary_dir.join(format!(
            ".{}.tmp-{}",
            version.file_name(&self.stem),
            std::process::id()
        ));

        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&tmp_path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp_path, &final_path)?;

        envelope.store(&self.envelope_path(version))?;
        self.sync_dir()?;
        Ok(final_path)
    }

    /// Point the entry symlink at `version`, atomically.
    ///
    /// Creates the new symlink under a temporary name and `rename(2)`s it
    /// over the entry path. Rename is atomic on POSIX: there is never a
    /// moment without a valid entry point. Never remove-then-recreate.
    pub fn activate(&self, version: &BinaryVersion) -> Result<(), UpgradeError> {
        let target = version.file_name(&self.stem);
        let target_path = self.binary_path(version);
        if !target_path.is_file() {
            return Err(UpgradeError::UnknownVersion {
                version: version.clone(),
            });
        }

        let tmp_link = self
            .binary_dir
            .join(format!(".{}.link-{}", self.stem, std::process::id()));
        // Remove a stale temp link from a previous crashed attempt, if any.
        let _ = std::fs::remove_file(&tmp_link);
        // The symlink is relative (target file name only) so the whole
        // directory can be moved or mounted elsewhere without breaking it.
        std::os::unix::fs::symlink(&target, &tmp_link)?;
        std::fs::rename(&tmp_link, self.symlink_path())?;
        self.sync_dir()?;
        Ok(())
    }

    /// The version the entry symlink currently points at.
    pub fn current_target(&self) -> Result<BinaryVersion, UpgradeError> {
        let target = std::fs::read_link(self.symlink_path())?;
        let name = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        Self::parse_versioned_name(name, &self.stem).ok_or_else(|| UpgradeError::InvalidVersion {
            input: name.to_string(),
            reason: "symlink target is not a versioned binary name".to_string(),
        })
    }

    /// All versions with a binary file in the store, unsorted.
    pub fn installed_versions(&self) -> Result<Vec<BinaryVersion>, UpgradeError> {
        let mut versions = Vec::new();
        for entry in std::fs::read_dir(&self.binary_dir)? {
            let entry = entry?;
            // Symlinks (the entry point) and directories don't count.
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(version) = Self::parse_versioned_name(name, &self.stem) {
                versions.push(version);
            }
        }
        Ok(versions)
    }

    /// Delete the oldest versions beyond `retain`, never touching versions
    /// in `protect` (typically: the current symlink target and any versions
    /// named in a live upgrade marker). Removes `.sig` sidecars with their
    /// binaries. Returns the versions deleted.
    pub fn garbage_collect(
        &self,
        retain: u32,
        protect: &[BinaryVersion],
    ) -> Result<Vec<BinaryVersion>, UpgradeError> {
        let mut versions = self.installed_versions()?;
        versions.sort();

        let keep_newest = retain as usize;
        let candidate_count = versions.len().saturating_sub(keep_newest);
        let mut deleted = Vec::new();
        for version in versions.into_iter().take(candidate_count) {
            if protect.contains(&version) {
                continue;
            }
            std::fs::remove_file(self.binary_path(&version))?;
            // The envelope may be missing (e.g. a hand-installed binary).
            match std::fs::remove_file(self.envelope_path(&version)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            deleted.push(version);
        }
        if !deleted.is_empty() {
            self.sync_dir()?;
        }
        Ok(deleted)
    }

    /// Parse `bun-v1.2.3` into a version, given stem `bun`. Returns `None`
    /// for foreign files (`bun-v1.2.3.sig`, `README`, temp files...).
    fn parse_versioned_name(name: &str, stem: &str) -> Option<BinaryVersion> {
        let rest = name.strip_prefix(stem)?.strip_prefix('-')?;
        rest.parse().ok()
    }

    /// Fsync the store directory so renames survive a power cut.
    fn sync_dir(&self) -> Result<(), UpgradeError> {
        let dir = std::fs::File::open(&self.binary_dir)?;
        dir.sync_all()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> BinaryVersion {
        s.parse().unwrap()
    }

    fn envelope() -> SignatureEnvelope {
        SignatureEnvelope {
            schema: 1,
            sha256: "00".to_string(),
            embedded: "AA==".to_string(),
            external: None,
        }
    }

    fn store() -> (tempfile::TempDir, BinaryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BinaryStore::new(dir.path().to_path_buf(), "bun".to_string());
        (dir, store)
    }

    #[test]
    fn stage_writes_binary_and_signature_with_exec_permissions() {
        let (_dir, store) = store();
        let path = store
            .stage(&v("0.2.0"), b"binary bytes", &envelope())
            .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"binary bytes");
        assert!(store.envelope_path(&v("0.2.0")).is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }

    #[test]
    fn activate_swaps_symlink_atomically_keeping_old_target() {
        let (_dir, store) = store();
        store.stage(&v("0.1.0"), b"old", &envelope()).unwrap();
        store.stage(&v("0.2.0"), b"new", &envelope()).unwrap();

        store.activate(&v("0.1.0")).unwrap();
        assert_eq!(store.current_target().unwrap(), v("0.1.0"));

        store.activate(&v("0.2.0")).unwrap();
        assert_eq!(store.current_target().unwrap(), v("0.2.0"));
        // The previous version's file is still there for rollback.
        assert!(store.binary_path(&v("0.1.0")).is_file());
        // The symlink resolves to the new content.
        assert_eq!(std::fs::read(store.symlink_path()).unwrap(), b"new");
    }

    #[test]
    fn activate_refuses_missing_target() {
        let (_dir, store) = store();
        let err = store.activate(&v("3.0.0")).unwrap_err();
        assert!(matches!(err, UpgradeError::UnknownVersion { .. }));
    }

    #[test]
    fn current_target_parses_version_from_symlink() {
        let (_dir, store) = store();
        store.stage(&v("1.2.3-rc.1"), b"rc", &envelope()).unwrap();
        store.activate(&v("1.2.3-rc.1")).unwrap();
        assert_eq!(store.current_target().unwrap(), v("1.2.3-rc.1"));
    }

    #[test]
    fn gc_keeps_newest_n_versions() {
        let (_dir, store) = store();
        for version in ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0"] {
            store.stage(&v(version), b"x", &envelope()).unwrap();
        }

        let deleted = store.garbage_collect(3, &[]).unwrap();

        assert_eq!(deleted, vec![v("0.1.0"), v("0.2.0")]);
        let mut left = store.installed_versions().unwrap();
        left.sort();
        assert_eq!(left, vec![v("0.3.0"), v("0.4.0"), v("0.5.0")]);
    }

    #[test]
    fn gc_never_deletes_protected_versions() {
        let (_dir, store) = store();
        for version in ["0.1.0", "0.2.0", "0.3.0", "0.4.0", "0.5.0"] {
            store.stage(&v(version), b"x", &envelope()).unwrap();
        }

        let deleted = store.garbage_collect(3, &[v("0.1.0")]).unwrap();

        assert_eq!(deleted, vec![v("0.2.0")]);
        assert!(store.binary_path(&v("0.1.0")).is_file());
    }

    #[test]
    fn gc_removes_sig_sidecars_with_binaries() {
        let (_dir, store) = store();
        for version in ["0.1.0", "0.2.0"] {
            store.stage(&v(version), b"x", &envelope()).unwrap();
        }

        store.garbage_collect(1, &[]).unwrap();

        assert!(!store.binary_path(&v("0.1.0")).exists());
        assert!(!store.envelope_path(&v("0.1.0")).exists());
        assert!(store.envelope_path(&v("0.2.0")).is_file());
    }

    #[test]
    fn gc_with_fewer_versions_than_retention_deletes_nothing() {
        let (_dir, store) = store();
        store.stage(&v("0.1.0"), b"x", &envelope()).unwrap();
        assert!(store.garbage_collect(3, &[]).unwrap().is_empty());
    }

    #[test]
    fn installed_versions_ignores_foreign_files_and_the_symlink() {
        let (dir, store) = store();
        store.stage(&v("0.1.0"), b"x", &envelope()).unwrap();
        store.activate(&v("0.1.0")).unwrap();
        std::fs::write(dir.path().join("README"), "hello").unwrap();
        std::fs::write(dir.path().join("bun-not-a-version"), "hello").unwrap();
        std::fs::write(dir.path().join("otherstem-v1.0.0"), "hello").unwrap();

        assert_eq!(store.installed_versions().unwrap(), vec![v("0.1.0")]);
    }
}
