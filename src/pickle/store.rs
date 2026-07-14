//! Content-addressed blob store for Pickle.
//!
//! Stores blobs as `{base_dir}/blobs/sha256/{hex}/data`. Shares the
//! same on-disk layout as `grill::image::ImageStore`, so blobs cached
//! from Docker Hub are visible to Pickle and vice versa.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as Sha2Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::types::{Digest, PickleError};

/// Write `data` to `path` durably: a unique temp file in the same
/// directory, `fsync` the file, rename over the target, then `fsync` the
/// parent directory so the rename itself survives a crash (REG5).
///
/// A crash at any point leaves either the old file or the new one, never a
/// torn half-written final. The temp name carries a random suffix so two
/// concurrent writers to the same digest never share a temp path.
///
/// `&Path`/`&[u8]` are borrows: this helper reads the bytes and the path
/// without taking ownership, so the caller keeps using them afterwards.
fn write_file_durably(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let tmp = parent.join(format!(
        ".{}.{:032x}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        rand::random::<u128>()
    ));

    // Scope the file handle so it closes before the rename.
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
    }

    // If the rename fails, clean up the temp so it isn't nominated as an
    // orphan later.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // fsync the directory so the rename (a directory-metadata change) is
    // durable. A missing/inaccessible dir handle is non-fatal — the data
    // file itself is already synced.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

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
/// and stateless path lookups (no shared mutable state for reads).
#[derive(Debug, Clone)]
pub struct BlobStore {
    base_dir: PathBuf,
}

impl BlobStore {
    /// Create a new blob store rooted at `base_dir`.
    ///
    /// The directory structure is created on demand — `base_dir` itself
    /// must exist, but subdirectories are created as blobs are written.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Path to a blob on disk.
    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.base_dir
            .join("blobs")
            .join("sha256")
            .join(digest.hex())
            .join("data")
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
        write_file_durably(&path, data)?;
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
    /// Hashing runs on the caller's thread; whole-blob callers on the async
    /// runtime should wrap this in `spawn_blocking`.
    pub fn revalidate_blob(&self, digest: &Digest) -> bool {
        let path = self.blob_path(digest);
        let Ok(data) = std::fs::read(&path) else {
            return false;
        };
        if compute_sha256(&data).as_str() == digest.as_str() {
            true
        } else {
            // Corrupt: remove it so the next pull refetches clean bytes.
            let _ = std::fs::remove_file(&path);
            false
        }
    }

    /// Delete a blob.
    pub fn delete_blob(&self, digest: &Digest) -> Result<(), PickleError> {
        let path = self.blob_path(digest);
        if path.exists() {
            std::fs::remove_file(&path)?;
            // Clean up empty parent directories
            if let Some(parent) = path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
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
        let upload_dir = self.base_dir.join("uploads");
        tokio::fs::create_dir_all(&upload_dir).await?;
        // Create empty file to mark the session
        tokio::fs::File::create(self.upload_path(&upload_id)).await?;
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
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await?;
        file.write_all(data).await?;
        file.flush().await?;
        let meta = tokio::fs::metadata(&path).await?;
        Ok(meta.len())
    }

    /// Complete an upload: verify digest, move to blob store.
    pub async fn complete_upload(
        &self,
        upload_id: &str,
        expected_digest: &Digest,
    ) -> Result<(), PickleError> {
        validate_upload_id(upload_id)?;
        let upload_path = self.upload_path(upload_id);
        if !upload_path.exists() {
            return Err(PickleError::UploadNotFound(upload_id.to_string()));
        }

        // Read the upload and verify digest
        let data = tokio::fs::read(&upload_path).await?;
        let actual = compute_sha256(&data);
        if actual.as_str() != expected_digest.as_str() {
            // Clean up the failed upload
            let _ = tokio::fs::remove_file(&upload_path).await;
            return Err(PickleError::DigestMismatch {
                expected: expected_digest.clone(),
                actual,
            });
        }

        // Durable move to the blob store (REG5): re-read the verified
        // upload bytes and write them through the fsync/rename transaction
        // (`write_file_durably`), then remove the upload temp. The digest
        // was already checked above, so these are the correct bytes. Runs on
        // a blocking thread — the file reads, fsyncs and rename are blocking
        // syscalls that must not sit on a Tokio worker.
        let blob_path = self.blob_path(expected_digest);
        let upload = upload_path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let data = std::fs::read(&upload)?;
            write_file_durably(&blob_path, &data)?;
            let _ = std::fs::remove_file(&upload);
            Ok(())
        })
        .await
        .map_err(|e| PickleError::CatalogPersist(format!("blob commit task failed: {e}")))??;

        Ok(())
    }

    /// Cancel an upload session, cleaning up the temp file.
    pub async fn cancel_upload(&self, upload_id: &str) {
        if validate_upload_id(upload_id).is_err() {
            return;
        }
        let path = self.upload_path(upload_id);
        let _ = tokio::fs::remove_file(path).await;
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
            if entry.file_type()?.is_dir() {
                let hex = entry.file_name().to_string_lossy().to_string();
                let data_path = entry.path().join("data");
                if data_path.exists() {
                    digests.push(Digest(format!("sha256:{hex}")));
                }
            }
        }
        Ok(digests)
    }

    /// The base directory of this store.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }
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
    async fn cancel_upload_cleans_up() {
        let (store, _dir) = test_store();
        let upload_id = store.initiate_upload().await.unwrap();
        store
            .write_upload_chunk(&upload_id, b"partial")
            .await
            .unwrap();
        store.cancel_upload(&upload_id).await;

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
        assert!(store.revalidate_blob(&digest));

        // Corrupt the on-disk bytes behind the store's back.
        std::fs::write(store.blob_path(&digest), b"tampered").unwrap();
        assert!(!store.revalidate_blob(&digest), "corrupt blob accepted");
        assert!(!store.has_blob(&digest), "corrupt blob was not removed");
    }

    #[test]
    fn revalidate_missing_blob_is_false() {
        let (store, _dir) = test_store();
        let digest = compute_sha256(b"never stored");
        assert!(!store.revalidate_blob(&digest));
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
