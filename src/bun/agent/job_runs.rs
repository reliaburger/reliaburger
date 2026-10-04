//! Jobs on the loop: the job ledger, retries, scheduled (cron) jobs and
//! what a job's exit means.

use super::*;

/// A job registered to run on a cron schedule rather than at deploy time.
///
/// `last_fired_minute` is the epoch-minute stamp of the most recent firing. The
/// cron tick runs every second but a schedule matches to minute resolution, so
/// we only fire when the stamp changes — otherwise a `* * * * *` job would fire
/// sixty times a minute.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ScheduledJob {
    pub(super) name: String,
    pub(super) namespace: String,
    pub(super) schedule: crate::meat::cron::CronSchedule,
    pub(super) spec: JobSpec,
    pub(super) last_fired_minute: Option<i64>,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Keep job evidence durable before runtime mutation or retry admission.
    pub(super) async fn commit_jobs(
        &mut self,
        next: BTreeMap<String, crate::bun::jobs::RecordedJob>,
    ) -> Result<(), BunError> {
        self.commit_job_inventory(crate::bun::jobs::JobInventory {
            jobs: next,
            retired: self.retired_batch_executions.clone(),
        })
        .await
    }

    pub(super) async fn commit_job_inventory(
        &mut self,
        next: crate::bun::jobs::JobInventory,
    ) -> Result<(), BunError> {
        // A no-op retirement of an unrelated app changes no job ownership.
        if next.jobs == self.recorded_jobs && next.retired == self.retired_batch_executions {
            return Ok(());
        }
        if self.job_store_uncertain {
            return Err(BunError::JobState(
                "a previous write is uncertain; restart Bun to reload it".into(),
            ));
        }
        let new_admission = next.jobs.iter().any(|(id, job)| {
            self.recorded_jobs
                .get(id)
                .is_none_or(|previous| previous.generation != job.generation)
        });
        if new_admission {
            self.preflight_job_inventory(next.clone()).await?;
        }
        if let Some(directory) = self.records_dir.clone() {
            self.job_store_uncertain = true;
            // New identities are fenced even when publication has an uncertain outcome.
            // A failed retirement move keeps the active owner until recovery.
            for (id, job) in &next.jobs {
                self.recorded_jobs
                    .entry(id.clone())
                    .or_insert_with(|| job.clone());
            }
            let records = next.clone();
            #[cfg(test)]
            self.loop_stalls.hold(LoopStall::Persist).await;
            let publication = tokio::task::spawn_blocking(move || {
                crate::bun::jobs::persist_inventory(&directory, records)
            });
            // LOOP-INLINE: detached checkpoint IO has a two-second bound and retains the ownership fence on timeout.
            tokio::time::timeout(std::time::Duration::from_secs(2), publication).await
                .map_err(|_| BunError::JobState("job checkpoint publication timed out; execution ownership remains uncertain".into()))?
                .map_err(|error| BunError::JobState(error.to_string()))?
                .map_err(|error| BunError::JobState(format!("job checkpoint publication is uncertain: {error}")))?;
        }
        self.recorded_jobs = next.jobs;
        self.retired_batch_executions = next.retired;
        self.job_store_uncertain = false;
        Ok(())
    }

    pub(super) async fn record_observed_job_exit(
        &mut self,
        id: &InstanceId,
        phase: crate::bun::jobs::JobPhase,
    ) -> Result<(), BunError> {
        let mut next = self.recorded_jobs.clone();
        let job = next
            .get_mut(&id.0)
            .ok_or_else(|| BunError::JobState(format!("missing attempt for {id}")))?;
        job.observe_phase(phase);
        // A short process can exit before a PID adoption record is available.
        // Persist its positive exit observation with the outcome, rather than
        // asking a replacement ProcessGrill to signal an unadoptable handle.
        // OCI runtimes retain named container resources after process exit.
        if job.runtime == crate::grill::records::RuntimeKind::Process {
            job.runtime_absent = true;
        }
        self.commit_jobs(next).await
    }

    pub(super) async fn record_job_runtime_absent(
        &mut self,
        id: &InstanceId,
    ) -> Result<(), BunError> {
        let mut jobs = self.recorded_jobs.clone();
        if let Some(job) = jobs.get_mut(&id.0) {
            job.runtime_absent = if job.batch_execution.is_some()
                && job.runtime != crate::grill::records::RuntimeKind::Process
            {
                // A stopped OCI process can still own a named container.
                // Refuse compact retirement unless inspection proves it absent.
                // LOOP-INLINE: one-second read-only inspection bounds positive OCI absence evidence.
                matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(1),
                        self.supervisor.grill().state(id),
                    )
                    .await,
                    Ok(Err(crate::grill::GrillError::NotFound { .. }))
                )
            } else {
                true
            };
        }
        self.commit_jobs(jobs).await
    }

    /// A new run may replace terminal evidence only after old runtime cleanup.
    pub(super) async fn prepare_job_run(
        &mut self,
        name: &str,
        namespace: &str,
        spec: &JobSpec,
        rerun_unknown: bool,
    ) -> Result<Vec<InstanceId>, BunError> {
        use crate::bun::jobs::{JobPhase, RecordedJob};
        let id = crate::grill::InstanceIdentity::new(namespace, name, 0).instance_id();
        let refuse = |reason: &str| BunError::JobState(format!("{namespace}/{name}: {reason}"));
        if self.job_store_uncertain {
            return Err(refuse("checkpoint is uncertain; restart Bun"));
        }
        // An earlier attempt of this apply may already be clearing the
        // previous run off the loop, and marked it Stopping to do so.
        let clearing = self.off_loop_work.started(
            &off_loop_work::WorkKey::ClearJobRun(id.clone()),
            self.incarnation_of(&id),
        );
        if let Some(instance) = self.supervisor.get_instance(&id) {
            if !instance.is_job || instance.app_name != name || instance.namespace != namespace {
                return Err(refuse("instance id belongs to another workload"));
            }
            if !rerun_unknown
                && !clearing
                && !matches!(
                    instance.state,
                    ContainerState::Stopped | ContainerState::Failed
                )
            {
                return Err(refuse(
                    "previous job still owns its runtime; stop it before applying again",
                ));
            }
        }
        let previous = self.recorded_jobs.get(&id.0).cloned();
        let next_cron_occurrence = self
            .scheduled_jobs
            .contains_key(&(name.into(), namespace.into()))
            && previous.as_ref().is_some_and(|job| job.runtime_absent);
        if previous.as_ref().is_some_and(|job| {
            matches!(
                job.phase,
                JobPhase::Unknown | JobPhase::Preparing | JobPhase::Launching
            )
        }) && !rerun_unknown
            && !next_cron_occurrence
        {
            return Err(refuse(
                "previous outcome is unknown; use apply --rerun-jobs for an explicit rerun",
            ));
        }
        let generation = match &previous {
            Some(job) => job
                .generation
                .checked_add(1)
                .ok_or_else(|| refuse("job generation exhausted"))?,
            None => 1,
        };
        if let Some(job) = &previous {
            if !job.runtime_absent {
                self.clear_previous_job_run(&id).await?;
            }
            self.record_job_runtime_absent(&id).await?;
            self.retire_instance_artifacts(&id).await?;
            // LOOP-INLINE: in-memory lock, no I/O
            self.supervisor.retire_instance(&id).await;
        }
        // LOOP-INLINE: in-memory lock, no I/O
        let ids = self
            .supervisor
            .deploy_job(name, namespace, spec, Instant::now())
            .await?;
        let mut next = self.recorded_jobs.clone();
        next.insert(
            id.0.clone(),
            RecordedJob {
                name: name.into(),
                namespace: namespace.into(),
                spec: spec.clone(),
                runtime: self.supervisor.grill().runtime_kind(),
                generation,
                restart_count: 0,
                phase: JobPhase::Preparing,
                runtime_absent: false,
                batch_execution: None,
            },
        );
        self.commit_jobs(next).await?;
        Ok(ids)
    }

    /// Retrying spends the budget before create/start, after retiring the old record.
    pub(super) async fn claim_job_retry(&mut self, id: &InstanceId) -> Result<(), BunError> {
        use crate::bun::jobs::{JobPhase, MAX_RETRIES};
        let count = self
            .supervisor
            .get_instance(id)
            .ok_or_else(|| BunError::InstanceNotFound {
                instance_id: id.clone(),
            })?
            .restart_count;
        let mut next = self.recorded_jobs.clone();
        let job = next
            .get_mut(&id.0)
            .ok_or_else(|| BunError::JobState(format!("missing attempt for {id}")))?;
        if !job.spec.run_before.is_empty()
            || count > MAX_RETRIES
            || count < job.restart_count
            || matches!(
                job.phase,
                JobPhase::Unknown
                    | JobPhase::Stopping
                    | JobPhase::Stopped
                    | JobPhase::Exited { code: 0 }
            )
        {
            return Err(BunError::JobState(format!(
                "automatic retry refused for {id}"
            )));
        }
        job.restart_count = count;
        if let Some(owner) = &mut job.batch_execution {
            owner.observed_exit_code = None;
            owner.observed_restart_count = None;
        }
        job.phase = JobPhase::Preparing;
        job.runtime_absent = false;
        self.commit_jobs(next).await
    }

    pub(super) fn job_state_label(&self, instance: &WorkloadInstance) -> String {
        if (instance.is_job && self.job_store_uncertain)
            || self
                .recorded_jobs
                .get(&instance.id.0)
                .is_some_and(|job| job.phase == crate::bun::jobs::JobPhase::Unknown)
        {
            "unknown".into()
        } else {
            instance.state.to_string()
        }
    }

    /// Post-start bookkeeping for a job instance (the loop side of the former
    /// `drive_job_startup`): store the OCI spec, log forwarder, on-disk
    /// record, and transitions to Running.
    pub(super) async fn finish_job_instance(
        &mut self,
        instance_id: &InstanceId,
        job_name: &str,
        namespace: &str,
        oci_spec: crate::grill::oci::OciSpec,
        evidence: &launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.oci_spec = Some(oci_spec);
        }
        self.spawn_log_forwarder(instance_id, job_name, namespace);
        self.persist_instance_record(instance_id, evidence).await?;
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::HealthWait)?;
            instance.state = instance.state.transition_to(ContainerState::Running)?;
        }
        Ok(())
    }

    /// Replace the schedule inventory only after its checkpoint is durable.
    pub(super) async fn commit_scheduled_jobs(
        &mut self,
        next: std::collections::HashMap<(String, String), ScheduledJob>,
    ) -> Result<(), BunError> {
        if self.scheduled_jobs_store_uncertain {
            return Err(BunError::ScheduleState(
                "a previous write is uncertain; restart Bun to reload the checkpoint".into(),
            ));
        }
        if next == self.scheduled_jobs {
            return Ok(());
        }
        if let Some(directory) = self.records_dir.clone() {
            let records = next
                .values()
                .map(|job| crate::bun::schedules::RecordedSchedule {
                    name: job.name.clone(),
                    namespace: job.namespace.clone(),
                    spec: job.spec.clone(),
                    last_fired_minute: job.last_fired_minute,
                })
                .collect();
            // spawn_blocking can finish after its caller is cancelled. Fence
            // scheduling before the await until memory and disk agree again.
            self.scheduled_jobs_store_uncertain = true;
            // Keep both old and proposed owners reachable if writing fails or
            // is cancelled. Retirement must not mistake either set for absent.
            for (key, job) in &next {
                self.scheduled_jobs
                    .entry(key.clone())
                    .or_insert_with(|| job.clone());
            }
            #[cfg(test)]
            self.loop_stalls.hold(LoopStall::Persist).await;
            // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
            tokio::task::spawn_blocking(move || {
                crate::bun::schedules::persist(&directory, records)
            })
            .await
            .map_err(|error| BunError::ScheduleState(error.to_string()))?
            .map_err(|error| BunError::ScheduleState(error.to_string()))?;
        }
        self.scheduled_jobs = next;
        self.scheduled_jobs_store_uncertain = false;
        Ok(())
    }

    /// Persist registrations and retire schedules removed by an explicit apply.
    pub(super) async fn register_scheduled_jobs(
        &mut self,
        config: &Config,
    ) -> Result<(), BunError> {
        if config.job.is_empty() {
            return Ok(());
        }
        let mut next = self.scheduled_jobs.clone();
        for (name, spec) in &config.job {
            let namespace = spec
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let key = (name.clone(), namespace.clone());
            let Some(expression) = spec.schedule.as_deref() else {
                next.remove(&key);
                continue;
            };
            let schedule = crate::meat::cron::CronSchedule::parse(expression)
                .map_err(|error| BunError::ScheduleState(error.to_string()))?;
            let last_fired_minute = next
                .get(&key)
                .and_then(|existing| existing.last_fired_minute);
            next.insert(
                key,
                ScheduledJob {
                    name: name.clone(),
                    namespace,
                    schedule,
                    spec: spec.clone(),
                    last_fired_minute,
                },
            );
        }
        self.commit_scheduled_jobs(next).await
    }

    /// Fire every scheduled job whose cron matches the current UTC minute.
    ///
    /// Called on the 1s event-loop tick, but a schedule only resolves to the
    /// minute, so each job fires at most once per matching minute (guarded by
    /// its epoch-minute stamp). Firing reuses the normal job deploy path with
    /// the `schedule` cleared, so the job actually runs this time.
    pub(super) async fn fire_due_jobs(&mut self) {
        if self.scheduled_jobs.is_empty() || self.scheduled_jobs_store_uncertain {
            return;
        }
        let now = time::OffsetDateTime::now_utc();
        let minute_stamp = now.unix_timestamp().div_euclid(60);

        let mut due: Vec<(String, String, JobSpec)> = Vec::new();
        let mut next = self.scheduled_jobs.clone();
        // LOOP-INLINE: in-memory lock, no I/O
        let active = self.deploy_operations.snapshot().await.active_deploys;
        for job in next.values_mut() {
            if active.iter().any(|operation| {
                operation
                    .targets
                    .iter()
                    .any(|target| target.name == job.name && target.namespace == job.namespace)
            }) {
                continue;
            }
            if job
                .last_fired_minute
                .is_some_and(|previous| previous >= minute_stamp)
            {
                continue;
            }
            if job.schedule.matches(now) {
                job.last_fired_minute = Some(minute_stamp);
                let mut spec = job.spec.clone();
                spec.schedule = None;
                due.push((job.name.clone(), job.namespace.clone(), spec));
            }
        }

        if due.is_empty() {
            return;
        }
        if let Err(error) = self.commit_scheduled_jobs(next).await {
            eprintln!("cron: firing refused: {error}");
            return;
        }
        for (name, namespace, spec) in due {
            self.record_event(
                crate::bun::events::EventKind::Deploy,
                crate::bun::events::EventSeverity::Info,
                Some(name.clone()),
                Some(namespace.clone()),
                format!("firing scheduled job {namespace}/{name}"),
            )
            .await;

            let mut config = Config::default();
            config.job.insert(name, spec);
            self.spawn_scheduled_job_deploy(config).await;
        }
    }

    /// Admit a cron firing without changing the registered schedule.
    pub(super) async fn spawn_scheduled_job_deploy(&mut self, config: Config) {
        let (events_tx, mut events_rx) = mpsc::channel::<ApplyEvent>(64);
        tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
        self.begin_deploy(config, events_tx, false, false).await;
    }

    /// A running job's process has exited. Record its outcome; on failure,
    /// attempt a restart or mark it Failed if the retry limit is exhausted.
    pub(super) async fn observe_job_exit(&mut self, id: &InstanceId, exit_code: Option<i32>) {
        let launching = self
            .recorded_jobs
            .get(&id.0)
            .is_some_and(|job| job.phase == crate::bun::jobs::JobPhase::Launching);
        if !launching {
            return;
        }
        let phase = match exit_code {
            Some(code) => crate::bun::jobs::JobPhase::Exited { code },
            None => crate::bun::jobs::JobPhase::Unknown,
        };
        if let Err(error) = self.record_observed_job_exit(id, phase).await {
            eprintln!("bun: job outcome retained as uncertain for {id}: {error}");
            return;
        }

        // Transition Running → Stopping → Stopped
        if let Some(instance) = self.supervisor.get_instance_mut(id) {
            instance.retry_pending = exit_code.is_some_and(|code| code != 0)
                && self
                    .recorded_jobs
                    .get(&id.0)
                    .is_some_and(|job| job.spec.run_before.is_empty());
            if let Ok(s) = instance.state.transition_to(ContainerState::Stopping) {
                instance.state = s;
            }
            if let Ok(s) = instance.state.transition_to(ContainerState::Stopped) {
                instance.state = s;
            }
        }

        if exit_code.is_none() {
            self.record_event(
                crate::bun::events::EventKind::JobFailed,
                crate::bun::events::EventSeverity::Warning,
                None,
                None,
                format!("job {id} outcome unknown; explicit rerun required"),
            )
            .await;
            return;
        }
        if exit_code == Some(0) {
            // Job completed successfully — stays in Stopped
            if let Some(instance) = self.supervisor.get_instance(id) {
                self.record_event(
                    crate::bun::events::EventKind::JobCompleted,
                    crate::bun::events::EventSeverity::Info,
                    Some(instance.app_name.clone()),
                    Some(instance.namespace.clone()),
                    format!("job {} completed", instance.app_name),
                )
                .await;
            }
            return;
        }

        // A failed migration is terminal for its apply. Only another explicit
        // apply may start a new generation after confirmed cleanup.
        if self
            .recorded_jobs
            .get(&id.0)
            .is_some_and(|job| !job.spec.run_before.is_empty())
        {
            return;
        }

        // Job failed — attempt restart
        // LOOP-INLINE: in-memory lock, no I/O
        match self.supervisor.maybe_restart(id, Instant::now()).await {
            Ok(true) => {
                // Now in Pending — drive_pending_restarts will handle it
                if let Some(instance) = self.supervisor.get_instance(id) {
                    self.record_event(
                        crate::bun::events::EventKind::Restart,
                        crate::bun::events::EventSeverity::Warning,
                        Some(instance.app_name.clone()),
                        Some(instance.namespace.clone()),
                        format!(
                            "instance {} restarted (attempt {})",
                            id.0, instance.restart_count
                        ),
                    )
                    .await;
                }
            }
            Ok(false) => {
                // Backoff not elapsed — will retry on next tick
            }
            Err(_) => {
                // Exceeded restart limit — mark as Failed
                if let Some(instance) = self.supervisor.get_instance_mut(id)
                    && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
                {
                    instance.state = s;
                    instance.retry_pending = false;
                }
                if let Some(instance) = self.supervisor.get_instance(id) {
                    self.record_event(
                        crate::bun::events::EventKind::JobFailed,
                        crate::bun::events::EventSeverity::Warning,
                        Some(instance.app_name.clone()),
                        Some(instance.namespace.clone()),
                        format!("job {} failed", instance.app_name),
                    )
                    .await;
                }
            }
        }
    }

    /// The same for every running job.
    #[cfg(test)]
    pub(super) async fn check_jobs(&mut self) {
        let reads = self.plan_state_reads(|instance| instance.is_job);
        let grill = self.supervisor.grill().clone();
        let sweep = state_sweep::sweep_states(grill, reads).await;
        self.apply_state_sweep(Ok(sweep)).await;
    }

    /// Kill a job's previous run before a rerun replaces it. The first
    /// attempt fences the instance (Stopping, no retries, no probes), so
    /// neither the health tick nor a restart touches it while the kill runs
    /// off the loop; the deploy worker asks again until it's confirmed.
    pub(super) async fn clear_previous_job_run(&mut self, id: &InstanceId) -> Result<(), BunError> {
        let key = off_loop_work::WorkKey::ClearJobRun(id.clone());
        if !self.off_loop_work.started(&key, self.incarnation_of(id)) {
            if let Some(instance) = self.supervisor.get_instance_mut(id) {
                instance.retry_pending = false;
                if instance.state.can_transition_to(ContainerState::Stopping) {
                    instance.state = ContainerState::Stopping;
                }
            }
            self.supervisor.health_checker_mut().unregister(id);
        }
        self.kill_off_the_loop(key, id).await?.map_err(|reason| {
            BunError::JobState(format!("{id}: previous run not cleared: {reason}"))
        })
    }
}
