//! Peer replication and background healing for Pickle.
//!
//! A committed manifest can still await its configured redundancy. The
//! leader's healer plans which live nodes should take another copy; each
//! chosen node copies the blobs straight from a live holder through the OCI
//! Distribution API, validating every digest, and confirms its own copy
//! through Raft.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::store::BlobStore;
use super::types::{Digest, ImageManifest, PickleError};

/// Configuration for replication.
#[derive(Debug, Clone)]
pub struct ReplicationConfig {
    /// Number of total copies (including the pushing node).
    pub redundancy: u32,
    /// Timeout for replicating to a single peer.
    pub peer_timeout: Duration,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            redundancy: 2,
            peer_timeout: Duration::from_secs(30),
        }
    }
}

/// A cluster peer that can receive replicated layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    /// Raft node ID.
    pub node_id: u64,
    /// Base URL for the peer's OCI API (e.g. `https://10.0.1.5:5000`).
    pub base_url: String,
}

/// Select peers for replication.
///
/// Picks up to `count` peers from the available set, excluding the
/// local node and any nodes that already hold all layers. Prefers
/// nodes not already in the holder set.
pub fn select_peers(
    available: &[Peer],
    local_node_id: u64,
    existing_holders: &BTreeSet<u64>,
    count: usize,
) -> Vec<Peer> {
    let mut candidates: Vec<&Peer> = available
        .iter()
        .filter(|p| p.node_id != local_node_id)
        .collect();

    // Sort: prefer peers not already holding the layers
    candidates.sort_by_key(|p| existing_holders.contains(&p.node_id));

    candidates.into_iter().take(count).cloned().collect()
}

/// The result of a replication attempt.
#[derive(Debug)]
pub struct ReplicationResult {
    /// Nodes that successfully received all layers.
    pub successful_nodes: BTreeSet<u64>,
    /// Nodes that failed (with reason).
    pub failed_nodes: Vec<(u64, String)>,
}

/// Whether the configured redundancy has actually been achieved (REG7/D11).
///
/// `holder_count` is the number of nodes now holding a full copy — the
/// local node plus every peer that received it. Durable redundancy means
/// at least `redundancy` distinct holders. A push to a cluster with no
/// reachable peer sits at `holder_count == 1`, which is **not** durable
/// redundancy when `redundancy > 1`: the caller must report "replication
/// pending", never a durable success.
pub fn redundancy_satisfied(holder_count: usize, redundancy: u32) -> bool {
    holder_count >= redundancy as usize
}

/// Check which layers a peer already has.
///
/// Returns the set of digests the peer already holds (via HEAD requests).
/// This avoids re-uploading layers that the peer already cached from
/// a previous push or pull-through.
pub async fn check_peer_has_layers(
    peer: &Peer,
    repository: &str,
    digests: &[&Digest],
    client: &reqwest::Client,
    timeout: Duration,
) -> BTreeSet<String> {
    let mut has = BTreeSet::new();
    for digest in digests {
        let url = format!(
            "{}/v2/{}/blobs/{}",
            peer.base_url,
            repository,
            digest.as_str()
        );
        let result = tokio::time::timeout(timeout, client.head(&url).send()).await;
        if let Ok(Ok(resp)) = result
            && resp.status().is_success()
        {
            has.insert(digest.as_str().to_string());
        }
    }
    has
}

/// Resolve an OCI upload `location` against the peer's base URL, refusing
/// any destination that isn't the peer's own origin (REG6).
///
/// Returns the absolute URL to PUT to, or `None` when the location points
/// somewhere else. Rules:
/// - a relative path (`/v2/…`) is resolved against `base_url`;
/// - an absolute URL is accepted only if its scheme + host + port match
///   `base_url`;
/// - anything with a different origin — an attacker host, a different
///   scheme, a different port — is rejected.
///
/// `&str` in, `Option<String>` out: a borrow of the inputs, an owned URL
/// only on success.
pub(crate) fn resolve_same_origin_location(base_url: &str, location: &str) -> Option<String> {
    // A relative path: must start with `/` (not `//`, which is
    // protocol-relative and can change host). Resolve against the base.
    if let Some(rest) = location.strip_prefix('/') {
        if rest.starts_with('/') {
            return None; // protocol-relative `//host/…`
        }
        let base = base_url.trim_end_matches('/');
        return Some(format!("{base}/{rest}"));
    }

    // An absolute URL: accept only if its origin equals the peer's.
    let base_origin = url_origin(base_url)?;
    let loc_origin = url_origin(location)?;
    if base_origin == loc_origin {
        Some(location.to_string())
    } else {
        None
    }
}

