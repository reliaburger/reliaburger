//! Content-addressed blob store for Pickle.
//!
//! Stores blobs as `{base_dir}/blobs/sha256/{hex}/data`. Shares the
//! same on-disk layout as `grill::image::ImageStore`, so blobs cached
//! from Docker Hub are visible to Pickle and vice versa.

use std::path::{Path, PathBuf};

use sha2::{Digest as Sha2Digest, Sha256};

use super::types::{Digest, PickleError};

/// Reject any upload id that isn't in the exact shape we generate.
///
/// Ids come from `format!("{:032x}", rand::random::<u128>())`, so a valid id is
/// 32 lowercase hex characters. Anything else — in particular a percent-decoded
/// `../` sequence from the OCI upload URL — is refused before it can be joined
/// into a filesystem path (see `upload_path`), closing a path-traversal that
/// let a client append to or delete files outside the uploads directory.
fn validate_upload_id(upload_id: &str) -> Result<(), PickleError> {
    let valid = upload_id.len() == 32
        && upload_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if valid {
        Ok(())
    } else {
        Err(PickleError::InvalidUploadId(upload_id.to_string()))
    }
}

/// Content-addressed blob store.
///
/// Thread-safe: all operations use atomic file moves (no partial reads)
/// and stateless path lookups for reads. Clones share payload admission and
/// durable write accounting; async writers run those transactions off Tokio.
#[derive(Debug, Clone)]
pub struct BlobStore {
    base_dir: PathBuf,
    budget: std::sync::Arc<super::storage_budget::StorageBudget>,
}

/// Exclusive process ownership of a registry's temporary upload directory.
/// Keep this guard alive until every registry writer has stopped.
///
/// The ownership is a `flock`, which belongs to the open file description,
/// not to the descriptor. A child process that another thread is spawning
/// holds a copy of every descriptor until its `exec` closes it, so closing
/// ours alone could leave the directory owned for a moment, and a
/// replacement claimed straight after a drop was refused (#497). Dropping
/// the owner unlocks first, which releases the lock for every copy at once.
#[derive(Debug)]
#[must_use = "keep the upload owner alive while registry writers can run"]
pub struct UploadDirectoryOwner {
    lock: std::fs::File,
}

impl Drop for UploadDirectoryOwner {
    fn drop(&mut self) {
        // Nothing useful can be done with a failed unlock: closing the
        // descriptor straight after still releases the lock eventually.
        let _ = self.lock.unlock();
    }
}

