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
        // Absence from the candidate is not withdrawal proof. Preserve every
        // earlier allocation until a separate, confirmed retirement removes it.
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
