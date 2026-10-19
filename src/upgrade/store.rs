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

/// Name prefixes of the temporary files a candidate binary or a staging
/// write uses in the binary directory.
pub(super) const TEMPORARY_PREFIXES: [&str; 2] = [".candidate-", ".staging-"];

/// How long a candidate or staging file must go unchanged before startup
/// treats it as a killed process's leftover: well past the longest fetch
/// (75 s) and compatibility query (60 s) an upgrade allows.
pub const CANDIDATE_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(600);

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
    /// The bytes go to a private temporary file in the binary directory,
    /// then [`stage_file`](Self::stage_file) takes over. Does NOT touch the
    /// symlink. An upgrade stages its candidate file directly instead, so it
    /// never holds the binary in memory (#892).
    pub fn stage(
        &self,
        version: &BinaryVersion,
        bytes: &[u8],
        envelope: &SignatureEnvelope,
    ) -> Result<PathBuf, UpgradeError> {
        use std::io::Write as _;
        std::fs::create_dir_all(&self.binary_dir)?;
        let mut file = tempfile::Builder::new()
            .prefix(TEMPORARY_PREFIXES[1])
            .tempfile_in(&self.binary_dir)?;
        file.write_all(bytes)?;
        self.stage_file(version, file.into_temp_path(), envelope)
    }

    /// Move a verified binary file and its envelope into the store.
    ///
    /// `binary` must be in the binary directory, so the move is a rename on
    /// one filesystem. It is fsynced, made executable, and renamed into
    /// place, so a crash mid-way never leaves a truncated file under a
    /// versioned name. Does NOT touch the symlink. Both files are hashed a
    /// piece at a time, so a release-sized binary is never read into memory.
    ///
    /// The store is keyed by version, so a version name must always mean
    /// the same bytes. Staging bytes identical to what is already stored
    /// leaves the binary alone (it may be the one running right now) and
    /// deletes `binary`; staging *different* bytes under an existing version
    /// is refused with [`UpgradeError::VersionContentConflict`] rather than
    /// silently replacing a rollback target or pretending a swap happened.
    pub fn stage_file(
        &self,
        version: &BinaryVersion,
        binary: tempfile::TempPath,
        envelope: &SignatureEnvelope,
    ) -> Result<PathBuf, UpgradeError> {
        std::fs::create_dir_all(&self.binary_dir)?;

        let final_path = self.binary_path(version);
        match super::signing::sha256_file(&final_path) {
            Ok(stored) => {
                let incoming = super::signing::sha256_file(&binary)?;
                if stored != incoming {
                    return Err(UpgradeError::VersionContentConflict {
                        version: version.clone(),
                        stored,
                        incoming,
                    });
                }
                envelope.store(&self.envelope_path(version))?;
                self.sync_dir()?;
                return Ok(final_path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        std::fs::File::open(&binary)?.sync_all()?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))?;
        }
        binary
            .persist(&final_path)
            .map_err(|failed| UpgradeError::from(failed.error))?;

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

    /// Delete candidate and staging files a killed process left behind.
    ///
    /// A candidate binary is written to a private temporary file in this
    /// directory (#892); a process killed mid-fetch can't delete its own, and
    /// each is as big as a release. Call it at startup, before anything can
    /// be writing one. A file changed in the last [`CANDIDATE_STALE_AFTER`]
    /// is left alone, in case another node shares this directory (several
    /// local nodes run from one build directory) and is fetching right now.
    /// Returns how many files it removed.
    pub fn remove_stale_candidates(&self) -> Result<usize, UpgradeError> {
        let entries = match std::fs::read_dir(&self.binary_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !TEMPORARY_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                continue;
            }
            let metadata = entry.metadata()?;
            let age = metadata
                .modified()?
                .elapsed()
                .unwrap_or(std::time::Duration::ZERO);
            if metadata.is_file() && age >= CANDIDATE_STALE_AFTER {
                std::fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
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
    fn startup_removes_only_files_left_by_a_killed_fetch() {
        let (dir, store) = store();
        store.stage(&v("0.2.0"), b"kept", &envelope()).unwrap();
        let old = std::time::SystemTime::now() - CANDIDATE_STALE_AFTER * 2;
        for name in [".candidate-abc123", ".staging-def456", ".unrelated"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"half a binary").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        // Another node sharing the directory may be writing this one now.
        std::fs::write(dir.path().join(".candidate-in-flight"), b"arriving").unwrap();

        assert_eq!(store.remove_stale_candidates().unwrap(), 2);

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                ".candidate-in-flight",
                ".unrelated",
                "bun-v0.2.0",
                "bun-v0.2.0.sig"
            ]
        );
    }

    #[test]
    fn staging_a_file_identical_to_the_stored_one_deletes_the_incoming_file() {
        let (dir, store) = store();
        store.stage(&v("0.2.0"), b"same", &envelope()).unwrap();
        let incoming = dir.path().join(".candidate-same");
        std::fs::write(&incoming, b"same").unwrap();

        store
            .stage_file(
                &v("0.2.0"),
                tempfile::TempPath::from_path(&incoming),
                &envelope(),
            )
            .unwrap();

        assert!(!incoming.exists());
        assert_eq!(
            std::fs::read(store.binary_path(&v("0.2.0"))).unwrap(),
            b"same"
        );
    }

    #[test]
    fn stage_refuses_different_bytes_under_an_existing_version() {
        let (_dir, store) = store();
        let path = store
            .stage(&v("0.2.0"), b"first build", &envelope())
            .unwrap();

        let err = store
            .stage(&v("0.2.0"), b"second build", &envelope())
            .unwrap_err();

        assert!(
            matches!(err, UpgradeError::VersionContentConflict { .. }),
            "{err}"
        );
        // The stored bytes are untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"first build");
    }

    #[test]
    fn stage_accepts_identical_bytes_again_and_refreshes_the_envelope() {
        let (_dir, store) = store();
        store.stage(&v("0.2.0"), b"same", &envelope()).unwrap();
        let mut signed = envelope();
        signed.external = Some("BB==".to_string());

        let path = store.stage(&v("0.2.0"), b"same", &signed).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"same");
        let stored = SignatureEnvelope::load(&store.envelope_path(&v("0.2.0"))).unwrap();
        assert_eq!(stored.external.as_deref(), Some("BB=="));
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
