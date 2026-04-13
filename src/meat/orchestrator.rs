//! Deploy orchestrator.
//!
//! Drives a rolling deploy through the state machine, one instance
//! at a time. Uses the `DeployDriver` trait to abstract instance
//! operations, making the orchestrator testable with a mock.

use std::time::Duration;

use super::deploy_types::*;
use super::types::{AppId, NodeId};

/// Abstraction over instance operations.
///
/// The orchestrator calls these methods to start/stop instances,
/// check health, and update routing. Tests use a mock; production
/// wires to the local supervisor, routing table, and service map.
pub trait DeployDriver {
    /// Start a new instance of the app on the given node.
    /// Returns the instance ID and optional host port.
    fn start_instance(
        &self,
        app_id: &AppId,
        node_id: &NodeId,
        image: &str,
    ) -> Result<(String, Option<u16>), DeployError>;

    /// Wait for a health check to pass, up to `timeout`.
    fn await_healthy(&self, instance_id: &str, timeout: Duration) -> Result<(), DeployError>;

    /// Add an instance to the routing table (ingress + service map).
    fn add_to_routing(&self, app_name: &str, instance_id: &str) -> Result<(), DeployError>;

    /// Remove an instance from the routing table.
    fn remove_from_routing(&self, app_name: &str, instance_id: &str) -> Result<(), DeployError>;

    /// Drain connections from an instance, waiting up to `timeout`.
    fn drain_instance(&self, instance_id: &str, timeout: Duration) -> Result<(), DeployError>;

    /// Stop (kill) an instance.
    fn stop_instance(&self, instance_id: &str) -> Result<(), DeployError>;

    /// Run a pre-deploy dependency job. Blocks until completion.
    fn run_dependency_job(&self, job_name: &str, image: &str) -> Result<(), DeployError>;

    /// Get current placements for an app (node + instance ID).
    fn current_placements(&self, app_id: &AppId) -> Vec<(NodeId, String)>;
}

/// Drives a deploy through the rolling sequence.
pub struct DeployOrchestrator<D: DeployDriver> {
    pub state: DeployState,
    driver: D,
}

impl<D: DeployDriver> DeployOrchestrator<D> {
    /// Create a new orchestrator for a deploy request.
    pub fn new(id: DeployId, request: DeployRequest, driver: D) -> Self {
        Self {
            state: DeployState::new(id, request),
            driver,
        }
    }

    /// Execute the full deploy sequence.
    ///
    /// Dispatches to the blue-green orchestrator if the strategy is
    /// `BlueGreen`, otherwise runs the rolling deploy path.
    pub fn execute(&mut self) -> Result<DeployResult, DeployError> {
        // Blue-green: delegate to the parallel-start orchestrator
        if self.state.request.config.strategy == DeployStrategy::BlueGreen {
            return super::blue_green::execute_blue_green(&mut self.state, &self.driver);
        }

        // Rolling: start
        self.state.transition(DeployEvent::Start)?;

        // Run pre-deploy dependencies
        if self.state.phase == DeployPhase::RunningPreDeps {
            match self.execute_pre_deps() {
                Ok(()) => self.state.transition(DeployEvent::PreDepsComplete)?,
                Err(e) => {
                    let _ = self.state.transition(DeployEvent::PreDepsFailed);
                    return Err(e);
                }
            }
        }

        // Build rollout steps from current placements
        let placements = self.driver.current_placements(&self.state.request.app_id);
        self.state.steps = placements
            .iter()
            .map(|(node, instance)| RolloutStep {
                node_id: node.clone(),
                old_instance: Some(instance.clone()),
                new_instance: None,
                phase: StepPhase::Pending,
            })
            .collect();

        // If no existing instances, create one fresh step
        if self.state.steps.is_empty() {
            self.state.steps.push(RolloutStep {
                node_id: NodeId::new("local"),
                old_instance: None,
                new_instance: None,
                phase: StepPhase::Pending,
            });
        }

        // Execute rolling steps
        let total = self.state.steps.len();
        for i in 0..total {
            self.state.current_step = i;
            match self.execute_step(i) {
                Ok(()) => {
                    self.state.steps[i].phase = StepPhase::Completed;
                    self.state.transition(DeployEvent::StepCompleted(i))?;
                }
                Err(_e) => {
                    self.state.steps[i].phase = StepPhase::Failed;
                    let _ = self.state.transition(DeployEvent::StepFailed(i));

                    if self.state.phase == DeployPhase::Reverting {
                        match self.execute_rollback(i) {
                            Ok(()) => {
                                let _ = self.state.transition(DeployEvent::RollbackComplete);
                            }
                            Err(_) => {
                                let _ = self.state.transition(DeployEvent::RollbackFailed);
                            }
                        }
                    }

                    return self.terminal_result();
                }
            }
        }

        self.state.transition(DeployEvent::AllStepsComplete)?;
        self.terminal_result()
    }

