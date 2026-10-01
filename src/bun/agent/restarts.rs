//! Restarts whose runtime steps run off the agent loop (#351, stage 2).
//!
//! A restart used to run start to finish inside one health tick: kill the
//! old container and wait for its exit (up to twice the stop-confirmation
//! timeout), create the replacement, start it. Every command waited for all
//! of it. Now each runtime step runs in its own task in `restart_steps`, and
//! its result comes back through a `select!` branch. The loop keeps
//! everything in between (discovery withdrawal, the job ledger, artifact
//! retirement, network preparation, the service map), so the invariants that
//! cross instances still hold because only the loop changes state.
//!
//! ```text
//!  tick            branch            branch             branch
//!  Pending ─Clear─▶ (Pending) ─Create─▶ (Preparing) ─Start─▶ (Starting) ─▶ HealthWait
//!                                         └─refused─Refuse─▶ Failed
//!  Stopping ─Cleanup─▶ Stopped ─▶ Pending (backoff permitting)
//! ```
//!
//! Each restart has at most one step in flight, recorded in `restarts`, so a
//! tick never starts a second restart of the same instance. A stop or a
//! retirement takes the instance back through its [`RestartGate`]: it
//! cancels the restart, waits for the step already running to finish, and
//! only then signals the runtime. A cancelled step never touches the runtime,
//! and the loop drops the result of one that finished anyway.
//!
//! This is a narrow slice of the per-instance supervisor the agent-loop
//! review called option (b): the step vocabulary here is the event
//! vocabulary such a supervisor would report, so it can absorb this instead
//! of replacing it.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{BunAgent, BunError, ContainerState, Grill, InstanceId, kill_runtime_instance};
use crate::grill::oci::OciSpec;

/// At most this many restarts have a runtime step in flight at once. A node
/// that lost every container restarts them a batch at a time, rather than
/// asking the runtime to kill and create dozens of containers at once.
pub(super) const RESTARTS_IN_FLIGHT_LIMIT: usize = 8;

/// The runtime half of one restart step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartStep {
    /// Kill what a failed attempt left behind and confirm its exit.
    Cleanup,
    /// Kill the crashed predecessor and confirm its exit, so the
    /// replacement can reuse its id.
    Clear,
    /// Create the replacement from its stored OCI spec.
    Create,
    /// Start the replacement and read its address.
    Start,
    /// Stop a created replacement whose network preparation was refused.
    Refuse,
}

/// What a restart carries from one step to the next.
#[derive(Debug, Clone)]
pub(super) struct RestartLaunch {
    pub(super) oci_spec: OciSpec,
    pub(super) app_name: String,
    pub(super) namespace: String,
    pub(super) host_port: Option<u16>,
}

/// How a stop or retirement takes an instance back from its restart.
///
/// The step task holds `lane` for as long as it talks to the runtime, and
/// checks `cancelled` once it holds it. So after `cancel`, waiting for the
/// lane ([`RestartGate::settled`]) means no step of this restart touches the
/// runtime again.
#[derive(Debug, Clone, Default)]
pub(super) struct RestartGate {
    cancelled: CancellationToken,
    lane: Arc<tokio::sync::Mutex<()>>,
}

impl RestartGate {
    pub(super) fn cancel(&self) {
        self.cancelled.cancel();
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.is_cancelled()
    }

    /// Wait until the step running now, if any, is done with the runtime.
    pub(super) async fn settled(&self) {
        let _lane = self.lane.lock().await;
    }
}

/// A restart taken back by a stop or retirement, whose in-flight step the
/// caller must let finish before it signals the runtime itself.
#[derive(Debug, Clone)]
pub(crate) struct TakenRestart {
    id: InstanceId,
    gate: RestartGate,
}

impl TakenRestart {
    /// Wait, at most `timeout`, for the restart's in-flight runtime step.
    /// A step that outlasts it fails the stop, which can be retried: better
    /// than signalling a container the step may be creating or starting.
    pub(crate) async fn settle(&self, timeout: Duration) -> Result<(), BunError> {
        tokio::time::timeout(timeout, self.gate.settled())
            .await
            .map_err(|_| BunError::StopUnconfirmed {
                instance_id: self.id.clone(),
                reason: "a restart's runtime step did not finish",
            })
    }
}

/// Settle every taken restart in turn, stopping at the first that won't.
pub(super) async fn settle_all(taken: &[TakenRestart], timeout: Duration) -> Result<(), BunError> {
    for restart in taken {
        restart.settle(timeout).await?;
    }
    Ok(())
}

/// One restart with a runtime step in flight.
#[derive(Debug)]
pub(super) struct RestartInFlight {
    step: RestartStep,
    launch: Option<RestartLaunch>,
    gate: RestartGate,
    task: tokio::task::Id,
}

