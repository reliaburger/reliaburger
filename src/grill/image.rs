/// OCI image pulling and unpacking.
///
/// Pulls container images from OCI-compliant registries (Docker Hub,
/// GHCR, etc.) using the OCI Distribution API. Layers are cached as
/// content-addressed blobs and unpacked into a rootfs directory that
/// runc can use directly.
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use std::collections::BTreeMap;

use oci_distribution::manifest::OciDescriptor;

use super::oci_pull::retry_registry_read;

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

    #[error("invalid image config {digest}: {reason}")]
    InvalidConfig { digest: String, reason: String },

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

/// Whether plain HTTP may reach `registry`. Remote registries stay on HTTPS;
/// local development registries and test fixtures on the same host don't need
/// a certificate merely to move bytes that are verified by digest anyway.
pub(crate) fn is_loopback_registry(registry: &str) -> bool {
    registry.starts_with("127.0.0.1:") || registry.starts_with("localhost:")
}

/// Registries that serve digest-pinned images on behalf of an upstream host,
/// e.g. `public.ecr.aws` → `mirror.internal:5000`.
///
/// A digest names exact bytes and every pull verifies the whole digest chain,
/// so a mirror can make a pull faster or fail it, but can never substitute
/// content. Tag references always go to their own registry: a mirror could
/// answer a mutable tag with a different image.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    try_from = "BTreeMap<String, String>",
    into = "BTreeMap<String, String>"
)]
pub struct ImageMirrors(BTreeMap<String, String>);

impl TryFrom<BTreeMap<String, String>> for ImageMirrors {
    type Error = ImageError;

    fn try_from(mirrors: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        Self::new(mirrors)
    }
}

impl From<ImageMirrors> for BTreeMap<String, String> {
    fn from(mirrors: ImageMirrors) -> Self {
        mirrors.0
    }
}

impl ImageMirrors {
    /// Validate `upstream host → mirror host[:port]` pairs.
    pub fn new(mirrors: BTreeMap<String, String>) -> Result<Self, ImageError> {
        for (upstream, mirror) in &mirrors {
            for host in [upstream, mirror] {
                let valid = !host.is_empty()
                    && !host.contains(['/', '@', '?', '#'])
                    && !host.chars().any(char::is_whitespace);
                if !valid {
                    return Err(ImageError::InvalidReference(format!(
                        "image mirror {upstream:?} = {mirror:?} must map one registry \
                         host[:port] to another, without a scheme or path"
                    )));
                }
            }
        }
        Ok(Self(mirrors))
    }

    /// The same repository and digest on `image`'s mirror, if it has one
    /// and `image` is pinned by digest.
    pub fn mirror_for(&self, image: &ImageReference) -> Option<ImageReference> {
        if !image.tag.starts_with("sha256:") {
            return None;
        }
        let mirror = self.0.get(&image.registry)?;
        Some(ImageReference {
            registry: mirror.clone(),
            repository: image.repository.clone(),
            tag: image.tag.clone(),
        })
    }

    /// Mirrors reached over plain HTTP (see [`is_loopback_registry`]).
    pub(crate) fn loopback_hosts(&self) -> Vec<String> {
        self.0
            .values()
            .filter(|mirror| is_loopback_registry(mirror))
            .cloned()
            .collect()
    }

    /// The configured `upstream → mirror` pairs.
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.0
    }
}

/// An image's blobs, materialised in local storage.
#[derive(Debug, Clone)]
pub struct LocalImageBlobs {
    /// Layer blobs, in manifest order (base first).
    pub layers: Vec<PathBuf>,
    /// The config blob.
    pub config: PathBuf,
    /// The config blob's digest, as the manifest names it.
    pub config_digest: String,
}

/// An unpacked image: its root filesystem and the config that says how
/// to run it.
#[derive(Debug, Clone)]
pub struct PulledImage {
    /// The shared, read-only rootfs generation.
    pub rootfs: PathBuf,
    /// The image's `Entrypoint`, `Cmd`, `Env`, `WorkingDir` and `User`,
    /// parsed from the digest-verified config blob.
    pub config: super::image_config::ImageConfig,
}

/// A cluster-backed layer source consulted before any external
/// registry (Phase 12 C2). Implemented over the Pickle catalog +
/// P2P pulls; injected late because the cluster subsystems start
/// after the runtime is selected.
///
/// `fetch_cluster_image` returns:
/// - `Ok(Some(blobs))` — the catalog knows `repository:tag`; its
///   config and layer blobs are now local and digest-verified.
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
    Box<dyn std::future::Future<Output = Result<Option<LocalImageBlobs>, String>> + Send + 'a>,
>;

/// Content-addressed image store on disk.
///
/// Disk layout:
/// ```text
/// {store_root}/
///   blobs/sha256/{digest}/data               — raw layer blobs
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
    /// Serialises generation publication across clones. Re-unpacking the same
    /// generation clears its directory, which can destroy a running
    /// container's rootfs; the completion marker makes subsequent pulls reuse
    /// the published tree instead.
    unpack_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// Added to every file's uid and gid at unpack time, so a user
    /// namespace mapping container id 0 to this host id sees the image's
    /// own ownership (see `grill::userns`). `None` keeps the unpacking
    /// user as owner.
    owner_shift: Option<u32>,
    /// Registries tried before the origin for digest-pinned images.
    mirrors: ImageMirrors,
}

/// The path of a blob in the storage shared by the registry and the runtime.
pub(crate) fn cached_blob_path(root: &Path, digest: &str) -> PathBuf {
    root.join("blobs").join("sha256").join(digest).join("data")
}

impl ImageStore {
    /// Create a new image store at the given root directory.
    pub fn new(store_root: PathBuf) -> Self {
        Self {
            store_root,
            cluster_source: std::sync::Arc::new(std::sync::OnceLock::new()),
            unpack_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            owner_shift: None,
            mirrors: ImageMirrors::default(),
        }
    }

    /// Try `mirrors` before an image's own registry for digest-pinned pulls.
    pub fn with_mirrors(mut self, mirrors: ImageMirrors) -> Self {
        self.mirrors = mirrors;
        self
    }

    /// Unpack images with each file owned by `base` plus its uid and gid
    /// in the layer, for containers in a user namespace mapping container
    /// id 0 to host id `base`. Needs root.
    pub fn with_owner_shift(mut self, base: u32) -> Self {
        self.owner_shift = Some(base);
        self
    }

    /// Directory containing this runtime's selected image storage.
    #[cfg(target_os = "linux")]
    pub(crate) fn storage_directory(&self) -> &Path {
        &self.store_root
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
    /// — the container keeps the path it was started with. The same content is
    /// published once and then reused; clearing and rebuilding an already-live
    /// generation would briefly remove files from running containers.
    ///
    /// Tar extraction is CPU-bound, so it runs on a blocking task.
    async fn unpack_to(
        &self,
        layer_paths: Vec<PathBuf>,
        rootfs: PathBuf,
    ) -> Result<PathBuf, ImageError> {
        let _guard = self.unpack_lock.lock().await;
        let generation = self.rootfs_generation_path(&rootfs, &layer_paths);
        let complete = generation.with_extension("complete");
        if complete.is_file() {
            return Ok(generation);
        }
        let target = generation.clone();
        let owner_shift = self.owner_shift;
        tokio::task::spawn_blocking(move || {
            unpack_layers_with_owner(&layer_paths, &target, owner_shift)?;
            // The marker is the only thing a later pull checks. Flush the
            // unpacked tree before writing it: after a power cut the V02 soak
            // found `.complete` present and `redis-server` empty, and every
            // redeploy failed with "exec format error".
            sync_filesystem(&target)?;
            crate::sesame::identity::atomic_write(&complete, b"complete\n")?;
            Ok::<(), ImageError>(())
        })
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
            // Every blob sits at `{digest}/data`, so the parent directory's
            // name is the layer's sha256 hex — immutable content identity.
            // Hash the ordered set into one generation id.
            let name = path
                .parent()
                .and_then(Path::file_name)
                .unwrap_or_default()
                .to_string_lossy();
            hasher.update(name.as_bytes());
            hasher.update(b"\n");
        }
        let generation = hex::encode(hasher.finalize());
        // Shifted and unshifted trees of the same layers differ on disk.
        match self.owner_shift {
            Some(base) => tag_rootfs.join(format!("gen-{}-owner-{base}", &generation[..16])),
            None => tag_rootfs.join(format!("gen-{}", &generation[..16])),
        }
    }

