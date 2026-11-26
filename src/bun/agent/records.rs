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
            .map_err(|reason| fail(&reason))?;
        // Its sidecars started before it did, so they're recorded now too.
        if let Some(instance) = self.supervisor.get_instance(instance_id)
            && let Some(spec) = instance.oci_spec.clone()
            && let Some(owner) = self.sidecar_owner(instance_id)
        {
            self.record_sidecars(instance_id, owner, &spec, None);
        }
        Ok(())
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
            sidecar_restarts: None,
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
            .map_err(|reason| fail(format!("persist replacement record: {reason}")))?;
        // The supervisor's image, as the instance's own launch records it,
        // not the spec's (#2336).
        let owner = self
            .sidecar_owner(&instance.instance_id)
            .unwrap_or_else(|| sidecars::SidecarOwner {
                namespace: instance.namespace.clone(),
                app_name: instance.app_name.clone(),
                image: instance.spec.image.clone().unwrap_or_default(),
            });
        self.record_sidecars(&instance.instance_id, owner, &instance.oci_spec, None);
        Ok(())
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
            sidecar_restarts: None,
        };
        Ok(record)
    }

    /// Reconcile launches that reached the runtime before agent adoption was durable.
    ///
    /// Returns the records it retired as stale, which the caller mustn't
    /// adopt: a record from an earlier boot whose launch the runtime lost,
    /// and a record that conflicts with the launch it names (#1852). Also
    /// returns the sidecars it left running for adoption (#1315): those
    /// whose record names the launch the runtime has, beside an instance
    /// whose own record does. Every other sidecar launch is retired here.
    pub(super) async fn reconcile_runtime_launches(
        &mut self,
        records: &[crate::grill::records::InstanceRecord],
        sidecar_records: &[crate::grill::records::InstanceRecord],
        launches: &[crate::grill::RuntimeLaunch],
    ) -> Result<RuntimeReconciliation, BunError> {
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
        let kept: Vec<&crate::grill::records::InstanceRecord> = sidecar_records
            .iter()
            .filter(|record| {
                let beside_recorded =
                    crate::grill::oci::sidecar_parent(&InstanceId(record.instance_id.clone()))
                        .is_some_and(|(parent, _)| recorded.contains(parent.0.as_str()));
                let launched = inventory
                    .get(record.instance_id.as_str())
                    .is_some_and(|launch| launch.launched(&record.oci_spec));
                beside_recorded && launched
            })
            .collect();
        recorded.extend(kept.iter().map(|record| record.instance_id.as_str()));
        let kept_sidecars: std::collections::HashSet<String> = kept
            .iter()
            .map(|record| record.instance_id.clone())
            .collect();
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
        Ok(RuntimeReconciliation {
            stale,
            kept_sidecars,
        })
    }

    /// Adopt the recorded sidecars of `parent`, an instance just adopted,
    /// and register all its sidecars (#1315). A sidecar whose record the
    /// runtime vouches for runs on; any other is retired and due at once,
    /// to start again beside its instance from the spec it was launched
    /// with (#1434). Returns which it adopted and which it retired.
    ///
    /// A sidecar's adoption is best effort, as its record is: one the
    /// runtime can't vouch for either way (an error, or no answer in time)
    /// is killed and retired like one it refused, and the node starts.
    async fn adopt_sidecars(
        &mut self,
        parent: &InstanceId,
        specs: &[crate::config::app::SidecarSpec],
        records: &std::collections::HashMap<String, crate::grill::records::InstanceRecord>,
        adoptable: impl Fn(&str) -> bool,
        now: Instant,
    ) -> Result<SidecarAdoption, BunError> {
        let mut outcome = SidecarAdoption::default();
        for spec in specs {
            let id = crate::grill::oci::sidecar_instance_id(parent, &spec.name);
            let Some(record) = records.get(&id.0).filter(|_| adoptable(&id.0)) else {
                continue;
            };
            let answer = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.supervisor.grill().adopt(&id, record),
            )
            .await;
            match answer {
                Ok(Ok(true)) => {
                    outcome.adopted.insert(id);
                    continue;
                }
                // The runtime refused it and has cleaned up what it owned.
                Ok(Ok(false)) => {}
                Ok(Err(error)) => {
                    eprintln!("bun: {id}: sidecar adoption failed, starting it again: {error}");
                    sidecars::kill_unless_forgotten(
                        self.supervisor.grill(),
                        &id,
                        self.stop_confirmation_timeout,
                    )
                    .await?;
                }
                Err(_) => {
                    eprintln!("bun: {id}: sidecar adoption timed out, starting it again");
                    sidecars::kill_unless_forgotten(
                        self.supervisor.grill(),
                        &id,
                        self.stop_confirmation_timeout,
                    )
                    .await?;
                }
            }
            if !self.defer_startup_retirement(&id).await? {
                self.retire_instance_artifacts_fully(&id).await?;
            }
            // Retirement may have made the runtime forget it, so nothing
            // after this may kill it again (review of #2269).
            outcome.retired.insert(id);
        }
        // Their counts and backoffs come back from their records, whether
        // or not the runtime adopted them (#2336).
        let restored: std::collections::HashMap<
            InstanceId,
            crate::grill::records::SidecarRestarts,
        > = specs
            .iter()
            .map(|spec| crate::grill::oci::sidecar_instance_id(parent, &spec.name))
            .filter_map(|id| {
                let restarts = records.get(&id.0)?.sidecar_restarts?;
                Some((id, restarts))
            })
            .collect();
        self.sidecars.register_adopted(
            parent,
            specs,
            &outcome.adopted,
            &restored,
            now,
            std::time::SystemTime::now(),
        )?;
        if !outcome.adopted.is_empty() {
            // Their cgroups are bound to their app's namespace on the next
            // tick, as a restarted sidecar's is (#1426).
            self.namespace_firewall_stale = true;
        }
        Ok(outcome)
    }

    /// After adoption: retire every sidecar startup left running that no
    /// adopted instance took, and remove every sidecar record but the
    /// adopted ones' (#1315). One `adopt_sidecars` already retired is
    /// skipped: runc has forgotten it, and a second kill is `NotFound`.
    async fn finish_sidecar_adoption(
        &mut self,
        directory: PathBuf,
        kept: &std::collections::HashSet<String>,
        outcome: &SidecarAdoption,
    ) -> Result<(), BunError> {
        let adopted = &outcome.adopted;
        for id in kept {
            let id = InstanceId(id.clone());
            if adopted.contains(&id) || outcome.retired.contains(&id) {
                continue;
            }
            kill_runtime_instance(self.supervisor.grill(), &id, self.stop_confirmation_timeout)
                .await?;
            if !self.defer_startup_retirement(&id).await? {
                self.retire_instance_artifacts_fully(&id).await?;
            }
        }
        let keep: std::collections::HashSet<String> =
            adopted.iter().map(|id| id.0.clone()).collect();
        let swept =
            tokio::task::spawn_blocking(move || remove_sidecar_records_except(&directory, &keep))
                .await
                .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        if let Err(error) = swept {
            // Only an optimisation is lost: a stale record is never adopted.
            eprintln!("bun: stale sidecar adoption records kept: {error}");
        }
        Ok(())
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
        let sidecar_directory = crate::grill::records::sidecar_records_dir(&dir);
        let sidecar_records = load_sidecar_records(sidecar_directory.clone()).await;
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
        let RuntimeReconciliation {
            stale,
            kept_sidecars,
        } = match &launch_inventory {
            Some(launches) => {
                self.reconcile_runtime_launches(&records, &sidecar_records, launches)
                    .await?
            }
            // A runtime that can't list its launches retired none, so every
            // recorded sidecar beside a recorded instance may still run.
            None => RuntimeReconciliation {
                stale: std::collections::HashSet::new(),
                kept_sidecars: sidecar_records
                    .iter()
                    .filter(|record| {
                        crate::grill::oci::sidecar_parent(&InstanceId(record.instance_id.clone()))
                            .is_some_and(|(parent, _)| {
                                records.iter().any(|owner| owner.instance_id == parent.0)
                            })
                    })
                    .map(|record| record.instance_id.clone())
                    .collect(),
            },
        };
        let sidecar_records: std::collections::HashMap<_, _> = sidecar_records
            .into_iter()
            .map(|record| (record.instance_id.clone(), record))
            .collect();
        let mut sidecar_adoption = SidecarAdoption::default();
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
            // Its sidecars come with it where their records prove they still
            // run; the rest start again on the next tick (#1120, #1315).
            if let Some(spec) = record
                .app_spec
                .as_ref()
                .filter(|spec| !spec.sidecar.is_empty())
            {
                let outcome = self
                    .adopt_sidecars(
                        &instance_id,
                        &spec.sidecar,
                        &sidecar_records,
                        |id| kept_sidecars.contains(id),
                        now,
                    )
                    .await?;
                sidecar_adoption.adopted.extend(outcome.adopted);
                sidecar_adoption.retired.extend(outcome.retired);
            }
            if let Some(spec) = record.app_spec {
                self.deployed_specs.insert(key, spec);
            }
            // Keep the adopted instance's output flowing into the log store.
            // Logs are captured under the runtime id (the container's name).
            self.spawn_log_forwarder(&runtime_id, &record.app_name, &record.namespace);
            adopted_count += 1;
        }

        self.finish_sidecar_adoption(sidecar_directory, &kept_sidecars, &sidecar_adoption)
            .await?;
        if adopted_count > 0 {
            println!("bun: adopted {adopted_count} running instance(s) from a previous process");
        }
        if !sidecar_adoption.adopted.is_empty() {
            println!(
                "bun: adopted {} running sidecar(s) with them",
                sidecar_adoption.adopted.len()
            );
        }

        // Identity dirs of instances that died while bun was down have no
        // live owner, so they are stale key material.
        self.finish_discovery_recovery().await?;
        self.sweep_orphaned_identity_dirs().await;

        Ok(adopted_count)
    }
}

