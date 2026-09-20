//! Data types for the Pickle image registry.
//!
//! Defines digests, manifests, layer descriptors, and all the types
//! that flow through Raft for manifest catalog and layer location tracking.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Digest
// ---------------------------------------------------------------------------

/// A content-addressed digest in the format `algorithm:hex`.
///
/// Only `sha256` is supported. The digest uniquely identifies a blob
/// (layer or config) in the content-addressed store.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Digest(pub String);

impl Digest {
    /// Create a new digest, validating the format.
    pub fn new(s: &str) -> Result<Self, PickleError> {
        Self::validate(s)?;
        Ok(Self(s.to_string()))
    }

    /// Create a digest from a known-good string (e.g. computed SHA-256).
    pub fn from_sha256_hex(hex: &str) -> Self {
        Self(format!("sha256:{hex}"))
    }

    /// Validate the digest format.
    fn validate(s: &str) -> Result<(), PickleError> {
        let Some((algo, hex)) = s.split_once(':') else {
            return Err(PickleError::InvalidDigest(format!(
                "missing algorithm prefix: {s}"
            )));
        };
        if algo != "sha256" {
            return Err(PickleError::InvalidDigest(format!(
                "unsupported algorithm: {algo} (only sha256 is supported)"
            )));
        }
        if hex.len() != 64 {
            return Err(PickleError::InvalidDigest(format!(
                "sha256 hex must be 64 chars, got {}",
                hex.len()
            )));
        }
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(PickleError::InvalidDigest(format!(
                "invalid hex characters in digest: {hex}"
            )));
        }
        Ok(())
    }

    /// Returns the hex part of the digest (after the `sha256:` prefix).
    pub fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }

    /// Returns the full digest string including algorithm prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Show abbreviated form for display
        let hex = self.hex();
        if hex.len() > 12 {
            write!(f, "sha256:{}...", &hex[..12])
        } else {
            write!(f, "{}", self.0)
        }
    }
}

// ---------------------------------------------------------------------------
// Layer descriptor
// ---------------------------------------------------------------------------

/// Describes a single layer or config blob in an image manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerDescriptor {
    /// Content-addressed digest of the blob.
    pub digest: Digest,
    /// Size in bytes.
    pub size: u64,
    /// OCI media type (e.g. `application/vnd.oci.image.layer.v1.tar+gzip`).
    pub media_type: String,
}

// ---------------------------------------------------------------------------
// Image manifest
// ---------------------------------------------------------------------------

/// An OCI image manifest stored in the Pickle registry.
///
/// This is the Raft-persisted representation. It tracks which tags
/// point to this manifest and when it was pushed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageManifest {
    /// Content-addressed digest of the manifest itself.
    pub digest: Digest,
    /// The config blob descriptor.
    pub config: LayerDescriptor,
    /// Layer blob descriptors, in order.
    pub layers: Vec<LayerDescriptor>,
    /// Repository name (e.g. `myapp` or `team/myapp`).
    pub repository: String,
    /// Tags pointing to this manifest (e.g. `{"latest", "v1.2.3"}`).
    pub tags: BTreeSet<String>,
    /// Total size in bytes (config + all layers).
    pub total_size: u64,
    /// When this manifest was first pushed.
    pub pushed_at: SystemTime,
    /// Raft node ID of the node that pushed this manifest.
    pub pushed_by: u64,
    /// Image signature, if signed. Attached after push via `AttachSignature`.
    #[serde(default)]
    pub signature: Option<ImageSignature>,
}

impl ImageManifest {
    /// All digests referenced by this manifest (config + layers).
    ///
    /// These are the blobs needed to *run* the image. For everything a
    /// catalogued tag pins in the blob store — including the manifest's
    /// own raw bytes — use [`Self::referenced_digests`].
    pub fn all_digests(&self) -> Vec<&Digest> {
        let mut digests = vec![&self.config.digest];
        for layer in &self.layers {
            digests.push(&layer.digest);
        }
        digests
    }

    /// Every digest this catalogue entry pins in the blob store: the
    /// manifest's own blob, then the config and layers (deduplicated —
    /// an index entry's config descriptor points back at the index
    /// blob itself).
    ///
    /// Holder tracking, replication, GC reachability and peer pulls
    /// must all agree on this set. The manifest blob used to be left
    /// out (REG1): GC would sweep it as an orphan after the grace
    /// window and the catalogue would point at a 404.
    pub fn referenced_digests(&self) -> Vec<&Digest> {
        let mut digests = vec![&self.digest];
        for digest in self.all_digests() {
            if !digests.contains(&digest) {
                digests.push(digest);
            }
        }
        digests
    }
}

// ---------------------------------------------------------------------------
// Raft commands for Pickle
// ---------------------------------------------------------------------------

/// Commit a manifest to the Raft catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestCommit {
    /// GC generation observed while verifying the publishing node's local blobs.
    /// Cluster publication refuses if collection advanced any declared holder.
    #[serde(default)]
    pub observed_gc_generation: u64,
    /// The manifest to store.
    pub manifest: ImageManifest,
    /// Tag to associate with this manifest (e.g. `"latest"`).
    pub tag: String,
    /// Nodes that hold all layers after replication.
    pub holder_nodes: BTreeSet<u64>,
}

/// Update which nodes hold copies of specific layers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateLayerLocations {
    /// Layer digest → set of node IDs that hold it.
    pub updates: Vec<(Digest, BTreeSet<u64>)>,
}

/// Report that a node has deleted layers during GC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GcReport {
    /// The node that ran GC.
    pub node_id: u64,
    /// Layer digests that were deleted from this node.
    pub deleted_layers: Vec<Digest>,
}

/// Delete a tag from the manifest catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteTag {
    /// Repository name.
    pub repository: String,
    /// Tag to delete.
    pub tag: String,
}

// ---------------------------------------------------------------------------
// Manifest catalog (part of DesiredState)
// ---------------------------------------------------------------------------

/// Public image-list entry, excluding internal ownership and storage-node details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSummary {
    /// Repository containing this manifest.
    pub repository: String,
    /// Exact content digest.
    pub digest: String,
    /// Current tags belonging to this repository copy.
    pub tags: BTreeSet<String>,
    /// Number of filesystem layers.
    pub layers: usize,
    /// Logical manifest content size in bytes.
    pub total_size: u64,
}

