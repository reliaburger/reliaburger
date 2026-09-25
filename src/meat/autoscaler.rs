//! Autoscaling controller.
//!
//! Runs on the Raft leader, evaluates apps with `AutoscaleSpec` every
//! evaluation interval. Queries Mayo for the average per-instance usage,
//! turns it into utilisation of the app's resource request, computes a
//! desired replica count with hysteresis and cooldown, and writes
//! `AutoscaleOverride` to persist the decision.
//!
//! Utilisation follows the Kubernetes HPA convention: `target = "50%"`
//! on `cpu` means "each replica uses, on average, half the CPU it
//! requested". An app with no CPU request is measured against one whole
//! core instead. Memory has no such natural unit, so scaling on memory
//! without a memory request is refused at config validation.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use crate::config::app::AutoscaleSpec;
use crate::config::types::ResourceRange;
use crate::meat::types::AppId;

/// One whole core in `process_cpu_percent` units: what CPU utilisation is
/// measured against when the app declares no CPU request.
pub const ONE_CORE_PERCENT: f64 = 100.0;

/// The resource an `[autoscale]` block scales on.
///
/// Each variant maps to a per-instance series the node collector
/// (`mayo::collector`) really records, labelled `app = "<namespace>/<app>"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoscaleMetric {
    /// CPU, measured against the app's `cpu` request.
    Cpu,
    /// Resident memory, measured against the app's `memory` request.
    Memory,
}

impl AutoscaleMetric {
    /// Parse the `[autoscale] metric` value. Only `cpu` and `memory` are
    /// supported; anything else would query a series nobody records.
    pub fn parse(name: &str) -> Result<Self, AutoscaleConfigError> {
        match name {
            "cpu" => Ok(Self::Cpu),
            "memory" => Ok(Self::Memory),
            other => Err(AutoscaleConfigError::UnsupportedMetric {
                metric: other.to_string(),
            }),
        }
    }

    /// The Mayo series the collector records for this resource.
    ///
    /// `process_cpu_percent` is percent of ONE core (a process saturating
    /// two cores reads 200), and `process_memory_bytes` is resident bytes.
    pub fn series_name(self) -> &'static str {
        match self {
            Self::Cpu => "process_cpu_percent",
            Self::Memory => "process_memory_bytes",
        }
    }
}

impl fmt::Display for AutoscaleMetric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cpu => write!(f, "cpu"),
            Self::Memory => write!(f, "memory"),
        }
    }
}

/// Parsed autoscale configuration with defaults applied.
#[derive(Debug, Clone)]
pub struct AutoscaleConfig {
    /// Resource to scale on.
    pub metric: AutoscaleMetric,
    /// Per-replica request in the series' own unit (percent of one core
    /// for CPU, bytes for memory). Always positive; one core
    /// ([`ONE_CORE_PERCENT`]) when CPU scaling has no CPU request.
    pub request: f64,
    /// Target utilisation of the request as a fraction (0.70 for 70%).
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

/// Why an `[autoscale]` block is invalid. Surfaced at config validation
/// so a bad block fails the deploy loudly instead of silently clamping
/// (DEP8: `min > max` used to be quietly clamped, hiding operator error).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AutoscaleConfigError {
    #[error("autoscale target {target:?} is not a valid percentage or fraction")]
    InvalidTarget { target: String },
    #[error("autoscale min ({min}) must not exceed max ({max})")]
    MinExceedsMax { min: u32, max: u32 },
    #[error("autoscale max must be at least 1")]
    ZeroMax,
    #[error("autoscale {field} {value:?} is not a valid duration")]
    InvalidDuration { field: &'static str, value: String },
    #[error("autoscale {field} must be positive")]
    ZeroDuration { field: &'static str },
    #[error("autoscale scale_down_threshold ({value}) must be between 0 and 1")]
    InvalidThreshold { value: f64 },
    #[error("autoscale metric {metric:?} is not supported; use \"cpu\" or \"memory\"")]
    UnsupportedMetric { metric: String },
    #[error(
        "autoscale on memory needs a non-zero memory request on the app \
         (e.g. memory = \"128Mi-512Mi\"): utilisation is measured against it"
    )]
    MissingMemoryRequest,
}

