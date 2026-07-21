/// Workload supervisor.
///
/// Manages all workload instances on a single node: creating instances
/// from app specs, tracking their lifecycle state, processing health
/// check results, and deciding when to restart failed instances.
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Instant;

use crate::config::app::AppSpec;
use crate::config::job::JobSpec;
use crate::config::types::Replicas;
use crate::grill::oci::OciSpec;
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
    /// Container IP address (set when per-container networking is active).
    /// Used for health checks and service discovery. `None` for ProcessGrill
    /// or when the container shares the host network.
    pub container_ip: Option<Ipv4Addr>,
    /// When this instance was created.
    pub created_at: Instant,
    /// Restart policy governing this instance.
    pub restart_policy: RestartPolicy,
    /// Health check config, if any. Stored here to avoid borrow conflicts
    /// when the supervisor needs both the instance and the health checker.
    pub health_config: Option<HealthCheckConfig>,
    /// Whether this instance is a job (run-to-completion) rather than an app.
    pub is_job: bool,
    /// OCI image reference, e.g. "docker.io/library/nginx:latest".
    pub image: String,
    /// Stored OCI spec for restart re-drive. Set during initial startup.
    pub oci_spec: Option<OciSpec>,
    /// Workload identity (SPIFFE cert + OIDC JWT), if issued.
    /// `None` until the first CSR is signed by the council.
    pub identity: Option<crate::sesame::types::WorkloadIdentity>,
    /// Path to the host directory where identity files are written.
    /// Bind-mounted into the container at `/run/reliaburger/identity/`.
    pub identity_mount: Option<std::path::PathBuf>,
}

/// What the runtime on this node can actually enforce.
///
/// The supervisor refuses a workload up front when the node can't honour
/// what it asks for, rather than starting it under weaker isolation than
/// requested (the D15/M22 "silent lie" the review flagged).
#[derive(Debug, Clone, Copy, Default)]
pub struct PlatformCapabilities {
    /// GPUs the detector found on this node. A workload requesting a GPU on
    /// a zero-GPU node is refused.
    pub gpu_count: u32,
    /// Whether `[resources] gpu_enabled` is set. When false, any
    /// GPU-requesting workload is refused regardless of detected hardware.
    pub gpu_enabled: bool,
    /// Whether this node runs rootless (non-root uid). Rootless runc can't
    /// set cgroup resource limits without a systemd user scope we don't
    /// wire, so a limit-requiring workload is refused rather than run
    /// silently unlimited.
    pub rootless: bool,
    /// Live hooks and runtime behaviour needed for pre-start, dual-stack
    /// egress enforcement.
    pub egress: crate::sesame::egress::EgressEnforcementCapability,
    /// Live `.internal` resolver readiness for this runtime's workload path.
    pub dns: crate::onion::dns::DnsCapability,
}

/// Manages all workload instances on this node.
///
/// Generic over `G: Grill` so tests can inject a mock runtime without
/// mocking frameworks. The compiler monomorphises this struct for each
/// concrete `G`, so there's no virtual dispatch cost.
pub struct WorkloadSupervisor<G: Grill> {
    pub(crate) grill: G,
    pub(crate) port_allocator: PortAllocator,
    pub(crate) instances: HashMap<InstanceId, WorkloadInstance>,
    health_checker: HealthChecker,
    /// Secondary index: (app_name, namespace) → instance IDs.
    /// Enables O(1) lookup by app without scanning all instances.
    pub(crate) app_instances: HashMap<(String, String), Vec<InstanceId>>,
    /// Process workload manager — validates exec binaries against the
    /// allowlist and manages script temp file lifecycle.
    process_manager: crate::grill::process_workload::ProcessManager,
    /// What this node can actually enforce (GPU, rootless limits).
    capabilities: PlatformCapabilities,
}

impl<G: Grill> WorkloadSupervisor<G> {
    /// Create a new supervisor with the given runtime and port allocator.
    ///
    /// Uses the default (deny-by-default) process-workloads policy and no
    /// special platform capabilities. The binary calls
    /// [`with_process_config`](Self::with_process_config) to feed the parsed
    /// `[process_workloads]` config and [`set_capabilities`](Self::set_capabilities)
    /// to record detected GPUs and rootless mode.
    pub fn new(grill: G, port_allocator: PortAllocator) -> Self {
        Self::with_process_config(
            grill,
            port_allocator,
            crate::config::process_workloads::ProcessWorkloadsConfig::default(),
        )
    }

