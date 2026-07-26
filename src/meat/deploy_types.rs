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

    /// Reject a surge/unavailable pair that can't make progress (M7).
    ///
    /// With `max_surge = 0` a rollout may not exceed the replica target, and
    /// with `max_unavailable = 0` it may not fall below it. Both at once
    /// leaves no room to move: you can't start a new instance and you can't
    /// retire an old one. Validated at apply time so it fails the deploy with
    /// an explanation, rather than wedging a rollout that looks live.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_surge == 0 && self.max_unavailable == 0 {
            return Err(
                "max_surge and max_unavailable are both 0, so a rolling deploy can never \
                 start a replacement or retire an old instance — set at least one to 1"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// What a rolling replacement should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollingStep {
    /// Bring up one more new instance.
    StartNew,
    /// Retire one old instance.
    RetireOld,
    /// A replacement is still owed but surge is exhausted by instances that
    /// haven't gone healthy yet. Wait for one of them, don't act.
    Wait,
    /// Every new instance is healthy and every old one is gone.
    Done,
    /// Neither action is permitted by the configured bounds. Only reachable
    /// with `max_surge = 0 && max_unavailable = 0`, which
    /// [`DeployConfig::validate`] rejects — so this exists to fail loudly
    /// rather than spin, not as an expected state.
    Stuck,
}

