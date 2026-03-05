/// Container lifecycle state machine.
///
/// Every workload instance (container or process) progresses through
/// these states. Transitions are validated: attempting an invalid
/// transition returns an error rather than silently corrupting state.
use std::fmt;

/// The lifecycle state of a workload instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerState {
    /// Spec received from scheduler. Preparation has not started.
    Pending,
    /// Image is being pulled (container) or binary is being validated (process).
    Preparing,
    /// Init containers are running sequentially.
    Initialising,
    /// The main process is starting.
    Starting,
    /// Running but the initial health check has not yet passed.
    HealthWait,
    /// Running and healthy. Registered in the service map.
    Running,
    /// Health check failed threshold_unhealthy consecutive times.
    Unhealthy,
    /// Graceful shutdown in progress (draining connections).
    Stopping,
    /// Process exited, resources cleaned up.
    Stopped,
    /// Permanent failure (exceeded restart limits, init container crash).
    Failed,
}

impl ContainerState {
    /// Returns whether transitioning from `self` to `next` is valid.
    pub fn can_transition_to(self, next: ContainerState) -> bool {
        matches!(
            (self, next),
            (ContainerState::Pending, ContainerState::Preparing)
                | (ContainerState::Preparing, ContainerState::Initialising)
                | (ContainerState::Preparing, ContainerState::Starting)
                | (ContainerState::Preparing, ContainerState::Failed)
                | (ContainerState::Initialising, ContainerState::Starting)
                | (ContainerState::Initialising, ContainerState::Failed)
                | (ContainerState::Starting, ContainerState::HealthWait)
                | (ContainerState::Starting, ContainerState::Failed)
                | (ContainerState::HealthWait, ContainerState::Running)
                | (ContainerState::HealthWait, ContainerState::Failed)
                | (ContainerState::Running, ContainerState::Unhealthy)
                | (ContainerState::Running, ContainerState::Stopping)
                | (ContainerState::Unhealthy, ContainerState::Running)
                | (ContainerState::Unhealthy, ContainerState::Stopping)
                | (ContainerState::Stopping, ContainerState::Stopped)
                | (ContainerState::Stopped, ContainerState::Pending)
        )
    }

    /// Attempt the transition, returning an error if invalid.
    pub fn transition_to(self, next: ContainerState) -> Result<ContainerState, InvalidTransition> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(InvalidTransition {
                from: self,
                to: next,
            })
        }
    }
}

impl fmt::Display for ContainerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContainerState::Pending => write!(f, "pending"),
            ContainerState::Preparing => write!(f, "preparing"),
            ContainerState::Initialising => write!(f, "initialising"),
            ContainerState::Starting => write!(f, "starting"),
            ContainerState::HealthWait => write!(f, "health-wait"),
            ContainerState::Running => write!(f, "running"),
            ContainerState::Unhealthy => write!(f, "unhealthy"),
            ContainerState::Stopping => write!(f, "stopping"),
            ContainerState::Stopped => write!(f, "stopped"),
            ContainerState::Failed => write!(f, "failed"),
        }
    }
}

/// Error returned when an invalid state transition is attempted.
#[derive(Debug, thiserror::Error)]
#[error("invalid state transition from {from} to {to}")]
pub struct InvalidTransition {
    pub from: ContainerState,
    pub to: ContainerState,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Valid transitions ----------------------------------------------------

    #[test]
    fn pending_to_preparing() {
        assert_eq!(
            ContainerState::Pending
                .transition_to(ContainerState::Preparing)
                .unwrap(),
            ContainerState::Preparing
        );
    }

    #[test]
    fn preparing_to_initialising() {
        assert!(ContainerState::Preparing.can_transition_to(ContainerState::Initialising));
    }

    #[test]
    fn preparing_to_starting() {
        assert!(ContainerState::Preparing.can_transition_to(ContainerState::Starting));
    }

    #[test]
    fn preparing_to_failed() {
        assert!(ContainerState::Preparing.can_transition_to(ContainerState::Failed));
    }

    #[test]
    fn initialising_to_starting() {
        assert!(ContainerState::Initialising.can_transition_to(ContainerState::Starting));
    }

    #[test]
    fn initialising_to_failed() {
        assert!(ContainerState::Initialising.can_transition_to(ContainerState::Failed));
    }

    #[test]
    fn starting_to_health_wait() {
        assert!(ContainerState::Starting.can_transition_to(ContainerState::HealthWait));
    }

    #[test]
    fn starting_to_failed() {
        assert!(ContainerState::Starting.can_transition_to(ContainerState::Failed));
    }

    #[test]
    fn health_wait_to_running() {
        assert!(ContainerState::HealthWait.can_transition_to(ContainerState::Running));
    }

    #[test]
    fn health_wait_to_failed() {
        assert!(ContainerState::HealthWait.can_transition_to(ContainerState::Failed));
    }

    #[test]
    fn running_to_unhealthy() {
        assert!(ContainerState::Running.can_transition_to(ContainerState::Unhealthy));
    }

