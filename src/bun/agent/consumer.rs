//! Durable consumer publication, complete view withdrawal and receipt permissions.

use super::{BunAgent, BunError, DiscoveryOwnership, Grill};
use crate::bun::consumer_owners::{
    ConsumerIdentity, ConsumerOwnership, ConsumerPhase, ConsumerPublication, ConsumerReceipt,
    ReceiptPhase,
};
use crate::cluster::orchestrate::IngressAssignment;
use crate::onion::{
    catalog::EndpointCatalog, service_id::ServiceId, service_map::ServiceMap,
    withdrawal::EndpointWithdrawalInstruction,
};

/// Confirmed publication and original withdrawals safe to acknowledge remotely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerUpdate {
    /// False while captured requests still retain an earlier view.
    pub published: bool,
    /// Original generations with durable local withdrawal proof.
    pub receipts: Vec<u64>,
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
        Ok(())
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

    pub(super) async fn synchronise_consumer(
        &mut self,
        generation: u64,
        catalog: EndpointCatalog,
        ingress: Vec<IngressAssignment>,
        withdrawals: Vec<EndpointWithdrawalInstruction>,
    ) -> Result<ConsumerUpdate, BunError> {
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled) {
            self.publish_cluster_catalogue(generation, catalog, ingress)
                .await?;
            return Ok(ConsumerUpdate {
                published: true,
                receipts: vec![],
            });
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
            return self.republish_after_withdrawal(owner, publication).await;
        }
        if owner.publications.last() != Some(&publication) {
            owner.publications.push(publication.clone());
            owner.phase = ConsumerPhase::Publishing;
            self.save_consumer(owner.clone()).await?;
            self.apply_consumer_publication(&publication).await?;
            owner.phase = ConsumerPhase::Active;
        }
        if Some(&owner) != self.consumer_owner() {
            self.save_consumer(owner).await?;
        }
        self.compact_consumer_publications().await?;
        Ok(self.consumer_update(true))
    }

    /// Recovery path: nothing from an earlier view may remain exposed before
    /// the new one publishes, so withdraw everything and wait for release.
    async fn republish_after_withdrawal(
        &mut self,
        mut owner: ConsumerOwnership,
        publication: ConsumerPublication,
    ) -> Result<ConsumerUpdate, BunError> {
        if owner.publications.last() != Some(&publication) {
            owner.publications.push(publication.clone());
        }
        owner.phase = ConsumerPhase::Withdrawing;
        self.save_consumer(owner).await?;
        if !self.withdraw_consumer_view().await? {
            return Ok(self.consumer_update(false));
        }
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
        self.save_consumer(owner.clone()).await?;
        owner.publications = vec![publication.clone()];
        owner.phase = ConsumerPhase::Publishing;
        self.save_consumer(owner.clone()).await?;
        self.apply_consumer_publication(&publication).await?;
        owner.phase = ConsumerPhase::Active;
        self.save_consumer(owner).await?;
        Ok(self.consumer_update(true))
    }

    /// Replace the kernel and userspace view in place. Unchanged services keep
    /// their entries throughout; only services absent from the new view leave
    /// the kernel, after DNS and ingress have stopped offering them.
    async fn apply_consumer_publication(
        &mut self,
        publication: &ConsumerPublication,
    ) -> Result<(), BunError> {
        let services =
            ServiceMap::from_snapshot(&publication.effective_services).map_err(failure)?;
        let routes = publication
            .ingress
            .iter()
            .map(|route| {
                (
                    (route.namespace.clone(), route.name.clone()),
                    route.config.clone(),
                )
            })
            .collect();
        let mut table = crate::wrapper::routing::RoutingTable::new();
        table.rebuild(&services, &routes).map_err(failure)?;
        for entry in &publication.effective_services {
            self.publish_backend_kernel(
                &ServiceId::new(&entry.namespace, &entry.app_name),
                &services,
            )
            .await?;
        }
        let previous = self
            .service_map_tx
            .borrow()
            .resolve_all()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        *self.routing_table.write().await = table;
        self.service_map_tx.send_replace(services);
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
    /// local change, such as a health transition or a replaced instance.
    pub(super) async fn refresh_consumer_view(&mut self) -> Result<(), BunError> {
        if !self.consumer_view_stale {
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
                self.drains
                    .start_drain(&crate::wrapper::draining::DrainCommand {
                        app_name: entry.app_name.clone(),
                        instance_id: backend.instance_id.clone(),
                        timeout: std::time::Duration::ZERO,
                    })
                    .await;
            }
        }
        self.drains.check_completions().await;
        for entry in &entries {
            for backend in &entry.backends {
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
