//! Smoker's node-level policy: how long a fault may live.
//!
//! A fault is a bounded experiment, so it must expire. Two limits shape that:
//! a **default** applied when the operator names no duration, and a **maximum**
//! that rejects a fault asking to live too long. Both are enforced server-side
//! (in the inject path), so a direct API call can't slip past the CLI's own
//! defaulting — the CLI is a convenience, not the boundary.
//!
//! Beneath both sits the hard 24-hour ceiling in
//! [`FaultRule::new`](super::types::FaultRule::new): even an absurd
//! `max_duration` can't keep a fault alive past a day.

use std::time::Duration;

/// The default duration when a fault is injected without one.
pub const DEFAULT_FAULT_DURATION: Duration = Duration::from_secs(10 * 60);

/// The default ceiling on a fault's requested duration.
pub const DEFAULT_MAX_FAULT_DURATION: Duration = Duration::from_secs(60 * 60);

/// Node-level Smoker limits, from `[smoker]` config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokerConfig {
    /// Applied when a fault names no duration.
    pub default_duration: Duration,
    /// A fault asking to live longer than this is rejected.
    pub max_duration: Duration,
}

impl Default for SmokerConfig {
    fn default() -> Self {
        Self {
            default_duration: DEFAULT_FAULT_DURATION,
            max_duration: DEFAULT_MAX_FAULT_DURATION,
        }
    }
}

/// Resolve the duration to actually apply, or reject it.
///
/// - An `instantaneous` fault (kill, resume) carries no meaningful duration, so
///   it passes through untouched.
/// - A zero `requested` duration means "unspecified" → the configured default.
/// - Anything over `max_duration` is rejected, with both the request and the
///   limit named so the operator sees why.
pub fn effective_duration(
    requested: Duration,
    instantaneous: bool,
    config: &SmokerConfig,
) -> Result<Duration, String> {
    if instantaneous {
        return Ok(requested);
    }
    let effective = if requested.is_zero() {
        config.default_duration
    } else {
        requested
    };
    if effective > config.max_duration {
        return Err(format!(
            "fault duration {}s exceeds the configured maximum of {}s",
            effective.as_secs(),
            config.max_duration.as_secs()
        ));
    }
    Ok(effective)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoker_config_defaults_to_ten_minutes_and_one_hour() {
        let config = SmokerConfig::default();
        assert_eq!(config.default_duration, Duration::from_secs(600));
        assert_eq!(config.max_duration, Duration::from_secs(3600));
    }

    #[test]
    fn fault_without_duration_uses_the_configured_default() {
        let config = SmokerConfig {
            default_duration: Duration::from_secs(300),
            max_duration: Duration::from_secs(3600),
        };
        assert_eq!(
            effective_duration(Duration::ZERO, false, &config).unwrap(),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn fault_exceeding_max_duration_is_rejected_with_both_values() {
        let config = SmokerConfig {
            default_duration: Duration::from_secs(600),
            max_duration: Duration::from_secs(3600),
        };
        let error = effective_duration(Duration::from_secs(7200), false, &config).unwrap_err();
        // Both the requested duration and the limit must appear.
        assert!(error.contains("7200"), "{error}");
        assert!(error.contains("3600"), "{error}");
    }

    #[test]
    fn a_within_limit_duration_passes_through() {
        let config = SmokerConfig::default();
        assert_eq!(
            effective_duration(Duration::from_secs(120), false, &config).unwrap(),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn an_instantaneous_fault_ignores_the_limits() {
        let config = SmokerConfig {
            default_duration: Duration::from_secs(600),
            max_duration: Duration::from_secs(1),
        };
        // A kill carries duration zero and must not be defaulted or rejected.
        assert_eq!(
            effective_duration(Duration::ZERO, true, &config).unwrap(),
            Duration::ZERO
        );
    }
}