impl BlobStore {
    /// Create a new blob store rooted at `base_dir`.
    ///
    /// The directory structure is created on demand — `base_dir` itself
    /// must exist, but subdirectories are created as blobs are written.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            budget: std::sync::Arc::new(super::storage_budget::StorageBudget::default()),
        }
    }

    /// Bound compressed CAS, upload temporary and repository receipt bytes.
    /// Configure before writers start; every clone shares the same budget.
    pub async fn configure_storage_limit(&self, limit: u64) -> Result<(), PickleError> {
        let root = self.base_dir.clone();
        let budget = self.budget.clone();
        tokio::task::spawn_blocking(move || budget.configure(&root, limit))
            .await
            .map_err(|error| {
                PickleError::CatalogPersist(format!("quota setup task failed: {error}"))
            })?
    }

    /// Claim exclusive upload ownership and reclaim abandoned temporary files.
    /// Call before starting any writer. Startup refuses competing owners,
    /// unrecognised entries and cleanup errors without serving partial recovery.
    pub async fn claim_upload_directory(&self) -> Result<UploadDirectoryOwner, PickleError> {
        let directory = self.base_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<UploadDirectoryOwner, PickleError> {
            std::fs::create_dir_all(&directory)?;
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let lock = options.open(directory.join(".upload-owner.lock"))?;
            lock.try_lock().map_err(|error| {
                std::io::Error::other(format!("registry upload directory is busy: {error}"))
            })?;
            // Own the lock before anything below can refuse the claim, so a
            // refusal unlocks it too.
            let owner = UploadDirectoryOwner { lock };
            let uploads = directory.join("uploads");
            std::fs::create_dir_all(&uploads)?;
            if !std::fs::symlink_metadata(&uploads)?.file_type().is_dir() {
                return Err(
                    std::io::Error::other("upload directory must not be a symbolic link").into(),
                );
            }
            for entry in std::fs::read_dir(&uploads)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    std::io::Error::other("upload directory contains a non-UTF-8 entry")
                })?;
                validate_upload_id(name)?;
                if !entry.file_type()?.is_file() {
                    return Err(std::io::Error::other(format!(
                        "upload {name} is not a regular temporary file",
                    ))
                    .into());
                }
                std::fs::remove_file(entry.path())?;
            }
            std::fs::File::open(uploads)?.sync_all()?;
            Ok(owner)
        })
        .await
        .map_err(|error| std::io::Error::other(format!("upload recovery task failed: {error}")))?
    }

    /// Path to a blob on disk.
    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        crate::grill::image::cached_blob_path(&self.base_dir, digest.hex())
    }

    /// Path for temporary upload files.
    fn upload_path(&self, upload_id: &str) -> PathBuf {
        self.base_dir.join("uploads").join(upload_id)
    }

    /// Check if a blob exists.
    pub fn has_blob(&self, digest: &Digest) -> bool {
        self.blob_path(digest).exists()
    }

    /// Get the size of a blob in bytes.
    pub fn blob_size(&self, digest: &Digest) -> Result<u64, PickleError> {
        let path = self.blob_path(digest);
        let meta =
            std::fs::metadata(&path).map_err(|_| PickleError::BlobNotFound(digest.clone()))?;
        Ok(meta.len())
    }

    /// Read a blob's contents.
    pub fn read_blob(&self, digest: &Digest) -> Result<Vec<u8>, PickleError> {
        let path = self.blob_path(digest);
        std::fs::read(&path).map_err(|_| PickleError::BlobNotFound(digest.clone()))
    }

    /// Store internal cache-fill bytes without blocking Tokio workers on the
    /// shared budget or filesystem transactions.
    pub async fn write_blob_async(&self, data: Vec<u8>, digest: Digest) -> Result<(), PickleError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.write_blob(&data, &digest))
            .await
            .map_err(|error| {
                PickleError::CatalogPersist(format!("blob write task failed: {error}"))
            })?
    }

    /// Write a blob directly (for small blobs or internal use).
    ///
    /// Verifies the SHA-256 digest matches before committing.
    pub fn write_blob(&self, data: &[u8], expected_digest: &Digest) -> Result<(), PickleError> {
        // Verify digest
        let actual = compute_sha256(data);
        if actual.as_str() != expected_digest.as_str() {
            return Err(PickleError::DigestMismatch {
                expected: expected_digest.clone(),
                actual,
            });
        }

        let path = self.blob_path(expected_digest);
        // Durable, crash-safe write: unique temp, fsync, rename, fsync dir
        // (REG5). A concurrent write of the same digest is harmless — both
        // rename identical content-addressed bytes over the same target.
        if path.is_file() && sha256_file(&path)? == *expected_digest {
            self.budget.sync_verified(&self.base_dir, &path)?;
            return Ok(());
        }
        self.budget.write_file(&self.base_dir, &path, data, None)?;
        Ok(())
    }

    /// Revalidate a cached blob against its digest, deleting it if it no
    /// longer matches (REG5).
    ///
    /// A blob whose bytes were truncated by a crash mid-write, or corrupted
    /// on disk, must not be served as if it were valid. The deploy/verify
    /// path calls this before trusting a locally-cached blob. Returns `true`
    /// when the blob is present and its content hashes to `digest`.
    ///
    /// Returns `Ok(false)` for a missing or corrupt blob and an error when the
    /// file exists but can't be read: an I/O failure says nothing about the
    /// bytes, so it must not look like a cache miss.
    ///
    /// The file is hashed in fixed-size chunks, so a multi-gigabyte layer
    /// never sits in memory. Hashing runs on the caller's thread; callers on
    /// the async runtime should wrap this in `spawn_blocking`.
    pub fn revalidate_blob(&self, digest: &Digest) -> Result<bool, PickleError> {
        let path = self.blob_path(digest);
        let actual = match sha256_file(&path) {
            Ok(actual) => actual,
            Err(PickleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.delete_blob(digest)?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if actual == *digest {
            return Ok(true);
        }
        // Corrupt: remove it so the next pull refetches clean bytes.
        self.delete_blob(digest)?;
        Ok(false)
    }

    /// An upload receipt identifies the exact repository and, for test-owned
    /// repositories, its current lease generation. Hash the encoded identity
    /// for the filename; repository strings never become filesystem paths.
    fn repository_upload_evidence(
        &self,
        digest: &Digest,
        repository: &str,
        lease: Option<&str>,
    ) -> Result<(PathBuf, Vec<u8>), PickleError> {
        let identity = serde_json::to_vec(&(repository, lease)).map_err(std::io::Error::other)?;
        let key = compute_sha256(&identity);
        let directory = self
            .blob_path(digest)
            .parent()
            .ok_or_else(|| std::io::Error::other("blob has no parent directory"))?
            .join("repositories");
        Ok((directory.join(key.hex()), identity))
    }

    pub(super) fn has_repository_upload(
        &self,
        digest: &Digest,
        repository: &str,
        lease: Option<&str>,
    ) -> Result<bool, PickleError> {
        let (path, identity) = self.repository_upload_evidence(digest, repository, lease)?;
        match std::fs::read(path) {
            Ok(stored) => Ok(stored == identity),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Retirement removes only this generation's evidence. Shared blob bytes
    /// and other repositories' upload receipts remain available.
    pub(super) fn retire_repository_uploads(
        &self,
        repository: &str,
        lease: &str,
    ) -> Result<(), PickleError> {
        let directory = self.base_dir.join("blobs/sha256");
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Ok(digest) =
                Digest::new(&format!("sha256:{}", entry.file_name().to_string_lossy()))
            else {
                continue;
            };
            let (path, _) = self.repository_upload_evidence(&digest, repository, Some(lease))?;
            self.budget.remove(&path)?;
        }
        Ok(())
    }

    /// Delete blob payload and any upload authority, including receipts left
    /// behind by an interrupted or externally missing payload removal.
    pub fn delete_blob(&self, digest: &Digest) -> Result<(), PickleError> {
        let path = self.blob_path(digest);
        self.budget.remove(&path)?;
        if let Some(parent) = path.parent() {
            let receipts = parent.join("repositories");
            match std::fs::read_dir(&receipts) {
                Ok(entries) => {
                    for entry in entries {
                        self.budget.remove(&entry?.path())?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let _ = std::fs::remove_dir(receipts);
            let _ = std::fs::remove_dir(parent);
        }
        Ok(())
    }

    /// Initiate an upload session. Returns the upload ID.
    ///
    /// The upload is written to a temporary file. When complete,
    /// `complete_upload()` verifies the digest and moves it to the
    /// blob store atomically.
    pub async fn initiate_upload(&self) -> Result<String, PickleError> {
        let upload_id = format!("{:032x}", rand::random::<u128>());
        let path = self.upload_path(&upload_id);
        let directory = self.base_dir.join("uploads");
        let budget = self.budget.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(directory)?;
            budget.create_empty(&path)
        })
        .await
        .map_err(|error| {
            PickleError::CatalogPersist(format!("upload creation task failed: {error}"))
        })??;
        Ok(upload_id)
    }

    /// Append data to an upload session.
    pub async fn write_upload_chunk(
        &self,
        upload_id: &str,
        data: &[u8],
    ) -> Result<u64, PickleError> {
        validate_upload_id(upload_id)?;
        let path = self.upload_path(upload_id);
        if !path.exists() {
            return Err(PickleError::UploadNotFound(upload_id.to_string()));
        }
        let budget = self.budget.clone();
        let bytes = data.to_vec();
        tokio::task::spawn_blocking(move || budget.append(&path, &bytes))
            .await
            .map_err(|error| {
                PickleError::CatalogPersist(format!("upload write task failed: {error}"))
            })?
    }

    /// Complete an upload: verify digest, move to blob store.
    pub async fn complete_upload(
        &self,
        upload_id: &str,
        expected_digest: &Digest,
    ) -> Result<(), PickleError> {
        self.complete_upload_guarded(upload_id, expected_digest, None, None)
            .await
    }

    /// Keep the API session's writer permit until the durable transaction finishes,
    /// even when the caller disconnects while the blocking task is running.
    pub(super) async fn complete_upload_guarded(
        &self,
        upload_id: &str,
        expected_digest: &Digest,
        writer: Option<tokio::sync::OwnedSemaphorePermit>,
        repository_writer: Option<super::lease::RepositoryReadGuard>,
    ) -> Result<(), PickleError> {
        self.complete_upload_with_repository(
            upload_id,
            expected_digest,
            writer,
            repository_writer,
            None,
        )
        .await
    }

    /// A public upload proves its bytes for one admitted repository. Commit
    /// evidence before acknowledging success, retaining writer fencing even
    /// when the HTTP caller disconnects.
    pub(super) async fn complete_repository_upload_guarded(
        &self,
        upload_id: &str,
        expected_digest: &Digest,
        writer: tokio::sync::OwnedSemaphorePermit,
        access: super::lease::RegistryWriteAccess,
        repository: &str,
    ) -> Result<(), PickleError> {
        self.complete_upload_with_repository(
            upload_id,
            expected_digest,
            Some(writer),
            access.guard,
            Some((repository.to_owned(), access.lease_id)),
        )
        .await
    }

    async fn complete_upload_with_repository(
        &self,
        upload_id: &str,
        expected_digest: &Digest,
        writer: Option<tokio::sync::OwnedSemaphorePermit>,
        repository_writer: Option<super::lease::RepositoryReadGuard>,
        repository: Option<(String, Option<String>)>,
    ) -> Result<(), PickleError> {
        validate_upload_id(upload_id)?;
        let receipt = repository
            .map(|(repository, lease)| {
                self.repository_upload_evidence(expected_digest, &repository, lease.as_deref())
            })
            .transpose()?;
        let upload = self.upload_path(upload_id);
        let destination = self.blob_path(expected_digest);
        let expected = expected_digest.clone();
        let root = self.base_dir.clone();
        let budget = self.budget.clone();
        tokio::task::spawn_blocking(move || -> Result<(), PickleError> {
            use std::io::Read as _;
            let _writer = writer;
            let _repository_writer = repository_writer;
            let mut file = std::fs::File::open(&upload)?;
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
            let actual = Digest::new(&format!("sha256:{}", hex::encode(hasher.finalize())))?;
            if actual != expected {
                budget.remove(&upload)?;
                return Err(PickleError::DigestMismatch { expected, actual });
            }
            drop(file);
            budget.publish(&root, &upload, &destination)?;
            if let Some((path, identity)) = receipt {
                let directory = path
                    .parent()
                    .ok_or_else(|| std::io::Error::other("receipt has no parent directory"))?;
                budget.write_file(&root, &path, &identity, Some(0o600))?;
                let blob_directory = directory
                    .parent()
                    .ok_or_else(|| std::io::Error::other("receipt parent missing"))?;
                budget.sync_directory(blob_directory)?;
            }
            Ok(())
        })
        .await
        .map_err(|error| PickleError::CatalogPersist(format!("blob commit task failed: {error}")))?
    }

    /// Commit an already-verified upload temp into the blob store by moving
    /// it, without re-reading its bytes (M11).
    ///
    /// The peer pull hashes incrementally as it streams to disk, so its
    /// digest is already proven. Unlike [`complete_upload`](Self::complete_upload),
    /// this skips the verification pass and just fsyncs the temp, renames it into place, and
    /// fsyncs the parent directory (REG5), all O(1) in memory. The caller MUST
    /// have verified the content hashes to `digest` first.
    pub async fn commit_upload_as_blob(
        &self,
        upload_id: &str,
        digest: &Digest,
    ) -> Result<(), PickleError> {
        validate_upload_id(upload_id)?;
        let upload_path = self.upload_path(upload_id);
        if !upload_path.exists() {
            return Err(PickleError::UploadNotFound(upload_id.to_string()));
        }
        let blob_path = self.blob_path(digest);
        // The temp and the blob store live under the same base dir, so the
        // rename is a same-filesystem atomic move. Runs on the blocking pool:
        // fsync and rename are blocking syscalls. A concurrent pull of the same
        // digest renames identical content-addressed bytes over the same
        // target — harmless.
        let budget = self.budget.clone();
        let root = self.base_dir.clone();
        tokio::task::spawn_blocking(move || budget.publish(&root, &upload_path, &blob_path))
            .await
            .map_err(|e| PickleError::CatalogPersist(format!("blob commit task failed: {e}")))??;
        Ok(())
    }

    /// The number of bytes accumulated in an in-flight upload session.
    ///
    /// Used to enforce the storage quota on chunked and bare-PUT uploads
    /// before they are committed (REG4/M10) — the on-disk temp file is the
    /// authoritative running size, independent of what the client claimed.
    pub async fn upload_size(&self, upload_id: &str) -> Result<u64, PickleError> {
        validate_upload_id(upload_id)?;
        let path = self.upload_path(upload_id);
        if !path.exists() {
            return Err(PickleError::UploadNotFound(upload_id.to_string()));
        }
        Ok(tokio::fs::metadata(&path).await?.len())
    }

    /// Cancel an upload session, cleaning up the temp file.
    pub async fn cancel_upload(&self, upload_id: &str) -> Result<(), PickleError> {
        validate_upload_id(upload_id)?;
        let path = self.upload_path(upload_id);
        let budget = self.budget.clone();
        tokio::task::spawn_blocking(move || budget.remove(&path))
            .await
            .map_err(|error| {
                PickleError::CatalogPersist(format!("upload removal task failed: {error}"))
            })?
    }

    /// List all blob digests in the store.
    pub fn list_blobs(&self) -> Result<Vec<Digest>, PickleError> {
        let sha_dir = self.base_dir.join("blobs").join("sha256");
        if !sha_dir.exists() {
            return Ok(Vec::new());
        }
        let mut digests = Vec::new();
        for entry in std::fs::read_dir(&sha_dir)? {
            let entry = entry?;
            let hex = entry.file_name().to_string_lossy().to_string();
            let Ok(digest) = Digest::new(&format!("sha256:{hex}")) else {
                continue;
            };
            if self.blob_path(&digest).is_file() {
                digests.push(digest);
            }
        }
        Ok(digests)
    }

    /// The base directory of this store.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
}

/// Stream a file through SHA-256 in 64 KiB chunks.
pub(crate) fn sha256_file(path: &Path) -> Result<Digest, PickleError> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(Digest::from_sha256_hex(&hex::encode(hasher.finalize())))
}

/// Compute the SHA-256 digest of data.
pub fn compute_sha256(data: &[u8]) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let hash = hasher.finalize();
    Digest::from_sha256_hex(&hex::encode(hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (BlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        (store, dir)
    }

    async fn repository_upload(
        store: &BlobStore,
        bytes: &[u8],
        repository: &str,
        lease: Option<&str>,
        expected: &Digest,
    ) -> Result<(), PickleError> {
        let upload = store.initiate_upload().await?;
        store.write_upload_chunk(&upload, bytes).await?;
        let writer = std::sync::Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();
        store
            .complete_repository_upload_guarded(
                &upload,
                expected,
                writer,
                crate::pickle::lease::RegistryWriteAccess {
                    lease_id: lease.map(str::to_owned),
                    guard: None,
                },
                repository,
            )
            .await
    }

    // Insert into pickle::store::tests after final555 adds the per-store seam
    // `budget.set_directory_sync_hook(Arc<dyn Fn(&Path)->io::Result<()> + Send + Sync>)`.
    // These are drafts and have not been compiled or claimed green.

    #[tokio::test]
    async fn new_blob_publication_syncs_each_parent_entry_before_success() {
        use std::sync::{Arc, Mutex};
        let directory = tempfile::tempdir().unwrap();
        let store = BlobStore::new(directory.path().join("new-store"));
        store.configure_storage_limit(2).await.unwrap();
        let synced = Arc::new(Mutex::new(Vec::new()));
        let observation = synced.clone();
        store
            .budget
            .set_directory_sync_hook(Arc::new(move |path: &Path| {
                std::fs::File::open(path)?.sync_all()?;
                observation.lock().unwrap().push(path.to_owned());
                Ok(())
            }));
        let digest = compute_sha256(b"aa");
        store.write_blob(b"aa", &digest).unwrap();
        let parents = synced.lock().unwrap();
        for required in [
            directory.path().to_path_buf(),
            store.base_dir().to_path_buf(),
            store.base_dir().join("blobs"),
            store.base_dir().join("blobs/sha256"),
            store.blob_path(&digest).parent().unwrap().to_path_buf(),
        ] {
            assert!(
                parents.contains(&required),
                "missing successful parent sync: {required:?}"
            );
        }
        assert_eq!(store.read_blob(&digest).unwrap(), b"aa");
    }

    #[tokio::test]
    async fn failed_new_digest_parent_sync_keeps_upload_reserved_and_retry_resyncs_visible_entry() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        let (store, _directory) = test_store();
        store.configure_storage_limit(2).await.unwrap();
        let digest = compute_sha256(b"aa");
        let failed_parent = store.base_dir().join("blobs/sha256");
        let enabled = Arc::new(AtomicBool::new(true));
        let failures = Arc::new(AtomicUsize::new(0));
        let fault = enabled.clone();
        let observed = failures.clone();
        store
            .budget
            .set_directory_sync_hook(Arc::new(move |path: &Path| {
                if path == failed_parent {
                    observed.fetch_add(1, Ordering::SeqCst);
                    if fault.load(Ordering::SeqCst) {
                        return Err(std::io::Error::other(
                            "injected new-directory parent sync failure",
                        ));
                    }
                }
                std::fs::File::open(path)?.sync_all()
            }));
        let upload = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload, b"aa").await.unwrap();
        assert!(store.complete_upload(&upload, &digest).await.is_err());
        assert_eq!(store.upload_size(&upload).await.unwrap(), 2);
        assert!(!store.has_blob(&digest));
        assert!(
            store.blob_path(&digest).parent().unwrap().is_dir(),
            "create happened before injected sync failure"
        );
        assert!(matches!(
            store.write_blob(b"b", &compute_sha256(b"b")),
            Err(PickleError::StorageQuotaExceeded)
        ));
        enabled.store(false, Ordering::SeqCst);
        store.complete_upload(&upload, &digest).await.unwrap();
        assert!(
            failures.load(Ordering::SeqCst) >= 2,
            "visible entry was not resynced on retry"
        );
        assert_eq!(store.read_blob(&digest).unwrap(), b"aa");
    }

    #[tokio::test]
    async fn verified_reuse_refuses_an_uncertain_containing_directory_sync() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (store, _directory) = test_store();
        store.configure_storage_limit(2).await.unwrap();
        let digest = compute_sha256(b"aa");
        let failed_directory = store.blob_path(&digest).parent().unwrap().to_path_buf();
        let enabled = Arc::new(AtomicBool::new(true));
        let fault = enabled.clone();
        store
            .budget
            .set_directory_sync_hook(Arc::new(move |path: &Path| {
                if path == failed_directory && fault.load(Ordering::SeqCst) {
                    return Err(std::io::Error::other("injected post-rename sync failure"));
                }
                std::fs::File::open(path)?.sync_all()
            }));
        assert!(store.write_blob(b"aa", &digest).is_err());
        assert_eq!(
            store.read_blob(&digest).unwrap(),
            b"aa",
            "rename happened before sync failure"
        );
        assert!(
            store.write_blob(b"aa", &digest).is_err(),
            "existing bytes do not excuse failed durability sync"
        );
        assert!(matches!(
            store.write_blob(b"b", &compute_sha256(b"b")),
            Err(PickleError::StorageQuotaExceeded)
        ));
        enabled.store(false, Ordering::SeqCst);
        store.write_blob(b"aa", &digest).unwrap();
        // Uncertain old reservation remains conservative until an explicit
        // reconciliation/confirmed cleanup or restart. Do not assert free bytes.
    }

    #[tokio::test]
    async fn all_upload_publication_paths_sync_new_payload_ancestor_entries() {
        use std::sync::{Arc, Mutex};
        for streamed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let store = BlobStore::new(directory.path().join("new-store"));
            store.configure_storage_limit(2).await.unwrap();
            let observed = Arc::new(Mutex::new(Vec::new()));
            let recording = observed.clone();
            store
                .budget
                .set_directory_sync_hook(Arc::new(move |path: &Path| {
                    std::fs::File::open(path)?.sync_all()?;
                    recording.lock().unwrap().push(path.to_owned());
                    Ok(())
                }));
            let digest = compute_sha256(b"aa");
            let upload = store.initiate_upload().await.unwrap();
            store.write_upload_chunk(&upload, b"aa").await.unwrap();
            if streamed {
                store.commit_upload_as_blob(&upload, &digest).await.unwrap();
            } else {
                store.complete_upload(&upload, &digest).await.unwrap();
            }
            let parents = observed.lock().unwrap();
            for expected in [
                directory.path().to_path_buf(),
                store.base_dir().to_path_buf(),
                store.base_dir().join("blobs"),
                store.base_dir().join("blobs/sha256"),
            ] {
                assert!(
                    parents.contains(&expected),
                    "streamed={streamed}, missing durable ancestor: {expected:?}"
                );
            }
            assert_eq!(store.read_blob(&digest).unwrap(), b"aa");
        }
    }

    #[tokio::test]
    async fn repository_receipt_parent_entry_is_synced_after_the_directory_exists() {
        use std::sync::{Arc, Mutex};
        let (store, _directory) = test_store();
        let digest = compute_sha256(b"receipt content");
        let blob_directory = store.blob_path(&digest).parent().unwrap().to_path_buf();
        let repositories = blob_directory.join("repositories");
        let observation = Arc::new(Mutex::new(Vec::new()));
        let recording = observation.clone();
        store
            .budget
            .set_directory_sync_hook(Arc::new(move |path: &Path| {
                std::fs::File::open(path)?.sync_all()?;
                recording
                    .lock()
                    .unwrap()
                    .push((path.to_owned(), repositories.is_dir()));
                Ok(())
            }));
        repository_upload(
            &store,
            b"receipt content",
            "rbtest-sync/web",
            Some("run-a"),
            &digest,
        )
        .await
        .unwrap();
        let records = observation.lock().unwrap();
        assert!(
            records
                .iter()
                .any(|(path, exists)| path == &blob_directory && *exists),
            "new repositories entry was not synced after its creation"
        );
        assert!(
            store
                .has_repository_upload(&digest, "rbtest-sync/web", Some("run-a"))
                .unwrap()
        );
    }

    #[tokio::test]
    async fn failed_removal_directory_sync_retains_capacity_until_confirmed_retry() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (store, _directory) = test_store();
        store.configure_storage_limit(2).await.unwrap();
        let digest = compute_sha256(b"aa");
        store.write_blob(b"aa", &digest).unwrap();
        let parent = store.blob_path(&digest).parent().unwrap().to_path_buf();
        let enabled = Arc::new(AtomicBool::new(true));
        let fault = enabled.clone();
        store
            .budget
            .set_directory_sync_hook(Arc::new(move |path: &Path| {
                if path == parent && fault.load(Ordering::SeqCst) {
                    return Err(std::io::Error::other("injected removal sync failure"));
                }
                std::fs::File::open(path)?.sync_all()
            }));
        assert!(store.delete_blob(&digest).is_err());
        assert!(
            !store.has_blob(&digest),
            "unlink completed before its directory sync failed"
        );
        assert!(
            matches!(
                store.write_blob(b"b", &compute_sha256(b"b")),
                Err(PickleError::StorageQuotaExceeded)
            ),
            "uncertain unlink incorrectly returned physical payload capacity"
        );
        enabled.store(false, Ordering::SeqCst);
        store.delete_blob(&digest).unwrap();
        store.write_blob(b"bb", &compute_sha256(b"bb")).unwrap();
        assert_eq!(store.read_blob(&compute_sha256(b"bb")).unwrap(), b"bb");
    }

    #[tokio::test]
    async fn repository_upload_evidence_survives_restart_and_is_not_global() {
        let (store, directory) = test_store();
        let digest = compute_sha256(b"shared");
        repository_upload(&store, b"shared", "team-a/web", None, &digest)
            .await
            .unwrap();
        let restarted = BlobStore::new(directory.path());
        assert!(
            restarted
                .has_repository_upload(&digest, "team-a/web", None)
                .unwrap()
        );
        assert!(
            !restarted
                .has_repository_upload(&digest, "team-b/web", None)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn failed_digest_completion_never_grants_existing_blob_authority() {
        let (store, _directory) = test_store();
        let digest = compute_sha256(b"private bytes");
        store.write_blob(b"private bytes", &digest).unwrap();
        assert!(
            repository_upload(&store, b"different bytes", "team-a/web", None, &digest)
                .await
                .is_err()
        );
        assert!(
            !store
                .has_repository_upload(&digest, "team-a/web", None)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn repository_evidence_failure_prevents_successful_upload_acknowledgement() {
        let (store, _directory) = test_store();
        let digest = compute_sha256(b"data");
        let (receipt, _) = store
            .repository_upload_evidence(&digest, "team-a/web", None)
            .unwrap();
        std::fs::create_dir_all(receipt).unwrap();
        assert!(
            repository_upload(&store, b"data", "team-a/web", None, &digest)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn retirement_forgets_only_the_exact_repository_generation() {
        let (store, _directory) = test_store();
        let digest = compute_sha256(b"shared");
        for (repo, lease) in [("rbtest-a/web", "run-a"), ("rbtest-b/web", "run-b")] {
            repository_upload(&store, b"shared", repo, Some(lease), &digest)
                .await
                .unwrap();
        }
        assert!(
            !store
                .has_repository_upload(&digest, "rbtest-a/web", Some("replacement"))
                .unwrap()
        );
        store
            .retire_repository_uploads("rbtest-a/web", "run-a")
            .unwrap();
        assert!(
            !store
                .has_repository_upload(&digest, "rbtest-a/web", Some("run-a"))
                .unwrap()
        );
        assert!(
            store
                .has_repository_upload(&digest, "rbtest-b/web", Some("run-b"))
                .unwrap()
        );
        assert_eq!(store.read_blob(&digest).unwrap(), b"shared");
    }

    #[tokio::test]
    async fn collection_and_corruption_remove_repository_upload_evidence() {
        let (store, _directory) = test_store();
        let digest = compute_sha256(b"data");
        repository_upload(&store, b"data", "team-a/web", None, &digest)
            .await
            .unwrap();
        store.delete_blob(&digest).unwrap();
        assert!(
            !store
                .has_repository_upload(&digest, "team-a/web", None)
                .unwrap()
        );
        repository_upload(&store, b"data", "team-a/web", None, &digest)
            .await
            .unwrap();
        std::fs::write(store.blob_path(&digest), b"corrupt").unwrap();
        assert!(!store.revalidate_blob(&digest).unwrap());
        assert!(
            !store
                .has_repository_upload(&digest, "team-a/web", None)
                .unwrap()
        );
    }

    #[tokio::test]
    async fn physical_budget_reserves_concurrent_uploads_and_releases_confirmed_cancellation() {
        let (store, _directory) = test_store();
        store.configure_storage_limit(4).await.unwrap();
        let a = store.initiate_upload().await.unwrap();
        let b = store.initiate_upload().await.unwrap();
        let (first, second) = tokio::join!(
            store.write_upload_chunk(&a, b"aaaa"),
            store.write_upload_chunk(&b, b"bbbb")
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let (winner, other) = if first.is_ok() { (&a, &b) } else { (&b, &a) };
        assert_eq!(store.upload_size(winner).await.unwrap(), 4);
        assert_eq!(store.upload_size(other).await.unwrap(), 0);
        store.cancel_upload(winner).await.unwrap();
        store.write_upload_chunk(other, b"cccc").await.unwrap();
    }

    #[tokio::test]
    async fn physical_budget_moves_upload_capacity_and_deduplicates_direct_writes() {
        let (store, _directory) = test_store();
        store.configure_storage_limit(4).await.unwrap();
        let bytes = b"aaaa";
        let digest = compute_sha256(bytes);
        let upload = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload, bytes).await.unwrap();
        store.complete_upload(&upload, &digest).await.unwrap();
        store.write_blob(bytes, &digest).unwrap();
        assert!(matches!(
            store.write_blob(b"b", &compute_sha256(b"b")),
            Err(PickleError::StorageQuotaExceeded)
        ));
        store.delete_blob(&digest).unwrap();
        store.write_blob(b"bbbb", &compute_sha256(b"bbbb")).unwrap();
    }

    #[tokio::test]
    async fn physical_budget_reconstructs_committed_and_temporary_bytes_after_restart() {
        let (store, directory) = test_store();
        store.write_blob(b"aa", &compute_sha256(b"aa")).unwrap();
        let upload = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload, b"bb").await.unwrap();
        let restarted = BlobStore::new(directory.path());
        restarted.configure_storage_limit(4).await.unwrap();
        assert!(matches!(
            restarted.write_upload_chunk(&upload, b"c").await,
            Err(PickleError::StorageQuotaExceeded)
        ));
        restarted.cancel_upload(&upload).await.unwrap();
        restarted.write_blob(b"cc", &compute_sha256(b"cc")).unwrap();
    }

    #[tokio::test]
    async fn failed_upload_removal_does_not_release_payload_capacity() {
        let (store, directory) = test_store();
        store.configure_storage_limit(2).await.unwrap();
        let upload = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload, b"aa").await.unwrap();
        // Hide the payload behind an unavailable upload directory. The failed
        // cleanup cannot prove those bytes disappeared or make room for a blob.
        std::fs::rename(
            directory.path().join("uploads"),
            directory.path().join("unavailable-uploads"),
        )
        .unwrap();
        std::fs::write(directory.path().join("uploads"), b"blocked directory").unwrap();
        assert!(store.cancel_upload(&upload).await.is_err());
        assert!(matches!(
            store.write_blob(b"b", &compute_sha256(b"b")),
            Err(PickleError::StorageQuotaExceeded)
        ));
        std::fs::remove_file(directory.path().join("uploads")).unwrap();
        std::fs::rename(
            directory.path().join("unavailable-uploads"),
            directory.path().join("uploads"),
        )
        .unwrap();
        store.cancel_upload(&upload).await.unwrap();
        store.write_blob(b"bb", &compute_sha256(b"bb")).unwrap();
    }

    #[tokio::test]
    async fn peer_commit_path_cannot_create_unbudgeted_bytes() {
        let (store, _directory) = test_store();
        store.configure_storage_limit(3).await.unwrap();
        let upload = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload, b"aaa").await.unwrap();
        store
            .commit_upload_as_blob(&upload, &compute_sha256(b"aaa"))
            .await
            .unwrap();
        let other = store.initiate_upload().await.unwrap();
        assert!(matches!(
            store.write_upload_chunk(&other, b"b").await,
            Err(PickleError::StorageQuotaExceeded)
        ));
    }

    #[tokio::test]
    async fn empty_upload_file_admission_is_bounded_and_cancel_restores_capacity() {
        let (store, _directory) = test_store();
        store.configure_storage_limit(1).await.unwrap();
        store.budget.set_file_limit(2).unwrap();
        let a = store.initiate_upload().await.unwrap();
        let b = store.initiate_upload().await.unwrap();
        assert!(matches!(
            store.initiate_upload().await,
            Err(PickleError::StorageQuotaExceeded)
        ));
        store.cancel_upload(&a).await.unwrap();
        let replacement = store.initiate_upload().await.unwrap();
        assert_eq!(
            std::fs::read_dir(store.base_dir().join("uploads"))
                .unwrap()
                .count(),
            2
        );
        store.cancel_upload(&b).await.unwrap();
        store.cancel_upload(&replacement).await.unwrap();
    }

    #[tokio::test]
    async fn receipt_replacement_and_retirement_account_exact_payload_bytes() {
        let (store, directory) = test_store();
        let digest = compute_sha256(b"aa");
        let (receipt, identity) = store
            .repository_upload_evidence(&digest, "rbtest-a/web", Some("lease-a"))
            .unwrap();
        let limit = 2 + 2 * identity.len() as u64;
        store.configure_storage_limit(limit).await.unwrap();
        for _ in 0..3 {
            repository_upload(&store, b"aa", "rbtest-a/web", Some("lease-a"), &digest)
                .await
                .unwrap();
        }
        assert_eq!(
            std::fs::read_dir(receipt.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        let restarted = BlobStore::new(directory.path());
        restarted
            .configure_storage_limit(2 + identity.len() as u64)
            .await
            .unwrap();
        let upload = restarted.initiate_upload().await.unwrap();
        assert!(matches!(
            restarted.write_upload_chunk(&upload, b"b").await,
            Err(PickleError::StorageQuotaExceeded)
        ));
        restarted
            .retire_repository_uploads("rbtest-a/web", "lease-a")
            .unwrap();
        restarted.write_upload_chunk(&upload, b"b").await.unwrap();
        assert_eq!(restarted.read_blob(&digest).unwrap(), b"aa");
    }

    #[tokio::test]
    async fn collection_returns_receipt_byte_and_file_capacity() {
        let (store, _directory) = test_store();
        let digest = compute_sha256(b"aa");
        let (_, identity) = store
            .repository_upload_evidence(&digest, "repo-a", None)
            .unwrap();
        store
            .configure_storage_limit(2 + identity.len() as u64)
            .await
            .unwrap();
        repository_upload(&store, b"aa", "repo-a", None, &digest)
            .await
            .unwrap();
        store.budget.set_file_limit(2).unwrap();
        assert!(matches!(
            store.initiate_upload().await,
            Err(PickleError::StorageQuotaExceeded)
        ));
        store.delete_blob(&digest).unwrap();
        let upload = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload, b"aa").await.unwrap();
    }

    #[tokio::test]
    async fn refused_direct_blob_admission_creates_no_empty_cas_directory() {
        let (store, _directory) = test_store();
        store.configure_storage_limit(1).await.unwrap();
        let digest = compute_sha256(b"aa");
        assert!(matches!(
            store.write_blob(b"aa", &digest),
            Err(PickleError::StorageQuotaExceeded)
        ));
        assert!(!store.blob_path(&digest).parent().unwrap().exists());
    }

    #[tokio::test]
    async fn startup_counts_receipts_and_their_atomic_write_leftovers() {
        let (store, directory) = test_store();
        let digest = compute_sha256(b"aa");
        repository_upload(&store, b"aa", "repo-a", None, &digest)
            .await
            .unwrap();
        let (receipt, identity) = store
            .repository_upload_evidence(&digest, "repo-a", None)
            .unwrap();
        std::fs::write(
            receipt.parent().unwrap().join(".interrupted.tmp"),
            b"prefix",
        )
        .unwrap();
        let restarted = BlobStore::new(directory.path());
        restarted
            .configure_storage_limit(2 + identity.len() as u64 + 6)
            .await
            .unwrap();
        restarted.budget.set_file_limit(3).unwrap();
        assert!(matches!(
            restarted.initiate_upload().await,
            Err(PickleError::StorageQuotaExceeded)
        ));
        restarted.budget.set_file_limit(4).unwrap();
        let upload = restarted.initiate_upload().await.unwrap();
        assert!(matches!(
            restarted.write_upload_chunk(&upload, b"b").await,
            Err(PickleError::StorageQuotaExceeded)
        ));
        restarted.delete_blob(&digest).unwrap();
        restarted.write_upload_chunk(&upload, b"b").await.unwrap();
    }

    #[test]
    fn compute_sha256_deterministic() {
        let d1 = compute_sha256(b"hello");
        let d2 = compute_sha256(b"hello");
        assert_eq!(d1, d2);
    }

    #[test]
    fn compute_sha256_different_inputs() {
        let d1 = compute_sha256(b"hello");
        let d2 = compute_sha256(b"world");
        assert_ne!(d1, d2);
    }

    #[test]
    fn write_and_read_blob() {
        let (store, _dir) = test_store();
        let data = b"layer data here";
        let digest = compute_sha256(data);

        store.write_blob(data, &digest).unwrap();
        assert!(store.has_blob(&digest));

        let read_back = store.read_blob(&digest).unwrap();
        assert_eq!(read_back, data);
    }

    #[test]
    fn write_blob_digest_mismatch() {
        let (store, _dir) = test_store();
        let data = b"real data";
        let wrong_digest = compute_sha256(b"different data");

        let result = store.write_blob(data, &wrong_digest);
        assert!(result.is_err());
    }

    #[test]
    fn has_blob_false_for_missing() {
        let (store, _dir) = test_store();
        let digest = compute_sha256(b"nonexistent");
        assert!(!store.has_blob(&digest));
    }

    #[test]
    fn blob_size_returns_correct_value() {
        let (store, _dir) = test_store();
        let data = b"twelve bytes";
        let digest = compute_sha256(data);
        store.write_blob(data, &digest).unwrap();

        assert_eq!(store.blob_size(&digest).unwrap(), 12);
    }

    #[test]
    fn delete_blob_removes_file() {
        let (store, _dir) = test_store();
        let data = b"to be deleted";
        let digest = compute_sha256(data);
        store.write_blob(data, &digest).unwrap();

        store.delete_blob(&digest).unwrap();
        assert!(!store.has_blob(&digest));
    }

    #[test]
    fn delete_nonexistent_blob_succeeds() {
        let (store, _dir) = test_store();
        let digest = compute_sha256(b"ghost");
        store.delete_blob(&digest).unwrap();
    }

    #[tokio::test]
    async fn upload_session_full_lifecycle() {
        let (store, _dir) = test_store();
        let data = b"uploaded blob content";
        let digest = compute_sha256(data);

        let upload_id = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload_id, data).await.unwrap();
        store.complete_upload(&upload_id, &digest).await.unwrap();

        assert!(store.has_blob(&digest));
        assert_eq!(store.read_blob(&digest).unwrap(), data);
    }

    #[tokio::test]
    async fn upload_chunked() {
        let (store, _dir) = test_store();
        let part1 = b"first half ";
        let part2 = b"second half";
        let full = [&part1[..], &part2[..]].concat();
        let digest = compute_sha256(&full);

        let upload_id = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload_id, part1).await.unwrap();
        let total = store.write_upload_chunk(&upload_id, part2).await.unwrap();
        assert_eq!(total, full.len() as u64);

        store.complete_upload(&upload_id, &digest).await.unwrap();
        assert_eq!(store.read_blob(&digest).unwrap(), full);
    }

    #[tokio::test]
    async fn commit_upload_as_blob_moves_the_temp_without_re_reading() {
        // M11: the streaming peer-pull path hashes as it writes, then commits
        // by rename. The committed blob must be byte-identical and the temp
        // gone (moved, not copied).
        let (store, _dir) = test_store();
        let part1 = b"streamed ";
        let part2 = b"in chunks";
        let full = [&part1[..], &part2[..]].concat();
        let digest = compute_sha256(&full);

        let upload_id = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload_id, part1).await.unwrap();
        store.write_upload_chunk(&upload_id, part2).await.unwrap();

        store
            .commit_upload_as_blob(&upload_id, &digest)
            .await
            .unwrap();

        assert!(store.has_blob(&digest));
        assert_eq!(store.read_blob(&digest).unwrap(), full);
        // The temp was moved, not left behind.
        assert!(!store.upload_path(&upload_id).exists());
    }

    #[tokio::test]
    async fn commit_upload_as_blob_fails_for_an_unknown_session() {
        let (store, _dir) = test_store();
        let digest = compute_sha256(b"anything");
        // A validly-shaped but never-created upload id.
        let result = store.commit_upload_as_blob(&"a".repeat(32), &digest).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn upload_digest_mismatch_rejects() {
        let (store, _dir) = test_store();
        let data = b"actual data";
        let wrong_digest = compute_sha256(b"wrong");

        let upload_id = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&upload_id, data).await.unwrap();

        let result = store.complete_upload(&upload_id, &wrong_digest).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn upload_nonexistent_session_fails() {
        let (store, _dir) = test_store();
        let result = store.write_upload_chunk("nonexistent", b"data").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn upload_id_with_traversal_is_rejected() {
        let (store, dir) = test_store();

        // A sentinel that lives outside the uploads/ directory. A traversal
        // upload id (`../secret`) would otherwise let a client append to it.
        let sentinel = dir.path().join("secret");
        std::fs::write(&sentinel, b"original").unwrap();

        for bad in ["../secret", "../../etc/passwd", "abc/def", "", "ABCDEF"] {
            let err = store.write_upload_chunk(bad, b"pwned").await.unwrap_err();
            assert!(
                matches!(err, PickleError::InvalidUploadId(_)),
                "expected InvalidUploadId for {bad:?}, got {err:?}"
            );
        }

        // The sentinel must be untouched, and no file written outside uploads/.
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"original");
    }

    #[tokio::test]
    async fn upload_directory_owner_excludes_competitors_and_reclaims_on_replacement() {
        let (store, _dir) = test_store();
        let owner = store.claim_upload_directory().await.unwrap();
        let id = store.initiate_upload().await.unwrap();
        store.write_upload_chunk(&id, b"active").await.unwrap();
        assert!(store.claim_upload_directory().await.is_err());
        assert_eq!(store.upload_size(&id).await.unwrap(), 6);
        drop(owner);
        let _replacement = store.claim_upload_directory().await.unwrap();
        assert!(!store.upload_path(&id).exists());
    }

    /// A child that another thread is forking holds a copy of every open
    /// descriptor until its `exec`, the lock file's included. Dropping the
    /// owner, or refusing a claim after taking the lock, must still release
    /// the lock at once, or the claim straight after is refused as busy (#497).
    #[tokio::test]
    async fn a_dropped_owner_is_replaced_while_other_threads_spawn_processes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (store, _dir) = test_store();
        let spawning = Arc::new(AtomicBool::new(true));
        let spawners: Vec<_> = (0..2)
            .map(|_| {
                let spawning = Arc::clone(&spawning);
                std::thread::spawn(move || {
                    while spawning.load(Ordering::Relaxed) {
                        let _ = std::process::Command::new("true")
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                })
            })
            .collect();
        let unexpected = store.base_dir.join("uploads").join("operator-file");
        let mut refused = 0;
        for _ in 0..300 {
            match store.claim_upload_directory().await {
                Ok(owner) => drop(owner),
                Err(_) => refused += 1,
            }
            // A claim refused for an unrecognised entry has taken the lock.
            std::fs::write(&unexpected, b"keep").unwrap();
            assert!(store.claim_upload_directory().await.is_err());
            std::fs::remove_file(&unexpected).unwrap();
        }
        spawning.store(false, Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().unwrap();
        }
        assert_eq!(refused, 0, "claims refused by a lock nobody holds");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn upload_recovery_never_follows_a_redirected_upload_directory() {
        let (store, _dir) = test_store();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("a".repeat(32));
        std::fs::write(&sentinel, b"not owned").unwrap();
        std::os::unix::fs::symlink(outside.path(), store.base_dir.join("uploads")).unwrap();
        assert!(store.claim_upload_directory().await.is_err());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"not owned");
    }

    #[tokio::test]
    async fn upload_recovery_preserves_unrecognised_entries_and_refuses_startup() {
        let (store, _dir) = test_store();
        let uploads = store.base_dir.join("uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        let unexpected = uploads.join("operator-file");
        std::fs::write(&unexpected, b"keep").unwrap();
        assert!(store.claim_upload_directory().await.is_err());
        assert_eq!(std::fs::read(&unexpected).unwrap(), b"keep");
        std::fs::remove_file(unexpected).unwrap();
        let directory = uploads.join("a".repeat(32));
        std::fs::create_dir(&directory).unwrap();
        assert!(store.claim_upload_directory().await.is_err());
        assert!(directory.is_dir());
        std::fs::remove_dir(directory).unwrap();
        let _owner = store.claim_upload_directory().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_upload_cleans_up() {
        let (store, _dir) = test_store();
        let upload_id = store.initiate_upload().await.unwrap();
        store
            .write_upload_chunk(&upload_id, b"partial")
            .await
            .unwrap();
        store.cancel_upload(&upload_id).await.unwrap();
        store.cancel_upload(&upload_id).await.unwrap();

        // Writing to cancelled session should fail
        let result = store.write_upload_chunk(&upload_id, b"more").await;
        assert!(result.is_err());
    }

    /// REG5: a durable write leaves no torn final file and no leftover
    /// temp. After it returns, the blob is present and reads back exactly.
    #[test]
    fn durable_write_leaves_no_temp_and_a_complete_blob() {
        let (store, _dir) = test_store();
        let data = b"durable content";
        let digest = compute_sha256(data);

        store.write_blob(data, &digest).unwrap();
        assert_eq!(store.read_blob(&digest).unwrap(), data);

        // No stray temp files in the blob's directory.
        let parent = store.blob_path(&digest).parent().unwrap().to_path_buf();
        let leftovers: Vec<_> = std::fs::read_dir(&parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "left a temp file behind: {leftovers:?}"
        );
    }

    /// REG5: a crash between temp-write and rename must leave the *old*
    /// blob intact. We simulate the interrupted write by dropping a stray
    /// temp file next to an existing good blob and confirming the good
    /// blob still reads back — the torn temp is never treated as the blob.
    #[test]
    fn torn_write_leaves_the_old_blob_intact() {
        let (store, _dir) = test_store();
        let data = b"the committed bytes";
        let digest = compute_sha256(data);
        store.write_blob(data, &digest).unwrap();

        // A leftover temp from an interrupted write of the same digest.
        let path = store.blob_path(&digest);
        let parent = path.parent().unwrap();
        let stray = parent.join(format!(
            ".{}.{:032x}.tmp",
            path.file_name().unwrap().to_string_lossy(),
            0u128
        ));
        std::fs::write(&stray, b"half-written garbage").unwrap();

        // The real blob is unaffected: the final path never held the temp.
        assert_eq!(store.read_blob(&digest).unwrap(), data);
    }

    /// REG5: a cached blob whose bytes were corrupted on disk must be
    /// rejected on revalidation, and removed so a refetch can replace it.
    #[test]
    fn corrupt_cached_blob_is_rejected_and_removed() {
        let (store, _dir) = test_store();
        let data = b"honest layer bytes";
        let digest = compute_sha256(data);
        store.write_blob(data, &digest).unwrap();
        assert!(store.revalidate_blob(&digest).unwrap());

        // Corrupt the on-disk bytes behind the store's back.
        std::fs::write(store.blob_path(&digest), b"tampered").unwrap();
        assert!(
            !store.revalidate_blob(&digest).unwrap(),
            "corrupt blob accepted"
        );
        assert!(!store.has_blob(&digest), "corrupt blob was not removed");
    }

    /// A blob larger than one read buffer hashes correctly across chunks,
    /// and a change in its last byte is still caught.
    #[test]
    fn multi_chunk_blob_revalidates_and_a_late_mismatch_is_caught() {
        let (store, _dir) = test_store();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let digest = compute_sha256(&data);
        store.write_blob(&data, &digest).unwrap();
        assert!(store.revalidate_blob(&digest).unwrap());

        let mut tampered = data.clone();
        *tampered.last_mut().unwrap() ^= 1;
        std::fs::write(store.blob_path(&digest), &tampered).unwrap();
        assert!(!store.revalidate_blob(&digest).unwrap());
        assert!(!store.has_blob(&digest));
    }

    /// An I/O failure says nothing about the bytes: it must surface as an
    /// error, not look like "not cached" and trigger a pointless refetch
    /// over a blob that may be perfectly good.
    #[test]
    fn unreadable_blob_is_an_error_not_a_cache_miss() {
        let (store, _dir) = test_store();
        let digest = compute_sha256(b"layer behind a broken disk");
        // A directory where the blob file should be: opening it for reading
        // fails with a real I/O error on every platform, even as root.
        std::fs::create_dir_all(store.blob_path(&digest)).unwrap();
        assert!(store.revalidate_blob(&digest).is_err());
        assert!(
            store.blob_path(&digest).exists(),
            "an unreadable blob must not be deleted as if it were corrupt"
        );
    }

    #[test]
    fn revalidate_missing_blob_is_false() {
        let (store, _dir) = test_store();
        let digest = compute_sha256(b"never stored");
        assert!(!store.revalidate_blob(&digest).unwrap());
    }

    #[test]
    fn list_blobs_empty_store() {
        let (store, _dir) = test_store();
        assert!(store.list_blobs().unwrap().is_empty());
    }

    #[test]
    fn list_blobs_returns_stored() {
        let (store, _dir) = test_store();
        let d1 = compute_sha256(b"blob1");
        let d2 = compute_sha256(b"blob2");
        store.write_blob(b"blob1", &d1).unwrap();
        store.write_blob(b"blob2", &d2).unwrap();

        let blobs = store.list_blobs().unwrap();
        assert_eq!(blobs.len(), 2);
    }
}
