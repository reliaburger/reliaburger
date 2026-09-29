//! Storage-node proof of an existing image, fenced against cleanup and GC.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::api::PickleState;
use super::authority::RegistryMutation;
use super::lease::{RegistryWriteAccess, is_test_repository, require_acceptance};
use super::types::{Digest, ImageCopyConfirmation, PickleError};

/// Receipt returned only after the receiving node commits its own verified copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageCopyReceipt {
    /// Storage node that supplied the proof.
    pub node_id: u64,
    /// Exact repository checked against current metadata.
    pub repository: String,
    /// Immutable manifest whose complete dependency set was verified.
    pub manifest_digest: Digest,
}

impl PickleState {
    /// Verify every local image blob and conditionally publish only this node's copy.
    /// Caller cancellation cannot abandon an admitted proof/publication transaction.
    pub async fn confirm_image_copy(
        &self,
        repository: &str,
        digest: &Digest,
    ) -> Result<ImageCopyReceipt, PickleError> {
        self.confirm_image_copy_with_access(repository, digest, None)
            .await
    }

    pub(crate) async fn confirm_image_copy_with_access(
        &self,
        repository: &str,
        digest: &Digest,
        access: Option<RegistryWriteAccess>,
    ) -> Result<ImageCopyReceipt, PickleError> {
        let state = self.clone();
        let repository = repository.to_owned();
        let digest = digest.clone();
        tokio::spawn(async move { state.confirm_copy_owned(repository, digest, access).await })
            .await
            .map_err(|error| {
                PickleError::ReplicationFailed(format!("copy confirmation task failed: {error}"))
            })?
    }

    async fn confirm_copy_owned(
        &self,
        repository: String,
        digest: Digest,
        access: Option<RegistryWriteAccess>,
    ) -> Result<ImageCopyReceipt, PickleError> {
        let access = match access {
            Some(access) => access,
            None => {
                self.admit_repository_write(&repository, None, None, true)
                    .await?
            }
        };
        if is_test_repository(&repository) != access.lease_id.is_some() {
            return Err(PickleError::LeaseDenied(
                "copy requires matching repository lease admission".into(),
            ));
        }
        let mut copy = ImageCopyConfirmation {
            repository: repository.clone(),
            manifest_digest: digest.clone(),
            node_id: self.node_raft_id,
            lease_id: access.lease_id.clone(),
            observed_gc_generation: 0,
            observed_at_unix_ms: crate::testkit::lease::now_unix_millis(),
        };
        let standalone = self.council.is_none() && self.forwarder.is_none();
        let operation = if standalone && copy.lease_id.is_some() {
            Some(
                self.test_leases
                    .begin_registry_copy(&copy)
                    .await
                    .map_err(|error| PickleError::LeaseDenied(error.to_string()))?,
            )
        } else {
            None
        };
        let mut local = Arc::clone(&self.catalog).write_owned().await;
        let view = if standalone {
            local.clone()
        } else {
            self.catalog_snapshot(&repository).await?
        };
        if local.repository_owners.get(&repository) != copy.lease_id.as_ref()
            || view.repository_owners.get(&repository) != copy.lease_id.as_ref()
        {
            return Err(PickleError::LeaseDenied(
                "repository copy generation changed".into(),
            ));
        }
        let manifest = view
            .get_repository_manifest(&repository, digest.as_str())
            .ok_or_else(|| PickleError::ManifestNotFound {
                repository: repository.clone(),
                tag: digest.to_string(),
            })?;
        let digests: Vec<Digest> = manifest.referenced_digests().into_iter().cloned().collect();
        copy.observed_gc_generation = self.registry_gc_generation().await?;
        let store = self.store.clone();
        let persist = self.persist_path.clone();
        let proof = copy.clone();
        // The blocking closure returns the guards, retaining them through the
        // subsequent authoritative response even if the original caller leaves.
        let (_local, _access, _operation) = tokio::task::spawn_blocking(move || {
            for digest in digests {
                let actual = super::store::sha256_file(&store.blob_path(&digest))?;
                if actual != digest {
                    return Err(PickleError::DigestMismatch {
                        expected: digest,
                        actual,
                    });
                }
            }
            if standalone {
                let mut next = local.clone();
                if !next.add_manifest_holder(
                    &proof.repository,
                    &proof.manifest_digest,
                    proof.node_id,
                ) {
                    return Err(PickleError::ManifestNotFound {
                        repository: proof.repository,
                        tag: proof.manifest_digest.to_string(),
                    });
                }
                if let Some(path) = persist {
                    next.persist_to(&path)?;
                }
                *local = next;
            }
            Ok((local, access, operation))
        })
        .await
        .map_err(|error| {
            PickleError::ReplicationFailed(format!("copy verification task failed: {error}"))
        })??;
        if let Some(response) = self.propose(RegistryMutation::Copy(copy)).await? {
            require_acceptance(response)?;
        }
        Ok(ImageCopyReceipt {
            node_id: self.node_raft_id,
            repository,
            manifest_digest: digest,
        })
    }
}
