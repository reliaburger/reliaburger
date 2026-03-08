/// OCI image pulling and unpacking.
///
/// Pulls container images from OCI-compliant registries (Docker Hub,
/// GHCR, etc.) using the OCI Distribution API. Layers are cached as
/// content-addressed blobs and unpacked into a rootfs directory that
/// runc can use directly.
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// A parsed OCI image reference.
///
/// Normalises Docker Hub shorthand: `"alpine"` becomes
/// `docker.io/library/alpine:latest`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageReference {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

/// Errors from image operations.
#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("invalid image reference: {0}")]
    InvalidReference(String),

    #[error("failed to pull manifest for {image}: {reason}")]
    ManifestPull { image: String, reason: String },

    #[error("failed to pull layer {digest}: {reason}")]
    LayerPull { digest: String, reason: String },

    #[error("digest mismatch for layer {digest}: expected {expected}, got {actual}")]
    DigestMismatch {
        digest: String,
        expected: String,
        actual: String,
    },

    #[error("failed to unpack layer {digest}: {reason}")]
    UnpackFailed { digest: String, reason: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl ImageReference {
    /// Parse an image reference string into its components.
    ///
    /// Handles Docker Hub shorthand:
    /// - `"alpine"` → `docker.io/library/alpine:latest`
    /// - `"alpine:3.19"` → `docker.io/library/alpine:3.19`
    /// - `"myuser/myimage:v1"` → `docker.io/myuser/myimage:v1`
    /// - `"ghcr.io/org/image:sha"` → as-is
    /// - `"localhost:5000/myimage:v1"` → as-is
    pub fn parse(s: &str) -> Result<Self, ImageError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ImageError::InvalidReference(
                "empty image reference".to_string(),
            ));
        }

        // Split off the tag (last `:` that isn't part of a port number)
        let (name_part, tag) = split_name_tag(s);

        // Determine if the first component is a registry (contains `.` or `:`)
        let parts: Vec<&str> = name_part.splitn(2, '/').collect();

        let (registry, repository) = if parts.len() == 1 {
            // Bare name like "alpine" → docker.io/library/alpine
            ("docker.io".to_string(), format!("library/{}", parts[0]))
        } else {
            let first = parts[0];
            if first.contains('.') || first.contains(':') {
                // Custom registry like "ghcr.io/org/image" or "localhost:5000/image"
                (first.to_string(), parts[1].to_string())
            } else {
                // Docker Hub user repo like "myuser/myimage"
                ("docker.io".to_string(), name_part.to_string())
            }
        };

        Ok(Self {
            registry,
            repository,
            tag,
        })
    }

    /// Format as a full reference string.
    pub fn full_reference(&self) -> String {
        format!("{}/{}:{}", self.registry, self.repository, self.tag)
    }

    /// Convert to an `oci_distribution::Reference` for the client.
    pub fn to_oci_reference(&self) -> Result<oci_distribution::Reference, ImageError> {
        self.full_reference()
            .parse()
            .map_err(|e: oci_distribution::ParseError| {
                ImageError::InvalidReference(format!("{}: {e}", self.full_reference()))
            })
    }
}

/// Split an image name into (name, tag). Defaults tag to "latest".
fn split_name_tag(s: &str) -> (&str, String) {
    // Find the last `/` to separate the path from the potential tag
    let after_last_slash = s.rfind('/').map(|i| i + 1).unwrap_or(0);
    let tail = &s[after_last_slash..];

    // Look for `:` in the tail portion (after the last `/`)
    if let Some(colon_pos) = tail.rfind(':') {
        let absolute_colon = after_last_slash + colon_pos;
        (&s[..absolute_colon], s[absolute_colon + 1..].to_string())
    } else {
        (s, "latest".to_string())
    }
}

/// Content-addressed image store on disk.
///
/// Disk layout:
/// ```text
/// {store_root}/
///   blobs/sha256/{digest}                    — raw layer blobs
///   rootfs/{registry}/{repo}/{tag}/          — unpacked filesystem
///   manifests/{registry}/{repo}/{tag}.json   — cached manifests
/// ```
pub struct ImageStore {
    store_root: PathBuf,
}

impl ImageStore {
    /// Create a new image store at the given root directory.
    pub fn new(store_root: PathBuf) -> Self {
        Self { store_root }
    }

