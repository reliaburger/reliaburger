//! Garbage collection for Pickle.
//!
//! GC is two-phase (M2): a node first *nominates* deletion candidates
//! with [`gc_candidates`] — deleting nothing — then submits them to an
//! arbiter, and finally deletes only what was approved with
//! [`delete_approved`]. In cluster mode the arbiter is the Raft state
//! machine (`ManifestCatalog::apply_gc_report` runs serialised through
//! the log, so two nodes racing to delete the last two copies of a
//! layer cannot both win). Single-node mode applies the same rule to
//! its local catalog.
//!
//! Candidate protections:
//! - Active images (referenced by deployed apps) are never nominated
//! - Tagged manifests' layers are never nominated
//! - Sole-copy layers are never nominated (the arbiter re-checks)
//! - Retention window: unreferenced images kept for `retain_days`
//! - Orphan grace: untracked blobs younger than `orphan_grace` are
//!   kept — a blob mid-push has no holder entry until its manifest
//!   commits, and must not be swept out from under the upload

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
    /// Minimum on-disk age before an untracked (orphan) blob becomes
    /// a candidate. Protects blobs belonging to in-flight pushes.
    pub orphan_grace: Duration,
}

/// Deletion candidates nominated by [`gc_candidates`].
///
/// Nothing has been deleted at this point; the candidates must go to
/// the arbiter first.
#[derive(Debug)]
pub struct GcCandidates {
    /// Layers this node proposes to delete.
    pub candidates: Vec<Digest>,
    /// Layers skipped due to sole-copy protection.
    pub sole_copy_protected: usize,
    /// Layers skipped due to active reference.
    pub active_protected: usize,
    /// Layers skipped due to retention window.
    pub retention_protected: usize,
    /// Untracked blobs skipped because they're within the orphan grace
    /// window (likely mid-push).
    pub orphan_grace_protected: usize,
}

impl GcCandidates {
    /// The report to submit to the arbiter, or `None` when there is
    /// nothing to nominate.
    pub fn report(&self, node_id: u64) -> Option<GcReport> {
        if self.candidates.is_empty() {
            None
        } else {
            Some(GcReport {
                node_id,
                deleted_layers: self.candidates.clone(),
            })
        }
    }
}

