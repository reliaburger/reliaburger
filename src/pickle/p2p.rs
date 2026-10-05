//! P2P download planning (Phase 12, slice C).
//!
//! A pure planner that decides which layer to fetch from which peer:
//! rarest layers first, load spread across the peers that hold them,
//! duplicates and locally-cached layers skipped. No I/O and no clock —
//! the parallel executor drives the plan; property tests drive the
//! planner across arbitrary topologies.

use std::collections::{HashMap, HashSet};

use std::sync::Arc;
use std::time::Duration;

use super::replication::Peer;
use super::types::{Digest, LayerDescriptor, ManifestCatalog, PickleError};
use crate::grill::image::LocalImageBlobs;

/// One planned fetch: this digest, from this peer.
#[derive(Debug, Clone)]
pub struct LayerFetch {
    pub digest: Digest,
    pub peer: Peer,
}

/// The output of [`plan_downloads`].
#[derive(Debug, Default)]
pub struct DownloadPlan {
    /// Planned fetches, rarest layer first.
    pub fetches: Vec<LayerFetch>,
    /// Digests no reachable peer holds — the caller falls back to the
    /// external registry path (or fails the pull honestly).
    pub unavailable: Vec<Digest>,
}

/// Plan which peer serves each needed layer.
///
/// - **Dedup:** a digest appearing twice in `needed` (a config blob
///   that doubles as a layer) is fetched once.
/// - **Skip local:** digests in `local` are excluded — they're already
///   in the blob store.
/// - **Rarest first:** layers with the fewest holding peers are
///   ordered first. When many nodes pull the same image at once, the
///   scarcest blobs spread fastest — the copies whose loss would hurt
///   most gain redundancy soonest.
/// - **Source balancing:** each layer is assigned to the holding peer
///   with the fewest assignments so far (ties broken by node id, so
///   plans are deterministic).
pub fn plan_downloads(
    needed: &[Digest],
    local: &HashSet<Digest>,
    catalog: &ManifestCatalog,
    peers: &[Peer],
    self_node: u64,
) -> DownloadPlan {
    // Dedup while preserving first-seen order, and drop local layers.
    let mut seen: HashSet<&Digest> = HashSet::new();
    let wanted: Vec<&Digest> = needed
        .iter()
        .filter(|d| !local.contains(*d) && seen.insert(*d))
        .collect();

    // For each wanted digest, the peers that hold it (never ourselves).
    let mut candidates: Vec<(&Digest, Vec<&Peer>)> = Vec::with_capacity(wanted.len());
    let mut unavailable = Vec::new();
    for digest in wanted {
        let holders = catalog.layer_holders(digest.as_str());
        let holding_peers: Vec<&Peer> = peers
            .iter()
            .filter(|p| p.node_id != self_node && holders.contains(&p.node_id))
            .collect();
        if holding_peers.is_empty() {
            unavailable.push(digest.clone());
        } else {
            candidates.push((digest, holding_peers));
        }
    }

    // Rarest first: ascending holder count.
    candidates.sort_by_key(|(_, holding)| holding.len());

    // Greedy least-loaded assignment.
    let mut load: HashMap<u64, usize> = HashMap::new();
    let mut fetches = Vec::with_capacity(candidates.len());
    for (digest, holding) in candidates {
        // Peer with the fewest assignments; ties broken by node id so
        // the plan is deterministic regardless of input order.
        let peer = holding
            .into_iter()
            .min_by_key(|p| (load.get(&p.node_id).copied().unwrap_or(0), p.node_id));
        if let Some(peer) = peer {
            *load.entry(peer.node_id).or_insert(0) += 1;
            fetches.push(LayerFetch {
                digest: digest.clone(),
                peer: peer.clone(),
            });
        }
    }

    DownloadPlan {
        fetches,
        unavailable,
    }
}

