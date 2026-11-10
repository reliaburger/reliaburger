/// Restart policy and exponential backoff computation.
///
/// Controls whether and when a failed or unhealthy workload instance
/// should be restarted. Apps default to unlimited restarts with
/// exponential backoff; jobs have a configurable maximum.
use std::time::Duration;

/// Controls restart behaviour for a workload instance.
///
/// The backoff grows exponentially: `initial * multiplier^count`,
/// capped at `max_backoff`. This prevents restart storms while still
/// recovering from transient failures.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    /// Maximum number of restarts before giving up.
    /// `None` means unlimited (the default for long-running apps).
    pub max_restarts: Option<u32>,
    /// Delay before the first restart.
    pub initial_backoff: Duration,
    /// Ceiling for the backoff duration.
    pub max_backoff: Duration,
    /// Multiplier applied to the backoff on each successive restart.
    pub backoff_multiplier: f64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: None,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(300),
            backoff_multiplier: 2.0,
        }
    }
}

impl RestartPolicy {
    /// Convenience constructor for jobs with a finite restart budget.
    pub fn for_job(max_restarts: u32) -> Self {
        Self {
            max_restarts: Some(max_restarts),
            ..Self::default()
        }
    }

    /// Compute the backoff duration for the given restart count.
    ///
    /// Uses `f64` arithmetic to avoid integer overflow: the exponential
    /// is computed as a float and capped at `max_backoff` before converting
    /// back to `Duration`.
    pub fn compute_backoff(&self, restart_count: u32) -> Duration {
        let base = self.initial_backoff.as_secs_f64();
        let multiplier = self.backoff_multiplier.powi(restart_count as i32);
        let uncapped = base * multiplier;
        let capped = uncapped.min(self.max_backoff.as_secs_f64());
        Duration::from_secs_f64(capped)
    }

    /// Whether the workload is allowed another restart.
    pub fn should_restart(&self, restart_count: u32) -> bool {
        match self.max_restarts {
            None => true,
            Some(max) => restart_count < max,
        }
    }
}

/// Why an instance is waiting to restart, kept until the restart records
/// its decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartCause {
    /// Its process was found no longer running.
    ProcessExited,
    /// The runtime couldn't start it on the last attempt.
    StartFailed,
}

/// The decision a restart records: `rule`, the instance, which attempt this
/// is, the budget and the backoff it waited out. `attempt` is the restart
/// count after the restart, so the backoff is the one for the count before.
pub fn restart_decision(
    rule: &'static str,
    instance: &str,
    attempt: u32,
    policy: &RestartPolicy,
) -> crate::bun::decisions::Decision {
    let backoff = policy.compute_backoff(attempt.saturating_sub(1));
    crate::bun::decisions::Decision::new(rule)
        .with("instance", instance)
        .with("attempt", attempt)
        .with("restart_limit", restart_limit(policy))
        .with("backoff_seconds", backoff.as_millis() as f64 / 1000.0)
}

/// The `restart_limit` evidence: the budget, or `unlimited`.
pub fn restart_limit(policy: &RestartPolicy) -> String {
    match policy.max_restarts {
        Some(max) => max.to_string(),
        None => "unlimited".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_decision_names_the_backoff_it_waited_out() {
        let policy = RestartPolicy::for_job(5);
        let details =
            restart_decision("restart.process_exited", "web-0", 3, &policy).into_details();
        assert_eq!(details["rule"], "restart.process_exited");
        assert_eq!(details["instance"], "web-0");
        assert_eq!(details["attempt"], "3");
        assert_eq!(details["restart_limit"], "5");
        // The third restart waited the backoff for two earlier ones: 1s * 2^2.
        assert_eq!(details["backoff_seconds"], "4");
    }

    #[test]
    fn a_long_running_apps_budget_is_unlimited() {
        let details = restart_decision(
            "restart.start_failed",
            "web-0",
            1,
            &RestartPolicy::default(),
        )
        .into_details();
        assert_eq!(details["restart_limit"], "unlimited");
        assert_eq!(details["backoff_seconds"], "1");
    }

    #[test]
    fn default_has_unlimited_restarts() {
        let policy = RestartPolicy::default();
        assert!(policy.max_restarts.is_none());
    }

    #[test]
    fn default_initial_backoff_is_one_second() {
        let policy = RestartPolicy::default();
        assert_eq!(policy.initial_backoff, Duration::from_secs(1));
    }

    #[test]
    fn default_max_backoff_is_five_minutes() {
        let policy = RestartPolicy::default();
        assert_eq!(policy.max_backoff, Duration::from_secs(300));
    }

    #[test]
    fn for_job_sets_max_restarts() {
        let policy = RestartPolicy::for_job(5);
        assert_eq!(policy.max_restarts, Some(5));
        // inherits other defaults
        assert_eq!(policy.initial_backoff, Duration::from_secs(1));
    }

    #[test]
    fn backoff_at_zero_restarts_equals_initial() {
        let policy = RestartPolicy::default();
        assert_eq!(policy.compute_backoff(0), Duration::from_secs(1));
    }

    #[test]
    fn backoff_grows_exponentially() {
        let policy = RestartPolicy::default();
        assert_eq!(policy.compute_backoff(1), Duration::from_secs(2));
        assert_eq!(policy.compute_backoff(2), Duration::from_secs(4));
        assert_eq!(policy.compute_backoff(3), Duration::from_secs(8));
    }

    #[test]
    fn backoff_caps_at_max() {
        let policy = RestartPolicy::default();
        // 2^10 = 1024 seconds > 300s max
        let backoff = policy.compute_backoff(10);
        assert_eq!(backoff, Duration::from_secs(300));
    }

    #[test]
    fn large_restart_count_does_not_overflow() {
        let policy = RestartPolicy::default();
        // f64 handles large exponents gracefully (saturates to infinity,
        // then .min(max) caps it)
        let backoff = policy.compute_backoff(1000);
        assert_eq!(backoff, policy.max_backoff);
    }

    #[test]
    fn should_restart_unlimited_always_true() {
        let policy = RestartPolicy::default();
        assert!(policy.should_restart(0));
        assert!(policy.should_restart(100));
        assert!(policy.should_restart(u32::MAX));
    }

    #[test]
    fn should_restart_below_limit() {
        let policy = RestartPolicy::for_job(3);
        assert!(policy.should_restart(0));
        assert!(policy.should_restart(1));
        assert!(policy.should_restart(2));
    }

    #[test]
    fn should_restart_at_limit_returns_false() {
        let policy = RestartPolicy::for_job(3);
        assert!(!policy.should_restart(3));
        assert!(!policy.should_restart(4));
    }
}