/// Phase 1: nominate deletion candidates. Deletes nothing.
pub fn gc_candidates(
    store: &BlobStore,
    catalog: &ManifestCatalog,
    active_images: &HashSet<String>,
    config: &GcConfig,
) -> Result<GcCandidates, super::types::PickleError> {
    let local_blobs = store.list_blobs()?;
    let now = SystemTime::now();
    let retention_cutoff = now - Duration::from_secs(config.retain_days as u64 * 86400);

    // Build the set of digests that are protected
    let mut protected_digests: HashSet<String> = HashSet::new();

    let mut sole_copy_count = 0;
    let mut active_count = 0;
    let mut retention_count = 0;
    let mut orphan_grace_count = 0;

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
            // Protect everything this manifest pins, its own blob
            // included (REG1) — the catalogue must keep serving the
            // manifest bytes, not just the layers.
            for layer_digest in manifest.referenced_digests() {
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

    let mut candidates: Vec<Digest> = Vec::new();
    for blob_digest in &local_blobs {
        if protected_digests.contains(blob_digest.as_str()) {
            continue;
        }

        let holders = catalog.layer_holders(blob_digest.as_str());
        if holders.is_empty() {
            // Untracked blob: an orphan, unless it's a push still in
            // flight — its manifest hasn't committed yet, so give it a
            // grace window before nominating.
            if blob_age(store, blob_digest, now) < config.orphan_grace {
                orphan_grace_count += 1;
                continue;
            }
        } else if holders.len() == 1 {
            // Sole copy: never nominate. The arbiter re-checks this
            // authoritatively; skipping here just avoids pointless
            // round-trips.
            sole_copy_count += 1;
            continue;
        }

        candidates.push(blob_digest.clone());
    }

    Ok(GcCandidates {
        candidates,
        sole_copy_protected: sole_copy_count,
        active_protected: active_count,
        retention_protected: retention_count,
        orphan_grace_protected: orphan_grace_count,
    })
}

/// Phase 2: physically delete the blobs the arbiter approved.
///
/// Per-blob failures are logged and skipped — a blob that fails to
/// delete stays on disk and gets re-nominated next sweep (its holder
/// entry is already gone, so it counts as an orphan then).
pub fn delete_approved(store: &BlobStore, approved: &[Digest]) -> Vec<Digest> {
    let mut deleted = Vec::new();
    for digest in approved {
        match store.delete_blob(digest) {
            Ok(()) => deleted.push(digest.clone()),
            Err(e) => eprintln!("gc: failed to delete blob {digest}: {e}"),
        }
    }
    deleted
}

/// On-disk age of a blob; zero when unknown (treated as brand new,
/// i.e. protected by the orphan grace window).
fn blob_age(store: &BlobStore, digest: &Digest, now: SystemTime) -> Duration {
    std::fs::metadata(store.blob_path(digest))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .unwrap_or(Duration::ZERO)
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
            orphan_grace: Duration::ZERO, // fresh test blobs are eligible
        }
    }

    #[test]
    fn candidates_nominate_unreferenced_blob_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let catalog = ManifestCatalog::default();

        // Write an orphan blob (not in any manifest)
        let orphan = test_digest("orphan1");
        write_fake_blob(&store, &orphan);

        let result = gc_candidates(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert_eq!(result.candidates.len(), 1);
        // Phase 1 never touches disk.
        assert!(store.has_blob(&orphan));

        // Phase 2 deletes only what was approved.
        let deleted = delete_approved(&store, &result.candidates);
        assert_eq!(deleted.len(), 1);
        assert!(!store.has_blob(&orphan));
    }

    #[test]
    fn fresh_orphans_are_protected_by_the_grace_window() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let catalog = ManifestCatalog::default();

        // A blob mid-push: on disk, no holder entry yet, seconds old.
        let inflight = test_digest("inflight1");
        write_fake_blob(&store, &inflight);

        let config = GcConfig {
            orphan_grace: Duration::from_secs(3600),
            ..default_config()
        };
        let result = gc_candidates(&store, &catalog, &HashSet::new(), &config).unwrap();
        assert!(result.candidates.is_empty());
        assert_eq!(result.orphan_grace_protected, 1);
        assert!(store.has_blob(&inflight));
    }

    /// REG1: the manifest's own blob is part of the image. Before the
    /// fix it was never in the protected set (only config + layers
    /// were), so after the orphan grace window GC deleted it and the
    /// catalogue pointed at a 404.
    #[test]
    fn gc_never_nominates_a_catalogued_manifests_own_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        let manifest = test_manifest("myapp", "m1", SystemTime::UNIX_EPOCH);
        for d in manifest.referenced_digests() {
            write_fake_blob(&store, d);
        }
        let manifest_digest = manifest.digest.clone();

        catalog.apply_manifest_commit(&ManifestCommit {
            manifest,
            tag: "latest".to_string(),
            holder_nodes: BTreeSet::from([1, 2]),
        });
        // Simulate a catalogue persisted before the fix: no holder
        // entry for the manifest blob, so it looks like an orphan.
        catalog
            .layer_locations
            .retain(|(d, _)| d != manifest_digest.as_str());

        let result = gc_candidates(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert!(
            !result.candidates.contains(&manifest_digest),
            "a tagged manifest's own blob must never be nominated"
        );
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

        let result = gc_candidates(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert!(result.candidates.is_empty());
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

        let result = gc_candidates(&store, &catalog, &active, &default_config()).unwrap();
        assert!(result.candidates.is_empty());
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

        let result = gc_candidates(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert!(result.candidates.is_empty());
        assert_eq!(result.sole_copy_protected, 1);
        assert!(store.has_blob(&orphan));
    }

    #[test]
    fn gc_nominates_multi_holder_unreferenced() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        let orphan = test_digest("multi1");
        write_fake_blob(&store, &orphan);

        // Two nodes hold this layer
        catalog.apply_update_locations(&crate::pickle::types::UpdateLayerLocations {
            updates: vec![(orphan.clone(), BTreeSet::from([1, 2]))],
        });

        let result = gc_candidates(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert_eq!(result.candidates.len(), 1);
        // Still on disk until the arbiter approves.
        assert!(store.has_blob(&orphan));
    }

    #[test]
    fn gc_report_contains_nominated_layers() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let mut catalog = ManifestCatalog::default();

        let orphan = test_digest("report1");
        write_fake_blob(&store, &orphan);

        // Multiple holders so sole-copy doesn't protect
        catalog.apply_update_locations(&crate::pickle::types::UpdateLayerLocations {
            updates: vec![(orphan.clone(), BTreeSet::from([1, 2]))],
        });

        let result = gc_candidates(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        let report = result.report(1).unwrap();
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

        // A manifest that exists but has no tags and is recent.
        catalog
            .manifests
            .push((manifest.digest.0.clone(), manifest.clone()));

        let config = GcConfig {
            retain_days: 7,
            ..default_config()
        };
        let result = gc_candidates(&store, &catalog, &HashSet::new(), &config).unwrap();
        // Layers from the recently pushed manifest should be protected
        assert!(result.candidates.is_empty());
    }

    #[test]
    fn gc_empty_store_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let catalog = ManifestCatalog::default();

        let result = gc_candidates(&store, &catalog, &HashSet::new(), &default_config()).unwrap();
        assert!(result.candidates.is_empty());
        assert!(result.report(1).is_none());
    }
}