/// Execute a download plan with bounded concurrency.
///
/// At most `concurrency` fetches run at once in a `JoinSet`; each
/// failed fetch is retried (sequentially, after the parallel pass)
/// against the digest's other holders. A digest that exhausts its
/// holders fails the whole call — the caller decides what a partial
/// image means, not this function.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pull_layers_parallel(
    plan: DownloadPlan,
    repository: &str,
    catalog: &ManifestCatalog,
    peers: &[Peer],
    state: &super::api::PickleState,
    access: &super::lease::RegistryWriteAccess,
    client: &reqwest::Client,
    concurrency: usize,
    timeout: Duration,
) -> Result<(), PickleError> {
    let concurrency = concurrency.max(1);
    let mut queue = plan.fetches.into_iter();
    let mut in_flight = tokio::task::JoinSet::new();
    let mut failed: Vec<(Digest, u64)> = Vec::new();

    loop {
        // Keep the window full, then wait for one completion.
        while in_flight.len() < concurrency {
            let Some(fetch) = queue.next() else { break };
            let repository = repository.to_string();
            let state = state.clone();
            let access = access.clone();
            let client = client.clone();
            in_flight.spawn(async move {
                let result = state
                    .pull_peer_blob_with_access(
                        &fetch.peer,
                        &repository,
                        &fetch.digest,
                        &client,
                        timeout,
                        Some(access),
                    )
                    .await;
                (fetch, result)
            });
        }
        let Some(joined) = in_flight.join_next().await else {
            break; // queue drained and nothing in flight
        };
        match joined {
            Ok((_, Ok(()))) => {}
            Ok((fetch, Err(_))) => failed.push((fetch.digest, fetch.peer.node_id)),
            Err(e) => {
                return Err(PickleError::ReplicationFailed(format!(
                    "layer fetch task failed: {e}"
                )));
            }
        }
    }

    // Retry pass: each failure tries the digest's remaining holders.
    for (digest, tried_node) in failed {
        if state.store.has_blob(&digest) {
            continue;
        }
        let holders = catalog.layer_holders(digest.as_str());
        let mut recovered = false;
        for peer in peers
            .iter()
            .filter(|p| p.node_id != tried_node && holders.contains(&p.node_id))
        {
            if state
                .pull_peer_blob_with_access(
                    peer,
                    repository,
                    &digest,
                    client,
                    timeout,
                    Some(access.clone()),
                )
                .await
                .is_ok()
            {
                recovered = true;
                break;
            }
        }
        if !recovered {
            return Err(PickleError::ReplicationFailed(format!(
                "layer {digest} unavailable from any holder"
            )));
        }
    }

    Ok(())
}

/// Cluster-backed image source: the Pickle catalog plus P2P layer
/// pulls, installed into the grill's `ImageStore` in cluster mode (and
/// standalone too — it resolves locally-pushed images without peers).
pub struct ClusterSource {
    pub state: super::api::PickleState,
    /// Live gossip membership; `None` standalone (local blobs only).
    pub members:
        Option<tokio::sync::watch::Receiver<Vec<crate::mustard::membership::MembershipSnapshot>>>,
    pub registry_port: u16,
    /// URL scheme peers are addressed by (`"http"` or `"https"` when the
    /// registry runs over TLS — REG4). Kept beside `registry_port` so the
    /// derived peer URLs match how the local registry actually serves.
    pub peer_scheme: String,
    /// Parallel fetches per image pull (`[images] p2p_concurrency`).
    pub concurrency: usize,
    pub client: reqwest::Client,
    /// Upstream client for the pull-through cache; `None` disables it.
    pub upstream: Option<Arc<dyn super::upstream::UpstreamRegistry>>,
    /// `[images] pull_through` — the cache's master switch.
    pub pull_through: bool,
    /// `[images] cache_recheck_secs`.
    pub cache_recheck_secs: u64,
    /// Serialises cache fills so two instances of the same new image
    /// landing at once don't both download it from upstream. One lock
    /// for all images — the simplest correct thing; per-image locks
    /// are an optimisation nobody has needed yet.
    pub fill_lock: tokio::sync::Mutex<()>,
}

impl ClusterSource {
    /// Resolve `repository:tag` in the catalog and materialise every
    /// blob locally, fetching missing layers from peers in parallel.
    ///
    /// Returns the config and layer blob paths (layers in manifest order,
    /// ready to unpack), or `None` when the catalog doesn't know the image.
    pub async fn ensure_image_local(
        &self,
        repository: &str,
        tag: &str,
    ) -> Result<Option<LocalImageBlobs>, PickleError> {
        let peers = match &self.members {
            Some(rx) => crate::cluster::identity::pickle_peers_scheme(
                &rx.borrow(),
                self.registry_port,
                &self.peer_scheme,
            ),
            None => Vec::new(),
        };
        self.ensure_image_local_with_peers(repository, tag, &peers)
            .await
    }

    /// [`Self::ensure_image_local`] with an explicit peer list. Tests
    /// call this directly: in-process registries sit on ephemeral
    /// ports, which the uniform-`registry_port` derivation can't
    /// address.
    pub async fn ensure_image_local_with_peers(
        &self,
        repository: &str,
        tag: &str,
        peers: &[Peer],
    ) -> Result<Option<LocalImageBlobs>, PickleError> {
        self.ensure_image_local_for_architecture(repository, tag, peers, std::env::consts::ARCH)
            .await
    }

