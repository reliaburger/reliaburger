//! Scripted multi-step chaos scenarios.
//!
//! Scenarios are defined in TOML files with a sequence of fault steps,
//! each with a target, fault type, value, and optional timing and scope.
//! Every step is parsed and checked before the first one is injected, so a
//! typo in step three can't leave steps one and two running on their own.
//! The executor then runs them in order, respecting `start_after` delays,
//! and supports dry-run mode and speed multipliers.

use std::time::Duration;

use super::types::{FaultRequest, FaultType, ScenarioStep, ScriptedScenario};
use crate::relish::RelishError;

/// A step in the timeline with its computed activation time and the
/// request it injects.
#[derive(Debug)]
pub struct TimelineEntry {
    /// When this step activates, relative to scenario start.
    pub activation: Duration,
    /// The step definition.
    pub step: ScenarioStep,
    /// The fault request this step sends, already validated.
    pub request: FaultRequest,
}

/// Parse a duration string (used in scenario files).
///
/// Supports: "200ms", "5s", "2m", "1h", or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration, RelishError> {
    crate::relish::fault::parse_duration(s)
}

/// Divide a duration by the speed multiplier.
fn scaled(duration: Duration, speed_multiplier: f64) -> Duration {
    if speed_multiplier != 1.0 && speed_multiplier > 0.0 {
        Duration::from_secs_f64(duration.as_secs_f64() / speed_multiplier)
    } else {
        duration
    }
}

/// Build a sorted timeline from scenario steps, validating every step.
///
/// Each step's `start_after` is parsed and used as the activation time;
/// steps without one activate immediately. The speed multiplier adjusts
/// all timings. Any invalid step fails the whole scenario, naming the
/// step, before anything has been injected.
pub fn build_timeline(
    scenario: &ScriptedScenario,
    speed_multiplier: f64,
) -> Result<Vec<TimelineEntry>, RelishError> {
    let mut timeline = Vec::with_capacity(scenario.steps.len());

    for (index, step) in scenario.steps.iter().enumerate() {
        let checked = || -> Result<TimelineEntry, RelishError> {
            let start_after = match &step.start_after {
                Some(s) => parse_duration(s)?,
                None => Duration::ZERO,
            };
            Ok(TimelineEntry {
                activation: scaled(start_after, speed_multiplier),
                step: step.clone(),
                request: step_to_fault_request(step, speed_multiplier)?,
            })
        };
        timeline.push(checked().map_err(|error| {
            RelishError::Scenario(format!(
                "step {} ({:?}) is invalid: {error}; nothing was injected",
                index + 1,
                step.description
            ))
        })?);
    }

    timeline.sort_by_key(|e| e.activation);
    Ok(timeline)
}

/// Refuse a scope field that `fault` can't use.
fn refuse_field(fault: &str, field: &str) -> RelishError {
    RelishError::Scenario(format!("a {fault} step does not take `{field}`"))
}

