//! Deploy orchestration types.
//!
//! Data structures for the deploy state machine: phases, steps,
//! events, config, history, and errors. These are persisted in Raft
//! for leader failover.

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::types::{AppId, NodeId};

/// Unique identifier for a deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeployId(pub u64);

/// A request to start a deploy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeployRequest {
    /// Which app to deploy.
    pub app_id: AppId,
    /// The new image to deploy.
    pub new_image: String,
    /// The previous image (for rollback).
    pub previous_image: Option<String>,
    /// Deploy configuration.
    pub config: DeployConfig,
    /// Jobs to run before the rolling phase (`run_before`).
    pub pre_deploy_jobs: Vec<String>,
}

/// Deploy configuration parsed from `DeploySpec`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeployConfig {
    /// Rolling or blue-green (only rolling implemented in Phase 7).
    pub strategy: DeployStrategy,
    /// Max instances above target during rollout.
    pub max_surge: u32,
    /// Max instances below target during rollout.
    pub max_unavailable: u32,
    /// How long to wait for connections to drain.
    pub drain_timeout: Duration,
    /// How long to wait for health checks after start.
    pub health_timeout: Duration,
    /// Automatically revert on health check failure.
    pub auto_rollback: bool,
}

impl Default for DeployConfig {
    fn default() -> Self {
        Self {
            strategy: DeployStrategy::Rolling,
            max_surge: 1,
            max_unavailable: 0,
            drain_timeout: Duration::from_secs(30),
            health_timeout: Duration::from_secs(60),
            auto_rollback: true,
        }
    }
}

impl DeployConfig {
    /// Parse from a `DeploySpec` (from app config). Missing fields use defaults.
    pub fn from_spec(spec: &crate::config::app::DeploySpec) -> Self {
        let mut cfg = Self::default();
        if let Some(ref s) = spec.strategy
            && s == "blue-green"
        {
            cfg.strategy = DeployStrategy::BlueGreen;
        }
        if let Some(v) = spec.max_surge {
            cfg.max_surge = v;
        }
        if let Some(v) = spec.max_unavailable {
            cfg.max_unavailable = v;
        }
        if let Some(ref s) = spec.drain_timeout
            && let Some(d) = parse_duration(s)
        {
            cfg.drain_timeout = d;
        }
        if let Some(ref s) = spec.health_timeout
            && let Some(d) = parse_duration(s)
        {
            cfg.health_timeout = d;
        }
        if let Some(v) = spec.auto_rollback {
            cfg.auto_rollback = v;
        }
        cfg
    }
}

/// Deploy strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployStrategy {
    Rolling,
    BlueGreen,
}

// ---------------------------------------------------------------------------
// Deploy phase state machine
// ---------------------------------------------------------------------------

/// The current phase of a deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployPhase {
    /// Waiting to start.
    Pending,
    /// Running pre-deploy jobs (`run_before`).
    RunningPreDeps,
    /// Rolling out new instances one at a time.
    Rolling,
    /// Stopped due to failure (auto_rollback=false).
    Halted,
    /// Actively reverting upgraded instances.
    Reverting,
    /// Successfully reverted all instances.
    RolledBack,
    /// All instances upgraded successfully.
    Completed,
    /// Pre-deploy dependency failed.
    Failed,
    /// Manually cancelled.
    Cancelled,
}

impl DeployPhase {
    /// Whether this phase is terminal (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            DeployPhase::Halted
                | DeployPhase::RolledBack
                | DeployPhase::Completed
                | DeployPhase::Failed
                | DeployPhase::Cancelled
        )
    }
}

/// Events that drive deploy phase transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployEvent {
    /// Start the deploy.
    Start,
    /// All pre-deploy jobs completed successfully.
    PreDepsComplete,
    /// A pre-deploy job failed.
    PreDepsFailed,
    /// A rollout step completed successfully.
    StepCompleted(usize),
    /// A rollout step failed (e.g. health check timeout).
    StepFailed(usize),
    /// All rollout steps completed.
    AllStepsComplete,
    /// Rollback completed successfully.
    RollbackComplete,
    /// Rollback itself failed.
    RollbackFailed,
    /// Deploy was cancelled.
    Cancel,
}

// ---------------------------------------------------------------------------
// Rollout steps
// ---------------------------------------------------------------------------

/// A single step in a rolling deploy (one instance replacement).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutStep {
    /// Which node this step targets.
    pub node_id: NodeId,
    /// The old instance being replaced.
    pub old_instance: Option<String>,
    /// The new instance being created.
    pub new_instance: Option<String>,
    /// Current phase of this step.
    pub phase: StepPhase,
}

