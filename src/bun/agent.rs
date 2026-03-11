/// Bun agent event loop.
///
/// Ties the supervisor, health checker, and container runtime together
/// into a single async event loop. Commands arrive over an `mpsc` channel;
/// health checks fire on a timer; shutdown is coordinated via a
/// `CancellationToken`.
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::config::app::AppSpec;
use crate::config::job::JobSpec;
use crate::grill::oci::{generate_job_oci_spec, generate_oci_spec};
use crate::grill::port::PortAllocator;
use crate::grill::state::ContainerState;
use crate::grill::{Grill, InstanceId};

use super::BunError;
use super::probe::probe_health;
use super::supervisor::WorkloadSupervisor;

/// A progress event emitted during a deploy operation.
///
/// Sent over an `mpsc` channel so the API layer can stream events
/// to the client via SSE. The client displays `Progress` messages
/// in real time and collects the final `Complete` or `Error` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ApplyEvent {
    /// Informational progress update.
    Progress { message: String },
    /// A single instance was created and started.
    InstanceCreated { id: String, app: String },
    /// The deploy finished successfully.
    Complete {
        created: usize,
        instances: Vec<String>,
    },
    /// The deploy failed.
    Error { message: String },
}

/// Commands sent to the agent over the command channel.
pub enum AgentCommand {
    /// Deploy workloads from a parsed Config.
    ///
    /// Progress events are streamed over the `events` channel so the
    /// API can relay them to the client in real time.
    Deploy {
        config: Config,
        events: mpsc::Sender<ApplyEvent>,
    },
    /// Stop all instances of an app in a namespace.
    Stop {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Get status of all instances.
    Status {
        response: oneshot::Sender<Vec<InstanceStatus>>,
    },
    /// Get logs for an app.
    Logs {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<String, BunError>>,
    },
}

/// Result of a deploy operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    /// Number of instances created.
    pub created: usize,
    /// Instance IDs that were created.
    pub instances: Vec<String>,
}

/// Status of a single workload instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    /// Instance ID.
    pub id: String,
    /// App name.
    pub app_name: String,
    /// Namespace.
    pub namespace: String,
    /// Current lifecycle state.
    pub state: String,
    /// Number of restarts.
    pub restart_count: u32,
    /// Allocated host port, if any.
    pub host_port: Option<u16>,
    /// OS process ID, if available.
    pub pid: Option<u32>,
}

/// The Bun agent. Generic over `G: Grill` so tests can inject mocks.
pub struct BunAgent<G: Grill> {
    supervisor: WorkloadSupervisor<G>,
    command_rx: mpsc::Receiver<AgentCommand>,
    shutdown: CancellationToken,
    volumes_dir: PathBuf,
}