    /// Path to a cached blob by its SHA-256 digest.
    pub fn blob_path(&self, digest: &str) -> PathBuf {
        // digest is typically "sha256:abcdef..." — strip the algorithm prefix
        let hash = digest.strip_prefix("sha256:").unwrap_or(digest);
        cached_blob_path(&self.store_root, hash)
    }

    /// Path to the unpacked rootfs for an image reference.
    pub fn rootfs_path(&self, image_ref: &ImageReference) -> PathBuf {
        // A colon separates overlayfs lower layers. Digest pins and registry
        // ports contain it, so encode it before this path reaches a mount.
        self.store_root
            .join("rootfs")
            .join(image_ref.registry.replace(':', "%3A"))
            .join(&image_ref.repository)
            .join(image_ref.tag.replace(':', "%3A"))
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
    /// Returns the unpacked rootfs and the image's config. Caches blobs and
    /// manifests on disk; subsequent pulls of the same image are fast.
    pub async fn pull_and_unpack(&self, image: &str) -> Result<PulledImage, ImageError> {
        let image_ref = ImageReference::parse(image)?;
        // Refuse a reference the registry client can't express before any
        // cluster or cache lookup acts on it.
        image_ref.to_oci_reference()?;

        let rootfs = self.rootfs_path(&image_ref);

        // Cluster-first: if the Pickle catalog knows this image, its
        // layers arrive from peers and unpack from the pickle blob
        // store. This isn't only a bandwidth win — the external client
        // below speaks HTTPS only, so cluster-pushed (plain-HTTP)
        // images can't be deployed any other way.
        if let Some(source) = self.cluster_source.get() {
            for (repo, tag) in cluster_candidates(&image_ref) {
                match source.fetch_cluster_image(&repo, &tag).await {
                    Ok(Some(blobs)) => {
                        return self.unpack_local(blobs, rootfs).await;
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
                Ok(Some(blobs)) => {
                    return self.unpack_local(blobs, rootfs).await;
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

        // A digest-pinned image may come from a configured mirror first. The
        // digest chain is verified either way, so a failing or dishonest
        // mirror costs time, never integrity.
        let (layers, config) = match self.mirrors.mirror_for(&image_ref) {
            Some(mirror) => match self.fetch_external(&image_ref, &mirror).await {
                Ok(fetched) => fetched,
                Err(reason) => {
                    eprintln!(
                        "warning: mirror {} failed for {image}: {reason} — \
                         falling back to {}",
                        mirror.registry, image_ref.registry
                    );
                    self.fetch_external(&image_ref, &image_ref).await?
                }
            },
            None => self.fetch_external(&image_ref, &image_ref).await?,
        };
        // Unpack layers into an immutable content-addressed generation
        // (REG5), not the shared tag directory — a re-pull after a tag move
        // gets a fresh generation and can't clobber a running container.
        // Tar extraction is CPU-bound, so it runs on a blocking task.
        let layer_paths: Vec<PathBuf> = layers.iter().map(|l| self.blob_path(&l.digest)).collect();
        let rootfs = self.unpack_to(layer_paths, rootfs).await?;
        Ok(PulledImage { rootfs, config })
    }

    /// Fetch `image`'s verified manifest, config and layer blobs from
    /// `source`, which is either the image's own registry or its mirror.
    /// Cache entries stay keyed by `image`, so both sources fill one cache.
    async fn fetch_external(
        &self,
        image_ref: &ImageReference,
        source: &ImageReference,
    ) -> Result<(Vec<OciDescriptor>, super::image_config::ImageConfig), ImageError> {
        // Keep remote registries on HTTPS. Loopback is the one exception:
        // local development registries and the hermetic test fixture do not
        // need a certificate merely to move bytes within the same host.
        let protocol = if is_loopback_registry(&source.registry) {
            oci_distribution::client::ClientProtocol::HttpsExcept(vec![source.registry.clone()])
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

        let oci_ref = source.to_oci_reference()?;

        // Verify the raw digest chain before publishing any cache metadata.
        let verified = retry_registry_read(super::oci_pull::METADATA_READ, || {
            super::oci_pull::pull_verified_manifest(&client, &oci_ref, &auth)
        })
        .await
        .map_err(|e| ImageError::ManifestPull {
            image: source.full_reference(),
            reason: e.to_string(),
        })?;

        let manifest = verified.manifest;
        let config = parse_config(&verified.config_bytes, &manifest.config.digest)?;

        // Save the manifest for cache validation
        let manifest_path = self.manifest_path(image_ref);
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
            let expected_size = u64::try_from(layer.size).map_err(|_| ImageError::LayerPull {
                digest: digest.clone(),
                reason: "negative layer size".into(),
            })?;
            let verify_size = |actual: u64| -> Result<(), ImageError> {
                if actual != expected_size {
                    return Err(ImageError::LayerPull {
                        digest: digest.clone(),
                        reason: format!(
                            "layer size mismatch: expected {expected_size}, received {actual}"
                        ),
                    });
                }
                Ok(())
            };
            if blob_path.exists() {
                verify_size(tokio::fs::metadata(&blob_path).await?.len())?;
                continue;
            }

            if let Some(parent) = blob_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            let blob_data = retry_registry_read(super::oci_pull::LAYER_READ, || async {
                // A failed transfer may have written a prefix. Each attempt
                // owns a fresh buffer; no partial bytes reach the cache.
                let mut blob_data = Vec::new();
                client.pull_blob(&oci_ref, layer, &mut blob_data).await?;
                Ok(blob_data)
            })
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

            verify_size(blob_data.len() as u64)?;

            // Write atomically (temp + rename) so a crash mid-write can't leave
            // a truncated blob at the final path that a later pull treats as a
            // valid cache hit (M3) — the `exists()` check above never
            // re-verifies a cached file. The digest was verified above, so a
            // completed rename only ever publishes a good blob.
            // The temp file is synced before the rename and the directory
            // after it, so a power cut can't publish an empty blob either.
            let published = blob_path.clone();
            tokio::task::spawn_blocking(move || {
                crate::sesame::identity::atomic_write(&published, &blob_data)
            })
            .await
            .map_err(|e| ImageError::UnpackFailed {
                digest: digest.clone(),
                reason: e.to_string(),
            })??;
        }
        Ok((manifest.layers, config))
    }

    /// Unpack blobs the cluster already holds, re-checking the config
    /// blob's digest: it decides what the container runs, and as whom.
    async fn unpack_local(
        &self,
        blobs: LocalImageBlobs,
        rootfs: PathBuf,
    ) -> Result<PulledImage, ImageError> {
        let bytes = tokio::fs::read(&blobs.config).await?;
        let actual = format!("sha256:{}", sha256_hex(&bytes));
        if actual != blobs.config_digest {
            return Err(ImageError::DigestMismatch {
                digest: blobs.config_digest.clone(),
                expected: blobs.config_digest,
                actual,
            });
        }
        let config = parse_config(&bytes, &blobs.config_digest)?;
        let rootfs = self.unpack_to(blobs.layers, rootfs).await?;
        Ok(PulledImage { rootfs, config })
    }
}

/// Flush every dirty page of the filesystem holding `path` to disk.
#[cfg(target_os = "linux")]
fn sync_filesystem(path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    let directory = std::fs::File::open(path)?;
    // `directory` stays open for the whole call, so the descriptor is valid.
    nix::unistd::syncfs(directory.as_raw_fd()).map_err(std::io::Error::from)
}

/// Flush every dirty page to disk. macOS has no per-filesystem sync.
#[cfg(not(target_os = "linux"))]
fn sync_filesystem(_path: &Path) -> std::io::Result<()> {
    nix::unistd::sync();
    Ok(())
}

/// Parse a digest-verified config blob.
fn parse_config(
    bytes: &[u8],
    digest: &str,
) -> Result<super::image_config::ImageConfig, ImageError> {
    super::image_config::ImageConfig::from_json(bytes).map_err(|e| ImageError::InvalidConfig {
        digest: digest.to_string(),
        reason: e.to_string(),
    })
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
    unpack_layers_with_owner(layer_paths, rootfs, None)
}

/// [`unpack_layers`], optionally shifting every entry's owner by
/// `owner_shift` (the layer's uid 0 becomes host uid `owner_shift`).
pub fn unpack_layers_with_owner(
    layer_paths: &[PathBuf],
    rootfs: &Path,
    owner_shift: Option<u32>,
) -> Result<(), ImageError> {
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
            let unpacked = entry
                .unpack_in(rootfs)
                .map_err(|e| ImageError::UnpackFailed {
                    digest: digest.clone(),
                    reason: format!("failed to unpack {}: {e}", path.display()),
                })?;
            if let Some(base) = owner_shift
                && unpacked
            {
                shift_owner(&entry, rootfs, &path, base).map_err(|reason| {
                    ImageError::UnpackFailed {
                        digest: digest.clone(),
                        reason: format!("failed to set owner of {}: {reason}", path.display()),
                    }
                })?;
            }
        }
    }

    if let Some(base) = owner_shift {
        own_implicit_directories(rootfs, base).map_err(|e| ImageError::UnpackFailed {
            digest: "rootfs".to_string(),
            reason: format!("failed to set owner of implicit directories: {e}"),
        })?;
    }
    Ok(())
}

/// Hand everything the unpacker created on its own (the rootfs itself, and
/// parent directories a layer never listed) to container root.
///
/// Every listed entry already has an owner at or above `base`, so anything
/// below it was created by us, as host root.
fn own_implicit_directories(rootfs: &Path, base: u32) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let mut pending = vec![rootfs.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.uid() < base || metadata.gid() < base {
            std::os::unix::fs::lchown(&path, Some(base), Some(base))?;
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

/// Give an unpacked entry its layer owner, shifted into the node's
/// container id range.
///
/// `chown` clears set-id bits on regular files, so the mode is restored
/// afterwards. Symlinks are re-owned without following them.
fn shift_owner<R: std::io::Read>(
    entry: &tar::Entry<'_, R>,
    rootfs: &Path,
    path: &Path,
    base: u32,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    let header = entry.header();
    let shifted = |id: u64| -> Result<u32, String> {
        u32::try_from(id)
            .ok()
            .filter(|id| *id < super::userns::CONTAINER_ID_COUNT)
            .map(|id| base + id)
            .ok_or_else(|| format!("owner id {id} is outside the container range"))
    };
    let uid = shifted(header.uid().map_err(|e| e.to_string())?)?;
    let gid = shifted(header.gid().map_err(|e| e.to_string())?)?;
    let target = safe_join(rootfs, path).ok_or("unsafe path")?;
    std::os::unix::fs::lchown(&target, Some(uid), Some(gid)).map_err(|e| e.to_string())?;
    if header.entry_type() != tar::EntryType::Symlink {
        let mode = header.mode().map_err(|e| e.to_string())?;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode & 0o7777))
            .map_err(|e| e.to_string())?;
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
    fn registry_and_runtime_share_blobs() {
        let root = tempfile::tempdir().unwrap();
        let image = ImageStore::new(root.path().to_path_buf());
        let registry = crate::pickle::store::BlobStore::new(root.path());
        let bytes = b"shared layer";
        let digest = crate::pickle::store::compute_sha256(bytes);
        registry.write_blob(bytes, &digest).unwrap();
        assert_eq!(
            std::fs::read(image.blob_path(digest.as_str())).unwrap(),
            bytes
        );
        assert_eq!(
            image.blob_path(digest.as_str()),
            registry.blob_path(&digest)
        );
    }

    #[test]
    fn registry_layer_generations_use_digest_instead_of_the_data_filename() {
        let store = ImageStore::new(PathBuf::from("/tmp/images"));
        let root = Path::new("/rootfs/tag");
        let first = store.rootfs_generation_path(root, &[PathBuf::from("/blobs/aaaa/data")]);
        let second = store.rootfs_generation_path(root, &[PathBuf::from("/blobs/bbbb/data")]);
        assert_ne!(first, second);
    }

    #[test]
    fn digest_pinned_rootfs_paths_are_safe_for_overlayfs() {
        let store = ImageStore::new(PathBuf::from("/var/lib/reliaburger/images"));
        let reference =
            ImageReference::parse(&format!("localhost:5000/demo@sha256:{}", "a".repeat(64)))
                .unwrap();
        let path = store.rootfs_path(&reference);
        assert!(!path.to_string_lossy().contains(':'));
        assert!(path.to_string_lossy().contains("localhost%3A5000"));
        assert!(path.to_string_lossy().contains("sha256%3A"));
    }

    #[test]
    fn blob_path_from_digest() {
        let store = ImageStore::new(PathBuf::from("/tmp/images"));
        let path = store.blob_path("sha256:abc123");
        assert_eq!(path, PathBuf::from("/tmp/images/blobs/sha256/abc123/data"));
    }

    #[test]
    fn blob_path_without_prefix() {
        let store = ImageStore::new(PathBuf::from("/tmp/images"));
        let path = store.blob_path("abc123");
        assert_eq!(path, PathBuf::from("/tmp/images/blobs/sha256/abc123/data"));
    }

    /// REG5: two different layer sets for the same tag land in *different*
    /// content-addressed generation directories, so a tag move or a
    /// concurrent push can't clobber a running container's rootfs. The same
    /// layer set is stable across calls and reused after publication.
    #[test]
    fn rootfs_generations_are_content_addressed_and_isolated() {
        let store = ImageStore::new(PathBuf::from("/tmp/images"));
        let tag_rootfs = PathBuf::from("/tmp/images/rootfs/docker.io/library/web/v1");

        let gen_a = store.rootfs_generation_path(
            &tag_rootfs,
            &[
                PathBuf::from("/b/sha256/aaaa/data"),
                PathBuf::from("/b/sha256/bbbb/data"),
            ],
        );
        let gen_a_again = store.rootfs_generation_path(
            &tag_rootfs,
            &[
                PathBuf::from("/b/sha256/aaaa/data"),
                PathBuf::from("/b/sha256/bbbb/data"),
            ],
        );
        let gen_b = store.rootfs_generation_path(
            &tag_rootfs,
            &[
                PathBuf::from("/b/sha256/cccc/data"),
                PathBuf::from("/b/sha256/dddd/data"),
            ],
        );

        assert_eq!(gen_a, gen_a_again, "same content must be stable");
        assert_ne!(gen_a, gen_b, "different content must not share a rootfs");
        assert!(gen_a.starts_with(&tag_rootfs));
        assert!(gen_b.starts_with(&tag_rootfs));
    }

    #[tokio::test]
    async fn repeated_unpack_reuses_live_generation_without_clearing_it() {
        let tmp = tempfile::tempdir().unwrap();
        let layer = tmp.path().join("layer.tar.gz");
        create_test_layer(&layer, &[("bin/tool", b"original")]);
        let store = ImageStore::new(tmp.path().join("store"));
        let tag_root = tmp.path().join("rootfs/tag");

        let generation = store
            .unpack_to(vec![layer.clone()], tag_root.clone())
            .await
            .unwrap();
        std::fs::write(generation.join("live-sentinel"), b"still mounted").unwrap();

        let same = store.unpack_to(vec![layer], tag_root).await.unwrap();
        assert_eq!(same, generation);
        assert!(
            generation.join("live-sentinel").exists(),
            "a repeated pull must not clear a generation used by a running container"
        );
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

    /// D1: image ownership survives into the node's container id range, so
    /// `redis` in the image owns `/data` inside the user namespace too.
    #[test]
    #[ignore = "requires root to chown into the container id range"]
    fn owner_shift_maps_layer_owners_into_the_container_range() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        assert!(nix::unistd::geteuid().is_root(), "run as root");
        let base = crate::grill::userns::HOST_ID_BASE;
        let tmp = tempfile::tempdir().unwrap();
        let layer = tmp.path().join("layer.tar.gz");
        {
            let file = std::fs::File::create(&layer).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut tar = tar::Builder::new(encoder);
            let mut data = tar::Header::new_gnu();
            data.set_entry_type(tar::EntryType::Directory);
            data.set_size(0);
            data.set_mode(0o750);
            data.set_uid(999);
            data.set_gid(1000);
            data.set_cksum();
            tar.append_data(&mut data, "data/", &[][..]).unwrap();
            let mut tool = tar::Header::new_gnu();
            tool.set_entry_type(tar::EntryType::Regular);
            tool.set_size(2);
            tool.set_mode(0o4755);
            tool.set_uid(0);
            tool.set_gid(0);
            tool.set_cksum();
            // No `usr/` or `usr/bin/` entries: the unpacker makes them.
            tar.append_data(&mut tool, "usr/bin/tool", &b"#!"[..])
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let rootfs = tmp.path().join("rootfs");
        unpack_layers_with_owner(&[layer], &rootfs, Some(base)).unwrap();

        let data = std::fs::metadata(rootfs.join("data")).unwrap();
        assert_eq!((data.uid(), data.gid()), (base + 999, base + 1000));
        assert_eq!(data.permissions().mode() & 0o7777, 0o750);
        let tool = std::fs::metadata(rootfs.join("usr/bin/tool")).unwrap();
        assert_eq!((tool.uid(), tool.gid()), (base, base));
        assert_eq!(
            tool.permissions().mode() & 0o7777,
            0o4755,
            "chown must not strip the set-id bit"
        );
        for implicit in ["", "usr", "usr/bin"] {
            let meta = std::fs::metadata(rootfs.join(implicit)).unwrap();
            assert_eq!((meta.uid(), meta.gid()), (base, base), "{implicit:?}");
        }
    }

    #[test]
    fn owner_shift_gets_its_own_rootfs_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = ImageStore::new(tmp.path().to_path_buf());
        let shifted = ImageStore::new(tmp.path().to_path_buf()).with_owner_shift(2_000_000_000);
        let layers = [PathBuf::from("/blobs/aaaa/data")];
        let root = Path::new("/rootfs/tag");
        assert_ne!(
            plain.rootfs_generation_path(root, &layers),
            shifted.rootfs_generation_path(root, &layers)
        );
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

    #[derive(Clone, Copy)]
    enum RegistryFaultTarget {
        Manifest,
        Configuration,
        Layer,
    }

    #[derive(Clone)]
    struct RegistryFault {
        target: RegistryFaultTarget,
        disconnect: bool,
        status: StatusCode,
        code: &'static str,
        remaining: Arc<AtomicUsize>,
        delay: std::time::Duration,
        received: Arc<tokio::sync::Notify>,
    }

    fn registry_fault(
        layer: bool,
        status: StatusCode,
        code: &'static str,
        count: usize,
    ) -> RegistryFault {
        RegistryFault {
            target: if layer {
                RegistryFaultTarget::Layer
            } else {
                RegistryFaultTarget::Manifest
            },
            disconnect: false,
            status,
            code,
            remaining: Arc::new(AtomicUsize::new(count)),
            delay: std::time::Duration::ZERO,
            received: Arc::new(tokio::sync::Notify::new()),
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum RegistryIntegrityCase {
        ChangedManifest,
        ChangedConfiguration,
        ChangedIndex,
        ChangedChild,
        ValidIndex,
        Amd64Index,
        WrongConfigurationSize,
        WrongChildSize,
        NegativeLayerSize,
        OverflowingLayerSizes,
        WrongLayerSize,
    }

    #[derive(Clone)]
    struct RegistryState {
        index: Option<(String, Vec<u8>)>,
        manifest_path: String,
        manifest: Vec<u8>,
        config_path: String,
        config: Vec<u8>,
        layer_path: String,
        layer: Vec<u8>,
        layer_requests: Arc<AtomicUsize>,
        manifest_requests: Arc<AtomicUsize>,
        fault: Option<RegistryFault>,
    }

    struct RegistryFixture {
        reference: String,
        layer_requests: Arc<AtomicUsize>,
        manifest_requests: Arc<AtomicUsize>,
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

        let is_index = state
            .index
            .as_ref()
            .is_some_and(|(index_path, _)| index_path == path);
        if path == state.manifest_path || is_index {
            state.manifest_requests.fetch_add(1, Ordering::SeqCst);
        }
        if path == state.layer_path {
            state.layer_requests.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(fault) = &state.fault
            && path
                == match fault.target {
                    RegistryFaultTarget::Manifest => &state.manifest_path,
                    RegistryFaultTarget::Configuration => &state.config_path,
                    RegistryFaultTarget::Layer => &state.layer_path,
                }
            && fault
                .remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
        {
            fault.received.notify_one();
            tokio::time::sleep(fault.delay).await;
            if fault.disconnect {
                use futures_util::StreamExt;
                let prefix = futures_util::stream::once(async {
                    Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"partial response"))
                });
                let failure = futures_util::stream::once(async {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    Err::<axum::body::Bytes, _>(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "injected disconnect",
                    ))
                });
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_LENGTH, 1000)
                    .body(Body::from_stream(prefix.chain(failure)))
                    .unwrap();
            }
            return Response::builder()
                .status(fault.status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({"errors": [{"code": fault.code, "message": "injected registry failure"}]}).to_string()))
                .unwrap();
        }

        let (body, content_type) = if is_index {
            (
                state.index.as_ref().unwrap().1.clone(),
                "application/vnd.oci.image.index.v1+json",
            )
        } else if path == state.manifest_path {
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

        let mut response = Response::builder().status(StatusCode::OK);
        if path == state.manifest_path || is_index {
            // Deliberately claim the requested digest even for changed bytes.
            // A client must hash the response instead of trusting this header.
            response = response.header("Docker-Content-Digest", path.rsplit('/').next().unwrap());
        }
        response
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .unwrap()
    }

    async fn start_registry_fixture() -> RegistryFixture {
        start_registry_fixture_with_fault(None).await
    }

    async fn start_registry_fixture_with_fault(fault: Option<RegistryFault>) -> RegistryFixture {
        start_registry_fixture_with_options(fault, None).await
    }

    async fn start_registry_fixture_with_options(
        fault: Option<RegistryFault>,
        integrity: Option<RegistryIntegrityCase>,
    ) -> RegistryFixture {
        let dir = tempfile::tempdir().unwrap();
        let layer_path = dir.path().join("layer.tar.gz");
        create_test_layer_with_dirs(
            &layer_path,
            &["bin/", "etc/"],
            &[("bin/sh", b"fixture shell"), ("etc/os-release", b"fixture")],
        );
        let layer = std::fs::read(layer_path).unwrap();
        let layer_digest = format!("sha256:{}", sha256_hex(&layer));

        let config = br#"{"architecture":"amd64","os":"linux","config":{"Entrypoint":["/bin/sh"],"Cmd":["-c","true"],"Env":["FIXTURE=1"],"WorkingDir":"/etc"},"rootfs":{"type":"layers","diff_ids":[]}}"#
            .to_vec();
        let config_digest = format!("sha256:{}", sha256_hex(&config));
        let mut manifest = serde_json::to_vec(&serde_json::json!({
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
        if matches!(
            integrity,
            Some(RegistryIntegrityCase::WrongConfigurationSize)
        ) {
            let mut value: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
            value["config"]["size"] = serde_json::json!(config.len() + 1);
            manifest = serde_json::to_vec(&value).unwrap();
        }
        if matches!(
            integrity,
            Some(
                RegistryIntegrityCase::NegativeLayerSize
                    | RegistryIntegrityCase::OverflowingLayerSizes
                    | RegistryIntegrityCase::WrongLayerSize
            )
        ) {
            let mut value: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
            match integrity.unwrap() {
                RegistryIntegrityCase::NegativeLayerSize => {
                    value["layers"][0]["size"] = serde_json::json!(-1)
                }
                RegistryIntegrityCase::WrongLayerSize => {
                    value["layers"][0]["size"] = serde_json::json!(layer.len() + 1)
                }
                _ => {
                    value["layers"][0]["size"] = serde_json::json!(i64::MAX);
                    let layer = value["layers"][0].clone();
                    value["layers"] = serde_json::json!([layer, layer, layer]);
                }
            }
            manifest = serde_json::to_vec(&value).unwrap();
        }
        let manifest_digest = format!("sha256:{}", sha256_hex(&manifest));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let layer_requests = Arc::new(AtomicUsize::new(0));
        let manifest_requests = Arc::new(AtomicUsize::new(0));
        let mut state = RegistryState {
            index: None,
            manifest_path: format!("/v2/fixture/manifests/{manifest_digest}"),
            manifest,
            config_path: format!("/v2/fixture/blobs/{config_digest}"),
            config,
            layer_path: format!("/v2/fixture/blobs/{layer_digest}"),
            layer,
            layer_requests: Arc::clone(&layer_requests),
            manifest_requests: Arc::clone(&manifest_requests),
            fault,
        };
        let mut root_digest = manifest_digest.clone();
        if matches!(
            integrity,
            Some(
                RegistryIntegrityCase::ChangedIndex
                    | RegistryIntegrityCase::ChangedChild
                    | RegistryIntegrityCase::ValidIndex
                    | RegistryIntegrityCase::Amd64Index
                    | RegistryIntegrityCase::WrongChildSize
            )
        ) {
            let mut entries = Vec::new();
            for architecture in ["amd64", "arm64"] {
                if matches!(integrity, Some(RegistryIntegrityCase::Amd64Index))
                    && architecture != "amd64"
                {
                    continue;
                }
                entries.push(serde_json::json!({
                        "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "digest": manifest_digest,
                        "size": state.manifest.len() + usize::from(matches!(integrity, Some(RegistryIntegrityCase::WrongChildSize))),
                        "platform": {"os": "linux", "architecture": architecture}
                    }));
            }
            let index = serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": entries
            }))
            .unwrap();
            root_digest = format!("sha256:{}", sha256_hex(&index));
            state.index = Some((format!("/v2/fixture/manifests/{root_digest}"), index));
        }
        let change_manifest = |bytes: &mut Vec<u8>| {
            let mut manifest: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            manifest["annotations"] =
                serde_json::json!({"fixture": "changed without changing the requested digest"});
            *bytes = serde_json::to_vec(&manifest).unwrap();
        };
        match integrity {
            Some(RegistryIntegrityCase::ChangedManifest | RegistryIntegrityCase::ChangedChild) => {
                change_manifest(&mut state.manifest)
            }
            Some(RegistryIntegrityCase::ChangedConfiguration) => state.config = b"{}".to_vec(),
            Some(RegistryIntegrityCase::ChangedIndex) => {
                change_manifest(&mut state.index.as_mut().unwrap().1)
            }
            _ => {}
        }
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
            reference: format!("{address}/fixture@{root_digest}"),
            layer_requests,
            manifest_requests,
            shutdown,
            task,
        }
    }

    #[tokio::test]
    async fn upstream_layer_sizes_refuse_negative_and_overflowing_totals() {
        use crate::pickle::upstream::UpstreamRegistry;
        for case in [
            RegistryIntegrityCase::NegativeLayerSize,
            RegistryIntegrityCase::OverflowingLayerSizes,
        ] {
            let fixture = start_registry_fixture_with_options(None, Some(case)).await;
            let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
            let reference = ImageReference::parse(&fixture.reference).unwrap();
            assert!(
                upstream.fetch_manifest(&reference).await.is_err(),
                "accepted {case:?}"
            );
            let directory = tempfile::tempdir().unwrap();
            let store = ImageStore::new(directory.path().to_path_buf());
            assert!(store.pull_and_unpack(&fixture.reference).await.is_err());
            assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), 0);
            assert!(!store.manifest_path(&reference).exists());
        }
    }

    #[tokio::test]
    async fn upstream_layer_sizes_do_not_allocate_from_untrusted_descriptors() {
        use crate::pickle::upstream::UpstreamRegistry;
        let fixture = start_registry_fixture().await;
        let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
        let reference = ImageReference::parse(&fixture.reference).unwrap();
        let manifest = upstream.fetch_manifest(&reference).await.unwrap();
        let mut layer = manifest.layers[0].clone();
        layer.size = u64::MAX;
        assert!(upstream.fetch_blob(&reference, &layer).await.is_err());
        assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn upstream_layer_sizes_match_direct_downloads_and_cached_blobs() {
        for warm_cache in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let store = ImageStore::new(directory.path().to_path_buf());
            if warm_cache {
                let valid = start_registry_fixture().await;
                store.pull_and_unpack(&valid.reference).await.unwrap();
            }
            let fixture = start_registry_fixture_with_options(
                None,
                Some(RegistryIntegrityCase::WrongLayerSize),
            )
            .await;
            assert!(
                store.pull_and_unpack(&fixture.reference).await.is_err(),
                "accepted wrong layer size with warm_cache={warm_cache}"
            );
            let reference = ImageReference::parse(&fixture.reference).unwrap();
            assert!(!store.rootfs_path(&reference).exists());
            assert_eq!(
                fixture.layer_requests.load(Ordering::SeqCst),
                usize::from(!warm_cache)
            );
        }
    }

    #[tokio::test]
    async fn upstream_layer_sizes_match_pull_through_downloads() {
        use crate::pickle::upstream::UpstreamRegistry;
        let fixture =
            start_registry_fixture_with_options(None, Some(RegistryIntegrityCase::WrongLayerSize))
                .await;
        let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
        let reference = ImageReference::parse(&fixture.reference).unwrap();
        let manifest = upstream.fetch_manifest(&reference).await.unwrap();
        assert!(
            upstream
                .fetch_blob(&reference, &manifest.layers[0])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn upstream_image_identity_is_verified_before_cache_publication() {
        for case in [
            RegistryIntegrityCase::ChangedConfiguration,
            RegistryIntegrityCase::ChangedManifest,
            RegistryIntegrityCase::ChangedIndex,
            RegistryIntegrityCase::ChangedChild,
            RegistryIntegrityCase::WrongConfigurationSize,
            RegistryIntegrityCase::WrongChildSize,
        ] {
            let fixture = start_registry_fixture_with_options(None, Some(case)).await;
            let directory = tempfile::tempdir().unwrap();
            let store = ImageStore::new(directory.path().to_path_buf());
            let result = store.pull_and_unpack(&fixture.reference).await;
            assert!(result.is_err(), "{case:?} was accepted: {result:?}");
            assert_eq!(
                fixture.layer_requests.load(Ordering::SeqCst),
                0,
                "unverified metadata reached layer fetch"
            );
            let reference = ImageReference::parse(&fixture.reference).unwrap();
            assert!(!store.manifest_path(&reference).exists());
            assert!(!store.rootfs_path(&reference).exists());
        }
    }

    #[tokio::test]
    async fn upstream_pull_through_verifies_the_requested_digest_chain() {
        use crate::pickle::upstream::UpstreamRegistry;
        for case in [
            RegistryIntegrityCase::ChangedIndex,
            RegistryIntegrityCase::ChangedChild,
            RegistryIntegrityCase::ChangedManifest,
            RegistryIntegrityCase::ChangedConfiguration,
            RegistryIntegrityCase::WrongConfigurationSize,
            RegistryIntegrityCase::WrongChildSize,
        ] {
            let fixture = start_registry_fixture_with_options(None, Some(case)).await;
            let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
            let reference = ImageReference::parse(&fixture.reference).unwrap();
            let result = upstream.fetch_manifest(&reference).await;
            assert!(
                result.is_err(),
                "{case:?} was accepted by pull-through: {result:?}"
            );
            assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn upstream_index_resolution_preserves_verified_child_bytes() {
        use crate::pickle::upstream::UpstreamRegistry;
        let fixture =
            start_registry_fixture_with_options(None, Some(RegistryIntegrityCase::ValidIndex))
                .await;
        let directory = tempfile::tempdir().unwrap();
        let store = ImageStore::new(directory.path().to_path_buf());
        let rootfs = store
            .pull_and_unpack(&fixture.reference)
            .await
            .unwrap()
            .rootfs;
        assert_eq!(
            std::fs::read(rootfs.join("bin/sh")).unwrap(),
            b"fixture shell"
        );
        let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
        let reference = ImageReference::parse(&fixture.reference).unwrap();
        let manifest = upstream.fetch_manifest(&reference).await.unwrap();
        assert_eq!(
            crate::pickle::store::compute_sha256(&manifest.manifest_bytes),
            manifest.digest
        );
        assert_eq!(
            crate::pickle::store::compute_sha256(&manifest.config_bytes),
            manifest.config.digest
        );
    }

    #[tokio::test]
    async fn upstream_selects_the_target_linux_architecture_before_fetching_blobs() {
        use crate::pickle::upstream::{OciUpstream, UpstreamRegistry};
        let fixture =
            start_registry_fixture_with_options(None, Some(RegistryIntegrityCase::Amd64Index))
                .await;
        let reference = ImageReference::parse(&fixture.reference).unwrap();
        for architecture in ["x86_64", "amd64"] {
            let upstream = OciUpstream::insecure_http(Default::default())
                .with_linux_architecture(architecture)
                .unwrap();
            assert!(upstream.fetch_manifest(&reference).await.is_ok());
        }
        let upstream = OciUpstream::insecure_http(Default::default())
            .with_linux_architecture("aarch64")
            .unwrap();
        assert!(upstream.fetch_manifest(&reference).await.is_err());
        assert!(
            OciUpstream::new(Default::default())
                .with_linux_architecture("unknown")
                .is_err()
        );
        assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn pull_through_registry_retries_transient_metadata_reads() {
        use crate::pickle::upstream::UpstreamRegistry;
        for head in [true, false] {
            let fixture = start_registry_fixture_with_fault(Some(registry_fault(
                false,
                StatusCode::TOO_MANY_REQUESTS,
                "TOOMANYREQUESTS",
                2,
            )))
            .await;
            let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
            let reference = ImageReference::parse(&fixture.reference).unwrap();
            if head {
                upstream.head_manifest_digest(&reference).await.unwrap();
            } else {
                upstream.fetch_manifest(&reference).await.unwrap();
            }
            assert_eq!(fixture.manifest_requests.load(Ordering::SeqCst), 3);
        }
    }

    #[tokio::test]
    async fn pull_through_registry_retries_interrupted_blobs_from_an_empty_buffer() {
        use crate::pickle::upstream::UpstreamRegistry;
        let mut fault = registry_fault(true, StatusCode::OK, "UNAVAILABLE", 1);
        fault.disconnect = true;
        let fixture = start_registry_fixture_with_fault(Some(fault)).await;
        let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
        let reference = ImageReference::parse(&fixture.reference).unwrap();
        let manifest = upstream.fetch_manifest(&reference).await.unwrap();
        let bytes = upstream
            .fetch_blob(&reference, &manifest.layers[0])
            .await
            .unwrap();
        assert_eq!(
            crate::pickle::store::compute_sha256(&bytes),
            manifest.layers[0].digest
        );
        assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), 2);
    }

    /// Let one stalled request hit its per-attempt ceiling in simulated time,
    /// then return to real time so the retry reaches the real HTTP fixture.
    async fn expire_stalled_attempt(
        received: &tokio::sync::Notify,
        budget: super::super::oci_pull::RegistryReadBudget,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(5), received.notified())
            .await
            .unwrap();
        // Pause only after the real HTTP server receives the request, so
        // simulated time cannot race socket readiness during setup.
        tokio::time::pause();
        tokio::time::advance(budget.attempt + std::time::Duration::from_secs(1)).await;
        tokio::time::resume();
    }

    #[tokio::test]
    async fn pull_through_registry_retries_a_stalled_read() {
        use super::super::oci_pull::{LAYER_READ, METADATA_READ};
        use crate::pickle::upstream::UpstreamRegistry;
        for mode in ["head", "manifest", "layer"] {
            let mut fault = registry_fault(
                mode == "layer",
                StatusCode::SERVICE_UNAVAILABLE,
                "UNAVAILABLE",
                1,
            );
            fault.delay = std::time::Duration::from_secs(3600);
            let received = fault.received.clone();
            let fixture = start_registry_fixture_with_fault(Some(fault)).await;
            let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
            let reference = ImageReference::parse(&fixture.reference).unwrap();
            let layer = if mode == "layer" {
                Some(upstream.fetch_manifest(&reference).await.unwrap().layers[0].clone())
            } else {
                None
            };
            let read = tokio::spawn(async move {
                match mode {
                    "head" => upstream.head_manifest_digest(&reference).await.map(|_| ()),
                    "manifest" => upstream.fetch_manifest(&reference).await.map(|_| ()),
                    _ => upstream
                        .fetch_blob(&reference, &layer.unwrap())
                        .await
                        .map(|_| ()),
                }
            });
            let budget = if mode == "layer" {
                LAYER_READ
            } else {
                METADATA_READ
            };
            expire_stalled_attempt(&received, budget).await;
            tokio::time::timeout(std::time::Duration::from_secs(10), read)
                .await
                .expect("the retry never completed")
                .unwrap()
                .unwrap_or_else(|error| panic!("{mode}: {error}"));
            let requests = if mode == "layer" {
                &fixture.layer_requests
            } else {
                &fixture.manifest_requests
            };
            assert_eq!(requests.load(Ordering::SeqCst), 2, "{mode}");
        }
    }

    #[tokio::test]
    async fn pull_through_registry_persistent_throttling_has_four_attempts() {
        use crate::pickle::upstream::UpstreamRegistry;
        let fixture = start_registry_fixture_with_fault(Some(registry_fault(
            false,
            StatusCode::TOO_MANY_REQUESTS,
            "TOOMANYREQUESTS",
            usize::MAX,
        )))
        .await;
        let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
        let reference = ImageReference::parse(&fixture.reference).unwrap();
        assert!(upstream.fetch_manifest(&reference).await.is_err());
        assert_eq!(fixture.manifest_requests.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn pull_through_registry_denial_and_integrity_errors_are_terminal() {
        use crate::pickle::upstream::UpstreamRegistry;
        let fixture = start_registry_fixture_with_fault(Some(registry_fault(
            false,
            StatusCode::FORBIDDEN,
            "DENIED",
            usize::MAX,
        )))
        .await;
        let upstream = crate::pickle::upstream::OciUpstream::insecure_http(Default::default());
        let reference = ImageReference::parse(&fixture.reference).unwrap();
        assert!(upstream.fetch_manifest(&reference).await.is_err());
        assert_eq!(fixture.manifest_requests.load(Ordering::SeqCst), 1);
        let fixture =
            start_registry_fixture_with_options(None, Some(RegistryIntegrityCase::WrongLayerSize))
                .await;
        let reference = ImageReference::parse(&fixture.reference).unwrap();
        let manifest = upstream.fetch_manifest(&reference).await.unwrap();
        assert!(
            upstream
                .fetch_blob(&reference, &manifest.layers[0])
                .await
                .is_err()
        );
        assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn registry_disconnect_retries_manifest_configuration_and_layer_reads() {
        for target in [
            RegistryFaultTarget::Manifest,
            RegistryFaultTarget::Configuration,
            RegistryFaultTarget::Layer,
        ] {
            let mut fault = registry_fault(false, StatusCode::OK, "UNAVAILABLE", 1);
            fault.target = target;
            fault.disconnect = true;
            let fixture = start_registry_fixture_with_fault(Some(fault)).await;
            let tmp = tempfile::tempdir().unwrap();
            let store = ImageStore::new(tmp.path().to_path_buf());
            let rootfs = store
                .pull_and_unpack(&fixture.reference)
                .await
                .unwrap()
                .rootfs;
            assert_eq!(
                std::fs::read(rootfs.join("bin/sh")).unwrap(),
                b"fixture shell"
            );
            assert_eq!(
                fixture.manifest_requests.load(Ordering::SeqCst),
                if matches!(target, RegistryFaultTarget::Layer) {
                    1
                } else {
                    2
                }
            );
            assert_eq!(
                fixture.layer_requests.load(Ordering::SeqCst),
                if matches!(target, RegistryFaultTarget::Layer) {
                    2
                } else {
                    1
                }
            );
        }
    }

    #[tokio::test]
    async fn registry_complete_corrupt_responses_are_not_retried() {
        for target in [
            RegistryFaultTarget::Manifest,
            RegistryFaultTarget::Configuration,
            RegistryFaultTarget::Layer,
        ] {
            let mut fault = registry_fault(false, StatusCode::OK, "INVALID", usize::MAX);
            fault.target = target;
            let fixture = start_registry_fixture_with_fault(Some(fault)).await;
            let tmp = tempfile::tempdir().unwrap();
            let store = ImageStore::new(tmp.path().to_path_buf());
            assert!(store.pull_and_unpack(&fixture.reference).await.is_err());
            assert_eq!(fixture.manifest_requests.load(Ordering::SeqCst), 1);
            assert_eq!(
                fixture.layer_requests.load(Ordering::SeqCst),
                usize::from(matches!(target, RegistryFaultTarget::Layer))
            );
        }
    }

    #[tokio::test]
    async fn registry_rate_limited_manifest_is_retried_before_unpacking() {
        let fixture = start_registry_fixture_with_fault(Some(registry_fault(
            false,
            StatusCode::TOO_MANY_REQUESTS,
            "TOOMANYREQUESTS",
            2,
        )))
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());
        let rootfs = store
            .pull_and_unpack(&fixture.reference)
            .await
            .unwrap()
            .rootfs;
        assert!(rootfs.join("bin/sh").exists());
        assert_eq!(fixture.manifest_requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn registry_unavailable_layer_is_retried_and_verified() {
        let fixture = start_registry_fixture_with_fault(Some(registry_fault(
            true,
            StatusCode::SERVICE_UNAVAILABLE,
            "UNAVAILABLE",
            1,
        )))
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());
        let rootfs = store
            .pull_and_unpack(&fixture.reference)
            .await
            .unwrap()
            .rootfs;
        assert_eq!(
            std::fs::read(rootfs.join("bin/sh")).unwrap(),
            b"fixture shell"
        );
        assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn registry_persistent_rate_limit_has_a_bounded_attempt_count() {
        let fixture = start_registry_fixture_with_fault(Some(registry_fault(
            false,
            StatusCode::TOO_MANY_REQUESTS,
            "TOOMANYREQUESTS",
            usize::MAX,
        )))
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            store.pull_and_unpack(&fixture.reference),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(ImageError::ManifestPull { .. })));
        assert_eq!(fixture.manifest_requests.load(Ordering::SeqCst), 4);
        assert!(
            !store
                .rootfs_path(&ImageReference::parse(&fixture.reference).unwrap())
                .exists()
        );
    }

    #[tokio::test]
    async fn registry_stalled_manifest_and_layer_reads_are_retried() {
        use super::super::oci_pull::{LAYER_READ, METADATA_READ};
        for target in [RegistryFaultTarget::Manifest, RegistryFaultTarget::Layer] {
            let layer = matches!(target, RegistryFaultTarget::Layer);
            let mut fault =
                registry_fault(layer, StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", 1);
            fault.delay = std::time::Duration::from_secs(3600);
            let received = fault.received.clone();
            let fixture = start_registry_fixture_with_fault(Some(fault)).await;
            let tmp = tempfile::tempdir().unwrap();
            let store = ImageStore::new(tmp.path().to_path_buf());
            let reference = fixture.reference.clone();
            let pull = tokio::spawn(async move { store.pull_and_unpack(&reference).await });
            expire_stalled_attempt(&received, if layer { LAYER_READ } else { METADATA_READ }).await;
            let rootfs = tokio::time::timeout(std::time::Duration::from_secs(10), pull)
                .await
                .expect("the retry never completed")
                .unwrap()
                .unwrap()
                .rootfs;
            assert_eq!(
                std::fs::read(rootfs.join("bin/sh")).unwrap(),
                b"fixture shell"
            );
            let (manifests, layers) = if layer { (1, 2) } else { (2, 1) };
            assert_eq!(fixture.manifest_requests.load(Ordering::SeqCst), manifests);
            assert_eq!(fixture.layer_requests.load(Ordering::SeqCst), layers);
        }
    }

    #[tokio::test]
    async fn registry_denial_is_not_retried() {
        let fixture = start_registry_fixture_with_fault(Some(registry_fault(
            false,
            StatusCode::FORBIDDEN,
            "DENIED",
            usize::MAX,
        )))
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());
        assert!(store.pull_and_unpack(&fixture.reference).await.is_err());
        assert_eq!(fixture.manifest_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn digest_pinned_local_registry_pull_creates_rootfs() {
        let fixture = start_registry_fixture().await;
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf());

        let pulled = store.pull_and_unpack(&fixture.reference).await.unwrap();
        assert!(pulled.rootfs.join("bin/sh").exists());
        assert!(pulled.rootfs.join("etc/os-release").exists());
        // The verified config blob comes back with the rootfs, so the
        // runtime can honour the image's entrypoint, env and working dir.
        assert_eq!(pulled.config.entrypoint, ["/bin/sh"]);
        assert_eq!(pulled.config.cmd, ["-c", "true"]);
        assert_eq!(pulled.config.env, ["FIXTURE=1"]);
        assert_eq!(pulled.config.working_dir.as_deref(), Some("/etc"));
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

    /// A loopback address nothing listens on: any request to it is refused.
    fn unreachable_registry() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    }

    /// `fixture`'s image under another registry host, and a mirror map
    /// sending that host to `mirror`.
    fn mirrored(fixture: &RegistryFixture, origin: &str, mirror: &str) -> (String, ImageMirrors) {
        let (_, path) = fixture.reference.split_once('/').unwrap();
        let mirrors =
            ImageMirrors::new(BTreeMap::from([(origin.to_owned(), mirror.to_owned())])).unwrap();
        (format!("{origin}/{path}"), mirrors)
    }

    fn registry_host(fixture: &RegistryFixture) -> &str {
        fixture.reference.split_once('/').unwrap().0
    }

    #[test]
    fn mirrors_apply_only_to_digest_pinned_references() {
        let mirrors = ImageMirrors::new(BTreeMap::from([(
            "public.ecr.aws".to_owned(),
            "127.0.0.1:5099".to_owned(),
        )]))
        .unwrap();
        let pinned = ImageReference::parse(&format!(
            "public.ecr.aws/docker/library/busybox@sha256:{}",
            "a".repeat(64)
        ))
        .unwrap();
        let mirror = mirrors.mirror_for(&pinned).unwrap();
        assert_eq!(mirror.registry, "127.0.0.1:5099");
        assert_eq!(mirror.repository, pinned.repository);
        assert_eq!(mirror.tag, pinned.tag);
        let tagged = ImageReference::parse("public.ecr.aws/docker/library/busybox:1.37").unwrap();
        assert_eq!(mirrors.mirror_for(&tagged), None);
        let elsewhere =
            ImageReference::parse(&format!("ghcr.io/org/app@sha256:{}", "a".repeat(64))).unwrap();
        assert_eq!(mirrors.mirror_for(&elsewhere), None);
        assert_eq!(mirrors.loopback_hosts(), ["127.0.0.1:5099"]);
    }

    #[test]
    fn mirrors_refuse_schemes_paths_and_empty_hosts() {
        for (upstream, mirror) in [
            ("public.ecr.aws", "http://127.0.0.1:5099"),
            ("public.ecr.aws", "mirror.internal/ecr"),
            ("public.ecr.aws", ""),
            ("", "mirror.internal"),
            ("public.ecr.aws", "user@mirror.internal"),
            ("public.ecr.aws", "mirror internal"),
        ] {
            let map = BTreeMap::from([(upstream.to_owned(), mirror.to_owned())]);
            assert!(
                ImageMirrors::new(map).is_err(),
                "accepted {upstream:?} = {mirror:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_mirror_serves_digest_pinned_pulls_without_the_origin() {
        let mirror = start_registry_fixture().await;
        let (reference, mirrors) =
            mirrored(&mirror, &unreachable_registry(), registry_host(&mirror));
        let tmp = tempfile::tempdir().unwrap();
        let store = ImageStore::new(tmp.path().to_path_buf()).with_mirrors(mirrors);
        let rootfs = store.pull_and_unpack(&reference).await.unwrap().rootfs;
        assert_eq!(
            std::fs::read(rootfs.join("bin/sh")).unwrap(),
            b"fixture shell"
        );
        assert_eq!(mirror.layer_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_denying_or_dishonest_mirror_falls_back_to_the_verified_origin() {
        for case in ["denied", "changed manifest", "changed configuration"] {
            let mirror = match case {
                "denied" => {
                    start_registry_fixture_with_fault(Some(registry_fault(
                        false,
                        StatusCode::FORBIDDEN,
                        "DENIED",
                        usize::MAX,
                    )))
                    .await
                }
                "changed manifest" => {
                    start_registry_fixture_with_options(
                        None,
                        Some(RegistryIntegrityCase::ChangedManifest),
                    )
                    .await
                }
                _ => {
                    start_registry_fixture_with_options(
                        None,
                        Some(RegistryIntegrityCase::ChangedConfiguration),
                    )
                    .await
                }
            };
            let origin = start_registry_fixture().await;
            let (reference, mirrors) =
                mirrored(&origin, registry_host(&origin), registry_host(&mirror));
            assert_eq!(reference, origin.reference, "{case}");
            let tmp = tempfile::tempdir().unwrap();
            let store = ImageStore::new(tmp.path().to_path_buf()).with_mirrors(mirrors);
            let pulled = store.pull_and_unpack(&reference).await.unwrap();
            assert_eq!(
                std::fs::read(pulled.rootfs.join("bin/sh")).unwrap(),
                b"fixture shell",
                "{case}"
            );
            assert_eq!(pulled.config.env, ["FIXTURE=1"], "{case}");
            assert_eq!(mirror.manifest_requests.load(Ordering::SeqCst), 1, "{case}");
            assert_eq!(mirror.layer_requests.load(Ordering::SeqCst), 0, "{case}");
            assert_eq!(origin.manifest_requests.load(Ordering::SeqCst), 1, "{case}");
        }
    }

    #[tokio::test]
    async fn pull_through_reads_digest_pinned_images_from_a_loopback_mirror() {
        use crate::pickle::upstream::{OciUpstream, UpstreamRegistry};
        let mirror = start_registry_fixture().await;
        let (reference, mirrors) =
            mirrored(&mirror, &unreachable_registry(), registry_host(&mirror));
        // The HTTPS client still reaches a loopback mirror over plain HTTP.
        let upstream = OciUpstream::new(Default::default()).with_mirrors(mirrors);
        let reference = ImageReference::parse(&reference).unwrap();
        let manifest = upstream.fetch_manifest(&reference).await.unwrap();
        let bytes = upstream
            .fetch_blob(&reference, &manifest.layers[0])
            .await
            .unwrap();
        assert_eq!(
            crate::pickle::store::compute_sha256(&bytes),
            manifest.layers[0].digest
        );
        assert_eq!(mirror.layer_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pull_through_falls_back_to_the_origin_when_the_mirror_lies() {
        use crate::pickle::upstream::{OciUpstream, UpstreamRegistry};
        let mirror =
            start_registry_fixture_with_options(None, Some(RegistryIntegrityCase::ChangedManifest))
                .await;
        let origin = start_registry_fixture().await;
        let (reference, mirrors) =
            mirrored(&origin, registry_host(&origin), registry_host(&mirror));
        let upstream = OciUpstream::insecure_http(Default::default()).with_mirrors(mirrors);
        let reference = ImageReference::parse(&reference).unwrap();
        let manifest = upstream.fetch_manifest(&reference).await.unwrap();
        assert_eq!(
            crate::pickle::store::compute_sha256(&manifest.manifest_bytes),
            manifest.digest
        );
        assert_eq!(mirror.manifest_requests.load(Ordering::SeqCst), 1);
        assert_eq!(origin.manifest_requests.load(Ordering::SeqCst), 1);
    }
}