    /// Run pre-deploy dependency jobs.
    fn execute_pre_deps(&self) -> Result<(), DeployError> {
        for job in &self.state.request.pre_deploy_jobs {
            self.driver
                .run_dependency_job(job, &self.state.request.new_image)?;
        }
        Ok(())
    }

    /// Execute a single rollout step: start new, health check, routing swap, drain old, stop old.
    fn execute_step(&mut self, idx: usize) -> Result<(), DeployError> {
        let step = &self.state.steps[idx];
        let node = step.node_id.clone();
        let app_id = &self.state.request.app_id;
        let image = &self.state.request.new_image;
        let config = &self.state.request.config;

        // 1. Start new instance
        self.state.steps[idx].phase = StepPhase::Starting;
        let (new_id, _port) = self.driver.start_instance(app_id, &node, image)?;
        self.state.steps[idx].new_instance = Some(new_id.clone());

        // 2. Health check
        self.state.steps[idx].phase = StepPhase::HealthChecking;
        self.driver.await_healthy(&new_id, config.health_timeout)?;

        // 3. Routing update (add new, remove old)
        self.state.steps[idx].phase = StepPhase::RoutingUpdate;
        self.driver.add_to_routing(&app_id.name, &new_id)?;
        if let Some(ref old_id) = self.state.steps[idx].old_instance {
            self.driver.remove_from_routing(&app_id.name, old_id)?;
        }

        // 4. Drain old
        self.state.steps[idx].phase = StepPhase::Draining;
        if let Some(ref old_id) = self.state.steps[idx].old_instance {
            let _ = self.driver.drain_instance(old_id, config.drain_timeout);
        }

        // 5. Stop old
        if let Some(ref old_id) = self.state.steps[idx].old_instance {
            let _ = self.driver.stop_instance(old_id);
        }

        Ok(())
    }

    /// Rollback all completed steps (revert instances upgraded before the failure).
    fn execute_rollback(&self, failed_at: usize) -> Result<(), DeployError> {
        let Some(ref prev_image) = self.state.request.previous_image else {
            return Ok(()); // No previous image to rollback to
        };

        for i in (0..failed_at).rev() {
            let step = &self.state.steps[i];
            if step.phase != StepPhase::Completed {
                continue;
            }

            // Stop the new instance
            if let Some(ref new_id) = step.new_instance {
                let _ = self
                    .driver
                    .remove_from_routing(&self.state.request.app_id.name, new_id);
                let _ = self.driver.stop_instance(new_id);
            }

            // Restart the old version
            let (restored_id, _) = self.driver.start_instance(
                &self.state.request.app_id,
                &step.node_id,
                prev_image,
            )?;
            self.driver
                .add_to_routing(&self.state.request.app_id.name, &restored_id)?;
        }

        Ok(())
    }