/// The manifest catalog stored in Raft as part of DesiredState.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestCatalog {
    /// Exact lease generation owning each reserved repository, even before a manifest.
    #[serde(default)]
    pub repository_owners: BTreeMap<String, String>,
    /// Per-repository manifest rows, carrying their content digest as the key.
    /// Identical content may have independent tags in several repositories.
    pub manifests: Vec<(String, ImageManifest)>,
    /// Tag→digest mappings. Key is `"repository:tag"`, value is digest string.
    pub tags: Vec<(String, String)>,
    /// Layer→holders mappings. Key is digest string, value is set of node IDs.
    pub layer_locations: Vec<(String, BTreeSet<u64>)>,
}

impl ManifestCatalog {
    /// Bind local storage to its exact lease before accepting any upload bytes.
    /// Existing unowned metadata cannot be adopted as proof of a fresh repository.
    pub fn claim_repository(
        &mut self,
        repository: &str,
        lease_id: &str,
    ) -> Result<(), PickleError> {
        if lease_id.is_empty()
            || !repository
                .split_once('/')
                .is_some_and(|(namespace, _)| namespace.starts_with("rbtest-"))
        {
            return Err(PickleError::LeaseDenied(
                "invalid repository lease identity".into(),
            ));
        }
        self.check_repository_owner(repository, lease_id)?;
        self.repository_owners
            .insert(repository.into(), lease_id.into());
        Ok(())
    }

    /// Refuse a different generation or metadata whose original owner is unknown.
    pub fn check_repository_owner(
        &self,
        repository: &str,
        lease_id: &str,
    ) -> Result<(), PickleError> {
        match self.repository_owners.get(repository) {
            Some(owner) if owner == lease_id => Ok(()),
            Some(_) => Err(PickleError::LeaseDenied(
                "repository belongs to another lease generation".into(),
            )),
            None if self
                .manifests
                .iter()
                .any(|(_, manifest)| manifest.repository == repository)
                || !self.tags_for_repository(repository).is_empty() =>
            {
                Err(PickleError::LeaseDenied(
                    "repository metadata has no confirmed lease owner".into(),
                ))
            }
            None => Ok(()),
        }
    }

    /// Retire this exact generation; an already empty repository is an idempotent retry.
    pub fn retire_leased_repository(
        &mut self,
        repository: &str,
        lease_id: &str,
    ) -> Result<(), PickleError> {
        self.check_repository_owner(repository, lease_id)?;
        self.retire_repository(repository);
        Ok(())
    }

    /// Describe committed images without exposing internal ownership records.
    pub fn images(&self) -> Vec<ImageSummary> {
        self.manifests
            .iter()
            .map(|(digest, manifest)| ImageSummary {
                repository: manifest.repository.clone(),
                digest: digest.clone(),
                tags: manifest.tags.clone(),
                layers: manifest.layers.len(),
                total_size: manifest.total_size,
            })
            .collect()
    }

    /// Project one repository without exposing unrelated manifests, tags or holders.
    pub fn repository_view(&self, repository: &str) -> Self {
        let prefix = format!("{repository}:");
        let mut view = Self {
            repository_owners: self
                .repository_owners
                .iter()
                .filter(|(name, _)| name.as_str() == repository)
                .map(|(name, owner)| (name.clone(), owner.clone()))
                .collect(),
            manifests: self
                .manifests
                .iter()
                .filter(|(_, manifest)| manifest.repository == repository)
                .cloned()
                .collect(),
            tags: self
                .tags
                .iter()
                .filter(|(name, _)| name.starts_with(&prefix))
                .cloned()
                .collect(),
            layer_locations: Vec::new(),
        };
        let referenced = view.referenced_digest_set();
        view.layer_locations = self
            .layer_locations
            .iter()
            .filter(|(digest, _)| referenced.contains(digest))
            .cloned()
            .collect();
        view
    }

    /// Logical stored image sizes used by repository and aggregate quota admission.
    pub fn stored_sizes(&self, repository: &str) -> (u64, u64) {
        let mut repository_bytes = 0u64;
        let mut total_bytes = 0u64;
        for (_, manifest) in &self.manifests {
            total_bytes = total_bytes.saturating_add(manifest.total_size);
            if manifest.repository == repository {
                repository_bytes = repository_bytes.saturating_add(manifest.total_size);
            }
        }
        (repository_bytes, total_bytes)
    }

    /// Look up shared content by digest, without selecting repository metadata.
    /// Repository-aware callers must use `get_repository_manifest` instead.
    pub fn get_manifest(&self, digest: &str) -> Option<&ImageManifest> {
        self.manifests
            .iter()
            .find(|(d, _)| d == digest)
            .map(|(_, m)| m)
    }

    /// Look up one repository's manifest metadata for content with this digest.
    pub fn get_repository_manifest(
        &self,
        repository: &str,
        digest: &str,
    ) -> Option<&ImageManifest> {
        self.manifests
            .iter()
            .find(|(stored, manifest)| stored == digest && manifest.repository == repository)
            .map(|(_, manifest)| manifest)
    }

    /// Look up a manifest by repository and tag.
    pub fn get_manifest_by_tag(&self, repository: &str, tag: &str) -> Option<&ImageManifest> {
        let key = format!("{repository}:{tag}");
        let digest = self.tags.iter().find(|(k, _)| k == &key).map(|(_, v)| v)?;
        self.get_repository_manifest(repository, digest)
    }

