//! Instance networking on the loop: the network references a launch
//! retains and releases, egress programming and enforcement, and the kernel
//! networking sweep.

use super::*;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Enforce the current kernel boundary and publish this tick's capabilities.
    pub(super) async fn refresh_egress_readiness(&mut self) {
        let egress = self.enforce_live_egress_or_stop().await;
        if let Some(readiness) = self.readiness.clone() {
            // LOOP-INLINE: in-memory lock, no I/O
            readiness
                .set_capabilities(crate::meat::cluster_state::NodeCapabilities {
                    egress,
                    dns: self.supervisor.dns_capability(),
                })
                .await;
        }
    }

    /// Read the hooks and enforcement map as kernel truth for reporting.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn live_egress_report_state(
        &self,
    ) -> (
        crate::meat::cluster_state::NodeCapabilities,
        std::collections::HashSet<InstanceId>,
    ) {
        #[cfg(test)]
        self.egress_observation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return (
                crate::meat::cluster_state::NodeCapabilities {
                    dns: self.supervisor.dns_capability(),
                    ..Default::default()
                },
                Default::default(),
            );
        };
        let mut ebpf = handle.lock().await;
        let capabilities = crate::meat::cluster_state::NodeCapabilities {
            egress: crate::sesame::egress::EgressEnforcementCapability {
                connect_ipv4: ebpf.is_attached(),
                connect_ipv6: ebpf.connect6_attached(),
                udp_ipv4: ebpf.sendmsg4_attached(),
                udp_ipv6: ebpf.sendmsg6_attached(),
                pre_start: self.supervisor.grill().honours_cgroup_path(),
            },
            dns: self.supervisor.dns_capability(),
        };
        let enforced_cgroups =
            crate::sesame::egress::list_enforced_cgroups(&mut ebpf.bpf).unwrap_or_default();
        let enforced = self
            .egress_bindings
            .iter()
            .filter(|(_, binding)| {
                binding.phase == PolicyPhase::Owned && enforced_cgroups.contains(&binding.cgroup_id)
            })
            .map(|(instance_id, _)| instance_id.clone())
            .collect();
        (capabilities, enforced)
    }

    /// A portable build has no kernel enforcement to report.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn live_egress_report_state(
        &self,
    ) -> (
        crate::meat::cluster_state::NodeCapabilities,
        std::collections::HashSet<InstanceId>,
    ) {
        #[cfg(test)]
        self.egress_observation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (
            crate::meat::cluster_state::NodeCapabilities {
                dns: self.supervisor.dns_capability(),
                ..Default::default()
            },
            Default::default(),
        )
    }

    /// Program a freshly-started instance's kernel networking: mirror its
    /// backend into `backend_map` (L8) and reconcile namespace-firewall maps
    /// (NET5). Egress is deliberately absent here: it must already have been
    /// programmed before `start`, never repaired in post-start bookkeeping.
    pub(super) async fn finish_instance_networking(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        self.publish_backend_ebpf(&service_id).await?;
        self.sync_firewall_ebpf().await;
        // A new caller must meet the network faults already active against
        // the services it calls.
        self.reconcile_network_faults().await;
        Ok(())
    }

    /// Program an instance's egress *before* its process starts, closing
    /// the window during which a fresh workload could connect anywhere
    /// (the connect hook allows everything for a cgroup with no
    /// `egress_enforced` flag). Only possible when the runtime honours
    /// the OCI `cgroupsPath` (root-mode runc): the agent creates the
    /// cgroup directory itself, programs the maps against its inode, and
    /// only then lets the runtime start the workload into it.
    ///
    /// Returns an error — failing the deploy closed — when enforcement is
    /// required but cannot be guaranteed (connect6 missing, cgroup id
    /// unresolvable, map programming failed).
    ///
    /// `egress` is the allowlist whoever prepared the start resolved, off
    /// the loop (#419).
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn apply_network_pre_start(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        cgroup_path: &std::path::Path,
        retained: Result<Option<crate::grill::runc_intent::NetworkReference>, BunError>,
        egress: launch_evidence::EgressResolution,
    ) -> Result<(), BunError> {
        self.retain_network_reference(instance_id, spec, retained)
            .await?;
        use crate::sesame::egress::{self, PreStartEgress};

        let has_allowlist = spec
            .and_then(|spec| spec.egress.as_ref())
            .is_some_and(|e| !e.allow.is_empty());
        let capability = match self.onion_ebpf.as_ref() {
            Some(handle) => {
                let handle = handle.lock().await;
                egress::EgressEnforcementCapability {
                    connect_ipv4: handle.is_attached(),
                    connect_ipv6: handle.connect6_attached(),
                    udp_ipv4: handle.sendmsg4_attached(),
                    udp_ipv6: handle.sendmsg6_attached(),
                    pre_start: self.supervisor.grill().honours_cgroup_path(),
                }
            }
            None => Default::default(),
        };

        // Create the cgroup directory before the runtime does, so its
        // inode — the id `bpf_get_current_cgroup_id()` will report — is
        // known before the process exists. runc joins an existing
        // `cgroupsPath` directory untouched, keeping the inode stable.
        let cgroup_id = if capability.can_enforce_allowlist() {
            // LOOP-INLINE: one cgroupfs mkdir, microseconds
            let _ = tokio::fs::create_dir_all(cgroup_path).await;
            egress::cgroup_id_of_path(cgroup_path)
        } else {
            None
        };

        let require_source =
            self.onion_ebpf.is_some() && self.supervisor.grill().honours_cgroup_path();
        match egress::plan_pre_start_egress(has_allowlist, capability, cgroup_id) {
            PreStartEgress::NoPolicy if require_source => {
                let cgroup_id = cgroup_id.ok_or_else(|| BunError::DeployFailed {
                    app_name: app_name.into(),
                    reason: "source namespace cgroup could not be prepared".into(),
                })?;
                self.program_egress_pre_start(instance_id, app_name, spec, cgroup_id, egress)
                    .await
            }
            PreStartEgress::NoPolicy => Ok(()),
            PreStartEgress::Refuse { reason } => Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!("egress enforcement for {}: {reason}", instance_id.0),
            }),
            PreStartEgress::Program { cgroup_id } => {
                self.program_egress_pre_start(instance_id, app_name, spec, cgroup_id, egress)
                    .await
            }
        }
    }

    /// Record the network reference the runtime retained for `id` before it
    /// starts. Whoever created the instance asked the runtime for it, off
    /// the loop (#351, stage 3); the loop checks it belongs to `id`'s
    /// generation and journals it.
    pub(super) async fn retain_network_reference(
        &mut self,
        id: &InstanceId,
        spec: Option<&AppSpec>,
        retained: Result<Option<crate::grill::runc_intent::NetworkReference>, BunError>,
    ) -> Result<(), BunError> {
        let retained = retained?;
        if !launch_evidence::retains_network(spec) {
            // The spec stopped publishing an address after the retain.
            if let Some(reference) = retained {
                self.hand_back_network_reference(reference);
            }
            return Ok(());
        }
        if let Some(reference) = retained {
            if reference.instance_id != *id {
                return Err(BunError::RetirementState {
                    instance_id: id.clone(),
                    reason: "runtime returned another instance's network reference".into(),
                });
            }
            if self
                .network_references
                .get(id)
                .is_some_and(|original| original != &reference)
            {
                return Err(BunError::RetirementState {
                    instance_id: id.clone(),
                    reason: "original network reference still belongs to another generation".into(),
                });
            }
            if let Err(error) = self.persist_discovery_reference(&reference).await {
                // A refusal decided in memory never reached the journal, so no
                // publication can name this address yet. Hand it back now rather
                // than leave a hold nothing tracks. After an uncertain write the
                // journal may record it, so only retirement may release it.
                if !matches!(self.discovery_ownership, DiscoveryOwnership::Uncertain) {
                    self.hand_back_network_reference(reference);
                }
                return Err(error);
            }
            self.network_references.insert(id.clone(), reference);
        }
        Ok(())
    }

    /// Give back a hold no journal records, from a task: nothing waits on
    /// it, and a release names its generation, so it can't touch a later
    /// retain of the same instance.
    pub(super) fn hand_back_network_reference(
        &self,
        reference: crate::grill::runc_intent::NetworkReference,
    ) {
        let grill = self.supervisor.grill().clone();
        tokio::spawn(async move {
            if let Err(error) = grill.release_network_reference(&reference).await {
                eprintln!(
                    "bun: handing back {}'s untracked network reference failed: {error}",
                    reference.instance_id
                );
            }
        });
    }

    pub(super) async fn release_network_reference(
        &mut self,
        id: &InstanceId,
        remote: Option<&crate::onion::producer::ProducerReleaseConfirmation>,
    ) -> Result<(), BunError> {
        // The runtime answers these under the instance's lifecycle lock, and
        // on runc the health sweep's and status reader's state reads queue
        // for it too, so either call can take longer than a turn (#387). Each
        // runs in a task: one that hasn't answered within the turn fails the
        // retirement with `StillRunning`, and the retry collects the same
        // task instead of asking again. A release is idempotent and names its
        // generation, so one that lands late is harmless.
        let reference = match self.network_references.get(id).cloned() {
            Some(reference) => reference,
            None => {
                let read = off_loop_work::WorkKey::ReadNetworkReference(id.clone());
                let Some(held) = self.read_network_reference(read, id).await? else {
                    return Ok(());
                };
                match self.journal_reference(&held) {
                    // The hold was retained but its launch never recorded it, so
                    // no publication ever named the address: nothing to withdraw.
                    JournalReference::Unrecorded => {
                        return self.finish_network_release(id, held).await;
                    }
                    // Recorded by a write whose outcome was uncertain at the time.
                    JournalReference::Recorded => {
                        self.network_references.insert(id.clone(), held.clone());
                        held
                    }
                    JournalReference::Unknown => {
                        return Err(BunError::RetirementState {
                            instance_id: id.clone(),
                            reason: "retained network reference requires original discovery reconciliation"
                                .into(),
                        });
                    }
                }
            }
        };
        self.authorise_local_discovery_release(&reference, remote)
            .await?;
        self.require_discovery_release_permission(&reference)?;
        self.finish_network_release(id, reference.clone()).await?;
        self.forget_released_discovery_reference(&reference).await?;
        self.network_references.remove(id);
        Ok(())
    }

    /// Which network reference the runtime holds for `id`, read in a task
    /// ([`off_loop_work`]) as `key`'s work. `StillRunning` means ask again.
    pub(super) async fn read_network_reference(
        &mut self,
        key: off_loop_work::WorkKey,
        id: &InstanceId,
    ) -> Result<Option<crate::grill::runc_intent::NetworkReference>, BunError> {
        let grill = self.supervisor.grill().clone();
        let read_id = id.clone();
        let read = async move {
            grill
                .network_reference(&read_id)
                .await
                .map_err(|error| error.to_string())
        };
        let incarnation = self.incarnation_of(id);
        let turn_deadline = self.turn_deadline();
        // LOOP-INLINE: `finish` waits with `timeout_at(turn_deadline)`
        self.network_reference_reads
            .finish(key, incarnation, read, turn_deadline)
            .await?
            .map_err(|reason| BunError::RetirementState {
                instance_id: id.clone(),
                reason,
            })
    }

    /// Hand `reference` back to the runtime from a task ([`off_loop_work`]).
    /// `StillRunning` means ask again.
    pub(super) async fn finish_network_release(
        &mut self,
        id: &InstanceId,
        reference: crate::grill::runc_intent::NetworkReference,
    ) -> Result<(), BunError> {
        let grill = self.supervisor.grill().clone();
        let release = async move {
            grill
                .release_network_reference(&reference)
                .await
                .map_err(|error| error.to_string())
        };
        let key = off_loop_work::WorkKey::ReleaseNetworkReference(id.clone());
        let incarnation = self.incarnation_of(id);
        self.finish_off_loop_work(key, incarnation, release)
            .await?
            .map_err(|reason| BunError::RetirementState {
                instance_id: id.clone(),
                reason,
            })
    }

    /// A build without the eBPF data path cannot enforce an allowlist.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn apply_network_pre_start(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        _cgroup_path: &std::path::Path,
        retained: Result<Option<crate::grill::runc_intent::NetworkReference>, BunError>,
        _egress: launch_evidence::EgressResolution,
    ) -> Result<(), BunError> {
        self.retain_network_reference(instance_id, spec, retained)
            .await?;
        if spec
            .and_then(|spec| spec.egress.as_ref())
            .is_some_and(|e| !e.allow.is_empty())
        {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "egress allowlist requires an eBPF-enabled binary".to_string(),
            });
        }
        Ok(())
    }

    /// The programming half of the pre-start path. Deploy-failure
    /// semantics: a transient DNS failure denies all egress and lets the
    /// instance start (the re-resolve loop fills the allowlist in later),
    /// but a programming or representation error fails the deploy — a
    /// workload must never start ahead of a policy we could not install.
    ///
    /// The DNS happened before this turn, wherever the start was prepared
    /// (#419). A resolution of a different allowlist than the one being
    /// programmed is refused rather than programmed deny-all: it means the
    /// spec changed while the start was being prepared.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn program_egress_pre_start(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        cgroup_id: u64,
        egress: launch_evidence::EgressResolution,
    ) -> Result<(), BunError> {
        let allow = spec
            .and_then(|spec| spec.egress.as_ref())
            .map(|policy| policy.allow.as_slice())
            .unwrap_or_default();
        if egress.allow.as_slice() != allow {
            return Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!(
                    "egress allowlist for {} changed while its start was prepared",
                    instance_id.0
                ),
            });
        }
        self.clear_egress(instance_id).await?;
        let resolved = egress.destinations;
        let union: Vec<_> = self
            .egress_bindings
            .values()
            .filter(|binding| binding.phase == PolicyPhase::Owned && binding.cgroup_id == cgroup_id)
            .flat_map(|binding| binding.resolved.iter().copied())
            .chain(resolved.iter().copied())
            .collect();
        crate::sesame::egress::merge_cidr_ports(&union).map_err(|error| {
            BunError::DeployFailed {
                app_name: app_name.into(),
                reason: error.to_string(),
            }
        })?;
        let original_spec = self
            .supervisor
            .get_instance(instance_id)
            .and_then(|instance| instance.oci_spec.clone())
            .ok_or_else(|| {
                BunError::AdoptionState(format!(
                    "egress owner {instance_id} has no original runtime input"
                ))
            })?;
        let source_identity = self
            .supervisor
            .get_instance(instance_id)
            .map(|instance| (instance.namespace.clone(), instance.app_name.clone()))
            .ok_or_else(|| BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            })?;
        let source_namespace = crate::onion::vip::name_to_id(&source_identity.0);
        // LOOP-INLINE: reads /proc boot_id, microseconds
        let boot_id = tokio::task::spawn_blocking(crate::bun::egress_owners::boot_id)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.egress_bindings.insert(
            instance_id.clone(),
            EgressBinding {
                phase: PolicyPhase::Owned,
                cgroup_id,
                source_namespace: Some(source_namespace),
                allow: allow.to_vec(),
                resolved,
                original_spec,
                runtime: self.supervisor.grill().runtime_kind(),
                boot_id,
            },
        );
        self.persist_egress_owners(self.egress_bindings.clone())
            .await?;
        let handle = self
            .onion_ebpf
            .clone()
            .ok_or_else(|| BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "kernel source policy is unavailable".into(),
            })?;
        self.cgroup_ns_bpf_keys.insert(cgroup_id);
        crate::sesame::firewall::write_cgroup_namespace_entry(
            &mut handle.lock().await.bpf,
            cgroup_id,
            source_namespace,
        )
        .map_err(|error| BunError::DeployFailed {
            app_name: app_name.into(),
            reason: error.to_string(),
        })?;
        let services = self
            .service_map
            .resolve_all()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let sources = std::collections::HashMap::from([(source_identity, vec![cgroup_id])]);
        let entries = crate::sesame::firewall::rules_to_bpf_entries(
            &crate::sesame::firewall::resolve_firewall_rules(&services, &sources),
        );
        for (key, value) in entries {
            // Remember partial publication before attempting the write. The
            // durable source owner retains every grant until confirmed cleanup.
            self.firewall_bpf_keys.insert(key);
            crate::sesame::firewall::write_firewall_entry(&mut handle.lock().await.bpf, key, value)
                .map_err(|error| BunError::DeployFailed {
                    app_name: app_name.into(),
                    reason: error.to_string(),
                })?;
        }
        if allow.is_empty() {
            return Ok(());
        }
        // Keep the enable flag while rebuilding. During a rollout the old and
        // new instances may share a cgroup, so removing it would open a gap.
        self.reprogram_cgroup_egress(cgroup_id, None)
            .await
            .map_err(|error| BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!("egress map programming failed for {instance_id}: {error}"),
            })
    }

    /// Lift egress enforcement for a stopped instance's cgroup (L16).
    ///
    /// Deletes the allow entries as well as the enable flag: cgroup ids are
    /// recycled by the kernel, and a stale allowlist left behind could open
    /// destinations for whatever workload next lands on that cgroup id (NET6).
    /// Goes through `reprogram_cgroup_egress` because instances can share a
    /// cgroup path — deleting one instance's entries directly would wipe a
    /// co-tenant's policy.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn clear_egress(&mut self, instance_id: &InstanceId) -> Result<(), BunError> {
        if self.egress_store_uncertain {
            return Err(BunError::AdoptionState(
                "egress ownership persistence is uncertain; restart to recover the checkpoint"
                    .into(),
            ));
        }
        let Some(binding) = self.egress_bindings.get(instance_id).cloned() else {
            return Ok(());
        };
        if binding.phase == PolicyPhase::Retired {
            return Ok(());
        }
        // LOOP-INLINE: reads /proc boot_id, microseconds
        let boot_id = tokio::task::spawn_blocking(crate::bun::egress_owners::boot_id)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        if binding.boot_id == boot_id {
            self.reprogram_cgroup_egress(binding.cgroup_id, Some(instance_id))
                .await
                .map_err(|error| BunError::RetirementState {
                    instance_id: instance_id.clone(),
                    reason: error.to_string(),
                })?;
        }
        if binding.boot_id == boot_id
            && binding.source_namespace.is_some()
            && !self.egress_bindings.iter().any(|(id, owner)| {
                id != instance_id
                    && owner.phase == PolicyPhase::Owned
                    && owner.cgroup_id == binding.cgroup_id
            })
        {
            let handle = self
                .onion_ebpf
                .clone()
                .ok_or_else(|| BunError::RetirementState {
                    instance_id: instance_id.clone(),
                    reason: "kernel source policy is unavailable".into(),
                })?;
            crate::sesame::firewall::delete_cgroup_firewall_state(
                &mut handle.lock().await.bpf,
                binding.cgroup_id,
            )
            .map_err(|error| BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason: error.to_string(),
            })?;
            self.cgroup_ns_bpf_keys.remove(&binding.cgroup_id);
            self.firewall_bpf_keys
                .retain(|key| key.src_cgroup_id != binding.cgroup_id);
        }
        // The caller has retired the previous workload. A different boot proves the
        // old kernel maps are gone; never delete a recycled current-boot key.
        let mut confirmed = binding;
        confirmed.phase = PolicyPhase::Retired;
        confirmed.resolved.clear();
        let mut owners = self.egress_bindings.clone();
        owners.insert(instance_id.clone(), confirmed.clone());
        self.persist_egress_owners(owners).await?;
        self.egress_bindings.insert(instance_id.clone(), confirmed);
        Ok(())
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn clear_egress(&mut self, _instance_id: &InstanceId) -> Result<(), BunError> {
        Ok(())
    }

    /// Rebuild one cgroup's policy, excluding a retiring instance only from the
    /// proposed kernel state. Its binding remains owned until every write succeeds.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn reprogram_cgroup_egress(
        &mut self,
        cgroup_id: u64,
        excluding: Option<&InstanceId>,
    ) -> Result<(), crate::sesame::egress::EgressMapError> {
        use crate::sesame::egress;
        if self.egress_store_uncertain {
            return Err(egress::EgressMapError::Unavailable);
        }
        let handle = self
            .onion_ebpf
            .clone()
            .ok_or(egress::EgressMapError::Unavailable)?;
        let survivors: Vec<_> = self
            .egress_bindings
            .iter()
            .filter(|(id, binding)| {
                binding.phase == PolicyPhase::Owned
                    && !binding.allow.is_empty()
                    && Some(*id) != excluding
                    && binding.cgroup_id == cgroup_id
            })
            .map(|(_, binding)| binding)
            .collect();
        let mut ebpf = handle.lock().await;
        if survivors.is_empty() {
            return egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id);
        }
        let union: Vec<_> = survivors
            .iter()
            .flat_map(|binding| binding.resolved.iter().copied())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let merged = egress::merge_cidr_ports(&union)?;
        egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id)?;
        egress::delete_cgroup_egress_entries(&mut ebpf.bpf, cgroup_id)?;
        egress::write_egress_destinations(&mut ebpf.bpf, cgroup_id, &union, &merged)
    }

    /// Stop every workload affected by an unconfirmed policy rewrite.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn handle_egress_rewrite_failure(
        &mut self,
        cgroup_id: u64,
        error: crate::sesame::egress::EgressMapError,
    ) {
        eprintln!("sesame: egress rewrite failed for cgroup {cgroup_id}: {error}");
        let affected = self
            .egress_bindings
            .iter()
            .filter(|(_, binding)| {
                binding.phase == PolicyPhase::Owned && binding.cgroup_id == cgroup_id
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.stop_instances_after_egress_loss(affected).await;
    }

    /// Fence executing workloads whose original namespace identity is unavailable.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn enforce_live_source_or_stop(&mut self) {
        if !self.supervisor.grill().honours_cgroup_path() {
            return;
        }
        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| {
                !matches!(
                    instance.state,
                    ContainerState::Pending
                        | ContainerState::Preparing
                        | ContainerState::Stopped
                        | ContainerState::Failed
                )
            })
            .map(|instance| instance.id.clone())
            .collect();
        let mut failed = std::collections::HashSet::new();
        let mut handle = handle.lock().await;
        let hooks = handle.is_attached()
            && handle.connect6_attached()
            && handle.sendmsg4_attached()
            && handle.sendmsg6_attached();
        for id in instances {
            let original = self
                .egress_bindings
                .get(&id)
                .filter(|owner| owner.phase == PolicyPhase::Owned)
                .and_then(|owner| {
                    owner
                        .source_namespace
                        .map(|namespace| (owner.cgroup_id, namespace))
                });
            let valid = if let Some((cgroup, namespace)) = original {
                hooks
                    && crate::sesame::firewall::read_firewall_state(&mut handle.bpf, cgroup, 0)
                        .is_ok_and(|state| state.source_namespace_id == Some(namespace))
            } else {
                false
            };
            if !valid {
                failed.insert(id);
            }
        }
        drop(handle);
        self.stop_instances_after_egress_loss(failed).await;
    }

    /// Verify the security boundary on every event-loop tick. Map drift gets
    /// one immediate repair attempt. If any required hook is gone, the map can't be
    /// read, or a repaired enforcement flag is still absent, stop every
    /// affected workload. Keeping it running would turn its allowlist into a
    /// label rather than a control.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn enforce_live_egress_or_stop(
        &mut self,
    ) -> crate::sesame::egress::EgressEnforcementCapability {
        #[cfg(test)]
        self.egress_observation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        use crate::sesame::egress;

        self.enforce_live_source_or_stop().await;

        // Pending/preparing work cannot execute yet. The deployment driver
        // installs policy before entering Initialising or Starting; monitoring
        // must not race that installation while an image is still being pulled.
        let unbound: std::collections::HashSet<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| {
                !matches!(
                    instance.state,
                    ContainerState::Pending
                        | ContainerState::Preparing
                        | ContainerState::Stopped
                        | ContainerState::Failed
                )
            })
            .filter(|instance| {
                self.egress_bindings
                    .get(&instance.id)
                    .is_none_or(|binding| binding.phase != PolicyPhase::Owned)
            })
            .filter(|instance| {
                self.deployed_specs
                    .get(&(instance.app_name.clone(), instance.namespace.clone()))
                    .and_then(|spec| spec.egress.as_ref())
                    .is_some_and(|policy| {
                        !policy.allow.is_empty() || !policy.allow_franchise.is_empty()
                    })
            })
            .map(|instance| instance.id.clone())
            .collect();
        let Some(handle) = self.onion_ebpf.clone() else {
            self.supervisor.set_egress_capability(Default::default());
            let mut affected: std::collections::HashSet<InstanceId> = self
                .egress_bindings
                .iter()
                .filter(|(_, binding)| binding.phase == PolicyPhase::Owned)
                .map(|(id, _)| id.clone())
                .collect();
            affected.extend(unbound);
            self.stop_instances_after_egress_loss(affected).await;
            return Default::default();
        };
        let expected: std::collections::HashSet<u64> = self
            .egress_bindings
            .values()
            .filter(|binding| binding.phase == PolicyPhase::Owned && !binding.allow.is_empty())
            .map(|b| b.cgroup_id)
            .collect();
        let (capability, kernel_enforced) = {
            let mut ebpf = handle.lock().await;
            let capability = egress::EgressEnforcementCapability {
                connect_ipv4: ebpf.is_attached(),
                connect_ipv6: ebpf.connect6_attached(),
                udp_ipv4: ebpf.sendmsg4_attached(),
                udp_ipv6: ebpf.sendmsg6_attached(),
                pre_start: self.supervisor.grill().honours_cgroup_path(),
            };
            let enforced = egress::list_enforced_cgroups(&mut ebpf.bpf).unwrap_or_default();
            (capability, enforced)
        };
        self.supervisor.set_egress_capability(capability);
        if expected.is_empty() && unbound.is_empty() {
            if capability.can_enforce_allowlist() {
                self.egress_affected_workloads.clear();
            }
            return capability;
        }

        let plan = egress::plan_live_egress_health(capability, &expected, &kernel_enforced);
        for cgroup_id in &plan.repair {
            eprintln!("sesame: live check restoring egress enforcement for cgroup {cgroup_id}");
            if let Err(error) = self.reprogram_cgroup_egress(*cgroup_id, None).await {
                self.handle_egress_rewrite_failure(*cgroup_id, error).await;
            }
        }

        let mut fence: std::collections::HashSet<u64> = plan.fence.into_iter().collect();
        if capability.can_enforce_allowlist() && !plan.repair.is_empty() {
            let verified = {
                let mut ebpf = handle.lock().await;
                egress::list_enforced_cgroups(&mut ebpf.bpf).unwrap_or_default()
            };
            fence.extend(expected.difference(&verified).copied());
        }
        if fence.is_empty() && unbound.is_empty() {
            self.egress_affected_workloads.clear();
            return capability;
        }

        let mut affected_ids: std::collections::HashSet<InstanceId> = self
            .egress_bindings
            .iter()
            .filter(|(_, binding)| {
                binding.phase == PolicyPhase::Owned && fence.contains(&binding.cgroup_id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        affected_ids.extend(unbound);
        self.stop_instances_after_egress_loss(affected_ids).await;
        capability
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn stop_instances_after_egress_loss(
        &mut self,
        affected_ids: std::collections::HashSet<InstanceId>,
    ) {
        if affected_ids.is_empty() {
            return;
        }
        let affected_apps: std::collections::HashSet<(String, String)> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| affected_ids.contains(&instance.id))
            .map(|instance| (instance.app_name.clone(), instance.namespace.clone()))
            .collect();
        self.egress_affected_workloads
            .extend(affected_apps.iter().cloned());
        for (app_name, namespace) in affected_apps {
            eprintln!("sesame: stopping {namespace}/{app_name}: live kernel policy was lost");
            // The stop waits out its grace off the loop; if it fails, its
            // completion fences execution (`fence_after_failed_stop`).
            if let Err(error) = self.stop_app_unattended(&app_name, &namespace).await {
                eprintln!(
                    "sesame: failed to stop {namespace}/{app_name} after egress loss: {error}"
                );
                // No stop is pending to ask again, so the next egress
                // check does, and collects whatever is still running.
                if let Err(error) = self.fence_after_failed_stop(&app_name, &namespace).await {
                    eprintln!("sesame: execution fence for {namespace}/{app_name}: {error}");
                }
            }
        }
    }

    /// Force-kill an app whose graceful stop failed, keeping every
    /// allocation it still owns.
    ///
    /// `Err(StillRunning)` means part of the fence is still running off the
    /// loop ([`off_loop_work`]): ask again, and the next attempt picks up the
    /// same work. Any other failure is reported here; the next egress check
    /// fences again.
    pub(super) async fn fence_after_failed_stop(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        #[cfg(any(test, all(feature = "ebpf", target_os = "linux")))]
        match self.fence_app_execution(app_name, namespace).await {
            Err(error @ BunError::StillRunning { .. }) => return Err(error),
            Err(error) => eprintln!(
                "sesame: execution fencing remains unconfirmed for {namespace}/{app_name}: {error}"
            ),
            Ok(()) => {}
        }
        // Only the egress fence asks for this, and it exists only with eBPF.
        #[cfg(not(any(test, all(feature = "ebpf", target_os = "linux"))))]
        let _ = (app_name, namespace);
        Ok(())
    }

    /// Stop unsafe execution while preserving refused discovery and policy cleanup.
    #[cfg(any(test, all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn fence_app_execution(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| {
                instance.app_name == app_name
                    && instance.namespace == namespace
                    && instance.state != ContainerState::Stopped
            })
            .map(|instance| {
                (
                    instance.id.clone(),
                    instance.container_ip.is_some() && instance.host_port.is_some(),
                )
            })
            .collect();
        // LOOP-INLINE: in-memory lock, no I/O
        self.supervisor.stop_app(app_name, namespace).await?;
        let mut first_error = None;
        let mut still_running = None;
        for (id, publishes_address) in instances {
            let kill = off_loop_work::WorkKey::FenceExecution(id.clone());
            // A kill already under way passed this check when it started.
            let checked = self.off_loop_work.started(&kill, self.incarnation_of(&id));
            let result = async {
                if publishes_address && !checked {
                    // On runc the read waits for the instance's lifecycle
                    // lock, which can outlast a turn (#393). It runs in a
                    // task; `StillRunning` leaves this instance unkilled
                    // until a later attempt collects the answer.
                    let read = off_loop_work::WorkKey::FenceNetworkReference(id.clone());
                    let reference = self.read_network_reference(read, &id).await?;
                    if reference.is_none()
                        || self
                            .network_references
                            .get(&id)
                            .is_some_and(|original| reference.as_ref() != Some(original))
                    {
                        return Err(BunError::RetirementState {
                            instance_id: id.clone(),
                            reason: "execution fencing requires the original retained address"
                                .into(),
                        });
                    }
                }
                self.retire_initialisers(&id).await?;
                // The kill runs off the loop too; until it's confirmed the
                // fence answers `StillRunning`, and asking again collects it.
                self.kill_off_the_loop(kill.clone(), &id)
                    .await?
                    .map_err(|reason| BunError::RetirementState {
                        instance_id: id.clone(),
                        reason: format!("execution fence kill unconfirmed: {reason}"),
                    })
            }
            .await;
            match result {
                Ok(()) => {}
                Err(error @ BunError::StillRunning { .. }) => {
                    still_running.get_or_insert(error);
                    continue;
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            }
            if let Some(instance) = self.supervisor.get_instance_mut(&id)
                && instance.state.can_transition_to(ContainerState::Stopped)
            {
                instance.state = ContainerState::Stopped;
            }
        }
        // Address holds, service keys, grants and adoption records remain owned.
        // An execution stop is not an acknowledgement of their retirement.
        // Work still running comes first: the caller asks again, and that
        // attempt reports any failure that remains.
        still_running.or(first_error).map_or(Ok(()), Err)
    }

    /// Portable builds cannot have live egress bindings.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn enforce_live_egress_or_stop(
        &mut self,
    ) -> crate::sesame::egress::EgressEnforcementCapability {
        #[cfg(test)]
        self.egress_observation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Default::default()
    }

    /// Periodically re-resolve DNS-based egress allowlists and reprogram the
    /// eBPF egress maps when an app's destination IPs change (L16). Rate-
    /// limited to roughly once every five minutes; a no-op while nothing
    /// enforces egress.
    ///
    /// The lookups run in a task ([`egress_resolution`]); the loop applies
    /// what they found when the task reports back, and starts no second
    /// re-resolution meanwhile.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) fn reresolve_egress(&mut self) {
        if self.egress_store_uncertain || self.egress_resolving.is_some() {
            return;
        }
        // ~5 minutes at the 1s event-loop tick.
        const RERESOLVE_EVERY_TICKS: u32 = 300;
        self.egress_reresolve_ticks += 1;
        if self.egress_reresolve_ticks < RERESOLVE_EVERY_TICKS || self.egress_bindings.is_empty() {
            return;
        }
        self.egress_reresolve_ticks = 0;

        if self.onion_ebpf.is_none() {
            return;
        }
        let requests = egress_resolution::requests(self.egress_bindings.iter());
        let task = self.follow_ups.spawn(async move {
            follow_ups::FollowUp::EgressResolved(egress_resolution::resolve(requests).await)
        });
        self.egress_resolving = Some(task.id());
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) fn reresolve_egress(&mut self) {}

    /// Record re-resolved allowlists, and reprogram the cgroups whose
    /// destinations changed, for the bindings they still describe.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn apply_egress_resolutions(
        &mut self,
        resolutions: Vec<egress_resolution::Resolution>,
    ) {
        if self.egress_store_uncertain {
            return;
        }
        for resolution in resolutions {
            let request = resolution.request;
            let new_resolved = match resolution.resolved {
                Ok(resolved) => resolved,
                Err(error) => {
                    eprintln!(
                        "sesame: egress re-resolve failed for {}: {error}",
                        request.instance_id.0
                    );
                    continue;
                }
            };
            let Some(binding) = self.egress_bindings.get_mut(&request.instance_id) else {
                continue;
            };
            if !egress_resolution::still_current(Some(&*binding), &request) {
                continue;
            }
            let (to_add, to_remove) =
                crate::sesame::egress::egress_diff(&binding.resolved, &new_resolved);
            if to_add.is_empty() && to_remove.is_empty() {
                continue;
            }

            // Record the new set, then rebuild the cgroup's kernel state
            // from all bindings: CIDR values are merged per cgroup, so a
            // delta write can't be applied entry by entry.
            binding.resolved = new_resolved;
            if let Err(error) = self.reprogram_cgroup_egress(request.cgroup_id, None).await {
                self.handle_egress_rewrite_failure(request.cgroup_id, error)
                    .await;
            }
        }
    }

    /// Reconcile kernel truth against live instances (the sweep half of the
    /// network-policy theme): scrub egress state whose cgroup no longer maps
    /// to a live instance, rewrite every live binding (idempotent repairs),
    /// while retaining unknown namespace keys. The one-second live check
    /// fences adopted policy-bearing workloads with no trustworthy binding;
    /// the sweep never installs their policy after they have already run.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn sweep_kernel_networking(&mut self) {
        use crate::sesame::egress;

        if self.egress_store_uncertain
            || self.ebpf_sweep_interval_secs == 0
            || self.onion_ebpf.is_none()
        {
            return;
        }
        self.ebpf_sweep_ticks += 1;
        if self.ebpf_sweep_ticks < self.ebpf_sweep_interval_secs {
            return;
        }
        self.ebpf_sweep_ticks = 0;
        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };

        // 1. Live instances with an allowlist but no binding are an invariant
        //    violation, not a repair opportunity after process start. The
        //    one-second live check stops them; repeat the check here as
        //    defence in depth instead of installing a late policy.
        let missing: std::collections::HashSet<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| {
                !matches!(
                    i.state,
                    crate::grill::state::ContainerState::Pending
                        | crate::grill::state::ContainerState::Preparing
                        | crate::grill::state::ContainerState::Stopped
                        | crate::grill::state::ContainerState::Failed
                )
            })
            .filter(|i| {
                self.egress_bindings
                    .get(&i.id)
                    .is_none_or(|binding| binding.phase != PolicyPhase::Owned)
            })
            .filter_map(|i| {
                self.deployed_specs
                    .get(&(i.app_name.clone(), i.namespace.clone()))
                    .filter(|s| s.egress.as_ref().is_some_and(|e| !e.allow.is_empty()))
                    .map(|_| i.id.clone())
            })
            .collect();
        for id in &missing {
            eprintln!(
                "sesame: sweep found unbound egress policy for {}; fencing",
                id.0
            );
        }
        self.stop_instances_after_egress_loss(missing).await;

        // 2. Kernel truth vs expected cgroups.
        let expected: std::collections::HashSet<u64> = self
            .egress_bindings
            .values()
            .filter(|binding| binding.phase == PolicyPhase::Owned && !binding.allow.is_empty())
            .map(|b| b.cgroup_id)
            .collect();
        let (kernel_enforced, kernel_entries) = {
            let mut ebpf = handle.lock().await;
            let enforced = match egress::list_enforced_cgroups(&mut ebpf.bpf) {
                Ok(set) => set,
                Err(e) => {
                    eprintln!("sesame: sweep could not list enforced cgroups: {e}");
                    return;
                }
            };
            let entries = match egress::list_egress_entry_cgroups(&mut ebpf.bpf) {
                Ok(set) => set,
                Err(e) => {
                    eprintln!("sesame: sweep could not list egress entries: {e}");
                    return;
                }
            };
            (enforced, entries)
        };
        let plan = egress::plan_egress_sweep(&expected, &kernel_enforced, &kernel_entries);
        if !plan.stale.is_empty() {
            let mut ebpf = handle.lock().await;
            for cgroup_id in &plan.stale {
                eprintln!(
                    "sesame: sweep deleting kernel egress state for departed cgroup {cgroup_id}"
                );
                if let Err(e) = egress::delete_cgroup_egress_state(&mut ebpf.bpf, *cgroup_id) {
                    eprintln!("sesame: sweep scrub failed for cgroup {cgroup_id}: {e}");
                }
            }
        }
        for cgroup_id in &plan.repair {
            eprintln!("sesame: sweep restoring egress enforcement for cgroup {cgroup_id}");
        }
        // Rewrite every live cgroup's entries: idempotent inserts, and the
        // only way lost entries (as opposed to a lost flag) come back.
        let live_cgroups: std::collections::HashSet<u64> = expected;
        for cgroup_id in live_cgroups {
            if let Err(error) = self.reprogram_cgroup_egress(cgroup_id, None).await {
                self.handle_egress_rewrite_failure(cgroup_id, error).await;
            }
        }

        // Unknown kernel keys are not proof of abandoned ownership. Retained
        // source owners authorise individual retirement; reconciliation retries
        // only the keys it already owns.
        self.sync_firewall_ebpf().await;
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn sweep_kernel_networking(&mut self) {}
}