    #[test]
    fn running_to_stopping() {
        assert!(ContainerState::Running.can_transition_to(ContainerState::Stopping));
    }

    #[test]
    fn unhealthy_to_running() {
        assert!(ContainerState::Unhealthy.can_transition_to(ContainerState::Running));
    }

    #[test]
    fn unhealthy_to_stopping() {
        assert!(ContainerState::Unhealthy.can_transition_to(ContainerState::Stopping));
    }

    #[test]
    fn stopping_to_stopped() {
        assert!(ContainerState::Stopping.can_transition_to(ContainerState::Stopped));
    }

    #[test]
    fn stopped_to_pending_for_restart() {
        assert!(ContainerState::Stopped.can_transition_to(ContainerState::Pending));
    }

    // -- Invalid transitions --------------------------------------------------

    #[test]
    fn pending_to_running_rejected() {
        assert!(!ContainerState::Pending.can_transition_to(ContainerState::Running));
    }

    #[test]
    fn stopped_to_running_rejected() {
        assert!(!ContainerState::Stopped.can_transition_to(ContainerState::Running));
    }

    #[test]
    fn failed_to_running_rejected() {
        assert!(!ContainerState::Failed.can_transition_to(ContainerState::Running));
    }

    #[test]
    fn running_to_pending_rejected() {
        assert!(!ContainerState::Running.can_transition_to(ContainerState::Pending));
    }

    #[test]
    fn health_wait_to_stopping_rejected() {
        assert!(!ContainerState::HealthWait.can_transition_to(ContainerState::Stopping));
    }

    #[test]
    fn transition_to_self_rejected() {
        assert!(!ContainerState::Running.can_transition_to(ContainerState::Running));
    }

    #[test]
    fn invalid_transition_returns_error() {
        let err = ContainerState::Pending
            .transition_to(ContainerState::Running)
            .unwrap_err();
        assert_eq!(err.from, ContainerState::Pending);
        assert_eq!(err.to, ContainerState::Running);
    }

    // -- Full lifecycle paths -------------------------------------------------

    #[test]
    fn happy_path_without_init_containers() {
        let mut state = ContainerState::Pending;
        state = state.transition_to(ContainerState::Preparing).unwrap();
        state = state.transition_to(ContainerState::Starting).unwrap();
        state = state.transition_to(ContainerState::HealthWait).unwrap();
        state = state.transition_to(ContainerState::Running).unwrap();
        state = state.transition_to(ContainerState::Stopping).unwrap();
        state = state.transition_to(ContainerState::Stopped).unwrap();
        assert_eq!(state, ContainerState::Stopped);
    }

    #[test]
    fn happy_path_with_init_containers() {
        let mut state = ContainerState::Pending;
        state = state.transition_to(ContainerState::Preparing).unwrap();
        state = state.transition_to(ContainerState::Initialising).unwrap();
        state = state.transition_to(ContainerState::Starting).unwrap();
        state = state.transition_to(ContainerState::HealthWait).unwrap();
        state = state.transition_to(ContainerState::Running).unwrap();
        assert_eq!(state, ContainerState::Running);
    }

    #[test]
    fn unhealthy_recover_cycle() {
        let mut state = ContainerState::Running;
        state = state.transition_to(ContainerState::Unhealthy).unwrap();
        state = state.transition_to(ContainerState::Running).unwrap();
        assert_eq!(state, ContainerState::Running);
    }

    #[test]
    fn unhealthy_restart_cycle() {
        let mut state = ContainerState::Running;
        state = state.transition_to(ContainerState::Unhealthy).unwrap();
        state = state.transition_to(ContainerState::Stopping).unwrap();
        state = state.transition_to(ContainerState::Stopped).unwrap();
        state = state.transition_to(ContainerState::Pending).unwrap();
        assert_eq!(state, ContainerState::Pending);
    }

    #[test]
    fn prepare_failure_path() {
        let mut state = ContainerState::Pending;
        state = state.transition_to(ContainerState::Preparing).unwrap();
        state = state.transition_to(ContainerState::Failed).unwrap();
        assert_eq!(state, ContainerState::Failed);
    }

    #[test]
    fn init_container_failure_path() {
        let mut state = ContainerState::Pending;
        state = state.transition_to(ContainerState::Preparing).unwrap();
        state = state.transition_to(ContainerState::Initialising).unwrap();
        state = state.transition_to(ContainerState::Failed).unwrap();
        assert_eq!(state, ContainerState::Failed);
    }

    // -- Display --------------------------------------------------------------

    #[test]
    fn display_format() {
        assert_eq!(ContainerState::Pending.to_string(), "pending");
        assert_eq!(ContainerState::HealthWait.to_string(), "health-wait");
        assert_eq!(ContainerState::Initialising.to_string(), "initialising");
    }

    #[test]
    fn invalid_transition_error_message() {
        let err = ContainerState::Pending
            .transition_to(ContainerState::Running)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid state transition from pending to running"
        );
    }
}
