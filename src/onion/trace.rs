//! Versioned connectivity-trace contract and pure verdict helpers.

use serde::{Deserialize, Serialize};

/// Current trace response schema.
pub const TRACE_SCHEMA_VERSION: u32 = 1;

/// One fixed probe request. Bun never accepts an arbitrary command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceRequest {
    /// Source application.
    pub source: String,
    /// Source namespace.
    pub source_namespace: String,
    /// Destination application, hostname or IP address.
    pub destination: String,
    /// Namespace for an internal destination.
    pub destination_namespace: String,
    /// Destination port. Internal services may derive this from live state.
    pub port: Option<u16>,
}

/// Complete four-step trace response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceResult {
    /// Exact response schema.
    pub schema_version: u32,
    /// Namespace-qualified source.
    pub source: String,
    /// Namespace-qualified internal destination, or external host.
    pub destination: String,
    /// Port actually probed.
    pub destination_port: u16,
    /// Node on which the source workload probe ran.
    pub source_node: String,
    /// Ordered DNS, service, firewall and TCP evidence.
    pub steps: Vec<TraceStep>,
    /// Worst verdict across all steps.
    pub overall_result: TraceVerdict,
    /// Observed TCP probe latency, when a probe ran.
    pub latency_ms: Option<f64>,
}

/// One trace layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceStep {
    /// Stable one-based ordering.
    pub step_number: u32,
    /// Stable human-readable layer name.
    pub name: String,
    /// Whether the fact was observed, inferred or unavailable.
    pub evidence: TraceEvidence,
    /// Bounded supporting facts.
    pub details: Vec<String>,
    /// Layer verdict.
    pub verdict: TraceVerdict,
}

/// Provenance of a trace step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceEvidence {
    /// A workload probe or live kernel/userspace state was read.
    Observed,
    /// Current declarations predict this result, but the live enforcement
    /// state could not be read directly.
    Inferred,
    /// The required observation could not be made.
    Unavailable,
}

/// A layer or complete trace outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum TraceVerdict {
    /// The layer was observed or safely inferred to work.
    Pass,
    /// The layer demonstrably failed.
    Fail { reason: String },
    /// Evidence was unavailable or incomplete.
    Unknown { reason: String },
}

/// Parsed output from one fixed shell probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutput {
    /// Exit status printed by the fixed wrapper.
    pub status: i32,
    /// Bounded diagnostic lines without the private marker.
    pub lines: Vec<String>,
}

/// Parse the private exit marker emitted by the fixed workload probe.
pub fn parse_probe_output(output: &str, marker: &str) -> Option<ProbeOutput> {
    const MAX_DETAIL_LINES: usize = 16;
    let marker_prefix = format!("{marker}=");
    let mut status = None;
    let mut lines = Vec::new();
    for line in output.lines() {
        if let Some(value) = line.trim().strip_prefix(&marker_prefix) {
            status = value.parse::<i32>().ok();
        } else if lines.len() < MAX_DETAIL_LINES {
            lines.push(line.to_string());
        }
    }
    status.map(|status| ProbeOutput { status, lines })
}

/// Evaluate the kernel firewall state using the same rule as the connect hook.
pub fn evaluate_firewall(
    source_namespace: Option<u32>,
    destination_namespace: u32,
    action: Option<u32>,
) -> TraceVerdict {
    let Some(source_namespace) = source_namespace else {
        return TraceVerdict::Fail {
            reason: "source cgroup is absent from the live namespace map; firewall enforcement would fail open".to_string(),
        };
    };
    if source_namespace == destination_namespace {
        return TraceVerdict::Pass;
    }
    if action == Some(crate::sesame::firewall::FIREWALL_ALLOW) {
        TraceVerdict::Pass
    } else {
        TraceVerdict::Fail {
            reason: "live firewall map has no allow entry for this cross-namespace connection"
                .to_string(),
        }
    }
}

/// Select the worst step verdict without losing Unknown.
pub fn overall_verdict(steps: &[TraceStep]) -> TraceVerdict {
    if let Some(reason) = steps.iter().find_map(|step| match &step.verdict {
        TraceVerdict::Fail { reason } => Some(reason.clone()),
        TraceVerdict::Pass | TraceVerdict::Unknown { .. } => None,
    }) {
        return TraceVerdict::Fail { reason };
    }
    if let Some(reason) = steps.iter().find_map(|step| match &step.verdict {
        TraceVerdict::Unknown { reason } => Some(reason.clone()),
        TraceVerdict::Pass | TraceVerdict::Fail { .. } => None,
    }) {
        return TraceVerdict::Unknown { reason };
    }
    TraceVerdict::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(verdict: TraceVerdict) -> TraceStep {
        TraceStep {
            step_number: 1,
            name: "test".to_string(),
            evidence: TraceEvidence::Observed,
            details: Vec::new(),
            verdict,
        }
    }

    #[test]
    fn cross_namespace_firewall_needs_a_live_allow_entry() {
        assert!(matches!(
            evaluate_firewall(Some(1), 2, None),
            TraceVerdict::Fail { .. }
        ));
        assert_eq!(evaluate_firewall(Some(1), 2, Some(1)), TraceVerdict::Pass);
    }

    #[test]
    fn same_namespace_firewall_passes_without_an_allow_entry() {
        assert_eq!(evaluate_firewall(Some(7), 7, None), TraceVerdict::Pass);
    }

    #[test]
    fn missing_source_namespace_is_a_fail_open_security_failure() {
        assert!(matches!(
            evaluate_firewall(None, 7, Some(1)),
            TraceVerdict::Fail { .. }
        ));
    }

    #[test]
    fn overall_failure_wins_and_unknown_is_not_green() {
        let unknown = step(TraceVerdict::Unknown {
            reason: "not observed".to_string(),
        });
        assert!(matches!(
            overall_verdict(&[step(TraceVerdict::Pass), unknown.clone()]),
            TraceVerdict::Unknown { .. }
        ));
        assert!(matches!(
            overall_verdict(&[
                unknown,
                step(TraceVerdict::Fail {
                    reason: "denied".to_string(),
                }),
            ]),
            TraceVerdict::Fail { .. }
        ));
    }

    #[test]
    fn probe_marker_is_removed_and_output_is_bounded() {
        let input = format!(
            "{}\n__RB_TRACE_DNS_STATUS__=0\n",
            (0..40)
                .map(|number| format!("line-{number}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let parsed = parse_probe_output(&input, "__RB_TRACE_DNS_STATUS__").unwrap();
        assert_eq!(parsed.status, 0);
        assert_eq!(parsed.lines.len(), 16);
        assert!(!parsed.lines.iter().any(|line| line.contains("STATUS")));
    }

    #[test]
    fn response_json_rejects_unknown_contract_fields() {
        let result = TraceResult {
            schema_version: TRACE_SCHEMA_VERSION,
            source: "default/api".to_string(),
            destination: "default/db".to_string(),
            destination_port: 5432,
            source_node: "node-a".to_string(),
            steps: vec![step(TraceVerdict::Pass)],
            overall_result: TraceVerdict::Pass,
            latency_ms: Some(1.25),
        };
        let mut value = serde_json::to_value(result).unwrap();
        value["surprise"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TraceResult>(value).is_err());
    }
}
