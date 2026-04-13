//! Autoscaling controller.
//!
//! Runs on the Raft leader, evaluates apps with `AutoscaleSpec` every
//! evaluation interval. Queries Mayo for average metric utilisation,
//! computes a desired replica count with hysteresis and cooldown, and
//! writes `AutoscaleOverride` to persist the decision.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::app::AutoscaleSpec;
use crate::meat::types::AppId;

/// Parsed autoscale configuration with defaults applied.
#[derive(Debug, Clone)]
pub struct AutoscaleConfig {
    /// Metric name (e.g. "cpu", "memory").
    pub metric: String,
    /// Target utilisation as a fraction (e.g. 0.70 for 70%).
    pub target: f64,
    /// Minimum replica count.
    pub min: u32,
    /// Maximum replica count.
    pub max: u32,
    /// Window over which to average the metric.
    pub evaluation_window: Duration,
    /// Minimum time between scale events.
    pub cooldown: Duration,
    /// Scale-down hysteresis factor (default 0.8).
    /// Only scale down when metric < target * scale_down_threshold.
    pub scale_down_threshold: f64,
}

impl AutoscaleConfig {
    /// Parse from an `AutoscaleSpec`, applying defaults for optional fields.
    pub fn from_spec(spec: &AutoscaleSpec) -> Option<Self> {
        let target = parse_percentage(&spec.target)?;
        Some(Self {
            metric: spec.metric.clone(),
            target,
            min: spec.min,
            max: spec.max,
            evaluation_window: spec
                .evaluation_window
                .as_deref()
                .and_then(parse_duration)
                .unwrap_or(Duration::from_secs(300)),
            cooldown: spec
                .cooldown
                .as_deref()
                .and_then(parse_duration)
                .unwrap_or(Duration::from_secs(180)),
            scale_down_threshold: spec.scale_down_threshold.unwrap_or(0.8),
        })
    }
}

/// Per-app autoscale state tracked by the controller.
#[derive(Debug, Clone)]
pub struct AutoscaleState {
    /// Baseline replica count (from config/git).
    pub baseline_replicas: u32,
    /// Current runtime replica count (may differ from baseline).
    pub current_replicas: u32,
    /// When the last scale event occurred.
    pub last_scale_event: Option<Instant>,
}

/// A scaling decision produced by the controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoscaleDecision {
    /// App that should be scaled.
    pub app_id: AppId,
    /// Previous replica count.
    pub from: u32,
    /// New replica count.
    pub to: u32,
    /// Human-readable reason.
    pub reason: String,
}

/// Evaluate whether an app should scale, given its current metric value.
///
/// Returns `Some(decision)` if scaling should occur, `None` if no change.
pub fn evaluate(
    app_id: &AppId,
    config: &AutoscaleConfig,
    state: &AutoscaleState,
    current_metric: f64,
    now: Instant,
) -> Option<AutoscaleDecision> {
    // Check cooldown
    if let Some(last) = state.last_scale_event
        && now.duration_since(last) < config.cooldown
    {
        return None;
    }

    let current = state.current_replicas;
    let desired = compute_desired(current, current_metric, config);

    if desired == current {
        return None;
    }

    Some(AutoscaleDecision {
        app_id: app_id.clone(),
        from: current,
        to: desired,
        reason: format!(
            "metric {}: {:.1}% (target {:.0}%), scaling {} -> {}",
            config.metric,
            current_metric * 100.0,
            config.target * 100.0,
            current,
            desired
        ),
    })
}

/// Compute the desired replica count from current metric utilisation.
///
/// Formula: `desired = ceil(current * (metric / target))`
/// Scale-down requires metric < target * scale_down_threshold (hysteresis).
/// Result is clamped to [min, max].
fn compute_desired(current: u32, metric: f64, config: &AutoscaleConfig) -> u32 {
    if config.target <= 0.0 || current == 0 {
        return current;
    }

    let ratio = metric / config.target;
    let raw_desired = (current as f64 * ratio).ceil() as u32;

    // Hysteresis: only scale down if metric is well below target
    let desired = if raw_desired < current {
        if metric < config.target * config.scale_down_threshold {
            raw_desired
        } else {
            current // not low enough to scale down
        }
    } else {
        raw_desired
    };

    desired.clamp(config.min, config.max)
}

