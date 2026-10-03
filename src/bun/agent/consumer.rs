//! Durable consumer publication, complete view withdrawal and receipt permissions.

use super::{BunAgent, BunError, DiscoveryOwnership, Grill};
use crate::bun::consumer_owners::{
    ConsumerIdentity, ConsumerOwnership, ConsumerPhase, ConsumerPublication, ConsumerReceipt,
    ReceiptPhase,
};
use crate::cluster::orchestrate::IngressAssignment;
use crate::onion::{
    catalog::EndpointCatalog, service_id::ServiceId, service_map::ServiceMap, types::ServiceEntry,
    withdrawal::EndpointWithdrawalInstruction,
};
use tokio::sync::oneshot;

/// Confirmed publication and original withdrawals safe to acknowledge remotely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerUpdate {
    /// False while captured requests still retain an earlier view.
    pub published: bool,
    /// Original generations with durable local withdrawal proof.
    pub receipts: Vec<u64>,
    /// Whether a local service registers only at its committed catalogue
    /// allocation. False when discovery ownership is disabled, where the
    /// agent allocates locally instead.
    pub requires_allocations: bool,
}

/// One leader answer for the consumer view, as the placement poll sends it,
/// or the last one again after a local change.
pub(super) struct ConsumerRequest {
    pub(super) generation: u64,
    pub(super) catalog: EndpointCatalog,
    pub(super) ingress: Vec<IngressAssignment>,
    pub(super) withdrawals: Vec<EndpointWithdrawalInstruction>,
    /// The poll waiting for the result; `None` for a local republication.
    pub(super) answer: Option<ConsumerAnswer>,
}

/// The placement poll waiting on a [`ConsumerRequest`].
pub(super) struct ConsumerAnswer {
    /// When the poll's request left, on the boot clock. The view lease
    /// runs from here.
    pub(super) requested_at_ns: u64,
    pub(super) response: oneshot::Sender<Result<ConsumerUpdate, BunError>>,
}

/// What a synchronisation does next. Each step journals at most one
/// discovery write: on the soak's disks a write took about 400 ms, and the
/// five a recovery's first answer needs held one turn for two seconds
/// (#505). The steps keep the order the single-turn version had.
enum ConsumerStep {
    /// Withdraw every earlier view (the journal already says Withdrawing),
    /// then publish this one.
    Withdraw(ConsumerPublication),
    /// Mark the receipts the full withdrawal proved, then publish this one.
    ProveReceipts(ConsumerPublication),
    /// Journal this view as the only one, then install it.
    Publish(ConsumerPublication),
    /// Record the installed view as active.
    Activate,
    /// Forget replaced views whose requests have released, and mark the
    /// receipts no remaining view contradicts.
    Compact,
}

/// Where a synchronisation stands after a step.
enum Progress {
    Next(ConsumerStep),
    Done(ConsumerUpdate),
}

/// A synchronisation part of the way through its steps.
struct ConsumerSync {
    next: ConsumerStep,
    answer: Option<ConsumerAnswer>,
}

/// The synchronisation under way, if any, and the answer waiting for it.
#[derive(Default)]
pub(super) struct ConsumerSyncs {
    current: Option<ConsumerSync>,
    waiting: Option<ConsumerRequest>,
    /// Whether this turn took a step.
    stepped_now: bool,
    /// Whether the last turn took a step, so a queued command goes next.
    stepped_last: bool,
}

impl ConsumerSyncs {
    /// No step is due.
    pub(super) fn is_idle(&self) -> bool {
        self.current.is_none() && self.waiting.is_none()
    }

    /// A step is due and goes ahead of queued commands: steps and commands
    /// take turns, so neither waits for all of the other.
    pub(super) fn step_first(&self) -> bool {
        !self.is_idle() && !self.stepped_last
    }

    /// Close the turn; true if it took a step.
    pub(super) fn end_turn(&mut self) -> bool {
        self.stepped_last = std::mem::take(&mut self.stepped_now);
        self.stepped_last
    }

    /// The view the synchronisation under way will publish, while the
    /// journal doesn't yet list it as the only one.
    fn pending_publication(&self) -> Option<&ConsumerPublication> {
        match &self.current.as_ref()?.next {
            ConsumerStep::Withdraw(publication)
            | ConsumerStep::ProveReceipts(publication)
            | ConsumerStep::Publish(publication) => Some(publication),
            ConsumerStep::Activate | ConsumerStep::Compact => None,
        }
    }