/// The `scheme://host[:port]` origin of a URL, lowercased. `None` if the
/// input has no scheme (so it isn't an absolute URL we can compare).
fn url_origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    // Authority ends at the first `/`, `?`, or `#`.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    Some(format!(
        "{}://{}",
        scheme.to_lowercase(),
        authority.to_lowercase()
    ))
}

/// Replicate a single layer to a peer via its OCI upload API.
pub async fn replicate_layer_to_peer(
    peer: &Peer,
    repository: &str,
    digest: &Digest,
    data: &[u8],
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<(), PickleError> {
    // Initiate upload
    let initiate_url = format!("{}/v2/{}/blobs/uploads/", peer.base_url, repository);
    let resp = tokio::time::timeout(timeout, client.post(&initiate_url).send())
        .await
        .map_err(|_| {
            PickleError::ReplicationFailed(format!(
                "timeout initiating upload to peer {}",
                peer.node_id
            ))
        })?
        .map_err(|e| PickleError::ReplicationFailed(format!("peer {}: {e}", peer.node_id)))?;

    if !resp.status().is_success() {
        return Err(PickleError::ReplicationFailed(format!(
            "peer {} rejected upload initiation: {}",
            peer.node_id,
            resp.status()
        )));
    }

    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            PickleError::ReplicationFailed(format!(
                "peer {} did not return upload location",
                peer.node_id
            ))
        })?
        .to_string();

    // Constrain the redirect to the peer's own origin (REG6). A compromised
    // peer could otherwise return an absolute `location` pointing at an
    // attacker host, making this node PUT the blob bytes there. We accept
    // only a relative path (resolved against the peer's base URL) or an
    // absolute URL whose origin matches the peer's — anything else is
    // refused before the PUT leaves.
    let complete_url =
        resolve_same_origin_location(&peer.base_url, &location).ok_or_else(|| {
            PickleError::ReplicationFailed(format!(
                "peer {} returned a cross-origin upload location: {location}",
                peer.node_id
            ))
        })?;
    let complete_url = format!("{complete_url}?digest={}", digest.as_str());

    let resp = tokio::time::timeout(
        timeout,
        client.put(&complete_url).body(data.to_vec()).send(),
    )
    .await
    .map_err(|_| {
        PickleError::ReplicationFailed(format!("timeout uploading to peer {}", peer.node_id))
    })?
    .map_err(|e| PickleError::ReplicationFailed(format!("peer {}: {e}", peer.node_id)))?;

    if resp.status().is_success() {
        Ok(())
    } else {
        Err(PickleError::ReplicationFailed(format!(
            "peer {} rejected blob upload: {}",
            peer.node_id,
            resp.status()
        )))
    }
}

/// Replicate all layers of a manifest to selected peers.
///
/// Returns the set of node IDs that successfully received all layers.
/// If fewer than `config.redundancy - 1` peers succeed, returns an error.
pub async fn replicate_manifest(
    manifest: &ImageManifest,
    store: &BlobStore,
    peers: &[Peer],
    config: &ReplicationConfig,
    client: &reqwest::Client,
) -> Result<ReplicationResult, PickleError> {
    // Everything the tag pins travels — the manifest blob included
    // (REG1), so a peer can serve the manifest GET after a pull.
    let all_digests: Vec<&Digest> = manifest.referenced_digests();
    let mut successful = BTreeSet::new();
    let mut failed = Vec::new();

    for peer in peers {
        // Check which layers the peer already has
        let already_has = check_peer_has_layers(
            peer,
            &manifest.repository,
            &all_digests,
            client,
            config.peer_timeout,
        )
        .await;

        let mut peer_ok = true;
        for digest in &all_digests {
            if already_has.contains(digest.as_str()) {
                continue; // Skip layers the peer already has
            }

            let data = match store.read_blob(digest) {
                Ok(d) => d,
                Err(e) => {
                    failed.push((peer.node_id, format!("missing local blob: {e}")));
                    peer_ok = false;
                    break;
                }
            };

            if let Err(e) = replicate_layer_to_peer(
                peer,
                &manifest.repository,
                digest,
                &data,
                client,
                config.peer_timeout,
            )
            .await
            {
                failed.push((peer.node_id, e.to_string()));
                peer_ok = false;
                break;
            }
        }

        if peer_ok {
            successful.insert(peer.node_id);
        }
    }

    // Check if we have enough replicas
    let required = (config.redundancy as usize).saturating_sub(1); // minus the local copy
    if successful.len() < required && !peers.is_empty() {
        return Err(PickleError::ReplicationFailed(format!(
            "only {}/{} peers succeeded",
            successful.len(),
            required
        )));
    }

    Ok(ReplicationResult {
        successful_nodes: successful,
        failed_nodes: failed,
    })
}

