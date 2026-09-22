//! Durable publication ownership at the agent boundary.

use super::{BunAgent, BunError, Grill};
use crate::bun::discovery_owners::DiscoveryJournal;

/// Publication is either unconfigured, exclusively owned, or fenced after uncertainty.
#[derive(Debug, Default)]
pub(super) enum DiscoveryOwnership {
    #[default]
    Disabled,
    Ready(DiscoveryJournal),
    Recovered(DiscoveryJournal),
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
        if !journal.inventory().services.is_empty()
            || !journal.inventory().references.is_empty()
            || journal.inventory().consumer.is_some()
        {
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

    /// Persist standalone release permission after the caller confirms local withdrawal.
    pub(super) async fn authorise_local_discovery_release(
        &mut self,
        reference: &crate::grill::runc_intent::NetworkReference,
    ) -> Result<(), BunError> {
        use crate::bun::discovery_owners::ReferencePhase;
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            return Ok(());
        }
        let refuse = |reason: &str| BunError::RetirementState {
            instance_id: reference.instance_id.clone(),
            reason: reason.into(),
        };
        if self.cluster.is_some() {
            return Err(refuse(
                "remote withdrawal must be confirmed before durable release permission",
            ));
        }
        let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
            &self.discovery_ownership
        else {
            return Err(refuse(
                "discovery ownership is uncertain; recovery required",
            ));
        };
        let owner = journal
            .inventory()
            .references
            .iter()
            .find(|owner| owner.reference == *reference)
            .ok_or_else(|| refuse("original discovery reference is missing"))?;
        let service = owner.service.clone();
        let original = journal
            .inventory()
            .services
            .iter()
            .find(|owner| {
                owner.entry.namespace == service.namespace && owner.entry.app_name == service.name
            })
            .ok_or_else(|| refuse("original service allocation is missing"))?;
        let live = self
            .service_map
            .resolve(&service)
            .ok_or_else(|| refuse("original service withdrawal is unproven"))?;
        if live.vip != original.entry.vip
            || live.port != original.entry.port
            || live
                .backends
                .iter()
                .any(|backend| backend.instance_id == reference.instance_id.0)
        {
            return Err(refuse("original service withdrawal is unproven"));
        }
        self.update_discovery_inventory(&service, |next| {
            for owner in &mut next.services {
                if owner.entry.namespace == service.namespace
                    && owner.entry.app_name == service.name
                {
                    owner
                        .entry
                        .backends
                        .retain(|backend| backend.instance_id != reference.instance_id.0);
                }
            }
            for owner in &mut next.references {
                if owner.reference == *reference {
                    owner.phase = ReferencePhase::ReleaseAuthorised;
                }
            }
        })
        .await
    }

    /// Retire an exact standalone allocation after kernel withdrawal and runtime cleanup.
    pub(super) async fn retire_discovery_service(
        &mut self,
        service: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        use crate::bun::discovery_owners::ServicePhase;
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            return Ok(());
        }
        let refuse = |reason: &str| BunError::BackendPublication {
            service: service.clone(),
            reason: reason.into(),
        };
        let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
            &self.discovery_ownership
        else {
            return Err(refuse(
                "discovery ownership is uncertain; recovery required",
            ));
        };
        let original = journal.inventory().services.iter().find(|owner| {
            owner.entry.namespace == service.namespace && owner.entry.app_name == service.name
        });
        let live = self.service_map.resolve(service);
        let Some(original) = original else {
            // Portless workloads and repeated acknowledged stops own no allocation.
            return if live.is_none() {
                Ok(())
            } else {
                Err(refuse("original service allocation is missing"))
            };
        };
        if self.cluster.is_some() {
            return Err(refuse(
                "remote withdrawal must be confirmed before service retirement",
            ));
        }
        let Some(live) = live else {
            return Err(refuse("original service withdrawal is unproven"));
        };
        if live.vip != original.entry.vip
            || live.port != original.entry.port
            || !live.backends.is_empty()
        {
            return Err(refuse("original service withdrawal is unproven"));
        }
        if journal
            .inventory()
            .references
            .iter()
            .any(|owner| owner.service == *service)
        {
            return Err(refuse("original runtime references still require release"));
        }
        // Include historical candidates: private metadata loss cannot prove that
        // a request which already captured an endpoint released it.
        let backends = original.entry.backends.clone();
        for backend in &backends {
            self.drains
                .start_drain(&crate::wrapper::draining::DrainCommand {
                    app_name: service.name.clone(),
                    instance_id: backend.instance_id.clone(),
                    timeout: std::time::Duration::ZERO,
                })
                .await;
        }
        self.drains.check_completions().await;
        for backend in &backends {
            if self.drains.is_draining(&backend.instance_id).await {
                return Err(refuse(
                    "captured ingress requests still require confirmed release",
                ));
            }
        }
        self.update_discovery_inventory(service, |next| {
            for owner in &mut next.services {
                if owner.entry.namespace == service.namespace
                    && owner.entry.app_name == service.name
                {
                    owner.entry.backends.clear();
                    owner.phase = ServicePhase::Withdrawn;
                }
            }
        })
        .await?;
        self.update_discovery_inventory(service, |next| {
            next.services.retain(|owner| {
                owner.entry.namespace != service.namespace || owner.entry.app_name != service.name
            });
        })
        .await
    }

    /// Forget a permission only after the runtime acknowledges the exact release.
    pub(super) async fn forget_released_discovery_reference(
        &mut self,
        reference: &crate::grill::runc_intent::NetworkReference,
    ) -> Result<(), BunError> {
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            return Ok(());
        }
        self.require_discovery_release_permission(reference)?;
        let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
            &self.discovery_ownership
        else {
            return Err(BunError::RetirementState {
                instance_id: reference.instance_id.clone(),
                reason: "discovery release acknowledgement is uncertain".into(),
            });
        };
        let Some(owner) = journal
            .inventory()
            .references
            .iter()
            .find(|owner| owner.reference == *reference)
        else {
            return Err(BunError::RetirementState {
                instance_id: reference.instance_id.clone(),
                reason: "original release permission is missing".into(),
            });
        };
        let service = owner.service.clone();
        self.update_discovery_inventory(&service, |next| {
            next.references
                .retain(|owner| owner.reference != *reference);
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
            DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)
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
        let (journal, recovered) =
            match std::mem::replace(&mut self.discovery_ownership, DiscoveryOwnership::Uncertain) {
                DiscoveryOwnership::Disabled => {
                    self.discovery_ownership = DiscoveryOwnership::Disabled;
                    return Ok(());
                }
                DiscoveryOwnership::Ready(journal) => (journal, false),
                DiscoveryOwnership::Recovered(journal) => (journal, true),
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
        self.discovery_ownership = if recovered {
            DiscoveryOwnership::Recovered(journal)
        } else {
            DiscoveryOwnership::Ready(journal)
        };
        Ok(())
    }

    /// Fresh-only configuration cannot guess original allocations during adoption.
    pub(super) fn require_discovery_recovery(
        &self,
        has_records: bool,
        has_launches: bool,
    ) -> Result<(), BunError> {
        match &self.discovery_ownership {
            DiscoveryOwnership::Disabled | DiscoveryOwnership::Recovered(_) => Ok(()),
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
