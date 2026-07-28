/// Shared mock Grill for tests.
///
/// Records all calls to the `Grill` trait so tests can assert on
/// the sequence and arguments of operations. Supports configurable
/// state and exit code responses for testing job completion and
/// restart scenarios.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::oci::OciSpec;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// Records all calls to the Grill trait for test assertions.
#[derive(Debug, Clone)]
pub struct MockGrill {
    calls: Arc<Mutex<Vec<(String, InstanceId)>>>,
    state_overrides: Arc<Mutex<HashMap<InstanceId, ContainerState>>>,
    exit_codes: Arc<Mutex<HashMap<InstanceId, Option<i32>>>>,
    adopt_results: Arc<Mutex<HashMap<InstanceId, bool>>>,
    container_ip: Arc<Mutex<Option<std::net::Ipv4Addr>>>,
    honours_cgroup_path: Arc<Mutex<bool>>,
    runtime_kind: Arc<Mutex<crate::grill::records::RuntimeKind>>,
    pid: Arc<Mutex<Option<u32>>>,
    rootless_network: Arc<Mutex<Option<crate::grill::records::RootlessNetworkRecord>>>,
    /// Deterministic gate for tests that need `create()` to remain in flight.
    block_create: Arc<AtomicBool>,
    create_started: Arc<tokio::sync::Semaphore>,
    create_release: Arc<tokio::sync::Semaphore>,
    /// When set, `stop` records the call but does NOT transition the instance
    /// to `Stopped` — the process ignores SIGTERM. Lets tests prove the
    /// exit-aware stop path escalates to SIGKILL (DEP6).
    ignore_stop: Arc<Mutex<bool>>,
}

impl Default for MockGrill {
    fn default() -> Self {
        Self {
            calls: Arc::default(),
            state_overrides: Arc::default(),
            exit_codes: Arc::default(),
            adopt_results: Arc::default(),
            container_ip: Arc::default(),
            honours_cgroup_path: Arc::default(),
            runtime_kind: Arc::new(Mutex::new(crate::grill::records::RuntimeKind::Process)),
            pid: Arc::default(),
            rootless_network: Arc::default(),
            block_create: Arc::new(AtomicBool::new(false)),
            create_started: Arc::new(tokio::sync::Semaphore::new(0)),
            create_release: Arc::new(tokio::sync::Semaphore::new(0)),
            ignore_stop: Arc::default(),
        }
    }
}

impl MockGrill {
    /// Create a new MockGrill.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a clone of all recorded calls.
    #[allow(dead_code)]
    pub fn calls(&self) -> Vec<(String, InstanceId)> {
        self.calls.lock().unwrap().clone()
    }

    /// Set the state that `state()` will return for a specific instance.
    #[allow(dead_code)]
    pub fn set_state(&self, instance: &InstanceId, state: ContainerState) {
        self.state_overrides
            .lock()
            .unwrap()
            .insert(instance.clone(), state);
    }

    /// Set the exit code that `exit_code()` will return for a specific instance.
    #[allow(dead_code)]
    pub fn set_exit_code(&self, instance: &InstanceId, code: Option<i32>) {
        self.exit_codes
            .lock()
            .unwrap()
            .insert(instance.clone(), code);
    }

    /// Set whether `adopt()` will succeed for a specific instance.
    /// Unconfigured instances decline adoption (like a dead process).
    #[allow(dead_code)]
    pub fn set_adopt_result(&self, instance: &InstanceId, adopted: bool) {
        self.adopt_results
            .lock()
            .unwrap()
            .insert(instance.clone(), adopted);
    }

    /// Make `container_ip()` report `ip` for every instance, simulating a
    /// runtime with per-container networking.
    #[allow(dead_code)]
    pub fn set_container_ip(&self, ip: std::net::Ipv4Addr) {
        *self.container_ip.lock().unwrap() = Some(ip);
    }

    /// Make `honours_cgroup_path()` report `value`, simulating a runtime
    /// (root-mode runc) that places workloads into the OCI `cgroupsPath`
    /// — which enables the agent's pre-start egress programming path.
    #[allow(dead_code)]
    pub fn set_honours_cgroup_path(&self, value: bool) {
        *self.honours_cgroup_path.lock().unwrap() = value;
    }