// ---------------------------------------------------------------------------
// The heal loop (Phase 12 B5)
// ---------------------------------------------------------------------------

/// A manifest that needs more copies, and the live nodes currently holding
/// *all* of its blobs. Produced by [`plan_heal`], consumed by
/// [`heal_tick`].
#[derive(Debug)]
pub struct HealCandidate<'a> {
    pub manifest: &'a super::types::ImageManifest,
    pub full_holders: BTreeSet<u64>,
}

/// Find under-replicated manifests, rarest first.
///
/// Holders count only while they are live (#697): `live` is every node
/// gossip reports Alive or Suspect, plus the planning node. A node that
/// died stays in the Raft holder sets until it is decommissioned, and
/// counting it would leave an image one node loss from gone while heal
/// thought it was safe. The caller passes the live set in, so this stays a
/// pure function of its inputs.
///
/// "Rarest first" means ascending holder count: the manifests closest
/// to loss are healed before merely under-replicated ones. The result
/// is capped at `max_per_tick`, so a freshly joined empty node cannot
/// trigger a cluster-wide replication storm — the loop catches up a
/// slice at a time.
pub fn plan_heal<'a>(
    catalog: &'a super::types::ManifestCatalog,
    live: &BTreeSet<u64>,
    redundancy: u32,
    max_per_tick: usize,
) -> Vec<HealCandidate<'a>> {
    let mut candidates: Vec<HealCandidate<'a>> = catalog
        .manifests
        .iter()
        .filter_map(|(_, manifest)| {
            let mut full_holders: Option<BTreeSet<u64>> = None;
            for digest in manifest.referenced_digests() {
                let holders = catalog.effective_holders(digest.as_str(), live);
                full_holders = Some(match full_holders {
                    Some(acc) => acc.intersection(&holders).copied().collect(),
                    None => holders,
                });
            }
            let full_holders = full_holders.unwrap_or_default();
            (full_holders.len() < redundancy as usize).then_some(HealCandidate {
                manifest,
                full_holders,
            })
        })
        .collect();

    candidates.sort_by_key(|c| c.full_holders.len());
    candidates.truncate(max_per_tick);
    candidates
}

/// Choose up to `needed` reachable peers that don't yet hold a full copy.
///
/// The planning node (the leader) goes last: it would otherwise end up
/// holding every image, fill its `max_storage` first and then fail every
/// heal while followers had room (#697). It is chosen only when no other
/// peer can take the copy.
pub fn choose_heal_targets(
    peers: &[Peer],
    self_node: u64,
    full_holders: &BTreeSet<u64>,
    needed: usize,
) -> Vec<Peer> {
    let mut candidates: Vec<&Peer> = peers
        .iter()
        .filter(|peer| !full_holders.contains(&peer.node_id))
        .collect();
    candidates.sort_by_key(|peer| (peer.node_id == self_node, peer.node_id));
    candidates.into_iter().take(needed).cloned().collect()
}

/// What one heal pass did: receiving-node confirmations, and the
/// per-manifest failures (the caller logs them; a failure on one
/// manifest never aborts the pass).
#[derive(Debug, Default)]
pub struct HealOutcome {
    /// Images that gained at least one confirmed copy this pass.
    pub confirmed_images: Vec<Digest>,
    pub errors: Vec<String>,
}

/// Body of a `POST /v2/<name>/replicas/<digest>` request: the peers the
/// receiving node may copy the image's blobs from, in preference order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaRequest {
    /// Live nodes holding some or all of the image's blobs.
    pub sources: Vec<Peer>,
}