    /// [`Self::ensure_image_local_with_peers`] for a given container
    /// architecture (`amd64`/`x86_64` or `arm64`/`aarch64`). When the
    /// reference names a multi-platform image (an image index), the
    /// `linux/<architecture>` image is the one materialised; tests pick the
    /// architecture to exercise both sides of an index.
    pub async fn ensure_image_local_for_architecture(
        &self,
        repository: &str,
        tag: &str,
        peers: &[Peer],
        architecture: &str,
    ) -> Result<Option<LocalImageBlobs>, PickleError> {
        let catalog = self.state.catalog_snapshot(repository).await?;
        // A digest in the tag position (`repo@sha256:…` references put
        // it there) resolves content-addressed, so the bytes verified
        // at admission are the bytes pulled — a tag moved between
        // verify and pull changes nothing (IMG1).
        let manifest = match Digest::new(tag) {
            Ok(digest) => catalog
                .get_repository_manifest(repository, digest.as_str())
                .cloned(),
            Err(_) => catalog.get_manifest_by_tag(repository, tag).cloned(),
        };
        let Some(manifest) = manifest else {
            return Ok(None);
        };
        self.materialise_for_architecture(repository, manifest, catalog, peers, architecture, None)
            .await
            .map(Some)
    }

    /// Make `root` runnable here: the image itself, or, for an index, the
    /// `linux/<architecture>` image it names. `fill` is the pull-through
    /// cache's upstream: a platform the catalogue doesn't hold yet is fetched
    /// from it, so each architecture fills its own part of a cached index.
    async fn materialise_for_architecture(
        &self,
        repository: &str,
        root: super::types::ImageManifest,
        mut catalog: ManifestCatalog,
        peers: &[Peer],
        architecture: &str,
        fill: Option<(
            &dyn super::upstream::UpstreamRegistry,
            &crate::grill::image::ImageReference,
        )>,
    ) -> Result<LocalImageBlobs, PickleError> {
        let mut manifest = root;
        // A multi-platform image: materialise the index (and the platform
        // manifests it pins), then pick this node's platform. The index bytes
        // name the platform manifest by digest and every blob is
        // digest-verified, so a signature over the index covers the image
        // pulled here.
        if manifest.is_index() {
            self.materialise(repository, &manifest, &catalog, peers)
                .await?;
            let store = self.state.store.clone();
            let index_digest = manifest.digest.clone();
            let index_bytes = tokio::task::spawn_blocking(move || store.read_blob(&index_digest))
                .await
                .map_err(|error| {
                    PickleError::ReplicationFailed(format!(
                        "reading the image index failed: {error}"
                    ))
                })??;
            let platform_digest = select_platform_manifest(&index_bytes, architecture)?;
            if let Some((upstream, image)) = fill
                && catalog
                    .get_repository_manifest(repository, platform_digest.as_str())
                    .is_none()
            {
                self.fill_platform(upstream, image, repository, &platform_digest)
                    .await?;
                catalog = self.state.catalog_snapshot(repository).await?;
            }
            manifest = catalog
                .get_repository_manifest(repository, platform_digest.as_str())
                .cloned()
                .ok_or_else(|| PickleError::ManifestNotFound {
                    repository: repository.to_string(),
                    tag: platform_digest.as_str().to_string(),
                })?;
            if manifest.is_index() {
                return Err(PickleError::ReplicationFailed(format!(
                    "image index {} names another index for linux/{architecture}",
                    manifest.digest
                )));
            }
        }

        self.materialise(repository, &manifest, &catalog, peers)
            .await?;
        Ok(local_blobs(&self.state.store, &manifest))
    }

    /// Make every blob a catalogue entry pins local (its own manifest blob
    /// included, REG1), fetching the missing ones from peers in parallel,
    /// then record this node as a holder.
    async fn materialise(
        &self,
        repository: &str,
        manifest: &super::types::ImageManifest,
        catalog: &ManifestCatalog,
        peers: &[Peer],
    ) -> Result<(), PickleError> {
        // Cached bytes still need a durable repository owner before use.
        let access = self
            .state
            .admit_repository_write(repository, None, None, true)
            .await?;

        let digests: Vec<Digest> = manifest.referenced_digests().into_iter().cloned().collect();
        let store = self.state.store.clone();
        let candidates = digests.clone();
        let owner = access.guard.clone();
        let local: HashSet<Digest> = tokio::task::spawn_blocking(move || {
            let _owner = owner;
            let mut local = HashSet::new();
            for digest in candidates {
                if store.revalidate_blob(&digest)? {
                    local.insert(digest);
                }
            }
            Ok::<_, PickleError>(local)
        })
        .await
        .map_err(|error| {
            PickleError::ReplicationFailed(format!("cache verification failed: {error}"))
        })??;

        let plan = plan_downloads(&digests, &local, catalog, peers, self.state.node_raft_id);
        if !plan.unavailable.is_empty() {
            let missing: Vec<String> = plan.unavailable.iter().map(|d| d.to_string()).collect();
            return Err(PickleError::ReplicationFailed(format!(
                "no reachable holder for layers: {}",
                missing.join(", ")
            )));
        }

        pull_layers_parallel(
            plan,
            repository,
            catalog,
            peers,
            &self.state,
            &access,
            &self.client,
            self.concurrency,
            Duration::from_secs(30),
        )
        .await?;

        self.state
            .confirm_image_copy_with_access(repository, &manifest.digest, Some(access))
            .await?;
        Ok(())
    }
}