impl AutoscaleConfig {
    /// Parse and validate from an `AutoscaleSpec` plus the app's `cpu` and
    /// `memory` ranges, applying defaults for optional fields. Rejects an
    /// unsupported metric, a missing or zero request for the chosen
    /// metric, `min > max`, a zero max, unparseable or zero
    /// windows/cooldowns, and an out-of-range hysteresis threshold. An
    /// invalid block is an error, never a silent clamp or a silent no-op.
    pub fn from_spec(
        spec: &AutoscaleSpec,
        cpu: Option<ResourceRange>,
        memory: Option<ResourceRange>,
    ) -> Result<Self, AutoscaleConfigError> {
        let metric = AutoscaleMetric::parse(&spec.metric)?;
        let request = match metric {
            // Millicores to percent of one core: 1000m = 100%. With no CPU
            // request, measure against one whole core. ProcessGrill and
            // rootless nodes refuse apps that declare cpu at all (they can't
            // enforce the limit), so refusing here would make CPU autoscaling
            // impossible on them.
            AutoscaleMetric::Cpu => cpu
                .map(|range| range.request as f64 / 10.0)
                .filter(|request| *request > 0.0)
                .unwrap_or(ONE_CORE_PERCENT),
            // Memory has no natural unit to fall back on, so it needs a request.
            AutoscaleMetric::Memory => memory
                .map(|range| range.request as f64)
                .filter(|request| *request > 0.0)
                .ok_or(AutoscaleConfigError::MissingMemoryRequest)?,
        };
        let target =
            parse_percentage(&spec.target).ok_or_else(|| AutoscaleConfigError::InvalidTarget {
                target: spec.target.clone(),
            })?;
        if spec.max == 0 {
            return Err(AutoscaleConfigError::ZeroMax);
        }
        if spec.min > spec.max {
            return Err(AutoscaleConfigError::MinExceedsMax {
                min: spec.min,
                max: spec.max,
            });
        }
        // The evaluation window must be positive (a zero-length averaging
        // window is meaningless). Cooldown MAY be zero — that legitimately
        // means "no cooldown, scale as soon as the metric warrants".
        let evaluation_window = parse_bounded_duration(
            "evaluation_window",
            spec.evaluation_window.as_deref(),
            300,
            false,
        )?;
        let cooldown = parse_bounded_duration("cooldown", spec.cooldown.as_deref(), 180, true)?;
        let scale_down_threshold = spec.scale_down_threshold.unwrap_or(0.8);
        if !(0.0..=1.0).contains(&scale_down_threshold) {
            return Err(AutoscaleConfigError::InvalidThreshold {
                value: scale_down_threshold,
            });
        }
        Ok(Self {
            metric,
            request,
            target,
            min: spec.min,
            max: spec.max,
            evaluation_window,
            cooldown,
            scale_down_threshold,
        })
    }

    /// Utilisation of the per-replica request, as a fraction, given the
    /// average per-instance value of [`AutoscaleMetric::series_name`].
    /// 1.0 means "using exactly what it asked for"; it can exceed 1.0
    /// because a request isn't a limit.
    pub fn utilisation(&self, series_value: f64) -> f64 {
        series_value / self.request
    }
}