/// The authenticated cluster HTTP client a node uses to copy blobs from
/// its peers when the leader asks it to take a replica. Installed on the
/// registry router as an axum extension.
#[derive(Clone)]
pub struct ReplicaClient(pub reqwest::Client);

/// How long one replica copy may take end to end, layers included.
pub const REPLICA_TIMEOUT: Duration = Duration::from_secs(300);

/// One heal pass over the catalog. Only the leader runs it.
///
/// For each under-replicated manifest (rarest first, capped at
/// `max_per_tick`) the leader chooses targets among live non-holders, and
/// each target copies the blobs straight from a live holder and confirms
/// its own verified copy through Raft (#697). The leader takes a copy
/// itself only when it is one of the chosen targets, so it never relays
/// every image through its own disk.
pub async fn heal_tick(
    catalog: &super::types::ManifestCatalog,
    state: &super::api::PickleState,
    peers: &[Peer],
    live: &BTreeSet<u64>,
    redundancy: u32,
    max_per_tick: usize,
    client: &reqwest::Client,
) -> HealOutcome {
    let self_node = state.node_raft_id;
    let mut outcome = HealOutcome::default();

    for candidate in plan_heal(catalog, live, redundancy, max_per_tick) {
        let manifest = candidate.manifest;
        let digests = manifest.referenced_digests();
        let holds_any = |node: u64| {
            digests.iter().any(|digest| {
                catalog
                    .effective_holders(digest.as_str(), live)
                    .contains(&node)
            })
        };
        let needed = (redundancy as usize).saturating_sub(candidate.full_holders.len());
        let mut targets = choose_heal_targets(peers, self_node, &candidate.full_holders, needed);
        // A copy the leader takes can serve the others, so take it first.
        targets.sort_by_key(|target| target.node_id != self_node);
        let mut sources: Vec<Peer> = peers
            .iter()
            .filter(|peer| holds_any(peer.node_id))
            .cloned()
            .collect();
        sources.sort_by_key(|peer| peer.node_id == self_node);

        let mut confirmed = false;
        for target in targets {
            let target_sources: Vec<Peer> = sources
                .iter()
                .filter(|source| source.node_id != target.node_id)
                .cloned()
                .collect();
            let result = if target.node_id == self_node {
                state
                    .copy_image_from(
                        &manifest.repository,
                        &manifest.digest,
                        &target_sources,
                        client,
                    )
                    .await
                    .map(|_| ())
            } else {
                request_peer_replica(
                    &target,
                    &manifest.repository,
                    &manifest.digest,
                    &target_sources,
                    client,
                    REPLICA_TIMEOUT,
                )
                .await
                .map(|_| ())
            };
            match result {
                Ok(()) => {
                    confirmed = true;
                    if !sources
                        .iter()
                        .any(|source| source.node_id == target.node_id)
                    {
                        sources.push(target);
                    }
                }
                Err(error) => outcome.errors.push(format!(
                    "replica of {}@{} on node {} failed: {error}",
                    manifest.repository,
                    manifest.digest.as_str(),
                    target.node_id
                )),
            }
        }
        if confirmed {
            outcome.confirmed_images.push(manifest.digest.clone());
        }
    }

    outcome
}