    /// Get all tags for a repository.
    pub fn tags_for_repository(&self, repository: &str) -> Vec<String> {
        let prefix = format!("{repository}:");
        self.tags
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.strip_prefix(&prefix).unwrap_or(k).to_string())
            .collect()
    }

    /// Get the set of nodes holding a layer.
    pub fn layer_holders(&self, digest: &str) -> BTreeSet<u64> {
        self.layer_locations
            .iter()
            .find(|(d, _)| d == digest)
            .map(|(_, holders)| holders.clone())
            .unwrap_or_default()
    }

    /// Apply a ManifestCommit.
    pub fn apply_manifest_commit(&mut self, commit: &ManifestCommit) {
        let digest_str = commit.manifest.digest.0.clone();
        let tag_key = format!("{}:{}", commit.manifest.repository, commit.tag);

        // Signatures attest content, while tags and retirement belong to a
        // repository. A new repository copy preserves the content signature.
        let signature = commit.manifest.signature.clone().or_else(|| {
            self.get_manifest(&digest_str)
                .and_then(|manifest| manifest.signature.clone())
        });
        // A pull may already have verified and pinned the old digest. Moving
        // its tag must not discard the metadata needed to finish that pull.
        self.tags.retain(|(key, _)| key != &tag_key);
        for (_, manifest) in self
            .manifests
            .iter_mut()
            .filter(|(_, manifest)| manifest.repository == commit.manifest.repository)
        {
            manifest.tags.remove(&commit.tag);
        }
        self.tags.push((tag_key, digest_str.clone()));

        if let Some((_, existing)) = self.manifests.iter_mut().find(|(digest, manifest)| {
            digest == &digest_str && manifest.repository == commit.manifest.repository
        }) {
            existing.tags.insert(commit.tag.clone());
        } else {
            let mut manifest = commit.manifest.clone();
            manifest.tags = BTreeSet::from([commit.tag.clone()]);
            manifest.signature = signature;
            self.manifests.push((digest_str.clone(), manifest));
        }

        // Update holder locations for everything this tag pins — the
        // manifest's own blob included (REG1), or GC and the heal loop
        // treat it as an orphan.
        for layer in commit.manifest.referenced_digests() {
            let layer_str = layer.0.clone();
            if let Some((_, holders)) = self
                .layer_locations
                .iter_mut()
                .find(|(d, _)| d == &layer_str)
            {
                for node in &commit.holder_nodes {
                    holders.insert(*node);
                }
            } else {
                self.layer_locations
                    .push((layer_str, commit.holder_nodes.clone()));
            }
        }
    }

    /// Apply an UpdateLayerLocations.
    pub fn apply_update_locations(&mut self, update: &UpdateLayerLocations) {
        for (digest, nodes) in &update.updates {
            let digest_str = digest.0.clone();
            if let Some((_, holders)) = self
                .layer_locations
                .iter_mut()
                .find(|(d, _)| d == &digest_str)
            {
                *holders = nodes.clone();
            } else {
                self.layer_locations.push((digest_str, nodes.clone()));
            }
        }
    }

    /// Apply a GcReport, arbitrating which deletions are safe.
    ///
    /// The report is a *proposal*: the node nominates layers it wants
    /// to delete, and this method — running serialised inside the Raft
    /// apply loop — approves only deletions that leave at least one
    /// other holder. This closes the M2 race where two nodes each
    /// holding one of two copies both saw `holders.len() == 2` and
    /// both deleted, losing the layer entirely.
    ///
    /// Returns the approved digests; the proposing node physically
    /// deletes only those. Untracked layers (no holder entry) are
    /// approved as orphans.
    ///
    /// The recheck runs against the **full catalogue reference set**
    /// immediately before approval (REG7/D11), not only sole-copy: a
    /// digest that any catalogued manifest still references — its config,
    /// a layer, or the manifest blob itself — is never approved for
    /// deletion, even if the nominating node saw it as an orphan when it
    /// built the report. The nomination and the approval can be separated
    /// by a fresh push that re-referenced the blob; this is the last,
    /// serialised chance to refuse.
    pub fn apply_gc_report(&mut self, report: &GcReport) -> Vec<Digest> {
        // The set of every digest a catalogued manifest pins right now.
        let referenced = self.referenced_digest_set();

        let mut approved = Vec::new();
        for digest in &report.deleted_layers {
            // Last-moment reference recheck: a still-referenced blob is
            // never deleted, regardless of holder bookkeeping.
            if referenced.contains(digest.as_str()) {
                continue;
            }
            let digest_str = &digest.0;
            match self
                .layer_locations
                .iter_mut()
                .find(|(d, _)| d == digest_str)
            {
                Some((_, holders)) => {
                    let others = holders.iter().filter(|&&n| n != report.node_id).count();
                    if others >= 1 {
                        // Approval may already have removed this holder before
                        // a failed deletion or crash. Reapprove its extra copy,
                        // while preserving the other advertised holder.
                        holders.remove(&report.node_id);
                        approved.push(digest.clone());
                    }
                    // Without another advertised holder, keep the layer.
                }
                None => {
                    // Orphan: not tracked in the catalog, nothing to lose.
                    approved.push(digest.clone());
                }
            }
        }
        approved
    }

    /// Every digest that a catalogued manifest currently pins in the blob
    /// store — each manifest's own blob, config and layers (REG7).
    ///
    /// Returns owned digest strings so the caller can borrow `self`
    /// mutably afterwards.
    pub fn referenced_digest_set(&self) -> std::collections::HashSet<String> {
        let mut set = std::collections::HashSet::new();
        for (_, manifest) in &self.manifests {
            for digest in manifest.referenced_digests() {
                set.insert(digest.0.clone());
            }
        }
        set
    }

    /// Remove all metadata for one retired repository, preserving shared content.
    /// The caller must establish workload retirement and fence every writer first.
    pub fn retire_repository(&mut self, repository: &str) {
        self.repository_owners.remove(repository);
        let candidates: std::collections::HashSet<_> = self
            .manifests
            .iter()
            .filter(|(_, manifest)| manifest.repository == repository)
            .flat_map(|(_, manifest)| {
                manifest
                    .referenced_digests()
                    .into_iter()
                    .map(|digest| digest.0.clone())
            })
            .collect();
        self.manifests
            .retain(|(_, manifest)| manifest.repository != repository);
        let prefix = format!("{repository}:");
        self.tags
            .retain(|(reference, _)| !reference.starts_with(&prefix));
        let referenced = self.referenced_digest_set();
        // Otherwise the normal last-copy guard would preserve unreferenced test
        // bytes forever. This changes metadata only; blob GC still rechecks refs.
        self.layer_locations
            .retain(|(digest, _)| !candidates.contains(digest) || referenced.contains(digest));
    }

    /// Serialise the catalog to a JSON file crash-safely (REG5): write to
    /// a *unique* temp file, fsync its bytes, rename over the target, then
    /// fsync the parent directory so the rename itself is durable.
    ///
    /// A crash between the write and the rename leaves the old catalogue
    /// intact — never a torn half-written file that would then fail to load
    /// and orphan every stored blob. The temp name carries a random suffix
    /// so two concurrent persists can't share (and clobber) one temp path.
    ///
    /// Single-node mode has no Raft to remember the catalog, and even
    /// cluster nodes want their local holder view back after a restart.
    pub fn persist_to(&self, path: &std::path::Path) -> Result<(), PickleError> {
        let json = serde_json::to_vec_pretty(self)
            .map_err(|error| PickleError::CatalogPersist(error.to_string()))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent)
            .map_err(|error| PickleError::CatalogPersist(error.to_string()))?;
        crate::sesame::identity::atomic_write_mode(path, &json, Some(0o600))
            .map_err(|error| PickleError::CatalogPersist(error.to_string()))
    }

    /// Load a catalog previously written by [`Self::persist_to`]. A missing
    /// file yields an empty catalog (fresh node); a corrupt file is an
    /// error — silently starting empty would orphan every stored blob.
    pub fn load_from(path: &std::path::Path) -> Result<Self, PickleError> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| PickleError::CatalogPersist(format!("corrupt catalog: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(PickleError::CatalogPersist(e.to_string())),
        }
    }

    /// Apply an AttachSignature (set the signature on an existing
    /// manifest). Returns `false` when the digest is unknown, so the
    /// state machine can refuse instead of no-opping (JOB7).
    pub fn apply_attach_signature(&mut self, attach: &AttachSignature) -> bool {
        let mut found = false;
        for (_, manifest) in self
            .manifests
            .iter_mut()
            .filter(|(digest, _)| digest == &attach.manifest_digest.0)
        {
            manifest.signature = Some(attach.signature.clone());
            found = true;
        }
        found
    }

    /// Apply a DeleteTag.
    pub fn apply_delete_tag(&mut self, delete: &DeleteTag) {
        let tag_key = format!("{}:{}", delete.repository, delete.tag);

        // Find the digest this tag pointed to
        let digest = self
            .tags
            .iter()
            .find(|(k, _)| k == &tag_key)
            .map(|(_, v)| v.clone());

        // Remove the tag
        self.tags.retain(|(k, _)| k != &tag_key);

        if let Some(digest_str) = digest {
            for (_, manifest) in self.manifests.iter_mut().filter(|(digest, manifest)| {
                digest == &digest_str && manifest.repository == delete.repository
            }) {
                manifest.tags.remove(&delete.tag);
            }
            self.manifests.retain(|(digest, manifest)| {
                digest != &digest_str
                    || manifest.repository != delete.repository
                    || !manifest.tags.is_empty()
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Image signing
// ---------------------------------------------------------------------------

/// A cryptographic signature over an image manifest digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageSignature {
    /// How the image was signed.
    pub method: SigningMethod,
    /// Base64-encoded ECDSA P-256 signature over the manifest digest string.
    pub signature: String,
    /// Material needed to verify the signature.
    pub verification_material: VerificationMaterial,
    /// When the signature was created.
    pub signed_at: SystemTime,
}

/// How an image was signed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SigningMethod {
    /// Keyless signing via workload identity OIDC token.
    /// The build job's SPIFFE identity serves as the signing credential.
    Keyless {
        /// OIDC issuer URL (e.g. "<https://prod.reliaburger.dev>").
        issuer: String,
        /// SPIFFE URI of the signing workload.
        identity: String,
    },
    /// External key-based signing (cosign-compatible).
    ExternalKey {
        /// Identifier for the signing key (matches trust policy).
        key_id: String,
    },
}

/// Material needed to verify an image signature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VerificationMaterial {
    /// DER-encoded X.509 certificate chain (leaf, intermediate, root).
    CertificateChain(Vec<Vec<u8>>),
    /// DER-encoded ECDSA P-256 public key.
    PublicKey(Vec<u8>),
}

/// Attach a signature to an existing manifest in Raft.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachSignature {
    /// Digest of the manifest to sign.
    pub manifest_digest: Digest,
    /// The signature to attach.
    pub signature: ImageSignature,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from Pickle operations.
#[derive(Debug, thiserror::Error)]
pub enum PickleError {
    /// Lease authority or exact repository ownership could not be established.
    #[error("repository lease denied: {0}")]
    LeaseDenied(String),
    #[error("invalid digest: {0}")]
    InvalidDigest(String),
    #[error("blob not found: {0}")]
    BlobNotFound(Digest),
    #[error("manifest not found: {repository}:{tag}")]
    ManifestNotFound { repository: String, tag: String },
    #[error("missing layer: {0}")]
    MissingLayer(Digest),
    #[error("upload session not found: {0}")]
    UploadNotFound(String),
    #[error("invalid upload id: {0}")]
    InvalidUploadId(String),
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },
    #[error("replication failed: {0}")]
    ReplicationFailed(String),
    #[error("catalog persistence failed: {0}")]
    CatalogPersist(String),
    #[error("signature verification failed: {0}")]
    SignatureError(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_lease_generation_survives_catalogue_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalogue.json");
        let catalog: ManifestCatalog = serde_json::from_value(serde_json::json!({
            "manifests": [], "tags": [], "layer_locations": [],
            "repository_owners": {"rbtest-run1/web": "run1"}
        }))
        .unwrap();
        catalog.persist_to(&path).unwrap();
        let reloaded = ManifestCatalog::load_from(&path).unwrap();
        assert_eq!(
            serde_json::to_value(reloaded).unwrap()["repository_owners"]["rbtest-run1/web"],
            "run1"
        );
    }

    #[test]
    fn repository_generation_refuses_stale_cleanup_and_unowned_metadata() {
        let mut catalog = ManifestCatalog::default();
        catalog.claim_repository("rbtest-run1/web", "run1").unwrap();
        catalog.claim_repository("rbtest-run1/web", "run1").unwrap();
        assert!(catalog.claim_repository("rbtest-run1/web", "run2").is_err());
        assert!(
            catalog
                .retire_leased_repository("rbtest-run1/web", "run2")
                .is_err()
        );
        assert_eq!(catalog.repository_owners["rbtest-run1/web"], "run1");
        catalog
            .retire_leased_repository("rbtest-run1/web", "run1")
            .unwrap();
        catalog
            .retire_leased_repository("rbtest-run1/web", "run1")
            .unwrap();
        catalog.claim_repository("rbtest-run1/web", "run2").unwrap();
        assert!(
            catalog
                .retire_leased_repository("rbtest-run1/web", "run1")
                .is_err()
        );
        assert_eq!(catalog.repository_owners["rbtest-run1/web"], "run2");
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: test_manifest("rbtest-legacy/web", "a"),
            tag: "latest".into(),
            holder_nodes: BTreeSet::from([1]),
        });
        assert!(
            catalog
                .claim_repository("rbtest-legacy/web", "run1")
                .is_err()
        );
        assert!(
            catalog
                .retire_leased_repository("rbtest-legacy/web", "run1")
                .is_err()
        );
        assert!(
            catalog
                .get_manifest_by_tag("rbtest-legacy/web", "latest")
                .is_some()
        );
        assert!(catalog.claim_repository("ordinary", "run1").is_err());
    }

    /// L10 regression: the catalog used to be `default()` on every
    /// boot, so image metadata evaporated on restart.
    #[test]
    fn catalog_persists_and_reloads_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");

        let mut catalog = ManifestCatalog::default();
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: vec![(
                Digest(format!("sha256:{:0>64}", "cafe")),
                BTreeSet::from([3, 9]),
            )],
        });
        catalog.persist_to(&path).unwrap();

        let reloaded = ManifestCatalog::load_from(&path).unwrap();
        assert_eq!(
            reloaded.layer_holders(&format!("sha256:{:0>64}", "cafe")),
            BTreeSet::from([3, 9])
        );
    }

    /// REG5: a durable catalogue persist leaves no torn file and no
    /// leftover temp — the reload sees exactly what was written, and a
    /// crash between temp-write and rename would have kept the old file.
    #[test]
    fn catalog_persist_leaves_no_temp_and_reloads_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");

        let mut catalog = ManifestCatalog::default();
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: vec![(
                Digest(format!("sha256:{:0>64}", "feed")),
                BTreeSet::from([1, 2]),
            )],
        });
        catalog.persist_to(&path).unwrap();

        // No stray temp files alongside the catalogue.
        let temps: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(temps.is_empty(), "persist left a temp file: {temps:?}");

        let reloaded = ManifestCatalog::load_from(&path).unwrap();
        assert_eq!(
            reloaded.layer_holders(&format!("sha256:{:0>64}", "feed")),
            BTreeSet::from([1, 2])
        );
    }

    /// REG5: an old catalogue survives a crash during a re-persist. A
    /// leftover temp from an interrupted persist must never be mistaken
    /// for the catalogue — the committed file still loads.
    #[test]
    fn catalog_persist_torn_write_keeps_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");

        let mut catalog = ManifestCatalog::default();
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: vec![(Digest(format!("sha256:{:0>64}", "aa")), BTreeSet::from([1]))],
        });
        catalog.persist_to(&path).unwrap();

        // Simulate an interrupted re-persist: a stray temp alongside.
        std::fs::write(
            dir.path().join(format!("catalog.{:032x}.json.tmp", 0u128)),
            b"half-written {{{",
        )
        .unwrap();

        let reloaded = ManifestCatalog::load_from(&path).unwrap();
        assert_eq!(
            reloaded.layer_holders(&format!("sha256:{:0>64}", "aa")),
            BTreeSet::from([1])
        );
    }

    #[test]
    fn catalog_load_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ManifestCatalog::load_from(&dir.path().join("nope.json")).unwrap();
        assert!(catalog.manifests.is_empty());
    }

    #[test]
    fn catalog_load_corrupt_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.json");
        std::fs::write(&path, b"not json at all {{{").unwrap();
        // Silently starting empty would orphan every stored blob.
        assert!(ManifestCatalog::load_from(&path).is_err());
    }

    /// REG7/D11: the GC apply rechecks the full catalogue reference set at
    /// approval time. A blob nominated as an orphan, but re-referenced by a
    /// manifest committed between nomination and approval, must NOT be
    /// deleted — even though its holder set would otherwise allow it.
    #[test]
    fn gc_report_apply_refuses_a_still_referenced_blob() {
        let mut catalog = ManifestCatalog::default();
        let manifest = test_manifest("myapp", "mfst1");
        let layer = manifest.layers[0].digest.clone();

        // Two nodes hold the layer, so holder bookkeeping alone would
        // approve a deletion — but the manifest still references it.
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2]),
        });

        let approved = catalog.apply_gc_report(&GcReport {
            node_id: 1,
            deleted_layers: vec![layer.clone()],
        });

        assert!(
            approved.is_empty(),
            "a catalogued manifest's layer must never be approved for deletion"
        );
        // And the holder set is untouched — the layer stays put.
        assert_eq!(
            catalog.layer_holders(layer.as_str()),
            BTreeSet::from([1, 2])
        );
    }

    #[test]
    fn gc_reapproval_preserves_the_last_holder_and_rechecks_new_references() {
        let mut catalog = ManifestCatalog::default();
        let manifest = test_manifest("ordinary", "mfst1");
        let layer = manifest.layers[0].digest.clone();
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: vec![(layer.clone(), BTreeSet::from([1, 2]))],
        });
        let report = GcReport {
            node_id: 1,
            deleted_layers: vec![layer.clone()],
        };
        assert_eq!(catalog.apply_gc_report(&report), vec![layer.clone()]);
        // Simulate restart after approval but before physical deletion.
        let mut catalog: ManifestCatalog =
            serde_json::from_slice(&serde_json::to_vec(&catalog).unwrap()).unwrap();
        assert_eq!(catalog.apply_gc_report(&report), vec![layer.clone()]);
        assert!(
            catalog
                .apply_gc_report(&GcReport {
                    node_id: 2,
                    deleted_layers: vec![layer.clone()]
                })
                .is_empty()
        );
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest,
            tag: "latest".into(),
            holder_nodes: BTreeSet::from([2]),
        });
        assert!(catalog.apply_gc_report(&report).is_empty());
        assert_eq!(catalog.layer_holders(layer.as_str()), BTreeSet::from([2]));
    }

    #[test]
    fn gc_report_apply_rejects_last_copy_and_approves_orphans() {
        let mut catalog = ManifestCatalog::default();
        let tracked = Digest(format!("sha256:{:0>64}", "aa"));
        let orphan = Digest(format!("sha256:{:0>64}", "bb"));
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: vec![(tracked.clone(), BTreeSet::from([1]))],
        });

        let approved = catalog.apply_gc_report(&GcReport {
            node_id: 1,
            deleted_layers: vec![tracked.clone(), orphan.clone()],
        });

        // The sole tracked copy is refused; the untracked orphan passes.
        assert_eq!(approved, vec![orphan]);
        assert_eq!(
            catalog.layer_holders(tracked.as_str()),
            BTreeSet::from([1]),
            "sole holder must be preserved"
        );
    }

    #[test]
    fn digest_new_valid() {
        let d =
            Digest::new("sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
                .unwrap();
        assert_eq!(
            d.hex(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn digest_new_missing_prefix() {
        assert!(Digest::new("abcdef01234567890123456789012345").is_err());
    }

    #[test]
    fn digest_new_wrong_algorithm() {
        assert!(
            Digest::new("md5:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
                .is_err()
        );
    }

    #[test]
    fn digest_new_wrong_length() {
        assert!(Digest::new("sha256:abcdef").is_err());
    }

    #[test]
    fn digest_new_invalid_hex() {
        assert!(
            Digest::new("sha256:gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg")
                .is_err()
        );
    }

    #[test]
    fn digest_new_empty() {
        assert!(Digest::new("").is_err());
    }

    #[test]
    fn digest_display_abbreviated() {
        let d = Digest::from_sha256_hex(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );
        let display = format!("{d}");
        assert_eq!(display, "sha256:abcdef012345...");
    }

    #[test]
    fn digest_from_sha256_hex() {
        let d = Digest::from_sha256_hex(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            d.as_str(),
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn digest_serde_round_trip() {
        let d = Digest::from_sha256_hex(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        );
        let json = serde_json::to_string(&d).unwrap();
        let decoded: Digest = serde_json::from_str(&json).unwrap();
        assert_eq!(d, decoded);
    }

    fn test_digest(suffix: &str) -> Digest {
        Digest(format!("sha256:{suffix:0>64}"))
    }

    fn test_layer(suffix: &str, size: u64) -> LayerDescriptor {
        LayerDescriptor {
            digest: test_digest(suffix),
            size,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
        }
    }

    #[test]
    fn repository_retirement_preserves_shared_references_and_unpins_exclusive_orphans() {
        let mut catalog = ManifestCatalog::default();
        let mut ordinary = test_manifest("ordinary", "a");
        let mut owned = ordinary.clone();
        owned.repository = "rbtest-run1/web".into();
        let unique = test_manifest("rbtest-run1/web", "b");
        ordinary.signature = Some(ImageSignature {
            method: SigningMethod::ExternalKey {
                key_id: "test".into(),
            },
            signature: "signature".into(),
            verification_material: VerificationMaterial::PublicKey(vec![1]),
            signed_at: std::time::SystemTime::UNIX_EPOCH,
        });
        for (manifest, tag) in [
            (ordinary.clone(), "latest"),
            (owned, "shared"),
            (unique.clone(), "unique"),
        ] {
            catalog.apply_manifest_commit(&ManifestCommit {
                observed_gc_generation: 0,
                manifest,
                tag: tag.into(),
                holder_nodes: BTreeSet::from([1]),
            });
        }
        // Digest-addressed publication uses a reference containing a colon.
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: unique.clone(),
            tag: unique.digest.as_str().into(),
            holder_nodes: BTreeSet::from([1]),
        });
        catalog.retire_repository("rbtest-run1/web");
        assert!(catalog.tags_for_repository("rbtest-run1/web").is_empty());
        assert!(
            catalog
                .get_repository_manifest("rbtest-run1/web", unique.digest.as_str())
                .is_none()
        );
        assert!(
            catalog
                .get_manifest_by_tag("ordinary", "latest")
                .unwrap()
                .signature
                .is_some()
        );
        let all = catalog.referenced_digest_set();
        let unique_digest = unique.digest.clone();
        assert!(!all.contains(unique_digest.as_str()));
        let shared_digest = ordinary.digest.clone();
        let approved = catalog.apply_gc_report(&GcReport {
            node_id: 1,
            deleted_layers: vec![unique_digest.clone(), shared_digest],
        });
        assert_eq!(approved, vec![unique_digest]);
        let saved = catalog.clone();
        catalog.retire_repository("rbtest-run1/web");
        assert_eq!(
            serde_json::to_value(&catalog).unwrap(),
            serde_json::to_value(saved).unwrap()
        );
    }

    fn test_manifest(repo: &str, digest_suffix: &str) -> ImageManifest {
        ImageManifest {
            digest: test_digest(digest_suffix),
            config: test_layer("cfg1", 1024),
            layers: vec![test_layer("layer1", 10240), test_layer("layer2", 20480)],
            repository: repo.to_string(),
            tags: BTreeSet::new(),
            total_size: 31744,
            pushed_at: SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None,
        }
    }

    #[test]
    fn repository_views_preserve_shared_holders_without_other_repository_metadata() {
        let mut catalog = ManifestCatalog::default();
        for (repository, digest, holder) in [
            ("rbtest-a/web", "shared", 1),
            ("ordinary", "shared", 2),
            ("rbtest-b/web", "other", 3),
        ] {
            catalog.apply_manifest_commit(&ManifestCommit {
                observed_gc_generation: 0,
                manifest: test_manifest(repository, digest),
                tag: "latest".into(),
                holder_nodes: BTreeSet::from([holder]),
            });
        }
        catalog
            .repository_owners
            .insert("rbtest-a/web".into(), "a".into());
        catalog
            .repository_owners
            .insert("rbtest-b/web".into(), "b".into());
        let view = catalog.repository_view("rbtest-a/web");
        assert_eq!(view.manifests.len(), 1);
        assert_eq!(view.tags.len(), 1);
        assert_eq!(
            view.repository_owners,
            BTreeMap::from([("rbtest-a/web".into(), "a".into())])
        );
        assert!(view.get_manifest_by_tag("ordinary", "latest").is_none());
        assert!(view.get_manifest_by_tag("rbtest-b/web", "latest").is_none());
        assert!(
            !view
                .layer_locations
                .iter()
                .any(|(digest, _)| digest == test_digest("other").as_str())
        );
        assert_eq!(
            view.layer_holders(test_digest("shared").as_str()),
            BTreeSet::from([1, 2])
        );
        assert_eq!(catalog.stored_sizes("rbtest-a/web"), (31744, 3 * 31744));
        let empty = catalog.repository_view("absent");
        assert!(
            empty.manifests.is_empty()
                && empty.tags.is_empty()
                && empty.layer_locations.is_empty()
                && empty.repository_owners.is_empty()
        );
    }

    #[test]
    fn image_manifest_all_digests() {
        let m = test_manifest("myapp", "mfst1");
        let digests = m.all_digests();
        assert_eq!(digests.len(), 3); // config + 2 layers
    }

    #[test]
    fn referenced_digests_include_the_manifest_blob() {
        let m = test_manifest("myapp", "mfst1");
        let digests = m.referenced_digests();
        assert_eq!(digests.len(), 4); // manifest + config + 2 layers
        assert!(digests.contains(&&m.digest));
    }

    /// REG1: an index pseudo-manifest uses its own blob as the config
    /// descriptor — the digest must not be pinned twice.
    #[test]
    fn referenced_digests_deduplicate_self_referencing_config() {
        let mut m = test_manifest("myapp", "mfst1");
        m.config = LayerDescriptor {
            digest: m.digest.clone(),
            size: 100,
            media_type: "application/vnd.oci.image.index.v1+json".to_string(),
        };
        m.layers.clear();
        assert_eq!(m.referenced_digests(), vec![&m.digest]);
    }

    /// REG1: committing a manifest must record the pusher as a holder
    /// of the manifest's own blob, not just its config and layers —
    /// otherwise GC sees it as an orphan and the heal loop never
    /// replicates it.
    #[test]
    fn manifest_commit_records_holders_for_the_manifest_blob() {
        let mut catalog = ManifestCatalog::default();
        let m = test_manifest("myapp", "mfst1");

        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2]),
        });

        assert_eq!(
            catalog.layer_holders(m.digest.as_str()),
            BTreeSet::from([1, 2]),
            "the manifest's own blob must have holders"
        );
    }

    #[test]
    fn image_manifest_serde_round_trip() {
        let m = test_manifest("myapp", "mfst1");
        let json = serde_json::to_string(&m).unwrap();
        let decoded: ImageManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, decoded);
    }

    #[test]
    fn manifest_catalog_commit_and_lookup() {
        let mut catalog = ManifestCatalog::default();
        let manifest = test_manifest("myapp", "mfst1");

        let commit = ManifestCommit {
            observed_gc_generation: 0,
            manifest: manifest.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2]),
        };
        catalog.apply_manifest_commit(&commit);

        let found = catalog.get_manifest_by_tag("myapp", "latest").unwrap();
        assert_eq!(found.digest, manifest.digest);
        assert!(found.tags.contains("latest"));
    }

    #[test]
    fn manifest_catalog_tag_update_changes_digest() {
        let mut catalog = ManifestCatalog::default();

        let m1 = test_manifest("myapp", "mfst1");
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m1.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        let m2 = test_manifest("myapp", "mfst2");
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m2.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        let found = catalog.get_manifest_by_tag("myapp", "latest").unwrap();
        assert_eq!(found.digest, m2.digest);
    }

    #[test]
    fn manifest_catalog_tags_for_repository() {
        let mut catalog = ManifestCatalog::default();
        let m = test_manifest("myapp", "mfst1");

        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m,
            tag: "v1.0".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        let tags = catalog.tags_for_repository("myapp");
        assert_eq!(tags.len(), 2);
        assert!(tags.contains(&"latest".to_string()));
        assert!(tags.contains(&"v1.0".to_string()));
    }

    #[test]
    fn identical_manifests_keep_repository_ownership_through_retirement() {
        let mut catalog = ManifestCatalog::default();
        let ordinary = test_manifest("production/app", "shared-manifest");
        let mut leased = ordinary.clone();
        leased.repository = "rbtest-owned/app".into();
        for manifest in [ordinary.clone(), leased] {
            catalog.apply_manifest_commit(&ManifestCommit {
                observed_gc_generation: 0,
                manifest,
                tag: "latest".into(),
                holder_nodes: BTreeSet::from([1]),
            });
        }
        assert_eq!(catalog.manifests.len(), 2);
        for repository in ["production/app", "rbtest-owned/app"] {
            let manifest = catalog.get_manifest_by_tag(repository, "latest").unwrap();
            assert_eq!(manifest.repository, repository);
            assert_eq!(manifest.tags, BTreeSet::from(["latest".into()]));
        }
        // An operator/test tag deletion must retire only its repository's row.
        catalog.apply_delete_tag(&DeleteTag {
            repository: "rbtest-owned/app".into(),
            tag: "latest".into(),
        });
        assert_eq!(catalog.manifests.len(), 1);
        let retained = catalog
            .get_manifest_by_tag("production/app", "latest")
            .unwrap();
        assert_eq!(retained.repository, "production/app");
        assert_eq!(retained.tags, BTreeSet::from(["latest".into()]));
        assert!(
            catalog
                .get_manifest_by_tag("rbtest-owned/app", "latest")
                .is_none()
        );
        assert!(
            catalog
                .apply_gc_report(&GcReport {
                    node_id: 1,
                    deleted_layers: ordinary.referenced_digests().into_iter().cloned().collect(),
                })
                .is_empty()
        );
    }

    #[test]
    fn moving_a_tag_preserves_the_verified_digest_in_its_repository() {
        let mut catalog = ManifestCatalog::default();
        let original = test_manifest("one", "original");
        let mut other = original.clone();
        other.repository = "two".into();
        for manifest in [original.clone(), other, test_manifest("one", "replacement")] {
            catalog.apply_manifest_commit(&ManifestCommit {
                observed_gc_generation: 0,
                manifest,
                tag: "latest".into(),
                holder_nodes: BTreeSet::from([1]),
            });
        }
        assert_eq!(catalog.manifests.len(), 3);
        assert!(
            catalog
                .get_repository_manifest("one", original.digest.as_str())
                .unwrap()
                .tags
                .is_empty()
        );
        assert_eq!(
            catalog.get_manifest_by_tag("two", "latest").unwrap().digest,
            original.digest
        );
        assert_ne!(
            catalog.get_manifest_by_tag("one", "latest").unwrap().digest,
            original.digest
        );
    }

    #[test]
    fn content_signatures_survive_repository_copies_and_tag_refresh() {
        let mut catalog = ManifestCatalog::default();
        let original = test_manifest("one", "signed");
        for repository in ["one", "two", "three", "one"] {
            let mut manifest = original.clone();
            manifest.repository = repository.into();
            catalog.apply_manifest_commit(&ManifestCommit {
                observed_gc_generation: 0,
                manifest,
                tag: "latest".into(),
                holder_nodes: BTreeSet::from([1]),
            });
            if repository == "two" {
                assert!(catalog.apply_attach_signature(&AttachSignature {
                    manifest_digest: original.digest.clone(),
                    signature: test_signature(),
                }));
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalog.json");
        catalog.persist_to(&path).unwrap();
        let recovered = ManifestCatalog::load_from(&path).unwrap();
        assert_eq!(recovered.manifests.len(), 3);
        for repository in ["one", "two", "three"] {
            let manifest = recovered.get_manifest_by_tag(repository, "latest").unwrap();
            assert_eq!(manifest.repository, repository);
            assert!(manifest.signature.is_some());
        }
    }

    #[test]
    fn manifest_catalog_layer_holders() {
        let mut catalog = ManifestCatalog::default();
        let m = test_manifest("myapp", "mfst1");

        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2, 3]),
        });

        let holders = catalog.layer_holders(m.layers[0].digest.as_str());
        assert_eq!(holders, BTreeSet::from([1, 2, 3]));
    }

    #[test]
    fn manifest_catalog_gc_report_removes_holder() {
        let mut catalog = ManifestCatalog::default();

        // An *unreferenced* multi-holder layer (no manifest pins it), so
        // the REG7 catalogue recheck doesn't protect it and the holder
        // bookkeeping is exercised on its own.
        let orphan = test_digest("looselayer");
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: vec![(orphan.clone(), BTreeSet::from([1, 2, 3]))],
        });

        catalog.apply_gc_report(&GcReport {
            node_id: 2,
            deleted_layers: vec![orphan.clone()],
        });

        let holders = catalog.layer_holders(orphan.as_str());
        assert_eq!(holders, BTreeSet::from([1, 3]));
    }

    #[test]
    fn manifest_catalog_delete_tag_removes_manifest_when_no_tags() {
        let mut catalog = ManifestCatalog::default();
        let m = test_manifest("myapp", "mfst1");

        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        catalog.apply_delete_tag(&DeleteTag {
            repository: "myapp".to_string(),
            tag: "latest".to_string(),
        });

        assert!(catalog.get_manifest_by_tag("myapp", "latest").is_none());
        assert!(catalog.get_manifest(m.digest.as_str()).is_none());
    }

    #[test]
    fn manifest_catalog_delete_tag_keeps_manifest_with_other_tags() {
        let mut catalog = ManifestCatalog::default();
        let m = test_manifest("myapp", "mfst1");

        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m.clone(),
            tag: "v1.0".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        catalog.apply_delete_tag(&DeleteTag {
            repository: "myapp".to_string(),
            tag: "latest".to_string(),
        });

        assert!(catalog.get_manifest_by_tag("myapp", "latest").is_none());
        assert!(catalog.get_manifest_by_tag("myapp", "v1.0").is_some());
        assert!(catalog.get_manifest(m.digest.as_str()).is_some());
    }

    #[test]
    fn manifest_catalog_update_layer_locations() {
        let mut catalog = ManifestCatalog::default();
        let digest = test_digest("layer1");

        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: vec![(digest.clone(), BTreeSet::from([1, 2]))],
        });

        assert_eq!(
            catalog.layer_holders(digest.as_str()),
            BTreeSet::from([1, 2])
        );

        // Overwrite with new set
        catalog.apply_update_locations(&UpdateLayerLocations {
            updates: vec![(digest.clone(), BTreeSet::from([3, 4]))],
        });

        assert_eq!(
            catalog.layer_holders(digest.as_str()),
            BTreeSet::from([3, 4])
        );
    }

    #[test]
    fn manifest_commit_serde_round_trip() {
        let commit = ManifestCommit {
            observed_gc_generation: 0,
            manifest: test_manifest("myapp", "mfst1"),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2]),
        };
        let json = serde_json::to_string(&commit).unwrap();
        let decoded: ManifestCommit = serde_json::from_str(&json).unwrap();
        assert_eq!(commit, decoded);
    }

    fn test_signature() -> ImageSignature {
        ImageSignature {
            method: SigningMethod::Keyless {
                issuer: "https://test.reliaburger.dev".to_string(),
                identity: "spiffe://test/ns/default/job/build".to_string(),
            },
            signature: "MEUCIQD...".to_string(),
            verification_material: VerificationMaterial::CertificateChain(vec![
                vec![1, 2, 3],
                vec![4, 5, 6],
            ]),
            signed_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn image_signature_serde_round_trip() {
        let sig = test_signature();
        let json = serde_json::to_string(&sig).unwrap();
        let decoded: ImageSignature = serde_json::from_str(&json).unwrap();
        assert_eq!(sig, decoded);
    }

    #[test]
    fn signing_method_keyless_serde() {
        let method = SigningMethod::Keyless {
            issuer: "https://prod.reliaburger.dev".to_string(),
            identity: "spiffe://prod/ns/ci/job/build-api".to_string(),
        };
        let json = serde_json::to_string(&method).unwrap();
        let decoded: SigningMethod = serde_json::from_str(&json).unwrap();
        assert_eq!(method, decoded);
        assert!(json.contains("Keyless"));
    }

    #[test]
    fn signing_method_external_key_serde() {
        let method = SigningMethod::ExternalKey {
            key_id: "cosign-key-abc123".to_string(),
        };
        let json = serde_json::to_string(&method).unwrap();
        let decoded: SigningMethod = serde_json::from_str(&json).unwrap();
        assert_eq!(method, decoded);
        assert!(json.contains("ExternalKey"));
    }

    #[test]
    fn manifest_with_signature_serde() {
        let mut m = test_manifest("myapp", "mfst1");
        m.signature = Some(test_signature());
        let json = serde_json::to_string(&m).unwrap();
        let decoded: ImageManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, decoded);
        assert!(decoded.signature.is_some());
    }

    #[test]
    fn manifest_catalog_attach_signature() {
        let mut catalog = ManifestCatalog::default();
        let m = test_manifest("myapp", "mfst1");
        catalog.apply_manifest_commit(&ManifestCommit {
            observed_gc_generation: 0,
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        assert!(
            catalog
                .get_manifest(m.digest.as_str())
                .unwrap()
                .signature
                .is_none()
        );

        assert!(catalog.apply_attach_signature(&AttachSignature {
            manifest_digest: m.digest.clone(),
            signature: test_signature(),
        }));

        let updated = catalog.get_manifest(m.digest.as_str()).unwrap();
        assert!(updated.signature.is_some());
    }

    #[test]
    fn manifest_catalog_attach_signature_missing_manifest_reports_failure() {
        let mut catalog = ManifestCatalog::default();
        // No manifest committed — attach reports the unknown digest so
        // the state machine can refuse it (JOB7).
        let attached = catalog.apply_attach_signature(&AttachSignature {
            manifest_digest: test_digest("nonexistent"),
            signature: test_signature(),
        });
        assert!(!attached);
        assert!(catalog.manifests.is_empty());
    }

    #[test]
    fn attach_signature_serde_round_trip() {
        let attach = AttachSignature {
            manifest_digest: test_digest("mfst1"),
            signature: test_signature(),
        };
        let json = serde_json::to_string(&attach).unwrap();
        let decoded: AttachSignature = serde_json::from_str(&json).unwrap();
        assert_eq!(attach, decoded);
    }
}
