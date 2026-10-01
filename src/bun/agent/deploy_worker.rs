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
    pub(super) rerun_unknown_jobs: bool,
    pub(super) grill: G,
    pub(super) ops: DeployOps,
    /// Shared drain tracker, so the worker can drain-and-stop a retiring
    /// instance off the command loop (M7) rather than sending the whole wait
    /// to the loop as an op.
    pub(super) drains: crate::wrapper::draining::SharedDrains,
    pub(super) operation: Option<crate::bun::deploy_operations::DeployOperationHandle>,
    /// The agent's `[runtime] stop_confirmation_timeout_secs`.
    pub(super) stop_confirmation_timeout: std::time::Duration,
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
        let result = self
            .ops
            .apply_network_pre_start(instance_id, app_name, spec, cgroup_path, retained)
            .await;
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

    /// Deploy all apps and jobs from a config, streaming progress events. The
    /// mirror of the former `BunAgent::deploy`, but off the command loop.
    pub(super) async fn run_deploy(self, config: Config, events: mpsc::Sender<ApplyEvent>) {
        if self.report_cancellation(&events).await {
            return;
        }
        let now = Instant::now();
        let mut all_ids: Vec<String> = Vec::new();
        // Jobs already run as `run_before` prerequisites, so the regular jobs
        // loop below doesn't run them a second time.
        let mut ran_prereqs: std::collections::HashSet<String> = std::collections::HashSet::new();
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

            // run_before (E): jobs declaring `run_before = ["app.<name>"]` must
            // run to completion before this app's deploy begins — migrations are
            // the classic case. A prerequisite failure aborts the whole deploy.
            let target = format!("app.{app_name}");
            for (job_name, job_spec) in &config.job {
                // Cron-scheduled jobs fire on their schedule, never as a
                // deploy-time prerequisite.
                if ran_prereqs.contains(job_name)
                    || job_spec.schedule.is_some()
                    || !job_spec.run_before.contains(&target)
                {
                    continue;
                }
                let job_ns = job_spec.namespace.as_deref().unwrap_or("default");
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!(
                            "running prerequisite job {job_name} before app {app_name}"
                        ),
                    })
                    .await;
                if let Err(e) = self.run_prerequisite_job(job_name, job_ns, job_spec).await {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
                ran_prereqs.insert(job_name.clone());
            }
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

            // Gate on image signature first (IMG1). A verified image comes back
            // pinned to its manifest digest; the pinned spec shadows the
            // original for the rest of this iteration.
            let pinned_spec;
            let spec = match self.ops.enforce_image_signature(spec).await {
                Ok(None) => spec,
                Ok(Some(pinned_image)) => {
                    let mut with_pin = spec.clone();
                    with_pin.image = Some(pinned_image);
                    pinned_spec = with_pin;
                    &pinned_spec
                }
                Err(reason) => {
                    let _ = events.send(ApplyEvent::Error { message: reason }).await;
                    return;
                }
            };

            // Asked before the new spec replaces the one the replicas run.
            let in_place = self
                .ops
                .replicas_to_add_in_place(app_name, namespace, spec)
                .await;
            if let Err(error) = self
                .ops
                .store_deployed_spec(app_name, namespace, spec)
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                return;
            }

            let existing = self.ops.list_existing_owned(app_name, namespace).await;

            if let Some(count) = in_place.filter(|_| !existing.is_empty()) {
                if self
                    .add_replicas_in_place(app_name, namespace, spec, count, &events)
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
                        self.blue_green_redeploy(app_name, namespace, spec, existing, &events, now)
                            .await
                    }
                    crate::meat::deploy_types::DeployStrategy::Rolling => {
                        self.rolling_redeploy(app_name, namespace, spec, existing, &events, now)
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

                self.ops
                    .provision_identity(app_name, namespace, id, false)
                    .await;

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

        if !config.job.is_empty()
            && let Some(operation) = &self.operation
        {
            operation
                .advance(
                    crate::bun::deploy_operations::DeployOperationPhase::DeployingJobs,
                    None,
                    format!("deploying {} job(s)", config.job.len()),
                )
                .await;
        }

        for (job_name, spec) in &config.job {
            if self.report_cancellation(&events).await {
                return;
            }
            // Already run to completion as a run_before prerequisite above, or a
            // cron-scheduled job that fires on its schedule rather than now.
            if ran_prereqs.contains(job_name) || spec.schedule.is_some() {
                continue;
            }
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            if let Some(operation) = &self.operation {
                operation
                    .advance(
                        crate::bun::deploy_operations::DeployOperationPhase::DeployingJobs,
                        Some(crate::bun::deploy_operations::DeployTarget {
                            kind: crate::bun::deploy_operations::DeployTargetKind::Job,
                            name: job_name.clone(),
                            namespace: namespace.to_string(),
                        }),
                        format!("deploying job {namespace}/{job_name}"),
                    )
                    .await;
            }
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("deploying job {job_name}"),
                })
                .await;

            let ids = match self
                .ops
                .supervisor_deploy_job(job_name, namespace, spec, self.rerun_unknown_jobs)
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

            for id in &ids {
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!("creating instance {}", id.0),
                    })
                    .await;

                if let Err(e) = self.drive_job(id, job_name, namespace, spec).await {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }

                let _ = events
                    .send(ApplyEvent::InstanceCreated {
                        id: id.0.clone(),
                        app: job_name.to_string(),
                    })
                    .await;
            }

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

    /// Run the same owned init chain for fresh and rolling replacements.
    pub(super) async fn drive_initialisers(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
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
            let init_oci = crate::grill::oci::generate_init_oci_spec(
                &init_spec.command,
                namespace,
                app_name,
                spec.image.as_deref(),
                &cgroup_path.to_string_lossy(),
                None,
            );
            self.grill.create(&init_id, &init_oci).await?;
            self.grill.start(&init_id).await?;

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

        // The image pull happens here, off the loop.
        self.grill.create(instance_id, &prepared.oci_spec).await?;
        self.ops
            .store_oci_spec(instance_id, prepared.oci_spec)
            .await;

        // create → program → start: the workload never runs ahead of its
        // egress policy (#86). On failure the loop stops the container.
        self.prepare_network(instance_id, app_name, Some(spec), &prepared.cgroup_path)
            .await?;

        if prepared.has_init {
            self.drive_initialisers(
                instance_id,
                app_name,
                namespace,
                spec,
                &prepared.cgroup_path,
            )
            .await?;
        }

        self.ops
            .transition_state(instance_id, ContainerState::Starting)
            .await?;
        self.grill.start(instance_id).await?;

        let evidence = launch_evidence::LaunchEvidence::read(&self.grill, instance_id).await;
        self.ops
            .finish_fresh_instance(instance_id, app_name, namespace, evidence)
            .await
    }

    /// Drive a job through create → source policy → start → Running.
    /// Jobs have no external allowlist or health checks.
    pub(super) async fn drive_job(
        &self,
        instance_id: &InstanceId,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
    ) -> Result<(), BunError> {
        self.ops
            .transition_state(instance_id, ContainerState::Preparing)
            .await?;

        let cgroup_path =
            crate::grill::cgroup::instance_cgroup_path(namespace, job_name, instance_id)?;
        let cgroup_str = cgroup_path.to_string_lossy();
        let oci_spec = generate_job_oci_spec(job_name, namespace, spec, &cgroup_str, None);

        self.grill.create(instance_id, &oci_spec).await?;
        self.ops.store_oci_spec(instance_id, oci_spec.clone()).await;
        self.prepare_network(instance_id, job_name, None, &cgroup_path)
            .await?;
        self.ops
            .transition_state(instance_id, ContainerState::Starting)
            .await?;
        self.grill.start(instance_id).await?;
        let evidence = launch_evidence::LaunchEvidence::read(&self.grill, instance_id).await;
        self.ops
            .finish_job_instance(instance_id, job_name, namespace, oci_spec, evidence)
            .await
    }

    /// Run a `run_before` prerequisite job to completion for dependency
    /// ordering. Deploys the job, then polls the runtime until every instance
    /// exits. Returns `Ok(())` only when all instances exit cleanly (code 0);
    /// a non-zero exit or a timeout is an error that aborts the gated deploy.
    pub(super) async fn run_prerequisite_job(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
    ) -> Result<(), BunError> {
        let ids = self
            .ops
            .supervisor_deploy_job(job_name, namespace, spec, self.rerun_unknown_jobs)
            .await?;
        for id in &ids {
            self.drive_job(id, job_name, namespace, spec).await?;

            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(RUN_BEFORE_TIMEOUT_SECS);
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let state = self.grill.state(id).await?;
                if state == ContainerState::Stopped {
                    let exit_code = self.grill.exit_code(id).await?;
                    if exit_code == Some(0) {
                        self.ops.confirm_job_success(id).await?;
                        break;
                    }
                    return Err(BunError::DeployFailed {
                        app_name: job_name.to_string(),
                        reason: format!(
                            "run_before job exited with {}",
                            exit_code
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "unknown status".to_string())
                        ),
                    });
                }
                if std::time::Instant::now() >= deadline {
                    let _ = self.grill.kill(id).await;
                    return Err(BunError::DeployFailed {
                        app_name: job_name.to_string(),
                        reason: format!(
                            "run_before job timed out after {RUN_BEFORE_TIMEOUT_SECS}s"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Start `count` more replicas beside the ones an app already runs, the
    /// way a fresh deploy starts its replicas. The running ones aren't
    /// touched. Returns `Break` when the caller must stop the whole deploy.
    pub(super) async fn add_replicas_in_place(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        count: u32,
        events: &mpsc::Sender<ApplyEvent>,
    ) -> std::ops::ControlFlow<()> {
        let _ = events
            .send(ApplyEvent::Progress {
                message: format!(
                    "adding {count} replica(s) of {app_name} beside the running ones (replicas: {})",
                    spec.replicas
                ),
            })
            .await;
        let ids = match self
            .ops
            .add_app_replicas(app_name, namespace, spec, count)
            .await
        {
            Ok(ids) => ids,
            Err(error) => {
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
            self.ops
                .provision_identity(app_name, namespace, id, false)
                .await;
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

    /// Rolling redeploy: start generation-tagged new instances, health check
    /// them off the loop, then retire the old ones. Returns `Break` when the
    /// caller must stop the whole deploy. On new-instance failure it keeps the
    /// old instances and returns `Continue`.
    pub(super) async fn rolling_redeploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
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
        let replica_count = match spec.replicas {
            crate::config::types::Replicas::Fixed(n) => n,
            crate::config::types::Replicas::DaemonSet => 1,
        };

        let mut new_ids: Vec<InstanceId> = Vec::new();
        let mut new_ports: std::collections::HashMap<InstanceId, Option<u16>> =
            std::collections::HashMap::new();
        let mut new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec> =
            std::collections::HashMap::new();
        let mut new_ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>> =
            std::collections::HashMap::new();
        let mut new_prepared: Vec<InstanceId> = Vec::new();
        let mut runtime_attempted = std::collections::HashSet::new();
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
        let mut next_replica_index: u32 = 0;
        loop {
            if self.report_cancellation(events).await {
                new_failed = true;
                break;
            }
            let step = crate::meat::deploy_types::plan_rolling_step(
                replica_count,
                new_ids.len() as u32,
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
                                app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
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
                                    app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
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

            let i = next_replica_index;
            next_replica_index += 1;
            let new_id = crate::grill::InstanceIdentity::canary(namespace, app_name, deploy_gen, i)
                .instance_id();
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("starting new instance {}", new_id.0),
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
            if let Err(e) = self.grill.create(&new_id, &oci_spec).await {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to create {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            // Same create → program → start ordering as the fresh path (#86).
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
            let initialised = async {
                self.drive_initialisers(&new_id, app_name, namespace, spec, &cgroup_path)
                    .await?;
                self.ops
                    .transition_state(&new_id, ContainerState::Starting)
                    .await
            }
            .await;
            if let Err(error) = initialised {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to initialise {}: {error}", new_id.0),
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

            // Health wait, off the command loop (this runs on the spawned
            // per-deploy task, so the full configured `health_timeout` is
            // honoured — M7). Waits for Running, then for the app's own HTTP
            // probe to pass (M5): a replacement is only announced healthy —
            // and only published as a backend below — once it answers the
            // health check the operator configured, not merely because its
            // process came up.
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
                        .provision_identity(app_name, namespace, &new_id, false)
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
            // DEP5/M7: route traffic onto the replacement the moment it's
            // healthy, before the planner is allowed to retire anything. With
            // `max_unavailable = 0` this is what makes the guarantee real —
            // retiring first and publishing later would leave a gap however
            // carefully the counts were tracked.
            if let Err(error) = self
                .ops
                .publish_new_backend(
                    app_name,
                    namespace,
                    &new_id,
                    host_port,
                    container_ip,
                    spec.port.is_some(),
                )
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
            new_ids.push(new_id);
        }

        if new_failed {
            self.abort_rollout(
                app_name,
                namespace,
                spec,
                &new_ids,
                &new_prepared,
                &runtime_attempted,
                &new_ports,
                &new_specs,
                deploy_config.auto_rollback,
                retired,
                replica_count,
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
                        app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
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
                outstanding,
                new_ids.clone(),
                new_ports.clone(),
                new_ips,
                new_specs.clone(),
                now,
            )
            .await
        {
            let retention = self
                .retain_started_replacements(
                    app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
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

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn abort_rollout(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        healthy: &[InstanceId],
        prepared: &[InstanceId],
        runtime_attempted: &std::collections::HashSet<InstanceId>,
        ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        specs: &std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
        auto_rollback: bool,
        retired: usize,
        replica_count: u32,
        events: &mpsc::Sender<ApplyEvent>,
    ) {
        let mut errors = Vec::new();
        if !auto_rollback
            && let Err(error) = self
                .retain_started_replacements(app_name, namespace, spec, healthy, ports, specs)
                .await
        {
            errors.push(error.to_string());
        }
        for id in prepared {
            if !auto_rollback && healthy.contains(id) {
                continue;
            }
            let cleanup = async {
                if let Some(restart) = self.ops.begin_retire(id).await? {
                    restart.settle(self.stop_confirmation_timeout).await?;
                }
                // A failed create may already own runtime resources. Only a
                // reservation that never attempted create proves their absence.
                if runtime_attempted.contains(id) {
                    kill_runtime_instance(&self.grill, id, self.stop_confirmation_timeout).await?;
                }
                self.ops.finish_retire(id).await
            }
            .await;
            if let Err(error) = cleanup {
                errors.push(format!("{id}: {error}"));
            }
        }
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
        } else {
            (
                crate::meat::deploy_types::DeployResult::Halted,
                format!(
                    "deploy halted: {} healthy new instance(s) left running; {retired} old instance(s) already retired",
                    if auto_rollback { 0 } else { healthy.len() }
                ),
            )
        };
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
                steps_completed: healthy.len(),
                steps_total: replica_count as usize,
                spec: Some(Box::new(spec.clone())),
            })
            .await;
        let _ = events.send(ApplyEvent::Error { message }).await;
    }

    /// Publish retirement intent before runtime exit can trigger the restart driver.
    pub(super) async fn retire_old_instance(
        &self,
        id: &InstanceId,
        drain_timeout: std::time::Duration,
    ) -> Result<(), BunError> {
        if let Some(restart) = self.ops.begin_retire(id).await? {
            restart.settle(self.stop_confirmation_timeout).await?;
        }
        drain_and_stop_instance(
            &self.drains,
            &self.grill,
            id,
            drain_timeout,
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
    pub(super) async fn blue_green_redeploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
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
        let replica_count = match spec.replicas {
            crate::config::types::Replicas::Fixed(n) => n,
            crate::config::types::Replicas::DaemonSet => 1,
        };

        let mut new_ids: Vec<InstanceId> = Vec::new();
        let mut new_ports: std::collections::HashMap<InstanceId, Option<u16>> =
            std::collections::HashMap::new();
        let mut new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec> =
            std::collections::HashMap::new();
        let mut new_ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>> =
            std::collections::HashMap::new();
        let mut new_prepared: Vec<InstanceId> = Vec::new();
        let mut runtime_attempted = std::collections::HashSet::new();
        let mut new_failed = false;

        // Start and health check the entire green fleet before touching blue.
        // Unlike the rolling planner, nothing retires here and nothing is
        // published to routing yet: green comes up dark, alongside blue.
        for i in 0..replica_count {
            if self.report_cancellation(events).await {
                new_failed = true;
                break;
            }
            let new_id = crate::grill::InstanceIdentity::canary(namespace, app_name, deploy_gen, i)
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
            if let Err(e) = self.grill.create(&new_id, &oci_spec).await {
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
                        .provision_identity(app_name, namespace, &new_id, false)
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
        if new_failed {
            self.abort_rollout(
                app_name,
                namespace,
                spec,
                &new_ids,
                &new_prepared,
                &runtime_attempted,
                &new_ports,
                &new_specs,
                deploy_config.auto_rollback,
                0,
                replica_count,
                events,
            )
            .await;
            return std::ops::ControlFlow::Break(());
        }

        // The whole green fleet is healthy. Cut over: publish every green
        // backend so routing picks them up while blue still serves, then
        // drain and stop blue on this spawned task (M7 — the bulk drain used
        // to run inside finalise on the command loop, freezing every agent
        // command for up to fleet-size × drain_timeout), and finally send the
        // fast bookkeeping to the loop.
        for new_id in &new_ids {
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
                self.abort_rollout(
                    app_name,
                    namespace,
                    spec,
                    &new_ids,
                    &new_prepared,
                    &runtime_attempted,
                    &new_ports,
                    &new_specs,
                    deploy_config.auto_rollback,
                    0,
                    replica_count,
                    events,
                )
                .await;
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
                        app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
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
                new_ips,
                new_specs.clone(),
                now,
            )
            .await
        {
            let retention = self
                .retain_started_replacements(
                    app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
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