/// The platform manifest an image index offers for `linux/<architecture>`.
///
/// `architecture` takes either spelling (`x86_64`/`amd64`, `aarch64`/`arm64`),
/// as external pulls do. A platform variant (`arm64/v8`) doesn't matter: the
/// first `linux/<architecture>` entry wins.
pub fn select_platform_manifest(
    index_bytes: &[u8],
    architecture: &str,
) -> Result<Digest, PickleError> {
    #[derive(serde::Deserialize)]
    struct Platform {
        os: String,
        architecture: String,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        digest: String,
        platform: Option<Platform>,
    }
    #[derive(serde::Deserialize)]
    struct Index {
        manifests: Vec<Entry>,
    }

    let wanted = crate::grill::oci_pull::linux_architecture(architecture).map_err(|_| {
        PickleError::NoPlatformManifest {
            architecture: architecture.to_string(),
        }
    })?;
    let index: Index = serde_json::from_slice(index_bytes).map_err(|error| {
        PickleError::ReplicationFailed(format!("image index is not valid json: {error}"))
    })?;
    let entry = index
        .manifests
        .into_iter()
        .find(|entry| {
            entry
                .platform
                .as_ref()
                .is_some_and(|p| p.os == "linux" && p.architecture == wanted)
        })
        .ok_or_else(|| PickleError::NoPlatformManifest {
            architecture: wanted.to_string(),
        })?;
    Digest::new(&entry.digest)
}

impl ClusterSource {
    /// Pull-through cache entry point: serve `image` from the cluster
    /// cache, filling it from upstream on a miss or a moved tag.
    ///
    /// `Ok(None)` means the cache is disabled or has no upstream — the
    /// caller falls through to a direct external pull.
    pub async fn ensure_external_image(
        &self,
        image: &crate::grill::image::ImageReference,
    ) -> Result<Option<LocalImageBlobs>, PickleError> {
        let peers = match &self.members {
            Some(rx) => crate::cluster::identity::pickle_peers_scheme(
                &rx.borrow(),
                self.registry_port,
                &self.peer_scheme,
            ),
            None => Vec::new(),
        };
        self.ensure_external_image_with_peers(image, &peers).await
    }

    /// [`Self::ensure_external_image`] with an explicit peer list
    /// (tests: ephemeral ports).
    pub async fn ensure_external_image_with_peers(
        &self,
        image: &crate::grill::image::ImageReference,
        peers: &[Peer],
    ) -> Result<Option<LocalImageBlobs>, PickleError> {
        self.ensure_external_image_for_architecture(image, peers, std::env::consts::ARCH)
            .await
    }

    /// [`Self::ensure_external_image_with_peers`] for a given container
    /// architecture (`amd64`/`x86_64` or `arm64`/`aarch64`).
    ///
    /// A multi-platform upstream image is cached as its index, under the
    /// tag. Each platform's image is fetched from upstream the first time a
    /// node of that architecture asks for it, then served from the cluster
    /// like any other cached image.
    pub async fn ensure_external_image_for_architecture(
        &self,
        image: &crate::grill::image::ImageReference,
        peers: &[Peer],
        architecture: &str,
    ) -> Result<Option<LocalImageBlobs>, PickleError> {
        use super::upstream::{CacheDecision, CacheState, decide, refresh_or_refetch};

        if !self.pull_through {
            return Ok(None);
        }
        let Some(upstream) = self.upstream.as_deref() else {
            return Ok(None);
        };

        let cached_repo = super::upstream::cached_repository(image);
        let recheck = std::time::Duration::from_secs(self.cache_recheck_secs);

        let catalog = self.state.catalog_snapshot(&cached_repo).await?;
        let cached = match decide(
            &catalog,
            &cached_repo,
            &image.tag,
            std::time::SystemTime::now(),
            recheck,
        ) {
            CacheState::Fresh => true,
            // Same digest, or upstream unreachable, in which case a stale
            // cache beats no image: availability over freshness, stated
            // plainly.
            CacheState::Stale(cached_digest) => matches!(
                refresh_or_refetch(upstream, image, &cached_digest).await,
                Ok(CacheDecision::Hit) | Err(_)
            ),
            CacheState::Miss => false,
        };
        if !cached {
            self.fill_root(upstream, image, &cached_repo, recheck)
                .await?;
        }

        let catalog = self.state.catalog_snapshot(&cached_repo).await?;
        let Some(root) = catalog
            .get_manifest_by_tag(&cached_repo, &image.tag)
            .cloned()
        else {
            return Ok(None);
        };
        self.materialise_for_architecture(
            &cached_repo,
            root,
            catalog,
            peers,
            architecture,
            Some((upstream, image)),
        )
        .await
        .map(Some)
    }