    /// Make `runtime_kind()` report `kind`, so tests can model a runc/Apple
    /// node (which enforces cgroup limits) versus the process runtime.
    #[allow(dead_code)]
    pub fn set_runtime_kind(&self, kind: crate::grill::records::RuntimeKind) {
        *self.runtime_kind.lock().unwrap() = kind;
    }

    /// Make `pid()` report a live test process so record-persistence paths run.
    #[allow(dead_code)]
    pub fn set_pid(&self, pid: u32) {
        *self.pid.lock().unwrap() = Some(pid);
    }

    /// Make adoption records include rootless userspace-network ownership.
    #[allow(dead_code)]
    pub fn set_rootless_network(&self, network: crate::grill::records::RootlessNetworkRecord) {
        *self.rootless_network.lock().unwrap() = Some(network);
    }

    /// Hold future `create()` calls until [`Self::release_creates`] is called.
    #[allow(dead_code)]
    pub fn block_creates(&self) {
        self.block_create.store(true, Ordering::SeqCst);
    }

    /// Wait until `count` blocked `create()` calls have started.
    #[allow(dead_code)]
    pub async fn wait_for_creates(&self, count: u32) {
        let permits = Arc::clone(&self.create_started)
            .acquire_many_owned(count)
            .await
            .expect("create gate closed");
        permits.forget();
    }

    /// Release `count` calls held by [`Self::block_creates`].
    #[allow(dead_code)]
    pub fn release_creates(&self, count: usize) {
        self.block_create.store(false, Ordering::SeqCst);
        self.create_release.add_permits(count);
    }

    /// Make `stop()` a no-op on state, simulating a process that ignores
    /// SIGTERM. The exit-aware stop path must then escalate to SIGKILL.
    #[allow(dead_code)]
    pub fn set_ignore_stop(&self, value: bool) {
        *self.ignore_stop.lock().unwrap() = value;
    }
}

impl super::Grill for MockGrill {
    async fn create(&self, instance: &InstanceId, _spec: &OciSpec) -> Result<(), GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("create".to_string(), instance.clone()));
        if self.block_create.load(Ordering::SeqCst) {
            self.create_started.add_permits(1);
            let permit = self
                .create_release
                .acquire()
                .await
                .expect("create gate closed");
            permit.forget();
        }
        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("start".to_string(), instance.clone()));
        Ok(())
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("stop".to_string(), instance.clone()));
        // A process that ignores SIGTERM stays as-is; the exit-aware stop path
        // must escalate to kill(). Otherwise reflect the stop in state (unless
        // a test pinned a specific state) so callers that poll for exit observe
        // it.
        if !*self.ignore_stop.lock().unwrap() {
            self.state_overrides
                .lock()
                .unwrap()
                .entry(instance.clone())
                .or_insert(ContainerState::Stopped);
        }
        Ok(())
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("kill".to_string(), instance.clone()));
        self.state_overrides
            .lock()
            .unwrap()
            .insert(instance.clone(), ContainerState::Stopped);
        Ok(())
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("state".to_string(), instance.clone()));
        let overrides = self.state_overrides.lock().unwrap();
        if let Some(&state) = overrides.get(instance) {
            return Ok(state);
        }
        Ok(ContainerState::Running)
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        let codes = self.exit_codes.lock().unwrap();
        codes.get(instance).copied().flatten()
    }

    async fn container_ip(&self, _instance: &InstanceId) -> Option<std::net::Ipv4Addr> {
        *self.container_ip.lock().unwrap()
    }

    fn runtime_kind(&self) -> crate::grill::records::RuntimeKind {
        *self.runtime_kind.lock().unwrap()
    }

    fn honours_cgroup_path(&self) -> bool {
        *self.honours_cgroup_path.lock().unwrap()
    }

    async fn pid(&self, _instance: &InstanceId) -> Option<u32> {
        *self.pid.lock().unwrap()
    }

    async fn rootless_network_record(
        &self,
        _instance: &InstanceId,
    ) -> Option<crate::grill::records::RootlessNetworkRecord> {
        self.rootless_network.lock().unwrap().clone()
    }

    async fn adopt(
        &self,
        instance: &InstanceId,
        _record: &super::records::InstanceRecord,
    ) -> Result<bool, GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("adopt".to_string(), instance.clone()));
        Ok(self
            .adopt_results
            .lock()
            .unwrap()
            .get(instance)
            .copied()
            .unwrap_or(false))
    }
}
