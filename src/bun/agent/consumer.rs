//! Durable consumer publication, complete view withdrawal and receipt permissions.

use super::discovery_ownership::{Landing, WriteOwner};
use super::{BunAgent, BunError, DiscoveryOwnership, Grill};
use crate::bun::consumer_owners::{
    ConsumerIdentity, ConsumerOwnership, ConsumerPhase, ConsumerPublication, ConsumerReceipt,
    ReceiptPhase,
};
use crate::bun::loop_meter::TurnPart;
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

/// What a synchronisation does next. Each step takes at most one slow
/// await: one discovery write, or the runtime inventory read that builds
/// the candidate view. On the soak's disks a write took about 400 ms, and
/// the five a recovery's first answer needs held one turn for two seconds
/// (#505); after a restart, the inventory read and the first write held
/// one turn for 1.1 s (#603). The steps keep the order the single-turn
/// version had.
///
/// A step waits for its write only until the turn's runtime budget runs
/// out. A write still going then (1.6 s after a restart in the 0.2.0
/// soaks, #890) carries on off the loop, and the step's remainder becomes
/// [`ConsumerStep::Landed`], which runs once the write has landed.
enum ConsumerStep {
    /// Record the answer's withdrawal obligations, and journal its view's
    /// first change, unless it is the view already published.
    Journal {
        publication: ConsumerPublication,
        withdrawals: Vec<EndpointWithdrawalInstruction>,
    },
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
    /// What a step does after its journal write, once that write has landed.
    Landed(AfterWrite),
}

/// The rest of a step whose journal write outlasted its turn.
enum AfterWrite {
    /// Nothing but the next step.
    Then(Box<ConsumerStep>),
    /// Install the view the write recorded, then activate it.
    Install(ConsumerPublication),
    /// The synchronisation is complete.
    Done,
}

impl ConsumerStep {
    /// The view this step, or what it finishes, has yet to publish.
    fn publication(&self) -> Option<&ConsumerPublication> {
        match self {
            ConsumerStep::Journal { publication, .. }
            | ConsumerStep::Withdraw(publication)
            | ConsumerStep::ProveReceipts(publication)
            | ConsumerStep::Publish(publication)
            | ConsumerStep::Landed(AfterWrite::Install(publication)) => Some(publication),
            ConsumerStep::Landed(AfterWrite::Then(step)) => step.publication(),
            ConsumerStep::Activate
            | ConsumerStep::Compact
            | ConsumerStep::Landed(AfterWrite::Done) => None,
        }
    }
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
    /// How the write a [`ConsumerStep::Landed`] waits for ended, once it has.
    landed: Option<Result<(), BunError>>,
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

    /// Keep how the write a [`ConsumerStep::Landed`] waits for ended.
    pub(super) fn write_landed(&mut self, result: Result<(), BunError>) {
        self.landed = Some(result);
    }

    /// Close the turn; true if it took a step.
    pub(super) fn end_turn(&mut self) -> bool {
        self.stepped_last = std::mem::take(&mut self.stepped_now);
        self.stepped_last
    }

    /// What the next turn's step is called, for the turn meter: the
    /// candidate view of a waiting answer, or the next step of the one
    /// under way.
    pub(super) fn next_step_label(&self) -> &'static str {
        let Some(sync) = &self.current else {
            return "consumer_sync/candidate";
        };
        match sync.next {
            ConsumerStep::Journal { .. } => "consumer_sync/journal",
            ConsumerStep::Withdraw(_) => "consumer_sync/withdraw",
            ConsumerStep::ProveReceipts(_) => "consumer_sync/prove_receipts",
            ConsumerStep::Publish(_) => "consumer_sync/publish",
            ConsumerStep::Activate => "consumer_sync/activate",
            ConsumerStep::Compact => "consumer_sync/compact",
            ConsumerStep::Landed(AfterWrite::Install(_)) => "consumer_sync/install",
            ConsumerStep::Landed(_) => "consumer_sync/landed",
        }
    }

    /// The view the synchronisation under way will publish, while the
    /// journal doesn't yet list it as the only one.
    fn pending_publication(&self) -> Option<&ConsumerPublication> {
        self.current.as_ref()?.next.publication()
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

/// Leader acknowledgements of locally proven receipts, and the journal
/// write that forgets them (#1340). A confirmation changes the ownership as
/// it is on disk, so it waits for any write in flight, but never on the
/// loop: it waits here, and the loop takes it in a turn of its own once the
/// journal is free.
#[derive(Default)]
pub(super) struct ReceiptConfirmations {
    /// Acknowledgements the journal hasn't been asked to forget yet.
    waiting: Vec<ReceiptConfirmation>,
    /// Acknowledgements whose write outlasted its turn, answered once it lands.
    forgetting: Vec<ReceiptConfirmation>,
}

/// The leader's exact acknowledgement of one receipt, and whoever waits to
/// hear that the receipt is forgotten.
struct ReceiptConfirmation {
    generation: u64,
    response: oneshot::Sender<Result<(), BunError>>,
}

impl ReceiptConfirmations {
    /// Some acknowledgement waits for the journal to be free.
    pub(super) fn is_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }

    /// Answer the acknowledgements whose write has just landed.
    pub(super) fn write_landed(&mut self, result: &Result<(), BunError>) {
        answer_receipts(std::mem::take(&mut self.forgetting), result);
    }
}

