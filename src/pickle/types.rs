//! Data types for the Pickle image registry.
//!
//! Defines digests, manifests, layer descriptors, and all the types
//! that flow through Raft for manifest catalog and layer location tracking.

use std::collections::BTreeSet;
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
    pub fn all_digests(&self) -> Vec<&Digest> {
        let mut digests = vec![&self.config.digest];
        for layer in &self.layers {
            digests.push(&layer.digest);
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

/// The manifest catalog stored in Raft as part of DesiredState.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestCatalog {
    /// Manifests keyed by digest string.
    pub manifests: Vec<(String, ImageManifest)>,
    /// Tag→digest mappings. Key is `"repository:tag"`, value is digest string.
    pub tags: Vec<(String, String)>,
    /// Layer→holders mappings. Key is digest string, value is set of node IDs.
    pub layer_locations: Vec<(String, BTreeSet<u64>)>,
}

impl ManifestCatalog {
    /// Look up a manifest by digest.
    pub fn get_manifest(&self, digest: &str) -> Option<&ImageManifest> {
        self.manifests
            .iter()
            .find(|(d, _)| d == digest)
            .map(|(_, m)| m)
    }

    /// Look up a manifest by repository and tag.
    pub fn get_manifest_by_tag(&self, repository: &str, tag: &str) -> Option<&ImageManifest> {
        let key = format!("{repository}:{tag}");
        let digest = self.tags.iter().find(|(k, _)| k == &key).map(|(_, v)| v)?;
        self.get_manifest(digest)
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

        // Remove old tag→digest mapping if tag existed
        self.tags.retain(|(k, _)| k != &tag_key);
        // Add new tag→digest
        self.tags.push((tag_key, digest_str.clone()));

        // Upsert the manifest (update tags if it already exists)
        if let Some((_, existing)) = self.manifests.iter_mut().find(|(d, _)| d == &digest_str) {
            existing.tags.insert(commit.tag.clone());
        } else {
            let mut manifest = commit.manifest.clone();
            manifest.tags.insert(commit.tag.clone());
            self.manifests.push((digest_str.clone(), manifest));
        }

        // Update layer locations for all layers in this manifest
        for layer in commit.manifest.all_digests() {
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

    /// Apply a GcReport (remove node from holder sets for deleted layers).
    pub fn apply_gc_report(&mut self, report: &GcReport) {
        for digest in &report.deleted_layers {
            let digest_str = &digest.0;
            if let Some((_, holders)) = self
                .layer_locations
                .iter_mut()
                .find(|(d, _)| d == digest_str)
            {
                holders.remove(&report.node_id);
            }
        }
    }

    /// Apply an AttachSignature (set the signature on an existing manifest).
    pub fn apply_attach_signature(&mut self, attach: &AttachSignature) {
        if let Some((_, manifest)) = self
            .manifests
            .iter_mut()
            .find(|(d, _)| d == &attach.manifest_digest.0)
        {
            manifest.signature = Some(attach.signature.clone());
        }
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

        // If the manifest has no remaining tags, remove it
        if let Some(digest_str) = digest {
            // Remove tag from the manifest's tag set
            if let Some((_, manifest)) = self.manifests.iter_mut().find(|(d, _)| d == &digest_str) {
                manifest.tags.remove(&delete.tag);
            }

            // Check if any other tags still reference this digest
            let still_referenced = self.tags.iter().any(|(_, v)| v == &digest_str);
            if !still_referenced {
                self.manifests.retain(|(d, _)| d != &digest_str);
            }
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
        /// OIDC issuer URL (e.g. "https://prod.reliaburger.dev").
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
    #[error("digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },
    #[error("replication failed: {0}")]
    ReplicationFailed(String),
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
    fn image_manifest_all_digests() {
        let m = test_manifest("myapp", "mfst1");
        let digests = m.all_digests();
        assert_eq!(digests.len(), 3); // config + 2 layers
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
            manifest: m1.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });

        let m2 = test_manifest("myapp", "mfst2");
        catalog.apply_manifest_commit(&ManifestCommit {
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
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });
        catalog.apply_manifest_commit(&ManifestCommit {
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
    fn manifest_catalog_layer_holders() {
        let mut catalog = ManifestCatalog::default();
        let m = test_manifest("myapp", "mfst1");

        catalog.apply_manifest_commit(&ManifestCommit {
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
        let m = test_manifest("myapp", "mfst1");

        catalog.apply_manifest_commit(&ManifestCommit {
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2, 3]),
        });

        catalog.apply_gc_report(&GcReport {
            node_id: 2,
            deleted_layers: vec![m.layers[0].digest.clone()],
        });

        let holders = catalog.layer_holders(m.layers[0].digest.as_str());
        assert_eq!(holders, BTreeSet::from([1, 3]));
    }

    #[test]
    fn manifest_catalog_delete_tag_removes_manifest_when_no_tags() {
        let mut catalog = ManifestCatalog::default();
        let m = test_manifest("myapp", "mfst1");

        catalog.apply_manifest_commit(&ManifestCommit {
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
            manifest: m.clone(),
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1]),
        });
        catalog.apply_manifest_commit(&ManifestCommit {
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

        catalog.apply_attach_signature(&AttachSignature {
            manifest_digest: m.digest.clone(),
            signature: test_signature(),
        });

        let updated = catalog.get_manifest(m.digest.as_str()).unwrap();
        assert!(updated.signature.is_some());
    }

    #[test]
    fn manifest_catalog_attach_signature_missing_manifest_is_noop() {
        let mut catalog = ManifestCatalog::default();
        // No manifest committed — attach should be a no-op
        catalog.apply_attach_signature(&AttachSignature {
            manifest_digest: test_digest("nonexistent"),
            signature: test_signature(),
        });
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
