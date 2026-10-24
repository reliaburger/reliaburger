//! The deploy worker: the task that drives one deploy's runtime steps off
//! the loop, asking the loop through [`DeployOps`] for every change of state.

use super::*;

/// Runs one deploy on its own spawned task so the command loop keeps
/// servicing health checks, restarts and other commands while an image pulls
/// or a rolling deploy waits on health (DEP4/codex-M3).
///
/// The worker owns the blocking grill I/O — create (the image pull), start,
/// init-container polling, and the rolling health wait — but not the
/// supervisor state machine. Every authoritative mutation travels back to the
/// loop as a `DeployOp` through `ops`, so the loop stays the single owner of
/// supervisor / service-map / networking state.
pub(super) struct DeployWorker<G: Grill> {
    pub(super) grill: G,
    pub(super) ops: DeployOps,
    /// Shared drain tracker, so the worker can drain-and-stop a retiring
    /// instance off the command loop (M7) rather than sending the whole wait
    /// to the loop as an op.
    pub(super) drains: crate::wrapper::draining::SharedDrains,
    pub(super) operation: Option<crate::bun::deploy_operations::DeployOperationHandle>,
    /// The agent's `[runtime] stop_confirmation_timeout_secs`.
    pub(super) stop_confirmation_timeout: std::time::Duration,
    /// Resolves an instance's egress allowlist before the loop programs
    /// it, so the loop never waits on DNS (#419).
    pub(super) egress: launch_evidence::EgressResolver,
    /// The node's secret providers, for `PROVIDER[...]` env values (#1123).
    pub(super) secrets: crate::sesame::secret_provider::SecretProviders,
}

/// The replacement instances a rollout, or the restoration of an aborted
/// one, started, and what launching them recorded.
#[derive(Debug, Default)]
pub(super) struct Replacements {
    /// Started, healthy and published as backends, in start order.
    pub(super) healthy: Vec<InstanceId>,
    pub(super) ports: std::collections::HashMap<InstanceId, Option<u16>>,
    pub(super) specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
    pub(super) ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
    /// Every reservation, healthy or not: what an abort must clean up.
    pub(super) prepared: Vec<InstanceId>,
    /// Reservations whose runtime create was attempted.
    pub(super) runtime_attempted: std::collections::HashSet<InstanceId>,
}

/// Everything [`DeployWorker::abort_rollout`] needs to know about a rollout
/// that failed part-way.
#[derive(Debug, Clone, Copy)]
pub(super) struct AbortedRollout<'a> {
    pub(super) app_name: &'a str,
    pub(super) namespace: &'a str,
    /// The spec the rollout was rolling out.
    pub(super) spec: &'a AppSpec,
    /// The spec the old instances run, when this node knows it.
    pub(super) previous: Option<&'a AppSpec>,
    pub(super) started: &'a Replacements,
    /// Old instances the rollout hadn't retired yet.
    pub(super) remaining_old: &'a [InstanceId],
    /// How many old instances it had retired.
    pub(super) retired: usize,
    pub(super) replica_count: u32,
    pub(super) config: &'a crate::meat::deploy_types::DeployConfig,
}

/// The last few hundred bytes of a runtime's captured stderr (`{stem}.stderr`),
/// on one line. `None` when nothing was captured or the file can't be read:
/// the caller still has the exit status to report.
pub(super) async fn captured_stderr_tail(stem: &std::path::Path) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(stem.with_extension("stderr"))
        .await
        .ok()?;
    let length = file.metadata().await.ok()?.len();
    file.seek(std::io::SeekFrom::Start(
        length.saturating_sub(INIT_FAILURE_STDERR_BYTES),
    ))
    .await
    .ok()?;
    let mut bytes = Vec::new();
    file.take(INIT_FAILURE_STDERR_BYTES)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    (!lines.is_empty()).then(|| lines.join("; "))
}