/// Tell each confirmation how its write ended.
fn answer_receipts(confirmations: Vec<ReceiptConfirmation>, result: &Result<(), BunError>) {
    for confirmation in confirmations {
        let answer = match result {
            Ok(()) => Ok(()),
            Err(error) => Err(failure(error)),
        };
        let _ = confirmation.response.send(answer);
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// The consumer's durable ownership. While a step's write is in flight
    /// this is the ownership before it; the view that write records counts
    /// through the step under way ([`Self::own_view_names`]).
    pub(super) fn consumer_owner(&self) -> Option<&ConsumerOwnership> {
        self.discovery_ownership.durable()?.consumer.as_ref()
    }

    pub(super) fn consumer_controls_views(&self) -> bool {
        self.consumer_owner().is_some()
            || (self.cluster.is_some()
                && matches!(self.discovery_ownership, DiscoveryOwnership::Uncertain))
    }

    /// Journal `next` and wait for it to land.
    async fn save_consumer(&mut self, next: ConsumerOwnership) -> Result<(), BunError> {
        self.write_consumer(next, None).await.map(|_| ())
    }

    /// Journal `next` for a consumer step: wait for it until the turn's
    /// runtime budget runs out, then let it go on off the loop (#890).
    async fn journal_consumer(&mut self, next: ConsumerOwnership) -> Result<Landing, BunError> {
        let deadline = self.turn_deadline();
        self.write_consumer(next, Some(deadline)).await
    }

    async fn write_consumer(
        &mut self,
        next: ConsumerOwnership,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Landing, BunError> {
        self.write_consumer_for(next, deadline, WriteOwner::ConsumerStep)
            .await
    }

    /// [`Self::write_consumer`], for `owner` to take over if the write
    /// outlasts `deadline`.
    async fn write_consumer_for(
        &mut self,
        next: ConsumerOwnership,
        deadline: Option<tokio::time::Instant>,
        owner: WriteOwner,
    ) -> Result<Landing, BunError> {
        // `next` was built from the journal as it is: no write in flight.
        if self.discovery_write_in_flight() {
            return Err(failure("a consumer journal write is still in flight"));
        }
        next.validate().map_err(failure)?;
        crate::bun::consumer_owners::validate_transition(self.consumer_owner(), Some(&next))
            .map_err(failure)?;
        self.write_discovery_inventory(
            &ServiceId::new("system", "discovery"),
            |inventory| inventory.consumer = Some(next),
            deadline,
            owner,
        )
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
        let mut ebpf = self.lock_kernel(handle).await;
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
    ///
    /// Outside a turn each step's write is waited out. Inside one a write
    /// still in flight isn't (#1336): the rest of the synchronisation goes
    /// to the loop, which takes it once the write has landed, as it would a
    /// local republication, and this answers `StillRunning`. Call it only
    /// with no synchronisation under way.
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
                Progress::Next(next) => {
                    if self.settle_discovery_write_outside_turn().await == Landing::InFlight {
                        // Nobody hears how it ends; a failure marks the view
                        // stale again (`answer_consumer_sync`).
                        self.consumer_syncs.current = Some(ConsumerSync { next, answer: None });
                        return Err(BunError::StillRunning {
                            work: "a consumer view's journal write".into(),
                        });
                    }
                    progress = self.consumer_step(next).await?;
                }
            }
        }
    }

    /// Build the view a leader answer asks for, from the runtime's launch
    /// inventory and the local service map. This step reads the inventory
    /// and journals nothing; [`Self::consumer_step`] takes the rest, one
    /// write a step (#505, #603).
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
        Ok(Progress::Next(ConsumerStep::Journal {
            publication,
            withdrawals,
        }))
    }

    /// Record a leader answer's withdrawal obligations and journal its
    /// first change: at most one discovery write.
    async fn journal_consumer_answer(
        &mut self,
        publication: ConsumerPublication,
        withdrawals: Vec<EndpointWithdrawalInstruction>,
    ) -> Result<Progress, BunError> {
        // Read afresh, as every later step does: since the candidate turn a
        // receipt may have been confirmed, or the view lease lapsed.
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
            let landing = self.journal_consumer(owner).await?;
            return Ok(then(landing, ConsumerStep::Withdraw(publication)));
        }
        if owner.publications.last() != Some(&publication) {
            // Journalled before the kernel sees it, so after a crash from
            // here on, recovery knows everything that may be exposed.
            owner.publications.push(publication.clone());
            owner.phase = ConsumerPhase::Publishing;
            if self.journal_consumer(owner).await? == Landing::InFlight {
                return Ok(Progress::Next(ConsumerStep::Landed(AfterWrite::Install(
                    publication,
                ))));
            }
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
            let landing = self.journal_consumer(owner).await?;
            return Ok(then(landing, ConsumerStep::Compact));
        }
        self.compact_step().await
    }

    /// Take one more step of a synchronisation [`Self::first_consumer_step`]
    /// started. Each step journals at most one discovery write, and reads
    /// the ownership afresh, so a receipt confirmed between turns stays gone.
    async fn consumer_step(&mut self, step: ConsumerStep) -> Result<Progress, BunError> {
        // Every driver takes a step only once the journal is free (#1336):
        // the loop's branches wait for the landing, and the inline drivers
        // settle the write outside a turn, or stop at it inside one.
        if self.discovery_write_in_flight() {
            return Err(failure("a consumer journal write is still in flight"));
        }
        match step {
            ConsumerStep::Journal {
                publication,
                withdrawals,
            } => self.journal_consumer_answer(publication, withdrawals).await,
            ConsumerStep::Withdraw(publication) => {
                let deadline = self.turn_deadline();
                let Some(landing) = self.withdraw_consumer_view_within(Some(deadline)).await?
                else {
                    // Requests still hold earlier backends. The journal says
                    // Withdrawing, so the next answer starts over from here.
                    return Ok(Progress::Done(self.consumer_update(false)));
                };
                Ok(then(landing, ConsumerStep::ProveReceipts(publication)))
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
                let landing = self.journal_consumer(owner).await?;
                Ok(then(landing, ConsumerStep::Publish(publication)))
            }
            ConsumerStep::Publish(publication) => {
                let mut owner = self
                    .consumer_owner()
                    .cloned()
                    .ok_or_else(|| failure("consumer ownership is missing"))?;
                owner.publications = vec![publication.clone()];
                owner.phase = ConsumerPhase::Publishing;
                if self.journal_consumer(owner).await? == Landing::InFlight {
                    return Ok(Progress::Next(ConsumerStep::Landed(AfterWrite::Install(
                        publication,
                    ))));
                }
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
                        let landing = self.journal_consumer(owner).await?;
                        return Ok(then(landing, ConsumerStep::Compact));
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
            ConsumerStep::Compact => self.compact_step().await,
            ConsumerStep::Landed(after) => {
                // A failed write fenced discovery; nothing it would have
                // recorded may reach the kernel.
                self.consumer_syncs
                    .landed
                    .take()
                    .unwrap_or_else(|| Err(failure("a consumer journal write was lost")))?;
                match after {
                    AfterWrite::Then(step) => Ok(Progress::Next(*step)),
                    AfterWrite::Install(publication) => {
                        self.apply_consumer_publication(&publication).await?;
                        Ok(Progress::Next(ConsumerStep::Activate))
                    }
                    AfterWrite::Done => Ok(Progress::Done(self.consumer_update(true))),
                }
            }
        }
    }

    /// Compact the published views, and finish the synchronisation once
    /// that has landed.
    async fn compact_step(&mut self) -> Result<Progress, BunError> {
        let deadline = self.turn_deadline();
        match self.compact_consumer_publications(Some(deadline)).await? {
            Landing::Landed => Ok(Progress::Done(self.consumer_update(true))),
            Landing::InFlight => Ok(Progress::Next(ConsumerStep::Landed(AfterWrite::Done))),
        }
    }

    /// Take a leader answer, or a local republication, through the loop
    /// (#505). It only queues here: every step, the first included, runs on
    /// a later turn of its own, so the turn that asked takes no slow await
    /// for it (#603). Whoever asked hears the result after the last step.
    /// An answer waits for any synchronisation under way, and replaces any
    /// answer already waiting: the leader's poll sends one at a time, so an
    /// older waiting answer is one its poll gave up on.
    pub(super) async fn request_consumer_sync(&mut self, request: ConsumerRequest) {
        if let Some(superseded) = self.consumer_syncs.waiting.replace(request) {
            self.answer_consumer_sync(
                superseded.answer,
                Err(failure("superseded by a newer leader answer")),
            )
            .await;
        }
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
    /// caller that needs the view settled now. Inside a turn a step's write
    /// in flight isn't waited for (#1336): this stops there with `InFlight`,
    /// and the loop takes the rest once the write has landed.
    pub(super) async fn finish_consumer_syncs(&mut self) -> Landing {
        while !self.consumer_syncs.is_idle() {
            if self.settle_discovery_write_outside_turn().await == Landing::InFlight {
                return Landing::InFlight;
            }
            self.continue_consumer_sync().await;
        }
        Landing::Landed
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
        let started = std::time::Instant::now();
        let installed = self.install_view(services, ingress).await;
        self.loop_meter
            .add_part(TurnPart::ViewInstall, started.elapsed());
        installed
    }

    async fn install_view(
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
        // Skip, never refuse: one app's invalid route must not hold back the
        // view every other app's discovery and ingress depend on (#700).
        if let Err(error) = table.rebuild(&map, &routes) {
            eprintln!("wrapper: consumer view skipped invalid routes: {error}");
        }
        for entry in services {
            self.publish_backend_kernel(&ServiceId::new(&entry.namespace, &entry.app_name), &map)
                .await?;
        }
        let mut current = self.routing_table.write().await;
        table.inherit_local_health(&current);
        *current = table;
        drop(current);
        self.service_map_tx.send_replace(map);
        Ok(())
    }

    /// Forget earlier views once requests that captured their removed backends
    /// have released, then mark receipts no remaining view contradicts.
    /// Waits for the write until `deadline` at most, as a step does.
    async fn compact_consumer_publications(
        &mut self,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Landing, BunError> {
        let Some(mut owner) = self.consumer_owner().cloned() else {
            return Ok(Landing::Landed);
        };
        let Some(current) = owner.publications.last().cloned() else {
            return Ok(Landing::Landed);
        };
        if owner.phase != ConsumerPhase::Active {
            return Ok(Landing::Landed);
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
            return Ok(Landing::Landed);
        }
        owner.publications = vec![current];
        for receipt in owner.receipts.values_mut() {
            if !owner.publications[0].intersects(&receipt.withdrawal) {
                receipt.phase = ReceiptPhase::Ready;
            }
        }
        if Some(&owner) == self.consumer_owner() {
            return Ok(Landing::Landed);
        }
        self.write_consumer(owner, deadline).await
    }

    /// Rebuild the published view from the last committed catalogue after a
    /// local change, such as a health transition or a replaced instance,
    /// inline: the caller sees the view settled when this returns. The loop
    /// uses [`Self::start_consumer_refresh`] instead.
    ///
    /// Inside a turn it waits for no journal write in flight (#1336). It
    /// answers `StillRunning` instead, the synchronisation goes on in the
    /// loop's own turns, and the caller asks again.
    pub(super) async fn refresh_consumer_view(&mut self) -> Result<(), BunError> {
        // Steps of another synchronisation may not interleave with these.
        let finished = self.finish_consumer_syncs().await;
        super::discovery_ownership::still_running_unless_landed(finished, || {
            "a consumer view's journal write".into()
        })?;
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
        // Cleared first: a synchronisation left to the loop marks the view
        // stale again only if it fails, and the caller's next ask then
        // synchronises inline again and hears why.
        self.consumer_view_stale = false;
        let synchronised = self
            .synchronise_consumer(last.generation, last.catalog, last.ingress, vec![])
            .await;
        if let Err(error) = synchronised {
            if !matches!(error, BunError::StillRunning { .. }) {
                self.consumer_view_stale = true;
            }
            return Err(error);
        }
        Ok(())
    }

    /// The loop's end-of-turn republication after a local change: like
    /// [`Self::refresh_consumer_view`], but its inventory read and writes
    /// run as steps on later turns. While another synchronisation is under way the view stays
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
        // A failure marks it stale again (`answer_consumer_sync`). The
        // sync is idle, so this queues without superseding anything; its
        // first step, the inventory read, runs on a turn of its own.
        self.consumer_view_stale = false;
        self.request_consumer_sync(ConsumerRequest {
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

    /// Take the leader's exact acknowledgement of receipt `generation`, and
    /// forget the receipt now if the journal is free. `response` hears once
    /// forgetting it is durable, or why it wasn't forgotten (#1340).
    pub(super) async fn request_receipt_confirmation(
        &mut self,
        generation: u64,
        response: oneshot::Sender<Result<(), BunError>>,
    ) {
        self.receipt_confirmations
            .waiting
            .push(ReceiptConfirmation {
                generation,
                response,
            });
        self.confirm_waiting_receipts().await;
    }

    /// Start the loop's turn for acknowledgements that waited for the journal.
    pub(super) fn begin_receipt_turn(&mut self) -> crate::bun::loop_meter::Turn {
        self.begin_turn(
            crate::bun::loop_meter::LoopBranch::FollowUp,
            Some("consumer_receipt/confirm"),
        )
    }

    /// Forget every waiting receipt the leader acknowledged, in one journal
    /// write. Only a locally proven receipt is forgotten; one that isn't is
    /// refused at once, and one already forgotten is confirmed at once.
    ///
    /// The receipts change as they are on disk, after any write in flight.
    /// Inside a turn that write isn't waited for: the acknowledgements wait,
    /// and the loop comes back once it has landed. The write itself waits
    /// only until the turn's deadline (#1340); one still going carries on
    /// off the loop, and the turn that lands it answers. Until then the
    /// receipt is still on disk, so the leader keeps hearing about it, as
    /// after a crash, and acknowledges it again.
    pub(super) async fn confirm_waiting_receipts(&mut self) {
        if !self.receipt_confirmations.is_waiting() {
            return;
        }
        if self.settle_discovery_write_outside_turn().await == Landing::InFlight {
            return;
        }
        let waiting = std::mem::take(&mut self.receipt_confirmations.waiting);
        let Some(durable) = self.consumer_owner() else {
            answer_receipts(waiting, &Err(failure("consumer ownership is unavailable")));
            return;
        };
        let mut owner = durable.clone();
        let mut forgetting = Vec::new();
        for confirmation in waiting {
            let proven = durable
                .receipts
                .get(&confirmation.generation)
                .map(|receipt| receipt.phase == ReceiptPhase::Ready);
            match proven {
                None => {
                    let _ = confirmation.response.send(Ok(()));
                }
                Some(true) => {
                    owner.receipts.remove(&confirmation.generation);
                    forgetting.push(confirmation);
                }
                Some(false) => {
                    let _ = confirmation.response.send(Err(failure(
                        "consumer receipt has no local withdrawal proof",
                    )));
                }
            }
        }
        if forgetting.is_empty() {
            return;
        }
        let deadline = self.turn_deadline();
        let written = self
            .write_consumer_for(owner, Some(deadline), WriteOwner::ConsumerReceipts)
            .await;
        match written {
            Ok(Landing::InFlight) => self.receipt_confirmations.forgetting.extend(forgetting),
            Ok(Landing::Landed) => answer_receipts(forgetting, &Ok(())),
            Err(error) => answer_receipts(forgetting, &Err(error)),
        }
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

    /// Withdraw every view this node may still expose, then journal it
    /// Withdrawn. False while captured requests still hold earlier backends.
    pub(super) async fn withdraw_consumer_view(&mut self) -> Result<bool, BunError> {
        Ok(self.withdraw_consumer_view_within(None).await?.is_some())
    }

    /// [`Self::withdraw_consumer_view`] for a step: `None` while requests
    /// still hold earlier backends, otherwise how its write went, waited for
    /// until `deadline` at most.
    async fn withdraw_consumer_view_within(
        &mut self,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<Landing>, BunError> {
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
        if let Some(inventory) = self.discovery_ownership.durable() {
            entries.extend(inventory.services.iter().map(|owner| owner.entry.clone()));
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
                    return Ok(None);
                }
            }
        }
        let mut owner = owner;
        owner.phase = ConsumerPhase::Withdrawn;
        self.write_consumer(owner, deadline).await.map(Some)
    }
}

/// Matches the default deploy drain: long enough for ordinary requests to
/// finish, short enough that withdrawal receipts still make progress.
const CONSUMER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Go on to `next`, now if the write landed, or once it has.
fn then(landing: Landing, next: ConsumerStep) -> Progress {
    match landing {
        Landing::Landed => Progress::Next(next),
        Landing::InFlight => Progress::Next(ConsumerStep::Landed(AfterWrite::Then(Box::new(next)))),
    }
}

fn failure(error: impl std::fmt::Display) -> BunError {
    BunError::ClusterPublication(error.to_string())
}