/// Phase of a single rollout step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepPhase {
    /// Not started yet.
    Pending,
    /// New instance is being started.
    Starting,
    /// Waiting for health check to pass.
    HealthChecking,
    /// Updating routing (add new, remove old).
    RoutingUpdate,
    /// Draining connections from old instance.
    Draining,
    /// Step completed: old instance stopped, new is live.
    Completed,
    /// Step failed (health check timeout, start error, etc.).
    Failed,
}

impl StepPhase {
    /// Whether this step is terminal.
    pub fn is_terminal(&self) -> bool {
        matches!(self, StepPhase::Completed | StepPhase::Failed)
    }
}

// ---------------------------------------------------------------------------
// Deploy state (the full mutable deploy)
// ---------------------------------------------------------------------------

/// The full state of a deploy operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeployState {
    /// Unique deploy identifier.
    pub id: DeployId,
    /// The deploy request that started this.
    pub request: DeployRequest,
    /// Current phase.
    pub phase: DeployPhase,
    /// Rollout steps (one per instance being replaced).
    pub steps: Vec<RolloutStep>,
    /// When the deploy was created.
    pub created_at: SystemTime,
    /// When the deploy entered its current phase.
    pub phase_changed_at: SystemTime,
    /// Index of the current step being executed.
    pub current_step: usize,
}

/// Errors from deploy operations.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error("invalid transition: {from:?} cannot handle {event:?}")]
    InvalidTransition {
        from: DeployPhase,
        event: DeployEvent,
    },
    #[error("deploy already active for {0}")]
    AlreadyActive(AppId),
    #[error("health check timed out for instance {0}")]
    HealthTimeout(String),
    #[error("instance start failed: {0}")]
    StartFailed(String),
    #[error("routing update failed: {0}")]
    RoutingFailed(String),
    #[error("drain failed: {0}")]
    DrainFailed(String),
    #[error("dependency job failed: {0}")]
    DependencyFailed(String),
    #[error("deploy cancelled")]
    Cancelled,
}

impl DeployState {
    /// Create a new deploy in the Pending phase.
    pub fn new(id: DeployId, request: DeployRequest) -> Self {
        let now = SystemTime::now();
        Self {
            id,
            request,
            phase: DeployPhase::Pending,
            steps: Vec::new(),
            created_at: now,
            phase_changed_at: now,
            current_step: 0,
        }
    }

    /// Transition the deploy phase based on an event.
    pub fn transition(&mut self, event: DeployEvent) -> Result<(), DeployError> {
        let new_phase = match (&self.phase, &event) {
            // Pending → start
            (DeployPhase::Pending, DeployEvent::Start) => {
                if self.request.pre_deploy_jobs.is_empty() {
                    DeployPhase::Rolling
                } else {
                    DeployPhase::RunningPreDeps
                }
            }
            (DeployPhase::Pending, DeployEvent::Cancel) => DeployPhase::Cancelled,

            // RunningPreDeps → deps done or failed
            (DeployPhase::RunningPreDeps, DeployEvent::PreDepsComplete) => DeployPhase::Rolling,
            (DeployPhase::RunningPreDeps, DeployEvent::PreDepsFailed) => DeployPhase::Failed,
            (DeployPhase::RunningPreDeps, DeployEvent::Cancel) => DeployPhase::Cancelled,

            // Rolling → step events
            (DeployPhase::Rolling, DeployEvent::StepCompleted(_)) => DeployPhase::Rolling,
            (DeployPhase::Rolling, DeployEvent::AllStepsComplete) => DeployPhase::Completed,
            (DeployPhase::Rolling, DeployEvent::StepFailed(_)) => {
                if self.request.config.auto_rollback {
                    DeployPhase::Reverting
                } else {
                    DeployPhase::Halted
                }
            }
            (DeployPhase::Rolling, DeployEvent::Cancel) => DeployPhase::Cancelled,

            // Reverting → done
            (DeployPhase::Reverting, DeployEvent::RollbackComplete) => DeployPhase::RolledBack,
            (DeployPhase::Reverting, DeployEvent::RollbackFailed) => DeployPhase::Failed,

            // Invalid transitions
            _ => {
                return Err(DeployError::InvalidTransition {
                    from: self.phase,
                    event,
                });
            }
        };

        self.phase = new_phase;
        self.phase_changed_at = SystemTime::now();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Deploy result and history
// ---------------------------------------------------------------------------

/// Terminal outcome of a deploy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployResult {
    Completed,
    RolledBack,
    Halted,
    Failed,
    Cancelled,
}

/// A summary entry for deploy history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeployHistoryEntry {
    pub id: DeployId,
    pub app_id: AppId,
    pub image: String,
    pub result: DeployResult,
    pub created_at: SystemTime,
    pub completed_at: SystemTime,
    pub steps_completed: usize,
    pub steps_total: usize,
}

