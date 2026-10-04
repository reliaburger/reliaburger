//! Preparing and finishing instance launches on the loop: image trust,
//! secrets and identities, and the fresh and rolling instance steps a deploy
//! worker asks for.

use super::*;

/// A known terminal failure may release the replicated gate; uncertainty cannot.
#[derive(Debug)]
pub struct PrerequisiteFailure {
    pub message: String,
    pub settled: bool,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    pub(super) fn validate_deploy_names(&self, config: &Config) -> Result<(), String> {
        use crate::bun::deploy_operations::DeployTargetKind;
        config
            .validate_workload_names()
            .map_err(|error| error.to_string())?;
        for (name, namespace) in config
            .app
            .iter()
            .map(|(name, spec)| (name, spec.namespace.as_deref().unwrap_or("default")))
            .chain(
                config
                    .job
                    .iter()
                    .map(|(name, spec)| (name, spec.namespace.as_deref().unwrap_or("default"))),
            )
        {
            let id = crate::grill::InstanceIdentity::new(namespace, name, 0).instance_id();
            if self
                .recorded_jobs
                .get(&id.0)
                .is_some_and(|job| job.batch_execution.is_some())
                || self.retired_batch_executions.contains_key(&id.0)
            {
                return Err(format!(
                    "workload {namespace}/{name} belongs to a batch execution"
                ));
            }
        }
        for (name, spec) in &config.app {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            if self
                .scheduled_jobs
                .contains_key(&(name.clone(), namespace.to_string()))
            {
                return Err(format!(
                    "workload {namespace}/{name} belongs to a registered cron job; stop it before deploying an app with that name"
                ));
            }
            self.supervisor
                .admit_workload_kind(
                    name,
                    spec.namespace.as_deref().unwrap_or("default"),
                    DeployTargetKind::App,
                )
                .map_err(|error| error.to_string())?;
        }
        for (name, spec) in &config.job {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            if !spec.run_before.is_empty()
                && self
                    .scheduled_jobs
                    .contains_key(&(name.clone(), namespace.into()))
            {
                return Err(format!(
                    "workload {namespace}/{name} belongs to a registered cron job; stop it before running a migration"
                ));
            }
            self.supervisor
                .admit_job(name, namespace, spec)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Reserve all local targets and perform trust admission before Raft intent
    /// or any execution. Failure here can release ownership without uncertainty.
    pub(super) async fn prepare_prerequisites(
        &mut self,
        config: Config,
        response: oneshot::Sender<
            Result<(Config, crate::bun::deploy_operations::DeployOperationHandle), String>,
        >,
    ) {
        if self.startup_cleanup_pending
            || self.draining.load(std::sync::atomic::Ordering::Relaxed)
            || self.stopping_target(&config).is_some()
            || self.restoring_target(&config).is_some()
        {
            let _ = response.send(Err(
                "node cleanup, stop, restore or drain still owns a target".into(),
            ));
            return;
        }
        if let Err(error) = self.validate_deploy_names(&config) {
            let _ = response.send(Err(error));
            return;
        }
        // LOOP-INLINE: in-memory target ownership, no I/O
        let operation = match self.deploy_operations.start(&config).await {
            Ok(operation) => operation,
            Err(error) => {
                let _ = response.send(Err(error.to_string()));
                return;
            }
        };
        let worker = self.prerequisite_worker(operation.clone());
        tokio::spawn(async move {
            let mut config = config;
            // This shared preflight performs no runtime create or start.
            let result = worker.preflight_prerequisites(&mut config).await;
            if let Err(message) = result {
                operation
                    .finish(
                        crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                        message.clone(),
                    )
                    .await;
                let _ = response.send(Err(message));
            } else if response.send(Ok((config, operation.clone()))).is_err() {
                operation
                    .finish(
                        crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                        "prepared prerequisite receiver disappeared before dispatch",
                    )
                    .await;
            }
        });
    }

    fn prerequisite_worker(
        &self,
        operation: crate::bun::deploy_operations::DeployOperationHandle,
    ) -> DeployWorker<G> {
        DeployWorker {
            rerun_unknown_jobs: false,
            prepared_batch_jobs: None,
            grill: self.supervisor.grill().clone(),
            ops: DeployOps {
                tx: self.deploy_ops_tx.clone(),
            },
            drains: self.drains.clone(),
            operation: Some(operation),
            stop_confirmation_timeout: self.stop_confirmation_timeout,
            egress: self.egress_resolver(),
        }
    }

    /// Run only migration jobs; the API retains the operation through commit.
    pub(super) fn run_prepared_prerequisites(
        &self,
        config: Config,
        operation: crate::bun::deploy_operations::DeployOperationHandle,
        response: oneshot::Sender<Result<(), PrerequisiteFailure>>,
    ) {
        let worker = self.prerequisite_worker(operation.clone());
        tokio::spawn(async move {
            let result = async {
                for (name, spec) in config
                    .job
                    .iter()
                    .filter(|(_, job)| !job.run_before.is_empty())
                {
                    if operation.cancellation_requested() {
                        return Err(PrerequisiteFailure {
                            message: "prerequisite cancellation retains uncertain ownership".into(),
                            settled: false,
                        });
                    }
                    worker
                        .run_prerequisite_job(
                            name,
                            spec.namespace.as_deref().unwrap_or("default"),
                            spec,
                        )
                        .await
                        .map_err(|error| PrerequisiteFailure {
                            settled: matches!(error, BunError::PrerequisiteFailed { .. }),
                            message: error.to_string(),
                        })?;
                }
                // Match standalone deploy's boundary after migration settlement.
                // A positive exit does not erase an accepted cancellation.
                if operation.cancellation_requested() {
                    return Err(PrerequisiteFailure {
                        message:
                            "prerequisite cancelled before app publication; ownership remains held"
                                .into(),
                        settled: false,
                    });
                }
                Ok(())
            }
            .await;
            if response.send(result).is_err() {
                operation
                    .finish(
                        crate::bun::deploy_operations::DeployOperationOutcome::Unknown,
                        "prerequisite result receiver disappeared; replicated claim remains held",
                    )
                    .await;
            }
        });
    }

    /// Admit and track either an operator apply or one cron firing through
    /// worker completion, including rollback and cancellation.
    pub(super) async fn begin_deploy(
        &mut self,
        config: Config,
        events: mpsc::Sender<ApplyEvent>,
        register_schedule: bool,
        rerun_unknown_jobs: bool,
    ) {
        if self.startup_cleanup_pending {
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events
                .send(ApplyEvent::Error {
                    message: "startup cleanup still owns runtime allocations; retry after recovery"
                        .into(),
                })
                .await;
            return;
        }
        if rerun_unknown_jobs && let Err(message) = crate::bun::jobs::validate_rerun(&config) {
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events
                .send(ApplyEvent::Error {
                    message: message.into(),
                })
                .await;
            return;
        }
        if let Err(message) = self.validate_deploy_names(&config) {
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
        // A stopping workload still owns its instances until their exit is
        // confirmed; a deploy must not replace them underneath the stop.
        if let Some(target) = self.stopping_target(&config) {
            let message = format!(
                "workload {}/{} is still stopping; retry once its exit is confirmed",
                target.namespace, target.name
            );
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
        if let Some((namespace, app)) = self.restoring_target(&config) {
            let message = format!(
                "volumes of {namespace}/{app} are being restored from a snapshot; retry once the restore finishes"
            );
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
        // LOOP-INLINE: in-memory lock, no I/O
        let operation = match self.deploy_operations.start(&config).await {
            Ok(operation) => operation,
            Err(error) => {
                let message = format!("deploy refused: {error}");
                self.record_event(
                    crate::bun::events::EventKind::Deploy,
                    crate::bun::events::EventSeverity::Critical,
                    None,
                    None,
                    message.clone(),
                )
                .await;
                // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
                let _ = events.send(ApplyEvent::Error { message }).await;
                return;
            }
        };
        // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
        let _ = events
            .send(ApplyEvent::Accepted {
                operation_id: operation.id().to_string(),
            })
            .await;
        if self.draining.load(std::sync::atomic::Ordering::Relaxed) {
            let message = "node is draining for a binary upgrade; retry shortly".to_string();
            self.record_event(
                crate::bun::events::EventKind::Deploy,
                crate::bun::events::EventSeverity::Critical,
                None,
                None,
                "deploy refused while node is draining".to_string(),
            )
            .await;
            // LOOP-INLINE: in-memory lock, no I/O
            operation
                .finish(
                    crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                    message.clone(),
                )
                .await;
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
        // Register any cron-scheduled jobs so the event loop fires them
        // on their schedule rather than at deploy time (E).
        if register_schedule && let Err(error) = self.register_scheduled_jobs(&config).await {
            let message = error.to_string();
            // LOOP-INLINE: in-memory lock, no I/O
            operation
                .finish(
                    crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                    message.clone(),
                )
                .await;
            // LOOP-INLINE: one of the first events on the caller's fresh channel; never waits
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }

        // Forward deploy events to the caller, mirroring errors into the
        // event store. The deploy itself runs on its own task so a slow
        // pull or a rolling health wait can't wedge this loop
        // (DEP4/codex-M3); it drives its authoritative steps back
        // through `deploy_ops_tx`.
        let (forward_tx, mut forward_rx) = mpsc::channel(64);
        let event_store = self.events.clone();
        let observed_operation = operation.clone();
        let worker = DeployWorker {
            rerun_unknown_jobs,
            prepared_batch_jobs: None,
            grill: self.supervisor.grill().clone(),
            ops: DeployOps {
                tx: self.deploy_ops_tx.clone(),
            },
            drains: self.drains.clone(),
            operation: Some(operation),
            stop_confirmation_timeout: self.stop_confirmation_timeout,
            egress: self.egress_resolver(),
        };
        let worker_task = tokio::spawn(async move {
            worker.run_deploy(config, forward_tx).await;
        });
        tokio::spawn(async move {
            use crate::bun::deploy_operations::DeployOperationOutcome;
            let mut outcome = DeployOperationOutcome::Unknown;
            let mut message = "deploy worker ended without a terminal event".to_string();
            let mut completion = None;
            let mut events = Some(events);
            while let Some(event) = forward_rx.recv().await {
                match &event {
                    ApplyEvent::Complete { created, .. } => {
                        if outcome != DeployOperationOutcome::Failed {
                            outcome = DeployOperationOutcome::Completed;
                            message = format!("deploy completed ({created} instances)");
                            // Success becomes visible only after all trailing
                            // bookkeeping and the worker itself have finished.
                            completion = Some(event);
                        }
                        continue;
                    }
                    ApplyEvent::Error { message: error } => {
                        outcome = DeployOperationOutcome::Failed;
                        message = error.clone();
                        completion = None;
                        if let Some(store) = &event_store {
                            let timestamp = SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            store.write().await.record(
                                timestamp,
                                crate::bun::events::EventKind::Deploy,
                                crate::bun::events::EventSeverity::Critical,
                                None,
                                None,
                                None,
                                error.clone(),
                            );
                        }
                    }
                    _ => {}
                }
                // A stalled or disconnected observer cannot hold the
                // worker's outcome hostage. Close a full stream; its
                // client sees an incomplete stream and can query the ID.
                if let Some(sender) = &events
                    && sender.try_send(event).is_err()
                {
                    events = None;
                }
            }
            if let Err(error) = worker_task.await {
                outcome = DeployOperationOutcome::Unknown;
                message = format!("deploy worker ended unexpectedly: {error}");
                completion = None;
            } else if observed_operation.cancellation_observed() {
                outcome = DeployOperationOutcome::Cancelled;
                message = "deploy cancelled; in-flight work has finished".into();
                completion = None;
            }
            // Error events can precede rollback. Release target ownership
            // only after the worker has completed every mutation.
            observed_operation.finish(outcome, message.clone()).await;
            if let Some(sender) = events {
                if let Some(event) = completion {
                    let _ = sender.try_send(event);
                } else if outcome == DeployOperationOutcome::Unknown {
                    let _ = sender.try_send(ApplyEvent::Error { message });
                }
            }
        });
    }

    /// Enforce the image trust policy for a workload before deploying it.
    ///
    /// Returns `Err(reason)` to reject the deploy. It's a no-op (`Ok(None)`)
    /// when the policy doesn't require signatures or for a process workload
    /// (no image to verify).
    ///
    /// When `require_signatures` is set and this node has no council handle,
    /// it can't reach the manifest catalogue or the cluster root CA — the
    /// verification material simply isn't here. That used to skip the check
    /// (a fail-OPEN: an unsigned image sailed through on any worker or
    /// standalone node). Now it fails CLOSED: an image deploy is refused
    /// because the node can't prove the image is signed (IMG2). Cluster nodes
    /// all run a `CouncilNode` that replicates this state, so only a genuine
    /// standalone node hits this refusal.
    ///
    /// For a Pickle-hosted image it verifies the signature against the cluster
    /// root CA and returns the digest-pinned reference (`repo@sha256:…`) the
    /// deploy must use, so the runtime pulls exactly the verified bytes — a
    /// tag can move between verify and pull (IMG1).
    pub(super) async fn enforce_image_signature(
        &self,
        spec: &AppSpec,
    ) -> Result<Option<String>, String> {
        self.enforce_image_reference_signature(spec.image.as_deref())
            .await
    }

    pub(super) async fn enforce_image_reference_signature(
        &self,
        image: Option<&str>,
    ) -> Result<Option<String>, String> {
        if !self.trust_policy.require_signatures {
            return Ok(None);
        }
        // A process workload has no image; nothing to verify.
        if image.is_none() {
            return Ok(None);
        }
        let Some(council) = self.cluster.as_ref().and_then(|c| c.council.as_ref()) else {
            return Err(format!(
                "image {} requires a signature but this node has no cluster trust state to verify it against (require_signatures is enabled); run in cluster mode or disable require_signatures",
                image.unwrap_or("<none>")
            ));
        };
        // LOOP-INLINE: reads the local council state machine; no quorum round trip
        let catalog = council.manifest_catalog().await;
        // LOOP-INLINE: reads the local council state machine; no quorum round trip
        let security_state = council.security_state().await;
        let root_ca = security_state
            .active_ca(crate::sesame::types::CaRole::Root)
            .map(|ca| ca.certificate_der.clone());
        let verified = crate::meat::scheduler::verify_image_signature(
            image,
            &catalog,
            &self.trust_policy,
            root_ca.as_deref(),
            Some(&security_state.crl),
        )
        .map_err(|e| e.to_string())?;
        Ok(match (image, verified) {
            (Some(image), Some(digest)) => {
                Some(crate::meat::scheduler::pin_image_reference(image, &digest))
            }
            _ => None,
        })
    }

    /// Every age identity that could decrypt this namespace's secrets, newest
    /// generation first: the namespace-scoped keys then the cluster-wide keys.
    ///
    /// Returning all live generations (not just the active one) is what makes a
    /// secret survive a rotation window — a value encrypted under the retiring
    /// key still decrypts until it is retired, and a value re-encrypted under
    /// the new key decrypts immediately (PKI8).
    pub(super) async fn decrypt_identities(&self, namespace: &str) -> Vec<age::x25519::Identity> {
        let Some(cluster) = self.cluster.as_ref() else {
            return Vec::new();
        };
        let Some(ikm) = cluster.wrapping_ikm else {
            return Vec::new();
        };
        let Some(council) = cluster.council.as_ref() else {
            return Vec::new();
        };
        // LOOP-INLINE: reads the local council state machine; no quorum round trip
        let security_state = council.security_state().await;

        let ns_scope = crate::sesame::types::AgeKeyScope::Namespace(namespace.to_string());
        security_state
            .age_keypairs_for_scope(&ns_scope)
            .into_iter()
            .chain(
                security_state
                    .age_keypairs_for_scope(&crate::sesame::types::AgeKeyScope::ClusterWide),
            )
            .filter_map(|kp| crate::sesame::secret::unwrap_age_identity(kp, &ikm).ok())
            .collect()
    }

    /// Build an OCI spec, decrypting `ENC[AGE:...]` env values with `identity`.
    ///
    /// This is synchronous on purpose: the `SecretDecryptor` closure is `!Send`
    /// and must never be held across an `.await` in the (spawned) agent task, so
    /// it is created and consumed entirely within this call.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn oci_spec_with_secrets(
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        instance_id: &str,
        host_port: Option<u16>,
        cgroup_str: &str,
        volumes_dir: Option<&std::path::Path>,
        netns_path: Option<&str>,
        identities: Vec<age::x25519::Identity>,
    ) -> Result<crate::grill::oci::OciSpec, BunError> {
        // Try each live generation's identity until one decrypts the value, so
        // a secret encrypted under any still-present key is readable across a
        // rotation window (PKI8).
        let decryptor: Option<crate::grill::oci::SecretDecryptor> = if identities.is_empty() {
            None
        } else {
            Some(Box::new(move |encrypted: &str| {
                let mut last_err = String::from("no age identity could decrypt the value");
                for id in &identities {
                    match crate::sesame::secret::decrypt_secret(encrypted, id) {
                        Ok(plain) => return Ok(plain),
                        Err(e) => last_err = e.to_string(),
                    }
                }
                Err(last_err)
            }) as crate::grill::oci::SecretDecryptor)
        };
        // A decryption failure fails the deploy closed (M4): the container must
        // not start with a broken secret injected as `DECRYPT_ERROR:...`.
        crate::grill::oci::generate_oci_spec_with_decryptor(
            app_name,
            namespace,
            spec,
            instance_id,
            host_port,
            cgroup_str,
            volumes_dir,
            netns_path,
            decryptor.as_ref(),
        )
        .map_err(|reason| BunError::DeployFailed {
            app_name: app_name.to_string(),
            reason,
        })
    }

    /// Fast pre-create bookkeeping for a fresh instance (the loop side of the
    /// former `drive_instance_startup`): transition to Preparing, prepare
    /// managed volumes and the identity dir, and build the OCI spec. The
    /// spawned deploy task calls `grill.create` with the returned spec off the
    /// loop, so the image pull no longer blocks health checks (DEP4).
    pub(super) async fn prepare_fresh_instance(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<PreparedInstance, BunError> {
        // Pending → Preparing. Storage provisioning below can outlast the
        // turn and answer `StillRunning`; the deploy worker then asks again
        // for the same incarnation, which is already Preparing (#386).
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            if instance.state != ContainerState::Preparing {
                instance.state = instance.state.transition_to(ContainerState::Preparing)?;
            }
        }

        let host_port = self
            .supervisor
            .get_instance(instance_id)
            .and_then(|i| i.host_port);

        let cgroup_path =
            crate::grill::cgroup::instance_cgroup_path(namespace, app_name, instance_id)?;
        let cgroup_str = cgroup_path.to_string_lossy().into_owned();
        let netns_path = self
            .netns_paths
            .get(instance_id)
            .map(|p| p.to_string_lossy().into_owned());
        let identities = self.decrypt_identities(namespace).await;
        if identities.is_empty() && spec.env.values().any(|v| v.is_encrypted()) {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "encrypted secrets require cluster security state (unavailable here)"
                    .to_string(),
            });
        }
        // Claim test storage and provision every bind source before launch.
        self.prepare_storage(app_name, namespace, spec).await?;

        // The per-instance identity dir must exist before create (PKI7).
        if let Err(e) = self.prepare_instance_identity(instance_id) {
            eprintln!("bun: warning: {e}");
        }

        let oci_spec = Self::oci_spec_with_secrets(
            app_name,
            namespace,
            spec,
            &instance_id.0,
            host_port,
            &cgroup_str,
            Some(&self.volumes_dir),
            netns_path.as_deref(),
            identities,
        )?;

        Ok(PreparedInstance {
            oci_spec,
            cgroup_path,
            has_init: !spec.init.is_empty(),
        })
    }

    /// Post-start bookkeeping for a fresh instance (the loop side of the tail
    /// of `drive_instance_startup`): record the container IP, transition to
    /// HealthWait (→Running if no health checks), register its service-map
    /// backend and finish kernel networking.
    pub(super) async fn finish_fresh_instance(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        evidence: &launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        self.record_launch_execution(instance_id, &evidence.execution)?;
        self.spawn_log_forwarder(instance_id, app_name, namespace);
        self.persist_instance_record(instance_id, evidence).await?;
        let container_ip = evidence.container_ip;

        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.container_ip = container_ip;
        }

        // Starting → HealthWait, then immediately to Running if no health checks
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::HealthWait)?;
            if instance.health_config.is_none() {
                instance.state = instance.state.transition_to(ContainerState::Running)?;
            }
        }

        if let Some(instance) = self.supervisor.get_instance(instance_id)
            && let Some(host_port) = instance.host_port
        {
            let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
            let backend = self.local_backend(
                instance_id,
                &service_id,
                instance.container_ip,
                host_port,
                instance.state == ContainerState::Running,
            );
            self.service_map
                .add_backend(&service_id, backend)
                .map_err(|error| BunError::BackendPublication {
                    service: service_id,
                    reason: error.to_string(),
                })?;
        }

        self.finish_instance_networking(app_name, namespace).await?;
        Ok(())
    }

