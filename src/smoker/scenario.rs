//! Scripted multi-step chaos scenarios.
//!
//! Scenarios are defined in TOML files with a sequence of fault steps,
//! each with a target, fault type, value, and optional timing. The
//! executor runs them in order, respecting `start_after` delays, and
//! supports dry-run mode and speed multipliers.

use std::time::Duration;

use super::types::{FaultRequest, FaultType, ScenarioStep, ScriptedScenario};
use crate::relish::RelishError;

/// Result of scenario execution.
#[derive(Debug)]
pub enum ScenarioResult {
    /// Scenario was executed successfully.
    Completed { steps_executed: usize },
    /// Dry-run: scenario was validated but not executed.
    DryRun { steps_planned: usize },
}

/// A step in the timeline with its computed activation time.
#[derive(Debug)]
pub struct TimelineEntry {
    /// When this step activates, relative to scenario start.
    pub activation: Duration,
    /// The step definition.
    pub step: ScenarioStep,
}

/// Parse a duration string (used in scenario files).
///
/// Supports: "200ms", "5s", "2m", "1h", or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration, RelishError> {
    crate::relish::fault::parse_duration(s)
}

/// Build a sorted timeline from scenario steps.
///
/// Each step's `start_after` is parsed and used as the activation time.
/// Steps without `start_after` activate immediately (Duration::ZERO).
/// The speed multiplier adjusts all timings.
pub fn build_timeline(
    scenario: &ScriptedScenario,
    speed_multiplier: f64,
) -> Result<Vec<TimelineEntry>, RelishError> {
    let mut timeline = Vec::new();

    for step in &scenario.steps {
        let start_after = match &step.start_after {
            Some(s) => parse_duration(s)?,
            None => Duration::ZERO,
        };

        let adjusted = if speed_multiplier != 1.0 && speed_multiplier > 0.0 {
            Duration::from_secs_f64(start_after.as_secs_f64() / speed_multiplier)
        } else {
            start_after
        };

        timeline.push(TimelineEntry {
            activation: adjusted,
            step: step.clone(),
        });
    }

    timeline.sort_by_key(|e| e.activation);
    Ok(timeline)
}

/// Convert a scenario step into a FaultRequest.
///
/// Parses the step's fault type and value strings into the typed
/// `FaultType` enum. Uses the same parsing logic as the CLI.
pub fn step_to_fault_request(
    step: &ScenarioStep,
    speed_multiplier: f64,
) -> Result<FaultRequest, RelishError> {
    let fault_type = match step.fault.as_str() {
        "delay" => {
            let delay_ns = crate::relish::fault::parse_delay_ns(&step.value)?;
            let jitter_ns = match &step.jitter {
                Some(j) => crate::relish::fault::parse_delay_ns(j)?,
                None => 0,
            };
            FaultType::Delay {
                delay_ns,
                jitter_ns,
            }
        }
        "drop" => {
            let probability = crate::relish::fault::parse_percentage(&step.value)?;
            FaultType::Drop { probability }
        }
        "dns" => FaultType::DnsNxdomain,
        "partition" => FaultType::Partition {
            source_app: None,
            source_cgroup_id: 0,
        },
        "bandwidth" => {
            let bytes_per_sec = crate::relish::fault::parse_bandwidth(&step.value)?;
            FaultType::Bandwidth { bytes_per_sec }
        }
        "cpu" => {
            let percentage = crate::relish::fault::parse_percentage(&step.value)?;
            FaultType::CpuStress {
                percentage,
                cores: None,
            }
        }
        "memory" => {
            if step.value.trim().eq_ignore_ascii_case("oom") {
                FaultType::MemoryPressure {
                    percentage: 0,
                    oom: true,
                }
            } else {
                let percentage = crate::relish::fault::parse_percentage(&step.value)?;
                FaultType::MemoryPressure {
                    percentage,
                    oom: false,
                }
            }
        }
        "disk-io" => {
            let bytes_per_sec = crate::relish::fault::parse_bandwidth(&step.value)?;
            FaultType::DiskIoThrottle {
                bytes_per_sec,
                write_only: false,
            }
        }
        "kill" => FaultType::Kill { count: 0 },
        "pause" => FaultType::Pause,
        other => {
            return Err(RelishError::ApiError {
                status: 0,
                body: format!("unknown fault type in scenario: {other}"),
            });
        }
    };

    let duration = match &step.duration {
        Some(d) => {
            let dur = parse_duration(d)?;
            if speed_multiplier != 1.0 && speed_multiplier > 0.0 {
                Duration::from_secs_f64(dur.as_secs_f64() / speed_multiplier)
            } else {
                dur
            }
        }
        None => Duration::from_secs(600), // default 10 minutes
    };

    Ok(FaultRequest {
        fault_type,
        target_service: step.target.clone(),
        target_instance: None,
        target_node: None,
        duration,
        injected_by: format!(
            "scenario:{}",
            std::env::var("USER").unwrap_or_else(|_| "unknown".into())
        ),
        reason: Some(step.description.clone()),
        include_leader: false,
        override_safety: false,
    })
}

/// Format a duration for display.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}s", secs)
    }
}

/// Print a dry-run summary of a scenario.
pub fn print_dry_run(scenario: &ScriptedScenario, timeline: &[TimelineEntry]) {
    println!("Scenario: {}", scenario.name);
    println!("Steps ({}):", timeline.len());
    for entry in timeline {
        let duration = entry
            .step
            .duration
            .as_deref()
            .unwrap_or("until scenario ends");
        println!(
            "  T+{:>6}: {} {} {} ({}) -- {}",
            format_duration(entry.activation),
            entry.step.fault,
            entry.step.target,
            entry.step.value,
            duration,
            entry.step.description,
        );
    }
}