/// What reconciling the runtime's launches with the records found.
pub(super) struct RuntimeReconciliation {
    /// Records retired as stale, which mustn't be adopted.
    pub(super) stale: std::collections::HashSet<String>,
    /// Recorded sidecars left running for adoption beside their instance.
    pub(super) kept_sidecars: std::collections::HashSet<String>,
}

/// What startup did with the recorded sidecars (#1315).
#[derive(Debug, Default)]
struct SidecarAdoption {
    /// Adopted, running on beside their instance.
    adopted: std::collections::HashSet<InstanceId>,
    /// Refused or uncertain, and retired: due to start again.
    retired: std::collections::HashSet<InstanceId>,
}

/// The sidecars' adoption records in `directory` (#1315). Unreadable ones
/// are reported and treated as absent: a sidecar with no record is retired
/// and started again, as every sidecar was before they had records, so they
/// never need to stop the node the way an instance's must.
async fn load_sidecar_records(directory: PathBuf) -> Vec<crate::grill::records::InstanceRecord> {
    let loaded =
        tokio::task::spawn_blocking(move || crate::grill::records::load_records(&directory)).await;
    match loaded {
        Ok(Ok(records)) => records,
        Ok(Err(error)) => {
            eprintln!("bun: sidecar adoption records unreadable, so sidecars restart: {error}");
            Vec::new()
        }
        Err(error) => {
            eprintln!("bun: sidecar adoption records unread, so sidecars restart: {error}");
            Vec::new()
        }
    }
}

/// Remove every record in the sidecar records `directory` but `keep`'s,
/// readable or not. A missing directory is fine.
fn remove_sidecar_records_except(
    directory: &std::path::Path,
    keep: &std::collections::HashSet<String>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path.extension().and_then(|extension| extension.to_str()) != Some("json")
            || keep.contains(stem)
        {
            continue;
        }
        crate::grill::records::remove_record(directory, stem)?;
    }
    Ok(())
}

/// Whether the record's process provably died with an earlier boot.
fn from_an_earlier_boot(record: &crate::grill::records::InstanceRecord) -> bool {
    match (&record.boot_id, crate::grill::records::current_boot()) {
        (Some(recorded), Some(current)) => *recorded != current,
        _ => false,
    }
}