    /// Test-only: a synchronisation that has withdrawn the old view and
    /// will publish `publication` next.
    #[cfg(test)]
    pub(super) fn hold_for_test(&mut self, publication: ConsumerPublication) {
        self.current = Some(ConsumerSync {
            next: ConsumerStep::ProveReceipts(publication),
            answer: None,
        });
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    pub(super) fn consumer_owner(&self) -> Option<&ConsumerOwnership> {
        match &self.discovery_ownership {
            DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal) => {
                journal.inventory().consumer.as_ref()
            }
            _ => None,
        }
    }

    pub(super) fn consumer_controls_views(&self) -> bool {
        self.consumer_owner().is_some()
            || (self.cluster.is_some()
                && matches!(self.discovery_ownership, DiscoveryOwnership::Uncertain))
    }

    async fn save_consumer(&mut self, next: ConsumerOwnership) -> Result<(), BunError> {
        next.validate().map_err(failure)?;
        crate::bun::consumer_owners::validate_transition(self.consumer_owner(), Some(&next))
            .map_err(failure)?;
        self.update_discovery_inventory(&ServiceId::new("system", "discovery"), |inventory| {
            inventory.consumer = Some(next)
        })
        .await
    }

    /// Reserve the council's exact allocation before a local clustered launch.
    pub(super) fn register_local_service(
        &mut self,
        id: &ServiceId,
        port: u16,
        firewall_allow_from: Option<Vec<String>>,
    ) -> Result<(), BunError> {
        let Some(owner) = self.consumer_owner() else {
            if self.consumer_controls_views() {
                return Err(failure("consumer ownership is uncertain"));
            }
            return self
                .service_map
                .register(id, port, firewall_allow_from)
                .map(|_| ())
                .map_err(failure);
        };
        let allocation = owner
            .publications
            .last()
            .and_then(|publication| publication.catalog.resolve(id))
            .filter(|service| service.port == port)
            .ok_or_else(|| failure("local service requires its committed cluster allocation"))?;
        let mut entries: Vec<_> = self
            .service_map
            .resolve_all()
            .into_iter()
            .cloned()
            .collect();
        entries.push(crate::onion::types::ServiceEntry {
            namespace: id.namespace.clone(),
            app_name: id.name.clone(),
            namespace_id: crate::onion::vip::name_to_id(&id.namespace),
            app_id: u32::from(allocation.vip.0),
            vip: allocation.vip,
            port,
            backends: vec![],
            firewall_allow_from,
        });
        self.service_map = ServiceMap::from_snapshot(&entries).map_err(failure)?;
        Ok(())
    }

    /// Recover enrolled consumer ownership before starting any proxy, DNS or adoption.
    /// All consumers from the previous Bun process must have stopped first.
    pub async fn recover_consumer_ownership(
        &mut self,
        directory: &std::path::Path,
        identity: ConsumerIdentity,
    ) -> Result<(), BunError> {
        if self
            .cluster
            .as_ref()
            .is_none_or(|cluster| cluster.local_node_id != identity.node_id)
        {
            return Err(failure(
                "consumer recovery requires its original cluster node",
            ));
        }
        self.recover_discovery(directory, Some(identity)).await?;
        let mut owner = self
            .consumer_owner()
            .cloned()
            .ok_or_else(|| failure("consumer ownership is missing"))?;
        owner.phase = ConsumerPhase::Withdrawing;
        self.save_consumer(owner).await?;
        if !self.withdraw_consumer_view().await? {
            return Err(failure(
                "previous consumer requests still retain publication",
            ));
        }
        // From here on this node routes cluster services only while the
        // leader keeps answering. Until the first answer, nothing routes.
        self.view_lease.enforce();
        self.sync_view_lease_kernel().await
    }