impl<G: Grill> BunAgent<G> {
    /// Create a new agent.
    pub fn new(
        grill: G,
        port_allocator: PortAllocator,
        command_rx: mpsc::Receiver<AgentCommand>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            supervisor: WorkloadSupervisor::new(grill, port_allocator),
            command_rx,
            shutdown,
            volumes_dir: crate::config::node::StorageSection::default().volumes,
        }
    }

    /// Run the agent event loop until shutdown is requested.
    pub async fn run(&mut self) {
        let mut health_interval = tokio::time::interval(std::time::Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    self.shutdown_all().await;
                    break;
                }
                Some(cmd) = self.command_rx.recv() => {
                    self.handle_command(cmd).await;
                }
                _ = health_interval.tick() => {
                    self.run_health_checks().await;
                    self.check_jobs().await;
                    self.drive_pending_restarts().await;
                }
            }
        }
    }

    /// Handle a single command.
    async fn handle_command(&mut self, cmd: AgentCommand) {
        match cmd {
            AgentCommand::Deploy { config, events } => {
                self.deploy(config, &events).await;
            }
            AgentCommand::Stop {
                app_name,
                namespace,
                response,
            } => {
                let result = self.stop_app(&app_name, &namespace).await;
                let _ = response.send(result);
            }
            AgentCommand::Status { response } => {
                let statuses = self.get_status().await;
                let _ = response.send(statuses);
            }
            AgentCommand::Logs {
                app_name,
                namespace,
                response,
            } => {
                let result = self.get_logs(&app_name, &namespace).await;
                let _ = response.send(result);
            }
        }
    }

    /// Deploy all apps and jobs from a config, streaming progress events.
    async fn deploy(&mut self, config: Config, events: &mpsc::Sender<ApplyEvent>) {
        let now = Instant::now();
        let mut all_ids = Vec::new();

        for (app_name, spec) in &config.app {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("deploying app {app_name} (replicas: {})", spec.replicas),
                })
                .await;

            let ids = match self
                .supervisor
                .deploy_app(app_name, namespace, spec, now)
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

            // Drive each instance through Pending → Preparing → Starting → HealthWait
            for id in &ids {
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!("creating instance {}", id.0),
                    })
                    .await;

                if let Err(e) = self
                    .drive_instance_startup(id, app_name, namespace, spec)
                    .await
                {
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
                        app: app_name.to_string(),
                    })
                    .await;
            }

            all_ids.extend(ids.iter().map(|id| id.0.clone()));
        }

        for (job_name, spec) in &config.job {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("deploying job {job_name}"),
                })
                .await;

            let ids = match self
                .supervisor
                .deploy_job(job_name, namespace, spec, now)
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

                if let Err(e) = self
                    .drive_job_startup(id, job_name, namespace, spec)
                    .await
                {
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

        let _ = events
            .send(ApplyEvent::Complete {
                created: all_ids.len(),
                instances: all_ids,
            })
            .await;
    }

    /// Drive a newly created instance through the startup state machine.
    async fn drive_instance_startup(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        // Pending → Preparing
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Preparing)?;
        }

        // Generate OCI spec and call grill.create()
        let host_port = self
            .supervisor
            .get_instance(instance_id)
            .and_then(|i| i.host_port);

        // Extract the replica index from "app_name-N" format
        let instance_index: u32 = instance_id
            .0
            .rsplit('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, app_name, instance_index);
        let cgroup_str = cgroup_path.to_string_lossy();
        let oci_spec = generate_oci_spec(
            app_name,
            namespace,
            spec,
            host_port,
            &cgroup_str,
            Some(&self.volumes_dir),
        );

        self.supervisor
            .grill()
            .create(instance_id, &oci_spec)
            .await?;

        // Store OCI spec for restart re-drive
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.oci_spec = Some(oci_spec);
        }

        // Run init containers if any
        if !spec.init.is_empty() {
            // Preparing → Initialising
            {
                let instance = self
                    .supervisor
                    .get_instance_mut(instance_id)
                    .ok_or_else(|| BunError::InstanceNotFound {
                        instance_id: instance_id.clone(),
                    })?;
                instance.state = instance.state.transition_to(ContainerState::Initialising)?;
            }

            for (i, init_spec) in spec.init.iter().enumerate() {
                let init_id = InstanceId(format!("{}-init-{i}", instance_id.0));
                let init_oci = crate::grill::oci::generate_init_oci_spec(
                    &init_spec.command,
                    namespace,
                    app_name,
                    spec.image.as_deref(),
                    &cgroup_str,
                );

                self.supervisor.grill().create(&init_id, &init_oci).await?;
                self.supervisor.grill().start(&init_id).await?;

                // Wait for init container to complete
                let failed = loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let state = self.supervisor.grill().state(&init_id).await?;
                    if state == ContainerState::Stopped {
                        let exit_code = self.supervisor.grill().exit_code(&init_id).await;
                        break exit_code != Some(0);
                    }
                };

                if failed {
                    if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
                        instance.state = instance.state.transition_to(ContainerState::Failed)?;
                    }
                    return Err(BunError::InitContainerFailed {
                        instance_id: instance_id.clone(),
                        init_index: i,
                    });
                }
            }
        }

        // Preparing/Initialising → Starting
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Starting)?;
        }

        // Call grill.start()
        self.supervisor.grill().start(instance_id).await?;

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

        Ok(())
    }

    /// Drive a job instance through startup: Pending → Preparing → Starting → Running.
    ///
    /// Jobs skip health checks and go straight to Running.
    async fn drive_job_startup(
        &mut self,
        instance_id: &InstanceId,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
    ) -> Result<(), BunError> {
        // Pending → Preparing
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Preparing)?;
        }

        let instance_index: u32 = instance_id
            .0
            .rsplit('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, job_name, instance_index);
        let cgroup_str = cgroup_path.to_string_lossy();
        let oci_spec = generate_job_oci_spec(job_name, namespace, spec, &cgroup_str);

        self.supervisor
            .grill()
            .create(instance_id, &oci_spec)
            .await?;

        // Store OCI spec for restart re-drive
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.oci_spec = Some(oci_spec);
        }

        // Preparing → Starting
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Starting)?;
        }

        self.supervisor.grill().start(instance_id).await?;

        // Starting → HealthWait → Running (no health checks for jobs)
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

    /// Run any due health checks.
    async fn run_health_checks(&mut self) {
        let now = Instant::now();

        // Collect all due checks
        let mut due_checks = Vec::new();
        while let Some((instance_id, config)) = self.supervisor.health_checker_mut().pop_due(now) {
            due_checks.push((instance_id, config));
        }

        for (instance_id, config) in due_checks {
            // Only probe instances in a probeable state
            let state = self.supervisor.get_instance(&instance_id).map(|i| i.state);

            let should_probe = matches!(
                state,
                Some(ContainerState::HealthWait)
                    | Some(ContainerState::Running)
                    | Some(ContainerState::Unhealthy)
            );

            if should_probe {
                let status = probe_health(&config, "127.0.0.1").await;

                let transition = self.supervisor.process_health_result(&instance_id, status);

                // Handle restart if unhealthy
                if let Ok(Some(ContainerState::Unhealthy)) = transition {
                    let _ = self.supervisor.maybe_restart(&instance_id, now).await;
                }
            }

            // Schedule the next check
            self.supervisor
                .health_checker_mut()
                .schedule_next(instance_id, now);
        }
    }

    /// Monitor running job instances for process exit.
    ///
    /// For each running job, polls the runtime to see if the process has
    /// exited. On success (exit code 0), transitions to Stopped. On
    /// failure, attempts a restart or marks as Failed if the retry limit
    /// is exhausted.
    async fn check_jobs(&mut self) {
        let now = Instant::now();

        // Check running job instances for process exit
        let running_jobs: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.is_job && i.state == ContainerState::Running)
            .map(|i| i.id.clone())
            .collect();

        for id in running_jobs {
            let grill_state = match self.supervisor.grill().state(&id).await {
                Ok(s) => s,
                Err(_) => continue,
            };

            if grill_state == ContainerState::Stopped {
                let exit_code = self.supervisor.grill().exit_code(&id).await;

                // Transition Running → Stopping → Stopped
                if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                    if let Ok(s) = instance.state.transition_to(ContainerState::Stopping) {
                        instance.state = s;
                    }
                    if let Ok(s) = instance.state.transition_to(ContainerState::Stopped) {
                        instance.state = s;
                    }
                }

                if exit_code == Some(0) {
                    // Job completed successfully — stays in Stopped
                    continue;
                }

                // Job failed — attempt restart
                match self.supervisor.maybe_restart(&id, now).await {
                    Ok(true) => {
                        // Now in Pending — drive_pending_restarts will handle it
                    }
                    Ok(false) => {
                        // Backoff not elapsed — will retry on next tick
                    }
                    Err(_) => {
                        // Exceeded restart limit — mark as Failed
                        if let Some(instance) = self.supervisor.get_instance_mut(&id)
                            && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
                        {
                            instance.state = s;
                        }
                    }
                }
            }
        }

        // Retry stopped failed jobs waiting for backoff
        let stopped_jobs: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.is_job && i.state == ContainerState::Stopped && i.restart_count > 0)
            .map(|i| i.id.clone())
            .collect();

        for id in stopped_jobs {
            match self.supervisor.maybe_restart(&id, now).await {
                Ok(true) => {
                    // Now in Pending — drive_pending_restarts will handle it
                }
                Ok(false) => {
                    // Still in backoff
                }
                Err(_) => {
                    if let Some(instance) = self.supervisor.get_instance_mut(&id)
                        && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
                    {
                        instance.state = s;
                    }
                }
            }
        }
    }

    /// Re-drive instances that are in Pending state after a restart.
    ///
    /// When `maybe_restart` transitions an instance back to Pending,
    /// this method picks it up and drives it through the startup
    /// sequence again using the stored OCI spec.
    async fn drive_pending_restarts(&mut self) {
        let pending_restarts: Vec<(InstanceId, crate::grill::oci::OciSpec)> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.state == ContainerState::Pending && i.restart_count > 0)
            .filter_map(|i| i.oci_spec.as_ref().map(|spec| (i.id.clone(), spec.clone())))
            .collect();

        for (id, oci_spec) in pending_restarts {
            // Pending → Preparing
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                match instance.state.transition_to(ContainerState::Preparing) {
                    Ok(s) => instance.state = s,
                    Err(_) => continue,
                }
            }

            if self
                .supervisor
                .grill()
                .create(&id, &oci_spec)
                .await
                .is_err()
            {
                continue;
            }

            // Preparing → Starting
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                match instance.state.transition_to(ContainerState::Starting) {
                    Ok(s) => instance.state = s,
                    Err(_) => continue,
                }
            }

            if self.supervisor.grill().start(&id).await.is_err() {
                continue;
            }

            // Starting → HealthWait, then Running if no health checks
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                if let Ok(s) = instance.state.transition_to(ContainerState::HealthWait) {
                    instance.state = s;
                }
                if instance.health_config.is_none()
                    && let Ok(s) = instance.state.transition_to(ContainerState::Running)
                {
                    instance.state = s;
                }
            }
        }
    }

    /// Stop an app's instances.
    async fn stop_app(&mut self, app_name: &str, namespace: &str) -> Result<(), BunError> {
        // Get instance IDs for this app
        let instances: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.app_name == app_name && i.namespace == namespace)
            .map(|i| i.id.clone())
            .collect();

        if instances.is_empty() {
            return Err(BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            });
        }

        // Stop via supervisor
        self.supervisor.stop_app(app_name, namespace).await?;

        // Send stop to runtime
        for id in &instances {
            let _ = self.supervisor.grill().stop(id).await;
        }

        // Transition Stopping → Stopped
        for id in &instances {
            if let Some(instance) = self.supervisor.get_instance_mut(id)
                && instance.state == ContainerState::Stopping
            {
                let _ = instance
                    .state
                    .transition_to(ContainerState::Stopped)
                    .map(|s| {
                        instance.state = s;
                    });
            }
        }

        Ok(())
    }

    /// Get status of all instances.
    async fn get_status(&self) -> Vec<InstanceStatus> {
        let mut statuses = Vec::new();
        for instance in self.supervisor.list_instances() {
            let pid = self.supervisor.grill().pid(&instance.id).await;
            statuses.push(InstanceStatus {
                id: instance.id.0.clone(),
                app_name: instance.app_name.clone(),
                namespace: instance.namespace.clone(),
                state: instance.state.to_string(),
                restart_count: instance.restart_count,
                host_port: instance.host_port,
                pid,
            });
        }
        statuses
    }

    /// Get logs for all instances of an app in a namespace.
    async fn get_logs(&self, app_name: &str, namespace: &str) -> Result<String, BunError> {
        let instance_ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|i| i.app_name == app_name && i.namespace == namespace)
            .map(|i| i.id.clone())
            .collect();

        if instance_ids.is_empty() {
            return Err(BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            });
        }

        let mut all_logs = String::new();
        for id in &instance_ids {
            let logs = self.supervisor.grill().logs(id).await.unwrap_or_default();
            if !logs.is_empty() {
                if instance_ids.len() > 1 {
                    all_logs.push_str(&format!("==> {id} <==\n"));
                }
                all_logs.push_str(&logs);
                if !logs.ends_with('\n') {
                    all_logs.push('\n');
                }
            }
        }
        Ok(all_logs)
    }

    /// Gracefully stop all instances.
    async fn shutdown_all(&mut self) {
        let ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .map(|i| i.id.clone())
            .collect();

        for id in &ids {
            let _ = self.supervisor.grill().stop(id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::mock::MockGrill;

    fn test_agent() -> (
        BunAgent<MockGrill>,
        mpsc::Sender<AgentCommand>,
        CancellationToken,
    ) {
        let (agent, tx, shutdown, _grill) = test_agent_with_grill();
        (agent, tx, shutdown)
    }

    fn test_agent_with_grill() -> (
        BunAgent<MockGrill>,
        mpsc::Sender<AgentCommand>,
        CancellationToken,
        MockGrill,
    ) {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        (agent, tx, shutdown, grill_handle)
    }

    /// Send a Deploy command and collect all events. Returns the list
    /// of events (the last one should be Complete or Error).
    async fn send_deploy(
        tx: &mpsc::Sender<AgentCommand>,
        config: Config,
    ) -> Vec<ApplyEvent> {
        let (event_tx, mut event_rx) = mpsc::channel(64);
        tx.send(AgentCommand::Deploy {
            config,
            events: event_tx,
        })
        .await
        .unwrap();

        let mut events = Vec::new();
        while let Some(e) = event_rx.recv().await {
            events.push(e);
        }
        events
    }

    /// Extract the Complete event from a list of deploy events.
    /// Panics if the last event is an Error or if there are no events.
    fn expect_complete(events: &[ApplyEvent]) -> (usize, &[String]) {
        match events.last().expect("no events received") {
            ApplyEvent::Complete {
                created,
                instances,
            } => (*created, instances),
            ApplyEvent::Error { message } => panic!("deploy failed: {message}"),
            other => panic!("unexpected final event: {other:?}"),
        }
    }

    fn basic_config() -> Config {
        let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#;
        Config::parse(toml_str).unwrap()
    }

    fn config_with_health() -> Config {
        let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [app.web.health]
            path = "/healthz"
        "#;
        Config::parse(toml_str).unwrap()
    }

    #[tokio::test]
    async fn deploy_command_creates_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        let (created, instances) = expect_complete(&events);
        assert_eq!(created, 1);
        assert_eq!(instances, &["web-0"]);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_streams_progress_events() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;

        // Should have progress events before the final Complete
        let progress_count = events
            .iter()
            .filter(|e| matches!(e, ApplyEvent::Progress { .. }))
            .count();
        assert!(progress_count >= 1, "expected progress events");

        let instance_created = events
            .iter()
            .any(|e| matches!(e, ApplyEvent::InstanceCreated { id, .. } if id == "web-0"));
        assert!(instance_created, "expected InstanceCreated for web-0");

        expect_complete(&events);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn status_returns_all_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        // Deploy first
        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        // Then get status
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();

        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].app_name, "web");
        // Without health checks, goes straight to Running
        assert_eq!(statuses[0].state, "running");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn stop_command_stops_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        // Deploy
        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        // Stop
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        resp_rx.await.unwrap().unwrap();

        // Verify stopped
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses[0].state, "stopped");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_with_health_check_starts_in_health_wait() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, config_with_health()).await;
        expect_complete(&events);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let statuses = resp_rx.await.unwrap();
        // The instance should be in health-wait (awaiting first health check)
        // or running (if the mock health check resolved before we queried status).
        // Both are correct — it's a race between the status query and the
        // health check timer.
        let state = &statuses[0].state;
        assert!(
            state == "health-wait" || state == "running",
            "expected health-wait or running, got {state}"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_all_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        shutdown.cancel();
        agent_handle.await.unwrap();
        // Agent ran shutdown_all — grill.stop() was called
    }

    #[tokio::test]
    async fn logs_returns_result_for_deployed_app() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Logs {
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        // MockGrill returns empty logs, but the call should succeed
        assert!(result.is_ok());

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn logs_for_unknown_app_errors() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Logs {
            app_name: "nope".to_string(),
            namespace: "default".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        assert!(result.is_err());

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn stop_unknown_app_errors() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "nope".to_string(),
            namespace: "default".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        assert!(result.is_err());

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    fn job_config() -> Config {
        let toml_str = r#"
            [job.migrate]
            image = "myapp:v1"
            command = ["echo", "done"]
        "#;
        Config::parse(toml_str).unwrap()
    }

    fn mixed_config() -> Config {
        let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [job.migrate]
            image = "myapp:v1"
            command = ["echo", "done"]
        "#;
        Config::parse(toml_str).unwrap()
    }

    #[tokio::test]
    async fn deploy_job_creates_instance() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, job_config()).await;
        let (created, instances) = expect_complete(&events);
        assert_eq!(created, 1);
        assert_eq!(instances, &["migrate-0"]);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_job_starts_in_running() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, job_config()).await;
        expect_complete(&events);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();

        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].app_name, "migrate");
        assert_eq!(statuses[0].state, "running");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_mixed_apps_and_jobs() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, mixed_config()).await;
        let (created, _instances) = expect_complete(&events);
        assert_eq!(created, 2);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();

        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses.len(), 2);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    fn config_with_init_container() -> Config {
        let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [[app.web.init]]
            command = ["echo", "init"]
        "#;
        Config::parse(toml_str).unwrap()
    }

    #[tokio::test]
    async fn deploy_with_init_container_succeeds() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();

        // Pre-configure: init container exits successfully
        let init_id = InstanceId("web-0-init-0".to_string());
        grill.set_state(&init_id, ContainerState::Stopped);
        grill.set_exit_code(&init_id, Some(0));

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, config_with_init_container()).await;
        let (created, _instances) = expect_complete(&events);
        assert_eq!(created, 1);

        // App should reach running after successful init
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses[0].state, "running");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_with_failing_init_container_fails() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();

        // Pre-configure: init container exits with failure
        let init_id = InstanceId("web-0-init-0".to_string());
        grill.set_state(&init_id, ContainerState::Stopped);
        grill.set_exit_code(&init_id, Some(1));

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, config_with_init_container()).await;
        let last = events.last().expect("no events");
        assert!(
            matches!(last, ApplyEvent::Error { .. }),
            "expected Error event, got {last:?}"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }
}
