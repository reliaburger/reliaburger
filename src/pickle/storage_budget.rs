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
    pub(super) fn sync_verified(&self, path: &Path) -> Result<(), PickleError> {
        let _state = self.lock()?;
        std::fs::File::open(path)?.sync_all()?;
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("payload parent missing"))?;
        std::fs::File::open(parent)?.sync_all()?;
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
            std::fs::create_dir_all(parent)?;
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
            std::fs::File::open(parent)?.sync_all()
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
            if !published && confirmed && std::fs::File::open(parent)?.sync_all().is_ok() {
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
    /// TODO(#555): also sync parent entries of newly created ancestors.
    pub(super) fn publish(&self, source: &Path, destination: &Path) -> Result<(), PickleError> {
        let mut state = self.lock()?;
        let bytes = size(source)?.ok_or_else(|| std::io::Error::other("upload payload missing"))?;
        state.charge(source, bytes)?;
        let parent = destination
            .parent()
            .ok_or_else(|| std::io::Error::other("blob parent missing"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::File::open(source)?.sync_all()?;
        std::fs::rename(source, destination)?;
        std::fs::File::open(parent)?.sync_all()?;
        if let Some(source_parent) = source.parent()
            && source_parent != parent
        {
            std::fs::File::open(source_parent)?.sync_all()?;
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
                Ok(directory) => directory.sync_all()?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        state.forget(path);
        Ok(())
    }
}