/// Parse an optional duration string, defaulting to `default_secs` when
/// absent. An unparseable duration is always an error. A zero duration is
/// an error unless `allow_zero` (cooldown may legitimately be zero — "scale
/// with no wait" — but a zero averaging window is meaningless).
fn parse_bounded_duration(
    field: &'static str,
    value: Option<&str>,
    default_secs: u64,
    allow_zero: bool,
) -> Result<Duration, AutoscaleConfigError> {
    let Some(raw) = value else {
        return Ok(Duration::from_secs(default_secs));
    };
    let parsed = parse_duration(raw).ok_or_else(|| AutoscaleConfigError::InvalidDuration {
        field,
        value: raw.to_string(),
    })?;
    if parsed.is_zero() && !allow_zero {
        return Err(AutoscaleConfigError::ZeroDuration { field });
    }
    Ok(parsed)
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

    /// 500 millicores: half a core.
    const CPU_REQUEST: Option<ResourceRange> = Some(ResourceRange {
        request: 500,
        limit: 1000,
    });
    /// 256 MiB.
    const MEMORY_REQUEST: Option<ResourceRange> = Some(ResourceRange {
        request: 256 * 1024 * 1024,
        limit: 512 * 1024 * 1024,
    });

    fn cpu_spec() -> AutoscaleSpec {
        AutoscaleSpec {
            metric: "cpu".to_string(),
            target: "50%".to_string(),
            min: 1,
            max: 5,
            evaluation_window: None,
            cooldown: None,
            scale_down_threshold: None,
        }
    }

    fn test_config() -> AutoscaleConfig {
        AutoscaleConfig {
            metric: AutoscaleMetric::Cpu,
            request: 50.0,
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
        let config = AutoscaleConfig::from_spec(&spec, CPU_REQUEST, MEMORY_REQUEST).unwrap();
        assert_eq!(config.target, 0.70);
        assert_eq!(config.evaluation_window, Duration::from_secs(300));
        assert_eq!(config.cooldown, Duration::from_secs(180));
        assert_eq!(config.scale_down_threshold, 0.8);
    }

    #[test]
    fn from_spec_rejects_min_greater_than_max() {
        let spec = AutoscaleSpec {
            metric: "cpu".to_string(),
            target: "70%".to_string(),
            min: 10,
            max: 3,
            evaluation_window: None,
            cooldown: None,
            scale_down_threshold: None,
        };
        assert_eq!(
            AutoscaleConfig::from_spec(&spec, CPU_REQUEST, MEMORY_REQUEST).unwrap_err(),
            AutoscaleConfigError::MinExceedsMax { min: 10, max: 3 },
            "min>max must be a validation error, not a silent clamp"
        );
    }

    #[test]
    fn from_spec_rejects_zero_and_unparseable_windows() {
        let base = AutoscaleSpec {
            metric: "cpu".to_string(),
            target: "70%".to_string(),
            min: 1,
            max: 5,
            evaluation_window: None,
            cooldown: None,
            scale_down_threshold: None,
        };
        // A zero evaluation window is meaningless and rejected...
        let zero_window = AutoscaleSpec {
            evaluation_window: Some("0s".to_string()),
            ..base.clone()
        };
        assert_eq!(
            AutoscaleConfig::from_spec(&zero_window, CPU_REQUEST, None).unwrap_err(),
            AutoscaleConfigError::ZeroDuration {
                field: "evaluation_window"
            }
        );
        // ...but a zero cooldown is legitimate ("scale with no wait").
        let zero_cooldown = AutoscaleSpec {
            cooldown: Some("0s".to_string()),
            ..base.clone()
        };
        assert_eq!(
            AutoscaleConfig::from_spec(&zero_cooldown, CPU_REQUEST, None)
                .unwrap()
                .cooldown,
            Duration::ZERO
        );
        let garbage = AutoscaleSpec {
            evaluation_window: Some("soon".to_string()),
            ..base.clone()
        };
        assert!(matches!(
            AutoscaleConfig::from_spec(&garbage, CPU_REQUEST, None),
            Err(AutoscaleConfigError::InvalidDuration {
                field: "evaluation_window",
                ..
            })
        ));
    }

    #[test]
    fn from_spec_rejects_zero_max_and_bad_threshold() {
        let base = AutoscaleSpec {
            metric: "cpu".to_string(),
            target: "70%".to_string(),
            min: 0,
            max: 0,
            evaluation_window: None,
            cooldown: None,
            scale_down_threshold: None,
        };
        assert_eq!(
            AutoscaleConfig::from_spec(&base, CPU_REQUEST, None).unwrap_err(),
            AutoscaleConfigError::ZeroMax
        );
        let bad_threshold = AutoscaleSpec {
            max: 5,
            scale_down_threshold: Some(1.5),
            ..base
        };
        assert!(matches!(
            AutoscaleConfig::from_spec(&bad_threshold, CPU_REQUEST, None),
            Err(AutoscaleConfigError::InvalidThreshold { .. })
        ));
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
        let config = AutoscaleConfig::from_spec(&spec, CPU_REQUEST, MEMORY_REQUEST).unwrap();
        assert_eq!(config.evaluation_window, Duration::from_secs(600));
        assert_eq!(config.cooldown, Duration::from_secs(300));
        assert_eq!(config.scale_down_threshold, 0.7);
    }

    #[test]
    fn metric_names_map_to_the_collector_series() {
        assert_eq!(AutoscaleMetric::Cpu.series_name(), "process_cpu_percent");
        assert_eq!(
            AutoscaleMetric::Memory.series_name(),
            "process_memory_bytes"
        );
    }

    #[test]
    fn from_spec_rejects_an_unsupported_metric() {
        let spec = AutoscaleSpec {
            metric: "requests_per_second".to_string(),
            ..cpu_spec()
        };
        assert_eq!(
            AutoscaleConfig::from_spec(&spec, CPU_REQUEST, MEMORY_REQUEST).unwrap_err(),
            AutoscaleConfigError::UnsupportedMetric {
                metric: "requests_per_second".to_string()
            }
        );
    }

    #[test]
    fn cpu_without_a_request_is_measured_against_one_core() {
        // ProcessGrill and rootless nodes refuse apps that declare cpu, so
        // CPU scaling must work without a request: 50% then means half a core.
        let config = AutoscaleConfig::from_spec(&cpu_spec(), None, None).unwrap();
        assert_eq!(config.request, ONE_CORE_PERCENT);
        assert!((config.utilisation(50.0) - 0.5).abs() < 1e-9);
        // A zero request is as good as none (utilisation of nothing is
        // undefined), so it falls back the same way.
        let zero = Some(ResourceRange {
            request: 0,
            limit: 500,
        });
        let config = AutoscaleConfig::from_spec(&cpu_spec(), zero, None).unwrap();
        assert_eq!(config.request, ONE_CORE_PERCENT);
    }

    #[test]
    fn from_spec_rejects_memory_scaling_without_a_memory_request() {
        let spec = AutoscaleSpec {
            metric: "memory".to_string(),
            ..cpu_spec()
        };
        assert_eq!(
            AutoscaleConfig::from_spec(&spec, CPU_REQUEST, None).unwrap_err(),
            AutoscaleConfigError::MissingMemoryRequest
        );
        let zero = Some(ResourceRange {
            request: 0,
            limit: 1024,
        });
        assert_eq!(
            AutoscaleConfig::from_spec(&spec, CPU_REQUEST, zero).unwrap_err(),
            AutoscaleConfigError::MissingMemoryRequest
        );
    }

    #[test]
    fn cpu_utilisation_is_measured_against_the_cpu_request() {
        // 500m requested = half a core. The collector reports percent of ONE
        // core, so a process using a quarter of a core reads 25.0: half its
        // request.
        let config = AutoscaleConfig::from_spec(&cpu_spec(), CPU_REQUEST, None).unwrap();
        assert!((config.utilisation(25.0) - 0.5).abs() < 1e-9);
        // A full core against half a core requested is 200% utilisation.
        assert!((config.utilisation(100.0) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn cpu_utilisation_handles_requests_above_one_core() {
        // 2000m requested; 150% of one core used → 75% of the request.
        let two_cores = Some(ResourceRange {
            request: 2000,
            limit: 2000,
        });
        let config = AutoscaleConfig::from_spec(&cpu_spec(), two_cores, None).unwrap();
        assert!((config.utilisation(150.0) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn memory_utilisation_is_measured_against_the_memory_request() {
        let spec = AutoscaleSpec {
            metric: "memory".to_string(),
            ..cpu_spec()
        };
        let config = AutoscaleConfig::from_spec(&spec, None, MEMORY_REQUEST).unwrap();
        assert_eq!(config.metric, AutoscaleMetric::Memory);
        let used = (128 * 1024 * 1024) as f64;
        assert!((config.utilisation(used) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_busy_replica_scales_up_through_the_real_units() {
        // One replica burning a full core against a 500m request at a 50%
        // target: utilisation 2.0, so ceil(1 * 2.0 / 0.5) = 4 replicas.
        let config = AutoscaleConfig::from_spec(&cpu_spec(), CPU_REQUEST, None).unwrap();
        let state = AutoscaleState {
            baseline_replicas: 1,
            current_replicas: 1,
            last_scale_event: None,
        };
        let decision = evaluate(
            &test_app(),
            &config,
            &state,
            config.utilisation(100.0),
            Instant::now(),
        )
        .expect("should scale up");
        assert_eq!(decision.to, 4);
    }
}