    /// The cosign signature payloads for `image` at `digest`, read through
    /// the pull-through cache: the `sha256-<hex>.sig` image is cached under
    /// `cache/<host>/<repo>` like any other tag, so the cluster asks upstream
    /// once and every node after that reads the cluster's copy.
    ///
    /// `Ok(None)` means the cache is off; the caller fetches the signature
    /// straight from the registry with [`super::cosign::fetch_signature`].
    pub async fn cosign_signature(
        &self,
        image: &crate::grill::image::ImageReference,
        digest: &Digest,
    ) -> Result<Option<Vec<super::cosign::SignedPayload>>, super::cosign::CosignError> {
        use super::cosign::{CosignError, signature_layers, signature_reference};

        let reference = signature_reference(image, digest);
        let unavailable = |reason: String| CosignError::SignatureUnavailable {
            reference: reference.full_reference(),
            reason,
        };
        let malformed = |reason: String| CosignError::Malformed {
            reference: reference.full_reference(),
            reason,
        };
        // The signature image's config says `"architecture": ""`, which no
        // platform check would pass, but a single manifest isn't platform
        // checked: only an index picks a platform.
        if self
            .ensure_external_image(&reference)
            .await
            .map_err(|e| unavailable(e.to_string()))?
            .is_none()
        {
            return Ok(None);
        }
        let cached_repo = super::upstream::cached_repository(&reference);
        let catalog = self
            .state
            .catalog_snapshot(&cached_repo)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let manifest_digest = catalog
            .get_manifest_by_tag(&cached_repo, &reference.tag)
            .map(|manifest| manifest.digest.clone())
            .ok_or_else(|| unavailable("the cache lost the signature image".to_string()))?;

        // `ensure_external_image` made every blob local, the manifest's own
        // included, so these are disk reads.
        let store = self.state.store.clone();
        let read = tokio::task::spawn_blocking(move || {
            let manifest = store.read_blob(&manifest_digest)?;
            let layers = match signature_layers(&manifest) {
                Ok(layers) => layers,
                Err(reason) => return Ok(Err(reason)),
            };
            let mut payloads = Vec::new();
            for layer in layers {
                let bytes = store.read_blob(&layer.digest)?;
                match layer.with_payload(bytes) {
                    Ok(payload) => payloads.push(payload),
                    Err(reason) => return Ok(Err(reason)),
                }
            }
            Ok::<_, PickleError>(Ok(payloads))
        })
        .await
        .map_err(|e| unavailable(format!("reading the cached signature failed: {e}")))?
        .map_err(|e| unavailable(e.to_string()))?;
        read.map(Some).map_err(malformed)
    }

    /// Cache what `image` names upstream under its tag: a single-platform
    /// image whole, or an index with its platform manifests but none of
    /// their configs or layers yet.
    async fn fill_root(
        &self,
        upstream: &dyn super::upstream::UpstreamRegistry,
        image: &crate::grill::image::ImageReference,
        cached_repo: &str,
        recheck: Duration,
    ) -> Result<(), PickleError> {
        use super::upstream::{CacheState, UpstreamRoot, decide};

        // Serialised so concurrent misses don't double-download. Re-check
        // after acquiring: another task may have filled while we waited.
        let _guard = self.fill_lock.lock().await;
        let catalog = self.state.catalog_snapshot(cached_repo).await?;
        if matches!(
            decide(
                &catalog,
                cached_repo,
                &image.tag,
                std::time::SystemTime::now(),
                recheck,
            ),
            CacheState::Fresh
        ) {
            return Ok(());
        }

        match upstream.fetch_root(image).await? {
            UpstreamRoot::Image(manifest) => {
                self.store_upstream_image(upstream, image, manifest, cached_repo, &image.tag)
                    .await
            }
            UpstreamRoot::Index(index) => {
                self.store_upstream_index(index, cached_repo, &image.tag)
                    .await
            }
        }
    }

    /// Cache one platform image of a cached index, named by its digest.
    async fn fill_platform(
        &self,
        upstream: &dyn super::upstream::UpstreamRegistry,
        image: &crate::grill::image::ImageReference,
        cached_repo: &str,
        platform: &Digest,
    ) -> Result<(), PickleError> {
        let _guard = self.fill_lock.lock().await;
        let catalog = self.state.catalog_snapshot(cached_repo).await?;
        if catalog
            .get_repository_manifest(cached_repo, platform.as_str())
            .is_some()
        {
            return Ok(());
        }
        let pinned = crate::grill::image::ImageReference {
            registry: image.registry.clone(),
            repository: image.repository.clone(),
            tag: platform.as_str().to_string(),
        };
        let manifest = upstream.fetch_manifest(&pinned).await?;
        if manifest.digest != *platform {
            return Err(PickleError::ReplicationFailed(format!(
                "upstream answered {} with manifest {}",
                platform, manifest.digest
            )));
        }
        // Tagged by its digest, as a pushed index's platform manifests are.
        self.store_upstream_image(upstream, &pinned, manifest, cached_repo, platform.as_str())
            .await
    }

