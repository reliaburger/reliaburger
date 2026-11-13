//! Durable instance records: persisting them, reconciling launches the
//! runtime reports, and adopting recorded instances after a restart.

use super::*;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Commit permission to execute only after the runtime has prepared this attempt.
    pub(super) async fn transition_deploy_state(
        &mut self,
        id: &InstanceId,
        to: ContainerState,
    ) -> Result<(), BunError> {
        let instance =
            self.supervisor
                .get_instance(id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: id.clone(),
                })?;
        let state = instance.state.transition_to(to)?;
        let instance =
            self.supervisor
                .get_instance_mut(id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: id.clone(),
                })?;
        instance.state = state;
        Ok(())
    }

    /// Write (or refresh) the instance record used for adoption after a bun
    /// restart or self-upgrade exec. Application acknowledgement requires this
    /// metadata.
    ///
    /// The write runs in a task ([`off_loop_work`]) and holds the turn only
    /// until its deadline (#1853): `StillRunning` means ask again, and the
    /// ask that finds it landed carries on. Nothing may treat the instance
    /// as running before then, because the record is what a restarted Bun
    /// adopts it by.
    pub(super) async fn persist_instance_record(
        &mut self,
        instance_id: &InstanceId,
        evidence: &launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        let Some(dir) = self.records_dir.clone() else {
            return Ok(());
        };
        let fail = |reason: &str| {
            BunError::AdoptionState(format!("cannot record {instance_id}: {reason}"))
        };
        let key = off_loop_work::WorkKey::PersistRecord(instance_id.clone());
        // This launch's write, not the instance's: a restart keeps
        // `created_at`, and must never collect an earlier launch's write.
        let incarnation = evidence.read_at;
        // An ask after `StillRunning` only collects the write it started.
        let record = if self.off_loop_work.started(&key, incarnation) {
            None
        } else {
            Some(self.instance_record(instance_id, evidence)?)
        };
        self.write_record_within_turn(key, incarnation, dir, record)
            .await?
            .map_err(|reason| fail(&reason))
    }

    /// The adoption record for a fresh or restarted instance.
    fn instance_record(
        &self,
        instance_id: &InstanceId,
        evidence: &launch_evidence::LaunchEvidence,
    ) -> Result<crate::grill::records::InstanceRecord, BunError> {
        let fail = |reason: &str| {
            BunError::AdoptionState(format!("cannot record {instance_id}: {reason}"))
        };
        let instance = self
            .supervisor
            .get_instance(instance_id)
            .ok_or_else(|| fail("instance is missing"))?;
        let runtime = self.supervisor.grill().runtime_kind_for_host(
            instance
                .oci_spec
                .as_ref()
                .is_some_and(|spec| spec.host_process),
        );
        // Apple workloads live in VMs. Record the launcher for provenance;
        // Apple adoption checks the named container, never this host PID.
        let pid = if runtime == crate::grill::records::RuntimeKind::Apple {
            Some(std::process::id())
        } else {
            evidence.pid
        };
        let Some(pid) = pid else {
            return Err(fail("runtime process identity is unavailable"));
        };
        let Some(pid_started_at) = crate::grill::records::process_start_time(pid) else {
            return Err(fail("runtime process identity could not be observed"));
        };
        let oci_spec = instance
            .oci_spec
            .clone()
            .ok_or_else(|| fail("runtime specification is missing"))?;

        let replica_index = crate::grill::InstanceIdentity::parse(&instance_id.0)
            .map(|ident| ident.ordinal)
            .unwrap_or(0);
        let record = crate::grill::records::InstanceRecord {
            schema: crate::grill::records::RECORD_SCHEMA,
            instance_id: instance_id.0.clone(),
            namespace: instance.namespace.clone(),
            app_name: instance.app_name.clone(),
            replica_index,
            image: instance.image.clone(),
            runtime,
            pid,
            pid_started_at,
            boot_id: crate::grill::records::current_boot(),
            // RunC uses the instance id as the container id (see runc.rs).
            runc_container_id: matches!(runtime, crate::grill::records::RuntimeKind::Runc)
                .then(|| instance_id.0.clone()),
            log_stem: evidence.log_stem.clone(),
            host_port: instance.host_port,
            app_spec: self
                .deployed_specs
                .get(&(instance.app_name.clone(), instance.namespace.clone()))
                .cloned(),
            oci_spec,
            rootless_network: evidence.rootless_network.clone(),
        };
        Ok(record)
    }

    /// Write `record` under `key` from a task, waiting for it only until the
    /// turn's deadline. `None` collects a write an earlier ask started.
    async fn write_record_within_turn(
        &mut self,
        key: off_loop_work::WorkKey,
        incarnation: Option<Instant>,
        directory: PathBuf,
        record: Option<crate::grill::records::InstanceRecord>,
    ) -> Result<Result<(), String>, BunError> {
        // Counted where the turn starts the write, not on each ask (#1625).
        #[cfg(test)]
        let delay = record
            .as_ref()
            .and_then(|_| self.loop_stalls.reach(LoopStall::RecordWrite));
        let write = async move {
            // Only reached when no task exists for this incarnation, which
            // `started` ruled out before passing `None`.
            let Some(record) = record else {
                return Err("the record write was never started".to_string());
            };
            #[cfg(test)]
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            tokio::task::spawn_blocking(move || {
                crate::grill::records::write_record(&directory, &record)
            })
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, incarnation, write).await
    }

    /// Whether `id`'s adoption record is being written right now. Removing
    /// the record meanwhile would let the write land after it and leave a
    /// record for an instance that no longer exists.
    pub(super) fn record_write_in_flight(&self, id: &InstanceId) -> bool {
        self.off_loop_work
            .is_running(&off_loop_work::WorkKey::PersistRecord(id.clone()))
    }

    /// Before removing `id`'s record, drop a write of it that landed but
    /// was never collected (its instance left `Starting` first), so no
    /// later ask can mistake that result for its own.
    pub(super) fn forget_landed_record_write(&mut self, id: &InstanceId) {
        self.off_loop_work
            .forget_finished(&off_loop_work::WorkKey::PersistRecord(id.clone()));
    }

    /// Persist launch evidence while the replacement is still owned by its
    /// rolling worker, before health wait or traffic publication. Like
    /// [`Self::persist_instance_record`], the write holds the turn only until
    /// its deadline: `StillRunning` means ask again.
    pub(super) async fn persist_rolling_instance(
        &mut self,
        instance: &RollingInstance,
    ) -> Result<(), BunError> {
        let Some(directory) = self.records_dir.clone() else {
            return Ok(());
        };
        let fail = |reason: String| BunError::DeployFailed {
            app_name: instance.app_name.clone(),
            reason,
        };
        let key = off_loop_work::WorkKey::PersistRecord(instance.instance_id.clone());
        let incarnation = instance.launch.read_at;
        let record = if self.off_loop_work.started(&key, incarnation) {
            None
        } else {
            Some(self.rolling_record(instance)?)
        };
        self.write_record_within_turn(key, incarnation, directory, record)
            .await?
            .map_err(|reason| fail(format!("persist replacement record: {reason}")))
    }

    /// The adoption record for a rolling replacement.
    fn rolling_record(
        &self,
        instance: &RollingInstance,
    ) -> Result<crate::grill::records::InstanceRecord, BunError> {
        let fail = |reason: String| BunError::DeployFailed {
            app_name: instance.app_name.clone(),
            reason,
        };
        let runtime = self.supervisor.grill().runtime_kind_for(&instance.oci_spec);
        let pid = if runtime == crate::grill::records::RuntimeKind::Apple {
            Some(std::process::id())
        } else {
            instance.launch.pid
        }
        .ok_or_else(|| {
            fail(
                "runtime did not expose a process identity for durable replacement adoption".into(),
            )
        })?;
        let pid_started_at = crate::grill::records::process_start_time(pid).ok_or_else(|| {
            fail("replacement process exited before its identity could be recorded".into())
        })?;
        let identity = crate::grill::InstanceIdentity::parse(&instance.instance_id.0)
            .ok_or_else(|| fail("replacement has an invalid instance identity".into()))?;
        let record = crate::grill::records::InstanceRecord {
            schema: crate::grill::records::RECORD_SCHEMA,
            instance_id: instance.instance_id.0.clone(),
            namespace: instance.namespace.clone(),
            app_name: instance.app_name.clone(),
            replica_index: identity.ordinal,
            image: instance.spec.image.clone().unwrap_or_default(),
            runtime,
            pid,
            pid_started_at,
            boot_id: crate::grill::records::current_boot(),
            runc_container_id: matches!(runtime, crate::grill::records::RuntimeKind::Runc)
                .then(|| instance.instance_id.0.clone()),
            log_stem: instance.launch.log_stem.clone(),
            host_port: instance.host_port,
            app_spec: Some(instance.spec.clone()),
            oci_spec: instance.oci_spec.clone(),
            rootless_network: instance.launch.rootless_network.clone(),
        };
        Ok(record)
    }

    /// Reconcile launches that reached the runtime before agent adoption was durable.
    ///
    /// Returns the records it retired as stale, which the caller mustn't
    /// adopt: a record from an earlier boot whose launch the runtime lost,
    /// and a record that conflicts with the launch it names (#1852).
    pub(super) async fn reconcile_runtime_launches(
        &mut self,
        records: &[crate::grill::records::InstanceRecord],
        launches: &[crate::grill::RuntimeLaunch],
    ) -> Result<std::collections::HashSet<String>, BunError> {
        let inventory: std::collections::HashMap<_, _> = launches
            .iter()
            .map(|launch| (launch.instance_id.0.as_str(), launch))
            .collect();
        if inventory.len() != launches.len() {
            return Err(BunError::AdoptionState(
                "duplicate runtime launch identity".into(),
            ));
        }
        let mut recorded: std::collections::HashSet<_> = records
            .iter()
            .map(|record| record.instance_id.as_str())
            .collect();
        // The record and the runtime's launch intent are written at different
        // moments, and a power cut can keep one without the other. Such a
        // record no longer describes what runs, so it's retired and the
        // instance reschedules, rather than one half-written record stopping
        // the whole node (#1852). Validate every record before retiring any.
        let mut lost_launches = Vec::new();
        let mut stale = std::collections::HashSet::new();
        for record in records {
            let Some(launch) = inventory.get(record.instance_id.as_str()) else {
                // With no launch, nothing in the runtime owns the record's
                // process. Only a reboot proves that process is gone.
                if !from_an_earlier_boot(record) {
                    return Err(BunError::AdoptionState(format!(
                        "instance {} has no runtime launch intent",
                        record.instance_id
                    )));
                }
                lost_launches.push(InstanceId(record.instance_id.clone()));
                stale.insert(record.instance_id.clone());
                continue;
            };
            if !launch.launched(&record.oci_spec) {
                // The launch is what the runtime runs: fence and retire it
                // below like a launch with no record at all.
                eprintln!(
                    "bun: {}: adoption record conflicts with its runtime launch; retiring the launch",
                    record.instance_id
                );
                stale.insert(record.instance_id.clone());
            }
        }
        recorded.retain(|id| !stale.contains(*id));
        for id in &lost_launches {
            eprintln!(
                "bun: {id}: adoption record from an earlier boot has no runtime launch; retiring it"
            );
            self.retire_instance_artifacts_fully(id).await?;
        }
        let mut retired = Vec::new();
        for launch in launches {
            let id = &launch.instance_id;
            if recorded.contains(id.0.as_str()) {
                continue;
            }
            let state = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.supervisor.grill().state(id),
            )
            .await
            .map_err(|_| {
                BunError::AdoptionState(format!("runtime inspection timed out for {id}"))
            })??;
            // Without the agent's acknowledgement record an active launch has
            // an uncertain outcome. Fence it before ordinary desired-state
            // reconciliation can authorise any replacement.
            if state != ContainerState::Stopped {
                kill_runtime_instance(self.supervisor.grill(), id, self.stop_confirmation_timeout)
                    .await?;
            }
            retired.push(id.clone());
        }
        // An unacknowledged init can share its parent's cgroup. Retiring
        // parent artifacts first would lift policy while that init still runs.
        for id in retired {
            if !self.defer_startup_retirement(&id).await? {
                self.retire_instance_artifacts_fully(&id).await?;
            }
        }
        Ok(stale)
    }

    /// Adopt still-running workloads recorded by a previous bun process.
    ///
    /// Called once at startup, BEFORE any reconciliation: adopted instances
    /// are seeded into the supervisor as Running so they don't get
    /// double-started. Records whose process is gone are deleted (the
    /// instance reschedules through the normal path). Returns the number
    /// of instances adopted. Any uncertain observation refuses startup and
    /// preserves durable records and identity material for recovery.
    ///
    /// App backoff starts fresh; normal reconciliation rebuilds cluster
    /// routing.
    pub async fn adopt_recorded_instances(&mut self) -> Result<usize, BunError> {
        let Some(dir) = self.records_dir.clone() else {
            return Ok(0);
        };
        let now = Instant::now();
        let mut adopted_count = 0;

        let records_dir = dir.clone();
        let records =
            tokio::task::spawn_blocking(move || crate::grill::records::load_records(&records_dir))
                .await
                .map_err(|error| BunError::AdoptionState(error.to_string()))?
                .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.require_discovery_recovery(!records.is_empty(), false)?;
        // Validate the entire inventory before adopting or deleting any owner.
        for record in &records {
            let base = crate::grill::InstanceIdentity::new(
                &record.namespace,
                &record.app_name,
                record.replica_index,
            );
            let generation = record
                .instance_id
                .strip_prefix(&format!("{}__{}-g", record.namespace, record.app_name))
                .and_then(|suffix| suffix.strip_suffix(&format!("-{}", record.replica_index)))
                .and_then(|value| value.parse::<u64>().ok());
            let matches_generation = generation.is_some_and(|generation| {
                crate::grill::InstanceIdentity::canary(
                    &record.namespace,
                    &record.app_name,
                    generation,
                    record.replica_index,
                )
                .instance_id()
                .0 == record.instance_id
            });
            if !crate::config::valid_workload_label(&record.namespace)
                || !crate::config::valid_workload_label(&record.app_name)
                || (base.instance_id().0 != record.instance_id && !matches_generation)
                || record
                    .app_spec
                    .as_ref()
                    .and_then(|spec| spec.namespace.as_ref())
                    .is_some_and(|namespace| namespace != &record.namespace)
            {
                return Err(BunError::AdoptionState(format!(
                    "unsupported or inconsistent workload identity in record {:?}; the record and runtime are preserved",
                    record.instance_id,
                )));
            }
            if record.runtime != self.supervisor.grill().runtime_kind_for(&record.oci_spec) {
                return Err(BunError::AdoptionState(format!(
                    "instance {} belongs to {:?}, but the selected runtime is {:?}",
                    record.instance_id,
                    record.runtime,
                    self.supervisor.grill().runtime_kind(),
                )));
            }
        }
        let launch_inventory = self
            .runtime_inventory(RUNTIME_INVENTORY_TIMEOUT, |reason| {
                BunError::AdoptionState(format!("startup adoption {reason}"))
            })
            .await?;
        self.require_discovery_recovery(
            !records.is_empty(),
            launch_inventory
                .as_ref()
                .is_some_and(|launches| !launches.is_empty()),
        )?;
        self.validate_recovered_discovery(&records, launch_inventory.as_deref())?;
        self.restore_egress_owners(&records, launch_inventory.as_deref())
            .await?;
        self.replay_discovery_releases().await?;
        let stale = match &launch_inventory {
            Some(launches) => self.reconcile_runtime_launches(&records, launches).await?,
            None => std::collections::HashSet::new(),
        };
        for record in records {
            if stale.contains(&record.instance_id) {
                continue;
            }
            // Startup preflight proved that runtime, record and supervisor
            // share the same identity. Never invent an alias for an old owner.
            let runtime_id = InstanceId(record.instance_id.clone());
            let instance_id = runtime_id.clone();
            // Never clobber an instance the current process already tracks.
            if self.supervisor.get_instance(&instance_id).is_some() {
                continue;
            }

            let adopted = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.supervisor.grill().adopt(&runtime_id, &record),
            )
            .await
            .map_err(|_| {
                BunError::AdoptionState(format!("runtime adoption timed out for {runtime_id}"))
            })??;
            if !adopted {
                if !self.defer_startup_retirement(&runtime_id).await? {
                    self.retire_instance_artifacts_fully(&runtime_id).await?;
                }
                continue;
            }

            if let Err(error) = self.restore_live_egress(&runtime_id, &record).await {
                // Adoption has proved this is our surviving runtime. Do not
                // publish it as Running without confirmed policy ownership.
                kill_runtime_instance(
                    self.supervisor.grill(),
                    &runtime_id,
                    self.stop_confirmation_timeout,
                )
                .await?;
                return Err(error);
            }

            // No fault survives its Bun: lift any cap or pause the previous
            // process left on this instance before publishing it (#699).
            self.release_leftover_faults(&runtime_id, &record.oci_spec)
                .await;

            // The surviving instance still holds its port.
            if let Some(port) = record.host_port {
                self.supervisor.port_allocator.reserve(port).await?;
            }

            // Rebuild the health check from the recorded app spec.
            let health_config = record
                .app_spec
                .as_ref()
                .and_then(crate::bun::health::HealthCheckConfig::for_app);
            if let Some(config) = &health_config {
                self.supervisor
                    .register_health(instance_id.clone(), config.clone(), now);
            }

            // Rebuild the workload's identity and rotation schedule from
            // its per-instance directory, so an adopted instance keeps
            // rotating on time instead of coming back with
            // `identity: None` (D9). The directory was created under the
            // runtime id, which is also the supervisor key. An
            // unprovisioned directory loads as `None` and the rotation loop
            // provisions afresh.
            let identity_dir = self.instance_identity_dir(&runtime_id);
            // BLOCKING-OK: adoption at startup, before the agent loop starts; the identity directory is tmpfs on a root node
            let identity = match crate::sesame::identity::load_identity(&identity_dir) {
                Ok(identity) => identity,
                Err(e) => {
                    eprintln!("bun: warning: could not restore identity for {runtime_id}: {e}");
                    None
                }
            };
            let identity_mount = identity.is_some().then(|| identity_dir.clone());

            let request = record
                .app_spec
                .as_ref()
                .map(crate::meat::admission::app_requests)
                .unwrap_or_default();
            self.supervisor
                .reserve_adopted_execution(instance_id.clone(), request);
            let key = (record.app_name.clone(), record.namespace.clone());
            let instance = WorkloadInstance {
                id: instance_id.clone(),
                app_name: record.app_name.clone(),
                namespace: record.namespace.clone(),
                state: ContainerState::Running,
                health_counters: crate::bun::health::HealthCounters::new(),
                restart_count: 0,
                last_restart: None,
                host_port: record.host_port,
                container_ip: None,
                created_at: now,
                restart_policy: crate::bun::restart::RestartPolicy::default(),
                health_config,
                retry_pending: false,
                image: record.image.clone(),
                oci_spec: Some(record.oci_spec.clone()),
                identity,
                identity_mount,
                launch_failure: None,
                restart_cause: None,
            };
            self.supervisor
                .instances
                .insert(instance_id.clone(), instance);
            // Startup read the inventory once, for every adopted launch;
            // publication takes the execution from here (#419).
            if let Some(launches) = &launch_inventory {
                self.record_launch_execution(
                    &instance_id,
                    &super::launch_evidence::LaunchExecution::of(launches, &instance_id),
                )?;
            }
            self.supervisor
                .app_instances
                .entry(key.clone())
                .or_default()
                .push(instance_id.clone());
            self.note_adopted_instance(&key, &instance_id, record.app_spec.as_ref(), &record.image);
            // Its sidecars have no records of their own, so startup retired
            // them with the other unrecorded launches. They start again on
            // the next tick, beside the adopted instance, from the spec it
            // was launched with (#1120, #1434).
            if let Some(spec) = record
                .app_spec
                .as_ref()
                .filter(|spec| !spec.sidecar.is_empty())
            {
                self.sidecars
                    .register_down(&instance_id, &spec.sidecar, now)?;
            }
            if let Some(spec) = record.app_spec {
                self.deployed_specs.insert(key, spec);
            }
            // Keep the adopted instance's output flowing into the log store.
            // Logs are captured under the runtime id (the container's name).
            self.spawn_log_forwarder(&runtime_id, &record.app_name, &record.namespace);
            adopted_count += 1;
        }

        if adopted_count > 0 {
            println!("bun: adopted {adopted_count} running instance(s) from a previous process");
        }

        // Identity dirs of instances that died while bun was down have no
        // live owner, so they are stale key material.
        self.finish_discovery_recovery().await?;
        self.sweep_orphaned_identity_dirs().await;

        Ok(adopted_count)
    }
}

/// Whether the record's process provably died with an earlier boot.
fn from_an_earlier_boot(record: &crate::grill::records::InstanceRecord) -> bool {
    match (&record.boot_id, crate::grill::records::current_boot()) {
        (Some(recorded), Some(current)) => *recorded != current,
        _ => false,
    }
}
