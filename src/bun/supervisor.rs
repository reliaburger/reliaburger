/// Workload supervisor.
///
/// Manages all workload instances on a single node: creating instances
/// from app specs, tracking their lifecycle state, processing health
/// check results, and deciding when to restart failed instances.
use std::collections::HashMap;
use std::time::Instant;

use crate::config::app::AppSpec;
use crate::config::types::Replicas;
use crate::grill::port::PortAllocator;
use crate::grill::state::ContainerState;
use crate::grill::{Grill, InstanceId};

use super::BunError;
use super::health::{
    HealthCheckConfig, HealthChecker, HealthCounters, HealthStatus, evaluate_result,
};
use super::restart::RestartPolicy;

/// Tracks the runtime state of a single workload instance.
#[derive(Debug)]
pub struct WorkloadInstance {
    /// Unique identifier for this instance.
    pub id: InstanceId,
    /// Name of the app this instance belongs to.
    pub app_name: String,
    /// Namespace of the app.
    pub namespace: String,
    /// Current lifecycle state.
    pub state: ContainerState,
    /// Health check counters for this instance.
    pub health_counters: HealthCounters,
    /// Number of times this instance has been restarted.
    pub restart_count: u32,
    /// When the last restart occurred.
    pub last_restart: Option<Instant>,
    /// Host port allocated for this instance, if any.
    pub host_port: Option<u16>,
    /// When this instance was created.
    pub created_at: Instant,
    /// Restart policy governing this instance.
    pub restart_policy: RestartPolicy,
    /// Health check config, if any. Stored here to avoid borrow conflicts
    /// when the supervisor needs both the instance and the health checker.
    pub health_config: Option<HealthCheckConfig>,
}

/// Manages all workload instances on this node.
///
/// Generic over `G: Grill` so tests can inject a mock runtime without
/// mocking frameworks. The compiler monomorphises this struct for each
/// concrete `G`, so there's no virtual dispatch cost.
pub struct WorkloadSupervisor<G: Grill> {
    grill: G,
    port_allocator: PortAllocator,
    instances: HashMap<InstanceId, WorkloadInstance>,
    health_checker: HealthChecker,
    /// Secondary index: (app_name, namespace) → instance IDs.
    /// Enables O(1) lookup by app without scanning all instances.
    app_instances: HashMap<(String, String), Vec<InstanceId>>,
}

impl<G: Grill> WorkloadSupervisor<G> {
    /// Create a new supervisor with the given runtime and port allocator.
    pub fn new(grill: G, port_allocator: PortAllocator) -> Self {
        Self {
            grill,
            port_allocator,
            instances: HashMap::new(),
            health_checker: HealthChecker::new(),
            app_instances: HashMap::new(),
        }
    }

    /// Deploy an app, creating workload instances in Pending state.
    ///
    /// Creates one instance per replica. For `DaemonSet` mode, creates
    /// a single instance (correct for single-node Phase 1).
    pub async fn deploy_app(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        now: Instant,
    ) -> Result<Vec<InstanceId>, BunError> {
        let replica_count = match spec.replicas {
            Replicas::Fixed(n) => n,
            Replicas::DaemonSet => 1,
        };

        let mut instance_ids = Vec::with_capacity(replica_count as usize);

        for i in 0..replica_count {
            let instance_id = InstanceId(format!("{app_name}-{i}"));

            // Allocate a host port if the app declares one
            let host_port = if spec.port.is_some() {
                Some(self.port_allocator.allocate().await?)
            } else {
                None
            };

            // Resolve health check config if specified
            let health_config = if let Some(ref health_spec) = spec.health {
                let app_port = spec.port.ok_or_else(|| BunError::NoPortForHealthCheck {
                    app_name: app_name.to_string(),
                })?;
                let config = HealthCheckConfig::from_spec(health_spec, app_port);
                // Register with the scheduler
                self.health_checker
                    .register(instance_id.clone(), config.clone(), now);
                Some(config)
            } else {
                None
            };

            let instance = WorkloadInstance {
                id: instance_id.clone(),
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                state: ContainerState::Pending,
                health_counters: HealthCounters::new(),
                restart_count: 0,
                last_restart: None,
                host_port,
                created_at: now,
                restart_policy: RestartPolicy::default(),
                health_config,
            };

            self.instances.insert(instance_id.clone(), instance);
            instance_ids.push(instance_id);
        }

        self.app_instances.insert(
            (app_name.to_string(), namespace.to_string()),
            instance_ids.clone(),
        );

        Ok(instance_ids)
    }