/// How a step ended.
#[derive(Debug)]
pub(super) enum StepResult {
    /// The runtime did it. A start also reports the replacement's address.
    Done { container_ip: Option<Ipv4Addr> },
    /// The runtime refused or didn't confirm.
    Failed(BunError),
    /// A stop or retirement took the instance back before the step began.
    Cancelled,
}

/// Restarts with a step in flight, by instance.
pub(super) type Restarts = std::collections::HashMap<InstanceId, RestartInFlight>;

/// The outcome of one step task, as `JoinSet::join_next_with_id` yields it.
pub(super) type RestartStepOutcome = Result<(tokio::task::Id, StepResult), tokio::task::JoinError>;

/// Run one step against the runtime, unless the restart was cancelled.
async fn run_step<G: Grill>(
    grill: G,
    id: InstanceId,
    step: RestartStep,
    oci_spec: Option<OciSpec>,
    gate: RestartGate,
    confirmation_timeout: Duration,
) -> StepResult {
    let _lane = gate.lane.lock().await;
    if gate.is_cancelled() {
        return StepResult::Cancelled;
    }
    let done = match step {
        RestartStep::Cleanup | RestartStep::Clear => {
            kill_runtime_instance(&grill, &id, confirmation_timeout).await
        }
        RestartStep::Create => match oci_spec {
            Some(spec) => grill.create(&id, &spec).await.map_err(BunError::from),
            None => Err(BunError::DeployFailed {
                app_name: id.0.clone(),
                reason: "restart has no stored OCI spec".into(),
            }),
        },
        RestartStep::Start => match grill.start(&id).await {
            // A re-created container may get a fresh IP.
            Ok(()) => {
                return StepResult::Done {
                    container_ip: grill.container_ip(&id).await,
                };
            }
            Err(error) => Err(error.into()),
        },
        RestartStep::Refuse => grill.stop(&id).await.map_err(BunError::from),
    };
    match done {
        Ok(()) => StepResult::Done { container_ip: None },
        Err(error) => StepResult::Failed(error),
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Hand one runtime step of `id`'s restart to a task.
    fn spawn_restart_step(
        &mut self,
        id: InstanceId,
        step: RestartStep,
        launch: Option<RestartLaunch>,
        gate: RestartGate,
    ) {
        let oci_spec = match step {
            RestartStep::Create => launch.as_ref().map(|launch| launch.oci_spec.clone()),
            _ => None,
        };
        let task = self
            .restart_steps
            .spawn(run_step(
                self.supervisor.grill().clone(),
                id.clone(),
                step,
                oci_spec,
                gate.clone(),
                self.stop_confirmation_timeout,
            ))
            .id();
        self.restarts.insert(
            id,
            RestartInFlight {
                step,
                launch,
                gate,
                task,
            },
        );
    }

    /// Whether `id` has a restart step in flight.
    pub(super) fn restarting(&self, id: &InstanceId) -> bool {
        self.restarts.contains_key(id)
    }

    /// Room for another restart step this tick.
    pub(super) fn restart_capacity_left(&self) -> bool {
        self.restarts.len() < RESTARTS_IN_FLIGHT_LIMIT
    }

    /// Take `id` back from its restart, if one is in flight. The restart
    /// won't touch the runtime again once the step already running is done,
    /// and the caller must wait for that ([`TakenRestart::settle`]) before
    /// it signals the runtime itself.
    pub(super) fn take_back_from_restart(&mut self, id: &InstanceId) -> Option<TakenRestart> {
        let restart = self.restarts.get(id)?;
        restart.gate.cancel();
        Some(TakenRestart {
            id: id.clone(),
            gate: restart.gate.clone(),
        })
    }

    /// Take every instance back from its restart, for shutdown.
    pub(super) fn take_back_all_restarts(&mut self) -> Vec<TakenRestart> {
        let ids: Vec<InstanceId> = self.restarts.keys().cloned().collect();
        ids.iter()
            .filter_map(|id| self.take_back_from_restart(id))
            .collect()
    }

    /// Begin cleaning up the runtime a failed restart attempt left behind.
    pub(super) async fn begin_restart_cleanup(&mut self, id: InstanceId) {
        if self.runtime_known_absent(&id) {
            self.restart_after_cleanup(&id).await;
            return;
        }
        self.spawn_restart_step(id, RestartStep::Cleanup, None, RestartGate::default());
    }

    /// Begin a restart of a Pending instance: kill the predecessor first.
    pub(super) async fn begin_restart_launch(&mut self, id: InstanceId, launch: RestartLaunch) {
        if self.runtime_known_absent(&id) {
            self.restart_after_clear(id, launch, RestartGate::default())
                .await;
            return;
        }
        self.spawn_restart_step(id, RestartStep::Clear, Some(launch), RestartGate::default());
    }

    /// Apply a finished restart step and start the next one.
    pub(super) async fn finish_restart_step(&mut self, outcome: RestartStepOutcome) {
        let (task, result) = match outcome {
            Ok(finished) => finished,
            Err(error) => (
                error.id(),
                StepResult::Failed(BunError::DeployFailed {
                    app_name: String::new(),
                    reason: format!("restart step task failed: {error}"),
                }),
            ),
        };
        let Some(id) = self
            .restarts
            .iter()
            .find(|(_, restart)| restart.task == task)
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let Some(restart) = self.restarts.remove(&id) else {
            return;
        };
        // A stop or retirement owns the instance now. It waited for this
        // step before signalling the runtime, and it cleans up what the
        // step made.
        if restart.gate.is_cancelled() || matches!(result, StepResult::Cancelled) {
            return;
        }
        let RestartInFlight {
            step, launch, gate, ..
        } = restart;
        match (step, result, launch) {
            (RestartStep::Cleanup, StepResult::Done { .. }, _) => {
                self.restart_after_cleanup(&id).await;
            }
            (RestartStep::Cleanup, StepResult::Failed(error), _) => {
                eprintln!("bun: failed restart of {id} awaits runtime cleanup: {error}");
            }
            (RestartStep::Clear, StepResult::Done { .. }, Some(launch)) => {
                self.restart_after_clear(id, launch, gate).await;
            }
            (RestartStep::Clear, StepResult::Failed(error), _) => {
                eprintln!("bun: restart of {id} awaits runtime cleanup: {error}");
            }
            (RestartStep::Create, StepResult::Done { .. }, Some(launch)) => {
                self.restart_after_create(id, launch, gate).await;
            }
            (RestartStep::Start, StepResult::Done { container_ip }, Some(launch)) => {
                self.restart_after_start(&id, &launch, container_ip).await;
            }
            (RestartStep::Create | RestartStep::Start, StepResult::Failed(error), _) => {
                self.record_failed_restart(&id, &error.to_string()).await;
            }
            (RestartStep::Refuse, StepResult::Done { .. }, _) => {
                if let Some(instance) = self.supervisor.get_instance_mut(&id)
                    && let Ok(state) = instance.state.transition_to(ContainerState::Failed)
                {
                    instance.state = state;
                }
            }
            (RestartStep::Refuse, StepResult::Failed(error), _) => {
                // The replacement is created but not stopped. Keep the
                // cleanup owed instead of abandoning it as Failed.
                self.record_failed_restart(
                    &id,
                    &format!("refused restart could not stop its created container: {error}"),
                )
                .await;
            }
            (step, StepResult::Done { .. }, None) => {
                eprintln!("bun: restart of {id} lost its launch details after {step:?}");
            }
            (_, StepResult::Cancelled, _) => {}
        }
    }

    /// Whether the job ledger already proves `id` has no runtime left.
    fn runtime_known_absent(&self, id: &InstanceId) -> bool {
        self.recorded_jobs
            .get(&id.0)
            .is_some_and(|job| job.runtime_absent)
    }

    /// The loop's step between Cleanup and a new attempt: the failed
    /// attempt's runtime is gone, so the instance may restart again.
    async fn restart_after_cleanup(&mut self, id: &InstanceId) {
        let Some(instance) = self.supervisor.get_instance_mut(id) else {
            return;
        };
        if !instance.retry_pending {
            return;
        }
        let Ok(stopped) = instance.state.transition_to(ContainerState::Stopped) else {
            return;
        };
        instance.state = stopped;
        self.retry_restart(id).await;
    }

    /// The loop's step between Clear and Create: settle the job ledger and
    /// the predecessor's artifacts, then hand the create to a task.
    async fn restart_after_clear(
        &mut self,
        id: InstanceId,
        launch: RestartLaunch,
        gate: RestartGate,
    ) {
        let Some(instance) = self.supervisor.get_instance(&id) else {
            return;
        };
        if instance.state != ContainerState::Pending {
            return;
        }
        if instance.is_job {
            if let Err(error) = self.record_job_runtime_absent(&id).await {
                eprintln!("bun: job retry cannot persist runtime absence for {id}: {error}");
                return;
            }
            if let Err(error) = self.retire_instance_artifacts(&id).await {
                eprintln!("bun: job retry retains artifacts for {id}: {error}");
                return;
            }
            if let Err(error) = self.claim_job_retry(&id).await {
                eprintln!("bun: job retry refused for {id}: {error}");
                return;
            }
        } else if let Err(error) = self.retire_restart_artifacts(&id).await {
            eprintln!("bun: application restart retains predecessor artifacts for {id}: {error}");
            return;
        }

        if let Some(instance) = self.supervisor.get_instance_mut(&id) {
            match instance.state.transition_to(ContainerState::Preparing) {
                Ok(state) => instance.state = state,
                Err(_) => return,
            }
        }
        self.spawn_restart_step(id, RestartStep::Create, Some(launch), gate);
    }

    /// The loop's step between Create and Start: program the recreated
    /// cgroup's network before anything runs in it, then take the durable
    /// job permit.
    async fn restart_after_create(
        &mut self,
        id: InstanceId,
        launch: RestartLaunch,
        gate: RestartGate,
    ) {
        if self
            .supervisor
            .get_instance(&id)
            .is_none_or(|instance| instance.state != ContainerState::Preparing)
        {
            return;
        }
        // Close the restart window too: the recreated cgroup gets its egress
        // programmed before start (the crash gave the instance a fresh cgroup
        // id). The AppSpec comes from the stored deploy record, the cgroup
        // path from the stored OCI spec. On failure the created container is
        // stopped and the restart refused — fail closed, same as a fresh
        // deploy.
        let restart_spec = self
            .deployed_specs
            .get(&(launch.app_name.clone(), launch.namespace.clone()))
            .cloned();
        let restart_egress = match launch.oci_spec.linux.host_cgroup_path() {
            Some(cgroup_path) => {
                self.apply_network_pre_start(
                    &id,
                    &launch.app_name,
                    restart_spec.as_ref(),
                    &cgroup_path,
                )
                .await
            }
            None if self.supervisor.grill().honours_cgroup_path()
                || restart_spec
                    .as_ref()
                    .and_then(|spec| spec.egress.as_ref())
                    .is_some_and(|policy| !policy.allow.is_empty()) =>
            {
                Err(BunError::DeployFailed {
                    app_name: launch.app_name.clone(),
                    reason: "restart has no original cgroup path for network preparation".into(),
                })
            }
            None => Ok(()),
        };
        if let Err(error) = restart_egress {
            eprintln!("bun: restart of {} refused: {error}", id.0);
            self.spawn_restart_step(id, RestartStep::Refuse, Some(launch), gate);
            return;
        }

        // The durable job permit must precede every retry's start too.
        if let Err(error) = self
            .transition_deploy_state(&id, ContainerState::Starting)
            .await
        {
            self.record_failed_restart(&id, &error.to_string()).await;
            return;
        }
        self.spawn_restart_step(id, RestartStep::Start, Some(launch), gate);
    }

    /// The loop's step after Start: stream the replacement's logs, record
    /// it for adoption, and route to its address.
    async fn restart_after_start(
        &mut self,
        id: &InstanceId,
        launch: &RestartLaunch,
        container_ip: Option<Ipv4Addr>,
    ) {
        if self
            .supervisor
            .get_instance(id)
            .is_none_or(|instance| instance.state != ContainerState::Starting)
        {
            return;
        }
        self.spawn_log_forwarder(id, &launch.app_name, &launch.namespace);
        if let Err(error) = self.persist_instance_record(id).await {
            self.record_failed_restart(id, &error.to_string()).await;
            return;
        }
        if let Some(instance) = self.supervisor.get_instance_mut(id) {
            instance.container_ip = container_ip;
        }
        let service_id =
            crate::onion::service_id::ServiceId::new(&launch.namespace, &launch.app_name);
        let mut candidate = self.service_map.clone();
        if let Some(port) = launch.host_port {
            let healthy = self
                .supervisor
                .get_instance(id)
                .is_some_and(|instance| instance.health_config.is_none());
            let backend = self.local_backend(id, &service_id, container_ip, port, healthy);
            if let Err(error) = candidate.add_backend(&service_id, backend) {
                self.record_failed_restart(id, &error.to_string()).await;
                return;
            }
        }
        // Keep every reader on the confirmed view. A runtime restart can
        // change its address, but does not establish application health.
        if let Err(error) = self.publish_backend_snapshot(&service_id, &candidate).await {
            self.record_failed_restart(id, &error.to_string()).await;
            return;
        }
        self.service_map = candidate;
        self.sync_firewall_ebpf().await;
        self.rebuild_routing_table().await;

        // Starting → HealthWait, then Running if no health checks
        if let Some(instance) = self.supervisor.get_instance_mut(id) {
            if let Ok(state) = instance.state.transition_to(ContainerState::HealthWait) {
                instance.state = state;
            }
            if instance.health_config.is_none()
                && let Ok(state) = instance.state.transition_to(ContainerState::Running)
            {
                instance.state = state;
            }
        }
    }

    /// Drive every restart step now in flight to its end, the next steps
    /// included, the way the loop's branch would. For tests that drive the
    /// agent without running its loop.
    #[cfg(test)]
    pub(super) async fn settle_restart_steps(&mut self) {
        while let Some(outcome) = self.restart_steps.join_next_with_id().await {
            self.finish_restart_step(outcome).await;
        }
    }
}