    /// Store an upstream image's manifest, config and layers, then commit
    /// it to the catalogue under `tag`.
    async fn store_upstream_image(
        &self,
        upstream: &dyn super::upstream::UpstreamRegistry,
        image: &crate::grill::image::ImageReference,
        manifest: super::upstream::UpstreamManifest,
        cached_repo: &str,
        tag: &str,
    ) -> Result<(), PickleError> {
        // The raw manifest bytes are a pinned blob like any layer
        // (REG1): the cache serves the manifest GET and peers pull it.
        self.state
            .store
            .write_blob_async(manifest.manifest_bytes, manifest.digest.clone())
            .await?;
        self.state
            .store
            .write_blob_async(manifest.config_bytes, manifest.config.digest.clone())
            .await?;
        for layer in &manifest.layers {
            if self.state.store.has_blob(&layer.digest) {
                continue;
            }
            let bytes = upstream.fetch_blob(image, layer).await?;
            // write_blob verifies the digest — a lying upstream fails here.
            self.state
                .store
                .write_blob_async(bytes, layer.digest.clone())
                .await?;
        }

        let total_size = manifest.layers.iter().map(|l| l.size).sum();
        let image_manifest = super::types::ImageManifest {
            digest: manifest.digest,
            config: manifest.config,
            layers: manifest.layers,
            repository: cached_repo.to_string(),
            tags: std::iter::once(tag.to_string()).collect(),
            total_size,
            pushed_at: std::time::SystemTime::now(),
            pushed_by: self.state.node_raft_id,
            // Upstream content was never signable by us; `cache/`
            // repositories are exempt from require_signatures.
            signature: None,
        };
        super::api::record_commit(&self.state, image_manifest, tag.to_string()).await
    }

    /// Store an upstream index and its platform manifests, then commit it
    /// under `tag` the way the registry records a pushed index: its own blob
    /// as the config descriptor, its platform manifests as the "layers".
    async fn store_upstream_index(
        &self,
        index: super::upstream::UpstreamIndex,
        cached_repo: &str,
        tag: &str,
    ) -> Result<(), PickleError> {
        for (descriptor, bytes) in &index.manifests {
            self.state
                .store
                .write_blob_async(bytes.clone(), descriptor.digest.clone())
                .await?;
        }
        self.state
            .store
            .write_blob_async(index.index_bytes.clone(), index.digest.clone())
            .await?;

        let size = index.index_bytes.len() as u64;
        let layers: Vec<LayerDescriptor> = index
            .manifests
            .into_iter()
            .map(|(descriptor, _)| descriptor)
            .collect();
        let entry = super::types::ImageManifest {
            digest: index.digest.clone(),
            config: LayerDescriptor {
                digest: index.digest,
                size,
                media_type: index.media_type,
                platform: None,
            },
            total_size: size + layers.iter().map(|l| l.size).sum::<u64>(),
            layers,
            repository: cached_repo.to_string(),
            tags: std::iter::once(tag.to_string()).collect(),
            pushed_at: std::time::SystemTime::now(),
            pushed_by: self.state.node_raft_id,
            signature: None,
        };
        super::api::record_commit(&self.state, entry, tag.to_string()).await
    }
}

/// Where a catalogued image's blobs sit in this node's store.
fn local_blobs(
    store: &super::store::BlobStore,
    manifest: &super::types::ImageManifest,
) -> LocalImageBlobs {
    LocalImageBlobs {
        layers: manifest
            .layers
            .iter()
            .map(|layer| store.blob_path(&layer.digest))
            .collect(),
        config: store.blob_path(&manifest.config.digest),
        // Digest's Display abbreviates; the image store re-checks the full value.
        config_digest: manifest.config.digest.as_str().to_string(),
    }
}