/// Load a scenario from a TOML file.
pub fn load_scenario(path: &std::path::Path) -> Result<ScriptedScenario, RelishError> {
    let content = std::fs::read_to_string(path)?;
    let scenario: ScriptedScenario =
        toml::from_str(&content).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse scenario: {e}"),
        })?;
    Ok(scenario)
}

// Make parsing functions accessible to scenario module
// (they're already pub in relish::fault, re-export isn't needed)

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scenario() -> ScriptedScenario {
        ScriptedScenario {
            name: "Test scenario".into(),
            steps: vec![
                ScenarioStep {
                    description: "Database latency".into(),
                    fault: "delay".into(),
                    target: "pg".into(),
                    value: "500ms".into(),
                    jitter: Some("200ms".into()),
                    duration: Some("2m".into()),
                    start_after: None,
                },
                ScenarioStep {
                    description: "Database drops".into(),
                    fault: "drop".into(),
                    target: "pg".into(),
                    value: "25%".into(),
                    jitter: None,
                    duration: Some("3m".into()),
                    start_after: Some("2m".into()),
                },
                ScenarioStep {
                    description: "Memory pressure".into(),
                    fault: "memory".into(),
                    target: "payment".into(),
                    value: "95%".into(),
                    jitter: None,
                    duration: Some("2m".into()),
                    start_after: Some("4m".into()),
                },
            ],
        }
    }

    #[test]
    fn build_timeline_sorts_by_activation() {
        let scenario = sample_scenario();
        let timeline = build_timeline(&scenario, 1.0).unwrap();
        assert_eq!(timeline.len(), 3);
        assert!(timeline[0].activation <= timeline[1].activation);
        assert!(timeline[1].activation <= timeline[2].activation);
    }

    #[test]
    fn build_timeline_first_step_at_zero() {
        let scenario = sample_scenario();
        let timeline = build_timeline(&scenario, 1.0).unwrap();
        assert_eq!(timeline[0].activation, Duration::ZERO);
    }

    #[test]
    fn build_timeline_speed_multiplier_halves_times() {
        let scenario = sample_scenario();
        let timeline = build_timeline(&scenario, 2.0).unwrap();
        // Step 2 has start_after = 2m, at 2x speed it should be 1m
        assert_eq!(timeline[1].activation, Duration::from_secs(60));
        // Step 3 has start_after = 4m, at 2x speed it should be 2m
        assert_eq!(timeline[2].activation, Duration::from_secs(120));
    }

    #[test]
    fn step_to_fault_request_delay() {
        let step = ScenarioStep {
            description: "test delay".into(),
            fault: "delay".into(),
            target: "redis".into(),
            value: "200ms".into(),
            jitter: Some("50ms".into()),
            duration: Some("5m".into()),
            start_after: None,
        };
        let req = step_to_fault_request(&step, 1.0).unwrap();
        assert!(matches!(
            req.fault_type,
            FaultType::Delay {
                delay_ns: 200_000_000,
                jitter_ns: 50_000_000
            }
        ));
        assert_eq!(req.target_service, "redis");
        assert_eq!(req.duration, Duration::from_secs(300));
    }

    #[test]
    fn step_to_fault_request_drop() {
        let step = ScenarioStep {
            description: "test drop".into(),
            fault: "drop".into(),
            target: "api".into(),
            value: "25%".into(),
            jitter: None,
            duration: Some("1m".into()),
            start_after: None,
        };
        let req = step_to_fault_request(&step, 1.0).unwrap();
        assert!(matches!(
            req.fault_type,
            FaultType::Drop { probability: 25 }
        ));
    }

    #[test]
    fn step_to_fault_request_memory_oom() {
        let step = ScenarioStep {
            description: "OOM".into(),
            fault: "memory".into(),
            target: "payment".into(),
            value: "oom".into(),
            jitter: None,
            duration: None,
            start_after: None,
        };
        let req = step_to_fault_request(&step, 1.0).unwrap();
        assert!(matches!(
            req.fault_type,
            FaultType::MemoryPressure { oom: true, .. }
        ));
    }

    #[test]
    fn step_to_fault_request_unknown_type_errors() {
        let step = ScenarioStep {
            description: "bad".into(),
            fault: "unknown-fault".into(),
            target: "x".into(),
            value: "y".into(),
            jitter: None,
            duration: None,
            start_after: None,
        };
        assert!(step_to_fault_request(&step, 1.0).is_err());
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(30)), "30s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2m5s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3660)), "1h1m");
    }

    #[test]
    fn dry_run_output_does_not_panic() {
        let scenario = sample_scenario();
        let timeline = build_timeline(&scenario, 1.0).unwrap();
        // Just verify it doesn't panic
        print_dry_run(&scenario, &timeline);
    }

    #[test]
    fn scenario_from_toml_round_trip() {
        let toml_str = r#"
            name = "cascade failure"

            [[step]]
            description = "latency spike"
            fault = "delay"
            target = "pg"
            value = "500ms"
            jitter = "200ms"
            duration = "2m"

            [[step]]
            description = "drop connections"
            fault = "drop"
            target = "pg"
            value = "25%"
            start_after = "2m"
            duration = "3m"
        "#;
        let scenario: ScriptedScenario = toml::from_str(toml_str).unwrap();
        let timeline = build_timeline(&scenario, 1.0).unwrap();
        assert_eq!(timeline.len(), 2);

        let req0 = step_to_fault_request(&timeline[0].step, 1.0).unwrap();
        assert!(matches!(req0.fault_type, FaultType::Delay { .. }));

        let req1 = step_to_fault_request(&timeline[1].step, 1.0).unwrap();
        assert!(matches!(req1.fault_type, FaultType::Drop { .. }));
    }
}
