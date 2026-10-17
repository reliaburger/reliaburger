//! Durable publication ownership at the agent boundary.

use super::{BunAgent, BunError, Grill};
use crate::bun::discovery_owners::{DiscoveryInventory, DiscoveryJournal};

/// Critical readiness subsystem that reports a fenced discovery journal. While
/// it is degraded the node cannot publish, withdraw or retire services.
pub(super) const DISCOVERY_JOURNAL_SUBSYSTEM: &str = "discovery:journal";

/// Publication is either unconfigured, exclusively owned, writing off the
/// loop, or fenced after uncertainty.
#[derive(Debug, Default)]
pub(super) enum DiscoveryOwnership {
    #[default]
    Disabled,
    Ready(DiscoveryJournal),
    Recovered(DiscoveryJournal),
    /// A consumer step's write outlasted its turn and goes on off the loop
    /// (#890). The journal itself is inside the write's task until it lands.
    Writing(Box<DiscoveryWrite>),
    Uncertain,
}

impl DiscoveryOwnership {
    /// The inventory on disk: the journal's, or while a write is in flight,
    /// the one before it. Only consumer steps leave a write in flight, and
    /// they change only the consumer's ownership, so the services and
    /// runtime references read the same either side of it.
    pub(super) fn durable(&self) -> Option<&DiscoveryInventory> {
        match self {
            DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal) => {
                Some(journal.inventory())
            }
            DiscoveryOwnership::Writing(write) => Some(&write.durable),
            DiscoveryOwnership::Disabled | DiscoveryOwnership::Uncertain => None,
        }
    }
}

/// A discovery-journal write still running after its turn ended.
#[derive(Debug)]
pub(super) struct DiscoveryWrite {
    /// The fsync'd persist, which hands the journal back when it lands.
    task: tokio::task::JoinHandle<std::io::Result<DiscoveryJournal>>,
    /// The inventory before this write, for readers meanwhile.
    durable: DiscoveryInventory,
    /// Whether the journal was recovered, so it goes back the same way.
    recovered: bool,
    /// Where to reopen the journal if the write fails.
    directory: std::path::PathBuf,
    /// The service the write's failure is reported against.
    service: crate::onion::service_id::ServiceId,
    /// When the write started, for the log line a slow one leaves.
    started: std::time::Instant,
}

/// How a discovery write ended, as the loop's `select!` branch sees it.
pub(super) type DiscoveryWriteOutcome =
    Result<std::io::Result<DiscoveryJournal>, tokio::task::JoinError>;

/// Whether a write a step started has landed, or goes on off the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Landing {
    /// The write is durable (or there was nothing to write).
    Landed,
    /// The turn's runtime budget ran out first. The step's next part waits
    /// for [`BunAgent::settle_discovery_write`].
    InFlight,
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

