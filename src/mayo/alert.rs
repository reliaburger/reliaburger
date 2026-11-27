//! Alert evaluation engine.
//!
//! Evaluates threshold-based alert rules against the MayoStore.
//! State machine: Inactive → Pending → Firing. Four built-in rules
//! cover the most common failure modes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::types::MetricKey;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// An alert rule definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRule {
    /// Unique name (e.g. `cpu_throttle`).
    pub name: String,
    /// The metric to check.
    pub metric_name: String,
    /// Threshold value.
    pub threshold: f64,
    /// Comparison operator.
    pub operator: AlertOperator,
    /// How long the condition must hold before firing.
    pub for_duration: Duration,
    /// Severity level.
    pub severity: AlertSeverity,
    /// Human-readable description.
    pub description: String,
}

/// Comparison operator for alert thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertOperator {
    GreaterThan,
    LessThan,
}

impl AlertOperator {
    /// Evaluate the operator.
    pub fn eval(&self, value: f64, threshold: f64) -> bool {
        match self {
            AlertOperator::GreaterThan => value > threshold,
            AlertOperator::LessThan => value < threshold,
        }
    }
}

/// Alert severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlertSeverity {
    Warning,
    Critical,
}

/// The state of a single alert instance.
#[derive(Debug, Clone, PartialEq)]
pub enum AlertState {
    /// Condition not met.
    Inactive,
    /// Condition met, waiting for `for_duration` to elapse.
    Pending { since: SystemTime },
    /// Condition held for the required duration. Alert is active.
    /// `missed` counts the evaluations in a row with no reading for it.
    Firing { since: SystemTime, missed: u32 },
}

/// Evaluations in a row a firing alert may go without a reading before it
/// resolves. A series that stops for good (its app was deleted) would
/// otherwise stay firing until Bun restarts (#2387).
pub const MAX_MISSED_EVALUATIONS: u32 = 10;

/// A state transition detected during evaluation.
///
/// Returned by `evaluate()` when an alert changes from non-firing to
/// firing, or from firing to resolved. Used to trigger webhook
/// notifications.
#[derive(Debug, Clone)]
pub struct AlertTransition {
    /// Labels identifying the metric series that changed state.
    pub labels: BTreeMap<String, String>,
    pub rule_name: String,
    pub severity: AlertSeverity,
    pub description: String,
    pub kind: TransitionKind,
    /// The metric value that triggered the transition.
    pub value: Option<f64>,
    /// When the alert started firing (for firing transitions).
    pub fired_at: Option<SystemTime>,
}

/// The kind of state transition.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionKind {
    /// Alert became active (transitioned to Firing).
    Firing,
    /// Alert was resolved (transitioned from Firing to Inactive).
    Resolved,
}

/// Alert phase carried by the public API, without the evaluator's clock state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertPhase {
    /// The condition is not currently met.
    Inactive,
    /// The condition is waiting for its configured duration.
    Pending,
    /// The condition has held long enough to alert.
    Firing,
}

/// Complete response from the alert inventory endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertsResponse {
    /// Observed statuses; a missing field is not evidence of an empty list.
    pub alerts: Vec<AlertStatus>,
}

/// A snapshot of an alert for API responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertStatus {
    /// Labels identifying this independent alert instance.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Name of the evaluated rule.
    pub rule_name: String,
    /// Observed phase of this labelled instance.
    pub state: AlertPhase,
    /// Severity configured by the rule.
    pub severity: AlertSeverity,
    /// Human-readable explanation of the condition.
    pub description: String,
    /// Unix seconds when the current pending or firing phase began.
    pub since: Option<u64>,
}

/// Identity of one rule evaluated against one labelled metric series.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AlertInstance {
    rule_name: String,
    labels: BTreeMap<String, String>,
}

/// Evaluates alert rules independently for each labelled metric series.
pub struct AlertEvaluator {
    rules: Vec<AlertRule>,
    states: BTreeMap<AlertInstance, AlertState>,
}

impl AlertEvaluator {
    /// Create an evaluator with the given rules.
    pub fn new(rules: Vec<AlertRule>) -> Self {
        Self {
            rules,
            states: BTreeMap::new(),
        }
    }

    /// Create an evaluator with the four default built-in rules.
    pub fn with_defaults() -> Self {
        Self::new(default_rules())
    }