    /// Create a supervisor with a custom process workloads config.
    pub fn with_process_config(
        grill: G,
        port_allocator: PortAllocator,
        process_config: crate::config::process_workloads::ProcessWorkloadsConfig,
    ) -> Self {
        Self {
            grill,
            port_allocator,
            instances: HashMap::new(),
            health_checker: HealthChecker::new(),
            app_instances: HashMap::new(),
            process_manager: crate::grill::process_workload::ProcessManager::new(process_config),
            capabilities: PlatformCapabilities::default(),
        }
    }

    /// Record what this node can enforce (detected GPUs, rootless mode).
    ///
    /// The binary calls this at startup after GPU detection so the
    /// supervisor can refuse workloads the node can't honour.
    pub fn set_capabilities(&mut self, capabilities: PlatformCapabilities) {
        self.capabilities = capabilities;
    }

    /// Refresh the live egress enforcement capability without disturbing
    /// the independently detected GPU and rootless values.
    pub fn set_egress_capability(
        &mut self,
        capability: crate::sesame::egress::EgressEnforcementCapability,
    ) {
        self.capabilities.egress = capability;
    }

    /// Return the DNS capability bound to this runtime.
    pub fn dns_capability(&self) -> crate::onion::dns::DnsCapability {
        self.capabilities.dns
    }

    /// Replace the process-workloads policy (host-exec/script allowlist,
    /// mount isolation, script dir). The binary feeds the parsed
    /// `[process_workloads]` config here so the deny-by-default allowlist
    /// comes from `node.toml`, not the constructor default.
    pub fn set_process_config(
        &mut self,
        config: crate::config::process_workloads::ProcessWorkloadsConfig,
    ) {
        self.process_manager = crate::grill::process_workload::ProcessManager::new(config);
    }

    /// Full admission check for an app: host-exec/script allowlist, GPU
    /// availability, rootless resource limits and egress enforcement.
    /// Returns the first refusal.
    fn admit_app(&self, app_name: &str, spec: &AppSpec) -> Result<(), BunError> {
        self.admit_process_workload(app_name, spec.exec.as_deref(), spec.script.as_deref())?;
        self.admit_gpu(app_name, spec.gpu.unwrap_or(0))?;
        self.admit_rootless_limits(app_name, spec.memory.is_some() || spec.cpu.is_some())?;
        self.admit_egress(app_name, spec.egress.as_ref())?;
        self.admit_dns(app_name)?;
        Ok(())
    }

    /// A DNS-enabled node may not create workloads until the resolver is both
    /// bound and reachable through the selected runtime. Bun normally proves
    /// this before constructing the supervisor; this check is the admission
    /// backstop for live tests and future runtime reconfiguration.
    fn admit_dns(&self, app_name: &str) -> Result<(), BunError> {
        let dns = self.capabilities.dns;
        if !dns.enabled || dns.can_resolve_internal() {
            return Ok(());
        }
        Err(BunError::DeployFailed {
            app_name: app_name.to_string(),
            reason:
                "internal DNS requires a ready responder and a workload-reachable resolver address"
                    .to_string(),
        })
    }

