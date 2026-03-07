/// Shared mock Grill for tests.
///
/// Records all calls to the `Grill` trait so tests can assert on
/// the sequence and arguments of operations.
use std::sync::{Arc, Mutex};

use super::oci::OciSpec;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// Records all calls to the Grill trait for test assertions.
#[derive(Debug, Clone, Default)]
pub struct MockGrill {
    calls: Arc<Mutex<Vec<(String, InstanceId)>>>,
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
        Ok(ContainerState::Running)
    }
}