    /// Evaluate fresh readings without combining different label sets.
    /// Missing readings cancel inconclusive pending alerts. A firing alert
    /// recovers on an in-range reading from that same series, or after
    /// [`MAX_MISSED_EVALUATIONS`] evaluations in a row without one.
    pub fn evaluate(&mut self, latest_values: &HashMap<MetricKey, f64>) -> Vec<AlertTransition> {
        let now = SystemTime::now();
        let mut transitions = Vec::new();
        for rule in &self.rules {
            let observed: BTreeMap<_, _> = latest_values
                .iter()
                .filter(|(key, _)| counts_for(rule, key))
                .map(|(key, value)| (key.labels.clone(), *value))
                .collect();
            let labels: BTreeSet<_> = observed
                .keys()
                .cloned()
                .chain(
                    self.states
                        .keys()
                        .filter(|key| key.rule_name == rule.name)
                        .map(|key| key.labels.clone()),
                )
                .collect();
            for labels in labels {
                let instance = AlertInstance {
                    rule_name: rule.name.clone(),
                    labels,
                };
                let previous = self
                    .states
                    .get(&instance)
                    .cloned()
                    .unwrap_or(AlertState::Inactive);
                let value = observed
                    .get(&instance.labels)
                    .copied()
                    .filter(|value| value.is_finite());
                let next = next_state(rule, &previous, value, now);
                let was_firing = matches!(previous, AlertState::Firing { .. });
                let is_firing = matches!(next, AlertState::Firing { .. });
                if was_firing != is_firing {
                    transitions.push(AlertTransition {
                        labels: instance.labels.clone(),
                        rule_name: rule.name.clone(),
                        severity: rule.severity,
                        description: rule.description.clone(),
                        kind: if is_firing {
                            TransitionKind::Firing
                        } else {
                            TransitionKind::Resolved
                        },
                        value,
                        fired_at: match next {
                            AlertState::Firing { since, .. } => Some(since),
                            _ => None,
                        },
                    });
                }
                // Retired inactive series do not grow the state map indefinitely.
                // A firing series stays until its telemetry proves recovery
                // or it has been missing for MAX_MISSED_EVALUATIONS.
                if value.is_none() && next == AlertState::Inactive {
                    self.states.remove(&instance);
                } else {
                    self.states.insert(instance, next);
                }
            }
        }
        transitions
    }

    /// Get all firing labelled alert instances.
    pub fn firing_alerts(&self) -> Vec<AlertStatus> {
        self.all_statuses()
            .into_iter()
            .filter(|status| status.state == AlertPhase::Firing)
            .collect()
    }

    /// Get every known series; an unobserved rule has one inactive status.
    pub fn all_statuses(&self) -> Vec<AlertStatus> {
        let mut statuses = Vec::new();
        for rule in &self.rules {
            let mut found = false;
            for (instance, state) in self
                .states
                .iter()
                .filter(|(key, _)| key.rule_name == rule.name)
            {
                found = true;
                statuses.push(status_for(rule, instance.labels.clone(), state));
            }
            if !found {
                statuses.push(status_for(rule, BTreeMap::new(), &AlertState::Inactive));
            }
        }
        statuses
    }
}

/// Whether `key` is a series `rule` judges: one with the rule's metric
/// name, and, for a rule on a node metric (`node_*`), only the node's own
/// series. Anything with an `app` label is an app's, and an app must not
/// raise the node's alerts (#2387).
fn counts_for(rule: &AlertRule, key: &MetricKey) -> bool {
    key.name.0 == rule.metric_name
        && !(rule.metric_name.starts_with("node_") && key.labels.contains_key("app"))
}

fn next_state(
    rule: &AlertRule,
    previous: &AlertState,
    value: Option<f64>,
    now: SystemTime,
) -> AlertState {
    match (previous, value) {
        (AlertState::Firing { since, missed }, None) => {
            if missed + 1 >= MAX_MISSED_EVALUATIONS {
                AlertState::Inactive
            } else {
                AlertState::Firing {
                    since: *since,
                    missed: missed + 1,
                }
            }
        }
        (_, None) => AlertState::Inactive,
        (AlertState::Inactive, Some(value)) if rule.operator.eval(value, rule.threshold) => {
            AlertState::Pending { since: now }
        }
        (AlertState::Pending { since }, Some(value))
            if rule.operator.eval(value, rule.threshold) =>
        {
            if now.duration_since(*since).unwrap_or_default() >= rule.for_duration {
                AlertState::Firing {
                    since: *since,
                    missed: 0,
                }
            } else {
                previous.clone()
            }
        }
        (AlertState::Firing { since, .. }, Some(value))
            if rule.operator.eval(value, rule.threshold) =>
        {
            AlertState::Firing {
                since: *since,
                missed: 0,
            }
        }
        (_, Some(_)) => AlertState::Inactive,
    }
}