    /// Mirror the view lease into the kernel, so the connect hook enforces
    /// it even if this process dies.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn sync_view_lease_kernel(&self) -> Result<(), BunError> {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return Ok(());
        };
        let value = crate::onion::types::ViewLeaseValue::from_lease(&self.view_lease);
        let mut ebpf = handle.lock().await;
        crate::onion::ebpf::maps::BpfServiceMap::new()
            .write_view_lease(&mut ebpf, value)
            .map_err(failure)
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn sync_view_lease_kernel(&self) -> Result<(), BunError> {
        Ok(())
    }

    /// Once the lease has lapsed, shrink the published view to this node's
    /// own backends. The leader may discharge this node soon after, and from
    /// then on nothing stops another node reusing a remote address this view
    /// names. A local backend's address is different: only this agent can
    /// release it, and [`confirm_producer_release`] refuses while any view
    /// this node published still names it.
    ///
    /// The durable publications stay as they are. They record what this node
    /// may still expose, and the whole view is a superset of the local one.
    ///
    /// [`confirm_producer_release`]: BunAgent::confirm_producer_release
    pub(super) async fn fence_lapsed_view(&mut self) -> Result<(), BunError> {
        if self.view_lease.is_valid() {
            return Ok(());
        }
        let Some(last) = self
            .consumer_owner()
            .filter(|owner| owner.phase == ConsumerPhase::Active)
            .and_then(|owner| owner.publications.last())
            .cloned()
        else {
            return Ok(());
        };
        let local = self.local_view(&last.effective_services);
        if self.lapsed_view.as_ref() == Some(&local) {
            return Ok(());
        }
        if self.lapsed_view.is_none() {
            eprintln!(
                "bun: the leader hasn't answered for {}s; routing only to this node's own \
                 backends until it does",
                crate::onion::lease::CONSUMER_VIEW_LEASE.as_secs()
            );
        }
        self.install_consumer_view(&local, &last.ingress).await?;
        self.lapsed_view = Some(local);
        Ok(())
    }

    /// The part of `published` that runs on this node now: local backends
    /// the agent still owns at the same address, with their current health.
    /// It never names anything `published` doesn't. Services keep
    /// their entries even with no local backend left, so their virtual
    /// addresses refuse connections instead of vanishing from DNS.
    fn local_view(&self, published: &[ServiceEntry]) -> Vec<ServiceEntry> {
        published
            .iter()
            .map(|entry| {
                let current = self
                    .service_map
                    .resolve(&ServiceId::new(&entry.namespace, &entry.app_name));
                let mut entry = entry.clone();
                entry.backends = entry
                    .backends
                    .iter()
                    .filter(|backend| backend.local)
                    .filter_map(|backend| {
                        current?
                            .backends
                            .iter()
                            .find(|owned| {
                                owned.local
                                    && owned.instance_id == backend.instance_id
                                    && owned.node_ip == backend.node_ip
                                    && owned.host_port == backend.host_port
                            })
                            .cloned()
                    })
                    .collect();
                entry
            })
            .collect()
    }

    /// Whether any view this node published may still route to `id`, one of
    /// its own instances. Its address can't be released until none does.
    /// A view a synchronisation under way is about to publish counts too:
    /// between its withdrawal and its publication the journal says
    /// Withdrawn, but the view it then installs was built before (#505).
    pub(super) fn own_view_names(&self, id: &crate::grill::InstanceId) -> bool {
        let names = |publication: &ConsumerPublication| {
            publication.effective_services.iter().any(|entry| {
                entry
                    .backends
                    .iter()
                    .any(|backend| backend.local && backend.instance_id == id.0)
            })
        };
        self.consumer_owner()
            .filter(|owner| owner.phase != ConsumerPhase::Withdrawn)
            .is_some_and(|owner| owner.publications.iter().any(names))
            || self.consumer_syncs.pending_publication().is_some_and(names)
    }

    /// Extend the view lease after publishing a leader's answer to a request
    /// sent at `requested_at_ns` on the boot clock.
    pub(super) async fn renew_view_lease(&mut self, requested_at_ns: u64) {
        self.view_lease.renew(requested_at_ns);
        if let Err(error) = self.sync_view_lease_kernel().await {
            // The kernel keeps its older expiry, so it fences early: safe.
            eprintln!("bun: cannot extend the kernel's view lease: {error}");
        }
    }

    async fn consumer_candidate(
        &self,
        generation: u64,
        catalog: EndpointCatalog,
        ingress: Vec<IngressAssignment>,
    ) -> Result<ConsumerPublication, BunError> {
        let owner = self
            .consumer_owner()
            .ok_or_else(|| failure("consumer ownership is missing"))?;
        if owner.publications.last().is_some_and(|last| {
            generation < last.generation
                || (generation == last.generation && catalog != last.catalog)
        }) {
            return Err(failure(
                "consumer catalogue generation regressed or changed",
            ));
        }
        catalog.validate_allocations().map_err(failure)?;
        // Cluster allocation is authoritative. Locally prepared or retiring
        // allocations remain reserved internally, but cannot invent a public VIP.
        let launches = self
            .complete_runtime_inventory(super::RUNTIME_INVENTORY_TIMEOUT, |reason| {
                failure(format!("consumer {reason}"))
            })
            .await?;
        let local: Vec<_> = self
            .service_map
            .resolve_all()
            .into_iter()
            .filter(|entry| {
                catalog
                    .resolve(&ServiceId::new(&entry.namespace, &entry.app_name))
                    .is_some_and(|service| service.vip == entry.vip && service.port == entry.port)
            })
            .map(|entry| {
                let mut entry = entry.clone();
                let service = catalog.resolve(&ServiceId::new(&entry.namespace, &entry.app_name));
                entry.backends.retain(|backend| {
                    service.is_some_and(|service| {
                        service.backends.iter().any(|remote| {
                            remote.node_id == owner.identity.node_id.0
                                && remote.execution.as_ref().is_some_and(|execution| {
                                    execution.instance_id.0 == backend.instance_id
                                        && launches.iter().any(|launch| {
                                            launch.instance_id == execution.instance_id
                                                && launch.generation == execution.generation
                                        })
                                })
                        })
                    })
                });
                entry
            })
            .collect();
        let local = ServiceMap::from_snapshot(&local).map_err(failure)?;
        let merged =
            local.with_cluster_catalog_excluding_node(&catalog, Some(&owner.identity.node_id.0));
        let mut routes = self.ingress_configs.clone();
        let mut seen = std::collections::HashSet::new();
        for route in ingress {
            let key = (route.namespace, route.name);
            if !seen.insert(key.clone()) {
                return Err(failure("duplicate consumer ingress assignment"));
            }
            routes.insert(key, route.config);
        }
        let mut ingress: Vec<_> = routes
            .into_iter()
            .map(|((namespace, name), config)| IngressAssignment {
                namespace,
                name,
                config,
            })
            .collect();
        ingress.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
        let publication = ConsumerPublication {
            generation,
            catalog,
            effective_services: merged.resolve_all().into_iter().cloned().collect(),
            ingress,
        };
        let candidate = ConsumerOwnership {
            identity: owner.identity.clone(),
            publications: vec![publication.clone()],
            phase: ConsumerPhase::Publishing,
            receipts: Default::default(),
        };
        candidate.validate().map_err(failure)?;
        Ok(publication)
    }

    /// Publish a leader answer, or republish the last one, all in one go:
    /// every step below, back to back. The loop spreads the same steps over
    /// several turns instead ([`Self::request_consumer_sync`]); this is for
    /// callers that must see the view settled before they carry on.
    pub(super) async fn synchronise_consumer(
        &mut self,
        generation: u64,
        catalog: EndpointCatalog,
        ingress: Vec<IngressAssignment>,
        withdrawals: Vec<EndpointWithdrawalInstruction>,
    ) -> Result<ConsumerUpdate, BunError> {
        let mut progress = self
            .first_consumer_step(generation, catalog, ingress, withdrawals)
            .await?;
        loop {
            match progress {
                Progress::Done(update) => return Ok(update),
                Progress::Next(step) => progress = self.consumer_step(step).await?,
            }
        }
    }

    /// Check a leader answer and journal its first change. Every path
    /// journals at most one discovery write here; [`Self::consumer_step`]
    /// takes the rest, one write a step (#505).
    async fn first_consumer_step(
        &mut self,
        generation: u64,
        catalog: EndpointCatalog,
        ingress: Vec<IngressAssignment>,
        withdrawals: Vec<EndpointWithdrawalInstruction>,
    ) -> Result<Progress, BunError> {
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            self.publish_cluster_catalogue(generation, catalog, ingress)
                .await?;
            return Ok(Progress::Done(ConsumerUpdate {
                published: true,
                receipts: vec![],
                requires_allocations: false,
            }));
        }
        let publication = self
            .consumer_candidate(generation, catalog, ingress)
            .await?;
        let mut owner = self
            .consumer_owner()
            .cloned()
            .ok_or_else(|| failure("consumer ownership is uncertain"))?;
        let mut seen = std::collections::HashSet::new();
        for withdrawal in withdrawals {
            if !seen.insert(withdrawal.generation) {
                return Err(failure("duplicate consumer withdrawal instruction"));
            }
            match owner.receipts.get(&withdrawal.generation) {
                Some(original) if original.withdrawal != withdrawal => {
                    return Err(failure("original consumer withdrawal instruction changed"));
                }
                Some(_) => {}
                None => {
                    owner.receipts.insert(
                        withdrawal.generation,
                        ConsumerReceipt {
                            withdrawal,
                            phase: ReceiptPhase::Pending,
                        },
                    );
                }
            }
        }
        if owner.receipts.values().any(|receipt| {
            receipt.phase == ReceiptPhase::Ready && publication.intersects(&receipt.withdrawal)
        }) {
            return Err(failure(
                "publication would resurrect a confirmed withdrawal",
            ));
        }
        if owner.phase != ConsumerPhase::Active {
            // Recovery path: nothing from an earlier view may remain exposed
            // before the new one publishes, so withdraw everything first.
            if owner.publications.last() != Some(&publication) {
                owner.publications.push(publication.clone());
            }
            owner.phase = ConsumerPhase::Withdrawing;
            self.save_consumer(owner).await?;
            return Ok(Progress::Next(ConsumerStep::Withdraw(publication)));
        }
        if owner.publications.last() != Some(&publication) {
            // Journalled before the kernel sees it, so after a crash from
            // here on, recovery knows everything that may be exposed.
            owner.publications.push(publication.clone());
            owner.phase = ConsumerPhase::Publishing;
            self.save_consumer(owner).await?;
            self.apply_consumer_publication(&publication).await?;
            return Ok(Progress::Next(ConsumerStep::Activate));
        }
        if self.lapsed_view.is_some() {
            // The leader answered with the view this node last published;
            // only the local part of it is installed while the lease lapsed.
            self.apply_consumer_publication(&publication).await?;
        }
        if Some(&owner) != self.consumer_owner() {
            // New withdrawal obligations, nothing else.
            self.save_consumer(owner).await?;
            return Ok(Progress::Next(ConsumerStep::Compact));
        }
        self.compact_consumer_publications().await?;
        Ok(Progress::Done(self.consumer_update(true)))
    }

    /// Take one more step of a synchronisation [`Self::first_consumer_step`]
    /// started. Each step journals at most one discovery write, and reads
    /// the ownership afresh, so a receipt confirmed between turns stays gone.
    async fn consumer_step(&mut self, step: ConsumerStep) -> Result<Progress, BunError> {
        match step {
            ConsumerStep::Withdraw(publication) => {
                if !self.withdraw_consumer_view().await? {
                    // Requests still hold earlier backends. The journal says
                    // Withdrawing, so the next answer starts over from here.
                    return Ok(Progress::Done(self.consumer_update(false)));
                }
                Ok(Progress::Next(ConsumerStep::ProveReceipts(publication)))
            }
            ConsumerStep::ProveReceipts(publication) => {
                // The journal says Withdrawn: nothing earlier is exposed, so
                // every withdrawal the new view doesn't contradict is proven.
                let mut owner = self
                    .consumer_owner()
                    .cloned()
                    .ok_or_else(|| failure("consumer ownership is missing"))?;
                for receipt in owner.receipts.values_mut() {
                    if !publication.intersects(&receipt.withdrawal) {
                        receipt.phase = ReceiptPhase::Ready;
                    } else if receipt.phase == ReceiptPhase::Ready {
                        return Err(failure(
                            "publication would resurrect a confirmed withdrawal",
                        ));
                    }
                }
                self.save_consumer(owner).await?;
                Ok(Progress::Next(ConsumerStep::Publish(publication)))
            }
            ConsumerStep::Publish(publication) => {
                let mut owner = self
                    .consumer_owner()
                    .cloned()
                    .ok_or_else(|| failure("consumer ownership is missing"))?;
                owner.publications = vec![publication.clone()];
                owner.phase = ConsumerPhase::Publishing;
                self.save_consumer(owner).await?;
                self.apply_consumer_publication(&publication).await?;
                Ok(Progress::Next(ConsumerStep::Activate))
            }
            ConsumerStep::Activate => {
                let mut owner = self
                    .consumer_owner()
                    .cloned()
                    .ok_or_else(|| failure("consumer ownership is missing"))?;
                match owner.phase {
                    ConsumerPhase::Publishing => {
                        owner.phase = ConsumerPhase::Active;
                        self.save_consumer(owner).await?;
                    }
                    ConsumerPhase::Active => {}
                    ConsumerPhase::Withdrawing | ConsumerPhase::Withdrawn => {
                        return Err(failure(
                            "consumer view was withdrawn during its publication",
                        ));
                    }
                }
                Ok(Progress::Next(ConsumerStep::Compact))
            }
            ConsumerStep::Compact => {
                self.compact_consumer_publications().await?;
                Ok(Progress::Done(self.consumer_update(true)))
            }
        }
    }

    /// Take a leader answer, or a local republication, through the loop
    /// (#505). Its first step runs now; the rest run on later turns, one
    /// write each, and whoever asked hears the result after the last one.
    /// While one synchronisation is under way a newer answer waits for it,
    /// and replaces any answer already waiting: the leader's poll sends one
    /// at a time, so an older waiting answer is one its poll gave up on.
    pub(super) async fn request_consumer_sync(&mut self, request: ConsumerRequest) {
        if !self.consumer_syncs.is_idle() {
            if let Some(superseded) = self.consumer_syncs.waiting.replace(request) {
                self.answer_consumer_sync(
                    superseded.answer,
                    Err(failure("superseded by a newer leader answer")),
                )
                .await;
            }
            return;
        }
        self.start_consumer_sync(request).await;
    }

    async fn start_consumer_sync(&mut self, request: ConsumerRequest) {
        let ConsumerRequest {
            generation,
            catalog,
            ingress,
            withdrawals,
            answer,
        } = request;
        let progress = self
            .first_consumer_step(generation, catalog, ingress, withdrawals)
            .await;
        self.advance_consumer_sync(progress, answer).await;
    }

    async fn advance_consumer_sync(
        &mut self,
        progress: Result<Progress, BunError>,
        answer: Option<ConsumerAnswer>,
    ) {
        match progress {
            Ok(Progress::Next(next)) => {
                self.consumer_syncs.current = Some(ConsumerSync { next, answer });
            }
            Ok(Progress::Done(update)) => self.answer_consumer_sync(answer, Ok(update)).await,
            Err(error) => self.answer_consumer_sync(answer, Err(error)).await,
        }
    }

    /// Take one turn's step of the synchronisation under way, or start the
    /// answer waiting behind it.
    pub(super) async fn continue_consumer_sync(&mut self) {
        self.consumer_syncs.stepped_now = true;
        if let Some(sync) = self.consumer_syncs.current.take() {
            let progress = self.consumer_step(sync.next).await;
            self.advance_consumer_sync(progress, sync.answer).await;
        } else if let Some(request) = self.consumer_syncs.waiting.take() {
            self.start_consumer_sync(request).await;
        }
    }

    /// Finish every synchronisation under way or waiting, inline, for a
    /// caller that needs the view settled now.
    pub(super) async fn finish_consumer_syncs(&mut self) {
        while !self.consumer_syncs.is_idle() {
            self.continue_consumer_sync().await;
        }
    }

    /// Tell whoever asked how a synchronisation ended.
    ///
    /// The view lease is renewed here, after the last step, as it was when
    /// the whole synchronisation ran in one turn. A view an earlier step
    /// installed after a lapse is safe until then: the kernel enforces the
    /// lapsed lease, so it routes only locally, and the answer is the
    /// current catalogue, so every remote address it names is live.
    async fn answer_consumer_sync(
        &mut self,
        answer: Option<ConsumerAnswer>,
        result: Result<ConsumerUpdate, BunError>,
    ) {
        let Some(answer) = answer else {
            // A local republication; a later turn tries again.
            if let Err(error) = result {
                eprintln!("bun: consumer view refresh awaits retry: {error}");
                self.consumer_view_stale = true;
            }
            return;
        };
        if matches!(&result, Ok(update) if update.published) {
            self.renew_view_lease(answer.requested_at_ns).await;
        }
        let result = match result {
            Err(error) => {
                let retry = self.consumer_update(false);
                if retry.receipts.is_empty() {
                    Err(error)
                } else {
                    // Capacity or candidate refusal must not starve
                    // already-proven receipts needed to free capacity.
                    eprintln!("bun: consumer publication awaits retry: {error}");
                    Ok(retry)
                }
            }
            success => success,
        };
        let _ = answer.response.send(result);
    }

    /// Replace the kernel and userspace view in place. Unchanged services keep
    /// their entries throughout; only services absent from the new view leave
    /// the kernel, after DNS and ingress have stopped offering them.
    async fn apply_consumer_publication(
        &mut self,
        publication: &ConsumerPublication,
    ) -> Result<(), BunError> {
        let previous = self
            .service_map_tx
            .borrow()
            .resolve_all()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        self.install_consumer_view(&publication.effective_services, &publication.ingress)
            .await?;
        self.lapsed_view = None;
        self.cluster_catalog = publication.catalog.clone();
        self.cluster_catalog_generation = Some(publication.generation);
        for entry in previous.iter().filter(|entry| {
            !publication
                .effective_services
                .iter()
                .any(|current| current.vip == entry.vip && current.port == entry.port)
        }) {
            self.withdraw_discovery_entry(entry).await?;
        }
        self.sync_firewall_ebpf().await;
        Ok(())
    }

    /// Point the kernel, then DNS and Wrapper, at `services`, updating each
    /// service's entry in place.
    async fn install_consumer_view(
        &mut self,
        services: &[ServiceEntry],
        ingress: &[IngressAssignment],
    ) -> Result<(), BunError> {
        let map = ServiceMap::from_snapshot(services).map_err(failure)?;
        let routes = ingress
            .iter()
            .map(|route| {
                (
                    (route.namespace.clone(), route.name.clone()),
                    route.config.clone(),
                )
            })
            .collect();
        let mut table = crate::wrapper::routing::RoutingTable::new();
        table.rebuild(&map, &routes).map_err(failure)?;
        for entry in services {
            self.publish_backend_kernel(&ServiceId::new(&entry.namespace, &entry.app_name), &map)
                .await?;
        }
        *self.routing_table.write().await = table;
        self.service_map_tx.send_replace(map);
        Ok(())
    }

    /// Forget earlier views once requests that captured their removed backends
    /// have released, then mark receipts no remaining view contradicts.
    async fn compact_consumer_publications(&mut self) -> Result<(), BunError> {
        let Some(mut owner) = self.consumer_owner().cloned() else {
            return Ok(());
        };
        let Some(current) = owner.publications.last().cloned() else {
            return Ok(());
        };
        if owner.phase != ConsumerPhase::Active {
            return Ok(());
        }
        let published: std::collections::HashSet<&str> = current
            .effective_services
            .iter()
            .flat_map(|entry| {
                entry
                    .backends
                    .iter()
                    .map(|backend| backend.instance_id.as_str())
            })
            .collect();
        let mut retiring = std::collections::BTreeMap::new();
        for publication in &owner.publications[..owner.publications.len() - 1] {
            for entry in &publication.effective_services {
                for backend in &entry.backends {
                    if !published.contains(backend.instance_id.as_str()) {
                        retiring.insert(backend.instance_id.clone(), entry.app_name.clone());
                    }
                }
            }
        }
        let retiring: Vec<_> = retiring
            .into_iter()
            .map(
                |(instance_id, app_name)| crate::wrapper::draining::DrainCommand {
                    app_name,
                    instance_id,
                    timeout: CONSUMER_DRAIN_TIMEOUT,
                },
            )
            .collect();
        // LOOP-INLINE: in-memory lock, no I/O
        if !self.drains.drain_all(&retiring).await {
            return Ok(());
        }
        owner.publications = vec![current];
        for receipt in owner.receipts.values_mut() {
            if !owner.publications[0].intersects(&receipt.withdrawal) {
                receipt.phase = ReceiptPhase::Ready;
            }
        }
        if Some(&owner) != self.consumer_owner() {
            self.save_consumer(owner).await?;
        }
        Ok(())
    }

    /// Rebuild the published view from the last committed catalogue after a
    /// local change, such as a health transition or a replaced instance,
    /// inline: the caller sees the view settled when this returns. The loop
    /// uses [`Self::start_consumer_refresh`] instead.
    pub(super) async fn refresh_consumer_view(&mut self) -> Result<(), BunError> {
        // Steps of another synchronisation may not interleave with these.
        self.finish_consumer_syncs().await;
        if !self.consumer_view_stale {
            return Ok(());
        }
        // A lapsed view keeps only local backends; only a fresh leader answer
        // brings the rest back. A local change still reaches the local part.
        if !self.view_lease.is_valid() {
            self.fence_lapsed_view().await?;
            self.consumer_view_stale = false;
            return Ok(());
        }
        // Before the first synchronisation after recovery, the next committed
        // catalogue rebuilds the view from current local state anyway.
        let Some(last) = self
            .consumer_owner()
            .filter(|owner| owner.phase == ConsumerPhase::Active)
            .and_then(|owner| owner.publications.last())
            .cloned()
        else {
            self.consumer_view_stale = false;
            return Ok(());
        };
        self.synchronise_consumer(last.generation, last.catalog, last.ingress, vec![])
            .await?;
        self.consumer_view_stale = false;
        Ok(())
    }

    /// The loop's end-of-turn republication after a local change: like
    /// [`Self::refresh_consumer_view`], but its writes run as steps on later
    /// turns. While another synchronisation is under way the view stays
    /// marked stale, and a turn after it finishes starts this one.
    pub(super) async fn start_consumer_refresh(&mut self) {
        if !self.consumer_view_stale || !self.consumer_syncs.is_idle() {
            return;
        }
        if !self.view_lease.is_valid() {
            // Installing the local part journals nothing.
            match self.fence_lapsed_view().await {
                Ok(()) => self.consumer_view_stale = false,
                Err(error) => eprintln!("bun: consumer view refresh awaits retry: {error}"),
            }
            return;
        }
        let Some(last) = self
            .consumer_owner()
            .filter(|owner| owner.phase == ConsumerPhase::Active)
            .and_then(|owner| owner.publications.last())
            .cloned()
        else {
            self.consumer_view_stale = false;
            return;
        };
        // A failure marks it stale again (`answer_consumer_sync`).
        self.consumer_view_stale = false;
        self.start_consumer_sync(ConsumerRequest {
            generation: last.generation,
            catalog: last.catalog,
            ingress: last.ingress,
            withdrawals: vec![],
            answer: None,
        })
        .await;
    }

    pub(super) fn consumer_update(&self, published: bool) -> ConsumerUpdate {
        ConsumerUpdate {
            published,
            receipts: self
                .consumer_owner()
                .map(|owner| {
                    owner
                        .receipts
                        .iter()
                        .filter_map(|(generation, receipt)| {
                            (receipt.phase == ReceiptPhase::Ready).then_some(*generation)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            requires_allocations: self.consumer_controls_views(),
        }
    }

    /// Forget only a locally proven receipt after its exact leader acknowledgement.
    pub(super) async fn confirm_consumer_receipt(
        &mut self,
        generation: u64,
    ) -> Result<(), BunError> {
        let mut owner = self
            .consumer_owner()
            .cloned()
            .ok_or_else(|| failure("consumer ownership is unavailable"))?;
        let Some(receipt) = owner.receipts.get(&generation) else {
            return Ok(());
        };
        if receipt.phase != ReceiptPhase::Ready {
            return Err(failure("consumer receipt has no local withdrawal proof"));
        }
        owner.receipts.remove(&generation);
        self.save_consumer(owner).await
    }

    /// Mark the published view out of date after a local change. The agent
    /// loop republishes it in place; until then the previous view keeps serving.
    pub(super) fn mark_consumer_view_stale(&mut self) -> Result<(), BunError> {
        if self.consumer_owner().is_none() {
            return Err(failure("consumer ownership is uncertain"));
        }
        self.consumer_view_stale = true;
        Ok(())
    }

    async fn clear_consumer_userspace(&mut self) {
        *self.routing_table.write().await = crate::wrapper::routing::RoutingTable::new();
        self.service_map_tx.send_replace(ServiceMap::new());
        self.cluster_catalog = EndpointCatalog::default();
        self.cluster_catalog_generation = None;
        self.cluster_ingress_configs.clear();
        self.lapsed_view = None;
    }

    pub(super) async fn withdraw_consumer_view(&mut self) -> Result<bool, BunError> {
        let owner = self
            .consumer_owner()
            .cloned()
            .ok_or_else(|| failure("consumer ownership is missing"))?;
        if owner.phase != ConsumerPhase::Withdrawing {
            return Err(failure("consumer view is not fenced for withdrawal"));
        }
        self.clear_consumer_userspace().await;
        let mut entries: Vec<_> = owner
            .publications
            .iter()
            .flat_map(|publication| publication.effective_services.iter().cloned())
            .collect();
        // Local journals may contain publication attempts made after the last
        // catalogue poll. They remain owned even if that poll never completed.
        if let DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal) =
            &self.discovery_ownership
        {
            entries.extend(
                journal
                    .inventory()
                    .services
                    .iter()
                    .map(|owner| owner.entry.clone()),
            );
        }
        for entry in &entries {
            self.withdraw_discovery_entry(entry).await?;
            for backend in &entry.backends {
                // LOOP-INLINE: in-memory lock, no I/O
                self.drains
                    .start_drain(&crate::wrapper::draining::DrainCommand {
                        app_name: entry.app_name.clone(),
                        instance_id: backend.instance_id.clone(),
                        timeout: std::time::Duration::ZERO,
                    })
                    .await;
            }
        }
        // LOOP-INLINE: in-memory lock, no I/O
        self.drains.check_completions().await;
        for entry in &entries {
            for backend in &entry.backends {
                // LOOP-INLINE: in-memory lock, no I/O
                if self.drains.is_draining(&backend.instance_id).await {
                    return Ok(false);
                }
            }
        }
        let mut owner = owner;
        owner.phase = ConsumerPhase::Withdrawn;
        self.save_consumer(owner).await?;
        Ok(true)
    }
}

/// Matches the default deploy drain: long enough for ordinary requests to
/// finish, short enough that withdrawal receipts still make progress.
const CONSUMER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn failure(error: impl std::fmt::Display) -> BunError {
    BunError::ClusterPublication(error.to_string())
}
