/// Health check scheduling and evaluation.
///
/// Separates the scheduling question ("when to probe") from the probing
/// question ("how to probe"). This module handles scheduling via a
/// priority-queue and evaluates probe results as pure functions.
/// The actual HTTP probing is deferred to integration tests.
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use crate::config::app::{HealthProtocol, HealthSpec};
use crate::grill::InstanceId;
use crate::grill::state::ContainerState;

/// Resolved health check configuration with defaults applied.
///
/// Created from a [`HealthSpec`] (which uses `Option` for every field
/// because TOML parsing allows omission) by filling in defaults.
#[derive(Debug, Clone)]
pub struct HealthCheckConfig {
    /// HTTP path to probe.
    pub path: String,
    /// Port to probe.
    pub port: u16,
    /// Protocol (HTTP or HTTPS).
    pub protocol: HealthProtocol,
    /// Time between probes.
    pub interval: Duration,
    /// Maximum time to wait for a probe response.
    pub timeout: Duration,
    /// Consecutive failures before marking unhealthy.
    pub threshold_unhealthy: u32,
    /// Consecutive successes before marking healthy.
    pub threshold_healthy: u32,
    /// Delay before the first probe after startup.
    pub initial_delay: Duration,
}

impl HealthCheckConfig {
    /// Resolve a [`HealthSpec`] into a concrete config, applying defaults.
    ///
    /// `app_port` is the app's declared port, used as the fallback when
    /// the health spec doesn't specify its own port.
    pub fn from_spec(spec: &HealthSpec, app_port: u16) -> Self {
        Self {
            path: spec.path.clone(),
            port: spec.port.unwrap_or(app_port),
            protocol: spec.protocol,
            interval: Duration::from_secs(spec.interval.unwrap_or(10)),
            timeout: Duration::from_secs(spec.timeout.unwrap_or(5)),
            threshold_unhealthy: spec.threshold_unhealthy.unwrap_or(3),
            threshold_healthy: spec.threshold_healthy.unwrap_or(1),
            initial_delay: Duration::from_secs(spec.initial_delay.unwrap_or(0)),
        }
    }
}

/// Tracks consecutive healthy/unhealthy probe results for one instance.
#[derive(Debug, Clone)]
pub struct HealthCounters {
    /// Consecutive successful probes.
    pub consecutive_healthy: u32,
    /// Consecutive failed probes.
    pub consecutive_unhealthy: u32,
}

impl HealthCounters {
    /// Create counters at zero.
    pub fn new() -> Self {
        Self {
            consecutive_healthy: 0,
            consecutive_unhealthy: 0,
        }
    }

    /// Record a successful probe. Resets the unhealthy streak.
    pub fn record_healthy(&mut self) {
        self.consecutive_healthy += 1;
        self.consecutive_unhealthy = 0;
    }

    /// Record a failed probe. Resets the healthy streak.
    pub fn record_unhealthy(&mut self) {
        self.consecutive_unhealthy += 1;
        self.consecutive_healthy = 0;
    }

    /// Reset both counters to zero.
    pub fn reset(&mut self) {
        self.consecutive_healthy = 0;
        self.consecutive_unhealthy = 0;
    }
}

impl Default for HealthCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// The result of a single health probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// The probe returned a successful HTTP status.
    Healthy,
    /// The probe returned an error HTTP status.
    Unhealthy,
    /// The probe timed out.
    Timeout,
    /// The probe could not connect.
    ConnectionRefused,
}

impl HealthStatus {
    /// Whether this result counts as a success.
    pub fn is_healthy(self) -> bool {
        matches!(self, HealthStatus::Healthy)
    }
}

