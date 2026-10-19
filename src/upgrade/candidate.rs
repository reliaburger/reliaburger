//! A candidate binary kept on disk, never in memory (#892).
//!
//! A release binary is over 100 MB. Holding it in a `Vec` while it was
//! fetched, hashed, signature-checked, copied for the compatibility probe and
//! staged cost a node several copies of it at once, and on a 2 GiB node that
//! already served the cluster's API the kernel killed Bun for it. So the
//! bytes go straight to a private file in the binary directory as they
//! arrive, and every later step reads that file: the signature check through
//! a read-only memory map, the compatibility probe by running it, and staging
//! by renaming it into place.

use std::path::Path;

use super::compatibility::Probe;
use super::error::UpgradeError;
use super::signing::{self, PublicKey, SignatureEnvelope};

/// A private copy of a candidate binary that nothing else can change.
///
/// The file lives in the binary directory, so staging it is a rename on the
/// same filesystem, and it is created owner-only with a name nobody can
/// predict. Dropping a `Candidate` deletes the file.
#[derive(Debug)]
pub(super) struct Candidate(tempfile::TempPath);

impl Candidate {
    /// A new, empty candidate in `directory`, and a handle to write it.
    ///
    /// The caller writes the binary through the handle, flushes it (a Tokio
    /// file finishes a write in the background) and drops it before the
    /// candidate runs: Linux refuses to exec a file open for writing.
    pub(super) async fn create(directory: &Path) -> Result<(Self, tokio::fs::File), UpgradeError> {
        let directory = directory.to_path_buf();
        let (file, path) = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            tempfile::Builder::new()
                .prefix(super::store::TEMPORARY_PREFIXES[0])
                .tempfile_in(&directory)
                .map(tempfile::NamedTempFile::into_parts)
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok((Self(path), tokio::fs::File::from_std(file)))
    }

    /// Copy the file at `source` into a new candidate in `directory`.
    ///
    /// The copy is the candidate, so a path that changes after it was read
    /// can't swap the bytes between the checks and the exec. This blocks:
    /// call it on the blocking pool. `std::io::copy` moves the bytes in the
    /// kernel where it can and through a small buffer where it can't.
    pub(super) fn copy_from(source: &Path, directory: &Path) -> Result<Self, UpgradeError> {
        std::fs::create_dir_all(directory)?;
        let mut input = std::fs::File::open(source)?;
        let (mut output, path) = tempfile::Builder::new()
            .prefix(super::store::TEMPORARY_PREFIXES[0])
            .tempfile_in(directory)?
            .into_parts();
        std::io::copy(&mut input, &mut output)?;
        Ok(Self(path))
    }

    /// The candidate's SHA-256, read in pieces. Blocks.
    pub(super) fn sha256(&self) -> std::io::Result<String> {
        signing::sha256_file(&self.0)
    }

    /// Check the candidate against its signature envelope, exactly as
    /// [`signing::verify_binary`] checks bytes. Blocks: it hashes the whole
    /// file and checks up to two signatures over it.
    pub(super) fn verify(
        &self,
        envelope: &SignatureEnvelope,
        release_keys: &[PublicKey],
        external_key: Option<&PublicKey>,
        network: bool,
    ) -> Result<(), UpgradeError> {
        let file = std::fs::File::open(&self.0)?;
        if file.metadata()?.len() == 0 {
            return signing::verify_binary(&[], envelope, release_keys, external_key, network);
        }
        // Ed25519 signs the whole message, and `ring` checks a signature
        // over one contiguous slice, so the check needs all the bytes at
        // once. A read-only map gives it them without copying them into the
        // heap: the pages come from the page cache, count as file-backed
        // rather than anonymous memory, and the kernel can drop them under
        // pressure and read them back.
        //
        // SAFETY: the map is undefined behaviour only if the file changes
        // while it is mapped. This file is a private copy that this process
        // created owner-only, under an unpredictable name in the binary
        // directory, and has finished writing; nothing else writes it, and
        // the map is dropped before this function returns.
        let map = unsafe { memmap2::Mmap::map(&file)? };
        signing::verify_binary(&map, envelope, release_keys, external_key, network)
    }

    /// Make the candidate runnable by its owner and hand it over as the
    /// compatibility probe. Blocks briefly (one `chmod`).
    pub(super) fn into_probe(self) -> Result<Probe, UpgradeError> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700))?;
        Ok(Probe::new(self.0))
    }

    /// The file, for [`BinaryStore::stage_file`](super::store::BinaryStore::stage_file).
    pub(super) fn into_temp_path(self) -> tempfile::TempPath {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt as _;

    #[tokio::test]
    async fn a_written_candidate_holds_every_piece_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let (candidate, mut file) = Candidate::create(directory.path()).await.unwrap();
        for piece in [&b"first "[..], b"second ", b"third"] {
            file.write_all(piece).await.unwrap();
        }
        file.flush().await.unwrap();
        drop(file);

        assert_eq!(std::fs::read(&candidate.0).unwrap(), b"first second third");
        assert_eq!(
            candidate.sha256().unwrap(),
            signing::sha256_hex(b"first second third")
        );
    }

    #[test]
    fn a_dropped_candidate_leaves_nothing_behind() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        std::fs::write(&source, b"bytes").unwrap();
        let binaries = directory.path().join("bin");

        let candidate = Candidate::copy_from(&source, &binaries).unwrap();
        assert_eq!(std::fs::read_dir(&binaries).unwrap().count(), 1);
        drop(candidate);

        assert_eq!(std::fs::read_dir(&binaries).unwrap().count(), 0);
    }

    #[test]
    fn a_copied_candidate_is_owner_only_and_independent_of_its_source() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        std::fs::write(&source, b"original").unwrap();

        let candidate = Candidate::copy_from(&source, directory.path()).unwrap();
        std::fs::write(&source, b"replaced").unwrap();

        assert_eq!(std::fs::read(&candidate.0).unwrap(), b"original");
        let mode = std::fs::metadata(&candidate.0)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "mode {mode:o}");
    }

    #[test]
    fn verification_reads_the_file_and_refuses_other_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let (pkcs8, public) = signing::generate_keypair().unwrap();
        let signed = b"the signed binary".to_vec();
        let envelope = SignatureEnvelope {
            schema: 1,
            sha256: signing::sha256_hex(&signed),
            embedded: signing::sign(&pkcs8, &signed).unwrap(),
            external: None,
        };
        let good = directory.path().join("good");
        std::fs::write(&good, &signed).unwrap();
        let tampered = directory.path().join("tampered");
        std::fs::write(&tampered, b"the signed binarY").unwrap();
        let empty = directory.path().join("empty");
        std::fs::write(&empty, b"").unwrap();

        let candidate = |path: &Path| Candidate::copy_from(path, directory.path()).unwrap();
        candidate(&good)
            .verify(&envelope, &[public], None, false)
            .unwrap();
        assert!(matches!(
            candidate(&tampered).verify(&envelope, &[public], None, false),
            Err(UpgradeError::HashMismatch { .. })
        ));
        assert!(matches!(
            candidate(&empty).verify(&envelope, &[public], None, false),
            Err(UpgradeError::HashMismatch { .. })
        ));
    }
}
