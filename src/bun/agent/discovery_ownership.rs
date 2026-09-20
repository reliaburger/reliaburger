//! Durable publication ownership at the agent boundary.

use super::{BunAgent, BunError, Grill};
use crate::bun::discovery_owners::DiscoveryJournal;

/// Publication is either unconfigured, exclusively owned, or fenced after uncertainty.
#[derive(Debug, Default)]
pub(super) enum DiscoveryOwnership {
    #[default]
    Disabled,
    Ready(DiscoveryJournal),
    Uncertain,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Enable durable publication on an empty agent before its first deployment.
    /// Existing ownership requires recovery reconciliation, which this entry point refuses.
    /// Production runtime selection remains separate from this opt-in integration.
    pub async fn enable_fresh_discovery_ownership(
        &mut self,
        directory: &std::path::Path,
    ) -> Result<(), BunError> {
        if !matches!(self.discovery_ownership, DiscoveryOwnership::Disabled)
            || !self.service_map.resolve_all().is_empty()
            || !self.supervisor.list_instances().is_empty()
        {
            return Err(BunError::AdoptionState(
                "discovery ownership must be enabled before deployment".into(),
            ));
        }
        self.discovery_ownership = DiscoveryOwnership::Uncertain;
        let journal = DiscoveryJournal::open_async(directory)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        if !journal.inventory().services.is_empty() || !journal.inventory().references.is_empty() {
            return Err(BunError::AdoptionState(
                "existing discovery ownership requires recovery reconciliation".into(),
            ));
        }
        self.discovery_ownership = DiscoveryOwnership::Ready(journal);
        Ok(())
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Preserve attempted publications before any kernel or userspace acknowledgement.
    pub(super) async fn persist_discovery_publication(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
        services: &crate::onion::service_map::ServiceMap,
    ) -> Result<(), BunError> {
        use crate::bun::discovery_owners::{ServiceOwner, ServicePhase};
        self.update_discovery_inventory(id, |next| {
            // Absence from the candidate is not withdrawal proof. Preserve
            // earlier allocations until their confirmed retirement removes them.
            for entry in services.resolve_all() {
                let owner = ServiceOwner {
                    entry: entry.clone(),
                    phase: ServicePhase::Owned,
                };
                if let Some(previous) = next.services.iter_mut().find(|previous| {
                    previous.entry.namespace == entry.namespace
                        && previous.entry.app_name == entry.app_name
                }) {
                    *previous = owner;
                } else {
                    next.services.push(owner);
                }
            }
        })
        .await
    }

    /// Record the original held runtime generation before allowing its launch.
    pub(super) async fn persist_discovery_reference(
        &mut self,
        reference: &crate::grill::runc_intent::NetworkReference,
    ) -> Result<(), BunError> {
        use crate::bun::discovery_owners::{ReferenceOwner, ReferencePhase};
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            return Ok(());
        }
        let instance = self
            .supervisor
            .get_instance(&reference.instance_id)
            .ok_or_else(|| BunError::InstanceNotFound {
                instance_id: reference.instance_id.clone(),
            })?;
        let service =
            crate::onion::service_id::ServiceId::new(&instance.namespace, &instance.app_name);
        self.update_discovery_inventory(&service, |next| {
            let owner = ReferenceOwner {
                service: service.clone(),
                reference: reference.clone(),
                phase: ReferencePhase::Held,
            };
            if let Some(previous) = next
                .references
                .iter_mut()
                .find(|previous| previous.reference.instance_id == reference.instance_id)
            {
                *previous = owner;
            } else {
                next.references.push(owner);
            }
        })
        .await
    }

    /// Refuse address release until an exact durable permission exists.
    pub(super) fn require_discovery_release_permission(
        &self,
        reference: &crate::grill::runc_intent::NetworkReference,
    ) -> Result<(), BunError> {
        use crate::bun::discovery_owners::ReferencePhase;
        match &self.discovery_ownership {
            DiscoveryOwnership::Disabled => Ok(()),
            DiscoveryOwnership::Ready(journal)
                if journal.inventory().references.iter().any(|owner| {
                    owner.reference == *reference
                        && owner.phase == ReferencePhase::ReleaseAuthorised
                }) =>
            {
                Ok(())
            }
            _ => Err(BunError::RetirementState {
                instance_id: reference.instance_id.clone(),
                reason: "original discovery reference has no durable release permission".into(),
            }),
        }
    }

    async fn update_discovery_inventory(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
        update: impl FnOnce(&mut crate::bun::discovery_owners::DiscoveryInventory) + Send,
    ) -> Result<(), BunError> {
        let failure = |reason: String| BunError::BackendPublication {
            service: id.clone(),
            reason,
        };
        let journal =
            match std::mem::replace(&mut self.discovery_ownership, DiscoveryOwnership::Uncertain) {
                DiscoveryOwnership::Disabled => {
                    self.discovery_ownership = DiscoveryOwnership::Disabled;
                    return Ok(());
                }
                DiscoveryOwnership::Ready(journal) => journal,
                DiscoveryOwnership::Uncertain => {
                    return Err(failure(
                        "discovery ownership is uncertain; recovery required".into(),
                    ));
                }
            };
        let mut next = journal.inventory().clone();
        update(&mut next);
        let journal = journal
            .persist(next)
            .await
            .map_err(|error| failure(error.to_string()))?;
        self.discovery_ownership = DiscoveryOwnership::Ready(journal);
        Ok(())
    }

    /// Fresh-only configuration cannot guess original allocations during adoption.
    pub(super) fn require_discovery_recovery(
        &self,
        has_records: bool,
        has_launches: bool,
    ) -> Result<(), BunError> {
        match &self.discovery_ownership {
            DiscoveryOwnership::Disabled => Ok(()),
            DiscoveryOwnership::Ready(journal)
                if !has_records
                    && !has_launches
                    && journal.inventory().services.is_empty()
                    && journal.inventory().references.is_empty() =>
            {
                Ok(())
            }
            _ => Err(BunError::AdoptionState(
                "original discovery ownership requires recovery reconciliation before adoption"
                    .into(),
            )),
        }
    }
}