impl crate::grill::image::ClusterImageSource for ClusterSource {
    fn fetch_cluster_image<'a>(
        &'a self,
        repository: &'a str,
        tag: &'a str,
    ) -> crate::grill::image::ClusterFetchFuture<'a> {
        Box::pin(async move {
            self.ensure_image_local(repository, tag)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn fetch_pull_through<'a>(
        &'a self,
        image: &'a crate::grill::image::ImageReference,
    ) -> crate::grill::image::ClusterFetchFuture<'a> {
        Box::pin(async move {
            self.ensure_external_image(image)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::types::UpdateLayerLocations;
    use std::collections::BTreeSet;

    fn digest(i: u64) -> Digest {
        Digest::new(&format!("sha256:{i:064x}")).unwrap()
    }

    fn peer(id: u64) -> Peer {
        Peer {
            node_id: id,
            base_url: format!("http://10.0.1.{id}:5000"),
        }
    }

    /// Catalog whose layer i is held by the given node sets.
    fn catalog_with(holders: &[(u64, &[u64])]) -> ManifestCatalog {
        let mut catalog = ManifestCatalog::default();
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: holders
                .iter()
                .map(|(i, nodes)| (digest(*i), nodes.iter().copied().collect::<BTreeSet<u64>>()))
                .collect(),
        });
        catalog
    }

    fn index_of(entries: &[(&str, &str, u64)]) -> Vec<u8> {
        let manifests: Vec<serde_json::Value> = entries
            .iter()
            .map(|(os, architecture, i)| {
                serde_json::json!({
                    "digest": digest(*i).as_str(),
                    "size": 1,
                    "platform": { "os": os, "architecture": architecture },
                })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({ "schemaVersion": 2, "manifests": manifests }))
            .unwrap()
    }

    #[test]
    fn platform_selection_picks_the_matching_architecture_in_either_spelling() {
        let index = index_of(&[("linux", "amd64", 1), ("linux", "arm64", 2)]);
        assert_eq!(
            select_platform_manifest(&index, "amd64").unwrap(),
            digest(1)
        );
        assert_eq!(
            select_platform_manifest(&index, "x86_64").unwrap(),
            digest(1)
        );
        assert_eq!(
            select_platform_manifest(&index, "aarch64").unwrap(),
            digest(2)
        );
    }

    #[test]
    fn platform_selection_ignores_other_operating_systems() {
        let index = index_of(&[("windows", "amd64", 1), ("linux", "amd64", 2)]);
        assert_eq!(
            select_platform_manifest(&index, "amd64").unwrap(),
            digest(2)
        );
    }

    #[test]
    fn platform_selection_refuses_a_missing_or_unknown_architecture() {
        let index = index_of(&[("linux", "amd64", 1)]);
        for architecture in ["arm64", "riscv64"] {
            assert!(matches!(
                select_platform_manifest(&index, architecture),
                Err(PickleError::NoPlatformManifest { .. })
            ));
        }
    }

    #[test]
    fn plan_orders_rarest_first() {
        // Layer 1 has two holders, layer 2 has one.
        let catalog = catalog_with(&[(1, &[2, 3]), (2, &[3])]);
        let peers = vec![peer(2), peer(3)];

        let plan = plan_downloads(
            &[digest(1), digest(2)],
            &HashSet::new(),
            &catalog,
            &peers,
            1,
        );

        assert_eq!(plan.fetches.len(), 2);
        assert_eq!(plan.fetches[0].digest, digest(2), "rarest layer first");
        assert!(plan.unavailable.is_empty());
    }

    #[test]
    fn plan_balances_across_sources() {
        // Six layers, all held by peers 2 and 3: neither gets more
        // than three.
        let layers: Vec<(u64, &[u64])> = (1..=6).map(|i| (i, [2u64, 3].as_slice())).collect();
        let catalog = catalog_with(&layers);
        let peers = vec![peer(2), peer(3)];
        let needed: Vec<Digest> = (1..=6).map(digest).collect();

        let plan = plan_downloads(&needed, &HashSet::new(), &catalog, &peers, 1);

        let mut per_peer: HashMap<u64, usize> = HashMap::new();
        for fetch in &plan.fetches {
            *per_peer.entry(fetch.peer.node_id).or_insert(0) += 1;
        }
        assert_eq!(plan.fetches.len(), 6);
        assert!(
            per_peer.values().all(|&n| n <= 3),
            "unbalanced: {per_peer:?}"
        );
    }

    #[test]
    fn plan_dedups_digests() {
        let catalog = catalog_with(&[(1, &[2])]);
        let peers = vec![peer(2)];

        // The same digest twice (config blob doubling as a layer).
        let plan = plan_downloads(
            &[digest(1), digest(1)],
            &HashSet::new(),
            &catalog,
            &peers,
            1,
        );

        assert_eq!(plan.fetches.len(), 1);
    }

    #[test]
    fn plan_skips_local_layers() {
        let catalog = catalog_with(&[(1, &[2]), (2, &[2])]);
        let peers = vec![peer(2)];
        let local: HashSet<Digest> = [digest(1)].into_iter().collect();

        let plan = plan_downloads(&[digest(1), digest(2)], &local, &catalog, &peers, 1);

        assert_eq!(plan.fetches.len(), 1);
        assert_eq!(plan.fetches[0].digest, digest(2));
    }

    #[test]
    fn plan_reports_unavailable_layers() {
        // Layer 2's only holder is ourselves; layer 3 has no holders.
        let catalog = catalog_with(&[(1, &[2]), (2, &[1]), (3, &[])]);
        let peers = vec![peer(2)];

        let plan = plan_downloads(
            &[digest(1), digest(2), digest(3)],
            &HashSet::new(),
            &catalog,
            &peers,
            1,
        );

        assert_eq!(plan.fetches.len(), 1);
        assert_eq!(plan.unavailable, vec![digest(2), digest(3)]);
    }

    // -- properties -----------------------------------------------------

    use proptest::prelude::*;

    /// An arbitrary topology: for each layer, the subset of peer ids
    /// (1..=n_peers) holding it. May be empty.
    fn arbitrary_topology() -> impl Strategy<Value = (Vec<Vec<u64>>, u64)> {
        (1u64..=8).prop_flat_map(|n_peers| {
            (
                proptest::collection::vec(
                    proptest::collection::btree_set(1u64..=n_peers, 0..=n_peers as usize)
                        .prop_map(|s| s.into_iter().collect::<Vec<u64>>()),
                    1..40,
                ),
                Just(n_peers),
            )
        })
    }

    proptest! {
        /// Every layer with at least one live holder is assigned
        /// exactly once; every layer with none lands in `unavailable`.
        #[test]
        fn complete_coverage((topology, n_peers) in arbitrary_topology()) {
            let holders: Vec<(u64, &[u64])> = topology
                .iter()
                .enumerate()
                .map(|(i, nodes)| (i as u64 + 1, nodes.as_slice()))
                .collect();
            let catalog = catalog_with(&holders);
            let peers: Vec<Peer> = (1..=n_peers).map(peer).collect();
            let needed: Vec<Digest> = (1..=topology.len() as u64).map(digest).collect();

            // self_node = 0: never a peer, so "live holder" == any holder.
            let plan = plan_downloads(&needed, &HashSet::new(), &catalog, &peers, 0);

            prop_assert_eq!(
                plan.fetches.len() + plan.unavailable.len(),
                topology.len()
            );
            let fetched: HashSet<&Digest> = plan.fetches.iter().map(|f| &f.digest).collect();
            prop_assert_eq!(fetched.len(), plan.fetches.len(), "a digest was fetched twice");
            for (i, nodes) in topology.iter().enumerate() {
                let d = digest(i as u64 + 1);
                if nodes.is_empty() {
                    prop_assert!(plan.unavailable.contains(&d));
                } else {
                    prop_assert!(fetched.contains(&d));
                }
            }
        }

        /// No layer is ever assigned to a peer that doesn't hold it.
        #[test]
        fn holders_only((topology, n_peers) in arbitrary_topology()) {
            let holders: Vec<(u64, &[u64])> = topology
                .iter()
                .enumerate()
                .map(|(i, nodes)| (i as u64 + 1, nodes.as_slice()))
                .collect();
            let catalog = catalog_with(&holders);
            let peers: Vec<Peer> = (1..=n_peers).map(peer).collect();
            let needed: Vec<Digest> = (1..=topology.len() as u64).map(digest).collect();

            let plan = plan_downloads(&needed, &HashSet::new(), &catalog, &peers, 0);

            for fetch in &plan.fetches {
                let holders = catalog.layer_holders(fetch.digest.as_str());
                prop_assert!(
                    holders.contains(&fetch.peer.node_id),
                    "{} assigned to non-holder {}",
                    fetch.digest,
                    fetch.peer.node_id
                );
            }
        }

        /// With a uniform topology (every layer held by the same k
        /// peers), greedy least-loaded stays within ceil(n/k) per peer.
        /// (Arbitrary topologies can't promise this: a sole holder of
        /// many layers must take them all.)
        #[test]
        fn balance_bound_uniform(
            n_layers in 1usize..40,
            k in 1u64..=8,
        ) {
            let holder_ids: Vec<u64> = (1..=k).collect();
            let holders: Vec<(u64, &[u64])> = (1..=n_layers as u64)
                .map(|i| (i, holder_ids.as_slice()))
                .collect();
            let catalog = catalog_with(&holders);
            let peers: Vec<Peer> = (1..=k).map(peer).collect();
            let needed: Vec<Digest> = (1..=n_layers as u64).map(digest).collect();

            let plan = plan_downloads(&needed, &HashSet::new(), &catalog, &peers, 0);

            let mut per_peer: HashMap<u64, usize> = HashMap::new();
            for fetch in &plan.fetches {
                *per_peer.entry(fetch.peer.node_id).or_insert(0) += 1;
            }
            let bound = n_layers.div_ceil(k as usize);
            prop_assert!(
                per_peer.values().all(|&n| n <= bound),
                "load {per_peer:?} exceeds ceil({n_layers}/{k}) = {bound}"
            );
        }
    }
}