    /// Create a store using the default rootless location.
    ///
    /// Uses `~/.local/share/reliaburger/images/` via the `dirs` crate.
    pub fn rootless_default() -> Self {
        let base = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp/reliaburger-images"))
            .join("reliaburger")
            .join("images");
        Self::new(base)
    }

    /// Path to a cached blob by its SHA-256 digest.
    pub fn blob_path(&self, digest: &str) -> PathBuf {
        // digest is typically "sha256:abcdef..." — strip the algorithm prefix
        let hash = digest.strip_prefix("sha256:").unwrap_or(digest);
        self.store_root.join("blobs").join("sha256").join(hash)
    }

    /// Path to the unpacked rootfs for an image reference.
    pub fn rootfs_path(&self, image_ref: &ImageReference) -> PathBuf {
        self.store_root
            .join("rootfs")
            .join(&image_ref.registry)
            .join(&image_ref.repository)
            .join(&image_ref.tag)
    }

    /// Path to the cached manifest for an image reference.
    fn manifest_path(&self, image_ref: &ImageReference) -> PathBuf {
        self.store_root
            .join("manifests")
            .join(&image_ref.registry)
            .join(&image_ref.repository)
            .join(format!("{}.json", image_ref.tag))
    }

    /// Pull an image and unpack it into a rootfs directory.
    ///
    /// Returns the path to the unpacked rootfs. Caches blobs and
    /// manifests on disk; subsequent pulls of the same image are fast.
    pub async fn pull_and_unpack(&self, image: &str) -> Result<PathBuf, ImageError> {
        let image_ref = ImageReference::parse(image)?;
        let oci_ref = image_ref.to_oci_reference()?;

        let rootfs = self.rootfs_path(&image_ref);

        // Create the OCI distribution client. The default ClientConfig
        // includes a platform resolver that picks the current host's
        // architecture from multi-platform manifest lists.
        let client_config = oci_distribution::client::ClientConfig {
            protocol: oci_distribution::client::ClientProtocol::Https,
            ..Default::default()
        };
        let client = oci_distribution::Client::new(client_config);
        let auth = oci_distribution::secrets::RegistryAuth::Anonymous;

        // Pull the manifest (handles multi-platform resolution automatically)
        let (manifest, _digest, _config) = client
            .pull_manifest_and_config(&oci_ref, &auth)
            .await
            .map_err(|e| ImageError::ManifestPull {
            image: image_ref.full_reference(),
            reason: e.to_string(),
        })?;

        // Save the manifest for cache validation
        let manifest_path = self.manifest_path(&image_ref);
        if let Some(parent) = manifest_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let manifest_json =
            serde_json::to_string_pretty(&manifest).map_err(|e| ImageError::ManifestPull {
                image: image_ref.full_reference(),
                reason: format!("failed to serialise manifest: {e}"),
            })?;
        tokio::fs::write(&manifest_path, &manifest_json).await?;

        // Download each layer blob (skip if cached)
        for layer in &manifest.layers {
            let digest = &layer.digest;
            let blob_path = self.blob_path(digest);

            if blob_path.exists() {
                continue;
            }

            if let Some(parent) = blob_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            let mut blob_data: Vec<u8> = Vec::new();
            client
                .pull_blob(&oci_ref, layer, &mut blob_data)
                .await
                .map_err(|e| ImageError::LayerPull {
                    digest: digest.clone(),
                    reason: e.to_string(),
                })?;

            // Verify the SHA-256 digest
            let computed = format!("sha256:{}", sha256_hex(&blob_data));
            if computed != *digest {
                return Err(ImageError::DigestMismatch {
                    digest: digest.clone(),
                    expected: digest.clone(),
                    actual: computed,
                });
            }

            tokio::fs::write(&blob_path, &blob_data).await?;
        }

        // Unpack layers into rootfs (base-first)
        // Do this in a blocking task since tar extraction is CPU-bound
        let layer_paths: Vec<PathBuf> = manifest
            .layers
            .iter()
            .map(|l| self.blob_path(&l.digest))
            .collect();
        let rootfs_clone = rootfs.clone();

        tokio::task::spawn_blocking(move || unpack_layers(&layer_paths, &rootfs_clone))
            .await
            .map_err(|e| ImageError::UnpackFailed {
                digest: "join".to_string(),
                reason: e.to_string(),
            })??;

        Ok(rootfs)
    }
}

