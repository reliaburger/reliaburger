//! Alert evaluation engine.
//!
//! Evaluates threshold-based alert rules against the MayoStore.
//! State machine: Inactive → Pending → Firing. Five built-in rules
//! cover the most common failure modes.

use std::collections::HashMap;
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
    Firing { since: SystemTime },
}

/// A state transition detected during evaluation.
///
/// Returned by `evaluate()` when an alert changes from non-firing to
/// firing, or from firing to resolved. Used to trigger webhook
/// notifications.
#[derive(Debug, Clone)]
pub struct AlertTransition {
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

/// A snapshot of an alert for API responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertStatus {
    pub rule_name: String,
    pub state: String,
    pub severity: AlertSeverity,
    pub description: String,
    pub since: Option<u64>,
}

/// Evaluates alert rules against current metric values.
pub struct AlertEvaluator {
    rules: Vec<AlertRule>,
    states: HashMap<String, AlertState>,
}

impl AlertEvaluator {
    /// Create an evaluator with the given rules.
    pub fn new(rules: Vec<AlertRule>) -> Self {
        let states = rules
            .iter()
            .map(|r| (r.name.clone(), AlertState::Inactive))
            .collect();
        Self { rules, states }
    }

    /// Create an evaluator with the 5 default built-in rules.
    pub fn with_defaults() -> Self {
        Self::new(default_rules())
    }

    /// Evaluate all rules against the provided metric values.
    ///
    /// `latest_values` maps metric names to their latest value.
    /// Call this on a timer (e.g. every 30s). Returns any state
    /// transitions (Firing or Resolved) for webhook dispatch.
    pub fn evaluate(&mut self, latest_values: &HashMap<String, f64>) -> Vec<AlertTransition> {
        let now = SystemTime::now();
        let mut transitions = Vec::new();

        for rule in &self.rules {
            let prev_state = self
                .states
                .get(&rule.name)
                .cloned()
                .unwrap_or(AlertState::Inactive);
            let value = latest_values.get(&rule.metric_name).copied();

            let breaching = value
                .map(|v| rule.operator.eval(v, rule.threshold))
                .unwrap_or(false);

            let new_state = match (&prev_state, breaching) {
                (AlertState::Inactive, true) => AlertState::Pending { since: now },
                (AlertState::Pending { since }, true) => {
                    if now.duration_since(*since).unwrap_or_default() >= rule.for_duration {
                        AlertState::Firing { since: *since }
                    } else {
                        prev_state.clone()
                    }
                }
                (AlertState::Firing { .. }, true) => prev_state.clone(),
                (_, false) => AlertState::Inactive,
            };

            // Detect transitions for webhook dispatch.
            let was_firing = matches!(prev_state, AlertState::Firing { .. });
            let now_firing = matches!(new_state, AlertState::Firing { .. });

            if now_firing && !was_firing {
                transitions.push(AlertTransition {
                    rule_name: rule.name.clone(),
                    severity: rule.severity,
                    description: rule.description.clone(),
                    kind: TransitionKind::Firing,
                    value,
                    fired_at: match &new_state {
                        AlertState::Firing { since } => Some(*since),
                        _ => None,
                    },
                });
            } else if was_firing && !now_firing {
                transitions.push(AlertTransition {
                    rule_name: rule.name.clone(),
                    severity: rule.severity,
                    description: rule.description.clone(),
                    kind: TransitionKind::Resolved,
                    value,
                    fired_at: None,
                });
            }

            self.states.insert(rule.name.clone(), new_state);
        }

        transitions
    }

    /// Get all active (firing) alerts.
    pub fn firing_alerts(&self) -> Vec<AlertStatus> {
        self.rules
            .iter()
            .filter_map(|rule| {
                let state = self.states.get(&rule.name)?;
                match state {
                    AlertState::Firing { since } => Some(AlertStatus {
                        rule_name: rule.name.clone(),
                        state: "firing".to_string(),
                        severity: rule.severity,
                        description: rule.description.clone(),
                        since: since
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs()),
                    }),
                    _ => None,
                }
            })
            .collect()
    }

    /// Get all alert statuses (including inactive and pending).
    pub fn all_statuses(&self) -> Vec<AlertStatus> {
        self.rules
            .iter()
            .map(|rule| {
                let state = self
                    .states
                    .get(&rule.name)
                    .cloned()
                    .unwrap_or(AlertState::Inactive);
                let (state_str, since) = match &state {
                    AlertState::Inactive => ("inactive".to_string(), None),
                    AlertState::Pending { since } => (
                        "pending".to_string(),
                        since
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs()),
                    ),
                    AlertState::Firing { since } => (
                        "firing".to_string(),
                        since
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs()),
                    ),
                };
                AlertStatus {
                    rule_name: rule.name.clone(),
                    state: state_str,
                    severity: rule.severity,
                    description: rule.description.clone(),
                    since,
                }
            })
            .collect()
    }
}

/// The 5 default built-in alert rules.
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
        AlertRule {
            name: "cpu_idle".to_string(),
            metric_name: "node_cpu_usage_percent".to_string(),
            threshold: 5.0,
            operator: AlertOperator::LessThan,
            for_duration: Duration::from_secs(30 * 60),
            severity: AlertSeverity::Warning,
            description: "CPU below 5% for 30 minutes (possible zombie)".to_string(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_values(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
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

    #[test]
    fn inactive_to_pending_on_breach() {
        let rule = AlertRule {
            for_duration: Duration::from_secs(60), // needs 60s to fire
            ..simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan)
        };
        let mut eval = AlertEvaluator::new(vec![rule]);
        eval.evaluate(&make_values(&[("cpu", 95.0)]));

        let statuses = eval.all_statuses();
        assert_eq!(statuses[0].state, "pending");
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
        assert_eq!(statuses[0].state, "inactive");
    }

    #[test]
    fn pending_to_inactive_on_recovery() {
        let rule = AlertRule {
            for_duration: Duration::from_secs(3600), // long duration
            ..simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan)
        };
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&make_values(&[("cpu", 95.0)])); // pending
        assert_eq!(eval.all_statuses()[0].state, "pending");

        eval.evaluate(&make_values(&[("cpu", 50.0)])); // recovery
        assert_eq!(eval.all_statuses()[0].state, "inactive");
    }

    #[test]
    fn missing_metric_does_not_fire() {
        let rule = simple_rule("test", "cpu", 80.0, AlertOperator::GreaterThan);
        let mut eval = AlertEvaluator::new(vec![rule]);

        eval.evaluate(&HashMap::new()); // no metrics at all
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
        assert_eq!(rules.len(), 5);
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
        assert_eq!(statuses.len(), 5);
        assert!(statuses.iter().all(|s| s.state == "inactive"));
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
            rule_name: "test".to_string(),
            state: "firing".to_string(),
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