impl DeployHistoryEntry {
    /// Create a history entry from a completed deploy state.
    pub fn from_state(state: &DeployState) -> Self {
        let result = match state.phase {
            DeployPhase::Completed => DeployResult::Completed,
            DeployPhase::RolledBack => DeployResult::RolledBack,
            DeployPhase::Halted => DeployResult::Halted,
            DeployPhase::Failed => DeployResult::Failed,
            DeployPhase::Cancelled => DeployResult::Cancelled,
            _ => DeployResult::Failed, // shouldn't happen for terminal states
        };
        let completed = state
            .steps
            .iter()
            .filter(|s| s.phase == StepPhase::Completed)
            .count();
        Self {
            id: state.id,
            app_id: state.request.app_id.clone(),
            image: state.request.new_image.clone(),
            result,
            created_at: state.created_at,
            completed_at: state.phase_changed_at,
            steps_completed: completed,
            steps_total: state.steps.len(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a duration string like "30s", "5m", "1h".
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix('s') {
        secs.parse::<u64>().ok().map(Duration::from_secs)
    } else if let Some(mins) = s.strip_suffix('m') {
        mins.parse::<u64>()
            .ok()
            .map(|m| Duration::from_secs(m * 60))
    } else if let Some(hours) = s.strip_suffix('h') {
        hours
            .parse::<u64>()
            .ok()
            .map(|h| Duration::from_secs(h * 3600))
    } else {
        s.parse::<u64>().ok().map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_request() -> DeployRequest {
        DeployRequest {
            app_id: AppId::new("web", "default"),
            new_image: "myapp:v2".to_string(),
            previous_image: Some("myapp:v1".to_string()),
            config: DeployConfig::default(),
            pre_deploy_jobs: Vec::new(),
        }
    }

    fn test_request_with_deps() -> DeployRequest {
        DeployRequest {
            app_id: AppId::new("web", "default"),
            new_image: "myapp:v2".to_string(),
            previous_image: Some("myapp:v1".to_string()),
            config: DeployConfig::default(),
            pre_deploy_jobs: vec!["migrate".to_string()],
        }
    }

    fn test_state() -> DeployState {
        DeployState::new(DeployId(1), test_request())
    }

    // -- Phase transitions (happy path) ---

    #[test]
    fn pending_start_goes_to_rolling_without_deps() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        assert_eq!(s.phase, DeployPhase::Rolling);
    }

    #[test]
    fn pending_start_goes_to_pre_deps_with_deps() {
        let mut s = DeployState::new(DeployId(1), test_request_with_deps());
        s.transition(DeployEvent::Start).unwrap();
        assert_eq!(s.phase, DeployPhase::RunningPreDeps);
    }

    #[test]
    fn pre_deps_complete_goes_to_rolling() {
        let mut s = DeployState::new(DeployId(1), test_request_with_deps());
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::PreDepsComplete).unwrap();
        assert_eq!(s.phase, DeployPhase::Rolling);
    }

    #[test]
    fn rolling_step_complete_stays_rolling() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::StepCompleted(0)).unwrap();
        assert_eq!(s.phase, DeployPhase::Rolling);
    }