    pub(super) async fn reserve_rolling_instance(
        &mut self,
        id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<Option<u16>, BunError> {
        if let Some(owner) = self.supervisor.get_instance(id) {
            return Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!(
                    "instance {id} is still owned by {}/{}",
                    owner.namespace, owner.app_name
                ),
            });
        }
        let host_port = if spec.port.is_some() {
            // LOOP-INLINE: in-memory lock, no I/O
            Some(self.supervisor.port_allocator.allocate().await?)
        } else {
            None
        };
        self.supervisor.instances.insert(
            id.clone(),
            crate::bun::supervisor::WorkloadInstance {
                id: id.clone(),
                app_name: app_name.into(),
                namespace: namespace.into(),
                state: ContainerState::Preparing,
                health_counters: Default::default(),
                restart_count: 0,
                last_restart: None,
                host_port,
                container_ip: None,
                created_at: Instant::now(),
                restart_policy: Default::default(),
                health_config: None,
                is_job: false,
                retry_pending: false,
                image: spec.image.clone().unwrap_or_default(),
                oci_spec: None,
                identity: None,
                identity_mount: None,
            },
        );
        self.supervisor
            .app_instances
            .entry((app_name.into(), namespace.into()))
            .or_default()
            .push(id.clone());
        Ok(host_port)
    }

    /// Fast pre-create bookkeeping for a rolling-redeploy instance: fail closed
    /// on undecryptable secrets, prepare its identity dir, build the OCI spec.
    /// The spawned task then creates and starts it off the loop.
    pub(super) async fn prepare_rolling_instance(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        host_port: Option<u16>,
    ) -> Result<crate::grill::oci::OciSpec, BunError> {
        let identities = self.decrypt_identities(namespace).await;
        if identities.is_empty() && spec.env.values().any(|v| v.is_encrypted()) {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!(
                    "cannot start {}: encrypted secrets require cluster security state",
                    instance_id.0
                ),
            });
        }
        self.prepare_storage(app_name, namespace, spec).await?;
        if let Err(e) = self.prepare_instance_identity(instance_id) {
            eprintln!("bun: warning: {e}");
        }
        let cgroup_path =
            crate::grill::cgroup::instance_cgroup_path(namespace, app_name, instance_id)?;
        let oci_spec = Self::oci_spec_with_secrets(
            app_name,
            namespace,
            spec,
            &instance_id.0,
            host_port,
            &cgroup_path.to_string_lossy(),
            Some(&self.volumes_dir),
            None,
            identities,
        )?;
        let owner = self
            .supervisor
            .get_instance_mut(instance_id)
            .ok_or_else(|| BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            })?;
        owner.oci_spec = Some(oci_spec.clone());
        Ok(oci_spec)
    }

    /// Forget the old instances and register the healthy new ones after a
    /// redeploy: rebuild the service map and health config, register backends,
    /// finish kernel networking, store ingress, record history.
    ///
    /// Bookkeeping only (M7): the deploy worker has already drained and
    /// stopped every instance in `existing` off the command loop via
    /// `drain_and_stop_instance` — no waiting happens here.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finalise_rolling_deploy(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        existing: &[InstanceId],
        new_ids: &[InstanceId],
        new_ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        new_ips: &std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
        mut new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
        now: Instant,
    ) -> Result<(), BunError> {
        // DEP5: the worker routed traffic to the fresh instances (published
        // backends) before draining and stopping the old ones, so by the time
        // this op runs the cut-over has already happened. What's left is to
        // tear the old bookkeeping down and install the new.
        // A failed first cleanup must not leave another exited old instance
        // eligible for the crash-restart driver.
        for old_id in existing {
            self.retain_stopped_instance(old_id);
        }
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        // M7: publishing backends and retiring old instances now happen
        // incrementally as the rollout steps, so by the time we get here both
        // are usually already done. These loops are idempotent catch-ups for
        // anything the stepped path didn't cover (a zero-port app, or an
        // instance the planner retired before this call).
        if spec.port.is_some() {
            for new_id in new_ids {
                if let Some(host_port) = new_ports.get(new_id).copied().flatten() {
                    let backend = self.local_backend(
                        new_id,
                        &service_id,
                        new_ips.get(new_id).copied().flatten(),
                        host_port,
                        true,
                    );
                    self.service_map
                        .add_backend(&service_id, backend)
                        .map_err(|error| BunError::BackendPublication {
                            service: service_id.clone(),
                            reason: error.to_string(),
                        })?;
                }
            }
            self.rebuild_routing_table().await;
        }

        for old_id in existing {
            match self.finish_retire_bookkeeping(old_id).await {
                // The tick finishes either: one waits on the leader, the
                // other on disk cleanup running off the loop.
                Err(BunError::ProducerReleasePending { .. } | BunError::StillRunning { .. }) => {
                    self.defer_retirement(old_id)
                }
                result => result?,
            }
        }
        // In a cluster the kernel entry also names other nodes' backends and
        // the new replicas; only the old instances leave it (#481). The
        // stepped rollout took them out of the local map, not the installed
        // view, so every local backend there that isn't a replacement is old.
        let replaced: Vec<InstanceId> = self
            .service_map_tx
            .borrow()
            .resolve(&service_id)
            .map(|entry| {
                entry
                    .backends
                    .iter()
                    .filter(|backend| backend.local)
                    .filter(|backend| !new_ids.iter().any(|new| new.0 == backend.instance_id))
                    .map(|backend| InstanceId(backend.instance_id.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if !self
            .withdraw_backends_from_view(&service_id, &replaced)
            .await?
        {
            self.withdraw_service_ebpf(&service_id).await?;
        }
        // Re-registration can be refused: a stop that withdrew the council's
        // allocation mid-rollout leaves nothing to register against. The
        // retained replacements then retire by proving withdrawal against
        // this local reservation, so a refusal must put it back.
        let reserved = self.service_map.clone();
        let _ = self.service_map.unregister(&service_id);

        for new_id in new_ids {
            let host_port = new_ports.get(new_id).copied().flatten();
            let health_config = spec
                .health
                .as_ref()
                .zip(spec.port)
                .map(|(hs, port)| crate::bun::health::HealthCheckConfig::from_spec(hs, port));
            if let Some(ref cfg) = health_config {
                self.supervisor
                    .register_health(new_id.clone(), cfg.clone(), now);
            }
            let (identity, identity_mount) = self
                .supervisor
                .get_instance_mut(new_id)
                .map(|owner| (owner.identity.take(), owner.identity_mount.take()))
                .unwrap_or_default();
            self.supervisor.instances.insert(
                new_id.clone(),
                crate::bun::supervisor::WorkloadInstance {
                    id: new_id.clone(),
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    state: crate::grill::state::ContainerState::Running,
                    health_counters: crate::bun::health::HealthCounters::new(),
                    restart_count: 0,
                    last_restart: None,
                    host_port,
                    container_ip: new_ips.get(new_id).copied().flatten(),
                    created_at: now,
                    restart_policy: crate::bun::restart::RestartPolicy::default(),
                    health_config,
                    is_job: false,
                    retry_pending: false,
                    image: spec.image.clone().unwrap_or_default(),
                    oci_spec: new_specs.remove(new_id),
                    identity,
                    identity_mount,
                },
            );
        }
        let key = (app_name.to_string(), namespace.to_string());
        self.supervisor.app_instances.insert(key, new_ids.to_vec());

        if let Some(port) = spec.port
            && let Err(error) = self.register_replacement_service(
                &service_id,
                port,
                spec,
                new_ids,
                new_ports,
                new_ips,
            )
        {
            self.service_map = reserved;
            return Err(error);
        }

        self.finish_instance_networking(app_name, namespace).await?;
        // The rolled-out spec owns the route now (#307): a changed host
        // replaces the old one, and a spec without ingress drops it. The
        // stopped-app restore before the rollout only inserts when nothing
        // is stored, so this is where a running app's route changes.
        let key = (namespace.to_string(), app_name.to_string());
        match &spec.ingress {
            Some(ingress) => {
                self.ingress_configs.insert(key, ingress.clone());
            }
            None => {
                self.ingress_configs.remove(&key);
            }
        }
        self.rebuild_routing_table().await;

        let entry = crate::meat::deploy_types::DeployHistoryEntry {
            id: crate::meat::deploy_types::DeployId(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ),
            app_id: crate::meat::types::AppId::new(app_name, namespace),
            image: spec.image.clone().unwrap_or_default(),
            result: crate::meat::deploy_types::DeployResult::Completed,
            created_at: SystemTime::now(),
            completed_at: SystemTime::now(),
            steps_completed: new_ids.len(),
            steps_total: new_ids.len(),
            spec: Some(Box::new(spec.clone())),
        };
        self.deploy_history.write().await.push(entry);
        Ok(())
    }

    /// Refuse user/cleanup stops while a deploy can still mutate the target.
    pub(super) async fn refuse_while_deploying(
        &self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        // Worker completion releases ownership only after its last runtime
        // mutation. Refuse before retiring a schedule or claiming a stop.
        // Both command admission and cron firing run on this same event loop.
        // LOOP-INLINE: in-memory lock, no I/O
        if let Some(operation) = self
            .deploy_operations
            .snapshot()
            .await
            .active_deploys
            .into_iter()
            .find(|operation| {
                operation
                    .targets
                    .iter()
                    .any(|target| target.name == app_name && target.namespace == namespace)
            })
        {
            return Err(BunError::WorkloadBusy {
                app_name: app_name.to_owned(),
                namespace: namespace.to_owned(),
                operation_id: operation.id,
            });
        }
        Ok(())
    }
}