impl super::api::PickleState {
    /// Make one catalogued image complete on this node: copy every blob it
    /// lacks from `sources` (tried in order, each blob digest-verified on
    /// arrival), then confirm this node's own verified copy through Raft.
    ///
    /// The leader calls it for itself when it is a chosen heal target, and
    /// every other target runs it when the leader asks over
    /// `POST /v2/<name>/replicas/<digest>`.
    pub async fn copy_image_from(
        &self,
        repository: &str,
        digest: &Digest,
        sources: &[Peer],
        client: &reqwest::Client,
    ) -> Result<super::copy::ImageCopyReceipt, PickleError> {
        let access = self
            .admit_repository_write(repository, None, None, true)
            .await?;
        let catalog = self.catalog_snapshot(repository).await?;
        let manifest = catalog
            .get_repository_manifest(repository, digest.as_str())
            .ok_or_else(|| PickleError::ManifestNotFound {
                repository: repository.to_string(),
                tag: digest.to_string(),
            })?;
        let timeout = Duration::from_secs(30);
        for blob in manifest.referenced_digests() {
            let mut copied = false;
            let mut last_error = None;
            for source in sources {
                match self
                    .pull_peer_blob_with_access(
                        source,
                        repository,
                        blob,
                        client,
                        timeout,
                        Some(access.clone()),
                    )
                    .await
                {
                    Ok(()) => {
                        copied = true;
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            if copied {
                continue;
            }
            // Nothing to copy from, but the bytes may already be here.
            let store = self.store.clone();
            let check = blob.clone();
            let local = tokio::task::spawn_blocking(move || store.revalidate_blob(&check))
                .await
                .map_err(|error| {
                    PickleError::ReplicationFailed(format!("cache verification failed: {error}"))
                })??;
            if !local {
                return Err(last_error.unwrap_or_else(|| {
                    PickleError::ReplicationFailed(format!("no live source holds blob {blob}"))
                }));
            }
        }
        self.confirm_image_copy_with_access(repository, digest, Some(access))
            .await
    }
}

/// Ask `target` to copy an image from `sources` and confirm its own copy.
/// The client must be the cluster's authenticated, non-redirecting
/// transport: the route only answers the cluster's service principal.
pub async fn request_peer_replica(
    target: &Peer,
    repository: &str,
    digest: &Digest,
    sources: &[Peer],
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<super::copy::ImageCopyReceipt, PickleError> {
    let url = format!(
        "{}/v2/{repository}/replicas/{}",
        target.base_url,
        digest.as_str()
    );
    let body = serde_json::to_vec(&ReplicaRequest {
        sources: sources.to_vec(),
    })
    .map_err(|error| PickleError::ReplicationFailed(error.to_string()))?;
    receipt_from(
        target,
        repository,
        digest,
        client.post(url).body(body),
        timeout,
    )
    .await
}

/// Send a copy request and accept only a receipt for exactly this node,
/// repository and digest.
async fn receipt_from(
    peer: &Peer,
    repository: &str,
    digest: &Digest,
    request: reqwest::RequestBuilder,
    timeout: Duration,
) -> Result<super::copy::ImageCopyReceipt, PickleError> {
    tokio::time::timeout(timeout, async {
        let mut response = request
            .send()
            .await
            .map_err(|error| PickleError::ReplicationFailed(error.to_string()))?;
        if !response.status().is_success() {
            return Err(PickleError::ReplicationFailed(format!(
                "peer {} refused copy confirmation: {}",
                peer.node_id,
                response.status()
            )));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| PickleError::ReplicationFailed(error.to_string()))?
        {
            if bytes.len().saturating_add(chunk.len()) > 16 * 1024 {
                return Err(PickleError::ReplicationFailed(
                    "copy receipt exceeds limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let receipt: super::copy::ImageCopyReceipt =
            serde_json::from_slice(&bytes).map_err(|error| {
                PickleError::ReplicationFailed(format!("invalid copy receipt: {error}"))
            })?;
        if receipt.node_id != peer.node_id
            || receipt.repository != repository
            || receipt.manifest_digest != *digest
        {
            return Err(PickleError::ReplicationFailed(
                "copy receipt does not match requested storage identity".into(),
            ));
        }
        Ok(receipt)
    })
    .await
    .map_err(|_| PickleError::ReplicationFailed("copy confirmation timed out".into()))?
}

/// Request proof from the receiving node even when HEAD avoided all uploads.
/// The client must use the cluster's non-redirecting authenticated transport.
pub async fn confirm_peer_copy(
    peer: &Peer,
    repository: &str,
    digest: &Digest,
    client: &reqwest::Client,
    timeout: Duration,
) -> Result<super::copy::ImageCopyReceipt, PickleError> {
    let url = format!(
        "{}/v2/{repository}/copies/{}",
        peer.base_url,
        digest.as_str()
    );
    receipt_from(peer, repository, digest, client.post(url), timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peers() -> Vec<Peer> {
        vec![
            Peer {
                node_id: 1,
                base_url: "http://10.0.1.1:5000".to_string(),
            },
            Peer {
                node_id: 2,
                base_url: "http://10.0.1.2:5000".to_string(),
            },
            Peer {
                node_id: 3,
                base_url: "http://10.0.1.3:5000".to_string(),
            },
            Peer {
                node_id: 4,
                base_url: "http://10.0.1.4:5000".to_string(),
            },
        ]
    }

    #[tokio::test]
    async fn peer_transfers_preserve_the_repository_namespace() {
        use axum::{
            Router,
            http::StatusCode,
            routing::{get, post},
        };
        let bytes = b"namespace-owned layer";
        let digest = super::super::store::compute_sha256(bytes);
        let blob_path = format!("/v2/rbtest-owned/team/web/blobs/{}", digest.as_str());
        let app = Router::new()
            .route(
                "/v2/rbtest-owned/team/web/blobs/uploads/",
                post(|| async {
                    (
                        StatusCode::ACCEPTED,
                        [("location", "/v2/rbtest-owned/team/web/blobs/uploads/owned")],
                    )
                }),
            )
            .route(
                "/v2/rbtest-owned/team/web/blobs/uploads/owned",
                axum::routing::put(|body: axum::body::Bytes| async move {
                    assert_eq!(body.as_ref(), b"namespace-owned layer");
                    StatusCode::CREATED
                }),
            )
            .route(&blob_path, get(|| async { "namespace-owned layer" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let peer = Peer {
            node_id: 1,
            base_url: format!("http://{address}"),
        };
        let client = reqwest::Client::new();
        let timeout = Duration::from_secs(2);
        let result = replicate_layer_to_peer(
            &peer,
            "rbtest-owned/team/web",
            &digest,
            bytes,
            &client,
            timeout,
        )
        .await;
        // Join the server on a failed assertion too; it owns no detached fixture.
        if let Err(error) = result {
            server.abort();
            let _ = server.await;
            panic!("nested peer upload failed: {error}");
        }
        let found =
            check_peer_has_layers(&peer, "rbtest-owned/team/web", &[&digest], &client, timeout)
                .await;
        assert!(found.contains(digest.as_str()));
        let directory = tempfile::tempdir().unwrap();
        let store = BlobStore::new(directory.path());
        super::super::pull::pull_layer_from_peer(
            &peer,
            "rbtest-owned/team/web",
            &digest,
            &store,
            &client,
            timeout,
        )
        .await
        .unwrap();
        assert_eq!(store.read_blob(&digest).unwrap(), bytes);
        server.abort();
        let _ = server.await;
    }

    // --- REG6: same-origin redirect constraint ---

    #[test]
    fn relative_location_resolves_against_the_peer() {
        let url = resolve_same_origin_location("http://10.0.1.5:5000", "/v2/app/blobs/uploads/abc")
            .unwrap();
        assert_eq!(url, "http://10.0.1.5:5000/v2/app/blobs/uploads/abc");
    }

    #[test]
    fn same_origin_absolute_location_is_allowed() {
        let url = resolve_same_origin_location(
            "https://10.0.1.5:5000",
            "https://10.0.1.5:5000/v2/app/blobs/uploads/abc",
        )
        .unwrap();
        assert_eq!(url, "https://10.0.1.5:5000/v2/app/blobs/uploads/abc");
    }

    #[test]
    fn cross_host_absolute_location_is_refused() {
        // The classic SSRF: a compromised peer redirects the PUT elsewhere.
        assert!(
            resolve_same_origin_location("http://10.0.1.5:5000", "http://attacker.example/collect")
                .is_none()
        );
    }

    #[test]
    fn different_port_or_scheme_is_refused() {
        assert!(
            resolve_same_origin_location("http://10.0.1.5:5000", "http://10.0.1.5:9999/x")
                .is_none()
        );
        assert!(
            resolve_same_origin_location("http://10.0.1.5:5000", "https://10.0.1.5:5000/x")
                .is_none()
        );
    }

    #[test]
    fn protocol_relative_location_is_refused() {
        // `//attacker/…` keeps the scheme but swaps the host.
        assert!(resolve_same_origin_location("http://10.0.1.5:5000", "//attacker/x").is_none());
    }

    #[test]
    fn select_peers_excludes_self() {
        let peers = test_peers();
        let selected = select_peers(&peers, 1, &BTreeSet::new(), 2);
        assert_eq!(selected.len(), 2);
        assert!(selected.iter().all(|p| p.node_id != 1));
    }

    #[test]
    fn select_peers_respects_count() {
        let peers = test_peers();
        let selected = select_peers(&peers, 1, &BTreeSet::new(), 1);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn select_peers_fewer_available_than_requested() {
        let peers = vec![Peer {
            node_id: 2,
            base_url: "http://10.0.1.2:5000".to_string(),
        }];
        let selected = select_peers(&peers, 1, &BTreeSet::new(), 5);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn select_peers_prefers_non_holders() {
        let peers = test_peers();
        let holders = BTreeSet::from([2, 3]);
        let selected = select_peers(&peers, 1, &holders, 2);

        // Should prefer node 4 (non-holder) over 2, 3
        assert!(selected.iter().any(|p| p.node_id == 4));
    }

    #[test]
    fn select_peers_empty_available() {
        let selected = select_peers(&[], 1, &BTreeSet::new(), 2);
        assert!(selected.is_empty());
    }

    #[test]
    fn select_peers_all_are_self() {
        let peers = vec![Peer {
            node_id: 1,
            base_url: "http://10.0.1.1:5000".to_string(),
        }];
        let selected = select_peers(&peers, 1, &BTreeSet::new(), 2);
        assert!(selected.is_empty());
    }

    #[test]
    fn replication_config_defaults() {
        let config = ReplicationConfig::default();
        assert_eq!(config.redundancy, 2);
        assert_eq!(config.peer_timeout, Duration::from_secs(30));
    }

    // -- plan_heal ----------------------------------------------------------

    use super::super::types::{ImageManifest, LayerDescriptor, ManifestCatalog, ManifestCommit};

    /// A digest whose hex encodes the suffix, so each manifest gets
    /// distinct layers.
    fn heal_digest(suffix: u8) -> Digest {
        Digest::new(&format!("sha256:{:064x}", suffix as u128)).unwrap()
    }

    fn heal_manifest(repo: &str, suffix: u8) -> ImageManifest {
        let layer = |s: u8| LayerDescriptor {
            digest: heal_digest(s),
            size: 100,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            platform: None,
        };
        ImageManifest {
            digest: heal_digest(suffix),
            config: layer(suffix + 1),
            layers: vec![layer(suffix + 2)],
            repository: repo.to_string(),
            tags: BTreeSet::new(),
            total_size: 200,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None,
        }
    }

    /// Catalog with three manifests holding 2, 0-ish (1), and 3 full
    /// copies respectively.
    fn heal_catalog() -> ManifestCatalog {
        let mut catalog = ManifestCatalog::default();
        for (repo, suffix, holders) in [
            ("two-copies", 10u8, vec![1u64, 2]),
            ("one-copy", 20, vec![1]),
            ("three-copies", 30, vec![1, 2, 3]),
        ] {
            catalog.apply_manifest_commit(&ManifestCommit {
                observed_gc_generation: 0,
                manifest: heal_manifest(repo, suffix),
                tag: "v1".to_string(),
                holder_nodes: holders.into_iter().collect(),
            });
        }
        catalog
    }

    fn all_live() -> BTreeSet<u64> {
        BTreeSet::from([1, 2, 3, 4])
    }

    /// #697: node 2 died. Raft still lists it as a holder of
    /// `two-copies`, but a dead node's copy can't serve a pull, so the
    /// image has one effective copy and must be healed.
    #[test]
    fn plan_heal_ignores_holders_that_are_not_live() {
        let catalog = heal_catalog();
        let live = BTreeSet::from([1, 3, 4]);

        let candidates = plan_heal(&catalog, &live, 2, 10);

        let two = candidates
            .iter()
            .find(|c| c.manifest.repository == "two-copies")
            .expect("a dead holder must not count towards redundancy");
        assert_eq!(two.full_holders, BTreeSet::from([1]));
        // `three-copies` still has two live holders (1 and 3).
        assert!(
            !candidates
                .iter()
                .any(|c| c.manifest.repository == "three-copies")
        );
    }

    /// #697: with the holder dead, the new copy goes to a live node that
    /// doesn't hold the image, and not to the leader while a follower can
    /// take it.
    #[test]
    fn heal_targets_a_live_non_holder_when_a_holder_died() {
        let catalog = heal_catalog();
        // Node 2 died, so gossip no longer offers it as a peer.
        let live = BTreeSet::from([1, 3, 4]);
        let peers: Vec<Peer> = test_peers()
            .into_iter()
            .filter(|peer| live.contains(&peer.node_id))
            .collect();
        let candidate = plan_heal(&catalog, &live, 2, 10)
            .into_iter()
            .find(|c| c.manifest.repository == "two-copies")
            .unwrap();
        let leader = 3;

        let targets = choose_heal_targets(&peers, leader, &candidate.full_holders, 1);

        assert_eq!(
            targets.iter().map(|peer| peer.node_id).collect::<Vec<_>>(),
            vec![4]
        );
        // Only when no follower is left does the leader take the copy.
        let targets = choose_heal_targets(&peers, leader, &BTreeSet::from([1, 4]), 1);
        assert_eq!(targets[0].node_id, leader);
    }

    #[test]
    fn audit_orders_rarest_first() {
        let catalog = heal_catalog();
        let candidates = plan_heal(&catalog, &all_live(), 3, 10);

        // three-copies is already at redundancy; the other two come
        // back rarest first.
        let repos: Vec<&str> = candidates
            .iter()
            .map(|c| c.manifest.repository.as_str())
            .collect();
        assert_eq!(repos, vec!["one-copy", "two-copies"]);
        assert_eq!(candidates[0].full_holders, BTreeSet::from([1]));
    }

    #[test]
    fn plan_heal_caps_work_per_tick() {
        let catalog = heal_catalog();
        let candidates = plan_heal(&catalog, &all_live(), 3, 1);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].manifest.repository, "one-copy");
    }

    #[test]
    fn plan_heal_empty_when_redundancy_met() {
        let catalog = heal_catalog();
        assert!(plan_heal(&catalog, &all_live(), 1, 10).is_empty());
    }

    /// REG1 upgrade path: a catalogue persisted before manifest blobs
    /// joined holder tracking has layers at full redundancy but no
    /// holder entry for the manifest blob itself. The heal loop must
    /// see that as under-replicated (and go fix it), not report the
    /// image healthy.
    #[test]
    fn plan_heal_treats_missing_manifest_blob_holders_as_under_replicated() {
        let mut catalog = heal_catalog();
        let manifest_digest = heal_digest(30); // "three-copies", holders {1,2,3}
        catalog
            .layer_locations
            .retain(|(d, _)| d != manifest_digest.as_str());

        let candidates = plan_heal(&catalog, &all_live(), 3, 10);
        assert!(
            candidates
                .iter()
                .any(|c| c.manifest.repository == "three-copies" && c.full_holders.is_empty()),
            "a manifest blob with no recorded holders must be healed"
        );
    }
}

#[cfg(test)]
mod copy_receipt_tests {
    use super::*;
    use axum::{Router, body::Body, response::Response, routing::post};

    #[tokio::test]
    async fn copy_receipts_refuse_wrong_identity_oversized_and_stalled_bodies() {
        let digest = super::super::store::compute_sha256(b"image");
        for mode in [
            "wrong-node",
            "wrong-repository",
            "wrong-digest",
            "oversized",
            "stalled",
        ] {
            let content = serde_json::to_vec(&super::super::copy::ImageCopyReceipt {
                node_id: if mode == "wrong-node" { 99 } else { 2 },
                repository: if mode == "wrong-repository" {
                    "elsewhere"
                } else {
                    "ordinary"
                }
                .into(),
                manifest_digest: if mode == "wrong-digest" {
                    super::super::store::compute_sha256(b"another")
                } else {
                    digest.clone()
                },
            })
            .unwrap();
            let app = Router::new().route(
                "/v2/ordinary/copies/{digest}",
                post(move || {
                    let content = content.clone();
                    async move {
                        let body = match mode {
                            "oversized" => Body::from(vec![b'x'; 16 * 1024 + 1]),
                            "stalled" => Body::from_stream(futures_util::stream::pending::<
                                Result<axum::body::Bytes, std::io::Error>,
                            >()),
                            _ => Body::from(content),
                        };
                        Response::new(body)
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let peer = Peer {
                node_id: 2,
                base_url: format!("http://{address}"),
            };
            let result = confirm_peer_copy(
                &peer,
                "ordinary",
                &digest,
                &reqwest::Client::new(),
                Duration::from_millis(200),
            )
            .await;
            assert!(result.is_err(), "{mode} cannot confirm storage");
            server.abort();
            let _ = server.await;
        }
    }
}
