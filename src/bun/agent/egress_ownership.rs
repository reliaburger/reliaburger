//! Restore durable policy authority before adopting surviving workloads.

use super::{BunAgent, BunError, Grill};
use crate::bun::egress_owners::PolicyPhase;
use crate::grill::{RuntimeLaunch, records::InstanceRecord};

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Validate the complete policy inventory before runtime recovery mutates owners.
    pub(super) async fn restore_egress_owners(
        &mut self,
        records: &[InstanceRecord],
        launches: Option<&[RuntimeLaunch]>,
    ) -> Result<(), BunError> {
        let Some(directory) = self.records_dir.clone() else {
            return Ok(());
        };
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        let delegated_sources = if let Some(data) = directory.parent() {
            crate::bun::task_namespace::TaskNamespacePolicy::recorded_sources(data)
                .await
                .map_err(|error| BunError::AdoptionState(error.to_string()))?
        } else {
            std::collections::BTreeMap::new()
        };
        let owners =
            tokio::task::spawn_blocking(move || crate::bun::egress_owners::load(&directory))
                .await
                .map_err(|error| BunError::AdoptionState(error.to_string()))?
                .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        let require_source =
            self.onion_ebpf.is_some() && self.supervisor.grill().honours_cgroup_path();
        #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
        let require_source = false;
        for record in records {
            if ((require_source
                && self
                    .supervisor
                    .grill()
                    .honours_cgroup_path_for(&record.oci_spec))
                || record
                    .app_spec
                    .as_ref()
                    .and_then(|spec| spec.egress.as_ref())
                    .is_some_and(|policy| !policy.allow.is_empty()))
                && !owners.contains_key(&crate::grill::InstanceId(record.instance_id.clone()))
            {
                return Err(BunError::AdoptionState(format!(
                    "protected instance {} has no policy ownership or retirement evidence",
                    record.instance_id
                )));
            }
        }
        for (id, owner) in &owners {
            if require_source && owner.source_namespace.is_none() {
                return Err(BunError::AdoptionState(format!(
                    "source owner {id} has no original namespace identity"
                )));
            }
            if owner.runtime
                != self
                    .supervisor
                    .grill()
                    .runtime_kind_for(&owner.original_spec)
            {
                return Err(BunError::AdoptionState(format!(
                    "egress owner {id} belongs to another runtime"
                )));
            }
            let record = records.iter().find(|record| record.instance_id == id.0);
            if let Some(record) = record {
                if record.oci_spec != owner.original_spec
                    || record
                        .app_spec
                        .as_ref()
                        .and_then(|spec| spec.egress.as_ref())
                        .map(|policy| policy.allow.as_slice())
                        .unwrap_or_default()
                        != owner.allow.as_slice()
                    || owner.source_namespace.is_some_and(|namespace| {
                        namespace != crate::onion::vip::name_to_id(&record.namespace)
                    })
                {
                    return Err(BunError::AdoptionState(format!(
                        "egress owner {id} conflicts with adoption input"
                    )));
                }
            } else if owner.phase == PolicyPhase::Owned && launches.is_none() {
                return Err(BunError::AdoptionState(format!(
                    "egress owner {id} has no complete runtime inventory"
                )));
            }
            if let Some(launches) = launches
                && owner.phase == PolicyPhase::Owned
                && !launches.iter().any(|launch| {
                    launch.instance_id == *id && launch.launched(&owner.original_spec)
                })
            {
                return Err(BunError::AdoptionState(format!(
                    "egress owner {id} conflicts with original runtime intent"
                )));
            }
        }
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        {
            let mut owners = owners;
            let resolver = self.egress_resolver();
            for owner in owners.values_mut() {
                if owner.phase == PolicyPhase::Owned {
                    owner.resolved = resolver.resolve_allowlist(&owner.allow).await;
                }
            }
            if let Some(handle) = self.onion_ebpf.clone() {
                let boot = tokio::task::spawn_blocking(crate::bun::egress_owners::boot_id)
                    .await
                    .map_err(|error| BunError::AdoptionState(error.to_string()))?
                    .map_err(|error| BunError::AdoptionState(error.to_string()))?;
                let sources: std::collections::HashSet<_> = owners
                    .values()
                    .filter(|owner| {
                        owner.phase == PolicyPhase::Owned
                            && owner.boot_id == boot
                            && owner.source_namespace.is_some()
                    })
                    .map(|owner| owner.cgroup_id)
                    .collect();
                let mut handle = handle.lock().await;
                let namespaces =
                    crate::sesame::firewall::list_cgroup_namespace_keys(&mut handle.bpf)
                        .map_err(|error| BunError::AdoptionState(error.to_string()))?;
                let rules = crate::sesame::firewall::list_firewall_keys(&mut handle.bpf)
                    .map_err(|error| BunError::AdoptionState(error.to_string()))?;
                for (cgroup, namespace) in &delegated_sources {
                    let observed =
                        crate::sesame::firewall::read_firewall_state(&mut handle.bpf, *cgroup, 0)
                            .map_err(|error| BunError::AdoptionState(error.to_string()))?
                            .source_namespace_id;
                    if observed.is_some_and(|observed| observed != *namespace) {
                        return Err(BunError::AdoptionState(
                            "delegated namespace kernel source conflicts with original ownership"
                                .into(),
                        ));
                    }
                }
                let known_sources: std::collections::HashSet<_> = sources
                    .iter()
                    .copied()
                    .chain(delegated_sources.keys().copied())
                    .collect();
                if !namespaces.is_subset(&known_sources)
                    || rules
                        .iter()
                        .any(|key| !sources.contains(&key.src_cgroup_id))
                {
                    return Err(BunError::AdoptionState(
                        "kernel source entries have no original ownership".into(),
                    ));
                }
                self.cgroup_ns_bpf_keys = sources;
                self.firewall_bpf_keys = rules;
            }
            self.egress_bindings = owners;
            self.egress_store_uncertain = false;
            let mut retained = self.egress_bindings.clone();
            retained.retain(|id, owner| {
                owner.phase == PolicyPhase::Owned
                    || records.iter().any(|record| record.instance_id == id.0)
            });
            if retained.len() != self.egress_bindings.len() {
                self.persist_egress_owners(retained.clone()).await?;
                self.egress_bindings = retained;
            }
        }
        #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
        if !owners.is_empty() {
            return Err(BunError::AdoptionState(
                "durable egress owners require Linux eBPF support".into(),
            ));
        }
        Ok(())
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    /// Publish the whole original ownership inventory, fencing later
    /// mutations after uncertain writes. Only adoption uses it, before the
    /// loop runs; the loop checkpoints one owner at a time with
    /// [`Self::checkpoint_egress_owner`], which leaves a slow write in flight.
    pub(super) async fn persist_egress_owners(
        &mut self,
        owners: std::collections::HashMap<
            crate::grill::InstanceId,
            crate::bun::egress_owners::EgressBinding,
        >,
    ) -> Result<(), BunError> {
        if self.egress_store_uncertain {
            return Err(BunError::AdoptionState(
                "egress ownership persistence is uncertain; restart to recover the checkpoint"
                    .into(),
            ));
        }
        let Some(directory) = self.records_dir.clone() else {
            return Ok(());
        };
        // Set this before awaiting the worker. An error after rename/fsync may
        // have changed durable state; no later mutation may overwrite it blindly.
        self.egress_store_uncertain = true;
        let started = std::time::Instant::now();
        #[cfg(test)]
        self.loop_stalls.hold(super::LoopStall::Persist).await;
        // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
        let persisted = tokio::task::spawn_blocking(move || {
            crate::bun::egress_owners::persist(&directory, owners)
        })
        .await;
        // Timed apart from the discovery journal's write, so a slow start's
        // line says which store held it (#1266).
        self.loop_meter.add_part(
            crate::bun::loop_meter::TurnPart::EgressWrite,
            started.elapsed(),
        );
        persisted
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.egress_store_uncertain = false;
        Ok(())
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    /// Forget positive policy-retirement evidence after adoption metadata is
    /// gone. The checkpoint write holds the turn only until its deadline
    /// (#1263): `StillRunning` means ask again, and the ask after the write
    /// has landed finds nothing left to forget.
    pub(super) async fn forget_retired_egress_owner(
        &mut self,
        id: &crate::grill::InstanceId,
    ) -> Result<(), BunError> {
        let still_running = || format!("checkpointing {id}'s forgotten egress owner");
        // Read the bindings as the last checkpoint write left them.
        let settled = self.settle_egress_write_outside_turn().await?;
        super::discovery_ownership::still_running_unless_landed(settled, still_running)?;
        let Some(owner) = self.egress_bindings.get(id) else {
            return Ok(());
        };
        if owner.phase != PolicyPhase::Retired {
            return Err(BunError::AdoptionState(format!(
                "egress owner {id} has not confirmed kernel retirement"
            )));
        }
        let deadline = self.turn_deadline();
        let landing = self
            .checkpoint_egress_owner(id, None, Some(deadline))
            .await?;
        super::discovery_ownership::still_running_unless_landed(landing, still_running)
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    /// Forget positive policy-retirement evidence after adoption metadata is gone.
    pub(super) async fn forget_retired_egress_owner(
        &mut self,
        _id: &crate::grill::InstanceId,
    ) -> Result<(), BunError> {
        Ok(())
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    /// Require retained ownership and enforcement before publishing a live adopter.
    pub(super) async fn restore_live_egress(
        &mut self,
        id: &crate::grill::InstanceId,
        record: &InstanceRecord,
    ) -> Result<(), BunError> {
        let protected = record
            .app_spec
            .as_ref()
            .and_then(|spec| spec.egress.as_ref())
            .is_some_and(|policy| !policy.allow.is_empty());
        let source = self
            .egress_bindings
            .get(id)
            .and_then(|binding| binding.source_namespace);
        if !protected && source.is_none() {
            return Ok(());
        }
        let fail = |reason: &str| {
            BunError::AdoptionState(format!("cannot restore egress for {id}: {reason}"))
        };
        let binding = self
            .egress_bindings
            .get(id)
            .ok_or_else(|| fail("original policy ownership is missing"))?;
        let boot_id = tokio::task::spawn_blocking(crate::bun::egress_owners::boot_id)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        let cgroup = self
            .supervisor
            .grill()
            .workload_cgroup(id)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        if binding.phase != PolicyPhase::Owned
            || binding.boot_id != boot_id
            || cgroup != Some(binding.cgroup_id)
            || !self
                .supervisor
                .grill()
                .honours_cgroup_path_for(&binding.original_spec)
        {
            return Err(fail("original runtime cgroup identity is unavailable"));
        }
        let handle = self
            .onion_ebpf
            .clone()
            .ok_or_else(|| fail("kernel data path is unavailable"))?;
        let mut ebpf = handle.lock().await;
        if !ebpf.is_attached()
            || !ebpf.connect6_attached()
            || !ebpf.sendmsg4_attached()
            || !ebpf.sendmsg6_attached()
            || (protected
                && !crate::sesame::egress::egress_enforced(&mut ebpf.bpf, binding.cgroup_id)
                    .map_err(|error| BunError::AdoptionState(error.to_string()))?)
        {
            return Err(fail("original enforcement is unavailable"));
        }
        if let Some(namespace) = source {
            let live =
                crate::sesame::firewall::read_firewall_state(&mut ebpf.bpf, binding.cgroup_id, 0)
                    .map_err(|error| BunError::AdoptionState(error.to_string()))?;
            if live.source_namespace_id != Some(namespace) {
                return Err(fail("original namespace enforcement is unavailable"));
            }
        }
        if !protected {
            return Ok(());
        }
        let cgroup_id = binding.cgroup_id;
        drop(ebpf);
        self.reprogram_cgroup_egress(cgroup_id, None)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    /// Require retained ownership and enforcement before publishing a live adopter.
    pub(super) async fn restore_live_egress(
        &mut self,
        _id: &crate::grill::InstanceId,
        record: &InstanceRecord,
    ) -> Result<(), BunError> {
        if record
            .app_spec
            .as_ref()
            .and_then(|spec| spec.egress.as_ref())
            .is_some_and(|policy| !policy.allow.is_empty())
        {
            return Err(BunError::AdoptionState(
                "protected adoption requires Linux eBPF support".into(),
            ));
        }
        Ok(())
    }
}

/// An egress owners checkpoint write still running after its turn ended
/// (#1263). The binding it records goes into `egress_bindings` only once it
/// has landed, so no co-tenant's map rebuild sees an owner the checkpoint
/// doesn't record yet.
#[cfg(any(test, all(feature = "ebpf", target_os = "linux")))]
#[derive(Debug)]
pub(super) struct EgressWrite {
    /// The fsync'd persist of the whole checkpoint.
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    /// The instance whose binding the write changes.
    instance: crate::grill::InstanceId,
    /// What that binding becomes once the write is durable; `None` forgets it.
    binding: Option<crate::bun::egress_owners::EgressBinding>,
    /// When the write started, for the log line a slow one leaves.
    started: std::time::Instant,
}

#[cfg(any(test, all(feature = "ebpf", target_os = "linux")))]
impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Checkpoint `id`'s binding as `binding` (`None` forgets it) and keep
    /// it in `egress_bindings` once the write is durable.
    ///
    /// The write waits until `deadline` at most, as a discovery journal
    /// write does (#1263): [`Landing::InFlight`] when it ran out first, and
    /// the write goes on off the loop. A write already in flight, anyone's,
    /// is `InFlight` too inside a turn, without waiting for it or writing.
    /// Either way the caller answers `StillRunning`, and its next ask finds
    /// the binding in `egress_bindings` once the write has landed. `None`
    /// waits for the write however long it takes.
    ///
    /// [`Landing::InFlight`]: super::discovery_ownership::Landing::InFlight
    pub(super) async fn checkpoint_egress_owner(
        &mut self,
        id: &crate::grill::InstanceId,
        binding: Option<crate::bun::egress_owners::EgressBinding>,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<super::discovery_ownership::Landing, BunError> {
        use super::discovery_ownership::Landing;
        // One write at a time, each from the state the last one left.
        if self.settle_egress_write_outside_turn().await? == Landing::InFlight {
            return Ok(Landing::InFlight);
        }
        if self.egress_store_uncertain {
            return Err(BunError::AdoptionState(
                "egress ownership persistence is uncertain; restart to recover the checkpoint"
                    .into(),
            ));
        }
        let Some(directory) = self.records_dir.clone() else {
            self.install_egress_owner(id, binding);
            return Ok(Landing::Landed);
        };
        let mut owners = self.egress_bindings.clone();
        match &binding {
            Some(binding) => owners.insert(id.clone(), binding.clone()),
            None => owners.remove(id),
        };
        let started = std::time::Instant::now();
        #[cfg(test)]
        let stalls = std::sync::Arc::clone(&self.loop_stalls);
        let mut task = tokio::spawn(async move {
            #[cfg(test)]
            stalls.hold(super::LoopStall::Persist).await;
            tokio::task::spawn_blocking(move || {
                crate::bun::egress_owners::persist(&directory, owners)
            })
            .await
            .map_err(std::io::Error::other)?
        });
        let joined = match deadline {
            // LOOP-INLINE: no deadline is only asked for outside the loop's turns
            None => Some((&mut task).await),
            Some(deadline) => tokio::time::timeout_at(deadline, &mut task).await.ok(),
        };
        // Timed apart from the discovery journal's write, so a slow turn's
        // line says which store held it (#1266).
        self.loop_meter.add_part(
            crate::bun::loop_meter::TurnPart::EgressWrite,
            started.elapsed(),
        );
        let Some(joined) = joined else {
            self.egress_write = Some(EgressWrite {
                task,
                instance: id.clone(),
                binding,
                started,
            });
            return Ok(Landing::InFlight);
        };
        self.finish_egress_write(joined, id, binding)
            .map(|()| Landing::Landed)
    }

    /// Whether an egress owners write is still going on off the loop, for
    /// the loop harness.
    #[cfg(test)]
    pub(super) fn egress_write_in_flight(&self) -> bool {
        self.egress_write.is_some()
    }

    /// Take back an egress owners write that has landed, or wait for one
    /// in flight outside a turn, where nothing else waits. Inside a turn a
    /// write still going is `InFlight`, unwaited. A write that failed
    /// fences every later egress mutation, and its error is the answer.
    pub(super) async fn settle_egress_write_outside_turn(
        &mut self,
    ) -> Result<super::discovery_ownership::Landing, BunError> {
        use super::discovery_ownership::Landing;
        let Some(write) = self.egress_write.as_mut() else {
            return Ok(Landing::Landed);
        };
        if !write.task.is_finished() && self.turn_deadline.is_some() {
            return Ok(Landing::InFlight);
        }
        let waited = std::time::Instant::now();
        // LOOP-INLINE: a finished write, or one outside the loop's turns
        let outcome = (&mut write.task).await;
        self.loop_meter.add_part(
            crate::bun::loop_meter::TurnPart::EgressWrite,
            waited.elapsed(),
        );
        let Some(EgressWrite {
            instance,
            binding,
            started,
            ..
        }) = self.egress_write.take()
        else {
            return Ok(Landing::Landed);
        };
        eprintln!(
            "bun: an egress owners write took {} ms; the agent loop went on meanwhile",
            started.elapsed().as_millis()
        );
        self.finish_egress_write(outcome, &instance, binding)
            .map(|()| Landing::Landed)
    }

    /// Take back an egress owners write that has already landed, on the
    /// tick, so its binding doesn't wait for its writer's next ask.
    pub(super) async fn collect_landed_egress_write(&mut self) {
        if !self
            .egress_write
            .as_ref()
            .is_some_and(|write| write.task.is_finished())
        {
            return;
        }
        if let Err(error) = self.settle_egress_write_outside_turn().await {
            eprintln!("bun: {error}");
        }
    }

    /// Install a durable write's binding, or fence egress after a failed one.
    fn finish_egress_write(
        &mut self,
        outcome: Result<std::io::Result<()>, tokio::task::JoinError>,
        id: &crate::grill::InstanceId,
        binding: Option<crate::bun::egress_owners::EgressBinding>,
    ) -> Result<(), BunError> {
        match outcome.unwrap_or_else(|error| Err(std::io::Error::other(error))) {
            Ok(()) => {
                self.install_egress_owner(id, binding);
                Ok(())
            }
            Err(error) => {
                // The write may or may not have reached disk. No later
                // mutation may overwrite it blindly; restart recovers.
                self.egress_store_uncertain = true;
                Err(BunError::AdoptionState(error.to_string()))
            }
        }
    }

    /// Put a durable binding in `egress_bindings`, or forget it.
    fn install_egress_owner(
        &mut self,
        id: &crate::grill::InstanceId,
        binding: Option<crate::bun::egress_owners::EgressBinding>,
    ) {
        match binding {
            Some(binding) => self.egress_bindings.insert(id.clone(), binding),
            None => self.egress_bindings.remove(id),
        };
    }
}

/// Nothing to collect without the eBPF data path.
#[cfg(not(any(test, all(feature = "ebpf", target_os = "linux"))))]
impl<G: Grill + Clone + 'static> BunAgent<G> {
    pub(super) async fn collect_landed_egress_write(&mut self) {}
}
