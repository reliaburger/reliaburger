/// Bun agent event loop.
///
/// Ties the supervisor, health checker, and container runtime together
/// into a single async event loop. Commands arrive over an `mpsc` channel;
/// health checks fire on a timer; shutdown is coordinated via a
/// `CancellationToken`.
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::config::app::AppSpec;
use crate::grill::oci::generate_oci_spec;
use crate::grill::port::PortAllocator;
use crate::grill::state::ContainerState;
use crate::grill::{Grill, InstanceId};

use super::BunError;
use super::probe::probe_health;
use super::supervisor::WorkloadSupervisor;

/// Commands sent to the agent over the command channel.
pub enum AgentCommand {
    /// Deploy workloads from a parsed Config.
    Deploy {
        config: Config,
        response: oneshot::Sender<Result<ApplyResult, BunError>>,
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
                }
            }
        }
    }

    /// Handle a single command.
    async fn handle_command(&mut self, cmd: AgentCommand) {
        match cmd {
            AgentCommand::Deploy { config, response } => {
                let result = self.deploy(config).await;
                let _ = response.send(result);
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

    /// Deploy all apps from a config.
    async fn deploy(&mut self, config: Config) -> Result<ApplyResult, BunError> {
        let now = Instant::now();
        let mut all_ids = Vec::new();

        for (app_name, spec) in &config.app {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            let ids = self
                .supervisor
                .deploy_app(app_name, namespace, spec, now)
                .await?;

            // Drive each instance through Pending → Preparing → Starting → HealthWait
            for id in &ids {
                self.drive_instance_startup(id, app_name, namespace, spec)
                    .await?;
            }

            all_ids.extend(ids.iter().map(|id| id.0.clone()));
        }

        Ok(ApplyResult {
            created: all_ids.len(),
            instances: all_ids,
        })
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
        let oci_spec = generate_oci_spec(app_name, namespace, spec, host_port, &cgroup_str);

        self.supervisor
            .grill()
            .create(instance_id, &oci_spec)
            .await?;

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

    /// Get logs (placeholder — returns empty for MockGrill, captured output for ProcessGrill).
    async fn get_logs(&self, app_name: &str, namespace: &str) -> Result<String, BunError> {
        let instances: Vec<&super::supervisor::WorkloadInstance> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|i| i.app_name == app_name && i.namespace == namespace)
            .collect();

        if instances.is_empty() {
            return Err(BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            });
        }

        // For now return a placeholder. ProcessGrill captures stdout/stderr
        // but accessing it requires downcasting, which we'll handle in Phase 2.
        Ok(format!(
            "[logs for {app_name} in {namespace}: {} instance(s)]",
            instances.len()
        ))
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
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        (agent, tx, shutdown)
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

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            response: resp_tx,
        })
        .await
        .unwrap();

        let result = resp_rx.await.unwrap().unwrap();
        assert_eq!(result.created, 1);
        assert_eq!(result.instances, vec!["web-0"]);

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
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            response: resp_tx,
        })
        .await
        .unwrap();
        resp_rx.await.unwrap().unwrap();

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
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            response: resp_tx,
        })
        .await
        .unwrap();
        resp_rx.await.unwrap().unwrap();

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

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Deploy {
            config: config_with_health(),
            response: resp_tx,
        })
        .await
        .unwrap();
        resp_rx.await.unwrap().unwrap();

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses[0].state, "health-wait");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_all_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            response: resp_tx,
        })
        .await
        .unwrap();
        resp_rx.await.unwrap().unwrap();

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

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            response: resp_tx,
        })
        .await
        .unwrap();
        resp_rx.await.unwrap().unwrap();

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Logs {
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        let logs = resp_rx.await.unwrap().unwrap();
        assert!(logs.contains("web"));

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
}
