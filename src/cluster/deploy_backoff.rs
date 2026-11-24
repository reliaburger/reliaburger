//! Exponential backoff for placements that fail to deploy.
//!
//! The placement reconciler polls the leader every couple of seconds. Before
//! this existed, a deploy that failed (an image whose process exits at once,
//! say) was simply retried on the next poll, so a broken app was redeployed
//! every two seconds forever, each attempt minting a new generation. The
//! supervisor's own restart backoff never applied, because the instance was
//! never adopted in the first place.
//!
//! [`DeployBackoff`] remembers consecutive failures per `(name, namespace)`
//! and the specification fingerprint they were for. A changed specification
//! is new desired state and is tried at once; the same specification waits
//! `initial * 2^(failures - 1)`, capped.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::bun::execution_budget::CapacityRefusalKind;

/// Delay after the first failure of a specification.
pub const INITIAL_DEPLOY_BACKOFF: Duration = Duration::from_secs(5);

/// Ceiling for the delay between attempts at one failing specification.
pub const MAX_DEPLOY_BACKOFF: Duration = Duration::from_secs(300);

/// The fixed delay before retrying a deploy a node refused while its job
/// work makes room for it (#1967). The node's claim, not this backoff,
/// bounds how long that goes on.
pub const CAPACITY_RETRY: Duration = Duration::from_secs(5);

/// Ceiling for the delay after a node refused a deploy with no room to make.
/// The leader moves the replica once it hears the refusal, so this mostly
/// bounds the case where it sends the replica back; five minutes left a
/// re-placed replica down long after it would have fitted.
pub const MAX_CAPACITY_BACKOFF: Duration = Duration::from_secs(30);

/// Placement key: application name and namespace.
pub type PlacementKey = (String, String);

#[derive(Debug, Clone)]
struct FailureRecord {
    fingerprint: String,
    failures: u32,
    retry_at: Instant,
}

/// What the reconciler learns when it records a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deferral {
    /// Consecutive failures of this specification, including this one.
    pub failures: u32,
    /// How long the reconciler waits before the next attempt.
    pub delay: Duration,
}

/// Per-placement deploy failure memory.
#[derive(Debug, Clone)]
pub struct DeployBackoff {
    initial: Duration,
    max: Duration,
    failures: BTreeMap<PlacementKey, FailureRecord>,
}

impl Default for DeployBackoff {
    fn default() -> Self {
        Self::new(INITIAL_DEPLOY_BACKOFF, MAX_DEPLOY_BACKOFF)
    }
}

