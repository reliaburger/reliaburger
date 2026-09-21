//! Startup recovery of original standalone discovery ownership.

use super::{BunAgent, BunError, ContainerState, DiscoveryOwnership, Grill};
use crate::bun::discovery_owners::{DiscoveryInventory, DiscoveryJournal, ReferencePhase};
use crate::grill::{RuntimeLaunch, records::InstanceRecord};
use crate::onion::{service_id::ServiceId, service_map::ServiceMap};

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Recover standalone ownership at startup after all previous node consumers stop.
    /// Exclude runtime registration until adoption finishes. Historical health is never
    /// published; runtime adoption and current policy must independently succeed.
    pub async fn recover_discovery_ownership(
        &mut self,
        directory: &std::path::Path,
    ) -> Result<(), BunError> {
        if !matches!(self.discovery_ownership, DiscoveryOwnership::Disabled)
            || !self.service_map.resolve_all().is_empty()
            || !self.supervisor.list_instances().is_empty()
            || self.cluster.is_some()
            || self.records_dir.is_none()
        {
            return Err(BunError::AdoptionState(
                "discovery recovery requires an empty standalone agent at startup".into(),
            ));
        }
        self.discovery_ownership = DiscoveryOwnership::Uncertain;
        let journal = DiscoveryJournal::open_async(directory)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        let launches = self
            .supervisor
            .grill()
            .launch_inventory()
            .await?
            .ok_or_else(|| {
                BunError::AdoptionState(
                    "discovery recovery requires complete runtime inventory".into(),
                )
            })?;
        let inventory = journal
            .reconcile_runtime_inventory(&launches)
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.validate_discovery_kernel_inventory(&inventory).await?;
        let entries: Vec<_> = inventory
            .services
            .iter()
            .map(|owner| {
                let mut entry = owner.entry.clone();
                entry.backends.clear();
                entry
            })
            .collect();
        let reserved = ServiceMap::from_snapshot(&entries)
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        let journal = journal
            .persist(inventory)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.service_map = reserved;
        // Keep the agent fenced until every original kernel key and destination
        // grant is withdrawn. A failed or cancelled step cannot enable adoption.
        for entry in &entries {
            self.withdraw_service_ebpf(&ServiceId::new(&entry.namespace, &entry.app_name))
                .await?;
        }
        self.network_references = journal
            .inventory()
            .references
            .iter()
            .map(|owner| (owner.reference.instance_id.clone(), owner.reference.clone()))
            .collect();
        self.discovery_ownership = DiscoveryOwnership::Recovered(journal);
        Ok(())
    }

    /// Refuse changed startup evidence before any runtime or release mutation.
    pub(super) fn validate_recovered_discovery(
        &self,
        records: &[InstanceRecord],
        launches: Option<&[RuntimeLaunch]>,
    ) -> Result<(), BunError> {
        let DiscoveryOwnership::Recovered(journal) = &self.discovery_ownership else {
            return Ok(());
        };
        if self.cluster.is_some() {
            return Err(BunError::AdoptionState(
                "remote discovery recovery is unconfirmed".into(),
            ));
        }
        let launches = launches.ok_or_else(|| {
            BunError::AdoptionState("complete runtime inventory is missing".into())
        })?;
        let reconciled = journal
            .reconcile_runtime_inventory(launches)
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        if reconciled.references.len() != journal.inventory().references.len() {
            return Err(BunError::AdoptionState(
                "runtime inventory changed during discovery recovery".into(),
            ));
        }
        for record in records {
            if !launches.iter().any(|launch| {
                launch.instance_id.0 == record.instance_id && launch.spec == record.oci_spec
            }) {
                return Err(BunError::AdoptionState(
                    "adoption record conflicts with original runtime inventory".into(),
                ));
            }
        }
        Ok(())
    }

    /// Complete exact permissions whose runtime acknowledgement may have been lost.
    pub(super) async fn replay_discovery_releases(&mut self) -> Result<(), BunError> {
        let DiscoveryOwnership::Recovered(journal) = &self.discovery_ownership else {
            return Ok(());
        };
        let pending: Vec<_> = journal
            .inventory()
            .references
            .iter()
            .filter(|owner| owner.phase == ReferencePhase::ReleaseAuthorised)
            .map(|owner| owner.reference.instance_id.clone())
            .collect();
        for id in pending {
            self.release_network_reference(&id).await?;
        }
        Ok(())
    }

    /// Publish only positively adopted runtimes and retire confirmed empty allocations.
    pub(super) async fn finish_discovery_recovery(&mut self) -> Result<(), BunError> {
        if !matches!(self.discovery_ownership, DiscoveryOwnership::Recovered(_)) {
            return Ok(());
        }
        let entries: Vec<_> = self
            .service_map
            .resolve_all()
            .into_iter()
            .cloned()
            .collect();
        for entry in entries {
            let service = ServiceId::new(&entry.namespace, &entry.app_name);
            let instances: Vec<_> = self
                .supervisor
                .list_instances()
                .into_iter()
                .filter(|instance| {
                    instance.namespace == service.namespace
                        && instance.app_name == service.name
                        && matches!(
                            instance.state,
                            ContainerState::Running | ContainerState::HealthWait
                        )
                })
                .map(|instance| {
                    (
                        instance.id.clone(),
                        instance.host_port,
                        instance.health_config.is_some(),
                    )
                })
                .collect();
            if instances.is_empty() {
                self.retire_discovery_service(&service).await?;
                self.service_map
                    .unregister(&service)
                    .map_err(|error| BunError::AdoptionState(error.to_string()))?;
                continue;
            }
            let mut candidate = self.service_map.clone();
            for (id, port, has_health) in instances {
                let ip = self
                    .supervisor
                    .grill()
                    .container_ip(&id)
                    .await
                    .ok_or_else(|| {
                        BunError::AdoptionState(format!(
                            "adopted runtime {id} has no confirmed container address"
                        ))
                    })?;
                let port = port.ok_or_else(|| {
                    BunError::AdoptionState(format!(
                        "adopted runtime {id} has no original published port"
                    ))
                })?;
                let backend = self.local_backend(&id, &service, Some(ip), port, !has_health);
                candidate
                    .add_backend(&service, backend)
                    .map_err(|error| BunError::AdoptionState(error.to_string()))?;
                if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                    instance.container_ip = Some(ip);
                    if has_health {
                        instance.state = ContainerState::HealthWait;
                    }
                }
            }
            self.publish_backend_snapshot(&service, &candidate).await?;
            self.service_map = candidate;
        }
        self.sync_firewall_ebpf().await;
        self.rebuild_routing_table().await;
        Ok(())
    }

    async fn validate_discovery_kernel_inventory(
        &self,
        inventory: &DiscoveryInventory,
    ) -> Result<(), BunError> {
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(handle) = &self.onion_ebpf {
            use crate::onion::types::{BackendKey, BackendValue};
            let mut handle = handle.lock().await;
            let map = handle
                .bpf
                .map_mut("backend_map")
                .ok_or_else(|| BunError::AdoptionState("kernel backend map is missing".into()))?;
            let map: aya::maps::HashMap<_, BackendKey, BackendValue> =
                aya::maps::HashMap::try_from(map)
                    .map_err(|error| BunError::AdoptionState(error.to_string()))?;
            for entry in map.iter() {
                let (key, value) =
                    entry.map_err(|error| BunError::AdoptionState(error.to_string()))?;
                if !inventory.services.iter().any(|owner| {
                    owner.entry.vip.to_network_byte_order() == key.vip
                        && owner.entry.port.to_be() == key.port
                        && owner.entry.app_id == value.app_id
                        && owner.entry.namespace_id == value.namespace_id
                }) {
                    return Err(BunError::AdoptionState(
                        "kernel backend has no original discovery owner".into(),
                    ));
                }
            }
            return Ok(());
        }
        let _ = inventory;
        if self.supervisor.grill().honours_cgroup_path() {
            return Err(BunError::AdoptionState(
                "owned container discovery recovery requires kernel inspection".into(),
            ));
        }
        Ok(())
    }
}