/// Decide the next step of a rolling replacement (M7).
///
/// The production rolling deploy used to bring up *every* replacement, then
/// retire *every* old instance. That's a valid rollout, and it's the one
/// `max_surge`/`max_unavailable` exist to let you avoid: on a 3-replica app
/// it doubles peak resource use, and the knobs parsed, validated and changed
/// nothing.
///
/// The two bounds are the whole decision:
///
/// - `max_surge` caps how far *above* the target the total may go, so it
///   decides whether another replacement may start.
/// - `max_unavailable` caps how far *below* the target the serving count may
///   fall, so it decides whether an old instance may retire.
///
/// `new_pending` counts instances started but not yet healthy — they consume
/// surge but don't serve. Kept pure so every combination is unit-testable
/// without a runtime.
pub fn plan_rolling_step(
    target: u32,
    new_healthy: u32,
    new_pending: u32,
    old_remaining: u32,
    max_surge: u32,
    max_unavailable: u32,
) -> RollingStep {
    if new_healthy >= target && old_remaining == 0 {
        return RollingStep::Done;
    }

    let serving = new_healthy + old_remaining;
    let total = new_healthy + new_pending + old_remaining;

    // Start a replacement when one is still owed and surge allows it.
    let owed = new_healthy + new_pending < target;
    if owed && total < target.saturating_add(max_surge) {
        return RollingStep::StartNew;
    }

    // Otherwise retire an old instance, provided serving stays within the
    // unavailable budget. `>` not `>=`: retiring drops `serving` by one, and
    // the floor is what we must not go below.
    if old_remaining > 0 && serving > target.saturating_sub(max_unavailable) {
        return RollingStep::RetireOld;
    }

    // A replacement is owed but surge is exhausted. If instances are still
    // coming up, waiting for one is the answer — never `Done`, which would
    // let a caller looping on "not Done" finish with work outstanding.
    if new_pending > 0 {
        return RollingStep::Wait;
    }

    if owed || old_remaining > 0 {
        RollingStep::Stuck
    } else {
        RollingStep::Done
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
    /// Blue-green: starting all green instances.
    StartingGreen,
    /// Blue-green: health checking all green instances.
    HealthCheckingGreen,
    /// Blue-green: performing atomic routing swap.
    RoutingSwitching,
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
    /// Start the deploy (rolling strategy).
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
    /// Blue-green: begin starting green instances.
    GreenStarting,
    /// Blue-green: all green instances started successfully.
    GreenAllStarted,
    /// Blue-green: all green instances passed health checks.
    GreenAllHealthy,
    /// Blue-green: a green instance failed (health or start).
    GreenHealthFailed,
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
            // Pending → start (rolling)
            (DeployPhase::Pending, DeployEvent::Start) => {
                if self.request.pre_deploy_jobs.is_empty() {
                    DeployPhase::Rolling
                } else {
                    DeployPhase::RunningPreDeps
                }
            }
            // Pending → blue-green start
            (DeployPhase::Pending, DeployEvent::GreenStarting) => DeployPhase::StartingGreen,
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

            // Blue-green: StartingGreen → health check or failure
            (DeployPhase::StartingGreen, DeployEvent::GreenAllStarted) => {
                DeployPhase::HealthCheckingGreen
            }
            (DeployPhase::StartingGreen, DeployEvent::GreenHealthFailed) => {
                if self.request.config.auto_rollback {
                    DeployPhase::Reverting
                } else {
                    DeployPhase::Halted
                }
            }
            (DeployPhase::StartingGreen, DeployEvent::Cancel) => DeployPhase::Cancelled,

            // Blue-green: HealthCheckingGreen → routing swap or failure
            (DeployPhase::HealthCheckingGreen, DeployEvent::GreenAllHealthy) => {
                DeployPhase::RoutingSwitching
            }
            (DeployPhase::HealthCheckingGreen, DeployEvent::GreenHealthFailed) => {
                if self.request.config.auto_rollback {
                    DeployPhase::Reverting
                } else {
                    DeployPhase::Halted
                }
            }
            (DeployPhase::HealthCheckingGreen, DeployEvent::Cancel) => DeployPhase::Cancelled,

            // Blue-green: RoutingSwitching → completed
            (DeployPhase::RoutingSwitching, DeployEvent::AllStepsComplete) => {
                DeployPhase::Completed
            }
            (DeployPhase::RoutingSwitching, DeployEvent::Cancel) => DeployPhase::Cancelled,

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
    /// The full app spec deployed, so `relish rollback` can re-apply a
    /// previous version. `None` for entries from paths that don't carry
    /// the spec (e.g. `from_state`, and pre-X3 history).
    #[serde(default)]
    pub spec: Option<Box<crate::config::app::AppSpec>>,
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
            spec: None,
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

    // -- Blue-green phase transitions ---

    #[test]
    fn pending_green_starting_goes_to_starting_green() {
        let mut s = test_state();
        s.transition(DeployEvent::GreenStarting).unwrap();
        assert_eq!(s.phase, DeployPhase::StartingGreen);
    }

    #[test]
    fn starting_green_all_started_goes_to_health_checking() {
        let mut s = test_state();
        s.transition(DeployEvent::GreenStarting).unwrap();
        s.transition(DeployEvent::GreenAllStarted).unwrap();
        assert_eq!(s.phase, DeployPhase::HealthCheckingGreen);
    }

    #[test]
    fn health_checking_green_all_healthy_goes_to_routing() {
        let mut s = test_state();
        s.transition(DeployEvent::GreenStarting).unwrap();
        s.transition(DeployEvent::GreenAllStarted).unwrap();
        s.transition(DeployEvent::GreenAllHealthy).unwrap();
        assert_eq!(s.phase, DeployPhase::RoutingSwitching);
    }

    #[test]
    fn routing_switching_all_steps_complete_goes_to_completed() {
        let mut s = test_state();
        s.transition(DeployEvent::GreenStarting).unwrap();
        s.transition(DeployEvent::GreenAllStarted).unwrap();
        s.transition(DeployEvent::GreenAllHealthy).unwrap();
        s.transition(DeployEvent::AllStepsComplete).unwrap();
        assert_eq!(s.phase, DeployPhase::Completed);
    }

    #[test]
    fn starting_green_failure_with_auto_rollback_goes_to_reverting() {
        let mut s = test_state();
        s.transition(DeployEvent::GreenStarting).unwrap();
        s.transition(DeployEvent::GreenHealthFailed).unwrap();
        assert_eq!(s.phase, DeployPhase::Reverting);
    }

    #[test]
    fn health_checking_green_failure_without_auto_rollback_goes_to_halted() {
        let mut req = test_request();
        req.config.auto_rollback = false;
        let mut s = DeployState::new(DeployId(1), req);
        s.transition(DeployEvent::GreenStarting).unwrap();
        s.transition(DeployEvent::GreenAllStarted).unwrap();
        s.transition(DeployEvent::GreenHealthFailed).unwrap();
        assert_eq!(s.phase, DeployPhase::Halted);
    }

    #[test]
    fn blue_green_phases_not_terminal() {
        assert!(!DeployPhase::StartingGreen.is_terminal());
        assert!(!DeployPhase::HealthCheckingGreen.is_terminal());
        assert!(!DeployPhase::RoutingSwitching.is_terminal());
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

    // -- plan_rolling_step (M7) ----------------------------------------------

    /// Drive a whole rollout through the planner, tracking the counts the
    /// production loop tracks, and return (peak total, minimum serving).
    ///
    /// This is the property that matters: `max_surge` bounds the peak and
    /// `max_unavailable` bounds the trough. Asserting on individual steps
    /// would pin an implementation; asserting on the envelope pins the
    /// contract.
    fn run_rollout(target: u32, existing: u32, max_surge: u32, max_unavailable: u32) -> (u32, u32) {
        let (mut healthy, mut old) = (0u32, existing);
        let mut peak_total = healthy + old;
        let mut min_serving = healthy + old;

        for _ in 0..1000 {
            match plan_rolling_step(target, healthy, 0, old, max_surge, max_unavailable) {
                RollingStep::StartNew => healthy += 1,
                RollingStep::RetireOld => old -= 1,
                RollingStep::Done => {
                    return (peak_total, min_serving);
                }
                other => panic!("unexpected {other:?} at healthy={healthy} old={old}"),
            }
            peak_total = peak_total.max(healthy + old);
            min_serving = min_serving.min(healthy + old);
        }
        panic!("rollout did not converge");
    }

    /// The default (surge 1, unavailable 0) must never exceed one extra
    /// instance and never drop below the target. The old code brought up all
    /// three replacements before retiring anything — peak 6, not 4.
    #[test]
    fn default_surge_keeps_one_spare_and_never_dips() {
        let (peak, min) = run_rollout(3, 3, 1, 0);
        assert_eq!(peak, 4, "default max_surge=1 allows exactly one extra");
        assert_eq!(
            min, 3,
            "default max_unavailable=0 must never dip below target"
        );
    }

    /// The resource-frugal setting: no surge, one instance may be missing. It
    /// retires first, then replaces, so total never exceeds the target.
    #[test]
    fn zero_surge_replaces_in_place() {
        let (peak, min) = run_rollout(3, 3, 0, 1);
        assert_eq!(peak, 3, "max_surge=0 must never exceed the target");
        assert_eq!(min, 2, "max_unavailable=1 permits exactly one gap");
    }

    /// A wide surge is allowed to go fast, which is the old behaviour — now
    /// as an explicit choice rather than the only option.
    #[test]
    fn full_surge_matches_the_old_all_at_once_behaviour() {
        let (peak, min) = run_rollout(3, 3, 3, 0);
        assert_eq!(peak, 6);
        assert_eq!(min, 3);
    }

    #[test]
    fn a_rollout_onto_an_empty_app_just_starts_replicas() {
        let (peak, min) = run_rollout(2, 0, 1, 0);
        assert_eq!(peak, 2);
        assert_eq!(min, 0);
    }

    /// Scaling down mid-rollout: more old instances than the new target.
    #[test]
    fn extra_old_instances_are_retired() {
        let (peak, _) = run_rollout(2, 5, 1, 0);
        assert!(peak >= 5);
        // Convergence (not panicking) is the assertion: every old instance
        // must be retired even when there are more of them than the target.
    }

    #[test]
    fn both_bounds_zero_is_stuck_and_rejected_by_validation() {
        assert_eq!(
            plan_rolling_step(3, 0, 0, 3, 0, 0),
            RollingStep::Stuck,
            "with no surge and no unavailable budget there is no legal move"
        );
        let config = DeployConfig {
            max_surge: 0,
            max_unavailable: 0,
            ..DeployConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(DeployConfig::default().validate().is_ok());
    }

    #[test]
    fn surge_exhausted_by_pending_instances_waits() {
        // One replacement is up but not yet healthy, surge is 1, so there is
        // no room to start another and nothing safe to retire.
        assert_eq!(
            plan_rolling_step(3, 0, 1, 3, 1, 0),
            RollingStep::Wait,
            "waiting is not the same as done — work is still owed"
        );
    }

    proptest::proptest! {
        /// For any target and any bounds that validation permits, a rollout
        /// converges and stays inside both envelopes. This is the invariant
        /// the whole feature exists to provide.
        #[test]
        fn rollouts_respect_both_bounds(
            target in 1u32..6,
            existing in 0u32..6,
            surge in 0u32..4,
            unavailable in 0u32..4,
        ) {
            proptest::prop_assume!(surge > 0 || unavailable > 0);
            let (peak, min) = run_rollout(target, existing, surge, unavailable);
            let ceiling = target + surge;
            proptest::prop_assert!(
                peak <= ceiling.max(existing),
                "peak {peak} exceeded target+surge {ceiling} (existing {existing})"
            );
            let floor = target.saturating_sub(unavailable);
            // A rollout that starts below the floor (scaling up from fewer
            // instances) can't be blamed for starting there.
            let start = existing.min(floor);
            proptest::prop_assert!(
                min >= floor.min(start),
                "serving dropped to {min}, below the floor {floor}"
            );
        }
    }
}