impl DeployBackoff {
    /// Create a backoff with the given first delay and ceiling.
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max,
            failures: BTreeMap::new(),
        }
    }

    /// Whether the reconciler may attempt `fingerprint` for `key` at `now`.
    ///
    /// A different fingerprint from the one that failed is always allowed:
    /// the operator changed the desired state, so the old failure says
    /// nothing about the new one.
    pub fn may_attempt(&self, key: &PlacementKey, fingerprint: &str, now: Instant) -> bool {
        match self.failures.get(key) {
            Some(record) if record.fingerprint == fingerprint => now >= record.retry_at,
            _ => true,
        }
    }

    /// Record a failed attempt and return how long to wait before the next.
    pub fn record_failure(
        &mut self,
        key: &PlacementKey,
        fingerprint: &str,
        now: Instant,
    ) -> Deferral {
        self.record(key, fingerprint, now, self.max)
    }

    /// Record an attempt the node refused for lack of room. It counts as a
    /// failure like any other, but a yielding refusal waits a fixed
    /// [`CAPACITY_RETRY`] and a full one at most [`MAX_CAPACITY_BACKOFF`].
    pub fn record_capacity_refusal(
        &mut self,
        key: &PlacementKey,
        fingerprint: &str,
        now: Instant,
        kind: CapacityRefusalKind,
    ) -> Deferral {
        match kind {
            CapacityRefusalKind::Yielding => self.record(key, fingerprint, now, CAPACITY_RETRY),
            CapacityRefusalKind::Full => self.record(key, fingerprint, now, MAX_CAPACITY_BACKOFF),
        }
    }

    fn record(
        &mut self,
        key: &PlacementKey,
        fingerprint: &str,
        now: Instant,
        ceiling: Duration,
    ) -> Deferral {
        let failures = match self.failures.get(key) {
            Some(record) if record.fingerprint == fingerprint => record.failures.saturating_add(1),
            _ => 1,
        };
        let delay = self.delay_for(failures).min(ceiling);
        self.failures.insert(
            key.clone(),
            FailureRecord {
                fingerprint: fingerprint.to_string(),
                failures,
                retry_at: now + delay,
            },
        );
        Deferral { failures, delay }
    }

    /// Forget failures once a placement converges or is withdrawn.
    pub fn clear(&mut self, key: &PlacementKey) {
        self.failures.remove(key);
    }

    /// Forget every placement not in `live`.
    pub fn retain(&mut self, live: impl Fn(&PlacementKey) -> bool) {
        self.failures.retain(|key, _| live(key));
    }

    fn delay_for(&self, failures: u32) -> Duration {
        // 2^31 seconds overflows nothing in f64 and the cap applies anyway.
        let exponent = failures.saturating_sub(1).min(31) as i32;
        let uncapped = self.initial.as_secs_f64() * 2f64.powi(exponent);
        Duration::from_secs_f64(uncapped.min(self.max.as_secs_f64()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PlacementKey {
        ("redis".to_string(), "default".to_string())
    }

    #[test]
    fn an_unseen_placement_may_be_attempted() {
        let backoff = DeployBackoff::default();
        assert!(backoff.may_attempt(&key(), "spec-a", Instant::now()));
    }

    #[test]
    fn a_failed_specification_waits_before_its_next_attempt() {
        let mut backoff = DeployBackoff::default();
        let now = Instant::now();
        let deferral = backoff.record_failure(&key(), "spec-a", now);
        assert_eq!(deferral.failures, 1);
        assert_eq!(deferral.delay, INITIAL_DEPLOY_BACKOFF);
        assert!(!backoff.may_attempt(&key(), "spec-a", now + Duration::from_secs(2)));
        assert!(backoff.may_attempt(&key(), "spec-a", now + INITIAL_DEPLOY_BACKOFF));
    }

    #[test]
    fn consecutive_failures_double_the_delay_up_to_the_cap() {
        let mut backoff = DeployBackoff::default();
        let now = Instant::now();
        let delays: Vec<u64> = (0..10)
            .map(|_| {
                backoff
                    .record_failure(&key(), "spec-a", now)
                    .delay
                    .as_secs()
            })
            .collect();
        assert_eq!(delays, vec![5, 10, 20, 40, 80, 160, 300, 300, 300, 300]);
    }

    /// #1967: a replica refused for lack of room on a node full of job
    /// executors waited up to five minutes between tries, long after the
    /// executors had made way for it.
    #[test]
    fn a_yielding_refusal_retries_every_five_seconds_however_often_it_repeats() {
        let mut backoff = DeployBackoff::default();
        let now = Instant::now();
        for _ in 0..10 {
            let deferral = backoff.record_capacity_refusal(
                &key(),
                "spec-a",
                now,
                CapacityRefusalKind::Yielding,
            );
            assert_eq!(deferral.delay, CAPACITY_RETRY);
        }
    }

    #[test]
    fn a_capacity_refusal_never_waits_more_than_thirty_seconds() {
        let mut backoff = DeployBackoff::default();
        let now = Instant::now();
        let delays: Vec<u64> = (0..8)
            .map(|_| {
                backoff
                    .record_capacity_refusal(&key(), "spec-a", now, CapacityRefusalKind::Full)
                    .delay
                    .as_secs()
            })
            .collect();
        assert_eq!(delays, vec![5, 10, 20, 30, 30, 30, 30, 30]);
    }

    #[test]
    fn other_failures_keep_the_five_minute_ceiling() {
        let mut backoff = DeployBackoff::default();
        let now = Instant::now();
        for _ in 0..6 {
            backoff.record_capacity_refusal(&key(), "spec-a", now, CapacityRefusalKind::Full);
        }
        let deferral = backoff.record_failure(&key(), "spec-a", now);
        assert_eq!(deferral.failures, 7);
        assert_eq!(deferral.delay, MAX_DEPLOY_BACKOFF);
    }

    #[test]
    fn eight_minutes_of_a_failing_app_is_a_handful_of_attempts_not_hundreds() {
        // The V02 soak saw generation g170 in eight minutes at a 2 s cadence.
        let mut backoff = DeployBackoff::default();
        let start = Instant::now();
        let mut attempts = 0;
        let mut second = 0;
        while second <= 8 * 60 {
            let now = start + Duration::from_secs(second);
            if backoff.may_attempt(&key(), "spec-a", now) {
                attempts += 1;
                backoff.record_failure(&key(), "spec-a", now);
            }
            second += 2;
        }
        assert!(attempts <= 8, "{attempts} attempts in eight minutes");
    }

    #[test]
    fn a_changed_specification_is_attempted_at_once_and_restarts_the_count() {
        let mut backoff = DeployBackoff::default();
        let now = Instant::now();
        backoff.record_failure(&key(), "spec-a", now);
        backoff.record_failure(&key(), "spec-a", now);
        assert!(backoff.may_attempt(&key(), "spec-b", now));
        let deferral = backoff.record_failure(&key(), "spec-b", now);
        assert_eq!(deferral.failures, 1);
        assert_eq!(deferral.delay, INITIAL_DEPLOY_BACKOFF);
    }

    #[test]
    fn clearing_a_placement_forgets_its_failures() {
        let mut backoff = DeployBackoff::default();
        let now = Instant::now();
        backoff.record_failure(&key(), "spec-a", now);
        backoff.clear(&key());
        assert!(backoff.may_attempt(&key(), "spec-a", now));
        assert_eq!(backoff.record_failure(&key(), "spec-a", now).failures, 1);
    }

    #[test]
    fn retain_drops_withdrawn_placements() {
        let mut backoff = DeployBackoff::default();
        let now = Instant::now();
        backoff.record_failure(&key(), "spec-a", now);
        backoff.retain(|_| false);
        assert!(backoff.may_attempt(&key(), "spec-a", now));
    }
}