    /// Refuse an allowlist unless the node can install it before start on
    /// both address families. A warning is not a security boundary.
    fn admit_egress(
        &self,
        app_name: &str,
        policy: Option<&crate::config::app::EgressSpec>,
    ) -> Result<(), BunError> {
        let Some(policy) = policy else {
            return Ok(());
        };
        if !policy.allow_franchise.is_empty() {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "cross-cluster egress is not implemented; refusing allow_franchise rather than running it unenforced"
                    .to_string(),
            });
        }
        if policy.allow.is_empty() || self.capabilities.egress.can_enforce_allowlist() {
            return Ok(());
        }
        Err(BunError::DeployFailed {
            app_name: app_name.to_string(),
            reason: "egress allowlist requires live connect4/connect6/sendmsg4/sendmsg6 hooks and a runtime that supports pre-start cgroup programming"
                .to_string(),
        })
    }

    /// Refuse an un-allowlisted host binary or script (D17/H8). Also refuses a
    /// host workload that requests mount isolation, which is documented and on
    /// by default but not yet implemented — see [`admit_process_workload`]'s
    /// mount-isolation guard.
    fn admit_process_workload(
        &self,
        app_name: &str,
        exec: Option<&std::path::Path>,
        script: Option<&str>,
    ) -> Result<(), BunError> {
        let reject =
            |e: crate::grill::process_workload::ProcessWorkloadError| BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!("process workload rejected: {e}"),
            };

        if let Some(exec_path) = exec {
            self.process_manager
                .prepare_exec(exec_path)
                .map_err(reject)?;
        }
        if script.is_some() && !self.process_manager.is_script_allowed() {
            return Err(reject(
                crate::grill::process_workload::ProcessWorkloadError::BinaryNotAllowed {
                    path: std::path::PathBuf::from(
                        crate::grill::process_workload::SCRIPT_INTERPRETER,
                    ),
                },
            ));
        }
        // Mount-namespace isolation for host `exec`/`script` workloads is
        // documented and on by default (on Linux), but it is NOT implemented:
        // the process is spawned with no namespace of its own. Refuse a host
        // workload that requests isolation rather than run it unprotected while
        // signalling that it's contained — such a process reads every other
        // workload's volumes and the node's identity/secret material as root.
        // An operator who accepts an unisolated host process must say so
        // explicitly with `mount_isolation = false`.
        let is_host_workload = exec.is_some() || script.is_some();
        if is_host_workload && self.process_manager.config().mount_isolation {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "process workload requests mount isolation \
                         ([process_workloads] mount_isolation, on by default on Linux) but \
                         mount-namespace isolation is not yet implemented; set \
                         mount_isolation = false to run it unisolated on the host, or run it \
                         as a container instead"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Refuse a GPU-requesting workload the node can't back (D15): either
    /// `gpu_enabled = false` or the detector found no GPU. Makes
    /// `gpu_enabled` effective instead of a silent lie.
    fn admit_gpu(&self, app_name: &str, requested: u32) -> Result<(), BunError> {
        if requested == 0 {
            return Ok(());
        }
        if !self.capabilities.gpu_enabled {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "workload requests a GPU but GPU support is disabled on this node \
                         ([resources] gpu_enabled = false)"
                    .to_string(),
            });
        }
        if self.capabilities.gpu_count < requested {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!(
                    "workload requests {requested} gpu(s) but this node has {} available",
                    self.capabilities.gpu_count
                ),
            });
        }
        Ok(())
    }

    /// Refuse a limit-requiring workload on a rootless node (M22): rootless
    /// runc can't set cgroup CPU/memory limits without a systemd user scope
    /// we don't wire, so the limit would be silently dropped. Fail loudly.
    fn admit_rootless_limits(&self, app_name: &str, has_limits: bool) -> Result<(), BunError> {
        if self.capabilities.rootless && has_limits {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "workload declares cpu/memory limits but this node is rootless and \
                         cannot enforce cgroup limits; run bun as root or drop the limits"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Register an instance's health check with the health checker.
    ///
    /// `deploy_app` does this for fresh deploys; the agent's rolling-redeploy
    /// path (which creates instances directly) calls this so replacement
    /// instances are actually probed.
    pub fn register_health(&mut self, id: InstanceId, config: HealthCheckConfig, now: Instant) {
        self.health_checker.register(id, config, now);
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
        // Refuse anything this node can't honour before we allocate a thing:
        // an un-allowlisted host binary/script, a GPU we don't have, or a
        // resource limit rootless can't enforce.
        self.admit_app(app_name, spec)?;

        let replica_count = match spec.replicas {
            Replicas::Fixed(n) => n,
            Replicas::DaemonSet => 1,
        };

        let mut instance_ids = Vec::with_capacity(replica_count as usize);

        for i in 0..replica_count {
            let instance_id =
                crate::grill::InstanceIdentity::new(namespace, app_name, i).instance_id();

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
                container_ip: None,
                created_at: now,
                restart_policy: RestartPolicy::default(),
                health_config,
                is_job: false,
                image: spec.image.clone().unwrap_or_default(),
                oci_spec: None,
                identity: None,
                identity_mount: None,
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

    /// Deploy a job, creating a single workload instance in Pending state.
    ///
    /// Jobs are run-to-completion tasks: no port allocation, no health
    /// checks, and a finite restart budget (3 retries by default).
    pub async fn deploy_job(
        &mut self,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
        now: Instant,
    ) -> Result<Vec<InstanceId>, BunError> {
        // Same admission gate as apps (jobs have no GPU field, so only the
        // host-exec/script allowlist and rootless-limit checks apply).
        self.admit_process_workload(job_name, spec.exec.as_deref(), spec.script.as_deref())?;
        self.admit_rootless_limits(job_name, spec.memory.is_some() || spec.cpu.is_some())?;

        let instance_id = crate::grill::InstanceIdentity::new(namespace, job_name, 0).instance_id();

        let instance = WorkloadInstance {
            id: instance_id.clone(),
            app_name: job_name.to_string(),
            namespace: namespace.to_string(),
            state: ContainerState::Pending,
            health_counters: HealthCounters::new(),
            restart_count: 0,
            last_restart: None,
            host_port: None,
            container_ip: None,
            created_at: now,
            restart_policy: RestartPolicy::for_job(3),
            health_config: None,
            is_job: true,
            image: spec.image.clone().unwrap_or_default(),
            oci_spec: None,
            identity: None,
            identity_mount: None,
        };

        self.instances.insert(instance_id.clone(), instance);
        self.app_instances.insert(
            (job_name.to_string(), namespace.to_string()),
            vec![instance_id.clone()],
        );

        Ok(vec![instance_id])
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

    /// Remove all instances of an app from the supervisor tracking.
    ///
    /// Kills running processes via the Grill and removes all state.
    /// Used during redeploy to clear stale instances before creating fresh ones.
    pub async fn remove_app(&mut self, app_name: &str, namespace: &str) {
        let key = (app_name.to_string(), namespace.to_string());
        let ids = match self.app_instances.remove(&key) {
            Some(ids) => ids,
            None => return,
        };

        for id in &ids {
            // Force-kill the process (SIGKILL, immediate, sets state to Stopped)
            let _ = self.grill.kill(id).await;
            self.health_checker.unregister(id);
            // Return the instance's host port to the pool before dropping it,
            // otherwise the allocation leaks until the agent restarts.
            if let Some(instance) = self.instances.get(id)
                && let Some(port) = instance.host_port
            {
                let _ = self.port_allocator.release(port).await;
            }
            self.instances.remove(id);
        }
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

    /// Mutable access to the health checker (e.g. to pop due checks).
    pub fn health_checker_mut(&mut self) -> &mut HealthChecker {
        &mut self.health_checker
    }

    /// Access the underlying runtime.
    pub fn grill(&self) -> &G {
        &self.grill
    }

    /// Clone the port allocator. Cheap: the allocation set is behind an
    /// `Arc<Mutex<_>>`, so the clone shares the same live reservation state.
    /// A spawned deploy task allocates ports through this shared handle.
    pub fn port_allocator(&self) -> PortAllocator {
        self.port_allocator.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::app::{HealthProtocol, HealthSpec};
    use crate::grill::mock::MockGrill;
    use std::time::Duration;

    // -- Helpers --------------------------------------------------------------

    fn test_supervisor() -> WorkloadSupervisor<MockGrill> {
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        WorkloadSupervisor::new(grill, port_allocator)
    }

    /// A supervisor whose process policy allowlists the given host binaries.
    fn supervisor_with_allowlist(
        binaries: Vec<std::path::PathBuf>,
    ) -> WorkloadSupervisor<MockGrill> {
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        WorkloadSupervisor::with_process_config(
            grill,
            port_allocator,
            crate::config::process_workloads::ProcessWorkloadsConfig {
                allowed_binaries: binaries,
                mount_isolation: false,
                script_dir: std::env::temp_dir().join("reliaburger-sup-test-scripts"),
            },
        )
    }

    fn basic_app_spec(port: Option<u16>) -> AppSpec {
        AppSpec {
            image: Some("nginx:latest".to_string()),
            command: Vec::new(),
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
            volumes: Vec::new(),
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

    #[tokio::test]
    async fn remove_app_releases_allocated_ports() {
        let mut sup = test_supervisor();
        let spec = app_spec_with_replicas(3, Some(8080));
        sup.deploy_app("web", "default", &spec, Instant::now())
            .await
            .unwrap();
        assert_eq!(
            sup.port_allocator.allocated_count().await,
            3,
            "each replica should allocate a port"
        );

        sup.remove_app("web", "default").await;
        assert_eq!(
            sup.port_allocator.allocated_count().await,
            0,
            "removing the app should release its ports"
        );
    }

    // -- deploy_app -----------------------------------------------------------

    #[tokio::test]
    async fn same_app_name_in_two_namespaces_coexists() {
        // DEP1 regression: `default/api` and `payments/api` used to key the
        // same `api-0`, so deploying the second clobbered the first. With
        // namespace-qualified identities they must coexist independently.
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_app_spec(None);

        let default_ids = sup.deploy_app("api", "default", &spec, now).await.unwrap();
        let payments_ids = sup.deploy_app("api", "payments", &spec, now).await.unwrap();

        assert_eq!(default_ids.len(), 1);
        assert_eq!(payments_ids.len(), 1);
        assert_ne!(
            default_ids[0], payments_ids[0],
            "the two namespaces must produce distinct instance ids"
        );
        assert_eq!(sup.list_instances().len(), 2, "both instances must survive");

        let a = sup
            .get_instance(&default_ids[0])
            .expect("default api alive");
        assert_eq!(a.namespace, "default");
        let b = sup
            .get_instance(&payments_ids[0])
            .expect("payments api alive");
        assert_eq!(b.namespace, "payments");
    }

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
        assert_eq!(ids[0], InstanceId("prod__api-0".to_string()));
        assert_eq!(ids[1], InstanceId("prod__api-1".to_string()));
        assert_eq!(ids[2], InstanceId("prod__api-2".to_string()));
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

    // -- deploy_job -----------------------------------------------------------

    fn basic_job_spec() -> crate::config::job::JobSpec {
        toml::from_str(
            r#"
            image = "myapp:v1"
            command = ["echo", "done"]
        "#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn deploy_job_creates_pending_instance() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_job_spec();

        let ids = sup
            .deploy_job("migrate", "default", &spec, now)
            .await
            .unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], InstanceId("default__migrate-0".to_string()));

        let instance = sup.get_instance(&ids[0]).unwrap();
        assert_eq!(instance.state, ContainerState::Pending);
        assert!(instance.is_job);
        assert!(instance.host_port.is_none());
        assert!(instance.health_config.is_none());
    }

    #[tokio::test]
    async fn deploy_job_has_finite_restart_policy() {
        let mut sup = test_supervisor();
        let now = Instant::now();
        let spec = basic_job_spec();

        let ids = sup
            .deploy_job("migrate", "default", &spec, now)
            .await
            .unwrap();

        let instance = sup.get_instance(&ids[0]).unwrap();
        assert_eq!(instance.restart_policy.max_restarts, Some(3));
    }

    // -- process-workload admission (D17/H8) ---------------------------------

    fn exec_app_spec(binary: &str) -> AppSpec {
        let mut spec = basic_app_spec(None);
        spec.image = None;
        spec.exec = Some(std::path::PathBuf::from(binary));
        spec
    }

    #[tokio::test]
    async fn host_exec_not_allowlisted_is_refused() {
        // Deny-by-default: with no allowlist configured, a host binary deploy
        // is refused rather than run.
        let mut sup = test_supervisor();
        let spec = exec_app_spec("/usr/bin/python3");
        let err = sup
            .deploy_app("script-job", "default", &spec, Instant::now())
            .await
            .unwrap_err();
        assert!(matches!(err, BunError::DeployFailed { .. }));
        assert_eq!(sup.list_instances().len(), 0, "nothing must be created");
    }

    #[tokio::test]
    async fn allowlisted_host_exec_runs() {
        // Policy from config, not a default: the same binary the previous
        // test refused now deploys because it is allowlisted.
        let mut sup = supervisor_with_allowlist(vec![std::path::PathBuf::from("/usr/bin/python3")]);
        let spec = exec_app_spec("/usr/bin/python3");
        let ids = sup
            .deploy_app("runner", "default", &spec, Instant::now())
            .await
            .expect("allowlisted exec should deploy");
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn host_workload_requesting_mount_isolation_is_refused() {
        // Even with the binary allowlisted, a host workload that requests mount
        // isolation (the Linux default) is refused: mount-namespace isolation
        // is not implemented, so admitting it would run an unprotected host
        // process while signalling it is contained. Only an explicit
        // `mount_isolation = false` opt-out (see `allowlisted_host_exec_runs`)
        // admits an unisolated host workload.
        let grill = MockGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut sup = WorkloadSupervisor::with_process_config(
            grill,
            port_allocator,
            crate::config::process_workloads::ProcessWorkloadsConfig {
                allowed_binaries: vec![std::path::PathBuf::from("/usr/bin/python3")],
                mount_isolation: true,
                script_dir: std::env::temp_dir().join("reliaburger-sup-iso-test"),
            },
        );
        let spec = exec_app_spec("/usr/bin/python3");
        let err = sup
            .deploy_app("runner", "default", &spec, Instant::now())
            .await
            .unwrap_err();
        match err {
            BunError::DeployFailed { reason, .. } => {
                assert!(reason.contains("mount_isolation"), "reason: {reason}");
            }
            other => panic!("expected DeployFailed, got {other:?}"),
        }
        assert_eq!(sup.list_instances().len(), 0, "nothing must be created");
    }

    #[tokio::test]
    async fn host_script_refused_without_interpreter_allowlisted() {
        // A `script` workload is host execution of /bin/sh, so deny-by-default
        // applies to it too — this used to slip past the exec-only check.
        let mut sup = test_supervisor();
        let mut spec = basic_app_spec(None);
        spec.image = None;
        spec.script = Some("echo hello".to_string());
        let err = sup
            .deploy_app("scripty", "default", &spec, Instant::now())
            .await
            .unwrap_err();
        assert!(matches!(err, BunError::DeployFailed { .. }));
    }

    #[tokio::test]
    async fn host_script_runs_when_interpreter_allowlisted() {
        let mut sup = supervisor_with_allowlist(vec![std::path::PathBuf::from(
            crate::grill::process_workload::SCRIPT_INTERPRETER,
        )]);
        let mut spec = basic_app_spec(None);
        spec.image = None;
        spec.script = Some("echo hello".to_string());
        let ids = sup
            .deploy_app("scripty", "default", &spec, Instant::now())
            .await
            .expect("allowlisted interpreter should let a script deploy");
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn container_workload_ignores_exec_allowlist() {
        // A normal container app (image, no exec/script) is unaffected by the
        // host-exec allowlist even when it's empty.
        let mut sup = test_supervisor();
        let spec = basic_app_spec(None); // has an image, no exec/script
        let ids = sup
            .deploy_app("web", "default", &spec, Instant::now())
            .await
            .expect("container workloads must not be gated by the host allowlist");
        assert_eq!(ids.len(), 1);
    }

    // -- egress admission -----------------------------------------------------

    #[tokio::test]
    async fn egress_allowlist_refused_without_kernel_enforcement() {
        let mut sup = test_supervisor();
        let mut spec = basic_app_spec(None);
        spec.egress = Some(crate::config::app::EgressSpec {
            allow: vec!["192.0.2.10:443".to_string()],
            allow_franchise: Vec::new(),
        });

        let err = sup
            .deploy_app("payer", "default", &spec, Instant::now())
            .await
            .expect_err("a default node has no live egress hooks");

        assert!(matches!(err, BunError::DeployFailed { .. }));
        assert!(sup.list_instances().is_empty());
    }

    #[tokio::test]
    async fn egress_allowlist_admitted_with_complete_live_capability() {
        let mut sup = test_supervisor();
        sup.set_egress_capability(crate::sesame::egress::EgressEnforcementCapability {
            connect_ipv4: true,
            connect_ipv6: true,
            udp_ipv4: true,
            udp_ipv6: true,
            pre_start: true,
        });
        let mut spec = basic_app_spec(None);
        spec.egress = Some(crate::config::app::EgressSpec {
            allow: vec!["192.0.2.10:443".to_string()],
            allow_franchise: Vec::new(),
        });

        let ids = sup
            .deploy_app("payer", "default", &spec, Instant::now())
            .await
            .expect("all required hooks are live");

        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn unimplemented_franchise_egress_is_refused_not_run_unrestricted() {
        let mut sup = test_supervisor();
        sup.set_egress_capability(crate::sesame::egress::EgressEnforcementCapability {
            connect_ipv4: true,
            connect_ipv6: true,
            udp_ipv4: true,
            udp_ipv6: true,
            pre_start: true,
        });
        let mut spec = basic_app_spec(None);
        spec.egress = Some(crate::config::app::EgressSpec {
            allow: Vec::new(),
            allow_franchise: vec!["redis.prod-west".to_string()],
        });

        let err = sup
            .deploy_app("payer", "default", &spec, Instant::now())
            .await
            .expect_err("franchise egress has no implementation yet");

        assert!(matches!(err, BunError::DeployFailed { .. }));
        assert!(sup.list_instances().is_empty());
    }

    // -- GPU admission (D15) --------------------------------------------------

    #[tokio::test]
    async fn gpu_workload_refused_on_node_without_gpu() {
        let mut sup = test_supervisor();
        sup.set_capabilities(PlatformCapabilities {
            gpu_count: 0,
            gpu_enabled: true,
            rootless: false,
            egress: Default::default(),
            dns: Default::default(),
        });
        let mut spec = basic_app_spec(None);
        spec.gpu = Some(1);
        let err = sup
            .deploy_app("trainer", "default", &spec, Instant::now())
            .await
            .unwrap_err();
        assert!(matches!(err, BunError::DeployFailed { .. }));
    }

    #[tokio::test]
    async fn gpu_workload_refused_when_gpu_disabled() {
        let mut sup = test_supervisor();
        sup.set_capabilities(PlatformCapabilities {
            gpu_count: 4, // hardware present but disabled by config
            gpu_enabled: false,
            rootless: false,
            egress: Default::default(),
            dns: Default::default(),
        });
        let mut spec = basic_app_spec(None);
        spec.gpu = Some(1);
        let err = sup
            .deploy_app("trainer", "default", &spec, Instant::now())
            .await
            .unwrap_err();
        assert!(matches!(err, BunError::DeployFailed { .. }));
    }

    #[tokio::test]
    async fn gpu_workload_runs_when_gpu_available() {
        let mut sup = test_supervisor();
        sup.set_capabilities(PlatformCapabilities {
            gpu_count: 2,
            gpu_enabled: true,
            rootless: false,
            egress: Default::default(),
            dns: Default::default(),
        });
        let mut spec = basic_app_spec(None);
        spec.gpu = Some(1);
        let ids = sup
            .deploy_app("trainer", "default", &spec, Instant::now())
            .await
            .expect("a GPU workload should run where a device exists");
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn non_gpu_workload_unaffected_by_gpu_capability() {
        let mut sup = test_supervisor();
        sup.set_capabilities(PlatformCapabilities {
            gpu_count: 0,
            gpu_enabled: false,
            rootless: false,
            egress: Default::default(),
            dns: Default::default(),
        });
        let spec = basic_app_spec(None); // no gpu request
        let ids = sup
            .deploy_app("web", "default", &spec, Instant::now())
            .await
            .expect("a workload requesting no GPU must not be refused");
        assert_eq!(ids.len(), 1);
    }

    // -- rootless resource limits (M22) --------------------------------------

    #[tokio::test]
    async fn rootless_node_refuses_limit_requiring_workload() {
        let mut sup = test_supervisor();
        sup.set_capabilities(PlatformCapabilities {
            gpu_count: 0,
            gpu_enabled: false,
            rootless: true,
            egress: Default::default(),
            dns: Default::default(),
        });
        let mut spec = basic_app_spec(None);
        spec.memory =
            Some(crate::config::types::ResourceRange::parse("128Mi-512Mi").expect("valid range"));
        let err = sup
            .deploy_app("limited", "default", &spec, Instant::now())
            .await
            .unwrap_err();
        assert!(
            matches!(err, BunError::DeployFailed { .. }),
            "rootless nodes must refuse cgroup-limit workloads, not silently drop the limit"
        );
    }

    #[tokio::test]
    async fn rootless_node_runs_unlimited_workload() {
        let mut sup = test_supervisor();
        sup.set_capabilities(PlatformCapabilities {
            gpu_count: 0,
            gpu_enabled: false,
            rootless: true,
            egress: Default::default(),
            dns: Default::default(),
        });
        let spec = basic_app_spec(None); // no cpu/memory limits
        let ids = sup
            .deploy_app("free", "default", &spec, Instant::now())
            .await
            .expect("a workload with no limits is fine on a rootless node");
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn dns_enabled_but_unready_node_refuses_before_workload_creation() {
        let mut sup = test_supervisor();
        sup.set_capabilities(PlatformCapabilities {
            dns: crate::onion::dns::DnsCapability {
                enabled: true,
                ready: false,
                ipv4: true,
                ipv6: false,
                workload_reachable: true,
            },
            ..Default::default()
        });

        let err = sup
            .deploy_app("client", "default", &basic_app_spec(None), Instant::now())
            .await
            .expect_err("an unready resolver must fail admission");

        assert!(matches!(err, BunError::DeployFailed { .. }));
        assert!(sup.list_instances().is_empty());
    }
}
