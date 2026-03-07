/// Shared mock Grill for tests.
///
/// Records all calls to the `Grill` trait so tests can assert on
/// the sequence and arguments of operations. Supports configurable
/// state and exit code responses for testing job completion and
/// restart scenarios.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::oci::OciSpec;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// Records all calls to the Grill trait for test assertions.
#[derive(Debug, Clone, Default)]
pub struct MockGrill {
    calls: Arc<Mutex<Vec<(String, InstanceId)>>>,
    state_overrides: Arc<Mutex<HashMap<InstanceId, ContainerState>>>,
    exit_codes: Arc<Mutex<HashMap<InstanceId, Option<i32>>>>,
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
}

impl super::Grill for MockGrill {
    async fn create(&self, instance: &InstanceId, _spec: &OciSpec) -> Result<(), GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("create".to_string(), instance.clone()));
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
        Ok(())
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.calls
            .lock()
            .unwrap()
            .push(("kill".to_string(), instance.clone()));
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
}