    /// Stop all instances of an app by transitioning Running/Unhealthy → Stopping.
    pub async fn stop_app(&mut self, app_name: &str, namespace: &str) -> Result<(), BunError> {
        let key = (app_name.to_string(), namespace.to_string());
        let ids = self
            .app_instances
            .get(&key)
            .ok_or_else(|| BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            })?
            .clone();

        for id in &ids {
            let instance =
                self.instances
                    .get_mut(id)
                    .ok_or_else(|| BunError::InstanceNotFound {
                        instance_id: id.clone(),
                    })?;

            if matches!(
                instance.state,
                ContainerState::Running | ContainerState::Unhealthy
            ) {
                instance.state = instance.state.transition_to(ContainerState::Stopping)?;
                self.health_checker.unregister(id);
            }
        }

        Ok(())
    }

    /// Get a reference to an instance by ID.
    pub fn get_instance(&self, id: &InstanceId) -> Option<&WorkloadInstance> {
        self.instances.get(id)
    }

    /// Get a mutable reference to an instance by ID.
    pub fn get_instance_mut(&mut self, id: &InstanceId) -> Option<&mut WorkloadInstance> {
        self.instances.get_mut(id)
    }

    /// List all instances.
    pub fn list_instances(&self) -> Vec<&WorkloadInstance> {
        self.instances.values().collect()
    }

    /// Process a health check result for an instance.
    ///
    /// Updates the instance's health counters, then evaluates whether
    /// a state transition is needed. Returns `Some(new_state)` if the
    /// instance's state changed.
    pub fn process_health_result(
        &mut self,
        instance_id: &InstanceId,
        status: HealthStatus,
    ) -> Result<Option<ContainerState>, BunError> {
        let instance =
            self.instances
                .get_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;

        // Update counters
        if status.is_healthy() {
            instance.health_counters.record_healthy();
        } else {
            instance.health_counters.record_unhealthy();
        }

        // Check for state transition
        let config = match &instance.health_config {
            Some(c) => c,
            None => return Ok(None),
        };

        let transition = evaluate_result(status, &instance.health_counters, instance.state, config);

        if let Some(new_state) = transition {
            instance.state = instance.state.transition_to(new_state)?;
            Ok(Some(new_state))
        } else {
            Ok(None)
        }
    }

    /// Check whether an instance should be restarted, and if so,
    /// transition it back to Pending.
    ///
    /// Returns `true` if the restart was initiated, `false` if the
    /// instance isn't eligible (not in a restartable state).
    pub async fn maybe_restart(
        &mut self,
        instance_id: &InstanceId,
        now: Instant,
    ) -> Result<bool, BunError> {
        let instance =
            self.instances
                .get_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;

        // Only restart from Unhealthy or Stopped states
        if !matches!(
            instance.state,
            ContainerState::Unhealthy | ContainerState::Stopped
        ) {
            return Ok(false);
        }

        if !instance
            .restart_policy
            .should_restart(instance.restart_count)
        {
            let max = instance.restart_policy.max_restarts.unwrap_or(0);
            return Err(BunError::RestartLimitExceeded {
                instance_id: instance_id.clone(),
                restart_count: instance.restart_count,
                max_restarts: max,
            });
        }

        // Check backoff timing
        let required_backoff = instance
            .restart_policy
            .compute_backoff(instance.restart_count);
        if let Some(last) = instance.last_restart
            && now.duration_since(last) < required_backoff
        {
            return Ok(false);
        }

        // For Unhealthy, we need to go through Stopping → Stopped → Pending.
        // For Stopped, go directly to Pending.
        if instance.state == ContainerState::Unhealthy {
            instance.state = instance.state.transition_to(ContainerState::Stopping)?;
            instance.state = instance.state.transition_to(ContainerState::Stopped)?;
        }

        instance.state = instance.state.transition_to(ContainerState::Pending)?;
        instance.restart_count += 1;
        instance.last_restart = Some(now);
        instance.health_counters.reset();

        Ok(true)
    }

    /// Access the health checker (e.g. to query next deadline).
    pub fn health_checker(&self) -> &HealthChecker {
        &self.health_checker
    }

    /// Access the underlying runtime.
    #[allow(dead_code)]
    pub fn grill(&self) -> &G {
        &self.grill
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::app::{HealthProtocol, HealthSpec};
    use crate::grill::oci::OciSpec;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // -- MockGrill ------------------------------------------------------------

    /// Records all calls to the Grill trait for test assertions.
    #[derive(Debug, Clone, Default)]
    struct MockGrill {
        calls: Arc<Mutex<Vec<(String, InstanceId)>>>,
    }

    impl MockGrill {
        fn new() -> Self {
            Self::default()
        }

        #[allow(dead_code)]
        fn calls(&self) -> Vec<(String, InstanceId)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Grill for MockGrill {
        async fn create(
            &self,
            instance: &InstanceId,
            _spec: &OciSpec,
        ) -> Result<(), crate::grill::GrillError> {
            self.calls
                .lock()
                .unwrap()
                .push(("create".to_string(), instance.clone()));
            Ok(())
        }

        async fn start(&self, instance: &InstanceId) -> Result<(), crate::grill::GrillError> {
            self.calls
                .lock()
                .unwrap()
                .push(("start".to_string(), instance.clone()));
            Ok(())
        }

        async fn stop(&self, instance: &InstanceId) -> Result<(), crate::grill::GrillError> {
            self.calls
                .lock()
                .unwrap()
                .push(("stop".to_string(), instance.clone()));
            Ok(())
        }

        async fn kill(&self, instance: &InstanceId) -> Result<(), crate::grill::GrillError> {
            self.calls
                .lock()
                .unwrap()
                .push(("kill".to_string(), instance.clone()));
            Ok(())
        }

        async fn state(
            &self,
            instance: &InstanceId,
        ) -> Result<ContainerState, crate::grill::GrillError> {
            self.calls
                .lock()
                .unwrap()
                .push(("state".to_string(), instance.clone()));
            Ok(ContainerState::Running)
        }
    }

    // -- Helpers --------------------------------------------------------------

    fn test_supervisor() -> WorkloadSupervisor<MockGrill> {
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        WorkloadSupervisor::new(grill, port_allocator)
    }

    fn basic_app_spec(port: Option<u16>) -> AppSpec {
        AppSpec {
            image: Some("nginx:latest".to_string()),
            exec: None,
            script: None,
            replicas: Replicas::Fixed(1),
            port,
            health: None,
            memory: None,
            cpu: None,
            gpu: None,
            env: Default::default(),
            config_file: Vec::new(),
            volume: None,
            init: Vec::new(),
            ingress: None,
            placement: None,
            deploy: None,
            firewall: None,
            egress: None,
            autoscale: None,
            namespace: None,
        }
    }

    fn app_spec_with_health(port: u16) -> AppSpec {
        let mut spec = basic_app_spec(Some(port));
        spec.health = Some(HealthSpec {
            path: "/healthz".to_string(),
            port: None,
            protocol: HealthProtocol::Http,
            interval: None,
            timeout: None,
            threshold_unhealthy: Some(3),
            threshold_healthy: Some(1),
            initial_delay: None,
        });
        spec
    }

    fn app_spec_with_replicas(n: u32, port: Option<u16>) -> AppSpec {
        let mut spec = basic_app_spec(port);
        spec.replicas = Replicas::Fixed(n);
        spec
    }

    // -- deploy_app -----------------------------------------------------------

    #[tokio::test]
    async fn deploy_creates_pending_instances() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(Some(8080));

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();
        assert_eq!(ids.len(), 1);

        let instance = sup.get_instance(&ids[0]).unwrap();
        assert_eq!(instance.state, ContainerState::Pending);
        assert_eq!(instance.app_name, "web");
        assert_eq!(instance.namespace, "default");
    }

    #[tokio::test]
    async fn deploy_creates_correct_replica_count() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = app_spec_with_replicas(3, Some(8080));

        let ids = sup.deploy_app("api", "prod", &spec, now).await.unwrap();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], InstanceId("api-0".to_string()));
        assert_eq!(ids[1], InstanceId("api-1".to_string()));
        assert_eq!(ids[2], InstanceId("api-2".to_string()));
    }

    #[tokio::test]
    async fn deploy_allocates_ports_when_declared() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(Some(8080));

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();
        let instance = sup.get_instance(&ids[0]).unwrap();
        assert!(instance.host_port.is_some());
        let port = instance.host_port.unwrap();
        assert!((30000..31000).contains(&port));
    }

    #[tokio::test]
    async fn deploy_no_port_when_none_declared() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(None);

        let ids = sup
            .deploy_app("worker", "default", &spec, now)
            .await
            .unwrap();
        let instance = sup.get_instance(&ids[0]).unwrap();
        assert!(instance.host_port.is_none());
    }

    #[tokio::test]
    async fn deploy_registers_health_check() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = app_spec_with_health(8080);

        sup.deploy_app("web", "default", &spec, now).await.unwrap();
        assert_eq!(sup.health_checker().registered_count(), 1);
    }

    #[tokio::test]
    async fn deploy_health_check_without_port_errors() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let mut spec = basic_app_spec(None);
        spec.health = Some(HealthSpec {
            path: "/healthz".to_string(),
            port: None,
            protocol: HealthProtocol::Http,
            interval: None,
            timeout: None,
            threshold_unhealthy: None,
            threshold_healthy: None,
            initial_delay: None,
        });

        let err = sup
            .deploy_app("web", "default", &spec, now)
            .await
            .unwrap_err();
        assert!(matches!(err, BunError::NoPortForHealthCheck { .. }));
    }

    // -- get/list instances ---------------------------------------------------

    #[tokio::test]
    async fn get_instance_returns_none_for_unknown() {
        let sup = test_supervisor();
        assert!(
            sup.get_instance(&InstanceId("nope-0".to_string()))
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_instances_returns_all() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = app_spec_with_replicas(2, Some(8080));

        sup.deploy_app("web", "default", &spec, now).await.unwrap();
        assert_eq!(sup.list_instances().len(), 2);
    }

    // -- stop_app -------------------------------------------------------------

    #[tokio::test]
    async fn stop_transitions_running_to_stopping() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(Some(8080));

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        // Manually advance instance to Running (simulating lifecycle)
        let instance = sup.get_instance_mut(&ids[0]).unwrap();
        instance.state = ContainerState::Running;

        sup.stop_app("web", "default").await.unwrap();

        let instance = sup.get_instance(&ids[0]).unwrap();
        assert_eq!(instance.state, ContainerState::Stopping);
    }

    #[tokio::test]
    async fn stop_unknown_app_errors() {
        let mut sup = test_supervisor();
        let err = sup.stop_app("nope", "default").await.unwrap_err();
        assert!(matches!(err, BunError::AppNotFound { .. }));
    }

    // -- process_health_result ------------------------------------------------

    #[tokio::test]
    async fn health_result_transitions_health_wait_to_running() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = app_spec_with_health(8080);

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        // Advance to HealthWait
        let instance = sup.get_instance_mut(&ids[0]).unwrap();
        instance.state = ContainerState::HealthWait;

        let result = sup
            .process_health_result(&ids[0], HealthStatus::Healthy)
            .unwrap();
        assert_eq!(result, Some(ContainerState::Running));
    }

    #[tokio::test]
    async fn health_result_transitions_running_to_unhealthy() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = app_spec_with_health(8080);

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        // Advance to Running
        let instance = sup.get_instance_mut(&ids[0]).unwrap();
        instance.state = ContainerState::Running;

        // Need 3 consecutive failures (threshold_unhealthy = 3)
        sup.process_health_result(&ids[0], HealthStatus::Unhealthy)
            .unwrap();
        sup.process_health_result(&ids[0], HealthStatus::Unhealthy)
            .unwrap();
        let result = sup
            .process_health_result(&ids[0], HealthStatus::Unhealthy)
            .unwrap();
        assert_eq!(result, Some(ContainerState::Unhealthy));
    }

    #[tokio::test]
    async fn health_result_recovers_unhealthy_to_running() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = app_spec_with_health(8080);

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        // Advance to Unhealthy
        let instance = sup.get_instance_mut(&ids[0]).unwrap();
        instance.state = ContainerState::Unhealthy;

        let result = sup
            .process_health_result(&ids[0], HealthStatus::Healthy)
            .unwrap();
        assert_eq!(result, Some(ContainerState::Running));
    }

    #[tokio::test]
    async fn health_result_below_threshold_no_transition() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = app_spec_with_health(8080);

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        // Advance to Running
        let instance = sup.get_instance_mut(&ids[0]).unwrap();
        instance.state = ContainerState::Running;

        // Only 1 failure, threshold is 3
        let result = sup
            .process_health_result(&ids[0], HealthStatus::Unhealthy)
            .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn health_result_unknown_instance_errors() {
        let mut sup = test_supervisor();
        let err = sup
            .process_health_result(&InstanceId("nope-0".to_string()), HealthStatus::Healthy)
            .unwrap_err();
        assert!(matches!(err, BunError::InstanceNotFound { .. }));
    }

    // -- maybe_restart --------------------------------------------------------

    #[tokio::test]
    async fn maybe_restart_from_stopped() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(Some(8080));

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        // Advance to Stopped
        let instance = sup.get_instance_mut(&ids[0]).unwrap();
        instance.state = ContainerState::Stopped;

        let restarted = sup.maybe_restart(&ids[0], now).await.unwrap();
        assert!(restarted);

        let instance = sup.get_instance(&ids[0]).unwrap();
        assert_eq!(instance.state, ContainerState::Pending);
        assert_eq!(instance.restart_count, 1);
    }

    #[tokio::test]
    async fn maybe_restart_limit_exceeded() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(Some(8080));

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        let instance = sup.get_instance_mut(&ids[0]).unwrap();
        instance.state = ContainerState::Stopped;
        instance.restart_policy = RestartPolicy::for_job(2);
        instance.restart_count = 2;

        let err = sup.maybe_restart(&ids[0], now).await.unwrap_err();
        assert!(matches!(err, BunError::RestartLimitExceeded { .. }));
    }

    #[tokio::test]
    async fn maybe_restart_not_restartable_state() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(Some(8080));

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        // Pending: not a restartable state
        let restarted = sup.maybe_restart(&ids[0], now).await.unwrap();
        assert!(!restarted);
    }

    #[tokio::test]
    async fn maybe_restart_respects_backoff_timing() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(Some(8080));

        let ids = sup.deploy_app("web", "default", &spec, now).await.unwrap();

        let instance = sup.get_instance_mut(&ids[0]).unwrap();
        instance.state = ContainerState::Stopped;
        instance.restart_count = 1;
        instance.last_restart = Some(now);

        // Too soon: backoff for restart_count=1 is 2s
        let half_second_later = now + Duration::from_millis(500);
        let restarted = sup.maybe_restart(&ids[0], half_second_later).await.unwrap();
        assert!(!restarted);

        // After backoff: should succeed
        let three_seconds_later = now + Duration::from_secs(3);
        let restarted = sup
            .maybe_restart(&ids[0], three_seconds_later)
            .await
            .unwrap();
        assert!(restarted);
    }

    #[tokio::test]
    async fn daemonset_creates_one_instance() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let mut spec = basic_app_spec(None);
        spec.replicas = Replicas::DaemonSet;

        let ids = sup
            .deploy_app("monitor", "system", &spec, now)
            .await
            .unwrap();
        assert_eq!(ids.len(), 1);
    }
}