fn status_for(
    rule: &AlertRule,
    labels: BTreeMap<String, String>,
    state: &AlertState,
) -> AlertStatus {
    let (phase, since) = match state {
        AlertState::Inactive => (AlertPhase::Inactive, None),
        AlertState::Pending { since } => (AlertPhase::Pending, Some(since)),
        AlertState::Firing { since, .. } => (AlertPhase::Firing, Some(since)),
    };
    AlertStatus {
        rule_name: rule.name.clone(),
        labels,
        state: phase,
        severity: rule.severity,
        description: rule.description.clone(),
        since: since
            .and_then(|since| since.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs()),
    }
}

/// The 4 default built-in alert rules.
///
/// All four are node-level and fire on pressure, never on idleness: an
/// idle node is spare capacity, not a fault (#2171).
pub fn default_rules() -> Vec<AlertRule> {
    vec![
        AlertRule {
            name: "cpu_throttle".to_string(),
            metric_name: "node_cpu_usage_percent".to_string(),
            threshold: 90.0,
            operator: AlertOperator::GreaterThan,
            for_duration: Duration::from_secs(5 * 60),
            severity: AlertSeverity::Critical,
            description: "CPU usage above 90% for 5 minutes".to_string(),
        },
        AlertRule {
            name: "oom_risk".to_string(),
            metric_name: "node_memory_usage_percent".to_string(),
            threshold: 85.0,
            operator: AlertOperator::GreaterThan,
            for_duration: Duration::from_secs(2 * 60),
            severity: AlertSeverity::Critical,
            description: "Memory usage above 85% for 2 minutes".to_string(),
        },
        AlertRule {
            name: "memory_high".to_string(),
            metric_name: "node_memory_usage_percent".to_string(),
            threshold: 70.0,
            operator: AlertOperator::GreaterThan,
            for_duration: Duration::from_secs(10 * 60),
            severity: AlertSeverity::Warning,
            description: "Memory usage above 70% for 10 minutes".to_string(),
        },
        AlertRule {
            name: "disk_high".to_string(),
            metric_name: "node_disk_usage_percent".to_string(),
            threshold: 80.0,
            operator: AlertOperator::GreaterThan,
            for_duration: Duration::from_secs(5 * 60),
            severity: AlertSeverity::Warning,
            description: "Disk usage above 80% for 5 minutes".to_string(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_values(pairs: &[(&str, f64)]) -> HashMap<MetricKey, f64> {
        pairs
            .iter()
            .map(|(k, v)| (MetricKey::simple(*k), *v))
            .collect()
    }

    fn simple_rule(name: &str, metric: &str, threshold: f64, op: AlertOperator) -> AlertRule {
        AlertRule {
            name: name.to_string(),
            metric_name: metric.to_string(),
            threshold,
            operator: op,
            for_duration: Duration::from_secs(0), // fires immediately for tests
            severity: AlertSeverity::Warning,
            description: "test rule".to_string(),
        }
    }

    /// The built-in rules describe the node. A series with an `app` label is
    /// an app's, whatever it is called (#2387).
    #[test]
    fn a_default_rule_ignores_series_that_carry_an_app_label() {
        let mut evaluator = AlertEvaluator::with_defaults();
        let forged = HashMap::from([(
            MetricKey::with_labels(
                "node_cpu_usage_percent",
                BTreeMap::from([("app".into(), "default/evil".into())]),
            ),
            99.0,
        )]);
        assert!(evaluator.evaluate(&forged).is_empty());
        // Past the rule's five minutes, as if the reading had held.
        for state in evaluator.states.values_mut() {
            if let AlertState::Pending { since } = state {
                *since -= Duration::from_secs(600);
            }
        }
        assert!(evaluator.evaluate(&forged).is_empty());
        assert!(evaluator.firing_alerts().is_empty());

        // The node's own reading still counts.
        let own = make_values(&[("node_cpu_usage_percent", 99.0)]);
        evaluator.evaluate(&own);
        assert!(evaluator.all_statuses().iter().any(
            |status| status.rule_name == "cpu_throttle" && status.state == AlertPhase::Pending
        ));
    }

    /// A firing alert whose series stopped reporting (the app that exported
    /// it was deleted) used to stay firing until Bun restarted (#2387).
    #[test]
    fn a_firing_alert_whose_series_is_gone_resolves_after_the_absence_limit() {
        let mut evaluator = AlertEvaluator::new(vec![simple_rule(
            "m-high",
            "m",
            50.0,
            AlertOperator::GreaterThan,
        )]);
        evaluator.evaluate(&make_values(&[("m", 99.0)]));
        let fired = evaluator.evaluate(&make_values(&[("m", 99.0)]));
        assert_eq!(fired.len(), 1);
        for _ in 1..MAX_MISSED_EVALUATIONS {
            assert!(evaluator.evaluate(&HashMap::new()).is_empty());
            assert_eq!(evaluator.firing_alerts().len(), 1);
        }
        let resolved = evaluator.evaluate(&HashMap::new());
        assert_eq!(resolved.len(), 1, "{resolved:?}");
        assert_eq!(resolved[0].kind, TransitionKind::Resolved);
        assert_eq!(resolved[0].value, None);
        assert!(evaluator.firing_alerts().is_empty());
        assert!(evaluator.states.is_empty(), "the state is dropped");
    }

    /// One missing reading doesn't reset the count of a later absence.
    #[test]
    fn a_reading_between_gaps_restarts_the_absence_count() {
        let mut evaluator = AlertEvaluator::new(vec![simple_rule(
            "m-high",
            "m",
            50.0,
            AlertOperator::GreaterThan,
        )]);
        evaluator.evaluate(&make_values(&[("m", 99.0)]));
        evaluator.evaluate(&make_values(&[("m", 99.0)]));
        for _ in 0..3 {
            for _ in 1..MAX_MISSED_EVALUATIONS {
                assert!(evaluator.evaluate(&HashMap::new()).is_empty());
            }
            assert!(evaluator.evaluate(&make_values(&[("m", 99.0)])).is_empty());
        }
        assert_eq!(evaluator.firing_alerts().len(), 1);
    }

    #[test]
    fn label_sets_have_independent_pending_firing_and_recovery() {
        let a = MetricKey::with_labels(
            "cpu",
            BTreeMap::from([
                ("namespace".into(), "a".into()),
                ("app".into(), "web".into()),
            ]),
        );
        let b = MetricKey::with_labels(
            "cpu",
            BTreeMap::from([
                ("namespace".into(), "b".into()),
                ("app".into(), "web".into()),
            ]),
        );
        let mut evaluator = AlertEvaluator::new(vec![simple_rule(
            "cpu-high",
            "cpu",
            80.0,
            AlertOperator::GreaterThan,
        )]);
        assert!(
            evaluator
                .evaluate(&HashMap::from([(a.clone(), 95.0)]))
                .is_empty()
        );
        let first = evaluator.evaluate(&HashMap::from([(a.clone(), 95.0), (b.clone(), 96.0)]));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].labels, a.labels);
        assert_eq!(first[0].kind, TransitionKind::Firing);
        assert!(
            evaluator
                .all_statuses()
                .iter()
                .any(|status| status.labels == b.labels && status.state == AlertPhase::Pending)
        );
        let second = evaluator.evaluate(&HashMap::from([(b.clone(), 96.0)]));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].labels, b.labels);
        assert_eq!(
            evaluator.firing_alerts().len(),
            2,
            "missing a-series data must not resolve it"
        );
        let recovery = evaluator.evaluate(&HashMap::from([(a.clone(), 10.0), (b.clone(), 96.0)]));
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].labels, a.labels);
        assert_eq!(recovery[0].kind, TransitionKind::Resolved);
        assert_eq!(evaluator.firing_alerts()[0].labels, b.labels);
        assert!(
            evaluator
                .evaluate(&HashMap::from([(b, f64::NAN)]))
                .is_empty()
        );
        assert_eq!(
            evaluator.firing_alerts().len(),
            1,
            "invalid data cannot prove recovery"
        );
    }

    #[test]
    fn inactive_to_pending_on_breach() {
        let rule = AlertRule {
            for_duration: Duration::from_secs(60), // needs 60s to fire
            ..simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan)
        };
        let mut eval = AlertEvaluator::new(vec![rule]);
        eval.evaluate(&make_values(&[("cpu", 95.0)]));

        let statuses = eval.all_statuses();
        assert_eq!(statuses[0].state, AlertPhase::Pending);
    }

    #[test]
    fn pending_to_firing_after_duration() {
        let rule = simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan);
        // for_duration = 0, so fires immediately
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 95.0)])); // pending
        eval.evaluate(&make_values(&[("cpu", 95.0)])); // firing (duration=0)

        let firing = eval.firing_alerts();
        assert_eq!(firing.len(), 1);
        assert_eq!(firing[0].rule_name, "test");
    }

    #[test]
    fn firing_to_inactive_on_recovery() {
        let rule = simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan);
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 95.0)]));
        eval.evaluate(&make_values(&[("cpu", 95.0)])); // firing
        assert_eq!(eval.firing_alerts().len(), 1);

        eval.evaluate(&make_values(&[("cpu", 50.0)])); // recovery
        assert_eq!(eval.firing_alerts().len(), 0);

        let statuses = eval.all_statuses();
        assert_eq!(statuses[0].state, AlertPhase::Inactive);
    }

    #[test]
    fn pending_to_inactive_on_recovery() {
        let rule = AlertRule {
            for_duration: Duration::from_secs(3600), // long duration
            ..simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan)
        };
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 95.0)])); // pending
        assert_eq!(eval.all_statuses()[0].state, AlertPhase::Pending);

        eval.evaluate(&make_values(&[("cpu", 50.0)])); // recovery
        assert_eq!(eval.all_statuses()[0].state, AlertPhase::Inactive);
    }

    #[test]
    fn missing_metric_does_not_fire() {
        let rule = simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan);
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&HashMap::new()); // no metrics at all
        assert_eq!(eval.firing_alerts().len(), 0);
    }

    #[test]
    fn pending_with_missing_metric_falls_back_to_inactive() {
        // OBS4: a *pending* alert whose data vanishes is inconclusive — we don't
        // fire on nothing, so it drops back to inactive (only a firing alert is
        // held through stale telemetry).
        let rule = AlertRule {
            for_duration: Duration::from_secs(3600), // long, so it stays pending
            ..simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan)
        };
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 95.0)])); // pending
        assert_eq!(eval.all_statuses()[0].state, AlertPhase::Pending);

        // Data disappears while pending.
        let t = eval.evaluate(&HashMap::new());
        assert!(t.is_empty(), "pending→inactive is not a firing transition");
        assert_eq!(eval.all_statuses()[0].state, AlertPhase::Inactive);
    }

    #[test]
    fn stale_telemetry_does_not_resolve_a_firing_alert() {
        // OBS4: an app that dies and stops emitting must not silently clear its
        // own alert. Once firing, a *missing* metric keeps it firing (we can't
        // prove recovery); only a real in-range reading resolves it.
        let rule = simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan);
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 95.0)])); // pending
        eval.evaluate(&make_values(&[("cpu", 95.0)])); // firing
        assert_eq!(eval.firing_alerts().len(), 1);

        // Telemetry stops (app died). No transition, still firing.
        let t = eval.evaluate(&HashMap::new());
        assert!(t.is_empty(), "stale telemetry must not emit a Resolved");
        assert_eq!(
            eval.firing_alerts().len(),
            1,
            "firing alert wrongly resolved on stale telemetry"
        );

        // A genuine in-range reading finally resolves it.
        let t = eval.evaluate(&make_values(&[("cpu", 10.0)]));
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kind, TransitionKind::Resolved);
        assert_eq!(eval.firing_alerts().len(), 0);
    }

    #[test]
    fn less_than_operator() {
        let rule = simple_rule("test", "cpu", 5.0, AlertOperator::LessThan);
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 2.0)]));
        eval.evaluate(&make_values(&[("cpu", 2.0)]));
        assert_eq!(eval.firing_alerts().len(), 1);
    }

    #[test]
    fn multiple_rules_independent() {
        let rules = vec![
            simple_rule("cpu_high", "cpu", 80.0, AlertOperator::GreaterThan),
            simple_rule("mem_high", "mem", 70.0, AlertOperator::GreaterThan),
        ];
        let mut eval = AlertEvaluator::new(rules);

        // Only CPU is high
        eval.evaluate(&make_values(&[("cpu", 95.0), ("mem", 50.0)]));
        eval.evaluate(&make_values(&[("cpu", 95.0), ("mem", 50.0)]));

        let firing = eval.firing_alerts();
        assert_eq!(firing.len(), 1);
        assert_eq!(firing[0].rule_name, "cpu_high");
    }

    #[test]
    fn default_rules_count() {
        let rules = default_rules();
        assert_eq!(rules.len(), 4);
    }

    /// An idle node is spare capacity, not a fault (#2171). With every
    /// default rule's `for` window zeroed, a node reading near-zero CPU
    /// and modest memory and disk must raise nothing, or `relish status`
    /// and `relish wtf` would call a quiet cluster DEGRADED.
    #[test]
    fn idle_healthy_node_fires_no_default_rule() {
        let rules = default_rules()
            .into_iter()
            .map(|rule| AlertRule {
                for_duration: Duration::from_secs(0),
                ..rule
            })
            .collect();
        let mut eval = AlertEvaluator::new(rules);
        let idle = make_values(&[
            ("node_cpu_usage_percent", 0.5),
            ("node_memory_usage_percent", 20.0),
            ("node_disk_usage_percent", 30.0),
        ]);
        eval.evaluate(&idle);
        let transitions = eval.evaluate(&idle);
        assert!(transitions.is_empty(), "{transitions:?}");
        assert!(
            eval.firing_alerts().is_empty(),
            "{:?}",
            eval.firing_alerts()
        );
        assert!(
            eval.all_statuses()
                .iter()
                .all(|s| s.state == AlertPhase::Inactive)
        );
    }

    #[test]
    fn evaluator_with_defaults_starts_inactive() {
        let eval = AlertEvaluator::with_defaults();
        let firing = eval.firing_alerts();
        assert!(firing.is_empty());
    }

    #[test]
    fn all_statuses_includes_every_rule() {
        let eval = AlertEvaluator::with_defaults();
        let statuses = eval.all_statuses();
        assert_eq!(statuses.len(), 4);
        assert!(statuses.iter().all(|s| s.state == AlertPhase::Inactive));
    }

    #[test]
    fn operator_eval_greater_than() {
        assert!(AlertOperator::GreaterThan.eval(90.0, 80.0));
        assert!(!AlertOperator::GreaterThan.eval(70.0, 80.0));
        assert!(!AlertOperator::GreaterThan.eval(80.0, 80.0)); // not strictly greater
    }

    #[test]
    fn operator_eval_less_than() {
        assert!(AlertOperator::LessThan.eval(3.0, 5.0));
        assert!(!AlertOperator::LessThan.eval(7.0, 5.0));
        assert!(!AlertOperator::LessThan.eval(5.0, 5.0)); // not strictly less
    }

    #[test]
    fn alert_status_serialises() {
        let status = AlertStatus {
            labels: BTreeMap::new(),
            rule_name: "test".to_string(),
            state: AlertPhase::Firing,
            severity: AlertSeverity::Critical,
            description: "test".to_string(),
            since: Some(1000),
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("firing"));
        assert!(json.contains("Critical"));
    }

    #[test]
    fn evaluate_returns_firing_transition() {
        let rule = simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan);
        let mut eval = AlertEvaluator::new(vec![rule]);

        let t1 = eval.evaluate(&make_values(&[("cpu", 95.0)])); // pending
        assert!(t1.is_empty()); // no transition yet

        let t2 = eval.evaluate(&make_values(&[("cpu", 95.0)])); // firing
        assert_eq!(t2.len(), 1);
        assert_eq!(t2[0].rule_name, "test");
        assert_eq!(t2[0].kind, TransitionKind::Firing);
        assert_eq!(t2[0].value, Some(95.0));
        assert!(t2[0].fired_at.is_some());
    }

    #[test]
    fn evaluate_returns_resolved_transition() {
        let rule = simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan);
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 95.0)])); // pending
        eval.evaluate(&make_values(&[("cpu", 95.0)])); // firing

        let t = eval.evaluate(&make_values(&[("cpu", 50.0)])); // resolved
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].rule_name, "test");
        assert_eq!(t[0].kind, TransitionKind::Resolved);
    }

    #[test]
    fn evaluate_no_transition_when_stable() {
        let rule = simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan);
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 95.0)])); // pending
        eval.evaluate(&make_values(&[("cpu", 95.0)])); // firing

        // Stays firing — no transition
        let t = eval.evaluate(&make_values(&[("cpu", 95.0)]));
        assert!(t.is_empty());

        // Resolved
        eval.evaluate(&make_values(&[("cpu", 50.0)]));

        // Stays inactive — no transition
        let t = eval.evaluate(&make_values(&[("cpu", 50.0)]));
        assert!(t.is_empty());
    }
}
