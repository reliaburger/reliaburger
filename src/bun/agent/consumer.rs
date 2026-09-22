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
            .supervisor
            .grill()
            .launch_inventory()
            .await?
            .ok_or_else(|| failure("consumer publication requires complete runtime inventory"))?;
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
        let mut changed = false;
        if owner.publications.last() != Some(&publication) {
            owner.publications.push(publication.clone());
            changed = true;
        }
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
                    changed = true;
                }
            }
        }
        if owner.receipts.values().any(|receipt| {
            receipt.phase == ReceiptPhase::Ready
                && publication_intersects(&publication, &receipt.withdrawal)
        }) {
            return Err(failure(
                "publication would resurrect a confirmed withdrawal",
            ));
        }
        if changed || owner.phase != ConsumerPhase::Active {
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
                if !publication_intersects(&publication, &receipt.withdrawal) {
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
            *self.routing_table.write().await = table;
            self.service_map_tx.send_replace(services);
            self.cluster_catalog = publication.catalog;
            self.cluster_catalog_generation = Some(generation);
            owner.phase = ConsumerPhase::Active;
            self.save_consumer(owner).await?;
            self.sync_firewall_ebpf().await;
        }
        Ok(self.consumer_update(true))
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

    /// Fence local view changes until the consumer journals and publishes their merged view.
    pub(super) async fn invalidate_consumer_view(&mut self) -> Result<(), BunError> {
        let Some(mut owner) = self.consumer_owner().cloned() else {
            return if self.consumer_controls_views() {
                Err(failure("consumer ownership is uncertain"))
            } else {
                Ok(())
            };
        };
        owner.phase = ConsumerPhase::Withdrawing;
        self.save_consumer(owner).await?;
        self.clear_consumer_userspace().await;
        Ok(())
    }

    async fn clear_consumer_userspace(&mut self) {
        *self.routing_table.write().await = crate::wrapper::routing::RoutingTable::new();
        self.service_map_tx.send_replace(ServiceMap::new());
        self.cluster_catalog = EndpointCatalog::default();
        self.cluster_catalog_generation = None;
        self.cluster_ingress_configs.clear();
    }

    async fn withdraw_consumer_view(&mut self) -> Result<bool, BunError> {
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

fn failure(error: impl std::fmt::Display) -> BunError {
    BunError::ClusterPublication(error.to_string())
}

fn publication_intersects(
    publication: &ConsumerPublication,
    withdrawal: &EndpointWithdrawalInstruction,
) -> bool {
    withdrawal.services.values().any(|removed| {
        publication
            .effective_services
            .iter()
            .any(|entry| removed.retire_vip && entry.vip == removed.service.vip)
            || publication.catalog.services.values().any(|service| {
                service.backends.iter().any(|candidate| {
                    removed.service.backends.iter().any(|original| {
                        candidate.node_id == original.node_id
                            && candidate.node_ip == original.node_ip
                            && candidate.host_port == original.host_port
                            && (original.execution.is_none()
                                || candidate.execution == original.execution)
                    })
                })
            })
    })
}