impl<G: Grill + Clone + 'static> DeployWorker<G> {
    /// Retain a created instance's network reference, have the loop record
    /// it and program the instance's network before it starts, and stop the
    /// container if the loop refuses. The runtime calls happen here, on the
    /// worker, not on the loop (#351, stage 3).
    pub(super) async fn prepare_network(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        let retained = launch_evidence::retain_network(&self.grill, instance_id, spec)
            .await
            .map_err(BunError::from);
        let egress = self.egress.resolve(spec).await;
        let result = match retained {
            // A failed retain fails the step without touching the journal.
            Err(error) => {
                self.ops
                    .apply_network_pre_start(
                        instance_id,
                        app_name,
                        spec,
                        cgroup_path,
                        Err(error),
                        egress,
                    )
                    .await
            }
            // The loop answers `StillRunning` while the reference's journal
            // write is in flight (#1266); ask again until it has landed.
            Ok(retained) => {
                deploy_ops::retry_while_still_running(|| {
                    self.ops.apply_network_pre_start(
                        instance_id,
                        app_name,
                        spec,
                        cgroup_path,
                        Ok(retained.clone()),
                        egress.clone(),
                    )
                })
                .await
            }
        };
        if result.is_err() {
            let _ = self.grill.stop(instance_id).await;
        }
        result
    }

    pub(super) async fn report_cancellation(&self, events: &mpsc::Sender<ApplyEvent>) -> bool {
        if self
            .operation
            .as_ref()
            .is_some_and(|operation| operation.cancellation_requested())
        {
            let _ = events
                .send(ApplyEvent::Error {
                    message: "deploy cancellation requested; finishing owned cleanup".into(),
                })
                .await;
            return true;
        }
        false
    }

    pub(super) async fn wait_for_deploy_health(
        &self,
        id: &InstanceId,
        spec: &AppSpec,
        container_ip: Option<std::net::Ipv4Addr>,
        wait: std::time::Duration,
    ) -> Result<(), String> {
        let health = wait_instance_healthy(&self.grill, id, spec, container_ip, wait);
        if let Some(operation) = &self.operation {
            tokio::select! {
                biased;
                _ = operation.cancelled() => Err("deploy cancellation requested; finishing owned cleanup".into()),
                result = health => result,
            }
        } else {
            health.await
        }
    }

    /// Deploy all apps from a config, streaming progress events. The
    /// mirror of the former `BunAgent::deploy`, but off the command loop.
    pub(super) async fn run_deploy(self, config: Config, events: mpsc::Sender<ApplyEvent>) {
        if self.report_cancellation(&events).await {
            return;
        }
        // Jobs take the common job lifecycle; a node's deploy runs apps only.
        if !config.job.is_empty() {
            let _ = events
                .send(ApplyEvent::Error {
                    message: "jobs run through the common job lifecycle, not a node's app deploy"
                        .into(),
                })
                .await;
            return;
        }
        let now = Instant::now();
        let mut all_ids: Vec<String> = Vec::new();
        let deployed_apps: Vec<(String, String)> = config
            .app
            .iter()
            .map(|(name, spec)| {
                (
                    name.clone(),
                    spec.namespace
                        .clone()
                        .unwrap_or_else(|| "default".to_string()),
                )
            })
            .collect();

        if !config.app.is_empty()
            && let Some(operation) = &self.operation
        {
            operation
                .advance(
                    crate::bun::deploy_operations::DeployOperationPhase::DeployingApps,
                    None,
                    format!("deploying {} app(s)", config.app.len()),
                )
                .await;
        }

        for (app_name, spec) in &config.app {
            if self.report_cancellation(&events).await {
                return;
            }
            let namespace = spec.namespace.as_deref().unwrap_or("default");

            if self.report_cancellation(&events).await {
                return;
            }

            if let Some(operation) = &self.operation {
                operation
                    .advance(
                        crate::bun::deploy_operations::DeployOperationPhase::DeployingApps,
                        Some(crate::bun::deploy_operations::DeployTarget {
                            kind: crate::bun::deploy_operations::DeployTargetKind::App,
                            name: app_name.clone(),
                            namespace: namespace.to_string(),
                        }),
                        format!("deploying app {namespace}/{app_name}"),
                    )
                    .await;
            }

            // Gate on image signature first (IMG1), for the app's image and
            // each init container's own (#701). A verified image comes back
            // pinned to its manifest digest; the pinned spec shadows the
            // original for the rest of this iteration.
            let pinned_spec = match self.pin_images(spec).await {
                Ok(pinned) => pinned,
                Err(reason) => {
                    let _ = events.send(ApplyEvent::Error { message: reason }).await;
                    return;
                }
            };
            let spec = pinned_spec.as_ref().unwrap_or(spec);

            // Asked before the new spec replaces the one the replicas run.
            let in_place = self
                .ops
                .share_change_in_place(app_name, namespace, spec)
                .await;
            // The spec the running replicas came from: what an aborted
            // rollout restores.
            let previous = match self
                .ops
                .store_deployed_spec(app_name, namespace, spec)
                .await
            {
                Ok(previous) => previous,
                Err(error) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    return;
                }
            };

            let existing = self.ops.list_existing_owned(app_name, namespace).await;

            if let Some(change) = in_place.filter(|_| !existing.is_empty()) {
                if self
                    .change_share_in_place(
                        app_name,
                        namespace,
                        spec,
                        previous.as_ref(),
                        change,
                        &events,
                    )
                    .await
                    .is_break()
                {
                    return;
                }
                all_ids.extend(
                    self.ops
                        .list_existing_owned(app_name, namespace)
                        .await
                        .iter()
                        .map(|id| id.0.clone()),
                );
                continue;
            }

            if !existing.is_empty() {
                // A standalone `relish stop` keeps its stopped replicas owned
                // but releases their service and ingress route. The rollout
                // over them publishes backends into that service, so it has
                // to exist again first.
                if let Err(error) = self
                    .ops
                    .restore_stopped_routing(app_name, namespace, spec)
                    .await
                {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    return;
                }
                // Dispatch on deploy strategy (E): blue-green stands up the
                // whole new fleet before swapping; rolling replaces one at a
                // time. Everything else about the deploy is identical. An app
                // with a managed volume always rolls stop-first (`for_app`).
                let strategy = crate::meat::deploy_types::DeployConfig::for_app(spec).strategy;
                let outcome = match strategy {
                    crate::meat::deploy_types::DeployStrategy::BlueGreen => {
                        self.blue_green_redeploy(
                            app_name,
                            namespace,
                            spec,
                            previous.as_ref(),
                            existing,
                            &events,
                            now,
                        )
                        .await
                    }
                    crate::meat::deploy_types::DeployStrategy::Rolling => {
                        self.rolling_redeploy(
                            app_name,
                            namespace,
                            spec,
                            previous.as_ref(),
                            existing,
                            &events,
                            now,
                        )
                        .await
                    }
                };
                if outcome.is_break() {
                    return;
                }
                all_ids.extend(
                    self.ops
                        .list_existing_owned(app_name, namespace)
                        .await
                        .iter()
                        .map(|id| id.0.clone()),
                );
                continue;
            }

            // Fresh deploy: no existing instances.
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("deploying app {app_name} (replicas: {})", spec.replicas),
                })
                .await;

            let ids = match self
                .ops
                .supervisor_deploy_app(app_name, namespace, spec)
                .await
            {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
            };

            if let Some(port) = spec.port {
                let firewall = spec.firewall.as_ref().and_then(|f| {
                    if f.allow_from.is_empty() {
                        None
                    } else {
                        Some(f.allow_from.clone())
                    }
                });
                if let Err(error) = self
                    .ops
                    .register_service_app(app_name, namespace, port, firewall)
                    .await
                {
                    // A node can receive a deploy before the council's allocation
                    // for it reaches its view. Leaving these Pending instances
                    // behind would turn the retry into a rollout of a service this
                    // node never published, which can never succeed.
                    self.ops
                        .abandon_unstarted_instances(app_name, namespace, &ids)
                        .await;
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    return;
                }
            }

            if let Some(ref ingress) = spec.ingress {
                self.ops.store_ingress(app_name, namespace, ingress).await;
            }

            for id in &ids {
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!("creating instance {}", id.0),
                    })
                    .await;

                if let Err(e) = self
                    .drive_fresh_instance(id, app_name, namespace, spec)
                    .await
                {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }

                self.ops.provision_identity(app_name, namespace, id).await;

                let _ = events
                    .send(ApplyEvent::InstanceCreated {
                        id: id.0.clone(),
                        app: app_name.to_string(),
                    })
                    .await;
            }

            self.ops
                .push_deploy_history(crate::meat::deploy_types::DeployHistoryEntry {
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
                    steps_completed: ids.len(),
                    steps_total: ids.len(),
                    spec: Some(Box::new(spec.clone())),
                })
                .await;

            all_ids.extend(ids.iter().map(|id| id.0.clone()));
        }

        if self.report_cancellation(&events).await {
            return;
        }
        if let Some(operation) = &self.operation {
            operation
                .advance(
                    crate::bun::deploy_operations::DeployOperationPhase::RebuildingRoutes,
                    None,
                    "rebuilding service and ingress routes",
                )
                .await;
        }
        self.ops.rebuild_routing_table().await;

        let _ = events
            .send(ApplyEvent::Complete {
                created: all_ids.len(),
                instances: all_ids,
            })
            .await;
        for (app, namespace) in deployed_apps {
            self.ops.record_deployed_event(&app, &namespace).await;
        }
    }

    /// Check every image `spec` runs against the trust policy: the app's and
    /// each init container's and sidecar's own. Returns the spec with each verified image
    /// pinned to its digest, or `None` when nothing needed pinning.
    async fn pin_images(&self, spec: &AppSpec) -> Result<Option<AppSpec>, String> {
        let mut pinned: Option<AppSpec> = None;
        if let Some(image) = self
            .ops
            .enforce_image_signature(spec.image.as_deref())
            .await?
        {
            pinned.get_or_insert_with(|| spec.clone()).image = Some(image);
        }
        for (index, init) in spec.init.iter().enumerate() {
            // An init without an image runs the app's, checked above.
            if init.image.is_none() {
                continue;
            }
            if let Some(image) = self
                .ops
                .enforce_image_signature(init.image.as_deref())
                .await?
                && let Some(slot) = pinned
                    .get_or_insert_with(|| spec.clone())
                    .init
                    .get_mut(index)
            {
                slot.image = Some(image);
            }
        }
        // A sidecar's own image answers to the same policy (#1120).
        for (index, sidecar) in spec.sidecar.iter().enumerate() {
            if sidecar.image.is_none() {
                continue;
            }
            if let Some(image) = self
                .ops
                .enforce_image_signature(sidecar.image.as_deref())
                .await?
                && let Some(slot) = pinned
                    .get_or_insert_with(|| spec.clone())
                    .sidecar
                    .get_mut(index)
            {
                slot.image = Some(image);
            }
        }
        Ok(pinned)
    }

    /// Run the same owned init chain for fresh and rolling replacements.
    pub(super) async fn drive_initialisers(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: &AppSpec,
        app_oci: &crate::grill::oci::OciSpec,
        cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        if spec.init.is_empty() {
            return Ok(());
        }
        self.ops
            .transition_state(instance_id, ContainerState::Initialising)
            .await?;
        for (i, init_spec) in spec.init.iter().enumerate() {
            let init_id = self.ops.register_initialiser(instance_id, i).await?;
            // Its own image, the app's env and mounts (#701).
            let init_oci = crate::grill::oci::generate_init_oci_spec(init_spec, app_oci);
            if let Err(error) = self.grill.create(&init_id, &init_oci).await {
                // A missing init binary fails its parent the way its own would.
                if let Some(problem) = error.launch_problem() {
                    self.ops
                        .record_launch_failure(instance_id, problem.to_string())
                        .await;
                }
                return Err(error.into());
            }
            // The parent's retirement forgets this launch's journal by its
            // generation. One the runtime can't say is kept, for the next
            // start's adoption to forget.
            if let Ok(Some(generation)) = self.grill.launch_generation(&init_id).await {
                self.ops
                    .record_initialiser_generation(instance_id, &init_id, generation)
                    .await;
            }
            self.start_or_fail(&init_id, instance_id).await?;

            // Bounded wait: a hung init can't wedge the deploy forever (and
            // no longer wedges the loop at all — this poll is off it).
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(INIT_TIMEOUT_SECS);
            let failure = loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let state = self.grill.state(&init_id).await?;
                if state == ContainerState::Stopped {
                    break match self.grill.exit_code(&init_id).await? {
                        Some(0) => None,
                        Some(code) => Some(format!("exited with code {code}")),
                        None => Some("stopped without an exit code".to_string()),
                    };
                }
                if std::time::Instant::now() >= deadline {
                    let _ = self.grill.kill(&init_id).await;
                    break Some(format!("did not finish within {INIT_TIMEOUT_SECS}s"));
                }
            };

            if let Some(failure) = failure {
                let _ = self
                    .ops
                    .transition_state(instance_id, ContainerState::Failed)
                    .await;
                let reason = match self.grill.log_stem(&init_id).await {
                    Some(stem) => match captured_stderr_tail(&stem).await {
                        Some(stderr) => format!("{failure}: {stderr}"),
                        None => failure,
                    },
                    None => failure,
                };
                return Err(BunError::InitContainerFailed {
                    instance_id: instance_id.clone(),
                    init_index: i,
                    reason,
                });
            }
            kill_runtime_instance(&self.grill, &init_id, self.stop_confirmation_timeout).await?;
            self.ops.forget_initialiser(instance_id, &init_id).await?;
            // Runc can remove the shared cgroup when an init exits. Its
            // successor must receive policy for the new kernel identity
            // before either another init or the main workload executes.
            self.prepare_network(instance_id, app_name, Some(spec), cgroup_path)
                .await?;
        }
        Ok(())
    }

    /// Start `spec`'s sidecars beside `instance_id` (#1120), after its init
    /// containers and before the instance itself, from `app_oci`, the spec
    /// the instance was created from with its secret references unresolved.
    /// Each is registered first, so a failure part-way leaves nothing the
    /// instance's retirement can't find. A sidecar that can't start fails
    /// its instance, with the reason.
    pub(super) async fn drive_sidecars(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        app_oci: &crate::grill::oci::OciSpec,
    ) -> Result<(), BunError> {
        if spec.sidecar.is_empty() {
            return Ok(());
        }
        let names = spec
            .sidecar
            .iter()
            .map(|sidecar| sidecar.name.clone())
            .collect();
        let registered = self.ops.register_sidecars(instance_id, names).await?;
        let launches = super::sidecars::sidecar_launches(spec, instance_id, app_oci);
        let Err(failure) = super::sidecars::launch_sidecars(
            &self.grill,
            &self.secrets,
            app_name,
            namespace,
            &launches,
        )
        .await
        else {
            return Ok(());
        };
        // The ones that did start mustn't run beside an instance that never
        // will. Retirement kills them again if this can't.
        if let Err(error) =
            super::sidecars::kill_sidecars(&self.grill, &registered, self.stop_confirmation_timeout)
                .await
        {
            eprintln!("bun: {instance_id}: sidecars of a failed launch await retirement: {error}");
        }
        let reason = format!(
            "sidecar {} failed to start: {}",
            failure.sidecar, failure.error
        );
        match failure.error.launch_problem() {
            Some(problem) => {
                self.ops
                    .record_launch_failure(
                        instance_id,
                        format!("sidecar {}: {problem}", failure.sidecar),
                    )
                    .await;
            }
            None => {
                let _ = self
                    .ops
                    .transition_state(instance_id, ContainerState::Failed)
                    .await;
            }
        }
        Err(BunError::DeployFailed {
            app_name: app_name.to_string(),
            reason,
        })
    }

    /// Resolve `spec`'s secret providers and create the instance in the
    /// runtime, returning the spec it was created from. A launch the runtime
    /// refuses (#881), or one whose secret a provider won't give (#1123),
    /// fails the instance with the reason before the error returns.
    async fn create_or_fail<'s>(
        &self,
        target: super::launch::LaunchTarget<'_>,
        spec: &'s crate::grill::oci::OciSpec,
    ) -> Result<std::borrow::Cow<'s, crate::grill::oci::OciSpec>, BunError> {
        let error = match super::launch::create_with_secrets(
            &self.grill,
            &self.secrets,
            target,
            spec,
        )
        .await
        {
            Ok(launch) => return Ok(launch),
            Err(error) => error,
        };
        if let Some(problem) = error.launch_problem() {
            self.ops
                .record_launch_failure(target.instance_id, problem.to_string())
                .await;
        }
        Err(error.into())
    }

    /// Start `launched` in the runtime. A launch the runtime refuses at
    /// `exec`, because the binary vanished after `create`'s check (#905),
    /// fails `instance_id` with the reason before the error returns. The two
    /// differ for an init container, whose refusal fails its parent.
    async fn start_or_fail(
        &self,
        launched: &InstanceId,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        let Err(error) = self.grill.start(launched).await else {
            return Ok(());
        };
        if let Some(problem) = error.launch_problem() {
            self.ops
                .record_launch_failure(instance_id, problem.to_string())
                .await;
        }
        Err(error.into())
    }

    /// Drive a fresh instance through create → egress → init → start →
    /// HealthWait. The blocking grill calls (create/init/start) run here on
    /// the task; the loop applies the state transitions and bookkeeping.
    pub(super) async fn drive_fresh_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        let prepared = self
            .ops
            .prepare_fresh_instance(instance_id, app_name, namespace, spec)
            .await?;

        // The image pull happens here, off the loop, and so does any secret
        // provider call. The supervisor keeps the spec with its references,
        // never the resolved secrets.
        let target = super::launch::LaunchTarget {
            instance_id,
            app_name,
            namespace,
        };
        let launch = self.create_or_fail(target, &prepared.oci_spec).await?;
        self.ops
            .store_oci_spec(instance_id, prepared.oci_spec.clone())
            .await;

        // create → program → start: the workload never runs ahead of its
        // egress policy (#86). On failure the loop stops the container.
        self.prepare_network(instance_id, app_name, Some(spec), &prepared.cgroup_path)
            .await?;

        if prepared.has_init {
            self.drive_initialisers(instance_id, app_name, spec, &launch, &prepared.cgroup_path)
                .await?;
        }
        // Sidecars after the inits and before the app (#1120).
        self.drive_sidecars(instance_id, app_name, namespace, spec, &prepared.oci_spec)
            .await?;

        self.ops
            .transition_state(instance_id, ContainerState::Starting)
            .await?;
        self.start_or_fail(instance_id, instance_id).await?;

        let evidence = launch_evidence::LaunchEvidence::read(&self.grill, instance_id).await;
        self.ops
            .finish_fresh_instance(instance_id, app_name, namespace, evidence)
            .await
    }

    /// Change this node's share of an app without touching the replicas
    /// that stay in it (#1111): retire the replicas whose ordinals left,
    /// then start the arriving ones the way a fresh deploy starts its
    /// replicas. Retiring first gives a full node back the room the arriving
    /// replicas need. Returns `Break` when the caller must stop the whole
    /// deploy.
    ///
    /// An admission refused here (no room, say) starts nothing, so the node
    /// still runs `previous`, less any replica that already left, and that's
    /// the spec it records again. The retry then adds the same replicas in
    /// place, rather than seeing its own spec already stored and rolling
    /// every replica, which would need room for all of them twice over
    /// (#971).
    pub(super) async fn change_share_in_place(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        previous: Option<&AppSpec>,
        change: super::scale_in_place::InPlaceChange,
        events: &mpsc::Sender<ApplyEvent>,
    ) -> std::ops::ControlFlow<()> {
        let super::scale_in_place::InPlaceChange { add: count, retire } = change;
        let message = match (count, retire.len()) {
            (0, 0) => format!(
                "{app_name} already runs its replicas (replicas: {})",
                spec.replicas
            ),
            (_, 0) => format!(
                "adding {count} replica(s) of {app_name} beside the running ones (replicas: {})",
                spec.replicas
            ),
            (_, leaving) => format!(
                "moving {app_name}'s share in place: {leaving} replica(s) leave, \
                 {count} arrive (replicas: {})",
                spec.replicas
            ),
        };
        let _ = events.send(ApplyEvent::Progress { message }).await;
        let drain_timeout = crate::meat::deploy_types::DeployConfig::for_app(spec).drain_timeout;
        let mut retired: Vec<InstanceId> = Vec::new();
        for old_id in &retire {
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("stopping instance {}: its replica left this node", old_id.0),
                })
                .await;
            let outcome = match self.retire_old_instance(old_id, drain_timeout).await {
                Ok(()) => match self.ops.finish_retire(old_id).await {
                    // Only other nodes' confirmations are outstanding; the
                    // agent loop finishes the release when they arrive.
                    Err(BunError::ProducerReleasePending { .. }) => {
                        self.ops.defer_retire(old_id).await;
                        Ok(())
                    }
                    other => other,
                },
                Err(error) => Err(error),
            };
            if let Err(error) = outcome {
                self.record_share_left(app_name, namespace, previous, &retired)
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("could not retire {}: {error}", old_id.0),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
            retired.push(old_id.clone());
        }
        let ids = match self
            .ops
            .add_app_replicas(app_name, namespace, spec, count)
            .await
        {
            Ok(ids) => ids,
            Err(error) => {
                self.record_share_left(app_name, namespace, previous, &retired)
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        };
        for id in &ids {
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("creating instance {}", id.0),
                })
                .await;
            // A replica that fails leaves the app short of its count, and not
            // every replica running, so the reconciler's retry rolls it.
            if let Err(error) = self
                .drive_fresh_instance(id, app_name, namespace, spec)
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
            self.ops.provision_identity(app_name, namespace, id).await;
            let _ = events
                .send(ApplyEvent::InstanceCreated {
                    id: id.0.clone(),
                    app: app_name.to_string(),
                })
                .await;
        }
        self.ops
            .push_deploy_history(crate::meat::deploy_types::DeployHistoryEntry {
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
                steps_completed: ids.len(),
                steps_total: ids.len(),
                spec: Some(Box::new(spec.clone())),
            })
            .await;
        std::ops::ControlFlow::Continue(())
    }

    /// Record what the node runs after a share change stopped partway: the
    /// previous share, less the replicas in `retired` that already left.
    async fn record_share_left(
        &self,
        app_name: &str,
        namespace: &str,
        previous: Option<&AppSpec>,
        retired: &[InstanceId],
    ) {
        let Some(previous) = previous else {
            return;
        };
        let running = super::scale_in_place::share_without(previous, retired);
        if let Err(error) = self
            .ops
            .store_deployed_spec(app_name, namespace, &running)
            .await
        {
            eprintln!("deploy: could not record {namespace}/{app_name}'s previous spec: {error}");
        }
    }

    /// Rolling redeploy: start generation-tagged new instances, health check
    /// them off the loop, then retire the old ones. Returns `Break` when the
    /// caller must stop the whole deploy. On new-instance failure it keeps the
    /// old capacity (see [`DeployWorker::abort_rollout`]) and returns `Break`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn rolling_redeploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        previous: Option<&AppSpec>,
        existing: Vec<InstanceId>,
        events: &mpsc::Sender<ApplyEvent>,
        now: Instant,
    ) -> std::ops::ControlFlow<()> {
        let _ = events
            .send(ApplyEvent::Progress {
                message: format!(
                    "rolling redeploy {app_name} ({} existing instance(s))",
                    existing.len()
                ),
            })
            .await;

        let deploy_config = crate::meat::deploy_types::DeployConfig::for_app(spec);

        let deploy_gen = match self.ops.next_deploy_gen(app_name).await {
            Ok(generation) => generation,
            Err(error) => {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        };
        // A new generation keeps the ordinals the leader assigned (#398):
        // `{app}-g{gen}-{ordinal}` replaces `{app}-{ordinal}`.
        let ordinals = spec.replica_ordinals();
        let replica_count = ordinals.len() as u32;

        let mut started = Replacements::default();
        let mut new_failed = false;

        // M7: drive the rollout through `plan_rolling_step` rather than
        // "start everything, then retire everything". The planner decides
        // whether the next move is a replacement or a retirement based on
        // `max_surge` (how far above the target we may go) and
        // `max_unavailable` (how far below), which previously parsed,
        // validated and changed nothing.
        //
        // `retired` tracks how many of `existing` are gone; `finalise_rolling_deploy`
        // is given only what's left, and its own retire loop is an idempotent
        // catch-up for anything the planner didn't reach.
        let mut retired: usize = 0;
        let mut next_replica_index: usize = 0;
        loop {
            if self.report_cancellation(events).await {
                new_failed = true;
                break;
            }
            let step = crate::meat::deploy_types::plan_rolling_step(
                replica_count,
                started.healthy.len() as u32,
                0, // the start path health-waits inline, so nothing is ever pending here
                (existing.len() - retired) as u32,
                deploy_config.max_surge,
                deploy_config.max_unavailable,
            );
            match step {
                crate::meat::deploy_types::RollingStep::Done => break,
                crate::meat::deploy_types::RollingStep::Wait => break,
                crate::meat::deploy_types::RollingStep::Stuck => {
                    // Config validation rejects the only combination that can
                    // produce this, so reaching it means the bounds came from
                    // somewhere that skipped validation. Fail loudly rather
                    // than spin.
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: format!(
                                "rolling deploy cannot progress with max_surge={} and \
                                 max_unavailable={}",
                                deploy_config.max_surge, deploy_config.max_unavailable
                            ),
                        })
                        .await;
                    new_failed = true;
                    break;
                }
                crate::meat::deploy_types::RollingStep::RetireOld => {
                    let old_id = existing[retired].clone();
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("stopping old instance {}", old_id.0),
                        })
                        .await;
                    // Drain and stop the old instance on this spawned deploy
                    // task (M7), then send only the fast bookkeeping to the
                    // command loop — the wait no longer stalls every command.
                    if let Err(error) = self
                        .retire_old_instance(&old_id, deploy_config.drain_timeout)
                        .await
                    {
                        let retention = self
                            .retain_started_replacements(
                                app_name,
                                namespace,
                                spec,
                                &started.healthy,
                                &started.ports,
                                &started.specs,
                            )
                            .await;
                        let _ = events
                            .send(ApplyEvent::Error {
                                message: format!(
                                    "old instance retirement unconfirmed: {error}; {}",
                                    match retention {
                                        Ok(()) =>
                                            "started replacements retained for cleanup".to_string(),
                                        Err(error) => format!(
                                            "could not retain replacement ownership: {error}"
                                        ),
                                    }
                                ),
                            })
                            .await;
                        return std::ops::ControlFlow::Break(());
                    }
                    match self.ops.finish_retire(&old_id).await {
                        // Stopped, drained and withdrawn locally; only other
                        // nodes' confirmations are outstanding. That can take
                        // as long as a lost node's view lease, and starting
                        // another generation wouldn't make it any shorter.
                        Err(BunError::ProducerReleasePending { .. }) => {
                            self.ops.defer_retire(&old_id).await;
                        }
                        Ok(()) => {}
                        Err(error) => {
                            let retention = self
                                .retain_started_replacements(
                                    app_name,
                                    namespace,
                                    spec,
                                    &started.healthy,
                                    &started.ports,
                                    &started.specs,
                                )
                                .await;
                            let detail = match retention {
                                Ok(()) => "started replacements retained for cleanup".into(),
                                Err(error) => {
                                    format!("could not retain replacement ownership: {error}")
                                }
                            };
                            let _ = events
                                .send(ApplyEvent::Error {
                                    message: format!(
                                        "old instance artifact retirement failed: {error}; {detail}"
                                    ),
                                })
                                .await;
                            return std::ops::ControlFlow::Break(());
                        }
                    }
                    retired += 1;
                    continue;
                }
                crate::meat::deploy_types::RollingStep::StartNew => {}
            }

            let ordinal = ordinals[next_replica_index];
            next_replica_index += 1;
            let new_id =
                crate::grill::InstanceIdentity::canary(namespace, app_name, deploy_gen, ordinal)
                    .instance_id();
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("starting new instance {}", new_id.0),
                })
                .await;
            let wait = effective_health_wait(&deploy_config);
            if let Err(message) = self
                .start_replacement(
                    app_name,
                    namespace,
                    spec,
                    &new_id,
                    wait,
                    &mut started,
                    events,
                )
                .await
            {
                let _ = events.send(ApplyEvent::Error { message }).await;
                new_failed = true;
                break;
            }
        }

        if new_failed {
            self.abort_rollout(
                AbortedRollout {
                    app_name,
                    namespace,
                    spec,
                    previous,
                    started: &started,
                    remaining_old: &existing[retired..],
                    retired,
                    replica_count,
                    config: &deploy_config,
                },
                events,
            )
            .await;
            return std::ops::ControlFlow::Break(());
        }

        // Anything the planner didn't reach (it stops once every replacement is
        // healthy, and a scale-down leaves surplus old instances) is retired
        // here. On a default rollout this is empty — the stop-progress lines
        // were already emitted per step above. The drain+stop wait runs on
        // this spawned task (M7); finalise only does the fast bookkeeping.
        let outstanding: Vec<InstanceId> = existing[retired..].to_vec();
        for old_id in &outstanding {
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("stopping old instance {}", old_id.0),
                })
                .await;
            if let Err(error) = self
                .retire_old_instance(old_id, deploy_config.drain_timeout)
                .await
            {
                let retention = self
                    .retain_started_replacements(
                        app_name,
                        namespace,
                        spec,
                        &started.healthy,
                        &started.ports,
                        &started.specs,
                    )
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!(
                            "old instance retirement unconfirmed: {error}; {}",
                            match retention {
                                Ok(()) => "started replacements retained for cleanup".to_string(),
                                Err(error) =>
                                    format!("could not retain replacement ownership: {error}"),
                            }
                        ),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        }

        let new_ids = started.healthy.clone();
        if let Err(error) = self
            .ops
            .finalise_rolling_deploy(
                app_name,
                namespace,
                spec,
                outstanding,
                new_ids.clone(),
                started.ports.clone(),
                started.ips.clone(),
                started.specs.clone(),
                now,
            )
            .await
        {
            let retention = self
                .retain_started_replacements(
                    app_name,
                    namespace,
                    spec,
                    &new_ids,
                    &started.ports,
                    &started.specs,
                )
                .await;
            let detail = match retention {
                Ok(()) => "started replacements retained for cleanup".into(),
                Err(error) => format!("could not retain replacement ownership: {error}"),
            };
            let _ = events
                .send(ApplyEvent::Error {
                    message: format!("rollout finalisation failed: {error}; {detail}"),
                })
                .await;
            return std::ops::ControlFlow::Break(());
        }

        for new_id in &new_ids {
            let _ = events
                .send(ApplyEvent::InstanceCreated {
                    id: new_id.0.clone(),
                    app: app_name.to_string(),
                })
                .await;
        }

        std::ops::ControlFlow::Continue(())
    }

    /// Start one rolling replacement under `new_id` from `spec`, wait for its
    /// health check, and publish it as a backend. Everything it reserves or
    /// learns goes into `started`, so an abort can clean it up. `Err` carries
    /// the reason the replacement didn't come up healthy.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn start_replacement(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        new_id: &InstanceId,
        wait: std::time::Duration,
        started: &mut Replacements,
        events: &mpsc::Sender<ApplyEvent>,
    ) -> Result<(), String> {
        let host_port = self
            .ops
            .reserve_rolling_instance(new_id, app_name, namespace, spec)
            .await
            .map_err(|error| error.to_string())?;

        started.ports.insert(new_id.clone(), host_port);
        started.prepared.push(new_id.clone());
        let oci_spec = self
            .ops
            .prepare_rolling_instance(new_id, app_name, namespace, spec, host_port)
            .await
            .map_err(|error| error.to_string())?;
        let Some(cgroup_path) = oci_spec.linux.host_cgroup_path() else {
            return Err(format!(
                "replacement {} has no valid original cgroup path",
                new_id.0
            ));
        };

        started.runtime_attempted.insert(new_id.clone());
        let target = super::launch::LaunchTarget {
            instance_id: new_id,
            app_name,
            namespace,
        };
        let launch =
            super::launch::create_with_secrets(&self.grill, &self.secrets, target, &oci_spec)
                .await
                .map_err(|error| format!("failed to create {}: {error}", new_id.0))?;
        // Same create → program → start ordering as the fresh path (#86).
        self.prepare_network(new_id, app_name, Some(spec), &cgroup_path)
            .await
            .map_err(|error| format!("failed to program egress for {}: {error}", new_id.0))?;
        let initialised = async {
            self.drive_initialisers(new_id, app_name, spec, &launch, &cgroup_path)
                .await?;
            self.drive_sidecars(new_id, app_name, namespace, spec, &oci_spec)
                .await?;
            self.ops
                .transition_state(new_id, ContainerState::Starting)
                .await
        }
        .await;
        initialised.map_err(|error| format!("failed to initialise {}: {error}", new_id.0))?;
        self.grill
            .start(new_id)
            .await
            .map_err(|error| format!("failed to start {}: {error}", new_id.0))?;
        let launch = launch_evidence::LaunchEvidence::read(&self.grill, new_id).await;
        self.ops
            .register_rolling_instance(RollingInstance {
                instance_id: new_id.clone(),
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: spec.clone(),
                oci_spec: oci_spec.clone(),
                host_port,
                launch,
            })
            .await
            .map_err(|error| error.to_string())?;

        let container_ip = self.grill.container_ip(new_id).await;

        // Health wait, off the command loop (this runs on the spawned
        // per-deploy task, so the full configured `health_timeout` is
        // honoured — M7). Waits for Running, then for the app's own HTTP
        // probe to pass (M5): a replacement is only announced healthy —
        // and only published as a backend below — once it answers the
        // health check the operator configured, not merely because its
        // process came up.
        self.wait_for_deploy_health(new_id, spec, container_ip, wait)
            .await?;
        let _ = events
            .send(ApplyEvent::Progress {
                message: format!("{} healthy ✓", new_id.0),
            })
            .await;
        self.ops
            .provision_identity(app_name, namespace, new_id)
            .await;

        started.specs.insert(new_id.clone(), oci_spec);
        started.ips.insert(new_id.clone(), container_ip);
        // DEP5/M7: route traffic onto the replacement the moment it's
        // healthy, before the planner is allowed to retire anything. With
        // `max_unavailable = 0` this is what makes the guarantee real —
        // retiring first and publishing later would leave a gap however
        // carefully the counts were tracked.
        self.ops
            .publish_new_backend(
                app_name,
                namespace,
                new_id,
                host_port,
                container_ip,
                spec.port.is_some(),
            )
            .await
            .map_err(|error| error.to_string())?;
        started.healthy.push(new_id.clone());
        Ok(())
    }

    /// Clean up after a rollout that failed part-way, without ending below
    /// the capacity it started from.
    ///
    /// Replacements that never went healthy are always torn down. What
    /// happens to the healthy ones depends on `auto_rollback`:
    ///
    /// - Off: they keep serving beside the old instances (`Halted`).
    /// - On, nothing retired yet: the old instances are all still there, so
    ///   the replacements go (`RolledBack`).
    /// - On, some old instances already retired: killing the replacements
    ///   would leave the app short, so the retired capacity comes back first,
    ///   on the previous spec, and only then do the replacements go. If the
    ///   previous spec can't come back, the healthy replacements stay and the
    ///   result is `Halted` with the reason (#692).
    pub(super) async fn abort_rollout(
        &self,
        rollout: AbortedRollout<'_>,
        events: &mpsc::Sender<ApplyEvent>,
    ) {
        let AbortedRollout {
            app_name,
            namespace,
            spec,
            previous,
            started,
            remaining_old,
            retired,
            replica_count,
            config,
        } = rollout;
        let auto_rollback = config.auto_rollback;
        let restore = auto_rollback && retired > 0;
        let keep_healthy = !auto_rollback || restore;
        let mut errors = Vec::new();
        if keep_healthy
            && let Err(error) = self
                .retain_started_replacements(
                    app_name,
                    namespace,
                    spec,
                    &started.healthy,
                    &started.ports,
                    &started.specs,
                )
                .await
        {
            errors.push(error.to_string());
        }
        for id in &started.prepared {
            if keep_healthy && started.healthy.contains(id) {
                continue;
            }
            if let Err(error) = self
                .discard_replacement(id, started.runtime_attempted.contains(id))
                .await
            {
                errors.push(format!("{id}: {error}"));
            }
        }
        let restoration = if restore && errors.is_empty() {
            Some(self.restore_previous_capacity(&rollout, events).await)
        } else {
            None
        };
        let (result, message) = if !errors.is_empty() {
            (
                crate::meat::deploy_types::DeployResult::Failed,
                format!(
                    "rollout cleanup incomplete; remaining owners retained: {}",
                    errors.join("; ")
                ),
            )
        } else if auto_rollback && retired == 0 {
            (
                crate::meat::deploy_types::DeployResult::RolledBack,
                "rolled back — old instances preserved".to_string(),
            )
        } else if let Some(restoration) = restoration {
            match restoration {
                Ok(restored) => (
                    crate::meat::deploy_types::DeployResult::RolledBack,
                    format!(
                        "rolled back — {restored} instance(s) restarted on the previous spec \
                         beside {} old instance(s)",
                        remaining_old.len()
                    ),
                ),
                Err(reason) => (
                    crate::meat::deploy_types::DeployResult::Halted,
                    format!(
                        "deploy halted: could not restore the previous spec ({reason}); \
                         healthy new instances left running beside {} old instance(s)",
                        remaining_old.len()
                    ),
                ),
            }
        } else {
            (
                crate::meat::deploy_types::DeployResult::Halted,
                format!(
                    "deploy halted: {} healthy new instance(s) left running; {retired} old instance(s) already retired",
                    if auto_rollback {
                        0
                    } else {
                        started.healthy.len()
                    }
                ),
            )
        };
        // Every instance runs the previous spec again, so that's the spec
        // this node runs: what a restart, a report or the next rollout reads.
        if result == crate::meat::deploy_types::DeployResult::RolledBack
            && let Some(previous) = previous
            && let Err(error) = self
                .ops
                .store_deployed_spec(app_name, namespace, previous)
                .await
        {
            eprintln!("deploy: could not record {namespace}/{app_name}'s previous spec: {error}");
        }
        self.ops
            .push_deploy_history(crate::meat::deploy_types::DeployHistoryEntry {
                id: crate::meat::deploy_types::DeployId(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                ),
                app_id: crate::meat::types::AppId::new(app_name, namespace),
                image: spec.image.clone().unwrap_or_default(),
                result,
                created_at: SystemTime::now(),
                completed_at: SystemTime::now(),
                steps_completed: started.healthy.len(),
                steps_total: replica_count as usize,
                spec: Some(Box::new(spec.clone())),
            })
            .await;
        // An operator's cancellation isn't evidence that the spec is bad.
        let cancelled = self
            .operation
            .as_ref()
            .is_some_and(|operation| operation.cancellation_requested());
        if !cancelled {
            let _ = events
                .send(ApplyEvent::RolloutAborted {
                    app: app_name.to_string(),
                    rolled_back: result == crate::meat::deploy_types::DeployResult::RolledBack,
                    reason: message.clone(),
                })
                .await;
        }
        let _ = events.send(ApplyEvent::Error { message }).await;
    }

    /// Bring an aborted rollout back to its old capacity on the previous
    /// spec: start previous-spec instances for the ordinals no old instance
    /// holds, retiring a healthy replacement only once one has come up, in
    /// the same surge and unavailable budget as the rollout. Answers how many
    /// previous-spec instances now run, or why it stopped.
    async fn restore_previous_capacity(
        &self,
        rollout: &AbortedRollout<'_>,
        events: &mpsc::Sender<ApplyEvent>,
    ) -> Result<usize, String> {
        let Some(previous) = rollout.previous else {
            return Err("this node doesn't know the spec its old instances ran".into());
        };
        let ordinal_of = |id: &InstanceId| {
            crate::grill::InstanceIdentity::parse(&id.0).map(|identity| identity.ordinal)
        };
        let held: std::collections::HashSet<u32> = rollout
            .remaining_old
            .iter()
            .filter_map(ordinal_of)
            .collect();
        // Ordinals no remaining old instance holds: first the ones nothing
        // runs (their old instance retired and its replacement failed), then
        // the ones a healthy replacement holds, which it gives back.
        let mut ordinals: Vec<u32> = rollout
            .spec
            .replica_ordinals()
            .into_iter()
            .filter(|ordinal| !held.contains(ordinal))
            .collect();
        ordinals.sort_by_key(|ordinal| {
            rollout
                .started
                .healthy
                .iter()
                .any(|id| ordinal_of(id) == Some(*ordinal))
        });
        let deploy_gen = self
            .ops
            .next_deploy_gen(rollout.app_name)
            .await
            .map_err(|error| error.to_string())?;
        let wait =
            effective_health_wait(&crate::meat::deploy_types::DeployConfig::for_app(previous));
        let mut restored = Replacements::default();
        let mut to_retire: std::collections::VecDeque<InstanceId> =
            rollout.started.healthy.iter().cloned().collect();
        let mut next_ordinal = ordinals.iter();
        let outcome: Result<(), String> = loop {
            let step = crate::meat::deploy_types::plan_rolling_step(
                ordinals.len() as u32,
                restored.healthy.len() as u32,
                0,
                to_retire.len() as u32,
                rollout.config.max_surge,
                rollout.config.max_unavailable,
            );
            match step {
                crate::meat::deploy_types::RollingStep::Done => break Ok(()),
                crate::meat::deploy_types::RollingStep::Wait
                | crate::meat::deploy_types::RollingStep::Stuck => {
                    break Err("the rollout's surge and unavailable bounds leave no move".into());
                }
                crate::meat::deploy_types::RollingStep::RetireOld => {
                    let Some(id) = to_retire.pop_front() else {
                        break Err("no replacement left to retire".into());
                    };
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("stopping new instance {}", id.0),
                        })
                        .await;
                    if let Err(error) = self
                        .retire_old_instance(&id, rollout.config.drain_timeout)
                        .await
                    {
                        break Err(format!("could not stop {id}: {error}"));
                    }
                    match self.ops.finish_retire(&id).await {
                        Err(BunError::ProducerReleasePending { .. }) => {
                            self.ops.defer_retire(&id).await;
                        }
                        Ok(()) => {}
                        Err(error) => break Err(format!("could not retire {id}: {error}")),
                    }
                }
                crate::meat::deploy_types::RollingStep::StartNew => {
                    let Some(&ordinal) = next_ordinal.next() else {
                        break Err("no ordinal left to restore".into());
                    };
                    let id = crate::grill::InstanceIdentity::canary(
                        rollout.namespace,
                        rollout.app_name,
                        deploy_gen,
                        ordinal,
                    )
                    .instance_id();
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("restarting the previous spec as {}", id.0),
                        })
                        .await;
                    if let Err(reason) = self
                        .start_replacement(
                            rollout.app_name,
                            rollout.namespace,
                            previous,
                            &id,
                            wait,
                            &mut restored,
                            events,
                        )
                        .await
                    {
                        break Err(reason);
                    }
                }
            }
        };
        // Restored instances are owned like any retained replacement, on the
        // spec they run. Ones that never came up go.
        let retained = self
            .retain_started_replacements(
                rollout.app_name,
                rollout.namespace,
                previous,
                &restored.healthy,
                &restored.ports,
                &restored.specs,
            )
            .await;
        let mut errors = Vec::new();
        for id in &restored.prepared {
            if restored.healthy.contains(id) {
                continue;
            }
            if let Err(error) = self
                .discard_replacement(id, restored.runtime_attempted.contains(id))
                .await
            {
                errors.push(format!("{id}: {error}"));
            }
        }
        outcome?;
        retained.map_err(|error| error.to_string())?;
        if !errors.is_empty() {
            return Err(format!(
                "failed restorations retained for cleanup: {}",
                errors.join("; ")
            ));
        }
        Ok(restored.healthy.len())
    }

    /// Stop and forget a replacement that never served. A reservation that
    /// never attempted a runtime create has nothing in the runtime to kill.
    async fn discard_replacement(
        &self,
        id: &InstanceId,
        runtime_attempted: bool,
    ) -> Result<(), BunError> {
        if let Some(restart) = self.ops.begin_retire(id).await?.restart {
            restart.settle(self.stop_confirmation_timeout).await?;
        }
        // A failed create may already own runtime resources. Only a
        // reservation that never attempted create proves their absence.
        if runtime_attempted {
            kill_runtime_instance(&self.grill, id, self.stop_confirmation_timeout).await?;
        }
        self.ops.finish_retire(id).await
    }

    /// Publish retirement intent before runtime exit can trigger the restart driver.
    pub(super) async fn retire_old_instance(
        &self,
        id: &InstanceId,
        drain_timeout: std::time::Duration,
    ) -> Result<(), BunError> {
        let begun = self.ops.begin_retire(id).await?;
        if let Some(restart) = begun.restart {
            restart.settle(self.stop_confirmation_timeout).await?;
        }
        // Without its own grace the old instance gets the drain timeout, as
        // it always has; with one, the drain and the grace are separate.
        let stop = InstanceStop::for_policy(begun.stop.as_ref(), drain_timeout)
            .with_sidecars(begun.sidecars);
        drain_and_stop_instance(
            &self.drains,
            &self.grill,
            id,
            drain_timeout,
            &stop,
            self.stop_confirmation_timeout,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn retain_started_replacements(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        ids: &[InstanceId],
        ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        specs: &std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
    ) -> Result<(), BunError> {
        for id in ids {
            let oci_spec = specs.get(id).ok_or_else(|| BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!("missing launch ownership for {id}"),
            })?;
            let launch = launch_evidence::LaunchEvidence::read(&self.grill, id).await;
            self.ops
                .retain_rolling_instance(RollingInstance {
                    instance_id: id.clone(),
                    app_name: app_name.into(),
                    namespace: namespace.into(),
                    spec: spec.clone(),
                    oci_spec: oci_spec.clone(),
                    host_port: ports.get(id).copied().flatten(),
                    launch,
                })
                .await?;
        }
        Ok(())
    }

    /// Blue-green redeploy: start the whole new ("green") fleet in parallel to
    /// the old ("blue") one, health check every green instance, and only then
    /// swap routing over and retire all of blue at once. Blue keeps serving the
    /// entire time green is coming up, so a failure anywhere in green tears the
    /// green fleet down and leaves blue untouched. Returns `Break` when the
    /// caller must stop the whole deploy.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn blue_green_redeploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        previous: Option<&AppSpec>,
        existing: Vec<InstanceId>,
        events: &mpsc::Sender<ApplyEvent>,
        now: Instant,
    ) -> std::ops::ControlFlow<()> {
        let _ = events
            .send(ApplyEvent::Progress {
                message: format!(
                    "blue-green redeploy {app_name} ({} blue instance(s))",
                    existing.len()
                ),
            })
            .await;

        let deploy_config = spec
            .deploy
            .as_ref()
            .map(crate::meat::deploy_types::DeployConfig::from_spec)
            .unwrap_or_default();
        let deploy_gen = match self.ops.next_deploy_gen(app_name).await {
            Ok(generation) => generation,
            Err(error) => {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        };
        // A new generation keeps the ordinals the leader assigned (#398):
        // `{app}-g{gen}-{ordinal}` replaces `{app}-{ordinal}`.
        let ordinals = spec.replica_ordinals();
        let replica_count = ordinals.len() as u32;

        let mut green = Replacements::default();
        let Replacements {
            healthy: new_ids,
            ports: new_ports,
            specs: new_specs,
            ips: new_ips,
            prepared: new_prepared,
            runtime_attempted,
        } = &mut green;
        let mut new_failed = false;

        // Start and health check the entire green fleet before touching blue.
        // Unlike the rolling planner, nothing retires here and nothing is
        // published to routing yet: green comes up dark, alongside blue.
        for &ordinal in &ordinals {
            if self.report_cancellation(events).await {
                new_failed = true;
                break;
            }
            let new_id =
                crate::grill::InstanceIdentity::canary(namespace, app_name, deploy_gen, ordinal)
                    .instance_id();
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("starting green instance {}", new_id.0),
                })
                .await;

            let host_port = match self
                .ops
                .reserve_rolling_instance(&new_id, app_name, namespace, spec)
                .await
            {
                Ok(port) => port,
                Err(error) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    new_failed = true;
                    break;
                }
            };

            new_ports.insert(new_id.clone(), host_port);
            new_prepared.push(new_id.clone());
            let oci_spec = match self
                .ops
                .prepare_rolling_instance(&new_id, app_name, namespace, spec, host_port)
                .await
            {
                Ok(oci_spec) => oci_spec,
                Err(e) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    new_failed = true;
                    break;
                }
            };
            let Some(cgroup_path) = oci_spec.linux.host_cgroup_path() else {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!(
                            "replacement {} has no valid original cgroup path",
                            new_id.0
                        ),
                    })
                    .await;
                new_failed = true;
                break;
            };

            runtime_attempted.insert(new_id.clone());
            let target = super::launch::LaunchTarget {
                instance_id: &new_id,
                app_name,
                namespace,
            };
            if let Err(e) =
                super::launch::create_with_secrets(&self.grill, &self.secrets, target, &oci_spec)
                    .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to create {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            if let Err(e) = self
                .prepare_network(&new_id, app_name, Some(spec), &cgroup_path)
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to program egress for {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            if let Err(e) = self.grill.start(&new_id).await {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to start {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            let launch = launch_evidence::LaunchEvidence::read(&self.grill, &new_id).await;
            if let Err(error) = self
                .ops
                .register_rolling_instance(RollingInstance {
                    instance_id: new_id.clone(),
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    spec: spec.clone(),
                    oci_spec: oci_spec.clone(),
                    host_port,
                    launch,
                })
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                new_failed = true;
                break;
            }

            let container_ip = self.grill.container_ip(&new_id).await;

            // Same M5 gate as the rolling path: green only counts as healthy
            // once its configured HTTP probe passes, not merely on Running —
            // otherwise a green fleet that starts but can't serve replaces a
            // blue fleet that can.
            let wait = effective_health_wait(&deploy_config);
            match self
                .wait_for_deploy_health(&new_id, spec, container_ip, wait)
                .await
            {
                Ok(()) => {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("{} healthy ✓", new_id.0),
                        })
                        .await;
                    self.ops
                        .provision_identity(app_name, namespace, &new_id)
                        .await;
                }
                Err(message) => {
                    let _ = events.send(ApplyEvent::Error { message }).await;
                    new_failed = true;
                    break;
                }
            }

            new_ports.insert(new_id.clone(), host_port);
            new_specs.insert(new_id.clone(), oci_spec);
            new_ips.insert(new_id.clone(), container_ip);
            new_ids.push(new_id);
        }

        if !new_failed && self.report_cancellation(events).await {
            new_failed = true;
        }
        // Nothing of blue has retired yet, so an abort only tears green down.
        let aborted = |green| AbortedRollout {
            app_name,
            namespace,
            spec,
            previous,
            started: green,
            remaining_old: &existing,
            retired: 0,
            replica_count,
            config: &deploy_config,
        };
        if new_failed {
            self.abort_rollout(aborted(&green), events).await;
            return std::ops::ControlFlow::Break(());
        }
        let Replacements {
            healthy: new_ids,
            ports: new_ports,
            specs: new_specs,
            ips: new_ips,
            ..
        } = &green;

        // The whole green fleet is healthy. Cut over: publish every green
        // backend so routing picks them up while blue still serves, then
        // drain and stop blue on this spawned task (M7 — the bulk drain used
        // to run inside finalise on the command loop, freezing every agent
        // command for up to fleet-size × drain_timeout), and finally send the
        // fast bookkeeping to the loop.
        for new_id in new_ids {
            if let Err(error) = self
                .ops
                .publish_new_backend(
                    app_name,
                    namespace,
                    new_id,
                    new_ports.get(new_id).copied().flatten(),
                    new_ips.get(new_id).copied().flatten(),
                    spec.port.is_some(),
                )
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                self.abort_rollout(aborted(&green), events).await;
                return std::ops::ControlFlow::Break(());
            }
        }
        for old_id in &existing {
            if let Err(error) = self
                .retire_old_instance(old_id, deploy_config.drain_timeout)
                .await
            {
                let retention = self
                    .retain_started_replacements(
                        app_name, namespace, spec, new_ids, new_ports, new_specs,
                    )
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!(
                            "old instance retirement unconfirmed: {error}; {}",
                            match retention {
                                Ok(()) => "started replacements retained for cleanup".to_string(),
                                Err(error) =>
                                    format!("could not retain replacement ownership: {error}"),
                            }
                        ),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        }
        if let Err(error) = self
            .ops
            .finalise_rolling_deploy(
                app_name,
                namespace,
                spec,
                existing,
                new_ids.clone(),
                new_ports.clone(),
                new_ips.clone(),
                new_specs.clone(),
                now,
            )
            .await
        {
            let retention = self
                .retain_started_replacements(
                    app_name, namespace, spec, new_ids, new_ports, new_specs,
                )
                .await;
            let detail = match retention {
                Ok(()) => "started replacements retained for cleanup".into(),
                Err(error) => format!("could not retain replacement ownership: {error}"),
            };
            let _ = events
                .send(ApplyEvent::Error {
                    message: format!("rollout finalisation failed: {error}; {detail}"),
                })
                .await;
            return std::ops::ControlFlow::Break(());
        }

        for new_id in new_ids {
            let _ = events
                .send(ApplyEvent::InstanceCreated {
                    id: new_id.0.clone(),
                    app: app_name.to_string(),
                })
                .await;
        }

        std::ops::ControlFlow::Continue(())
    }
}

/// The health-wait deadline for a rolling redeploy: the configured
/// `health_timeout`, uncapped (M7).
///
/// The rolling redeploy runs on a spawned per-deploy task, so a long wait
/// doesn't stall the command loop; the previous `.min(5s)` cap silently
/// clamped a configured 60s timeout to 5s and rolled back any container slower
/// than that to become healthy.
pub(super) fn effective_health_wait(
    config: &crate::meat::deploy_types::DeployConfig,
) -> std::time::Duration {
    config.health_timeout
}