/// Manage autoscale state for all apps.
#[derive(Debug, Default)]
pub struct AutoscaleTracker {
    states: HashMap<AppId, AutoscaleState>,
}

impl AutoscaleTracker {
    /// Get or create the state for an app.
    pub fn get_or_insert(&mut self, app_id: &AppId, baseline: u32) -> &mut AutoscaleState {
        self.states.entry(app_id.clone()).or_insert(AutoscaleState {
            baseline_replicas: baseline,
            current_replicas: baseline,
            last_scale_event: None,
        })
    }

    /// Apply a scaling decision (update current replicas and timestamp).
    pub fn apply_decision(&mut self, decision: &AutoscaleDecision, now: Instant) {
        if let Some(state) = self.states.get_mut(&decision.app_id) {
            state.current_replicas = decision.to;
            state.last_scale_event = Some(now);
        }
    }

    /// Update baseline when config changes (e.g. from GitOps).
    pub fn update_baseline(&mut self, app_id: &AppId, new_baseline: u32) {
        if let Some(state) = self.states.get_mut(app_id) {
            state.baseline_replicas = new_baseline;
            // Reset current to baseline if baseline changed
            state.current_replicas = new_baseline;
            state.last_scale_event = None;
        }
    }

    /// Get the current override for an app (None if at baseline).
    pub fn get_override(&self, app_id: &AppId) -> Option<u32> {
        self.states.get(app_id).and_then(|s| {
            if s.current_replicas != s.baseline_replicas {
                Some(s.current_replicas)
            } else {
                None
            }
        })
    }

    /// Remove tracking for an app.
    pub fn remove(&mut self, app_id: &AppId) {
        self.states.remove(app_id);
    }
}

// ---------------------------------------------------------------------------
// Async task runner
// ---------------------------------------------------------------------------

/// Run the autoscaler evaluation loop.
///
/// Spawned as a tokio task on the Raft leader. Evaluates all apps
/// with `AutoscaleSpec` at the configured interval, queries Mayo for
/// metrics, and produces `AutoscaleDecision`s that the caller can
/// write to Raft.
///
/// The `app_provider` closure is called each tick to get the current
/// set of apps with autoscaling. The `metric_provider` closure queries
/// Mayo for the average metric value.
pub async fn run_autoscale_loop<F, M>(
    mut tracker: AutoscaleTracker,
    app_provider: F,
    metric_provider: M,
    decision_tx: tokio::sync::mpsc::Sender<AutoscaleDecision>,
    cancel: tokio_util::sync::CancellationToken,
) where
    F: Fn() -> Vec<(AppId, AutoscaleConfig, u32)> + Send + 'static,
    M: Fn(&str, &str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<f64>> + Send>>
        + Send
        + 'static,
{
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = cancel.cancelled() => break,
        }

        let now = Instant::now();
        let apps = app_provider();

        for (app_id, config, baseline) in &apps {
            let state = tracker.get_or_insert(app_id, *baseline);

            // Query the metric
            let metric_value = metric_provider(&config.metric, &app_id.name).await;
            let Some(current_metric) = metric_value else {
                continue; // no data yet
            };

            if let Some(decision) = evaluate(app_id, config, state, current_metric, now) {
                tracker.apply_decision(&decision, now);
                let _ = decision_tx.send(decision).await;
            }
        }
    }
}

/// Parse a percentage string like "70%" into a fraction (0.70).
fn parse_percentage(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f64>().ok().map(|v| v / 100.0)
    } else {
        // Try as a raw fraction
        s.parse::<f64>().ok()
    }
}

