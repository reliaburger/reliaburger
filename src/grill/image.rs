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

    /// Format as a full reference string. A digest in the tag position
    /// formats as `registry/repository@sha256:…`, which the OCI client
    /// parses as a digest reference.
    pub fn full_reference(&self) -> String {
        if self.tag.starts_with("sha256:") {
            format!("{}/{}@{}", self.registry, self.repository, self.tag)
        } else {
            format!("{}/{}:{}", self.registry, self.repository, self.tag)
        }
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
///
/// A digest-pinned reference (`name@sha256:…`) carries the digest in
/// the tag position — content addressing makes a tag redundant, and
/// downstream code recognises the `sha256:` prefix.
fn split_name_tag(s: &str) -> (&str, String) {
    if let Some((name, digest)) = s.split_once('@') {
        return (name, digest.to_string());
    }

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

/// A cluster-backed layer source consulted before any external
/// registry (Phase 12 C2). Implemented over the Pickle catalog +
/// P2P pulls; injected late because the cluster subsystems start
/// after the runtime is selected.
///
/// `fetch_cluster_image` returns:
/// - `Ok(Some(layer_paths))` — the catalog knows `repository:tag`;
///   all layer blobs are now local, in manifest order, at these paths.
/// - `Ok(None)` — not a cluster image; fall through to the external
///   registry.
/// - `Err(reason)` — the catalog knows the image but its layers could
///   not be materialised. The caller must NOT fall back: the same name
///   on an external registry is a different (wrong) image.
pub trait ClusterImageSource: Send + Sync {
    fn fetch_cluster_image<'a>(
        &'a self,
        repository: &'a str,
        tag: &'a str,
    ) -> ClusterFetchFuture<'a>;

    /// Pull-through cache for external references, consulted after the
    /// cluster candidates miss. `Ok(None)` = cache disabled or
    /// unavailable — fall through to a direct external pull. Unlike
    /// `fetch_cluster_image`, an error here is also a fall-through
    /// (same image identity upstream, so a direct pull is safe).
    fn fetch_pull_through<'a>(&'a self, image: &'a ImageReference) -> ClusterFetchFuture<'a> {
        let _ = image;
        Box::pin(std::future::ready(Ok(None)))
    }
}

/// Boxed future returned by [`ClusterImageSource::fetch_cluster_image`]
/// (the trait must be `dyn`-safe, so no `impl Future` here).
pub type ClusterFetchFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Option<Vec<PathBuf>>, String>> + Send + 'a>,
>;

/// Content-addressed image store on disk.
///
/// Disk layout:
/// ```text
/// {store_root}/
///   blobs/sha256/{digest}                    — raw layer blobs
///   rootfs/{registry}/{repo}/{tag}/          — unpacked filesystem
///   manifests/{registry}/{repo}/{tag}.json   — cached manifests
/// ```
#[derive(Clone)]
pub struct ImageStore {
    store_root: PathBuf,
    /// Set once at startup when clustering is enabled; shared across
    /// clones (the runc grill holds one). A lock-free `OnceLock` read
    /// sits on every pull, a set happens at most once.
    cluster_source: std::sync::Arc<std::sync::OnceLock<std::sync::Arc<dyn ClusterImageSource>>>,
}

impl ImageStore {
    /// Create a new image store at the given root directory.
    pub fn new(store_root: PathBuf) -> Self {
        Self {
            store_root,
            cluster_source: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Install the cluster image source. Called once after the cluster
    /// subsystems start; later calls are ignored (`OnceLock`).
    pub fn set_cluster_source(&self, source: std::sync::Arc<dyn ClusterImageSource>) {
        let _ = self.cluster_source.set(source);
    }

    /// Unpack pre-fetched layer blobs into an immutable, content-addressed
    /// rootfs *generation* (REG5).
    ///
    /// The generation directory is named by a hash of the ordered layer
    /// digests, so two different images (or two tags of the same repo that
    /// point at different content) never share a rootfs. A tag move
    /// therefore extracts into a *fresh* directory and cannot delete or
    /// re-extract another generation while a container is running out of it
    /// — the container keeps the path it was started with. Re-extracting the
    /// same content is idempotent: the target is unique to that content, so
    /// `unpack_layers`'s clear-and-recreate rebuilds an identical tree.
    ///
    /// Tar extraction is CPU-bound, so it runs on a blocking task.
    async fn unpack_to(
        &self,
        layer_paths: Vec<PathBuf>,
        rootfs: PathBuf,
    ) -> Result<PathBuf, ImageError> {
        let generation = self.rootfs_generation_path(&rootfs, &layer_paths);
        let target = generation.clone();
        tokio::task::spawn_blocking(move || unpack_layers(&layer_paths, &target))
            .await
            .map_err(|e| ImageError::UnpackFailed {
                digest: "join".to_string(),
                reason: e.to_string(),
            })??;
        Ok(generation)
    }

    /// The content-addressed rootfs generation directory for a set of
    /// layer blobs (REG5).
    ///
    /// Sits under the tag's rootfs directory, in a `gen-{hash}` subdirectory
    /// keyed by the ordered layer digests. Different content lands in a
    /// different generation, so a concurrent push or a tag move never
    /// clobbers a live container's filesystem.
    pub fn rootfs_generation_path(&self, tag_rootfs: &Path, layer_paths: &[PathBuf]) -> PathBuf {
        let mut hasher = Sha256::new();
        for path in layer_paths {
            // The blob filename is the layer's sha256 hex — immutable
            // content identity. Hash the ordered set into one generation id.
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            hasher.update(name.as_bytes());
            hasher.update(b"\n");
        }
        let generation = hex::encode(hasher.finalize());
        tag_rootfs.join(format!("gen-{}", &generation[..16]))
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

        // Cluster-first: if the Pickle catalog knows this image, its
        // layers arrive from peers and unpack from the pickle blob
        // store. This isn't only a bandwidth win — the external client
        // below speaks HTTPS only, so cluster-pushed (plain-HTTP)
        // images can't be deployed any other way.
        if let Some(source) = self.cluster_source.get() {
            for (repo, tag) in cluster_candidates(&image_ref) {
                match source.fetch_cluster_image(&repo, &tag).await {
                    Ok(Some(layer_paths)) => {
                        return self.unpack_to(layer_paths, rootfs).await;
                    }
                    Ok(None) => continue,
                    // The catalog knows the image but its layers are
                    // unreachable. Do NOT fall through: `web:v1` on an
                    // external registry is a different image.
                    Err(reason) => {
                        return Err(ImageError::LayerPull {
                            digest: format!("{repo}:{tag}"),
                            reason,
                        });
                    }
                }
            }

            // Not a cluster image — try the pull-through cache. Errors
            // fall through to the direct pull: the upstream identity is
            // the same either way, so degrading is safe (and logged).
            match source.fetch_pull_through(&image_ref).await {
                Ok(Some(layer_paths)) => {
                    return self.unpack_to(layer_paths, rootfs).await;
                }
                Ok(None) => {}
                Err(reason) => {
                    eprintln!(
                        "warning: pull-through cache failed for {image}: {reason} — \
                         falling back to a direct pull"
                    );
                }
            }
        }

        // Keep remote registries on HTTPS. Loopback is the one exception:
        // local development registries and the hermetic test fixture do not
        // need a certificate merely to move bytes within the same host.
        let protocol = if image_ref.registry.starts_with("127.0.0.1:")
            || image_ref.registry.starts_with("localhost:")
        {
            oci_distribution::client::ClientProtocol::HttpsExcept(vec![image_ref.registry.clone()])
        } else {
            oci_distribution::client::ClientProtocol::Https
        };

        // The default ClientConfig also includes a platform resolver that
        // picks the current host's architecture from manifest lists.
        let client_config = oci_distribution::client::ClientConfig {
            protocol,
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

        // Unpack layers into an immutable content-addressed generation
        // (REG5), not the shared tag directory — a re-pull after a tag move
        // gets a fresh generation and can't clobber a running container.
        // Tar extraction is CPU-bound, so it runs on a blocking task.
        let layer_paths: Vec<PathBuf> = manifest
            .layers
            .iter()
            .map(|l| self.blob_path(&l.digest))
            .collect();
        self.unpack_to(layer_paths, rootfs).await
    }
}

/// Repository/tag candidates to try against the Pickle catalog for a
/// parsed image reference.
///
/// Parsing normalises `web:v1` to `docker.io/library/web:v1`, but the
/// catalog stores whatever repository the pusher used in the URL path
/// (`/v2/web/manifests/v1` → `web`). So for Docker Hub shorthand we
/// try the bare name first, then the normalised form. Explicit
/// registries can't be pushed to Pickle under that name — no
/// candidates (the pull-through cache handles them separately).
pub fn cluster_candidates(image_ref: &ImageReference) -> Vec<(String, String)> {
    if image_ref.registry != "docker.io" {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    if let Some(bare) = image_ref.repository.strip_prefix("library/") {
        candidates.push((bare.to_string(), image_ref.tag.clone()));
    }
    candidates.push((image_ref.repository.clone(), image_ref.tag.clone()));
    candidates
}

/// Compute the SHA-256 hex digest of some data.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Join `rel` onto `base`, rejecting any path that would escape `base`.
///
/// Only plain path components are allowed; absolute prefixes, root, and
/// parent-dir (`..`) components are refused. Regular tar entries are sanitised
/// by the tar crate's `unpack_in`, but whiteout targets are resolved by hand,
/// so without this a layer entry such as `../../etc/.wh.passwd` would delete
/// host files outside the rootfs.
fn safe_join(base: &Path, rel: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = base.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    out.starts_with(base).then_some(out)
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
                    let Some(target) = safe_join(rootfs, parent) else {
                        eprintln!(
                            "image: skipping opaque whiteout with unsafe path {}",
                            path.display()
                        );
                        continue;
                    };
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
                    // Route the whole relative path through safe_join so neither
                    // the parent nor a crafted `deleted_name` can escape rootfs.
                    let Some(target) = safe_join(rootfs, &parent.join(deleted_name)) else {
                        eprintln!(
                            "image: skipping whiteout with unsafe path {}",
                            path.display()
                        );
                        continue;
                    };
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
    use axum::body::Body;
    use axum::extract::{Request, State};
    use axum::http::{Response, StatusCode, header};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;

    // -- cluster_candidates ------------------------------------------------

    #[test]
    fn cluster_candidates_bare_name_tries_bare_then_normalised() {
        let r = ImageReference::parse("web:v1").unwrap();
        assert_eq!(
            cluster_candidates(&r),
            vec![
                ("web".to_string(), "v1".to_string()),
                ("library/web".to_string(), "v1".to_string()),
            ]
        );
    }

    #[test]
    fn cluster_candidates_user_repo_uses_repository_as_is() {
        let r = ImageReference::parse("team/app:v2").unwrap();
        assert_eq!(
            cluster_candidates(&r),
            vec![("team/app".to_string(), "v2".to_string())]
        );
    }

    #[test]
    fn cluster_candidates_explicit_registry_has_none() {
        let r = ImageReference::parse("ghcr.io/org/app:v1").unwrap();
        assert!(cluster_candidates(&r).is_empty());
    }

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

    /// IMG1: a digest-pinned reference carries the digest in the tag
    /// position and round-trips through the OCI reference parser.
    #[test]
    fn parse_digest_pinned_reference() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let r = ImageReference::parse(&format!("myapp@{digest}")).unwrap();
        assert_eq!(r.registry, "docker.io");
        assert_eq!(r.repository, "library/myapp");
        assert_eq!(r.tag, digest);
        assert_eq!(
            r.full_reference(),
            format!("docker.io/library/myapp@{digest}")
        );
        assert!(r.to_oci_reference().is_ok());
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

    /// REG5: two different layer sets for the same tag land in *different*
    /// content-addressed generation directories, so a tag move or a
    /// concurrent push can't clobber a running container's rootfs. The same
    /// layer set is stable across calls (idempotent re-extract target).
    #[test]
    fn rootfs_generations_are_content_addressed_and_isolated() {
        let store = ImageStore::new(PathBuf::from("/tmp/images"));
        let tag_rootfs = PathBuf::from("/tmp/images/rootfs/docker.io/library/web/v1");

        let gen_a = store.rootfs_generation_path(
            &tag_rootfs,
            &[
                PathBuf::from("/b/sha256/aaaa"),
                PathBuf::from("/b/sha256/bbbb"),
            ],
        );
        let gen_a_again = store.rootfs_generation_path(
            &tag_rootfs,
            &[
                PathBuf::from("/b/sha256/aaaa"),
                PathBuf::from("/b/sha256/bbbb"),
            ],
        );
        let gen_b = store.rootfs_generation_path(
            &tag_rootfs,
            &[
                PathBuf::from("/b/sha256/cccc"),
                PathBuf::from("/b/sha256/dddd"),
            ],
        );

        assert_eq!(gen_a, gen_a_again, "same content must be stable");
        assert_ne!(gen_a, gen_b, "different content must not share a rootfs");
        assert!(gen_a.starts_with(&tag_rootfs));
        assert!(gen_b.starts_with(&tag_rootfs));
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
    fn whiteout_with_parent_traversal_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();

        // A sentinel file that lives *outside* the rootfs, as a sibling.
        let sentinel = tmp.path().join("evil").join("target.txt");
        std::fs::create_dir_all(sentinel.parent().unwrap()).unwrap();
        std::fs::write(&sentinel, b"do not delete me").unwrap();

        // A malicious layer whose whiteout entry tries to climb out of the
        // rootfs (`../evil/.wh.target.txt`) to delete the sentinel.
        let layer = tmp.path().join("evil-layer.tar.gz");
        create_layer_with_raw_name(&layer, b"../evil/.wh.target.txt");

        let rootfs = tmp.path().join("rootfs");
        // Unpacking must succeed (a bad entry is skipped, not fatal)...
        unpack_layers(&[layer], &rootfs).unwrap();
        // ...and the out-of-rootfs sentinel must survive.
        assert!(sentinel.exists(), "traversal whiteout escaped the rootfs");
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

    /// Build a one-entry layer whose name is written verbatim into the tar
    /// header, bypassing the tar crate's own `..` rejection — needed to
    /// simulate a malicious layer that a real registry could serve.
    fn create_layer_with_raw_name(path: &Path, raw_name: &[u8]) {
        let file = std::fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut tar = tar::Builder::new(encoder);

        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        if let Some(gnu) = header.as_gnu_mut() {
            gnu.name[..raw_name.len()].copy_from_slice(raw_name);
        }
        header.set_cksum();
        tar.append(&header, &[][..]).unwrap();

        tar.into_inner().unwrap().finish().unwrap();
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

    // -- Hermetic OCI distribution fixture ------------------------------------

    #[derive(Clone)]
    struct RegistryState {
        manifest_path: String,
        manifest: Vec<u8>,
        config_path: String,
        config: Vec<u8>,
        layer_path: String,
        layer: Vec<u8>,
        layer_requests: Arc<AtomicUsize>,
    }

    struct RegistryFixture {
        reference: String,
        layer_requests: Arc<AtomicUsize>,
        shutdown: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for RegistryFixture {
        fn drop(&mut self) {
            self.shutdown.cancel();
            self.task.abort();
        }
    }

    async fn registry_response(
        State(state): State<RegistryState>,
        request: Request,
    ) -> Response<Body> {
        let path = request.uri().path();
        if path == "/v2/" {
            return Response::builder()
                .status(StatusCode::OK)
                .body(Body::empty())
                .unwrap();
        }

        let (body, content_type) = if path == state.manifest_path {
            (
                state.manifest.clone(),
                "application/vnd.oci.image.manifest.v1+json",
            )
        } else if path == state.config_path {
            (
                state.config.clone(),
                "application/vnd.oci.image.config.v1+json",
            )
        } else if path == state.layer_path {
            state.layer_requests.fetch_add(1, Ordering::SeqCst);
            (
                state.layer.clone(),
                "application/vnd.oci.image.layer.v1.tar+gzip",
            )
        } else {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .unwrap();
        };

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .unwrap()
    }

    async fn start_registry_fixture() -> RegistryFixture {
        let dir = tempfile::tempdir().unwrap();
        let layer_path = dir.path().join("layer.tar.gz");
        create_test_layer_with_dirs(
            &layer_path,
            &["bin/", "etc/"],
            &[("bin/sh", b"fixture shell"), ("etc/os-release", b"fixture")],
        );
        let layer = std::fs::read(layer_path).unwrap();
        let layer_digest = format!("sha256:{}", sha256_hex(&layer));

        let config =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
                .to_vec();
        let config_digest = format!("sha256:{}", sha256_hex(&config));
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config.len(),
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                "digest": layer_digest,
                "size": layer.len(),
            }],
        }))
        .unwrap();
        let manifest_digest = format!("sha256:{}", sha256_hex(&manifest));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let layer_requests = Arc::new(AtomicUsize::new(0));
        let state = RegistryState {
            manifest_path: format!("/v2/fixture/manifests/{manifest_digest}"),
            manifest,
            config_path: format!("/v2/fixture/blobs/{config_digest}"),
            config,
            layer_path: format!("/v2/fixture/blobs/{layer_digest}"),
            layer,
            layer_requests: Arc::clone(&layer_requests),
        };
        let app = axum::Router::new()
            .fallback(registry_response)
            .with_state(state);
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(server_shutdown.cancelled_owned())
                .await
                .unwrap();
        });

        RegistryFixture {
            reference: format!("{address}/fixture@{manifest_digest}"),
            layer_requests,
            shutdown,
            task,
        }
    }

    #[tokio::test]
    async fn digest_pinned_local_registry_pull_creates_rootfs() {
        let fixture = start_registry_fixture().await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());

        let rootfs = store.pull_and_unpack(&fixture.reference).await.unwrap();
        assert!(rootfs.join("bin/sh").exists());
        assert!(rootfs.join("etc/os-release").exists());
    }

    #[tokio::test]
    async fn repeated_pull_reuses_the_content_addressed_layer() {
        let fixture = start_registry_fixture().await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());

        store.pull_and_unpack(&fixture.reference).await.unwrap();
        store.pull_and_unpack(&fixture.reference).await.unwrap();

        assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn local_registry_missing_manifest_is_an_error() {
        let fixture = start_registry_fixture().await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());
        let registry = fixture.reference.split('/').next().unwrap();
        let missing = format!("{registry}/missing@sha256:{}", "0".repeat(64));

        let result = store.pull_and_unpack(&missing).await;
        assert!(result.is_err());
    }
}
