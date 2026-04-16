//! Garbage collection for Pickle.
//!
//! Runs periodically on each node to reclaim disk space from unreferenced
//! image layers. Protections:
//! - Active images (referenced by apps in DesiredState) are never collected
//! - Sole-copy layers (only one node holds them) are never collected
//! - Retention window: unreferenced images are kept for `gc_retain_days`

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use super::store::BlobStore;
use super::types::{Digest, GcReport, ManifestCatalog};

/// Configuration for garbage collection.
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Days to retain unreferenced images before collection.
    pub retain_days: u32,
    /// This node's ID.
    pub node_id: u64,
}

/// The result of a GC sweep.
#[derive(Debug)]
pub struct GcResult {
    /// Layers deleted from this node.
    pub deleted: Vec<Digest>,
    /// Layers skipped due to sole-copy protection.
    pub sole_copy_protected: usize,
    /// Layers skipped due to active reference.
    pub active_protected: usize,
    /// Layers skipped due to retention window.
    pub retention_protected: usize,
    /// The GcReport to propose to Raft (if any layers were deleted).
    pub report: Option<GcReport>,
}

/// Run a garbage collection sweep.
///
/// Examines all blobs on the local store and determines which can be
/// safely deleted. Never deletes:
/// - Layers referenced by any manifest that has tags (active images)
/// - Layers referenced by apps in the active deployment set
/// - Layers where this node is the sole holder
/// - Layers from manifests pushed within the retention window
pub fn gc_sweep(
    store: &BlobStore,
    catalog: &ManifestCatalog,
    active_images: &HashSet<String>,
    config: &GcConfig,
) -> Result<GcResult, super::types::PickleError> {
    let local_blobs = store.list_blobs()?;
    let now = SystemTime::now();
    let retention_cutoff = now - Duration::from_secs(config.retain_days as u64 * 86400);

    // Build the set of digests that are protected
    let mut protected_digests: HashSet<String> = HashSet::new();

    let mut sole_copy_count = 0;
    let mut active_count = 0;
    let mut retention_count = 0;

    // Protect all layers from manifests that have tags or are actively deployed
    for (digest_str, manifest) in &catalog.manifests {
        let is_tagged = !manifest.tags.is_empty();
        let is_active = active_images.contains(digest_str)
            || manifest
                .tags
                .iter()
                .any(|t| active_images.contains(&format!("{}:{}", manifest.repository, t)));

        let within_retention = manifest.pushed_at >= retention_cutoff;

        if is_tagged || is_active || within_retention {
            // Protect all layers in this manifest
            for layer_digest in manifest.all_digests() {
                protected_digests.insert(layer_digest.0.clone());
            }
            if is_active {
                active_count += 1;
            }
            if within_retention && !is_active && !is_tagged {
                retention_count += 1;
            }
        }
    }

    // Check sole-copy protection for each local blob
    let mut candidates: Vec<Digest> = Vec::new();
    for blob_digest in &local_blobs {
        if protected_digests.contains(blob_digest.as_str()) {
            continue;
        }

        let holders = catalog.layer_holders(blob_digest.as_str());
        // Empty holders = orphaned blob not tracked in Raft (safe to delete)
        // Single holder = sole copy (never delete)
        if holders.len() == 1 {
            sole_copy_count += 1;
            continue;
        }

        candidates.push(blob_digest.clone());
    }

    // Delete candidates
    let mut deleted = Vec::new();
    for digest in &candidates {
        if let Err(e) = store.delete_blob(digest) {
            eprintln!("gc: failed to delete blob {digest}: {e}");
            continue;
        }
        deleted.push(digest.clone());
    }

    let report = if deleted.is_empty() {
        None
    } else {
        Some(GcReport {
            node_id: config.node_id,
            deleted_layers: deleted.clone(),
        })
    };

    Ok(GcResult {
        deleted,
        sole_copy_protected: sole_copy_count,
        active_protected: active_count,
        retention_protected: retention_count,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::types::{ImageManifest, LayerDescriptor, ManifestCatalog, ManifestCommit};
    use std::collections::BTreeSet;

    fn test_digest(suffix: &str) -> Digest {
        Digest(format!("sha256:{suffix:0>64}"))
    }

    fn test_layer(suffix: &str) -> LayerDescriptor {
        LayerDescriptor {
            digest: test_digest(suffix),
            size: 1024,
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
        }
    }

    fn test_manifest(repo: &str, digest_suffix: &str, pushed_at: SystemTime) -> ImageManifest {
        ImageManifest {
            digest: test_digest(digest_suffix),
            config: test_layer(&format!("cfg-{digest_suffix}")),
            layers: vec![test_layer(&format!("layer-{digest_suffix}"))],
            repository: repo.to_string(),
            tags: BTreeSet::new(),
            total_size: 2048,
            pushed_at,
            pushed_by: 1,
            signature: None,
        }
    }

    fn write_fake_blob(store: &BlobStore, digest: &Digest) {
        let path = store.blob_path(digest);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"data").unwrap();
    }

    fn default_config() -> GcConfig {
        GcConfig {
            retain_days: 0, // No retention for tests
            node_id: 1,
        }
    }

    #[test]
    fn gc_collects_unreferenced_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let catalog = ManifestCatalog::default();

        // Write an orphan blob (not in any manifest)
        let orphan = test_digest("orphan1");
        write_fake_blob(&store, &orphan);

        let result = gc_sweep(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert_eq!(result.deleted.len(), 1);
        assert!(!store.has_blob(&orphan));
    }

    #[test]
    fn gc_protects_tagged_manifest_layers() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        let manifest = test_manifest("myapp", "m1", SystemTime::UNIX_EPOCH);
        for d in manifest.all_digests() {
            write_fake_blob(&store, d);
        }

        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2]),
        });

        let result = gc_sweep(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert!(result.deleted.is_empty());
    }

    #[test]
    fn gc_protects_active_deployment_images() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        let manifest = test_manifest("myapp", "m1", SystemTime::UNIX_EPOCH);
        for d in manifest.all_digests() {
            write_fake_blob(&store, d);
        }

        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2]),
        });

        // Mark as actively deployed
        let active = HashSet::from(["myapp:latest".to_string()]);

        let result = gc_sweep(&store, &catalog, &active, &default_config()).unwrap();
        assert!(result.deleted.is_empty());
        assert!(result.active_protected > 0);
    }

    #[test]
    fn gc_protects_sole_copy() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        let orphan = test_digest("sole1");
        write_fake_blob(&store, &orphan);

        // Set up layer_locations: only this node holds it
        catalog.apply_update_locations(&crate::pickle::types::UpdateLayerLocations {
            updates: vec![(orphan.clone(), BTreeSet::from([1]))],
        });

        let result = gc_sweep(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert!(result.deleted.is_empty());
        assert_eq!(result.sole_copy_protected, 1);
        assert!(store.has_blob(&orphan));
    }

    #[test]
    fn gc_collects_multi_holder_unreferenced() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        let orphan = test_digest("multi1");
        write_fake_blob(&store, &orphan);

        // Two nodes hold this layer
        catalog.apply_update_locations(&crate::pickle::types::UpdateLayerLocations {
            updates: vec![(orphan.clone(), BTreeSet::from([1, 2]))],
        });

        let result = gc_sweep(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert_eq!(result.deleted.len(), 1);
        assert!(!store.has_blob(&orphan));
    }

    #[test]
    fn gc_report_contains_deleted_layers() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        let orphan = test_digest("report1");
        write_fake_blob(&store, &orphan);

        // Multiple holders so sole-copy doesn't protect
        catalog.apply_update_locations(&crate::pickle::types::UpdateLayerLocations {
            updates: vec![(orphan.clone(), BTreeSet::from([1, 2]))],
        });

        let result = gc_sweep(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        let report = result.report.unwrap();
        assert_eq!(report.node_id, 1);
        assert_eq!(report.deleted_layers.len(), 1);
    }

    #[test]
    fn gc_protects_within_retention_window() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        // Manifest pushed "now" with 7-day retention
        let manifest = test_manifest("myapp", "m1", SystemTime::now());
        for d in manifest.all_digests() {
            write_fake_blob(&store, d);
        }

        // Delete the tag so it's "unreferenced" but within retention
        catalog.apply_manifest_commit(&ManifestCommit {
            manifest: manifest.clone(),
            tag: "old".to_string(),
            holder_nodes: BTreeSet::from([1, 2]),
        });
        catalog.apply_delete_tag(&crate::pickle::types::DeleteTag {
            repository: "myapp".to_string(),
            tag: "old".to_string(),
        });

        // Wait, the manifest is gone now after delete_tag. Let me adjust:
        // Actually, we need a manifest that exists but has no tags and is recent.
        // Re-add the manifest without tags directly.
        catalog
            .manifests
            .push((manifest.digest.0.clone(), manifest.clone()));

        let config = GcConfig {
            retain_days: 7,
            node_id: 1,
        };
        let result = gc_sweep(&store, &catalog, &HashSet::new(), &config).unwrap();
        // Layers from the recently pushed manifest should be protected
        assert!(result.deleted.is_empty());
    }

    #[test]
    fn gc_empty_store_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let catalog = ManifestCatalog::default();

        let result = gc_sweep(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert!(result.deleted.is_empty());
        assert!(result.report.is_none());
    }
}