/// Compute the SHA-256 hex digest of some data.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Unpack OCI image layers (gzipped tarballs) into a rootfs directory.
///
/// Layers are applied base-first (index 0 is the bottom layer).
/// Handles OCI whiteout files:
/// - `.wh.<name>` — delete `<name>` from a lower layer
/// - `.wh..wh..opq` — clear the entire directory (opaque whiteout)
pub fn unpack_layers(layer_paths: &[PathBuf], rootfs: &Path) -> Result<(), ImageError> {
    // Clear and recreate rootfs
    if rootfs.exists() {
        std::fs::remove_dir_all(rootfs).map_err(|e| ImageError::UnpackFailed {
            digest: "rootfs".to_string(),
            reason: format!("failed to clear rootfs: {e}"),
        })?;
    }
    std::fs::create_dir_all(rootfs).map_err(|e| ImageError::UnpackFailed {
        digest: "rootfs".to_string(),
        reason: format!("failed to create rootfs: {e}"),
    })?;

    for layer_path in layer_paths {
        let digest = layer_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let file = std::fs::File::open(layer_path).map_err(|e| ImageError::UnpackFailed {
            digest: digest.clone(),
            reason: format!("failed to open layer blob: {e}"),
        })?;

        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        archive.set_preserve_permissions(true);
        archive.set_unpack_xattrs(true);
        archive.set_overwrite(true);

        for entry_result in archive.entries().map_err(|e| ImageError::UnpackFailed {
            digest: digest.clone(),
            reason: format!("failed to read tar entries: {e}"),
        })? {
            let mut entry = entry_result.map_err(|e| ImageError::UnpackFailed {
                digest: digest.clone(),
                reason: format!("failed to read tar entry: {e}"),
            })?;

            let path = entry.path().map_err(|e| ImageError::UnpackFailed {
                digest: digest.clone(),
                reason: format!("failed to read entry path: {e}"),
            })?;
            let path = path.to_path_buf();

            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            // Handle opaque whiteout: clear the entire parent directory
            if file_name == ".wh..wh..opq" {
                if let Some(parent) = path.parent() {
                    let target = rootfs.join(parent);
                    if target.exists() {
                        // Remove all existing contents but keep the directory
                        for child in
                            std::fs::read_dir(&target).map_err(|e| ImageError::UnpackFailed {
                                digest: digest.clone(),
                                reason: format!("failed to read dir for opaque whiteout: {e}"),
                            })?
                        {
                            let child = child.map_err(|e| ImageError::UnpackFailed {
                                digest: digest.clone(),
                                reason: format!("failed to read dir entry: {e}"),
                            })?;
                            let child_path = child.path();
                            if child_path.is_dir() {
                                let _ = std::fs::remove_dir_all(&child_path);
                            } else {
                                let _ = std::fs::remove_file(&child_path);
                            }
                        }
                    }
                }
                continue;
            }

            // Handle whiteout: delete the named file from a lower layer
            if let Some(deleted_name) = file_name.strip_prefix(".wh.") {
                if let Some(parent) = path.parent() {
                    let target = rootfs.join(parent).join(deleted_name);
                    if target.is_dir() {
                        let _ = std::fs::remove_dir_all(&target);
                    } else {
                        let _ = std::fs::remove_file(&target);
                    }
                }
                continue;
            }

            // Skip device nodes (can't create without root)
            let entry_type = entry.header().entry_type();
            if entry_type == tar::EntryType::Block || entry_type == tar::EntryType::Char {
                continue;
            }

            // Unpack the entry
            entry
                .unpack_in(rootfs)
                .map_err(|e| ImageError::UnpackFailed {
                    digest: digest.clone(),
                    reason: format!("failed to unpack {}: {e}", path.display()),
                })?;
        }
    }

    Ok(())
}