/// Evaluate a probe result and determine whether a state transition is needed.
///
/// Pure function: takes the probe result, current counters (already updated),
/// the instance's current state, and the health config. Returns `Some(new_state)`
/// if a transition should happen, `None` otherwise.
pub fn evaluate_result(
    status: HealthStatus,
    counters: &HealthCounters,
    current_state: ContainerState,
    config: &HealthCheckConfig,
) -> Option<ContainerState> {
    match (current_state, status.is_healthy()) {
        // Waiting for initial health: enough consecutive successes → Running
        (ContainerState::HealthWait, true)
            if counters.consecutive_healthy >= config.threshold_healthy =>
        {
            Some(ContainerState::Running)
        }
        // Running: enough consecutive failures → Unhealthy
        (ContainerState::Running, false)
            if counters.consecutive_unhealthy >= config.threshold_unhealthy =>
        {
            Some(ContainerState::Unhealthy)
        }
        // Unhealthy: enough consecutive successes → recover to Running
        (ContainerState::Unhealthy, true)
            if counters.consecutive_healthy >= config.threshold_healthy =>
        {
            Some(ContainerState::Running)
        }
        // All other combinations: no state change
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Priority-queue scheduler
// ---------------------------------------------------------------------------

/// An entry in the health check priority queue.
#[derive(Debug, Clone, Eq, PartialEq)]
struct HealthCheckEntry {
    deadline: Instant,
    instance_id: InstanceId,
}

impl Ord for HealthCheckEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline)
    }
}

impl PartialOrd for HealthCheckEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Schedules health checks using a min-heap ordered by deadline.
///
/// Uses explicit `Instant` parameters (instead of calling `Instant::now()`
/// internally) so tests can be deterministic without time-mocking libraries.
///
/// Unregistered entries are lazily skipped in `pop_due` rather than eagerly
/// removed from the heap — simpler and efficient enough for our scale.
pub struct HealthChecker {
    /// Min-heap: earliest deadline first via `Reverse`.
    heap: BinaryHeap<Reverse<HealthCheckEntry>>,
    /// Config per instance. Also serves as the "registered" set for
    /// lazy deletion: if an instance_id isn't here, stale heap entries
    /// are skipped.
    configs: HashMap<InstanceId, HealthCheckConfig>,
}

impl HealthChecker {
    /// Create an empty health checker.
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            configs: HashMap::new(),
        }
    }

    /// Register an instance for health checking.
    ///
    /// The first probe is scheduled at `now + config.initial_delay`.
    pub fn register(&mut self, instance_id: InstanceId, config: HealthCheckConfig, now: Instant) {
        let deadline = now + config.initial_delay;
        self.configs.insert(instance_id.clone(), config);
        self.heap.push(Reverse(HealthCheckEntry {
            deadline,
            instance_id,
        }));
    }

    /// Remove an instance from health checking.
    ///
    /// Stale entries in the heap are skipped lazily by `pop_due`.
    pub fn unregister(&mut self, instance_id: &InstanceId) {
        self.configs.remove(instance_id);
    }

    /// The earliest deadline in the queue, or `None` if empty.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.heap.peek().map(|Reverse(entry)| entry.deadline)
    }

    /// Pop the next due check if its deadline has passed.
    ///
    /// Skips entries for unregistered instances (lazy deletion).
    /// Returns the instance ID and a clone of its config.
    pub fn pop_due(&mut self, now: Instant) -> Option<(InstanceId, HealthCheckConfig)> {
        while let Some(Reverse(entry)) = self.heap.peek() {
            if entry.deadline > now {
                return None;
            }
            let Reverse(entry) = self.heap.pop().unwrap();
            // Skip stale entries from unregistered instances
            if let Some(config) = self.configs.get(&entry.instance_id) {
                return Some((entry.instance_id, config.clone()));
            }
        }
        None
    }

    /// Schedule the next check for an instance at `now + interval`.
    ///
    /// Call this after processing a health check result.
    pub fn schedule_next(&mut self, instance_id: InstanceId, now: Instant) {
        if let Some(config) = self.configs.get(&instance_id) {
            let deadline = now + config.interval;
            self.heap.push(Reverse(HealthCheckEntry {
                deadline,
                instance_id,
            }));
        }
    }

    /// Number of currently registered instances.
    pub fn registered_count(&self) -> usize {
        self.configs.len()
    }
}

