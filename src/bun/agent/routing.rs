//! Service routing this node publishes: backends and firewall rules in the
//! eBPF maps, the service catalogue, the routing table and the firewall.

use super::*;

#[cfg(all(test, not(all(feature = "ebpf", target_os = "linux"))))]
thread_local! {
    /// The VIPs whose whole kernel entry an agent on this thread withdrew,
    /// so tests without the eBPF data path can still see a withdrawal.
    /// Tests run on a current-thread runtime, and a thread runs one test at
    /// a time; read it with [`take_whole_entry_withdrawals`].
    static WHOLE_ENTRY_WITHDRAWALS: std::cell::RefCell<Vec<crate::onion::vip::VirtualIP>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Drain the whole-entry withdrawals recorded on this thread.
#[cfg(all(test, not(all(feature = "ebpf", target_os = "linux"))))]
pub(super) fn take_whole_entry_withdrawals() -> Vec<crate::onion::vip::VirtualIP> {
    WHOLE_ENTRY_WITHDRAWALS.with_borrow_mut(std::mem::take)
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Republish the current `DnsNxdomain` fault set to the DNS responder.
    ///
    /// Rebuilt from the fault registry so it always reflects reality after an
    /// apply, clear, or expiry. Namespace-qualified identities prevent an
    /// authorised fault in one tenant from affecting another tenant's service.
    pub(super) fn publish_dns_faults(&self) {
        let faults = self
            .fault_registry
            .iter()
            .filter(|rule| {
                matches!(
                    rule.fault_type,
                    crate::smoker::types::FaultType::DnsNxdomain
                )
            })
            .filter_map(|rule| {
                Some((
                    crate::onion::service_id::ServiceId::new(
                        rule.namespace.as_ref()?,
                        &rule.target_service,
                    ),
                    rule.expires_at_ns,
                ))
            });
        let _ = self
            .dns_faults_tx
            .send(crate::onion::dns::DnsFaultState::from_faults(faults));
    }

    /// Require successful kernel publication before acknowledging deployment.
    pub(super) async fn publish_backend_ebpf(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        let services = self.service_map.clone();
        self.publish_backend_snapshot(id, &services).await
    }

    /// Journal attempted routing before acknowledging its kernel publication.
    pub(super) async fn publish_backend_snapshot(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
        services: &crate::onion::service_map::ServiceMap,
    ) -> Result<(), BunError> {
        self.persist_discovery_publication(id, services).await?;
        if self.consumer_controls_views() {
            return self.mark_consumer_view_stale();
        }
        self.publish_backend_kernel(id, services).await
    }

    /// Publish a validated candidate before exposing it to userspace readers.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn publish_backend_kernel(
        &self,
        id: &crate::onion::service_id::ServiceId,
        services: &crate::onion::service_map::ServiceMap,
    ) -> Result<(), BunError> {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return Ok(());
        };
        let Some(entry) = services.resolve(id).cloned() else {
            return Ok(());
        };
        let bpf = crate::onion::ebpf::maps::BpfServiceMap::for_consumer(
            self.cluster
                .as_ref()
                .map_or("", |cluster| cluster.local_node_id.0.as_str()),
        );
        let mut ebpf = handle.lock().await;
        bpf.update_backends_bpf(&mut ebpf, entry.vip, entry.port, &entry)
            .map_err(|error| BunError::BackendPublication {
                service: id.clone(),
                reason: error.to_string(),
            })
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn publish_backend_kernel(
        &self,
        _id: &crate::onion::service_id::ServiceId,
        _services: &crate::onion::service_map::ServiceMap,
    ) -> Result<(), BunError> {
        Ok(())
    }

    /// Withdraw a service's backend and destination grants before releasing
    /// its allocated VIP. A failed removal retains the original service entry.
    /// A no-op without the eBPF data path loaded.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn withdraw_service_ebpf(
        &self,
        id: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        // Read the VIP + port straight from the live entry: the VIP is
        // whatever the map allocated (which may have probed off the natural
        // hash on a collision), so we must not re-derive it here.
        let Some(entry) = self.service_map.resolve(id) else {
            return Ok(());
        };
        self.withdraw_discovery_entry(entry).await
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn withdraw_discovery_entry(
        &self,
        entry: &crate::onion::types::ServiceEntry,
    ) -> Result<(), BunError> {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return Ok(());
        };
        let id = crate::onion::service_id::ServiceId::new(&entry.namespace, &entry.app_name);
        let (vip, port, destination) = (entry.vip, entry.port, entry.app_id);
        let bpf = crate::onion::ebpf::maps::BpfServiceMap::new();
        let mut ebpf = handle.lock().await;
        bpf.remove_backends_bpf(&mut ebpf, vip, port)
            .map_err(|error| BunError::BackendRetirement {
                service: id.clone(),
                reason: error.to_string(),
            })?;
        crate::sesame::firewall::delete_destination_firewall_state(&mut ebpf.bpf, destination)
            .map_err(|error| BunError::DestinationRetirement {
                service: id.clone(),
                reason: error.to_string(),
            })
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn withdraw_service_ebpf(
        &self,
        id: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        let Some(entry) = self.service_map.resolve(id) else {
            return Ok(());
        };
        self.withdraw_discovery_entry(entry).await
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn withdraw_discovery_entry(
        &self,
        _entry: &crate::onion::types::ServiceEntry,
    ) -> Result<(), BunError> {
        #[cfg(test)]
        WHOLE_ENTRY_WITHDRAWALS.with_borrow_mut(|withdrawn| withdrawn.push(_entry.vip));
        Ok(())
    }

    /// Reconcile the namespace-firewall eBPF maps against current state (NET5).
    ///
    /// Writes `cgroup_namespace_map` (cgroup → namespace) for every running
    /// instance — which is what makes the connect hook enforce cross-namespace
    /// isolation at all: with the source's namespace unknown the hook lets
    /// every connection through. Writes `firewall_map` for each explicit
    /// cross-namespace `allow_from` rule. Both maps are rebuilt from scratch
    /// each call (a new instance of app A changes rules wherever A is a
    /// *source*), deleting keys no longer desired. A no-op without eBPF.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn sync_firewall_ebpf(&mut self) {
        if self.egress_store_uncertain {
            return;
        }
        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };

        // cgroup id(s) per (namespace, app), from currently-running instances.
        // Keying by the namespace-qualified identity — not the bare app name —
        // is what stops same-named apps in different namespaces from sharing a
        // firewall rule or a namespace mapping (H9). Collect the pairs first so
        // the `list_instances` borrow is released before the async workload-identity lookups.
        let pairs: Vec<((String, String), InstanceId, bool)> = self
            .supervisor
            .list_instances()
            .into_iter()
            .map(|i| {
                (
                    (i.namespace.clone(), i.app_name.clone()),
                    i.id.clone(),
                    i.is_being_created(),
                )
            })
            .collect();
        let mut cgroup_ids: std::collections::HashMap<(String, String), Vec<u64>> =
            std::collections::HashMap::new();
        // Until a sync completes, the tick tries again (#351, stage 3).
        self.namespace_firewall_stale = true;
        let deadline = self.turn_deadline();
        for (key, id, being_created) in pairs {
            if let Some(owner) = self.egress_bindings.get(&id)
                && owner.phase == PolicyPhase::Owned
                && owner.source_namespace.is_some()
            {
                cgroup_ids.entry(key).or_default().push(owner.cgroup_id);
                continue;
            }
            // No cgroup exists yet, and asking the runtime would hold the
            // agent loop until the instance's image pull finishes (Z6.7).
            if being_created {
                continue;
            }
            let cgroup =
                tokio::time::timeout_at(deadline, self.supervisor.grill().workload_cgroup(&id))
                    .await;
            match cgroup {
                Ok(Ok(Some(cgroup))) => cgroup_ids.entry(key).or_default().push(cgroup),
                Ok(Ok(None)) => {}
                // Unavailable source evidence cannot authorise erasing
                // previously installed namespace/firewall bindings.
                Ok(Err(error)) => {
                    eprintln!("sesame: source identity for {id} is unavailable: {error}");
                    return;
                }
                Err(_) => {
                    eprintln!(
                        "sesame: source identity for {id} did not arrive within the turn; the next tick retries"
                    );
                    return;
                }
            }
        }

        let services: Vec<crate::onion::types::ServiceEntry> = self
            .merged_service_map()
            .resolve_all()
            .into_iter()
            .cloned()
            .collect();
        let ns_entries = crate::sesame::firewall::resolve_cgroup_namespace_entries(&cgroup_ids);
        let fw_entries = crate::sesame::firewall::rules_to_bpf_entries(
            &crate::sesame::firewall::resolve_firewall_rules(&services, &cgroup_ids),
        );

        let mut ebpf = handle.lock().await;
        if let Err(error) = crate::sesame::firewall::reconcile_firewall_maps(
            &mut ebpf.bpf,
            &ns_entries,
            &fw_entries,
            &mut self.cgroup_ns_bpf_keys,
            &mut self.firewall_bpf_keys,
        ) {
            eprintln!("sesame: firewall reconciliation failed: {error}");
        } else {
            self.namespace_firewall_stale = false;
        }
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn sync_firewall_ebpf(&mut self) {}

    /// Register a rolled-out app's service and its replacement backends. The
    /// caller restores the previous reservation if this refuses.
    pub(super) fn register_replacement_service(
        &mut self,
        service_id: &crate::onion::service_id::ServiceId,
        port: u16,
        spec: &AppSpec,
        new_ids: &[InstanceId],
        new_ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        new_ips: &std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
    ) -> Result<(), BunError> {
        let firewall = spec
            .firewall
            .as_ref()
            .filter(|firewall| !firewall.allow_from.is_empty())
            .map(|firewall| firewall.allow_from.clone());
        self.register_local_service(service_id, port, firewall)?;
        for new_id in new_ids {
            let Some(host_port) = new_ports.get(new_id).copied().flatten() else {
                continue;
            };
            let backend = self.local_backend(
                new_id,
                service_id,
                new_ips.get(new_id).copied().flatten(),
                host_port,
                true,
            );
            self.service_map
                .add_backend(service_id, backend)
                .map_err(|error| BunError::BackendPublication {
                    service: service_id.clone(),
                    reason: error.to_string(),
                })?;
        }
        Ok(())
    }

    /// Prepare every view before replacing any confirmed cluster publication.
    pub(super) async fn publish_cluster_catalogue(
        &mut self,
        generation: u64,
        catalog: crate::onion::catalog::EndpointCatalog,
        ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
    ) -> Result<(), BunError> {
        if self.consumer_controls_views() {
            return Err(BunError::ClusterPublication(
                "durable consumer publication requires withdrawal instructions".into(),
            ));
        }
        if (generation == 0 && !catalog.is_empty())
            || self.cluster_catalog_generation.is_some_and(|confirmed| {
                generation < confirmed
                    || (generation == confirmed && catalog != self.cluster_catalog)
            })
        {
            return Err(BunError::ClusterPublication(
                "catalogue generation is stale or conflicts with confirmed publication".into(),
            ));
        }
        catalog
            .validate_allocations()
            .map_err(|error| BunError::ClusterPublication(error.to_string()))?;
        let cluster_ingress: std::collections::HashMap<_, _> = ingress
            .into_iter()
            .map(|route| ((route.namespace, route.name), route.config))
            .collect();
        let local_name = self
            .cluster
            .as_ref()
            .map(|cluster| cluster.local_node_id.0.as_str());
        let merged = self
            .service_map
            .with_cluster_catalog_excluding_node(&catalog, local_name);
        let entries: Vec<_> = merged.resolve_all().into_iter().cloned().collect();
        crate::onion::service_map::ServiceMap::from_snapshot(&entries)
            .map_err(|error| BunError::ClusterPublication(error.to_string()))?;
        if self.cluster_catalog == catalog && self.cluster_ingress_configs == cluster_ingress {
            // Even an identical catalogue can advance after an intermediate
            // publication. Delayed replies must not regress that confirmation.
            self.cluster_catalog_generation = Some(generation);
            return Ok(());
        }
        let mut ingress = self.ingress_configs.clone();
        ingress.extend(cluster_ingress.clone());
        let mut candidate = crate::wrapper::routing::RoutingTable::new();
        candidate
            .rebuild(&merged, &ingress)
            .map_err(|error| BunError::ClusterPublication(error.to_string()))?;

        // Readers may retain old request guards. This commits the new views,
        // but is not evidence that those older requests have drained.
        let mut table = self.routing_table.write().await;
        *table = candidate;
        self.cluster_catalog = catalog;
        self.cluster_catalog_generation = Some(generation);
        self.cluster_ingress_configs = cluster_ingress;
        self.service_map_tx.send_replace(merged);
        Ok(())
    }

    pub(super) fn merged_service_map(&self) -> crate::onion::service_map::ServiceMap {
        if self.consumer_controls_views() {
            return self.service_map_tx.borrow().clone();
        }
        // Membership can lag or omit a non-voter. Local retirement must not
        // depend on the council having already learned this node's identity.
        let local_name = self
            .cluster
            .as_ref()
            .map(|cluster| cluster.local_node_id.0.as_str());
        self.service_map
            .with_cluster_catalog_excluding_node(&self.cluster_catalog, local_name)
    }

    /// Use the container port for direct netns traffic, and the published port
    /// when the runtime shares the host network.
    pub(super) fn local_backend(
        &self,
        instance_id: &InstanceId,
        service: &crate::onion::service_id::ServiceId,
        container_ip: Option<std::net::Ipv4Addr>,
        host_port: u16,
        healthy: bool,
    ) -> crate::onion::types::BackendInstance {
        let port = if container_ip.is_some() {
            self.deployed_specs
                .get(&(service.name.clone(), service.namespace.clone()))
                .and_then(|spec| spec.port)
                .unwrap_or(host_port)
        } else {
            host_port
        };
        crate::onion::types::BackendInstance {
            instance_id: instance_id.0.clone(),
            node_ip: container_ip.unwrap_or(std::net::Ipv4Addr::LOCALHOST),
            host_port: port,
            healthy,
            local: true,
        }
    }

    /// Rebuild the Wrapper routing table from the current service map
    /// and ingress configs.
    ///
    /// Resolution uses the *merged* view: the local service map overlaid
    /// with the replicated cluster catalogue (12b.4), so both DNS and the
    /// ingress routing table can reach services whose backends live on other
    /// nodes. The local map alone still drives eBPF backend-map syncing —
    /// this merge only affects what DNS/ingress resolve.
    pub(super) async fn rebuild_routing_table(&self) {
        if self.consumer_controls_views() {
            return;
        }
        let merged = self.merged_service_map();

        let mut table = self.routing_table.write().await;
        // Invalid ingress configs (unsupported TLS mode, zero/overflow rate)
        // are rejected here: their routes are skipped rather than installed,
        // so a bad app can't serve TLS traffic in plaintext or divide by zero.
        let mut ingress = self.ingress_configs.clone();
        ingress.extend(self.cluster_ingress_configs.clone());
        if let Err(e) = table.rebuild(&merged, &ingress) {
            eprintln!("wrapper: ingress routing rebuild rejected some routes: {e}");
        }
        drop(table);

        // Retain the latest view even before the first DNS subscriber attaches.
        self.service_map_tx.send_replace(merged);
    }

    /// Reconcile the perimeter firewall if cluster membership changed. The
    /// `nft` subprocess runs off the loop; until it reports back, the tick
    /// leaves the firewall alone.
    pub(super) fn reconcile_firewall(&mut self) {
        if !self.perimeter_config.enabled || self.firewall_applying.is_some() {
            return;
        }

        // Collect cluster node IPs from gossip membership. Reconcile when
        // the *set* changes — a node swap keeps the count constant (M18) —
        // and always on the first pass (`None`), so a standalone node with
        // no peers still gets the firewall applied.
        let cluster_nodes = self.collect_cluster_node_ips();
        if self.last_firewall_nodes.as_ref() == Some(&cluster_nodes) {
            return;
        }

        let ruleset = match crate::firewall::rules::generate_ruleset(
            &self.perimeter_config,
            &cluster_nodes,
        ) {
            Ok(ruleset) => ruleset,
            Err(e) => {
                // A malformed operator CIDR never reaches nft (NET8); the
                // previous ruleset stays in force.
                eprintln!("warning: firewall ruleset generation failed: {e}");
                return;
            }
        };

        self.spawn_perimeter_apply(ruleset, cluster_nodes);
    }

    /// Withdraw local routing and poll request release without blocking the agent loop.
    pub(super) async fn poll_instance_withdrawal(
        &mut self,
        id: &InstanceId,
        timeout: std::time::Duration,
    ) -> Result<bool, BunError> {
        self.withdraw_instance_backend(id).await?;
        // LOOP-INLINE: in-memory lock, no I/O
        Ok(self
            .drains
            .drain_all(&[crate::wrapper::draining::DrainCommand {
                app_name: String::new(),
                instance_id: id.0.clone(),
                timeout,
            }])
            .await)
    }

    /// Add one freshly-healthy replacement to the service map and rebuild the
    /// routing table, so traffic moves onto it before anything old retires (M7).
    pub(super) async fn publish_new_backend(
        &mut self,
        app_name: &str,
        namespace: &str,
        new_id: &InstanceId,
        host_port: Option<u16>,
        container_ip: Option<std::net::Ipv4Addr>,
        has_port: bool,
    ) -> Result<(), BunError> {
        if !has_port {
            return Ok(());
        }
        let Some(host_port) = host_port else {
            return Err(BunError::BackendPublication {
                service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                reason: "replacement has no allocated port".into(),
            });
        };
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        let backend = self.local_backend(new_id, &service_id, container_ip, host_port, true);
        let mut candidate = self.service_map.clone();
        candidate
            .add_backend(&service_id, backend)
            .map_err(|error| BunError::BackendPublication {
                service: service_id.clone(),
                reason: error.to_string(),
            })?;
        self.publish_backend_snapshot(&service_id, &candidate)
            .await?;
        self.service_map = candidate;
        self.rebuild_routing_table().await;
        Ok(())
    }

    /// Confirm one backend's withdrawal before runtime cleanup can reuse its address.
    pub(super) async fn withdraw_instance_backend(
        &mut self,
        id: &InstanceId,
    ) -> Result<(), BunError> {
        let Some(owner) = self.supervisor.get_instance(id) else {
            return Ok(());
        };
        let service = crate::onion::service_id::ServiceId::new(&owner.namespace, &owner.app_name);
        let Some(mut entry) = self.service_map.resolve(&service).cloned() else {
            return Ok(());
        };
        let had_backend = entry
            .backends
            .iter()
            .any(|backend| backend.instance_id == id.0);
        entry.backends.retain(|backend| backend.instance_id != id.0);
        if self.consumer_controls_views() {
            self.mark_consumer_view_stale()?;
            // A prior attempt may have removed the local backend before remote
            // consumers confirmed. Remote receipts, not this return, prove release.
            if had_backend {
                self.service_map
                    .remove_backend(&service, &id.0)
                    .map_err(|error| BunError::BackendRetirement {
                        service,
                        reason: error.to_string(),
                    })?;
            }
            return Ok(());
        }
        // Keep the original userspace owner on refusal. A retry must still know
        // the exact allocated key and the backend whose removal is outstanding.
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(handle) = self.onion_ebpf.as_ref() {
            let mut ebpf = handle.lock().await;
            let map = crate::onion::ebpf::maps::BpfServiceMap::for_consumer(
                self.cluster
                    .as_ref()
                    .map_or("", |cluster| cluster.local_node_id.0.as_str()),
            );
            let failure =
                |error: crate::onion::ebpf::maps::BpfMapError| BunError::BackendRetirement {
                    service: service.clone(),
                    reason: error.to_string(),
                };
            // A missing userspace backend is not evidence that an earlier kernel
            // rewrite succeeded. Conversely, never recreate a confirmed absent key.
            if map
                .read_backends(&mut ebpf, entry.vip, entry.port)
                .map_err(failure)?
                .is_some()
            {
                map.update_backends_bpf(&mut ebpf, entry.vip, entry.port, &entry)
                    .map_err(failure)?;
            }
        }
        if had_backend {
            self.service_map
                .remove_backend(&service, &id.0)
                .map_err(|error| BunError::BackendRetirement {
                    service,
                    reason: error.to_string(),
                })?;
        }
        self.rebuild_routing_table().await;
        Ok(())
    }
}