/// Check if a string looks like an image reference rather than a filesystem path.
///
/// Image references contain `:` (tag separator) or don't start with `/`.
/// Filesystem paths start with `/` or `.`.
pub fn looks_like_image_ref(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Absolute paths are not image refs
    if s.starts_with('/') || s.starts_with('.') {
        return false;
    }
    // If it doesn't start with / or ., it's likely an image reference
    // (e.g. "alpine", "alpine:latest", "ghcr.io/org/image:v1")
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- ImageReference::parse -------------------------------------------------

    #[test]
    fn parse_bare_name_adds_docker_hub_library() {
        let r = ImageReference::parse("alpine").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn parse_name_with_tag() {
        let r = ImageReference::parse("alpine:3.19").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/alpine");
        assert_eq!(r.tag, "3.19");
    }

    #[test]
    fn parse_user_repo() {
        let r = ImageReference::parse("myuser/myimage:v1").unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "myuser/myimage");
        assert_eq!(r.tag, "v1");
    }

    #[test]
    fn parse_custom_registry() {
        let r = ImageReference::parse("ghcr.io/org/image:sha").unwrap();
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "org/image");
        assert_eq!(r.tag, "sha");
    }

    #[test]
    fn parse_registry_with_port() {
        let r = ImageReference::parse("localhost:5000/myimage:v1").unwrap();
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "myimage");
        assert_eq!(r.tag, "v1");
    }

    #[test]
    fn parse_empty_string_errors() {
        assert!(ImageReference::parse("").is_err());
        assert!(ImageReference::parse("   ").is_err());
    }

    // -- Store path construction -----------------------------------------------

    #[test]
    fn blob_path_from_digest() {
        let store = ImageStore::new(PathBuf::from("/tmp/images"));
        let path = store.blob_path("sha256:abc123");
        assert_eq!(path, PathBuf::from("/tmp/images/blobs/sha256/abc123"));
    }

    #[test]
    fn blob_path_without_prefix() {
        let store = ImageStore::new(PathBuf::from("/tmp/images"));
        let path = store.blob_path("abc123");
        assert_eq!(path, PathBuf::from("/tmp/images/blobs/sha256/abc123"));
    }

    #[test]
    fn rootfs_path_from_reference() {
        let store = ImageStore::new(PathBuf::from("/tmp/images"));
        let image_ref = ImageReference::parse("alpine:3.19").unwrap();
        let path = store.rootfs_path(&image_ref);
        assert_eq!(
            path,
            PathBuf::from("/tmp/images/rootfs/docker.io/library/alpine/3.19")
        );
    }

    // -- looks_like_image_ref --------------------------------------------------

    #[test]
    fn image_ref_detection() {
        assert!(looks_like_image_ref("alpine"));
        assert!(looks_like_image_ref("alpine:latest"));
        assert!(looks_like_image_ref("myuser/myimage:v1"));
        assert!(looks_like_image_ref("ghcr.io/org/image:v1"));
        assert!(!looks_like_image_ref("/var/lib/rootfs"));
        assert!(!looks_like_image_ref("./rootfs"));
        assert!(!looks_like_image_ref(""));
    }

    // -- Layer unpacking -------------------------------------------------------

    #[test]
    fn unpack_single_layer_creates_files() {
        let tmp = tempfile::tempdir().unwrap();

        // Create a synthetic gzipped tar with a single file
        let layer_path = tmp.path().join("layer.tar.gz");
        create_test_layer(&layer_path, &[("hello.txt", b"hello world")]);

        let rootfs = tmp.path().join("rootfs");
        unpack_layers(&[layer_path], &rootfs).unwrap();

        let content = std::fs::read_to_string(rootfs.join("hello.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn unpack_multi_layer_applies_in_order() {
        let tmp = tempfile::tempdir().unwrap();

        // Layer 1: create a file
        let layer1 = tmp.path().join("layer1.tar.gz");
        create_test_layer(&layer1, &[("data.txt", b"from layer 1")]);

        // Layer 2: overwrite the file
        let layer2 = tmp.path().join("layer2.tar.gz");
        create_test_layer(&layer2, &[("data.txt", b"from layer 2")]);

        let rootfs = tmp.path().join("rootfs");
        unpack_layers(&[layer1, layer2], &rootfs).unwrap();

        let content = std::fs::read_to_string(rootfs.join("data.txt")).unwrap();
        assert_eq!(content, "from layer 2");
    }

    #[test]
    fn unpack_whiteout_deletes_file() {
        let tmp = tempfile::tempdir().unwrap();

        // Layer 1: create two files
        let layer1 = tmp.path().join("layer1.tar.gz");
        create_test_layer(
            &layer1,
            &[("keep.txt", b"keep me"), ("remove.txt", b"delete me")],
        );

        // Layer 2: whiteout for remove.txt
        let layer2 = tmp.path().join("layer2.tar.gz");
        create_test_layer(&layer2, &[(".wh.remove.txt", b"")]);

        let rootfs = tmp.path().join("rootfs");
        unpack_layers(&[layer1, layer2], &rootfs).unwrap();

        assert!(rootfs.join("keep.txt").exists());
        assert!(!rootfs.join("remove.txt").exists());
        assert!(!rootfs.join(".wh.remove.txt").exists());
    }

    #[test]
    fn unpack_opaque_whiteout_clears_directory() {
        let tmp = tempfile::tempdir().unwrap();

        // Layer 1: create a directory with files
        let layer1 = tmp.path().join("layer1.tar.gz");
        create_test_layer_with_dirs(
            &layer1,
            &["subdir/"],
            &[
                ("subdir/old1.txt", b"old file 1"),
                ("subdir/old2.txt", b"old file 2"),
            ],
        );

        // Layer 2: opaque whiteout + new file in subdir
        let layer2 = tmp.path().join("layer2.tar.gz");
        create_test_layer_with_dirs(
            &layer2,
            &["subdir/"],
            &[
                ("subdir/.wh..wh..opq", b""),
                ("subdir/new.txt", b"new file"),
            ],
        );

        let rootfs = tmp.path().join("rootfs");
        unpack_layers(&[layer1, layer2], &rootfs).unwrap();

        assert!(!rootfs.join("subdir/old1.txt").exists());
        assert!(!rootfs.join("subdir/old2.txt").exists());
        assert!(rootfs.join("subdir/new.txt").exists());
    }

    #[test]
    fn unpack_preserves_symlinks() {
        let tmp = tempfile::tempdir().unwrap();

        let layer_path = tmp.path().join("layer.tar.gz");
        create_test_layer_with_symlinks(
            &layer_path,
            &[("target.txt", b"target content")],
            &[("link.txt", "target.txt")],
        );

        let rootfs = tmp.path().join("rootfs");
        unpack_layers(&[layer_path], &rootfs).unwrap();

        assert!(rootfs.join("link.txt").is_symlink());
        let content = std::fs::read_to_string(rootfs.join("link.txt")).unwrap();
        assert_eq!(content, "target content");
    }

    // -- Test helpers ----------------------------------------------------------

    fn create_test_layer(path: &Path, files: &[(&str, &[u8])]) {
        create_test_layer_with_dirs(path, &[], files);
    }

    fn create_test_layer_with_dirs(path: &Path, dirs: &[&str], files: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);

        for dir in dirs {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, dir, &[][..]).unwrap();
        }

        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            tar.append_data(&mut header, name, &content[..]).unwrap();
        }

        tar.into_inner().unwrap().finish().unwrap();
    }

    fn create_test_layer_with_symlinks(
        path: &Path,
        files: &[(&str, &[u8])],
        symlinks: &[(&str, &str)],
    ) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);

        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            tar.append_data(&mut header, name, &content[..]).unwrap();
        }

        for (link_name, target) in symlinks {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o777);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_cksum();
            tar.append_link(&mut header, link_name, target).unwrap();
        }

        tar.into_inner().unwrap().finish().unwrap();
    }

    // -- Integration tests (env-gated) -----------------------------------------

    fn image_pull_tests_enabled() -> bool {
        std::env::var("RELIABURGER_IMAGE_PULL_TESTS").is_ok()
    }

    #[tokio::test]
    async fn pull_alpine_creates_rootfs() {
        if !image_pull_tests_enabled() {
            eprintln!("skipping image pull test (set RELIABURGER_IMAGE_PULL_TESTS=1 to enable)");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());

        let rootfs = store.pull_and_unpack("alpine:latest").await.unwrap();
        assert!(rootfs.join("bin/sh").exists());
        assert!(rootfs.join("etc/alpine-release").exists());
    }

    #[tokio::test]
    async fn pull_cached_is_fast() {
        if !image_pull_tests_enabled() {
            eprintln!("skipping image pull test (set RELIABURGER_IMAGE_PULL_TESTS=1 to enable)");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());

        // First pull
        store.pull_and_unpack("alpine:latest").await.unwrap();

        // Second pull should reuse cached blobs
        let start = std::time::Instant::now();
        store.pull_and_unpack("alpine:latest").await.unwrap();
        let elapsed = start.elapsed();

        // Cached pull should be fast (manifest re-fetch + no blob downloads)
        assert!(
            elapsed.as_secs() < 30,
            "cached pull took too long: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn pull_nonexistent_image_errors() {
        if !image_pull_tests_enabled() {
            eprintln!("skipping image pull test (set RELIABURGER_IMAGE_PULL_TESTS=1 to enable)");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());

        let result = store
            .pull_and_unpack("reliaburger-nonexistent-image-for-testing:latest")
            .await;
        assert!(result.is_err());
    }
}
