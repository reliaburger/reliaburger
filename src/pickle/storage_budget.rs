//! Shared admission for compressed blobs, upload temporaries and receipts.
//!
//! Disk transactions run on blocking workers. The mutex covers reservation and
//! disk mutation together, so cancellation cannot release a worker's reservation.
use super::types::PickleError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A byte ceiling alone cannot bound empty uploads or tiny receipt files.
pub(super) const MAX_STORAGE_FILES: usize = 65_536;

#[derive(Debug, Default)]
pub(super) struct StorageBudget {
    state: Mutex<BudgetState>,
    #[cfg(test)]
    directory_sync_hook: Mutex<Option<DirectorySyncHook>>,
}

#[cfg(test)]
type DirectorySyncCallback = dyn Fn(&Path) -> std::io::Result<()> + Send + Sync;

#[cfg(test)]
#[derive(Clone)]
struct DirectorySyncHook(std::sync::Arc<DirectorySyncCallback>);
#[cfg(test)]
impl std::fmt::Debug for DirectorySyncHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DirectorySyncHook(..)")
    }
}

#[derive(Debug)]
struct BudgetState {
    limit: Option<u64>,
    file_limit: usize,
    used: u64,
    files: HashMap<PathBuf, u64>,
}
impl Default for BudgetState {
    fn default() -> Self {
        Self {
            limit: None,
            file_limit: MAX_STORAGE_FILES,
            used: 0,
            files: HashMap::new(),
        }
    }
}
impl BudgetState {
    fn charge(&mut self, path: &Path, bytes: u64) -> Result<(), PickleError> {
        let old = self.files.get(path).copied().unwrap_or(0);
        let total = self
            .used
            .checked_sub(old)
            .and_then(|n| n.checked_add(bytes))
            .ok_or(PickleError::StorageQuotaExceeded)?;
        if self.limit.is_some_and(|limit| limit != 0 && total > limit)
            || (!self.files.contains_key(path) && self.files.len() >= self.file_limit)
        {
            return Err(PickleError::StorageQuotaExceeded);
        }
        self.used = total;
        self.files.insert(path.to_owned(), bytes);
        Ok(())
    }
    fn transfer(&mut self, source: &Path, destination: &Path) {
        let bytes = self.files.get(source).copied().unwrap_or(0);
        self.forget(destination);
        self.files.remove(source);
        self.files.insert(destination.to_owned(), bytes);
    }
    fn forget(&mut self, path: &Path) {
        if let Some(bytes) = self.files.remove(path) {
            self.used = self.used.saturating_sub(bytes);
        }
    }
}

