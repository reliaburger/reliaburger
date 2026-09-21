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
    launch_inventory: Arc<tokio::sync::Mutex<Option<Vec<super::RuntimeLaunch>>>>,
    network_references:
        Arc<tokio::sync::Mutex<HashMap<InstanceId, super::runc_intent::NetworkReference>>>,
    honours_cgroup_path: Arc<Mutex<bool>>,
    runtime_kind: Arc<Mutex<crate::grill::records::RuntimeKind>>,
    pid: Arc<Mutex<Option<u32>>>,
    cgroup_paths: Arc<Mutex<HashMap<InstanceId, std::path::PathBuf>>>,
    rootless_network: Arc<Mutex<Option<crate::grill::records::RootlessNetworkRecord>>>,
    exec_outputs: Arc<Mutex<std::collections::VecDeque<String>>>,
    /// Deterministic gate for tests that need `exec()` to remain in flight.
    block_exec: Arc<AtomicBool>,
    block_network_release: Arc<AtomicBool>,
    network_release_started: Arc<tokio::sync::Semaphore>,
    network_release_resume: Arc<tokio::sync::Semaphore>,
    exec_started: Arc<tokio::sync::Semaphore>,
    exec_release: Arc<tokio::sync::Semaphore>,
    /// Deterministic gate for tests that need `create()` to remain in flight.
    block_create: Arc<AtomicBool>,
    create_started: Arc<tokio::sync::Semaphore>,
    create_release: Arc<tokio::sync::Semaphore>,
    /// Deterministic gate before the workload's start returns.
    block_start: Arc<AtomicBool>,
    start_started: Arc<tokio::sync::Semaphore>,
    start_release: Arc<tokio::sync::Semaphore>,
    /// Deterministic gate for testing unfinished deployment rollback.
    block_kill: Arc<AtomicBool>,
    kill_started: Arc<tokio::sync::Semaphore>,
    kill_release: Arc<tokio::sync::Semaphore>,
    /// When set, `stop` records the call but does NOT transition the instance
    /// to `Stopped` — the process ignores SIGTERM. Lets tests prove the
    /// exit-aware stop path escalates to SIGKILL (DEP6).
    ignore_stop: Arc<Mutex<bool>>,
    ignore_kill: Arc<AtomicBool>,
    fail_kill: Arc<AtomicBool>,
    fail_create: Arc<AtomicBool>,
    fail_start: Arc<AtomicBool>,
    fail_state: Arc<AtomicBool>,
    inspection_failures: Arc<Mutex<std::collections::HashSet<InstanceId>>>,
}

impl Default for MockGrill {
    fn default() -> Self {
        Self {
            calls: Arc::default(),
            state_overrides: Arc::default(),
            exit_codes: Arc::default(),
            adopt_results: Arc::default(),
            container_ip: Arc::default(),
            network_references: Arc::default(),
            launch_inventory: Arc::default(),
            honours_cgroup_path: Arc::default(),
            runtime_kind: Arc::new(Mutex::new(crate::grill::records::RuntimeKind::Process)),
            pid: Arc::default(),
            cgroup_paths: Arc::default(),
            rootless_network: Arc::default(),
            exec_outputs: Arc::default(),
            block_exec: Arc::new(AtomicBool::new(false)),
            block_network_release: Arc::new(AtomicBool::new(false)),
            network_release_started: Arc::new(tokio::sync::Semaphore::new(0)),
            network_release_resume: Arc::new(tokio::sync::Semaphore::new(0)),
            exec_started: Arc::new(tokio::sync::Semaphore::new(0)),
            exec_release: Arc::new(tokio::sync::Semaphore::new(0)),
            block_create: Arc::new(AtomicBool::new(false)),
            create_started: Arc::new(tokio::sync::Semaphore::new(0)),
            create_release: Arc::new(tokio::sync::Semaphore::new(0)),
            block_start: Arc::new(AtomicBool::new(false)),
            start_started: Arc::new(tokio::sync::Semaphore::new(0)),
            start_release: Arc::new(tokio::sync::Semaphore::new(0)),
            block_kill: Arc::new(AtomicBool::new(false)),
            kill_started: Arc::new(tokio::sync::Semaphore::new(0)),
            kill_release: Arc::new(tokio::sync::Semaphore::new(0)),
            ignore_stop: Arc::default(),
            ignore_kill: Arc::default(),
            fail_kill: Arc::default(),
            fail_create: Arc::default(),
            fail_start: Arc::default(),
            fail_state: Arc::default(),
            inspection_failures: Arc::default(),
        }
    }
}

impl MockGrill {
    /// Create a new MockGrill.
    pub fn new() -> Self {
        Self::default()
    }

    /// Keep reporting the existing state after an acknowledged kill.
    pub fn set_ignore_kill(&self, value: bool) {
        self.ignore_kill.store(value, Ordering::SeqCst);
    }