impl Default for HealthChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- HealthCheckConfig resolution -----------------------------------------

    fn make_spec(path: &str) -> HealthSpec {
        HealthSpec {
            path: path.to_string(),
            port: None,
            protocol: HealthProtocol::Http,
            interval: None,
            timeout: None,
            threshold_unhealthy: None,
            threshold_healthy: None,
            initial_delay: None,
        }
    }

    #[test]
    fn config_from_spec_applies_defaults() {
        let spec = make_spec("/healthz");
        let config = HealthCheckConfig::from_spec(&spec, 8080);
        assert_eq!(config.path, "/healthz");
        assert_eq!(config.port, 8080);
        assert_eq!(config.interval, Duration::from_secs(10));
        assert_eq!(config.timeout, Duration::from_secs(5));
        assert_eq!(config.threshold_unhealthy, 3);
        assert_eq!(config.threshold_healthy, 1);
        assert_eq!(config.initial_delay, Duration::ZERO);
    }

    #[test]
    fn config_from_spec_uses_custom_values() {
        let spec = HealthSpec {
            path: "/ready".to_string(),
            port: Some(9090),
            protocol: HealthProtocol::Https,
            interval: Some(30),
            timeout: Some(10),
            threshold_unhealthy: Some(5),
            threshold_healthy: Some(2),
            initial_delay: Some(15),
        };
        let config = HealthCheckConfig::from_spec(&spec, 8080);
        assert_eq!(config.port, 9090);
        assert_eq!(config.protocol, HealthProtocol::Https);
        assert_eq!(config.interval, Duration::from_secs(30));
        assert_eq!(config.timeout, Duration::from_secs(10));
        assert_eq!(config.threshold_unhealthy, 5);
        assert_eq!(config.threshold_healthy, 2);
        assert_eq!(config.initial_delay, Duration::from_secs(15));
    }

    #[test]
    fn config_from_spec_falls_back_to_app_port() {
        let spec = make_spec("/health");
        let config = HealthCheckConfig::from_spec(&spec, 3000);
        assert_eq!(config.port, 3000);
    }

    // -- HealthCounters -------------------------------------------------------

    #[test]
    fn counters_start_at_zero() {
        let counters = HealthCounters::new();
        assert_eq!(counters.consecutive_healthy, 0);
        assert_eq!(counters.consecutive_unhealthy, 0);
    }

    #[test]
    fn record_healthy_increments_and_resets_unhealthy() {
        let mut counters = HealthCounters::new();
        counters.record_unhealthy();
        counters.record_unhealthy();
        counters.record_healthy();
        assert_eq!(counters.consecutive_healthy, 1);
        assert_eq!(counters.consecutive_unhealthy, 0);
    }

    #[test]
    fn record_unhealthy_increments_and_resets_healthy() {
        let mut counters = HealthCounters::new();
        counters.record_healthy();
        counters.record_healthy();
        counters.record_unhealthy();
        assert_eq!(counters.consecutive_unhealthy, 1);
        assert_eq!(counters.consecutive_healthy, 0);
    }

    #[test]
    fn reset_clears_both_counters() {
        let mut counters = HealthCounters::new();
        counters.record_healthy();
        counters.record_healthy();
        counters.reset();
        assert_eq!(counters.consecutive_healthy, 0);
        assert_eq!(counters.consecutive_unhealthy, 0);
    }

    // -- HealthStatus ---------------------------------------------------------

    #[test]
    fn healthy_is_healthy() {
        assert!(HealthStatus::Healthy.is_healthy());
    }

    #[test]
    fn unhealthy_statuses_are_not_healthy() {
        assert!(!HealthStatus::Unhealthy.is_healthy());
        assert!(!HealthStatus::Timeout.is_healthy());
        assert!(!HealthStatus::ConnectionRefused.is_healthy());
    }

    // -- evaluate_result ------------------------------------------------------

    fn default_config() -> HealthCheckConfig {
        HealthCheckConfig::from_spec(&make_spec("/healthz"), 8080)
    }

    #[test]
    fn health_wait_transitions_to_running_when_threshold_met() {
        let config = default_config();
        let counters = HealthCounters {
            consecutive_healthy: 1,
            consecutive_unhealthy: 0,
        };
        let result = evaluate_result(
            HealthStatus::Healthy,
            &counters,
            ContainerState::HealthWait,
            &config,
        );
        assert_eq!(result, Some(ContainerState::Running));
    }

    #[test]
    fn health_wait_no_transition_below_threshold() {
        let mut config = default_config();
        config.threshold_healthy = 3;
        let counters = HealthCounters {
            consecutive_healthy: 2,
            consecutive_unhealthy: 0,
        };
        let result = evaluate_result(
            HealthStatus::Healthy,
            &counters,
            ContainerState::HealthWait,
            &config,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn running_transitions_to_unhealthy_when_threshold_met() {
        let config = default_config();
        let counters = HealthCounters {
            consecutive_healthy: 0,
            consecutive_unhealthy: 3,
        };
        let result = evaluate_result(
            HealthStatus::Unhealthy,
            &counters,
            ContainerState::Running,
            &config,
        );
        assert_eq!(result, Some(ContainerState::Unhealthy));
    }

    #[test]
    fn running_no_transition_below_unhealthy_threshold() {
        let config = default_config();
        let counters = HealthCounters {
            consecutive_healthy: 0,
            consecutive_unhealthy: 2,
        };
        let result = evaluate_result(
            HealthStatus::Unhealthy,
            &counters,
            ContainerState::Running,
            &config,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn unhealthy_recovers_to_running() {
        let config = default_config();
        let counters = HealthCounters {
            consecutive_healthy: 1,
            consecutive_unhealthy: 0,
        };
        let result = evaluate_result(
            HealthStatus::Healthy,
            &counters,
            ContainerState::Unhealthy,
            &config,
        );
        assert_eq!(result, Some(ContainerState::Running));
    }

    #[test]
    fn timeout_counts_as_unhealthy() {
        let config = default_config();
        let counters = HealthCounters {
            consecutive_healthy: 0,
            consecutive_unhealthy: 3,
        };
        let result = evaluate_result(
            HealthStatus::Timeout,
            &counters,
            ContainerState::Running,
            &config,
        );
        assert_eq!(result, Some(ContainerState::Unhealthy));
    }

    #[test]
    fn connection_refused_counts_as_unhealthy() {
        let config = default_config();
        let counters = HealthCounters {
            consecutive_healthy: 0,
            consecutive_unhealthy: 3,
        };
        let result = evaluate_result(
            HealthStatus::ConnectionRefused,
            &counters,
            ContainerState::Running,
            &config,
        );
        assert_eq!(result, Some(ContainerState::Unhealthy));
    }

    #[test]
    fn irrelevant_state_returns_none() {
        let config = default_config();
        let counters = HealthCounters {
            consecutive_healthy: 10,
            consecutive_unhealthy: 0,
        };
        // Pending state: health checks don't drive transitions
        let result = evaluate_result(
            HealthStatus::Healthy,
            &counters,
            ContainerState::Pending,
            &config,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn stopping_state_ignores_health_results() {
        let config = default_config();
        let counters = HealthCounters {
            consecutive_healthy: 0,
            consecutive_unhealthy: 10,
        };
        let result = evaluate_result(
            HealthStatus::Unhealthy,
            &counters,
            ContainerState::Stopping,
            &config,
        );
        assert_eq!(result, None);
    }

    // -- HealthChecker scheduling ---------------------------------------------

    #[test]
    fn register_schedules_first_check_after_initial_delay() {
        let mut checker = HealthChecker::new();
        let now = Instant::now();
        let mut config = default_config();
        config.initial_delay = Duration::from_secs(5);

        checker.register(InstanceId("web-0".to_string()), config, now);

        assert_eq!(checker.registered_count(), 1);
        let deadline = checker.next_deadline().unwrap();
        assert_eq!(deadline, now + Duration::from_secs(5));
    }

    #[test]
    fn register_with_zero_delay_schedules_immediately() {
        let mut checker = HealthChecker::new();
        let now = Instant::now();
        let config = default_config();

        checker.register(InstanceId("web-0".to_string()), config, now);
        assert_eq!(checker.next_deadline().unwrap(), now);
    }

    #[test]
    fn pop_due_returns_none_before_deadline() {
        let mut checker = HealthChecker::new();
        let now = Instant::now();
        let mut config = default_config();
        config.initial_delay = Duration::from_secs(10);

        checker.register(InstanceId("web-0".to_string()), config, now);

        // 5 seconds later: not yet due
        assert!(checker.pop_due(now + Duration::from_secs(5)).is_none());
    }

    #[test]
    fn pop_due_returns_entry_at_deadline() {
        let mut checker = HealthChecker::new();
        let now = Instant::now();
        let config = default_config();

        checker.register(InstanceId("web-0".to_string()), config, now);

        let (id, _cfg) = checker.pop_due(now).unwrap();
        assert_eq!(id, InstanceId("web-0".to_string()));
    }

    #[test]
    fn pop_due_returns_earliest_first() {
        let mut checker = HealthChecker::new();
        let now = Instant::now();

        let mut early_config = default_config();
        early_config.initial_delay = Duration::from_secs(1);
        let mut late_config = default_config();
        late_config.initial_delay = Duration::from_secs(5);

        checker.register(InstanceId("late-0".to_string()), late_config, now);
        checker.register(InstanceId("early-0".to_string()), early_config, now);

        let after_both = now + Duration::from_secs(10);
        let (first_id, _) = checker.pop_due(after_both).unwrap();
        assert_eq!(first_id, InstanceId("early-0".to_string()));

        let (second_id, _) = checker.pop_due(after_both).unwrap();
        assert_eq!(second_id, InstanceId("late-0".to_string()));
    }

    #[test]
    fn unregister_causes_lazy_skip() {
        let mut checker = HealthChecker::new();
        let now = Instant::now();
        let config = default_config();

        checker.register(InstanceId("web-0".to_string()), config.clone(), now);
        checker.register(InstanceId("web-1".to_string()), config, now);
        checker.unregister(&InstanceId("web-0".to_string()));

        assert_eq!(checker.registered_count(), 1);

        let (id, _) = checker.pop_due(now).unwrap();
        assert_eq!(id, InstanceId("web-1".to_string()));
    }

    #[test]
    fn schedule_next_adds_interval() {
        let mut checker = HealthChecker::new();
        let now = Instant::now();
        let config = default_config(); // interval = 10s

        checker.register(InstanceId("web-0".to_string()), config, now);
        // Pop the initial entry
        let _ = checker.pop_due(now).unwrap();

        // Schedule next
        checker.schedule_next(InstanceId("web-0".to_string()), now);

        let deadline = checker.next_deadline().unwrap();
        assert_eq!(deadline, now + Duration::from_secs(10));
    }

    #[test]
    fn empty_checker_has_no_deadline() {
        let checker = HealthChecker::new();
        assert!(checker.next_deadline().is_none());
        assert_eq!(checker.registered_count(), 0);
    }

    #[test]
    fn pop_due_on_empty_returns_none() {
        let mut checker = HealthChecker::new();
        assert!(checker.pop_due(Instant::now()).is_none());
    }
}
