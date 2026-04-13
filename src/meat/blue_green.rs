//! Blue-green deploy orchestrator.
//!
//! Unlike rolling deploys (one-at-a-time), blue-green deploys start
//! all new ("green") instances in parallel, health check them all,
//! then atomically swap routing from the old ("blue") fleet to the
//! green fleet. If any green instance fails, the entire green fleet
//! is torn down and blue continues serving.

use super::deploy_types::*;
use super::orchestrator::DeployDriver;
use super::types::{AppId, NodeId};

/// Execute a blue-green deploy using the given driver.
///
/// Returns the terminal `DeployResult` and mutates `state` to reflect
/// all phase transitions and step progress.
pub fn execute_blue_green<D: DeployDriver>(
    state: &mut DeployState,
    driver: &D,
) -> Result<DeployResult, DeployError> {
    // Phase: StartingGreen — start all green instances
    state.transition(DeployEvent::GreenStarting)?;

    let placements = driver.current_placements(&state.request.app_id);

    // Build steps: one per existing instance (blue), or one fresh if none
    if placements.is_empty() {
        state.steps.push(RolloutStep {
            node_id: NodeId::new("local"),
            old_instance: None,
            new_instance: None,
            phase: StepPhase::Pending,
        });
    } else {
        state.steps = placements
            .iter()
            .map(|(node, instance)| RolloutStep {
                node_id: node.clone(),
                old_instance: Some(instance.clone()),
                new_instance: None,
                phase: StepPhase::Pending,
            })
            .collect();
    }

    // Start all green instances
    let mut green_ids: Vec<String> = Vec::new();
    for (i, step) in state.steps.iter_mut().enumerate() {
        step.phase = StepPhase::Starting;
        match driver.start_instance(
            &state.request.app_id,
            &step.node_id,
            &state.request.new_image,
        ) {
            Ok((instance_id, _port)) => {
                step.new_instance = Some(instance_id.clone());
                green_ids.push(instance_id);
            }
            Err(e) => {
                step.phase = StepPhase::Failed;
                // Abort: stop all green instances started so far
                abort_green(driver, &state.request.app_id, &green_ids);
                state.transition(DeployEvent::GreenHealthFailed)?;
                if state.phase == DeployPhase::Reverting {
                    let _ = state.transition(DeployEvent::RollbackComplete);
                }
                return terminal_result(state, Some(e));
            }
        }
        state.current_step = i;
    }

    // Phase: HealthCheckingGreen — health check all green instances
    state.transition(DeployEvent::GreenAllStarted)?;

    let health_timeout = state.request.config.health_timeout;
    for (i, step) in state.steps.iter_mut().enumerate() {
        step.phase = StepPhase::HealthChecking;
        if let Err(e) = driver.await_healthy(
            step.new_instance.as_deref().unwrap_or("unknown"),
            health_timeout,
        ) {
            step.phase = StepPhase::Failed;
            // Abort: stop all green instances
            abort_green(driver, &state.request.app_id, &green_ids);
            state.transition(DeployEvent::GreenHealthFailed)?;
            if state.phase == DeployPhase::Reverting {
                let _ = state.transition(DeployEvent::RollbackComplete);
            }
            return terminal_result(state, Some(e));
        }
        state.current_step = i;
    }

    // Phase: RoutingSwitching — atomic routing swap
    state.transition(DeployEvent::GreenAllHealthy)?;

    // Add all green to routing
    for step in &state.steps {
        if let Some(ref new_id) = step.new_instance {
            driver.add_to_routing(&state.request.app_id.name, new_id)?;
        }
    }

    // Remove all blue from routing
    for step in &state.steps {
        if let Some(ref old_id) = step.old_instance {
            let _ = driver.remove_from_routing(&state.request.app_id.name, old_id);
        }
    }

    // Drain and stop all blue instances
    let drain_timeout = state.request.config.drain_timeout;
    for step in &mut state.steps {
        step.phase = StepPhase::Draining;
        if let Some(ref old_id) = step.old_instance {
            let _ = driver.drain_instance(old_id, drain_timeout);
            let _ = driver.stop_instance(old_id);
        }
        step.phase = StepPhase::Completed;
    }

    state.transition(DeployEvent::AllStepsComplete)?;
    terminal_result(state, None)
}