fn size(path: &Path) -> std::io::Result<Option<u64>> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => Err(std::io::Error::other("image payload is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn scan(directory: &Path, files: &mut HashMap<PathBuf, u64>) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(std::io::Error::other(
                "image payload must not be a symbolic link",
            ));
        }
        if kind.is_dir() {
            scan(&entry.path(), files)?;
        } else if kind.is_file() {
            files.insert(entry.path(), entry.metadata()?.len());
        } else {
            return Err(std::io::Error::other("image payload is not a regular file"));
        }
    }
    Ok(())
}
impl StorageBudget {
    /// Test-local instrumentation of the existing real directory syncs.
    /// It does not add the missing ancestor sync operations under test.
    #[cfg(test)]
    pub(super) fn set_directory_sync_hook(&self, hook: std::sync::Arc<DirectorySyncCallback>) {
        *self
            .directory_sync_hook
            .lock()
            .expect("test sync hook poisoned") = Some(DirectorySyncHook(hook));
    }
    fn sync_open_directory(&self, path: &Path, directory: &std::fs::File) -> std::io::Result<()> {
        #[cfg(test)]
        {
            let hook = self
                .directory_sync_hook
                .lock()
                .map_err(|_| std::io::Error::other("test sync hook poisoned"))?
                .as_ref()
                .map(|hook| hook.0.clone());
            if let Some(hook) = hook {
                return hook(path);
            }
        }
        #[cfg(not(test))]
        let _ = path;
        directory.sync_all()
    }
    pub(super) fn sync_directory(&self, path: &Path) -> std::io::Result<()> {
        let directory = std::fs::File::open(path)?;
        self.sync_open_directory(path, &directory)
    }
    /// Confirm every directory entry needed to reach a published payload.
    /// Retry parent syncs even for visible directories: an earlier mkdir may
    /// have succeeded before its parent sync failed. Existing configured path
    /// aliases keep the same metadata-following semantics as create_dir_all.
    fn ensure_payload_directory(&self, root: &Path, directory: &Path) -> std::io::Result<()> {
        let relative = directory
            .strip_prefix(root)
            .map_err(|_| std::io::Error::other("payload directory is outside image store"))?;
        let mut missing = Vec::new();
        let mut ancestor = if root.as_os_str().is_empty() {
            Path::new(".")
        } else {
            root
        };
        while !Self::directory_exists(ancestor)? {
            missing.push(ancestor.to_owned());
            ancestor = match ancestor.parent() {
                Some(parent) if parent.as_os_str().is_empty() => Path::new("."),
                Some(parent) => parent,
                None => {
                    return Err(std::io::Error::other(
                        "image store has no existing ancestor",
                    ));
                }
            };
        }
        self.ensure_directory_entry(ancestor)?;
        for created in missing.into_iter().rev() {
            self.ensure_directory_entry(&created)?;
        }
        let mut current = root.to_owned();
        for component in relative.components() {
            let std::path::Component::Normal(name) = component else {
                return Err(std::io::Error::other("invalid payload directory component"));
            };
            current.push(name);
            self.ensure_directory_entry(&current)?;
        }
        Ok(())
    }
    fn directory_exists(path: &Path) -> std::io::Result<bool> {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => Ok(true),
            Ok(_) => Err(std::io::Error::other("image directory is not a directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
    fn ensure_directory_entry(&self, path: &Path) -> std::io::Result<()> {
        if !Self::directory_exists(path)? {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists
                        && Self::directory_exists(path)? => {}
                Err(error) => return Err(error),
            }
        }
        if let Some(parent) = path.parent() {
            self.sync_directory(if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            })?;
        }
        Ok(())
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BudgetState>, PickleError> {
        self.state
            .lock()
            .map_err(|_| std::io::Error::other("image storage budget poisoned").into())
    }
    /// Rebuild all payload charges before writers start, including receipt and
    /// atomic-write temporaries. Existing over-limit storage is retained, but
    /// further admission fails until deletion restores capacity.
    pub(super) fn configure(&self, root: &Path, limit: u64) -> Result<(), PickleError> {
        let mut state = self.lock()?;
        if let Some(existing) = state.limit {
            return if existing == limit {
                Ok(())
            } else {
                Err(
                    std::io::Error::other("image storage budget already configured differently")
                        .into(),
                )
            };
        }
        let mut files = HashMap::new();
        scan(&root.join("uploads"), &mut files)?;
        scan(&root.join("blobs/sha256"), &mut files)?;
        state.used = files
            .values()
            .try_fold(0u64, |total, bytes| total.checked_add(*bytes))
            .ok_or(PickleError::StorageQuotaExceeded)?;
        state.files = files;
        state.limit = Some(limit);
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn set_file_limit(&self, limit: usize) -> Result<(), PickleError> {
        self.lock()?.file_limit = limit;
        Ok(())
    }
    /// Reusing verified bytes acknowledges a durable blob too. A previous
    /// rename may have returned a directory-sync error, so retry the syncs
    /// before acknowledging reuse. Uncertain reservations remain charged.
    pub(super) fn sync_verified(&self, root: &Path, path: &Path) -> Result<(), PickleError> {
        let _state = self.lock()?;
        std::fs::File::open(path)?.sync_all()?;
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("payload parent missing"))?;
        self.ensure_payload_directory(root, parent)?;
        self.sync_directory(parent)?;
        Ok(())
    }

    pub(super) fn create_empty(&self, path: &Path) -> Result<(), PickleError> {
        let mut state = self.lock()?;
        state.charge(path, 0)?;
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path);
        if result.is_err() {
            state.forget(path);
        }
        result?;
        Ok(())
    }
    /// Reserve the entire temporary copy, even for a replacement. Only a
    /// confirmed file and containing-directory sync releases the old charge.
    pub(super) fn write_file(
        &self,
        root: &Path,
        path: &Path,
        data: &[u8],
        mode: Option<u32>,
    ) -> Result<(), PickleError> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("payload parent missing"))?;
        let temporary = parent.join(format!(".data.{:032x}.tmp", rand::random::<u128>()));
        let mut state = self.lock()?;
        state.charge(&temporary, data.len() as u64)?;
        use std::io::Write as _;
        let mut published = false;
        let result = (|| -> std::io::Result<()> {
            self.ensure_payload_directory(root, parent)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            if let Some(mode) = mode {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(mode);
            }
            #[cfg(not(unix))]
            let _ = mode;
            let mut file = options.open(&temporary)?;
            file.write_all(data)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, path)?;
            published = true;
            self.sync_directory(parent)
        })();
        if result.is_ok() {
            state.transfer(&temporary, path);
        } else {
            let cleanup = std::fs::remove_file(&temporary);
            let confirmed = cleanup.is_ok()
                || cleanup
                    .as_ref()
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            // If rename succeeded but sync failed, retain the temporary charge
            // until restart. If cleanup is durable, release its actual prefix.
            if !published && confirmed && {
                let directory = std::fs::File::open(parent)?;
                self.sync_open_directory(parent, &directory).is_ok()
            } {
                state.forget(&temporary);
            }
        }
        result.map_err(Into::into)
    }
    pub(super) fn append(&self, path: &Path, data: &[u8]) -> Result<u64, PickleError> {
        use std::io::Write as _;
        let mut state = self.lock()?;
        let previous = std::fs::metadata(path)?.len();
        let total = previous
            .checked_add(data.len() as u64)
            .ok_or(PickleError::StorageQuotaExceeded)?;
        state.charge(path, total)?;
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
            file.write_all(data)?;
            file.flush()
        })();
        let current = std::fs::metadata(path)?.len();
        // A failed write may leave a prefix; retain that prefix's charge.
        state.charge(path, current)?;
        result?;
        Ok(current)
    }
    /// A verified upload already owns its reservation. Transfer it only after
    /// the file and both containing directories acknowledge the rename.
    /// The complete ancestor chain is confirmed before publishing, including
    /// visible directories left by a previously failed parent sync.
    pub(super) fn publish(
        &self,
        root: &Path,
        source: &Path,
        destination: &Path,
    ) -> Result<(), PickleError> {
        let mut state = self.lock()?;
        let bytes = size(source)?.ok_or_else(|| std::io::Error::other("upload payload missing"))?;
        state.charge(source, bytes)?;
        let parent = destination
            .parent()
            .ok_or_else(|| std::io::Error::other("blob parent missing"))?;
        self.ensure_payload_directory(root, parent)?;
        std::fs::File::open(source)?.sync_all()?;
        std::fs::rename(source, destination)?;
        self.sync_directory(parent)?;
        if let Some(source_parent) = source.parent()
            && source_parent != parent
        {
            self.sync_directory(source_parent)?;
        }
        state.transfer(source, destination);
        Ok(())
    }
    /// Retain a failed removal's charge. A later confirmed retry also frees
    /// charges whose unlink succeeded but whose previous directory sync failed.
    pub(super) fn remove(&self, path: &Path) -> Result<(), PickleError> {
        let mut state = self.lock()?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(parent) = path.parent() {
            match std::fs::File::open(parent) {
                Ok(directory) => self.sync_open_directory(parent, &directory)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        state.forget(path);
        Ok(())
    }
}