    #[test]
    fn rolling_all_steps_complete_goes_to_completed() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::AllStepsComplete).unwrap();
        assert_eq!(s.phase, DeployPhase::Completed);
    }

    // -- Failure paths ---

    #[test]
    fn pre_deps_failed_goes_to_failed() {
        let mut s = DeployState::new(DeployId(1), test_request_with_deps());
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::PreDepsFailed).unwrap();
        assert_eq!(s.phase, DeployPhase::Failed);
    }

    #[test]
    fn step_failed_with_auto_rollback_goes_to_reverting() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::StepFailed(1)).unwrap();
        assert_eq!(s.phase, DeployPhase::Reverting);
    }

    #[test]
    fn step_failed_without_auto_rollback_goes_to_halted() {
        let mut req = test_request();
        req.config.auto_rollback = false;
        let mut s = DeployState::new(DeployId(1), req);
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::StepFailed(1)).unwrap();
        assert_eq!(s.phase, DeployPhase::Halted);
    }

    #[test]
    fn reverting_complete_goes_to_rolled_back() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::StepFailed(0)).unwrap();
        s.transition(DeployEvent::RollbackComplete).unwrap();
        assert_eq!(s.phase, DeployPhase::RolledBack);
    }

    #[test]
    fn reverting_failed_goes_to_failed() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::StepFailed(0)).unwrap();
        s.transition(DeployEvent::RollbackFailed).unwrap();
        assert_eq!(s.phase, DeployPhase::Failed);
    }

    // -- Cancel ---

    #[test]
    fn pending_cancel() {
        let mut s = test_state();
        s.transition(DeployEvent::Cancel).unwrap();
        assert_eq!(s.phase, DeployPhase::Cancelled);
    }

    #[test]
    fn rolling_cancel() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::Cancel).unwrap();
        assert_eq!(s.phase, DeployPhase::Cancelled);
    }

    // -- Invalid transitions ---

    #[test]
    fn completed_cannot_transition() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::AllStepsComplete).unwrap();
        let err = s.transition(DeployEvent::Start).unwrap_err();
        assert!(matches!(err, DeployError::InvalidTransition { .. }));
    }

    #[test]
    fn halted_cannot_transition() {
        let mut req = test_request();
        req.config.auto_rollback = false;
        let mut s = DeployState::new(DeployId(1), req);
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::StepFailed(0)).unwrap();
        assert!(s.transition(DeployEvent::Start).is_err());
    }

    #[test]
    fn rolling_cannot_handle_pre_deps() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        assert!(s.transition(DeployEvent::PreDepsComplete).is_err());
    }

    // -- Terminal state checks ---

    #[test]
    fn terminal_phases() {
        assert!(DeployPhase::Completed.is_terminal());
        assert!(DeployPhase::RolledBack.is_terminal());
        assert!(DeployPhase::Halted.is_terminal());
        assert!(DeployPhase::Failed.is_terminal());
        assert!(DeployPhase::Cancelled.is_terminal());
        assert!(!DeployPhase::Pending.is_terminal());
        assert!(!DeployPhase::Rolling.is_terminal());
    }

    #[test]
    fn step_phase_terminal() {
        assert!(StepPhase::Completed.is_terminal());
        assert!(StepPhase::Failed.is_terminal());
        assert!(!StepPhase::Pending.is_terminal());
        assert!(!StepPhase::HealthChecking.is_terminal());
    }

    // -- DeployConfig ---

    #[test]
    fn deploy_config_defaults() {
        let cfg = DeployConfig::default();
        assert_eq!(cfg.strategy, DeployStrategy::Rolling);
        assert_eq!(cfg.max_surge, 1);
        assert_eq!(cfg.max_unavailable, 0);
        assert!(cfg.auto_rollback);
    }

    #[test]
    fn deploy_config_from_spec() {
        let spec = crate::config::app::DeploySpec {
            strategy: Some("rolling".to_string()),
            max_surge: Some(2),
            max_unavailable: Some(1),
            drain_timeout: Some("60s".to_string()),
            health_timeout: Some("2m".to_string()),
            auto_rollback: Some(false),
        };
        let cfg = DeployConfig::from_spec(&spec);
        assert_eq!(cfg.max_surge, 2);
        assert_eq!(cfg.max_unavailable, 1);
        assert_eq!(cfg.drain_timeout, Duration::from_secs(60));
        assert_eq!(cfg.health_timeout, Duration::from_secs(120));
        assert!(!cfg.auto_rollback);
    }

    // -- Duration parsing ---

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
    }

    #[test]
    fn parse_duration_hours() {
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn parse_duration_bare_number() {
        assert_eq!(parse_duration("42"), Some(Duration::from_secs(42)));
    }

    #[test]
    fn parse_duration_invalid() {
        assert_eq!(parse_duration("abc"), None);
    }

    // -- History entry ---

    #[test]
    fn history_entry_from_completed_state() {
        let mut s = test_state();
        s.steps = vec![
            RolloutStep {
                node_id: NodeId::new("n1"),
                old_instance: None,
                new_instance: None,
                phase: StepPhase::Completed,
            },
            RolloutStep {
                node_id: NodeId::new("n2"),
                old_instance: None,
                new_instance: None,
                phase: StepPhase::Completed,
            },
        ];
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::AllStepsComplete).unwrap();

        let entry = DeployHistoryEntry::from_state(&s);
        assert_eq!(entry.result, DeployResult::Completed);
        assert_eq!(entry.steps_completed, 2);
        assert_eq!(entry.steps_total, 2);
    }

    // -- Serde round-trips ---

    #[test]
    fn deploy_state_serde_round_trip() {
        let s = test_state();
        let json = serde_json::to_string(&s).unwrap();
        let decoded: DeployState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, s.id);
        assert_eq!(decoded.phase, s.phase);
    }

    #[test]
    fn deploy_history_serde_round_trip() {
        let mut s = test_state();
        s.transition(DeployEvent::Start).unwrap();
        s.transition(DeployEvent::AllStepsComplete).unwrap();
        let entry = DeployHistoryEntry::from_state(&s);
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: DeployHistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.result, DeployResult::Completed);
    }
}