/// Convert a scenario step into a FaultRequest.
///
/// Parses the step's fault type and value strings into the typed
/// `FaultType` enum, with the same parsing as the CLI, and checks its
/// scope fields: `from` only on a delay or partition, `count` only on a
/// kill, and no `instance` on a DNS fault.
pub fn step_to_fault_request(
    step: &ScenarioStep,
    speed_multiplier: f64,
) -> Result<FaultRequest, RelishError> {
    let fault = step.fault.as_str();
    let source_app = step.from.clone();
    if source_app.is_some() && !matches!(fault, "delay" | "partition") {
        return Err(refuse_field(fault, "from"));
    }
    if step.count.is_some() && fault != "kill" {
        return Err(refuse_field(fault, "count"));
    }
    if step.instance.is_some() && fault == "dns" {
        return Err(refuse_field(fault, "instance"));
    }
    let fault_type = match fault {
        "delay" => {
            let delay_ns = crate::relish::fault::parse_delay_ns(&step.value)?;
            let jitter_ns = match &step.jitter {
                Some(j) => crate::relish::fault::parse_delay_ns(j)?,
                None => 0,
            };
            FaultType::Delay {
                delay_ns,
                jitter_ns,
                source_app,
            }
        }
        "drop" => {
            let probability = crate::relish::fault::parse_percentage(&step.value)?;
            FaultType::Drop { probability }
        }
        "dns" => {
            if !(step.value.is_empty() || step.value.eq_ignore_ascii_case("nxdomain")) {
                return Err(RelishError::Scenario(format!(
                    "unknown dns fault type {:?}; supported: nxdomain",
                    step.value
                )));
            }
            FaultType::DnsNxdomain
        }
        "partition" => FaultType::Partition { source_app },
        // Bun refuses it, so a scenario that relies on it is refused here,
        // before its other steps run.
        "bandwidth" => {
            return Err(RelishError::Scenario(
                "bandwidth faults are not implemented yet (#366); use a delay step".to_string(),
            ));
        }
        "cpu" => {
            let percentage = crate::relish::fault::parse_percentage(&step.value)?;
            FaultType::CpuStress {
                percentage,
                cores: None,
            }
        }
        "memory" => FaultType::MemoryPressure {
            percentage: crate::relish::fault::parse_percentage(&step.value)?,
        },
        "disk-io" => {
            let bytes_per_sec = crate::relish::fault::parse_bandwidth(&step.value)?;
            FaultType::DiskIoThrottle {
                bytes_per_sec,
                write_only: false,
            }
        }
        // `count = 0` (every replica) is what the replica rail always
        // refuses, so an unset count takes one replica, as the CLI does.
        "kill" => match step.count {
            Some(0) => {
                return Err(RelishError::Scenario(
                    "a kill step's count must be at least 1".to_string(),
                ));
            }
            count => FaultType::Kill {
                count: count.unwrap_or(1),
            },
        },
        "pause" => FaultType::Pause,
        other => {
            return Err(RelishError::Scenario(format!(
                "unknown fault type in scenario: {other}"
            )));
        }
    };

    let duration = match (&fault_type, &step.duration) {
        (_, Some(d)) => scaled(parse_duration(d)?, speed_multiplier),
        // A kill is instantaneous, as `relish fault kill` sends it.
        (FaultType::Kill { .. }, None) => Duration::ZERO,
        (_, None) => Duration::from_secs(600), // default 10 minutes
    };

    Ok(FaultRequest {
        workload: Default::default(),
        task_index: None,
        fault_type,
        target_service: step.target.clone(),
        // Absent means the API's `default`, as for `relish fault`.
        namespace: step.namespace.clone(),
        target_instance: step.instance.clone(),
        target_node: None,
        duration,
        // Bun attributes the request from the authenticated context, never
        // from client-controlled JSON.
        injected_by: String::new(),
        reason: Some(step.description.clone()),
        include_leader: false,
        override_safety: false,
        acknowledged: false,
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
    let scenario: ScriptedScenario = toml::from_str(&content)
        .map_err(|e| RelishError::Scenario(format!("failed to parse scenario: {e}")))?;
    Ok(scenario)
}

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
                    ..ScenarioStep::default()
                },
                ScenarioStep {
                    description: "Database drops".into(),
                    fault: "drop".into(),
                    target: "pg".into(),
                    value: "25%".into(),
                    duration: Some("3m".into()),
                    start_after: Some("2m".into()),
                    ..ScenarioStep::default()
                },
                ScenarioStep {
                    description: "Memory pressure".into(),
                    fault: "memory".into(),
                    target: "payment".into(),
                    value: "95%".into(),
                    duration: Some("2m".into()),
                    start_after: Some("4m".into()),
                    ..ScenarioStep::default()
                },
            ],
        }
    }

    fn step(fault: &str, value: &str) -> ScenarioStep {
        ScenarioStep {
            description: format!("a {fault}"),
            fault: fault.into(),
            target: "redis".into(),
            value: value.into(),
            ..ScenarioStep::default()
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

    /// Each step used to be parsed only when its turn came, so a typo in
    /// step three aborted after steps one and two were already injected.
    #[test]
    fn scenario_with_a_bad_step_injects_nothing() {
        let mut scenario = sample_scenario();
        scenario.steps[2].value = "95 percent".into();
        let error = build_timeline(&scenario, 1.0).unwrap_err().to_string();
        assert!(error.contains("step 3"), "{error}");
        assert!(error.contains("Memory pressure"), "{error}");
        assert!(error.contains("nothing was injected"), "{error}");
    }

    /// `Kill { count: 0 }` means every replica, which the replica rail
    /// always refuses, so every scenario kill step failed.
    #[test]
    fn kill_step_defaults_to_one_replica() {
        let request = step_to_fault_request(&step("kill", ""), 1.0).unwrap();
        assert!(matches!(request.fault_type, FaultType::Kill { count: 1 }));
        assert_eq!(request.duration, Duration::ZERO);

        let mut two = step("kill", "");
        two.count = Some(2);
        let request = step_to_fault_request(&two, 1.0).unwrap();
        assert!(matches!(request.fault_type, FaultType::Kill { count: 2 }));

        two.count = Some(0);
        assert!(step_to_fault_request(&two, 1.0).is_err());
    }

    #[test]
    fn step_carries_namespace_and_source() {
        let mut partition = step("partition", "");
        partition.namespace = Some("team-a".into());
        partition.from = Some("checkout".into());
        partition.instance = Some("team-a__redis-0".into());

        let request = step_to_fault_request(&partition, 1.0).unwrap();

        assert_eq!(request.namespace.as_deref(), Some("team-a"));
        assert_eq!(request.target_instance.as_deref(), Some("team-a__redis-0"));
        assert!(matches!(
            request.fault_type,
            FaultType::Partition { source_app: Some(ref source) } if source == "checkout"
        ));
    }

    #[test]
    fn a_step_field_its_fault_cannot_use_is_refused() {
        let mut drop = step("drop", "10%");
        drop.from = Some("checkout".into());
        assert!(step_to_fault_request(&drop, 1.0).is_err());

        let mut pause = step("pause", "");
        pause.count = Some(1);
        assert!(step_to_fault_request(&pause, 1.0).is_err());

        let mut dns = step("dns", "nxdomain");
        dns.instance = Some("default__redis-0".into());
        assert!(step_to_fault_request(&dns, 1.0).is_err());
    }

    #[test]
    fn a_bandwidth_step_is_refused_up_front() {
        assert!(step_to_fault_request(&step("bandwidth", "1mbps"), 1.0).is_err());
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
            ..ScenarioStep::default()
        };
        let req = step_to_fault_request(&step, 1.0).unwrap();
        assert!(matches!(
            req.fault_type,
            FaultType::Delay {
                delay_ns: 200_000_000,
                jitter_ns: 50_000_000,
                source_app: None
            }
        ));
        assert_eq!(req.target_service, "redis");
        assert_eq!(req.namespace, None);
        assert_eq!(req.duration, Duration::from_secs(300));
    }

    #[test]
    fn step_to_fault_request_drop() {
        let req = step_to_fault_request(&step("drop", "25%"), 1.0).unwrap();
        assert!(matches!(
            req.fault_type,
            FaultType::Drop { probability: 25 }
        ));
    }

    #[test]
    fn step_to_fault_request_unknown_type_errors() {
        assert!(step_to_fault_request(&step("unknown-fault", "y"), 1.0).is_err());
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

            [[step]]
            description = "lose a replica"
            fault = "kill"
            target = "api"
            namespace = "team-a"
            count = 1
            start_after = "4m"
        "#;
        let scenario: ScriptedScenario = toml::from_str(toml_str).unwrap();
        let timeline = build_timeline(&scenario, 1.0).unwrap();
        assert_eq!(timeline.len(), 3);
        assert!(matches!(
            timeline[0].request.fault_type,
            FaultType::Delay { .. }
        ));
        assert!(matches!(
            timeline[1].request.fault_type,
            FaultType::Drop { .. }
        ));
        assert_eq!(timeline[2].request.namespace.as_deref(), Some("team-a"));
    }

    #[test]
    fn a_misspelt_step_field_fails_the_scenario() {
        let toml_str = r#"
            name = "typo"

            [[step]]
            description = "latency"
            fault = "delay"
            target = "pg"
            value = "500ms"
            namepsace = "team-a"
        "#;
        assert!(toml::from_str::<ScriptedScenario>(toml_str).is_err());
    }

    #[test]
    fn the_shipped_example_scenario_is_valid() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/phase-8/chaos-scenario.toml");
        let scenario = load_scenario(&path).unwrap();
        build_timeline(&scenario, 1.0).unwrap();
    }
}
