//! Retiring instances and workloads: killing off the loop, the artifacts
//! an instance leaves behind, and the final shutdown.

use super::*;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Retire a workload inline: the same steps a `Retire` command takes, for
    /// tests that drive the agent without running its loop.
    #[cfg(test)]
    pub(super) async fn retire_workload(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        self.refuse_while_deploying(app_name, namespace).await?;
        match self.stop_app(app_name, namespace).await {
            Ok(()) | Err(BunError::AppNotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        self.release_retired_workload(app_name, namespace).await
    }

    /// Forget a workload's ownership once its stop has confirmed every exit.
    pub(super) async fn release_retired_workload(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| instance.app_name == app_name && instance.namespace == namespace)
            .map(|instance| instance.id.clone())
            .collect();
        for id in instances {
            // LOOP-INLINE: in-memory lock, no I/O
            self.supervisor.retire_instance(&id).await;
        }
        self.deployed_specs
            .remove(&(app_name.to_string(), namespace.to_string()));
        let mut inventory = crate::bun::jobs::JobInventory {
            jobs: self.recorded_jobs.clone(),
            retired: self.retired_batch_executions.clone(),
        };
        let retiring: Vec<_> = inventory
            .jobs
            .iter()
            .filter(|(_, job)| job.name == app_name && job.namespace == namespace)
            .map(|(id, job)| (id.clone(), job.clone()))
            .collect();
        for (id, job) in retiring {
            if let Some(owner) = &job.batch_execution {
                if !job.runtime_absent {
                    return Err(BunError::JobState(
                        "batch execution retirement lacks positive runtime absence".into(),
                    ));
                }
                // Only current-attempt terminal evidence becomes a terminal proof.
                // An interrupted retry or explicit unknown stop remains unknown.
                let phase = job
                    .batch_terminal_exit()
                    .map_or(crate::bun::jobs::JobPhase::Unknown, |code| {
                        crate::bun::jobs::JobPhase::Exited { code }
                    });
                inventory.retired.insert(
                    id.clone(),
                    crate::bun::jobs::RetiredBatchExecution {
                        name: job.name.clone(),
                        namespace: job.namespace.clone(),
                        generation: job.generation,
                        restart_count: job.restart_count,
                        batch_execution: owner.clone(),
                        runtime_absent: true,
                        phase,
                    },
                );
            }
            inventory.jobs.remove(&id);
        }
        self.commit_job_inventory(inventory).await?;
        Ok(())
    }

    /// Managed storage retirement only ever touches an owned test namespace.
    pub(super) fn require_test_namespace(app_name: &str, namespace: &str) -> Result<(), BunError> {
        if crate::testkit::lease::valid_test_namespace(namespace) {
            return Ok(());
        }
        Err(BunError::RetirementState {
            instance_id: InstanceId(format!("{namespace}/{app_name}")),
            reason: "managed storage retirement requires an owned test namespace".into(),
        })
    }

    /// Remove a retired lease's disposable managed storage.
    /// The removal runs in a task ([`off_loop_work`]); `StillRunning` means ask
    /// again.
    pub(super) async fn retire_test_storage(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let manager = crate::grill::volume::VolumeManager::new(self.volumes_dir.clone());
        let key = off_loop_work::WorkKey::RetireTestStorage {
            namespace: namespace.to_string(),
            app: app_name.to_string(),
        };
        let (namespace, app) = (namespace.to_string(), app_name.to_string());
        let removal = async move {
            tokio::task::spawn_blocking(move || manager.retire_test_storage(&namespace, &app))
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, None, removal)
            .await?
            .map_err(|reason| BunError::DeployFailed {
                app_name: app_name.into(),
                reason,
            })
    }

    /// Remove predecessor execution/policy evidence before an automatic restart.
    /// Runtime retirement must already be confirmed. The same logical workload
    /// keeps its identity bundle and mount; final retirement removes those too.
    pub(super) async fn retire_restart_artifacts(
        &mut self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        let remote = self.confirm_producer_release(instance_id).await?;
        self.clear_egress(instance_id).await?;
        self.release_network_reference(instance_id, remote.as_ref())
            .await?;
        if let Some(directory) = self.records_dir.clone() {
            let id = instance_id.0.clone();
            // LOOP-INLINE: fsync'd persist (#351 decision 2); the slow-disk scenario bounds it
            tokio::task::spawn_blocking(move || {
                crate::grill::records::remove_record(&directory, &id)
            })
            .await
            .map_err(|error| BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason: error.to_string(),
            })?
            .map_err(|error| BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason: error.to_string(),
            })?;
        }
        self.forget_retired_egress_owner(instance_id).await?;
        // Validate the mount source before a new runtime can consume it. The
        // preparation is idempotent and preserves this workload's credentials.
        self.prepare_instance_identity(instance_id)
    }

    pub(super) async fn retire_initialisers(
        &mut self,
        parent: &InstanceId,
    ) -> Result<(), BunError> {
        let children = self.initialisers.get(parent).cloned().unwrap_or_default();
        // An initialiser has normally exited long before its parent retires,
        // so confirming that is quick. One the runtime can't confirm within
        // the turn fails the retirement, which retries; the kill is
        // idempotent.
        let deadline = self.turn_deadline();
        for child in children {
            tokio::time::timeout_at(
                deadline,
                kill_runtime_instance(
                    self.supervisor.grill(),
                    &child,
                    self.stop_confirmation_timeout,
                ),
            )
            .await
            .map_err(|_| BunError::StopUnconfirmed {
                instance_id: child.clone(),
                reason: "initialiser exit was not confirmed within the turn",
            })??;
            if let Some(remaining) = self.initialisers.get_mut(parent) {
                remaining.remove(&child);
            }
        }
        self.initialisers.remove(parent);
        Ok(())
    }

    /// Retire durable artifacts before allowing the caller to forget an owner.
    ///
    /// The identity directory and adoption record go last, from a task
    /// ([`off_loop_work`]): `Err(BunError::StillRunning)` means that removal
    /// hasn't finished within the turn, and asking again picks it up where
    /// it is without repeating the steps before it.
    pub(super) async fn retire_instance_artifacts(
        &mut self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        let key = off_loop_work::WorkKey::RetireArtifacts(instance_id.clone());
        let incarnation = self.incarnation_of(instance_id);
        if !self.off_loop_work.started(&key, incarnation) {
            self.retire_instance_artifacts_up_to_disk(instance_id)
                .await?;
        }
        let identity_dir = self.instance_identity_dir(instance_id);
        let records_dir = self.records_dir.clone();
        let id = instance_id.0.clone();
        #[cfg(test)]
        let stalls = Arc::clone(&self.loop_stalls);
        let cleanup = async move {
            #[cfg(test)]
            stalls.hold(LoopStall::ArtifactCleanup).await;
            tokio::task::spawn_blocking(move || {
                crate::sesame::identity::cleanup_identity_dir(&identity_dir)?;
                if let Some(directory) = records_dir {
                    crate::grill::records::remove_record(&directory, &id)?;
                }
                Ok::<(), std::io::Error>(())
            })
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, incarnation, cleanup)
            .await?
            .map_err(|reason| BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason,
            })?;
        self.forget_retired_egress_owner(instance_id).await?;
        self.launch_executions.remove(instance_id);
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.identity = None;
            instance.identity_mount = None;
        }
        Ok(())
    }

    /// Retirement up to the disk cleanup: initialisers, routing, producer
    /// release, egress and the network reference.
    pub(super) async fn retire_instance_artifacts_up_to_disk(
        &mut self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        self.retire_initialisers(instance_id).await?;
        if !self
            .poll_instance_withdrawal(instance_id, std::time::Duration::ZERO)
            .await?
        {
            return Err(BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason: "captured ingress requests still require confirmed release".into(),
            });
        }
        let remote = self.confirm_producer_release(instance_id).await?;
        self.clear_egress(instance_id).await?;
        self.release_network_reference(instance_id, remote.as_ref())
            .await
    }

    /// Retire an instance's artifacts outside the loop (startup adoption),
    /// where nothing else is waiting, so a slow disk is simply waited out.
    pub(super) async fn retire_instance_artifacts_fully(
        &mut self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        loop {
            match self.retire_instance_artifacts(instance_id).await {
                Err(BunError::StillRunning { .. }) => continue,
                result => return result,
            }
        }
    }

    /// Force-kill `id` and confirm its exit from a task, as `key`'s work
    /// (#351, stage 3). A restart in flight gives the instance up first, and
    /// the task lets its runtime step finish before it signals anything.
    /// Ownership stays put until the kill is confirmed: the outer `Err` is
    /// `StillRunning` while it isn't yet, and the inner one says why the
    /// runtime didn't confirm it.
    pub(super) async fn kill_off_the_loop(
        &mut self,
        key: off_loop_work::WorkKey,
        id: &InstanceId,
    ) -> Result<Result<(), String>, BunError> {
        let incarnation = self.incarnation_of(id);
        let taken = if self.off_loop_work.started(&key, incarnation) {
            None
        } else {
            self.take_back_from_restart(id)
        };
        let runtime_absent = self
            .recorded_jobs
            .get(&id.0)
            .is_some_and(|job| job.runtime_absent);
        let grill = self.supervisor.grill().clone();
        let confirmation = self.stop_confirmation_timeout;
        let target = id.clone();
        let kill = async move {
            if let Some(restart) = taken {
                restart
                    .settle(confirmation)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            if runtime_absent {
                return Ok(());
            }
            kill_runtime_instance(&grill, &target, confirmation)
                .await
                .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, incarnation, kill).await
    }

    /// Kill `id` off the loop and wait until it's confirmed, the way a
    /// caller that keeps asking would. For tests that drive the agent
    /// without its loop.
    #[cfg(test)]
    pub(super) async fn kill_and_wait_for_exit(&mut self, id: &InstanceId) -> Result<(), BunError> {
        loop {
            match self
                .kill_off_the_loop(off_loop_work::WorkKey::ClearJobRun(id.clone()), id)
                .await
            {
                Err(BunError::StillRunning { .. }) => tokio::task::yield_now().await,
                Err(error) => return Err(error),
                Ok(result) => return result.map_err(|reason| BunError::StopIncomplete { reason }),
            }
        }
    }

    /// Withdraw traffic before fencing supervision and permitting an off-loop stop.
    pub(super) async fn begin_instance_retirement(
        &mut self,
        id: &InstanceId,
    ) -> Result<Option<restarts::TakenRestart>, BunError> {
        if self.supervisor.get_instance(id).is_none() {
            return Err(BunError::InstanceNotFound {
                instance_id: id.clone(),
            });
        }
        self.withdraw_instance_backend(id).await?;
        if let Some(instance) = self.supervisor.get_instance_mut(id) {
            instance.retry_pending = false;
            if instance.state.can_transition_to(ContainerState::Stopping) {
                instance.state = ContainerState::Stopping;
            }
        }
        self.supervisor.health_checker_mut().unregister(id);
        Ok(self.take_back_from_restart(id))
    }

    /// Drain, stop and forget one old instance (M7).
    ///
    /// The fast `&mut self` bookkeeping half of retiring one old instance: the
    /// worker has already drained and stopped it off the command loop (M7), so
    /// this only lifts egress, cleans identity, and drops the record and
    /// supervisor entry. Interleaving it with replacement is what gives
    /// `max_surge` and `max_unavailable` their meaning.
    pub(super) async fn finish_retire_bookkeeping(
        &mut self,
        old_id: &InstanceId,
    ) -> Result<(), BunError> {
        // The worker already observed exit. Preserve a stopped cleanup owner,
        // so a filesystem failure cannot make the restart driver revive it.
        self.retain_stopped_instance(old_id);
        self.withdraw_instance_backend(old_id).await?;
        self.retire_instance_artifacts(old_id).await?;
        // LOOP-INLINE: in-memory lock, no I/O
        self.supervisor.retire_instance(old_id).await;
        self.sync_firewall_ebpf().await;
        self.rebuild_routing_table().await;
        Ok(())
    }

    /// Retain cleanup ownership after observed runtime exit without restarting it.
    pub(super) fn retain_stopped_instance(&mut self, old_id: &InstanceId) {
        if let Some(instance) = self.supervisor.get_instance_mut(old_id) {
            instance.state = ContainerState::Stopped;
            instance.retry_pending = false;
        }
        self.supervisor.health_checker_mut().unregister(old_id);
    }

    /// Gracefully stop all instances.
    pub(super) async fn shutdown_all(&mut self) {
        // Reverse every owned fault before the process goes away. The
        // node-pressure helper also has PR_SET_PDEATHSIG and startup sweeping
        // for crash recovery, but graceful shutdown should leave no helper or
        // cgroup behind in the first place.
        let faults = self.fault_registry.clear();
        for rule in &faults {
            self.reverse_fault(rule).await;
        }
        // A pressure helper stops in a task; this last turn waits for every
        // one, a start still in flight included, for a few seconds at most.
        let pressure = Arc::clone(&self.node_pressure);
        // LOOP-INLINE: shutdown's last turn; no caller waits for another
        let _ = tokio::time::timeout(SHUTDOWN_PRESSURE_CLEAR, async move {
            pressure.lock().await.clear_all().await;
        })
        .await;
        self.reconcile_network_faults().await;
        self.publish_dns_faults();

        let mut ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .map(|i| i.id.clone())
            .collect();

        ids.extend(
            self.initialisers
                .values()
                .flat_map(|children| children.iter().cloned()),
        );

        // A restart step still creating or starting a container would race
        // the signals below. Take every instance back and let those finish.
        for restart in self.take_back_all_restarts() {
            // LOOP-INLINE: shutdown's last turn; settle carries its own deadline
            if let Err(error) = restart.settle(self.stop_confirmation_timeout).await {
                eprintln!("bun: shutting down despite a restart in flight: {error}");
            }
        }

        // Ask everything to stop (SIGTERM), wait (up to a grace period, but no
        // longer than needed) for it to exit, then force-kill (SIGKILL) whatever
        // is still running so nothing is orphaned.
        for id in &ids {
            // LOOP-INLINE: shutdown's last turn; nothing is queued behind it
            let _ = self.supervisor.grill().stop(id).await;
        }
        let deadline = Instant::now() + self.shutdown_grace;
        loop {
            let mut all_stopped = true;
            for id in &ids {
                if !matches!(
                    self.supervisor.grill().state(id).await,
                    Ok(ContainerState::Stopped)
                ) {
                    all_stopped = false;
                    break;
                }
            }
            if all_stopped || Instant::now() >= deadline {
                break;
            }
            // LOOP-INLINE: shutdown's last turn; nothing is queued behind it
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        for id in &ids {
            if !matches!(
                self.supervisor.grill().state(id).await,
                Ok(ContainerState::Stopped)
            ) {
                // LOOP-INLINE: shutdown's last turn; nothing is queued behind it
                let _ = self.supervisor.grill().kill(id).await;
            }
        }
    }
}