/// How the durable journal accounts for a runtime address hold the agent
/// has no in-memory record of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JournalReference {
    /// The journal records exactly this hold.
    Recorded,
    /// The journal is authoritative and names no hold for this instance.
    Unrecorded,
    /// Ownership is disabled, uncertain, or records a different hold.
    Unknown,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Classify a runtime hold against the durable discovery journal. A
    /// launch is only allowed after its hold is recorded, so an unrecorded
    /// hold belongs to an instance that never started.
    pub(super) fn journal_reference(
        &self,
        held: &crate::grill::runc_intent::NetworkReference,
    ) -> JournalReference {
        let Some(inventory) = self.discovery_ownership.durable() else {
            return JournalReference::Unknown;
        };
        match inventory
            .references
            .iter()
            .find(|owner| owner.reference.instance_id == held.instance_id)
        {
            None => JournalReference::Unrecorded,
            Some(owner) if owner.reference == *held => JournalReference::Recorded,
            Some(_) => JournalReference::Unknown,
        }
    }

    /// Keep the execution whoever started `id` read from the runtime, for
    /// the discovery journal's next publication. An inventory that couldn't
    /// be read fails the step while the journal is on, as the publication
    /// that read it inside the turn used to: the journal can't name the
    /// execution without it.
    pub(super) fn record_launch_execution(
        &mut self,
        id: &crate::grill::InstanceId,
        execution: &super::launch_evidence::LaunchExecution,
    ) -> Result<(), BunError> {
        use super::launch_evidence::LaunchExecution;
        match execution {
            LaunchExecution::Generation(generation) => {
                self.launch_executions
                    .insert(id.clone(), generation.clone());
            }
            LaunchExecution::Unrecorded => {
                self.launch_executions.remove(id);
            }
            LaunchExecution::Unknown(reason) => {
                self.launch_executions.remove(id);
                if !matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
                    return Err(BunError::AdoptionState(format!("publication {reason}")));
                }
            }
        }
        Ok(())
    }

    /// Preserve attempted publications before any kernel or userspace acknowledgement.
    pub(super) async fn persist_discovery_publication(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
        services: &crate::onion::service_map::ServiceMap,
    ) -> Result<(), BunError> {
        use crate::bun::discovery_owners::{ServiceOwner, ServicePhase};
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            return Ok(());
        }
        // What each backend's launch ran, as whoever started it read it:
        // no runtime read here, inside the turn (#419).
        let executions = self.launch_executions.clone();
        self.update_discovery_inventory(id, |next| {
            // Absence from the candidate is not withdrawal proof. Preserve
            // earlier allocations until their confirmed retirement removes them.
            for entry in services.resolve_all() {
                let owner = ServiceOwner {
                    executions: entry
                        .backends
                        .iter()
                        .filter_map(|backend| {
                            let id = crate::grill::InstanceId(backend.instance_id.clone());
                            executions
                                .get(&id)
                                .map(|generation| (backend.instance_id.clone(), generation.clone()))
                        })
                        .collect(),
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

    /// Persist release permission after local withdrawal and exact cluster confirmation.
    pub(super) async fn authorise_local_discovery_release(
        &mut self,
        reference: &crate::grill::runc_intent::NetworkReference,
        remote: Option<&crate::onion::producer::ProducerReleaseConfirmation>,
    ) -> Result<(), BunError> {
        use crate::bun::discovery_owners::ReferencePhase;
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            return Ok(());
        }
        // Decide against the journal itself, not the one before a write.
        self.settle_discovery_write().await;
        // The original grant was fsynced before the runtime release. Replaying
        // that exact permission must also work while the leader is unavailable.
        if self.require_discovery_release_permission(reference).is_ok() {
            return Ok(());
        }
        let refuse = |reason: &str| BunError::RetirementState {
            instance_id: reference.instance_id.clone(),
            reason: reason.into(),
        };
        if let Some(cluster) = &self.cluster
            && remote.is_none_or(|confirmed| {
                confirmed.node_id != cluster.local_node_id.0
                    || confirmed.execution.instance_id != reference.instance_id
                    || confirmed.execution.generation
                        != crate::grill::RuntimeGeneration::runc(reference.generation.as_str())
            })
        {
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
                    owner.executions.remove(&reference.instance_id.0);
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

    /// Retire an exact local allocation after consumer withdrawal and runtime cleanup.
    pub(super) async fn retire_discovery_service(
        &mut self,
        service: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        use crate::bun::discovery_owners::ServicePhase;
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            return Ok(());
        }
        self.settle_discovery_write().await;
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
        let original = journal
            .inventory()
            .services
            .iter()
            .find(|owner| {
                owner.entry.namespace == service.namespace && owner.entry.app_name == service.name
            })
            .cloned();
        let live = self.service_map.resolve(service);
        let Some(original) = original else {
            // Portless workloads and repeated acknowledged stops own no allocation.
            return if live.is_none() {
                Ok(())
            } else {
                Err(refuse("original service allocation is missing"))
            };
        };
        if self.cluster.is_some() && self.consumer_owner().is_none() {
            return Err(refuse("original consumer ownership is missing"));
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
        if self.cluster.is_some() {
            // Release only this node's reservation. The council independently
            // retains the global VIP until every registered consumer confirms.
            // Republishing drops this service's local backends from the view;
            // the drains below prove its captured requests have released.
            self.mark_consumer_view_stale()?;
            self.refresh_consumer_view().await?;
            // A failed publication leaves the kernel view uncertain until the
            // next synchronisation withdraws it completely.
            if !self.consumer_owner().is_some_and(|owner| {
                matches!(
                    owner.phase,
                    crate::bun::consumer_owners::ConsumerPhase::Active
                        | crate::bun::consumer_owners::ConsumerPhase::Withdrawn
                )
            }) {
                return Err(refuse(
                    "consumer view has not settled after a failed publication",
                ));
            }
        }
        // Include historical candidates: private metadata loss cannot prove that
        // a request which already captured an endpoint released it.
        let drains: Vec<_> = original
            .entry
            .backends
            .iter()
            .map(|backend| crate::wrapper::draining::DrainCommand {
                app_name: service.name.clone(),
                instance_id: backend.instance_id.clone(),
                timeout: std::time::Duration::ZERO,
            })
            .collect();
        // LOOP-INLINE: in-memory lock, no I/O
        if !self.drains.drain_all(&drains).await {
            return Err(refuse(
                "captured ingress requests still require confirmed release",
            ));
        }
        self.update_discovery_inventory(service, |next| {
            for owner in &mut next.services {
                if owner.entry.namespace == service.namespace
                    && owner.entry.app_name == service.name
                {
                    owner.entry.backends.clear();
                    owner.executions.clear();
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
        self.settle_discovery_write().await;
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
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            return Ok(());
        }
        match self.discovery_ownership.durable() {
            Some(inventory)
                if inventory.references.iter().any(|owner| {
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

    /// Journal `update` and wait for it to land.
    pub(super) async fn update_discovery_inventory(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
        update: impl FnOnce(&mut DiscoveryInventory) + Send,
    ) -> Result<(), BunError> {
        match self.write_discovery_inventory(id, update, None).await? {
            Landing::Landed => Ok(()),
            // With no deadline the write is waited for; this only settles
            // what can't happen.
            Landing::InFlight => {
                self.settle_discovery_write().await;
                Ok(())
            }
        }
    }

    /// Journal `update`, waiting for the write until `deadline` at most:
    /// [`Landing::InFlight`] when it ran out first, and the write goes on off
    /// the loop (#890). `None` waits for the write however long it takes.
    pub(super) async fn write_discovery_inventory(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
        update: impl FnOnce(&mut DiscoveryInventory) + Send,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Landing, BunError> {
        let failure = |reason: String| BunError::BackendPublication {
            service: id.clone(),
            reason,
        };
        // One write at a time, each from the state the last one left.
        self.settle_discovery_write().await;
        self.reopen_uncertain_discovery().await;
        let (journal, recovered) = match &self.discovery_ownership {
            DiscoveryOwnership::Disabled => return Ok(Landing::Landed),
            DiscoveryOwnership::Ready(journal) => (journal, false),
            DiscoveryOwnership::Recovered(journal) => (journal, true),
            DiscoveryOwnership::Writing(_) | DiscoveryOwnership::Uncertain => {
                return Err(failure(
                    "discovery ownership is uncertain; recovery required".into(),
                ));
            }
        };
        let mut next = journal.inventory().clone();
        update(&mut next);
        // A refusal decided in memory never touches disk, so it can't make the
        // durable state uncertain.
        journal
            .check(&next)
            .map_err(|error| failure(error.to_string()))?;
        let directory = journal.directory().to_owned();
        // Readers see this while the write is in flight. A write the turn
        // waits out never needs it, but which one it will be isn't known yet.
        let durable = deadline.map(|_| journal.inventory().clone());
        let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
            std::mem::replace(&mut self.discovery_ownership, DiscoveryOwnership::Uncertain)
        else {
            return Err(failure("discovery ownership changed during update".into()));
        };
        let started = std::time::Instant::now();
        #[cfg(test)]
        let stalls = std::sync::Arc::clone(&self.loop_stalls);
        let mut task = tokio::spawn(async move {
            #[cfg(test)]
            stalls.hold(super::LoopStall::Persist).await;
            journal.persist(next).await
        });
        let joined = match deadline {
            // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
            None => Some((&mut task).await),
            Some(deadline) => tokio::time::timeout_at(deadline, &mut task).await.ok(),
        };
        self.loop_meter.add_part(
            crate::bun::loop_meter::TurnPart::JournalWrite,
            started.elapsed(),
        );
        let Some(joined) = joined else {
            let Some(durable) = durable else {
                return Err(failure("discovery write lost its durable inventory".into()));
            };
            self.discovery_ownership = DiscoveryOwnership::Writing(Box::new(DiscoveryWrite {
                task,
                durable,
                recovered,
                directory,
                service: id.clone(),
                started,
            }));
            return Ok(Landing::InFlight);
        };
        self.finish_discovery_write(joined, recovered, directory, id)
            .await
            .map(|()| Landing::Landed)
    }

    /// Whether a consumer step's write is still going on off the loop.
    pub(super) fn discovery_write_in_flight(&self) -> bool {
        matches!(self.discovery_ownership, DiscoveryOwnership::Writing(_))
    }

    /// Resolves when the write in flight lands; never, if there is none.
    /// The loop's `select!` waits on it, so a slow write holds no turn.
    pub(super) async fn discovery_write_done(
        ownership: &mut DiscoveryOwnership,
    ) -> DiscoveryWriteOutcome {
        match ownership {
            DiscoveryOwnership::Writing(write) => (&mut write.task).await,
            _ => std::future::pending().await,
        }
    }

    /// Wait for a write a consumer step left in flight, and hand its result
    /// to that step. Every journal write and every decision made against
    /// the journal itself settles first.
    pub(super) async fn settle_discovery_write(&mut self) {
        let DiscoveryOwnership::Writing(write) = &mut self.discovery_ownership else {
            return;
        };
        // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
        let outcome = (&mut write.task).await;
        self.land_discovery_write(outcome).await;
    }

    /// Take back the journal a write in flight returned, or fence discovery
    /// if it failed, and hand the result to the consumer step waiting for it.
    pub(super) async fn land_discovery_write(&mut self, outcome: DiscoveryWriteOutcome) {
        let DiscoveryOwnership::Writing(write) =
            std::mem::replace(&mut self.discovery_ownership, DiscoveryOwnership::Uncertain)
        else {
            return;
        };
        let DiscoveryWrite {
            recovered,
            directory,
            service,
            started,
            ..
        } = *write;
        let took = started.elapsed();
        eprintln!(
            "bun: a discovery journal write took {} ms; the agent loop went on meanwhile",
            took.as_millis()
        );
        let landed = self
            .finish_discovery_write(outcome, recovered, directory, &service)
            .await;
        self.consumer_syncs.write_landed(landed);
    }

    /// Put the journal back after its write, or fence discovery after a
    /// failed one.
    async fn finish_discovery_write(
        &mut self,
        outcome: DiscoveryWriteOutcome,
        recovered: bool,
        directory: std::path::PathBuf,
        id: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        let persisted = outcome.unwrap_or_else(|error| Err(std::io::Error::other(error)));
        match persisted {
            Ok(journal) => {
                self.discovery_ownership = if recovered {
                    DiscoveryOwnership::Recovered(journal)
                } else {
                    DiscoveryOwnership::Ready(journal)
                };
                Ok(())
            }
            Err(error) => {
                // The write may or may not have reached disk. Reopening later
                // adopts whichever checkpoint is durable; the journal is written
                // before any kernel or userspace effect, so either is safe.
                self.discovery_ownership = DiscoveryOwnership::Uncertain;
                self.discovery_reopen = Some((directory, recovered));
                if let Some(readiness) = &self.readiness {
                    // LOOP-INLINE: in-memory lock, no I/O
                    readiness.register(DISCOVERY_JOURNAL_SUBSYSTEM, true).await;
                    // LOOP-INLINE: in-memory lock, no I/O
                    readiness
                        .degraded(DISCOVERY_JOURNAL_SUBSYSTEM, error.to_string())
                        .await;
                }
                Err(BunError::BackendPublication {
                    service: id.clone(),
                    reason: error.to_string(),
                })
            }
        }
    }

    /// Reopen the discovery journal after a write whose outcome is unknown.
    /// Called before each update and on every agent tick, so any caller path
    /// recovers once the disk (or a still-running write worker) allows it.
    pub(super) async fn reopen_uncertain_discovery(&mut self) {
        if !matches!(self.discovery_ownership, DiscoveryOwnership::Uncertain) {
            return;
        }
        let Some((directory, recovered)) = self.discovery_reopen.clone() else {
            return;
        };
        // LOOP-INLINE: reopens the journal on disk, like a persist
        match DiscoveryJournal::open_async(&directory).await {
            Ok(journal) => {
                self.discovery_ownership = if recovered {
                    DiscoveryOwnership::Recovered(journal)
                } else {
                    DiscoveryOwnership::Ready(journal)
                };
                self.discovery_reopen = None;
                if let Some(readiness) = &self.readiness {
                    // LOOP-INLINE: in-memory lock, no I/O
                    readiness.ready(DISCOVERY_JOURNAL_SUBSYSTEM).await;
                }
            }
            Err(error) => {
                if let Some(readiness) = &self.readiness {
                    // LOOP-INLINE: in-memory lock, no I/O
                    readiness
                        .degraded(DISCOVERY_JOURNAL_SUBSYSTEM, error.to_string())
                        .await;
                }
            }
        }
    }

    /// Fresh-only configuration cannot guess original allocations during adoption.
    pub(super) fn require_discovery_recovery(
        &self,
        has_records: bool,
        has_launches: bool,
    ) -> Result<(), BunError> {
        let recovered = match &self.discovery_ownership {
            DiscoveryOwnership::Disabled | DiscoveryOwnership::Recovered(_) => return Ok(()),
            DiscoveryOwnership::Writing(write) => write.recovered,
            DiscoveryOwnership::Ready(_) | DiscoveryOwnership::Uncertain => false,
        };
        match self.discovery_ownership.durable() {
            _ if recovered => Ok(()),
            Some(inventory)
                if !has_records
                    && !has_launches
                    && inventory.services.is_empty()
                    && inventory.references.is_empty() =>
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
