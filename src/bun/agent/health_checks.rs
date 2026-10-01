//! Health on the loop: the probes each tick runs, what their results do to
//! an instance, and the exits the tick notices.

use super::*;

/// Wait for a replacement instance to become healthy, on the deploy worker
/// (M5): first for the runtime to report `Running`, then — when the app
/// declares a health check — for the HTTP probe itself to pass
/// `threshold_healthy` consecutive times.
///
/// The old wait stopped at `Running`, the runtime's "process alive" view. A
/// version that started but failed its probe was announced healthy, published
/// as a routable backend, and allowed to replace instances that were
/// genuinely serving; its first real probe only ran after the deploy
/// finalised. Apps without a health check keep the `Running`-only wait.
///
/// Probes use the same config as steady-state monitoring afterwards
/// (`HealthCheckConfig::from_spec` on the spec's container port, probed at
/// `probe_host`), honouring `initial_delay` and re-probing at `interval`
/// capped to 500 ms — a deploy gate wants responsiveness, not the
/// steady-state cadence — all bounded by the deploy's `health_timeout`
/// deadline. Returns the failure message for the deploy's error event.
pub(super) async fn wait_instance_healthy<G: Grill>(
    grill: &G,
    id: &InstanceId,
    spec: &AppSpec,
    container_ip: Option<std::net::Ipv4Addr>,
    wait: std::time::Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + wait;
    let mut state = grill.state(id).await;
    while std::time::Instant::now() < deadline
        && !matches!(state, Ok(crate::grill::state::ContainerState::Running))
    {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        state = grill.state(id).await;
    }
    match state {
        Ok(crate::grill::state::ContainerState::Running) => {}
        Ok(state) => {
            return Err(format!(
                "{} not healthy (state: {state}), rolling back",
                id.0
            ));
        }
        Err(_) => return Err(format!("{} state unknown, rolling back", id.0)),
    }

    let Some(config) = spec
        .health
        .as_ref()
        .zip(spec.port)
        .map(|(hs, port)| crate::bun::health::HealthCheckConfig::from_spec(hs, port))
    else {
        return Ok(());
    };

    let host = probe_host(container_ip);
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    tokio::time::sleep(config.initial_delay.min(remaining)).await;
    let mut consecutive = 0u32;
    let mut last_status;
    // At least one probe runs even if `initial_delay` consumed the deadline,
    // so a tight `health_timeout` degrades to a single-shot check rather than
    // failing without ever asking the app.
    loop {
        last_status = crate::bun::probe::probe_health(&config, &host)
            .await
            .map_err(|error| format!("{}: {error}", id.0))?;
        if last_status == crate::bun::health::HealthStatus::Healthy {
            consecutive += 1;
            if consecutive >= config.threshold_healthy {
                return Ok(());
            }
        } else {
            consecutive = 0;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "{} failed its health check ({last_status:?} at {}:{}{}), rolling back",
                id.0, host, config.port, config.path
            ));
        }
        tokio::time::sleep(
            config
                .interval
                .min(deadline.saturating_duration_since(std::time::Instant::now()))
                .min(std::time::Duration::from_millis(500)),
        )
        .await;
    }
}