/// Parse a duration string like "5m", "30s", "3m".
fn parse_duration(s: &str) -> Option<Duration> {
    crate::meat::deploy_types::parse_duration(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AutoscaleConfig {
        AutoscaleConfig {
            metric: "cpu".to_string(),
            target: 0.70,
            min: 2,
            max: 10,
            evaluation_window: Duration::from_secs(300),
            cooldown: Duration::from_secs(180),
            scale_down_threshold: 0.8,
        }
    }

    fn test_app() -> AppId {
        AppId::new("web", "default")
    }

    fn test_state(current: u32) -> AutoscaleState {
        AutoscaleState {
            baseline_replicas: 3,
            current_replicas: current,
            last_scale_event: None,
        }
    }

    #[test]
    fn scales_up_when_metric_exceeds_target() {
        // cpu at 90%, target 70%, current 3 → desired = ceil(3 * 0.90/0.70) = ceil(3.86) = 4
        let config = test_config();
        let state = test_state(3);
        let decision = evaluate(&test_app(), &config, &state, 0.90, Instant::now());
        let d = decision.expect("should scale up");
        assert_eq!(d.from, 3);
        assert_eq!(d.to, 4);
    }

    #[test]
    fn scales_down_with_hysteresis() {
        // cpu at 50%, target 70%, threshold 0.8 → 50% < 70%*0.8=56% → scale down
        // desired = ceil(4 * 0.50/0.70) = ceil(2.86) = 3
        let config = test_config();
        let state = test_state(4);
        let decision = evaluate(&test_app(), &config, &state, 0.50, Instant::now());
        let d = decision.expect("should scale down");
        assert_eq!(d.from, 4);
        assert_eq!(d.to, 3);
    }

    #[test]
    fn no_scale_down_above_hysteresis() {
        // cpu at 60%, target 70%, threshold 0.8 → 60% > 56% → no scale down
        let config = test_config();
        let state = test_state(4);
        let decision = evaluate(&test_app(), &config, &state, 0.60, Instant::now());
        assert!(decision.is_none(), "should not scale down above hysteresis");
    }

    #[test]
    fn respects_cooldown() {
        let config = test_config();
        let recent = Instant::now() - Duration::from_secs(60); // 60s ago (< 180s cooldown)
        let state = AutoscaleState {
            baseline_replicas: 3,
            current_replicas: 3,
            last_scale_event: Some(recent),
        };
        let decision = evaluate(&test_app(), &config, &state, 0.95, Instant::now());
        assert!(decision.is_none(), "should respect cooldown");
    }

    #[test]
    fn cooldown_expired_allows_scaling() {
        let config = test_config();
        let old = Instant::now() - Duration::from_secs(200); // 200s ago (> 180s cooldown)
        let state = AutoscaleState {
            baseline_replicas: 3,
            current_replicas: 3,
            last_scale_event: Some(old),
        };
        let decision = evaluate(&test_app(), &config, &state, 0.95, Instant::now());
        assert!(decision.is_some(), "should allow scaling after cooldown");
    }

    #[test]
    fn clamps_to_max() {
        // Very high load → desired exceeds max
        let config = test_config(); // max = 10
        let state = test_state(8);
        let decision = evaluate(&test_app(), &config, &state, 0.99, Instant::now());
        let d = decision.expect("should scale up");
        assert!(d.to <= 10, "should not exceed max, got {}", d.to);
    }

    #[test]
    fn clamps_to_min() {
        // Very low load → desired below min
        let config = test_config(); // min = 2
        let state = test_state(5);
        let decision = evaluate(&test_app(), &config, &state, 0.10, Instant::now());
        let d = decision.expect("should scale down");
        assert!(d.to >= 2, "should not go below min, got {}", d.to);
    }

    #[test]
    fn no_change_when_at_target() {
        // cpu at 70%, target 70% → ratio = 1.0, desired = current
        let config = test_config();
        let state = test_state(3);
        let decision = evaluate(&test_app(), &config, &state, 0.70, Instant::now());
        assert!(decision.is_none(), "no change when at target");
    }

    #[test]
    fn formula_ceil() {
        // ceil(3 * 90/70) = ceil(3.857) = 4
        let desired = compute_desired(3, 0.90, &test_config());
        assert_eq!(desired, 4);
    }

    #[test]
    fn parse_percentage_with_percent() {
        assert_eq!(parse_percentage("70%"), Some(0.70));
        assert_eq!(parse_percentage("100%"), Some(1.0));
        assert_eq!(parse_percentage(" 50% "), Some(0.50));
    }

    #[test]
    fn parse_percentage_raw_fraction() {
        assert_eq!(parse_percentage("0.7"), Some(0.7));
    }

    #[test]
    fn parse_percentage_invalid() {
        assert_eq!(parse_percentage("abc"), None);
    }

    #[test]
    fn tracker_get_or_insert() {
        let mut tracker = AutoscaleTracker::default();
        let app = test_app();
        let state = tracker.get_or_insert(&app, 3);
        assert_eq!(state.current_replicas, 3);
        assert_eq!(state.baseline_replicas, 3);
    }

    #[test]
    fn tracker_apply_decision() {
        let mut tracker = AutoscaleTracker::default();
        let app = test_app();
        tracker.get_or_insert(&app, 3);
        let decision = AutoscaleDecision {
            app_id: app.clone(),
            from: 3,
            to: 5,
            reason: "test".to_string(),
        };
        tracker.apply_decision(&decision, Instant::now());
        assert_eq!(tracker.get_override(&app), Some(5));
    }

    #[test]
    fn tracker_update_baseline_resets() {
        let mut tracker = AutoscaleTracker::default();
        let app = test_app();
        tracker.get_or_insert(&app, 3);
        let decision = AutoscaleDecision {
            app_id: app.clone(),
            from: 3,
            to: 5,
            reason: "test".to_string(),
        };
        tracker.apply_decision(&decision, Instant::now());
        assert_eq!(tracker.get_override(&app), Some(5));

        // Git changes replicas to 4 → baseline update resets override
        tracker.update_baseline(&app, 4);
        assert_eq!(tracker.get_override(&app), None);
    }

    #[test]
    fn tracker_no_override_at_baseline() {
        let mut tracker = AutoscaleTracker::default();
        let app = test_app();
        tracker.get_or_insert(&app, 3);
        assert_eq!(
            tracker.get_override(&app),
            None,
            "no override when at baseline"
        );
    }

    #[test]
    fn from_spec_parses_basic() {
        let spec = AutoscaleSpec {
            metric: "cpu".to_string(),
            target: "70%".to_string(),
            min: 2,
            max: 10,
            evaluation_window: None,
            cooldown: None,
            scale_down_threshold: None,
        };
        let config = AutoscaleConfig::from_spec(&spec).unwrap();
        assert_eq!(config.target, 0.70);
        assert_eq!(config.evaluation_window, Duration::from_secs(300));
        assert_eq!(config.cooldown, Duration::from_secs(180));
        assert_eq!(config.scale_down_threshold, 0.8);
    }

    #[test]
    fn from_spec_with_overrides() {
        let spec = AutoscaleSpec {
            metric: "memory".to_string(),
            target: "80%".to_string(),
            min: 1,
            max: 20,
            evaluation_window: Some("10m".to_string()),
            cooldown: Some("5m".to_string()),
            scale_down_threshold: Some(0.7),
        };
        let config = AutoscaleConfig::from_spec(&spec).unwrap();
        assert_eq!(config.evaluation_window, Duration::from_secs(600));
        assert_eq!(config.cooldown, Duration::from_secs(300));
        assert_eq!(config.scale_down_threshold, 0.7);
    }
}