    /// Make force-kill requests fail without changing runtime state.
    pub fn set_fail_kill(&self, value: bool) {
        self.fail_kill.store(value, Ordering::SeqCst);
    }

    /// Fail creation after recording the attempted runtime mutation.
    pub fn set_fail_create(&self, value: bool) {
        self.fail_create.store(value, Ordering::SeqCst);
    }

    /// Fail start after recording the attempted runtime mutation.
    pub fn set_fail_start(&self, value: bool) {
        self.fail_start.store(value, Ordering::SeqCst);
    }

    /// Make runtime state inspection fail without proving absence.
    pub fn set_fail_state(&self, value: bool) {
        self.fail_state.store(value, Ordering::SeqCst);
    }

    /// Fail state inspection only for the named instance.
    pub fn set_instance_inspection_failure(&self, instance: &InstanceId, fail: bool) {
        let mut failures = self.inspection_failures.lock().unwrap();
        if fail {
            failures.insert(instance.clone());
        } else {
            failures.remove(instance);
        }
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

    /// Queue deterministic combined outputs for successive `exec()` calls.
    #[allow(dead_code)]
    pub fn set_exec_outputs(&self, outputs: impl IntoIterator<Item = String>) {
        *self.exec_outputs.lock().unwrap() = outputs.into_iter().collect();
    }

    /// Hold future `exec()` calls until [`Self::release_execs`] is called.
    #[allow(dead_code)]
    pub fn block_execs(&self) {
        self.block_exec.store(true, Ordering::SeqCst);
    }

    /// Wait until `count` blocked `exec()` calls have started.
    #[allow(dead_code)]
    pub async fn wait_for_execs(&self, count: u32) {
        let permits = Arc::clone(&self.exec_started)
            .acquire_many_owned(count)
            .await
            .expect("exec gate closed");
        permits.forget();
    }

    /// Release `count` calls held by [`Self::block_execs`].
    #[allow(dead_code)]
    pub fn release_execs(&self, count: usize) {
        self.block_exec.store(false, Ordering::SeqCst);
        self.exec_release.add_permits(count);
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

    /// Hold future `start()` calls until [`Self::release_starts`] is called.
    #[allow(dead_code)]
    pub fn block_starts(&self) {
        self.block_start.store(true, Ordering::SeqCst);
    }

    /// Wait until `count` blocked `start()` calls have started.
    #[allow(dead_code)]
    pub async fn wait_for_starts(&self, count: u32) {
        let permits = Arc::clone(&self.start_started)
            .acquire_many_owned(count)
            .await
            .expect("start gate closed");
        permits.forget();
    }

    /// Release `count` calls held by [`Self::block_starts`].
    #[allow(dead_code)]
    pub fn release_starts(&self, count: usize) {
        self.block_start.store(false, Ordering::SeqCst);
        self.start_release.add_permits(count);
    }

    /// Hold future `kill()` calls until [`Self::release_kills`] is called.
    #[allow(dead_code)]
    pub fn block_kills(&self) {
        self.block_kill.store(true, Ordering::SeqCst);
    }

    /// Wait until `count` blocked `kill()` calls have started.
    #[allow(dead_code)]
    pub async fn wait_for_kills(&self, count: u32) {
        let permits = Arc::clone(&self.kill_started)
            .acquire_many_owned(count)
            .await
            .unwrap();
        permits.forget();
    }

    /// Release held kills and allow subsequent calls through.
    #[allow(dead_code)]
    pub fn release_kills(&self, count: usize) {
        self.block_kill.store(false, Ordering::SeqCst);
        self.kill_release.add_permits(count);
    }

    /// Make `stop()` a no-op on state, simulating a process that ignores
    /// SIGTERM. The exit-aware stop path must then escalate to SIGKILL.
    #[allow(dead_code)]
    pub fn set_ignore_stop(&self, value: bool) {
        *self.ignore_stop.lock().unwrap() = value;
    }
}

impl MockGrill {
    /// Supply a complete original runtime inventory for recovery tests.
    pub async fn set_launch_inventory(&self, launches: Vec<super::RuntimeLaunch>) {
        *self.launch_inventory.lock().await = Some(launches);
    }

    /// Configure an original runtime address reference returned by retention/inspection.
    pub async fn set_network_reference(&self, reference: super::runc_intent::NetworkReference) {
        self.network_references
            .lock()
            .await
            .insert(reference.instance_id.clone(), reference);
    }
}

impl MockGrill {
    /// Pause address release while a test inspects durable permission.
    pub fn block_network_releases(&self) {
        self.block_network_release.store(true, Ordering::SeqCst);
    }
    /// Wait for a paused address release.
    pub async fn wait_for_network_release(&self) {
        self.network_release_started
            .acquire()
            .await
            .unwrap()
            .forget();
    }
    /// Resume one paused address release.
    pub fn resume_network_release(&self) {
        self.block_network_release.store(false, Ordering::SeqCst);
        self.network_release_resume.add_permits(1);
    }
}

impl super::Grill for MockGrill {
    async fn launch_inventory(&self) -> Result<Option<Vec<super::RuntimeLaunch>>, GrillError> {
        Ok(self.launch_inventory.lock().await.clone())
    }

    async fn release_network_reference(
        &self,
        reference: &super::runc_intent::NetworkReference,
    ) -> Result<(), GrillError> {
        self.calls.lock().unwrap().push((
            "release_network_reference".into(),
            reference.instance_id.clone(),
        ));
        if self.block_network_release.load(Ordering::SeqCst) {
            self.network_release_started.add_permits(1);
            self.network_release_resume
                .acquire()
                .await
                .unwrap()
                .forget();
        }
        let mut references = self.network_references.lock().await;
        if references
            .get(&reference.instance_id)
            .is_some_and(|held| held != reference)
        {
            return Err(GrillError::StateUnavailable {
                instance: reference.instance_id.clone(),
                reason: "network reference belongs to another generation".into(),
            });
        }
        references.remove(&reference.instance_id);
        if let Some(launches) = self.launch_inventory.lock().await.as_mut() {
            for launch in launches {
                if launch.instance_id == reference.instance_id {
                    launch.network_reference = Some(
                        super::runc_intent::NetworkReferenceState::Released(reference.clone()),
                    );
                }
            }
        }
        Ok(())
    }

    async fn retain_network_reference(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<super::runc_intent::NetworkReference>, GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("retain_network_reference".into(), instance.clone()));
        Ok(self.network_references.lock().await.get(instance).cloned())
    }

    async fn network_reference(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<super::runc_intent::NetworkReference>, GrillError> {
        Ok(self.network_references.lock().await.get(instance).cloned())
    }

    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
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
        if self.fail_create.load(Ordering::SeqCst) {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "injected create failure".into(),
            });
        }
        if let Some(path) = spec.linux.host_cgroup_path() {
            self.cgroup_paths
                .lock()
                .unwrap()
                .insert(instance.clone(), path);
        }
        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("start".to_string(), instance.clone()));
        if self.block_start.load(Ordering::SeqCst) {
            self.start_started.add_permits(1);
            let permit = self
                .start_release
                .acquire()
                .await
                .expect("start gate closed");
            permit.forget();
        }
        if self.fail_start.load(Ordering::SeqCst) {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "injected start failure".into(),
            });
        }
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
        if self.block_kill.load(Ordering::SeqCst) {
            self.kill_started.add_permits(1);
            let permit = self.kill_release.acquire().await.unwrap();
            permit.forget();
        }
        if self.fail_kill.load(Ordering::SeqCst) {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "injected kill failure".into(),
            });
        }
        if !self.ignore_kill.load(Ordering::SeqCst) {
            self.state_overrides
                .lock()
                .unwrap()
                .insert(instance.clone(), ContainerState::Stopped);
        }
        Ok(())
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("state".to_string(), instance.clone()));
        if self.fail_state.load(Ordering::SeqCst)
            || self.inspection_failures.lock().unwrap().contains(instance)
        {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "injected state inspection failure".into(),
            });
        }
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

    async fn workload_cgroup(&self, instance: &InstanceId) -> Result<Option<u64>, GrillError> {
        if !*self.honours_cgroup_path.lock().unwrap()
            || self.state_overrides.lock().unwrap().get(instance) == Some(&ContainerState::Stopped)
        {
            return Ok(None);
        }
        Ok(self
            .cgroup_paths
            .lock()
            .unwrap()
            .get(instance)
            .and_then(|path| crate::sesame::egress::cgroup_id_of_path(path)))
    }

    async fn pid(&self, _instance: &InstanceId) -> Option<u32> {
        *self.pid.lock().unwrap()
    }

    async fn exec(&self, instance: &InstanceId, _command: &[String]) -> Result<String, GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("exec".to_string(), instance.clone()));
        if self.block_exec.load(Ordering::SeqCst) {
            self.exec_started.add_permits(1);
            let permit = self.exec_release.acquire().await.expect("exec gate closed");
            permit.forget();
        }
        self.exec_outputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })
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
        record: &super::records::InstanceRecord,
    ) -> Result<bool, GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("adopt".to_string(), instance.clone()));
        if self.fail_state.load(Ordering::SeqCst)
            || self.inspection_failures.lock().unwrap().contains(instance)
        {
            return Err(GrillError::StateUnavailable {
                instance: instance.clone(),
                reason: "simulated adoption inspection failure".into(),
            });
        }
        let adopted = self
            .adopt_results
            .lock()
            .unwrap()
            .get(instance)
            .copied()
            .unwrap_or(false);
        if adopted && let Some(path) = record.oci_spec.linux.host_cgroup_path() {
            self.cgroup_paths
                .lock()
                .unwrap()
                .insert(instance.clone(), path);
        }
        Ok(adopted)
    }
}