/// The address to probe an instance's health check at.
///
/// A container with its own IP (runc/apple per-container netns) is probed at
/// that IP; ProcessGrill shares the host network, so it falls back to loopback.
/// Previously hardcoded to loopback, which flapped every runc app unhealthy.
pub(super) fn probe_host(container_ip: Option<std::net::Ipv4Addr>) -> String {
    container_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Run any due health checks.
    pub(super) async fn run_health_checks(&mut self) {
        let now = Instant::now();
        let mut due = Vec::new();
        while let Some(check) = self.supervisor.health_checker_mut().pop_due(now) {
            due.push(check);
        }
        for (instance_id, config) in due {
            let Some(instance) = self.supervisor.get_instance(&instance_id) else {
                continue;
            };
            if !matches!(
                instance.state,
                ContainerState::HealthWait | ContainerState::Running | ContainerState::Unhealthy
            ) || !self.health_inflight.insert(instance_id.clone())
            {
                self.supervisor
                    .health_checker_mut()
                    .schedule_next(instance_id, now);
                continue;
            }
            let host = probe_host(instance.container_ip);
            let created_at = instance.created_at;
            let results = self.deploy_ops_tx.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                let status = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    status = probe_health(&config, &host) => status,
                };
                let result = DeployOp::HealthProbeResult {
                    instance_id,
                    created_at,
                    status,
                };
                tokio::select! {
                    _ = shutdown.cancelled() => {},
                    _ = results.send(result) => {},
                }
            });
        }
    }

    pub(super) async fn complete_health_probe(
        &mut self,
        instance_id: InstanceId,
        created_at: Instant,
        status: Result<crate::bun::health::HealthStatus, crate::bun::probe::ProbeError>,
    ) {
        self.health_inflight.remove(&instance_id);
        let now = Instant::now();
        let Some(instance) = self.supervisor.get_instance(&instance_id) else {
            return;
        };
        // A newer registration owns the cadence of a replaced instance.
        if instance.created_at != created_at {
            return;
        }
        if !matches!(
            instance.state,
            ContainerState::HealthWait | ContainerState::Running | ContainerState::Unhealthy
        ) {
            // The instance left the probed states while this probe was in
            // flight (killed, restarting). Discard the result but keep its
            // cadence, as `run_health_checks` does for a skipped check: a
            // restart reuses this registration, so dropping it here would
            // leave the restarted instance in HealthWait with no probes.
            self.supervisor
                .health_checker_mut()
                .schedule_next(instance_id, now);
            return;
        }
        let status = match status {
            Ok(status) => status,
            Err(error) => {
                eprintln!("bun: {}: {error}", instance_id.0);
                self.supervisor
                    .health_checker_mut()
                    .schedule_next(instance_id, now);
                return;
            }
        };
        let transition = self.supervisor.process_health_result(&instance_id, status);

        if let Ok(Some(ContainerState::Unhealthy)) = transition
            && let Some(instance) = self.supervisor.get_instance(&instance_id)
        {
            self.record_event(
                crate::bun::events::EventKind::Health,
                crate::bun::events::EventSeverity::Warning,
                Some(instance.app_name.clone()),
                Some(instance.namespace.clone()),
                format!("instance {} became unhealthy", instance_id.0),
            )
            .await;
        }
        // Retry publication even when health state already changed on an earlier
        // probe. A refused withdrawal must not advance the restart state machine.
        if let Err(error) = self.publish_instance_health(&instance_id).await {
            eprintln!("bun: {error}");
            self.supervisor
                .health_checker_mut()
                .schedule_next(instance_id, now);
            return;
        }

        // A later probe can complete publication that the transition probe failed.
        // LOOP-INLINE: in-memory lock, no I/O
        if self
            .supervisor
            .get_instance(&instance_id)
            .is_some_and(|instance| instance.state == ContainerState::Unhealthy)
            && self
                .supervisor
                .maybe_restart(&instance_id, now)
                .await
                .unwrap_or(false)
            && let Some(instance) = self.supervisor.get_instance(&instance_id)
        {
            self.record_event(
                crate::bun::events::EventKind::Restart,
                crate::bun::events::EventSeverity::Warning,
                Some(instance.app_name.clone()),
                Some(instance.namespace.clone()),
                format!(
                    "instance {} restarted (attempt {})",
                    instance_id.0, instance.restart_count
                ),
            )
            .await;
        }

        self.supervisor
            .health_checker_mut()
            .schedule_next(instance_id, now);
    }

    /// Confirm health publication before routing changes or automatic restart.
    pub(super) async fn publish_instance_health(
        &mut self,
        id: &InstanceId,
    ) -> Result<(), BunError> {
        let instance =
            self.supervisor
                .get_instance(id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: id.clone(),
                })?;
        if instance.host_port.is_none() {
            return Ok(());
        }
        let service =
            crate::onion::service_id::ServiceId::new(&instance.namespace, &instance.app_name);
        let healthy = instance.state == ContainerState::Running;
        // `service_map` changes only after a successful publication, so a
        // failed attempt still differs here and the next probe retries it.
        let published = self
            .service_map
            .resolve(&service)
            .and_then(|entry| {
                entry
                    .backends
                    .iter()
                    .find(|backend| backend.instance_id == id.0)
            })
            .is_some_and(|backend| backend.healthy == healthy);
        if published {
            return Ok(());
        }
        let mut candidate = self.service_map.clone();
        candidate
            .set_backend_health(&service, &id.0, healthy)
            .map_err(|error| BunError::BackendPublication {
                service: service.clone(),
                reason: error.to_string(),
            })?;
        self.publish_backend_snapshot(&service, &candidate).await?;
        self.service_map = candidate;
        self.rebuild_routing_table().await;
        Ok(())
    }

    /// A running app's process has exited, which a health check catches
    /// only if the app has one. Mark it Stopped and route it through the
    /// restart path.
    pub(super) async fn observe_app_exit(&mut self, id: &InstanceId) {
        if let Some(instance) = self.supervisor.get_instance_mut(id) {
            instance.retry_pending = true;
            if let Ok(s) = instance.state.transition_to(ContainerState::Stopping) {
                instance.state = s;
            }
            if let Ok(s) = instance.state.transition_to(ContainerState::Stopped) {
                instance.state = s;
            }
        }
        // LOOP-INLINE: in-memory lock, no I/O
        if let Err(BunError::RestartLimitExceeded { .. }) =
            self.supervisor.maybe_restart(id, Instant::now()).await
            && let Some(instance) = self.supervisor.get_instance_mut(id)
            && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
        {
            instance.state = s;
        }
    }

    /// Read every running app's state and apply what the reads saw, inline.
    /// For tests that drive the agent without running its loop.
    #[cfg(test)]
    pub(super) async fn check_apps(&mut self) {
        let reads = self.plan_state_reads(|instance| !instance.is_job);
        let grill = self.supervisor.grill().clone();
        let sweep = state_sweep::sweep_states(grill, reads).await;
        self.apply_state_sweep(Ok(sweep)).await;
    }
}
