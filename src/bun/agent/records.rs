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
        if instance.is_job && to == ContainerState::Starting {
            if self.job_store_uncertain {
                return Err(BunError::JobState(
                    "checkpoint is uncertain; restart Bun".into(),
                ));
            }
            let mut jobs = self.recorded_jobs.clone();
            let job = jobs
                .get_mut(&id.0)
                .ok_or_else(|| BunError::JobState(format!("missing attempt for {id}")))?;
            if job.phase != crate::bun::jobs::JobPhase::Preparing || job.runtime_absent {
                return Err(BunError::JobState(format!(
                    "job {id} has no prepared attempt"
                )));
            }
            job.phase = crate::bun::jobs::JobPhase::Launching;
            self.commit_jobs(jobs).await?;
        }
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
    /// metadata; short jobs recover through their separate attempt record.
    pub(super) async fn persist_instance_record(
        &self,
        instance_id: &InstanceId,
        evidence: &launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        let Some(dir) = self.records_dir.clone() else {
            return Ok(());
        };
        let fail = |reason: &str| {
            BunError::AdoptionState(format!("cannot record {instance_id}: {reason}"))
        };
        let instance = self
            .supervisor
            .get_instance(instance_id)
            .ok_or_else(|| fail("instance is missing"))?;
        let runtime = self.supervisor.grill().runtime_kind();
        // Apple workloads live in VMs. Record the launcher for provenance;
        // Apple adoption checks the named container, never this host PID.
        let pid = if runtime == crate::grill::records::RuntimeKind::Apple {
            Some(std::process::id())
        } else {
            evidence.pid
        };
        let Some(pid) = pid else {
            return if instance.is_job {
                Ok(())
            } else {
                Err(fail("runtime process identity is unavailable"))
            };
        };
        let Some(pid_started_at) = crate::grill::records::process_start_time(pid) else {
            return if instance.is_job {
                Ok(())
            } else {
                Err(fail("runtime process identity could not be observed"))
            };
        };
        let oci_spec = instance
            .oci_spec
            .clone()
            .ok_or_else(|| fail("runtime specification is missing"))?;

        let replica_index = crate::grill::InstanceIdentity::parse(&instance_id.0)
            .map(|ident| ident.ordinal)
            .unwrap_or(0);
        let record = crate::grill::records::InstanceRecord {
            schema: 2,
            instance_id: instance_id.0.clone(),
            namespace: instance.namespace.clone(),
            app_name: instance.app_name.clone(),
            replica_index,
            is_job: instance.is_job,
            image: instance.image.clone(),
            runtime,
            pid,
            pid_started_at,
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
        #[cfg(test)]
        self.loop_stalls.hold(LoopStall::Persist).await;
        // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
        tokio::task::spawn_blocking(move || crate::grill::records::write_record(&dir, &record))
            .await
            .map_err(|error| fail(&error.to_string()))?
            .map_err(|error| fail(&error.to_string()))
    }

    /// Persist launch evidence while the replacement is still owned by its
    /// rolling worker, before health wait or traffic publication.
    pub(super) async fn persist_rolling_instance(
        &self,
        instance: &RollingInstance,
    ) -> Result<(), BunError> {
        let Some(directory) = self.records_dir.clone() else {
            return Ok(());
        };
        let fail = |reason: String| BunError::DeployFailed {
            app_name: instance.app_name.clone(),
            reason,
        };
        let runtime = self.supervisor.grill().runtime_kind();
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
            schema: 2,
            instance_id: instance.instance_id.0.clone(),
            namespace: instance.namespace.clone(),
            app_name: instance.app_name.clone(),
            replica_index: identity.ordinal,
            is_job: false,
            image: instance.spec.image.clone().unwrap_or_default(),
            runtime,
            pid,
            pid_started_at,
            runc_container_id: matches!(runtime, crate::grill::records::RuntimeKind::Runc)
                .then(|| instance.instance_id.0.clone()),
            log_stem: instance.launch.log_stem.clone(),
            host_port: instance.host_port,
            app_spec: Some(instance.spec.clone()),
            oci_spec: instance.oci_spec.clone(),
            rootless_network: instance.launch.rootless_network.clone(),
        };
        // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
        tokio::task::spawn_blocking(move || {
            crate::grill::records::write_record(&directory, &record)
        })
        .await
        .map_err(|error| fail(format!("persist replacement record: {error}")))?
        .map_err(|error| fail(format!("persist replacement record: {error}")))
    }

    /// Reconcile launches that reached the runtime before agent adoption was durable.
    pub(super) async fn reconcile_runtime_launches(
        &mut self,
        records: &[crate::grill::records::InstanceRecord],
        jobs: &mut std::collections::BTreeMap<String, crate::bun::jobs::RecordedJob>,
        launches: &[crate::grill::RuntimeLaunch],
    ) -> Result<(), BunError> {
        use crate::bun::jobs::JobPhase;
        let inventory: std::collections::HashMap<_, _> = launches
            .iter()
            .map(|launch| (launch.instance_id.0.as_str(), launch))
            .collect();
        if inventory.len() != launches.len() {
            return Err(BunError::AdoptionState(
                "duplicate runtime launch identity".into(),
            ));
        }
        let recorded: std::collections::HashSet<_> = records
            .iter()
            .map(|record| record.instance_id.as_str())
            .collect();
        // Validate all cross-record relationships before retiring any owner.
        for record in records {
            let launch = inventory.get(record.instance_id.as_str()).ok_or_else(|| {
                BunError::AdoptionState(format!(
                    "instance {} has no runtime launch intent",
                    record.instance_id
                ))
            })?;
            if !launch.launched(&record.oci_spec)
                || jobs
                    .get(&record.instance_id)
                    .is_some_and(|job| job.phase == JobPhase::Preparing)
            {
                return Err(BunError::AdoptionState(format!(
                    "instance {} conflicts with runtime preparation",
                    record.instance_id
                )));
            }
        }
        for (id, job) in jobs.iter() {
            if !inventory.contains_key(id.as_str())
                && !job.runtime_absent
                && job.phase != JobPhase::Preparing
            {
                return Err(BunError::AdoptionState(format!(
                    "job {id} has no runtime launch intent"
                )));
            }
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
            if state != ContainerState::Stopped
                && jobs.get(&id.0).is_some_and(|job| job.runtime_absent)
            {
                return Err(BunError::AdoptionState(format!(
                    "job {id} has conflicting live and terminal evidence"
                )));
            }
            // Without the agent's acknowledgement record an active launch has
            // an uncertain outcome. Fence it before ordinary desired-state
            // reconciliation can authorise any replacement.
            if state != ContainerState::Stopped {
                kill_runtime_instance(self.supervisor.grill(), id, self.stop_confirmation_timeout)
                    .await?;
            }
            if let Some(job) = jobs.get_mut(&id.0) {
                job.runtime_absent = true;
                job.phase = match job.phase {
                    JobPhase::Launching if state == ContainerState::Stopped => {
                        match self.supervisor.grill().exit_code(id).await? {
                            Some(code) => JobPhase::Exited { code },
                            None => JobPhase::Unknown,
                        }
                    }
                    JobPhase::Preparing | JobPhase::Launching => JobPhase::Unknown,
                    JobPhase::Stopping => JobPhase::Stopped,
                    ref phase => phase.clone(),
                };
                self.commit_jobs(jobs.clone()).await?;
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
        for (id, job) in jobs.iter_mut() {
            if !inventory.contains_key(id.as_str()) && job.phase == JobPhase::Preparing {
                // A complete mandatory intent inventory plus the pre-execution
                // phase proves no runtime was activated for this preparation.
                job.phase = JobPhase::Unknown;
                job.runtime_absent = true;
            }
        }
        self.commit_jobs(jobs.clone()).await
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
    /// Jobs restore durable retry budgets and retain unknown outcomes. App
    /// backoff starts fresh; normal reconciliation rebuilds cluster routing.
    pub async fn adopt_recorded_instances(&mut self) -> Result<usize, BunError> {
        let Some(dir) = self.records_dir.clone() else {
            return Ok(0);
        };
        let now = Instant::now();
        let mut adopted_count = 0;

        let records_dir = dir.clone();
        let (records, schedules, jobs) = tokio::task::spawn_blocking(move || {
            let records = crate::grill::records::load_records(&records_dir)?;
            let schedules = crate::bun::schedules::load(&records_dir)?;
            let jobs = crate::bun::jobs::load(&records_dir)?;
            Ok::<_, std::io::Error>((records, schedules, jobs))
        })
        .await
        .map_err(|error| BunError::AdoptionState(error.to_string()))?
        .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.require_discovery_recovery(!records.is_empty(), false)?;
        for job in jobs.values() {
            if job.runtime != self.supervisor.grill().runtime_kind() {
                return Err(BunError::AdoptionState(
                    "job attempt belongs to another runtime".into(),
                ));
            }
        }
        // Validate the entire inventory before adopting or deleting any owner.
        for record in &records {
            if record.is_job && !jobs.contains_key(&record.instance_id) {
                return Err(BunError::AdoptionState(format!(
                    "job {} has no durable attempt",
                    record.instance_id
                )));
            }
            if let Some(job) = jobs.get(&record.instance_id)
                && (!record.is_job
                    || record.namespace != job.namespace
                    || record.app_name != job.name
                    || record.image != job.spec.image.clone().unwrap_or_default())
            {
                return Err(BunError::AdoptionState(
                    "job record conflicts with attempt ownership".into(),
                ));
            }
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
            if record.runtime != self.supervisor.grill().runtime_kind() {
                return Err(BunError::AdoptionState(format!(
                    "instance {} belongs to {:?}, but the selected runtime is {:?}",
                    record.instance_id,
                    record.runtime,
                    self.supervisor.grill().runtime_kind(),
                )));
            }
        }
        let mut restored = std::collections::HashMap::new();
        for stored in schedules {
            let namespace = stored.spec.namespace.as_deref().unwrap_or("default");
            if namespace != stored.namespace
                || stored.name.is_empty()
                || stored.last_fired_minute.is_some_and(|minute| minute < 0)
            {
                return Err(BunError::AdoptionState(
                    "invalid scheduled-job identity or firing stamp".into(),
                ));
            }
            let mut config = Config::default();
            config.job.insert(stored.name.clone(), stored.spec.clone());
            config
                .validate()
                .map_err(|error| BunError::AdoptionState(error.to_string()))?;
            let expression = stored.spec.schedule.as_deref().ok_or_else(|| {
                BunError::AdoptionState("recorded cron job has no schedule".into())
            })?;
            let schedule = crate::meat::cron::CronSchedule::parse(expression)
                .map_err(|error| BunError::AdoptionState(error.to_string()))?;
            let key = (stored.name.clone(), stored.namespace.clone());
            let job = ScheduledJob {
                name: stored.name,
                namespace: stored.namespace,
                spec: stored.spec,
                schedule,
                last_fired_minute: stored.last_fired_minute,
            };
            if restored.insert(key, job).is_some() {
                return Err(BunError::AdoptionState(
                    "duplicate scheduled-job identity".into(),
                ));
            }
        }
        self.scheduled_jobs = restored;
        self.scheduled_jobs_store_uncertain = false;
        self.recorded_jobs = jobs.clone();
        self.job_store_uncertain = false;
        let mut recovered_jobs = jobs;
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
        if let Some(launches) = &launch_inventory {
            self.reconcile_runtime_launches(&records, &mut recovered_jobs, launches)
                .await?;
        }
        let mut adopted_jobs = std::collections::HashSet::new();
        for record in records {
            // Startup preflight proved that runtime, record and supervisor
            // share the same identity. Never invent an alias for an old owner.
            let runtime_id = InstanceId(record.instance_id.clone());
            let instance_id = runtime_id.clone();
            // Never clobber an instance the current process already tracks.
            if self.supervisor.get_instance(&instance_id).is_some() {
                if record.is_job {
                    adopted_jobs.insert(instance_id.0.clone());
                }
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
                if let Some(job) = recovered_jobs.get_mut(&runtime_id.0) {
                    job.runtime_absent = true;
                    if matches!(
                        job.phase,
                        crate::bun::jobs::JobPhase::Preparing
                            | crate::bun::jobs::JobPhase::Launching
                    ) {
                        job.phase = if launch_inventory.is_some()
                            && job.phase == crate::bun::jobs::JobPhase::Launching
                        {
                            match self.supervisor.grill().exit_code(&runtime_id).await? {
                                Some(code) => crate::bun::jobs::JobPhase::Exited { code },
                                None => crate::bun::jobs::JobPhase::Unknown,
                            }
                        } else {
                            crate::bun::jobs::JobPhase::Unknown
                        };
                    }
                    // Preserve the positive observation before deleting the
                    // only record that let this runtime prove absence.
                    self.commit_jobs(recovered_jobs.clone()).await?;
                }
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

            let recorded_job = recovered_jobs.get(&runtime_id.0);
            if let Some(job) = recorded_job {
                if matches!(job.phase, crate::bun::jobs::JobPhase::Exited { .. })
                    || job.runtime_absent
                {
                    return Err(BunError::AdoptionState(format!(
                        "job {runtime_id} has conflicting live and terminal evidence"
                    )));
                }
                adopted_jobs.insert(runtime_id.0.clone());
            }
            // The surviving instance still holds its port.
            if let Some(port) = record.host_port {
                self.supervisor.port_allocator.reserve(port).await?;
            }

            // Rebuild the health check from the recorded app spec.
            let health_config = record.app_spec.as_ref().and_then(|spec| {
                let health = spec.health.as_ref()?;
                let port = spec.port?;
                Some(crate::bun::health::HealthCheckConfig::from_spec(
                    health, port,
                ))
            });
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
            let identity = match crate::sesame::identity::load_identity(&identity_dir) {
                Ok(identity) => identity,
                Err(e) => {
                    eprintln!("bun: warning: could not restore identity for {runtime_id}: {e}");
                    None
                }
            };
            let identity_mount = identity.is_some().then(|| identity_dir.clone());

            let key = (record.app_name.clone(), record.namespace.clone());
            let instance = WorkloadInstance {
                id: instance_id.clone(),
                app_name: record.app_name.clone(),
                namespace: record.namespace.clone(),
                state: if recorded_job.is_some_and(|job| {
                    matches!(
                        job.phase,
                        crate::bun::jobs::JobPhase::Stopping | crate::bun::jobs::JobPhase::Stopped
                    )
                }) {
                    ContainerState::Stopping
                } else {
                    ContainerState::Running
                },
                health_counters: crate::bun::health::HealthCounters::new(),
                restart_count: recorded_job.map_or(0, |job| job.restart_count),
                last_restart: None,
                host_port: record.host_port,
                container_ip: None,
                created_at: now,
                restart_policy: if record.is_job {
                    crate::bun::restart::RestartPolicy::for_job(crate::bun::jobs::MAX_RETRIES)
                } else {
                    crate::bun::restart::RestartPolicy::default()
                },
                health_config,
                is_job: record.is_job,
                retry_pending: false,
                image: record.image.clone(),
                oci_spec: Some(record.oci_spec.clone()),
                identity,
                identity_mount,
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
            if !record.is_job {
                self.note_adopted_instance(
                    &key,
                    &instance_id,
                    record.app_spec.as_ref(),
                    &record.image,
                );
            }
            if let Some(spec) = record.app_spec {
                self.deployed_specs.insert(key, spec);
            }
            // Keep the adopted instance's output flowing into the log store.
            // Logs are captured under the runtime id (the container's name).
            self.spawn_log_forwarder(&runtime_id, &record.app_name, &record.namespace);
            adopted_count += 1;
        }

        for (id, job) in &mut recovered_jobs {
            if !adopted_jobs.contains(id)
                && matches!(
                    job.phase,
                    crate::bun::jobs::JobPhase::Preparing | crate::bun::jobs::JobPhase::Launching
                )
            {
                job.phase = crate::bun::jobs::JobPhase::Unknown;
            }
        }
        self.commit_jobs(recovered_jobs).await?;
        // Keep terminal/unknown evidence visible even after its runtime is gone.
        for (id, job) in self.recorded_jobs.clone() {
            let instance_id = InstanceId(id);
            if self.supervisor.get_instance(&instance_id).is_some() {
                continue;
            }
            self.supervisor
                .deploy_job(&job.name, &job.namespace, &job.spec, now)
                .await?;
            let cgroup = crate::grill::cgroup::instance_cgroup_path(
                &job.namespace,
                &job.name,
                &instance_id,
            )?;
            let spec = generate_job_oci_spec(
                &job.name,
                &job.namespace,
                &job.spec,
                &cgroup.to_string_lossy(),
                None,
            );
            if let Some(instance) = self.supervisor.get_instance_mut(&instance_id) {
                instance.restart_count = job.restart_count;
                instance.restart_policy =
                    crate::bun::restart::RestartPolicy::for_job(crate::bun::jobs::MAX_RETRIES);
                instance.state = match job.phase {
                    crate::bun::jobs::JobPhase::Unknown => ContainerState::Failed,
                    crate::bun::jobs::JobPhase::Exited { code }
                        if code != 0 && job.restart_count >= crate::bun::jobs::MAX_RETRIES =>
                    {
                        ContainerState::Failed
                    }
                    crate::bun::jobs::JobPhase::Stopping => ContainerState::Stopping,
                    _ => ContainerState::Stopped,
                };
                instance.retry_pending = matches!(job.phase, crate::bun::jobs::JobPhase::Exited { code } if code != 0)
                    && job.restart_count < crate::bun::jobs::MAX_RETRIES;
                instance.oci_spec = Some(spec);
                instance.last_restart = Some(now);
            }
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