    /// Extract the terminal result from the current phase.
    fn terminal_result(&self) -> Result<DeployResult, DeployError> {
        match self.state.phase {
            DeployPhase::Completed => Ok(DeployResult::Completed),
            DeployPhase::RolledBack => Ok(DeployResult::RolledBack),
            DeployPhase::Halted => Ok(DeployResult::Halted),
            DeployPhase::Failed => Ok(DeployResult::Failed),
            DeployPhase::Cancelled => Ok(DeployResult::Cancelled),
            _ => Err(DeployError::StartFailed(
                "deploy not in terminal state".to_string(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Mock driver for testing
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::cell::RefCell;

    /// A mock driver that records calls and can be configured to fail.
    ///
    /// Uses separate counters for start and health check calls so it
    /// works with both rolling (interleaved) and blue-green (batched)
    /// deploy strategies.
    pub struct MockDriver {
        placements: Vec<(NodeId, String)>,
        next_instance_id: RefCell<u32>,
        fail_health_at_call: Option<usize>,
        fail_start_at_call: Option<usize>,
        fail_dep_job: Option<String>,
        /// Counts stop_instance calls (legacy, used by rolling step tracking).
        step_counter: RefCell<usize>,
        /// Counts start_instance calls independently.
        start_counter: RefCell<usize>,
        /// Counts await_healthy calls independently.
        health_counter: RefCell<usize>,
    }

    impl MockDriver {
        pub fn new(placements: Vec<(NodeId, String)>) -> Self {
            Self {
                placements,
                next_instance_id: RefCell::new(100),
                fail_health_at_call: None,
                fail_start_at_call: None,
                fail_dep_job: None,
                step_counter: RefCell::new(0),
                start_counter: RefCell::new(0),
                health_counter: RefCell::new(0),
            }
        }

        /// Fail the Nth health check call (0-indexed).
        pub fn fail_health_at(mut self, call: usize) -> Self {
            self.fail_health_at_call = Some(call);
            self
        }

        /// Fail the Nth start_instance call (0-indexed).
        pub fn fail_start_at(mut self, call: usize) -> Self {
            self.fail_start_at_call = Some(call);
            self
        }

        pub fn fail_dep_job(mut self, name: &str) -> Self {
            self.fail_dep_job = Some(name.to_string());
            self
        }
    }

    impl DeployDriver for MockDriver {
        fn start_instance(
            &self,
            _app_id: &AppId,
            _node_id: &NodeId,
            _image: &str,
        ) -> Result<(String, Option<u16>), DeployError> {
            let call = {
                let mut c = self.start_counter.borrow_mut();
                let v = *c;
                *c += 1;
                v
            };
            if self.fail_start_at_call == Some(call) {
                return Err(DeployError::StartFailed("mock start failure".into()));
            }
            let mut id = self.next_instance_id.borrow_mut();
            let instance = format!("instance-{id}");
            *id += 1;
            Ok((instance, Some(8080)))
        }

        fn await_healthy(&self, _instance_id: &str, _timeout: Duration) -> Result<(), DeployError> {
            let call = {
                let mut c = self.health_counter.borrow_mut();
                let v = *c;
                *c += 1;
                v
            };
            if self.fail_health_at_call == Some(call) {
                return Err(DeployError::HealthTimeout("mock health timeout".into()));
            }
            Ok(())
        }

        fn add_to_routing(&self, _app_name: &str, _instance_id: &str) -> Result<(), DeployError> {
            Ok(())
        }

        fn remove_from_routing(
            &self,
            _app_name: &str,
            _instance_id: &str,
        ) -> Result<(), DeployError> {
            Ok(())
        }

        fn drain_instance(
            &self,
            _instance_id: &str,
            _timeout: Duration,
        ) -> Result<(), DeployError> {
            Ok(())
        }

        fn stop_instance(&self, _instance_id: &str) -> Result<(), DeployError> {
            // Increment step counter after a full step cycle
            *self.step_counter.borrow_mut() += 1;
            Ok(())
        }

        fn run_dependency_job(&self, job_name: &str, _image: &str) -> Result<(), DeployError> {
            if self.fail_dep_job.as_deref() == Some(job_name) {
                return Err(DeployError::DependencyFailed(format!(
                    "job {job_name} failed"
                )));
            }
            Ok(())
        }

        fn current_placements(&self, _app_id: &AppId) -> Vec<(NodeId, String)> {
            self.placements.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockDriver;
    use super::*;

    fn request(image: &str) -> DeployRequest {
        DeployRequest {
            app_id: AppId::new("web", "default"),
            new_image: image.to_string(),
            previous_image: Some("myapp:v1".to_string()),
            config: DeployConfig::default(),
            pre_deploy_jobs: Vec::new(),
        }
    }

    fn placements(n: usize) -> Vec<(NodeId, String)> {
        (0..n)
            .map(|i| (NodeId::new(format!("node-{i}")), format!("web-{i}")))
            .collect()
    }

    #[test]
    fn happy_path_3_replicas() {
        let driver = MockDriver::new(placements(3));
        let mut orch = DeployOrchestrator::new(DeployId(1), request("myapp:v2"), driver);
        let result = orch.execute().unwrap();
        assert_eq!(result, DeployResult::Completed);
        assert_eq!(orch.state.phase, DeployPhase::Completed);
        assert_eq!(orch.state.steps.len(), 3);
        assert!(
            orch.state
                .steps
                .iter()
                .all(|s| s.phase == StepPhase::Completed)
        );
    }

    #[test]
    fn happy_path_single_replica() {
        let driver = MockDriver::new(placements(1));
        let mut orch = DeployOrchestrator::new(DeployId(1), request("myapp:v2"), driver);
        let result = orch.execute().unwrap();
        assert_eq!(result, DeployResult::Completed);
    }

    #[test]
    fn no_existing_instances_creates_one() {
        let driver = MockDriver::new(vec![]);
        let mut orch = DeployOrchestrator::new(DeployId(1), request("myapp:v2"), driver);
        let result = orch.execute().unwrap();
        assert_eq!(result, DeployResult::Completed);
        assert_eq!(orch.state.steps.len(), 1);
    }

    #[test]
    fn health_failure_with_auto_rollback() {
        let driver = MockDriver::new(placements(3)).fail_health_at(1);
        let mut orch = DeployOrchestrator::new(DeployId(1), request("myapp:v2"), driver);
        let result = orch.execute().unwrap();
        assert_eq!(result, DeployResult::RolledBack);
        assert_eq!(orch.state.steps[0].phase, StepPhase::Completed);
        assert_eq!(orch.state.steps[1].phase, StepPhase::Failed);
        assert_eq!(orch.state.steps[2].phase, StepPhase::Pending);
    }

    #[test]
    fn health_failure_without_auto_rollback() {
        let mut req = request("myapp:v2");
        req.config.auto_rollback = false;
        let driver = MockDriver::new(placements(3)).fail_health_at(1);
        let mut orch = DeployOrchestrator::new(DeployId(1), req, driver);
        let result = orch.execute().unwrap();
        assert_eq!(result, DeployResult::Halted);
    }

    #[test]
    fn start_failure_at_step() {
        let driver = MockDriver::new(placements(2)).fail_start_at(0);
        let mut orch = DeployOrchestrator::new(DeployId(1), request("myapp:v2"), driver);
        let result = orch.execute().unwrap();
        // First step fails on start, auto_rollback triggers but nothing to rollback
        assert!(matches!(
            result,
            DeployResult::RolledBack | DeployResult::Failed
        ));
    }

    #[test]
    fn dependency_job_success() {
        let mut req = request("myapp:v2");
        req.pre_deploy_jobs = vec!["migrate".to_string()];
        let driver = MockDriver::new(placements(1));
        let mut orch = DeployOrchestrator::new(DeployId(1), req, driver);
        let result = orch.execute().unwrap();
        assert_eq!(result, DeployResult::Completed);
    }

    #[test]
    fn dependency_job_failure() {
        let mut req = request("myapp:v2");
        req.pre_deploy_jobs = vec!["migrate".to_string()];
        let driver = MockDriver::new(placements(1)).fail_dep_job("migrate");
        let mut orch = DeployOrchestrator::new(DeployId(1), req, driver);
        let result = orch.execute();
        assert!(result.is_err());
    }

    #[test]
    fn steps_have_new_instance_ids_after_completion() {
        let driver = MockDriver::new(placements(2));
        let mut orch = DeployOrchestrator::new(DeployId(1), request("myapp:v2"), driver);
        orch.execute().unwrap();
        assert!(orch.state.steps[0].new_instance.is_some());
        assert!(orch.state.steps[1].new_instance.is_some());
        // New IDs should be different from old
        assert_ne!(
            orch.state.steps[0].new_instance.as_deref(),
            orch.state.steps[0].old_instance.as_deref()
        );
    }

    #[test]
    fn rollback_restores_previous_instances() {
        // 3 replicas, step 2 fails health, step 0+1 completed → rollback reverts 0+1
        let driver = MockDriver::new(placements(3)).fail_health_at(2);
        let mut orch = DeployOrchestrator::new(DeployId(1), request("myapp:v2"), driver);
        let result = orch.execute().unwrap();
        assert_eq!(result, DeployResult::RolledBack);
    }
}