/// Stop all green instances that have been started (cleanup on failure).
fn abort_green<D: DeployDriver>(driver: &D, app_id: &AppId, green_ids: &[String]) {
    for id in green_ids {
        let _ = driver.remove_from_routing(&app_id.name, id);
        let _ = driver.stop_instance(id);
    }
}

/// Extract the terminal result from the current deploy phase.
fn terminal_result(
    state: &DeployState,
    _original_error: Option<DeployError>,
) -> Result<DeployResult, DeployError> {
    match state.phase {
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

#[cfg(test)]
mod tests {
    use super::super::orchestrator::mock::MockDriver;
    use super::*;

    fn bg_request(image: &str) -> DeployRequest {
        DeployRequest {
            app_id: AppId::new("web", "default"),
            new_image: image.to_string(),
            previous_image: Some("myapp:v1".to_string()),
            config: DeployConfig {
                strategy: DeployStrategy::BlueGreen,
                ..DeployConfig::default()
            },
            pre_deploy_jobs: Vec::new(),
        }
    }

    fn placements(n: usize) -> Vec<(NodeId, String)> {
        (0..n)
            .map(|i| (NodeId::new(format!("node-{i}")), format!("web-{i}")))
            .collect()
    }

    #[test]
    fn blue_green_happy_path_3_replicas() {
        let driver = MockDriver::new(placements(3));
        let mut state = DeployState::new(DeployId(1), bg_request("myapp:v2"));
        let result = execute_blue_green(&mut state, &driver).unwrap();
        assert_eq!(result, DeployResult::Completed);
        assert_eq!(state.phase, DeployPhase::Completed);
        assert_eq!(state.steps.len(), 3);
        assert!(state.steps.iter().all(|s| s.phase == StepPhase::Completed));
        // All green instances should have been assigned
        assert!(state.steps.iter().all(|s| s.new_instance.is_some()));
    }

    #[test]
    fn blue_green_health_failure_aborts() {
        // Second green instance fails health
        let driver = MockDriver::new(placements(3)).fail_health_at(1);
        let mut state = DeployState::new(DeployId(1), bg_request("myapp:v2"));
        let result = execute_blue_green(&mut state, &driver).unwrap();
        assert_eq!(result, DeployResult::RolledBack);
        // Blue instances should still be referenced (not stopped)
        assert!(state.steps[0].old_instance.is_some());
    }

    #[test]
    fn blue_green_health_failure_without_auto_rollback() {
        let mut req = bg_request("myapp:v2");
        req.config.auto_rollback = false;
        let driver = MockDriver::new(placements(3)).fail_health_at(1);
        let mut state = DeployState::new(DeployId(1), req);
        let result = execute_blue_green(&mut state, &driver).unwrap();
        assert_eq!(result, DeployResult::Halted);
    }

    #[test]
    fn blue_green_no_existing_instances() {
        let driver = MockDriver::new(vec![]);
        let mut state = DeployState::new(DeployId(1), bg_request("myapp:v2"));
        let result = execute_blue_green(&mut state, &driver).unwrap();
        assert_eq!(result, DeployResult::Completed);
        assert_eq!(state.steps.len(), 1);
    }

    #[test]
    fn blue_green_start_failure_aborts() {
        let driver = MockDriver::new(placements(3)).fail_start_at(1);
        let mut state = DeployState::new(DeployId(1), bg_request("myapp:v2"));
        let result = execute_blue_green(&mut state, &driver).unwrap();
        // Should have rolled back (first green started, second failed)
        assert!(matches!(
            result,
            DeployResult::RolledBack | DeployResult::Halted
        ));
    }

    #[test]
    fn blue_green_single_replica() {
        let driver = MockDriver::new(placements(1));
        let mut state = DeployState::new(DeployId(1), bg_request("myapp:v2"));
        let result = execute_blue_green(&mut state, &driver).unwrap();
        assert_eq!(result, DeployResult::Completed);
        assert_eq!(state.steps.len(), 1);
        assert_eq!(state.steps[0].phase, StepPhase::Completed);
    }
}
