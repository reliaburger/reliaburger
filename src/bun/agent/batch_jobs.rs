//! Trusted batch admission: one durable inventory publication before any launch.

use super::*;
use crate::bun::batch::BatchExecutionLabel;
use crate::bun::jobs::{BatchExecutionOwnership, JobInventory, JobPhase, RecordedJob};

impl<G: Grill + Clone + 'static> BunAgent<G> {
    pub(super) fn batch_owned_identities(
        &self,
        identities: &[(String, String)],
    ) -> std::collections::BTreeSet<String> {
        identities
            .iter()
            .filter_map(|(name, namespace)| {
                let id = crate::grill::InstanceIdentity::new(namespace, name, 0)
                    .instance_id()
                    .0;
                (self
                    .recorded_jobs
                    .get(&id)
                    .is_some_and(|job| job.batch_execution.is_some())
                    || self.retired_batch_executions.contains_key(&id))
                .then_some(id)
            })
            .collect()
    }

    pub(super) fn logical_execution_name(&self, id: &InstanceId, fallback: &str) -> String {
        if let Some(job) = self.recorded_jobs.get(&id.0) {
            return job.logical_name().to_string();
        }
        self.retired_batch_executions.get(&id.0).map_or_else(
            || fallback.to_string(),
            |proof| proof.batch_execution.logical_name.clone(),
        )
    }

    pub(super) fn resolve_execution_logs(
        &self,
        app: &str,
        namespace: &str,
        selected: Option<&str>,
    ) -> Result<LogExecutionSelection, BunError> {
        let aliases: BTreeMap<String, (String, String)> = self
            .recorded_jobs
            .iter()
            .filter_map(|(id, job)| {
                job.batch_execution
                    .as_ref()
                    .filter(|_| job.namespace == namespace)
                    .map(|owner| (id.clone(), (job.name.clone(), owner.logical_name.clone())))
            })
            .chain(
                self.retired_batch_executions
                    .iter()
                    .filter(|(_, proof)| proof.namespace == namespace)
                    .map(|(id, proof)| {
                        (
                            id.clone(),
                            (
                                proof.name.clone(),
                                proof.batch_execution.logical_name.clone(),
                            ),
                        )
                    }),
            )
            .collect();
        if let Some(instance) = selected {
            if let Some((execution, logical)) = aliases.get(instance) {
                if app != execution && app != logical {
                    return Err(BunError::BatchConflict(
                        "instance does not belong to the requested log path".into(),
                    ));
                }
                return Ok(LogExecutionSelection {
                    logical_name: logical.clone(),
                    instances: vec![instance.into()],
                    selected_instance: Some(instance.into()),
                });
            }
            let ordinary = self
                .supervisor
                .list_instances()
                .into_iter()
                .any(|i| i.id.0 == instance && i.namespace == namespace && i.app_name == app);
            if !ordinary {
                return Err(BunError::BatchConflict(
                    "instance does not belong to the requested namespace and workload".into(),
                ));
            }
            return Ok(LogExecutionSelection {
                logical_name: app.into(),
                instances: vec![instance.into()],
                selected_instance: Some(instance.into()),
            });
        }
        let direct = aliases.iter().find(|(_, (execution, _))| execution == app);
        let logical: Vec<String> = aliases
            .iter()
            .filter(|(_, (_, label))| label == app)
            .map(|(id, _)| id.clone())
            .collect();
        if let Some((id, (_, label))) = direct {
            if logical.iter().any(|logical_id| logical_id != id) {
                return Err(BunError::BatchConflict(
                    "ambiguous execution and logical label; select an explicit instance".into(),
                ));
            }
            return Ok(LogExecutionSelection {
                logical_name: label.clone(),
                instances: vec![id.clone()],
                selected_instance: Some(id.clone()),
            });
        }
        let mut instances: std::collections::BTreeSet<String> = logical.into_iter().collect();
        instances.extend(
            self.supervisor
                .list_instances()
                .into_iter()
                .filter(|i| i.namespace == namespace && i.app_name == app)
                .map(|i| i.id.0.clone()),
        );
        Ok(LogExecutionSelection {
            logical_name: app.into(),
            instances: instances.into_iter().collect(),
            selected_instance: None,
        })
    }

    /// Encode `inventory` once, refusing it before any fence if it would
    /// leave the checkpoint without room for its records' transitions.
    pub(super) async fn preflight_job_inventory(
        &self,
        inventory: JobInventory,
    ) -> Result<crate::bun::jobs::EncodedInventory, BunError> {
        #[cfg(test)]
        self.loop_stalls.hold(LoopStall::JobInventoryEncode).await;
        // Every recorded job has passed validation, so its digest is proven.
        let verified = self.recorded_jobs.clone();
        // LOOP-INLINE: off-thread inventory encoding has a two-second bound before any publication.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || crate::bun::jobs::preflight(inventory, &verified)),
        )
        .await
        .map_err(|_| {
            BunError::BatchCapacity("inventory preflight timed out before publication".into())
        })?
        .map_err(|error| BunError::BatchCapacity(error.to_string()))?
        .map_err(|error| BunError::BatchCapacity(error.to_string()))
    }

    /// Exact local replays do not spend another generation or spawn another worker.
    pub(super) async fn begin_owned_batch(
        &mut self,
        batch_id: u64,
        config: Config,
        labels: BTreeMap<String, BatchExecutionLabel>,
        events: mpsc::Sender<ApplyEvent>,
    ) -> Result<BTreeMap<String, i32>, BunError> {
        if self.job_store_uncertain
            || self.startup_cleanup_pending
            || self.draining.load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(BunError::JobState(
                "batch execution admission is uncertain or unavailable".into(),
            ));
        }
        config
            .validate_intrinsic()
            .map_err(|error| BunError::BatchConflict(error.to_string()))?;
        if batch_id == 0 || config.job.is_empty() || labels.len() != config.job.len() {
            return Err(BunError::BatchConflict(
                "invalid batch ownership map".into(),
            ));
        }
        let mut inventory = JobInventory {
            jobs: self.recorded_jobs.clone(),
            retired: self.retired_batch_executions.clone(),
        };
        let mut fresh = Config::default();
        let mut prepared = BTreeMap::new();
        let mut terminal = BTreeMap::new();
        for (name, spec) in &config.job {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            let label = labels
                .get(name)
                .ok_or_else(|| BunError::BatchConflict("missing batch label".into()))?;
            if label.namespace != namespace
                || !crate::config::valid_workload_label(&label.name)
                || spec.schedule.is_some()
                || !spec.run_before.is_empty()
            {
                return Err(BunError::BatchConflict(
                    "invalid batch label or unsupported declarative job field".into(),
                ));
            }
            let id = crate::grill::InstanceIdentity::new(namespace, name, 0).instance_id();
            let digest = crate::meat::batch_execution::spec_digest(namespace, &label.name, spec)
                .map_err(|error| BunError::BatchConflict(error.to_string()))?;
            let matches = |owner: &BatchExecutionOwnership| {
                owner.batch_id == batch_id
                    && owner.logical_name == label.name
                    && owner.spec_digest == digest
            };
            if let Some(proof) = inventory.retired.get(&id.0) {
                if !matches(&proof.batch_execution) {
                    return Err(BunError::BatchConflict(
                        "retired execution belongs to a different batch, label or spec".into(),
                    ));
                }
                let code = proof.terminal_exit().ok_or_else(|| {
                    BunError::JobState(
                        "retired execution has an unknown outcome; it cannot launch again".into(),
                    )
                })?;
                terminal.insert(name.clone(), code);
                continue;
            }
            if let Some(job) = inventory.jobs.get(&id.0) {
                let owner = job.batch_execution.as_ref().ok_or_else(|| {
                    BunError::BatchConflict("execution identity belongs to an ordinary job".into())
                })?;
                if !matches(owner) || &job.spec != spec {
                    return Err(BunError::BatchConflict(
                        "execution belongs to a different batch, label or spec".into(),
                    ));
                }
                if let Some(code) = job.batch_terminal_exit() {
                    terminal.insert(name.clone(), code);
                } else if matches!(
                    job.phase,
                    JobPhase::Unknown | JobPhase::Stopping | JobPhase::Stopped
                ) {
                    return Err(BunError::JobState(
                        "owned execution's original attempt has an unknown outcome".into(),
                    ));
                }
                continue;
            }
            if self
                .scheduled_jobs
                .contains_key(&(name.clone(), namespace.to_string()))
                || self
                    .deployed_specs
                    .contains_key(&(name.clone(), namespace.to_string()))
                || self.supervisor.get_instance(&id).is_some()
            {
                return Err(BunError::BatchConflict(
                    "execution identity already belongs to an ordinary workload".into(),
                ));
            }
            self.supervisor
                .admit_job(name, namespace, spec)
                .map_err(|error| BunError::BatchConflict(error.to_string()))?;
            inventory.jobs.insert(
                id.0.clone(),
                RecordedJob {
                    name: name.clone(),
                    namespace: namespace.into(),
                    spec: spec.clone(),
                    runtime: self.supervisor.grill().runtime_kind(),
                    generation: 1,
                    restart_count: 0,
                    phase: JobPhase::Preparing,
                    runtime_absent: false,
                    batch_execution: Some(BatchExecutionOwnership {
                        batch_id,
                        logical_name: label.name.clone(),
                        spec_digest: digest,
                        observed_exit_code: None,
                        observed_restart_count: None,
                    }),
                },
            );
            fresh.job.insert(name.clone(), spec.clone());
            prepared.insert(name.clone(), 1);
        }
        if fresh.job.is_empty() {
            return Ok(terminal);
        }
        let encoded = self.preflight_job_inventory(inventory).await?;
        // LOOP-INLINE: in-memory deployment ownership lock, no IO.
        let operation = self
            .deploy_operations
            .start(&fresh)
            .await
            .map_err(|error| BunError::BatchConflict(error.to_string()))?;
        if let Err(error) = self.commit_encoded_job_inventory(encoded).await {
            // LOOP-INLINE: in-memory operation completion, no IO.
            operation
                .finish(
                    crate::bun::deploy_operations::DeployOperationOutcome::Unknown,
                    error.to_string(),
                )
                .await;
            return Err(error);
        }
        for (name, spec) in &fresh.job {
            // LOOP-INLINE: in-memory supervisor admission, no runtime launch or IO.
            if let Err(error) = self
                .supervisor
                .deploy_job(
                    name,
                    spec.namespace.as_deref().unwrap_or("default"),
                    spec,
                    Instant::now(),
                )
                .await
            {
                let mut jobs = self.recorded_jobs.clone();
                for admitted in fresh.job.keys() {
                    let ns = fresh.job[admitted]
                        .namespace
                        .as_deref()
                        .unwrap_or("default");
                    let id = crate::grill::InstanceIdentity::new(ns, admitted, 0).instance_id();
                    if let Some(job) = jobs.get_mut(&id.0) {
                        job.phase = JobPhase::Unknown;
                    }
                }
                let _ = self.commit_jobs(jobs).await;
                // LOOP-INLINE: in-memory operation completion, no IO.
                operation
                    .finish(
                        crate::bun::deploy_operations::DeployOperationOutcome::Unknown,
                        error.to_string(),
                    )
                    .await;
                return Err(BunError::JobState(error.to_string()));
            }
        }
        let worker = DeployWorker {
            rerun_unknown_jobs: false,
            prepared_batch_jobs: Some(prepared),
            grill: self.supervisor.grill().clone(),
            ops: DeployOps {
                tx: self.deploy_ops_tx.clone(),
            },
            drains: self.drains.clone(),
            operation: Some(operation.clone()),
            stop_confirmation_timeout: self.stop_confirmation_timeout,
            egress: self.egress_resolver(),
        };
        let (forward, mut received) = mpsc::channel(64);
        let worker_task = tokio::spawn(async move {
            worker.run_deploy(fresh, forward).await;
        });
        tokio::spawn(async move {
            let mut outcome = crate::bun::deploy_operations::DeployOperationOutcome::Unknown;
            let mut message = "batch worker ended without completion".to_string();
            while let Some(event) = received.recv().await {
                match &event {
                    ApplyEvent::Complete { .. } => {
                        outcome = crate::bun::deploy_operations::DeployOperationOutcome::Completed;
                        message = "batch runtime launch completed".into();
                    }
                    ApplyEvent::Error { message: error } => {
                        outcome = crate::bun::deploy_operations::DeployOperationOutcome::Unknown;
                        message = error.clone();
                    }
                    _ => {}
                }
                let _ = events.send(event).await;
            }
            if worker_task.await.is_err() {
                outcome = crate::bun::deploy_operations::DeployOperationOutcome::Unknown;
            }
            operation.finish(outcome, message).await;
        });
        Ok(terminal)
    }
}
