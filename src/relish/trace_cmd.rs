//! `relish trace` source-node selection, rendering and exit contract.

use std::fmt::Write as _;
use std::time::Duration;

use crate::bun::agent::NodeStatus;
use crate::onion::trace::{TraceEvidence, TraceRequest, TraceResult, TraceVerdict};

use super::client::BunClient;
use super::{CommandOutcome, OutputFormat, RelishError};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// Arguments accepted by the trace command body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceArgs {
    /// Source application.
    pub source: String,
    /// Source namespace.
    pub source_namespace: String,
    /// Internal app name or external hostname/IP.
    pub destination: String,
    /// Internal destination namespace.
    pub destination_namespace: String,
    /// Destination port, derived for internal services when absent.
    pub port: Option<u16>,
    /// TCP connects to make (1-10).
    pub count: u32,
    /// Human, JSON or YAML output.
    pub output: OutputFormat,
}

/// Run `relish trace` against the configured cluster.
pub async fn run(args: TraceArgs) -> Result<CommandOutcome, RelishError> {
    run_with_client(args, &BunClient::default_local()).await
}

/// Run with an explicit entry client for tests and embedded callers.
pub async fn run_with_client(
    args: TraceArgs,
    entry: &BunClient,
) -> Result<CommandOutcome, RelishError> {
    let request = TraceRequest {
        source: args.source,
        source_namespace: args.source_namespace,
        destination: args.destination,
        destination_namespace: args.destination_namespace,
        port: args.port,
        count: Some(args.count),
    };
    let result = trace(&request, entry).await?;
    println!("{}", render_result(&result, args.output)?);
    Ok(outcome(&result.overall_result))
}

/// Run a trace on a node that runs the source workload, reached through the
/// entry node's relay.
pub async fn trace(request: &TraceRequest, entry: &BunClient) -> Result<TraceResult, RelishError> {
    let source = find_source_client(entry, &request.source, &request.source_namespace).await?;
    tokio::time::timeout(Duration::from_secs(50), source.trace(request))
        .await
        .map_err(|_| RelishError::RequestTimeout)?
}

async fn find_source_client(
    entry: &BunClient,
    app: &str,
    namespace: &str,
) -> Result<BunClient, RelishError> {
    let nodes = tokio::time::timeout(DISCOVERY_TIMEOUT, entry.nodes())
        .await
        .map_err(|_| RelishError::RequestTimeout)??;
    let candidates = if nodes.is_empty() {
        vec![("local".to_string(), entry.clone())]
    } else {
        nodes
            .into_iter()
            .filter(|node| node.state == "alive")
            .filter_map(|node| node_candidate(entry, node))
            .collect::<Vec<_>>()
    };
    let statuses = futures_util::future::join_all(candidates.into_iter().map(
        |(node_id, client)| async move {
            let result = tokio::time::timeout(DISCOVERY_TIMEOUT, client.status()).await;
            (node_id, client, result)
        },
    ))
    .await;
    statuses
        .into_iter()
        .filter_map(|(node_id, client, result)| match result {
            Ok(Ok(instances))
                if instances.iter().any(|instance| {
                    instance.app_name == app
                        && instance.namespace == namespace
                        && instance.state == "running"
                }) =>
            {
                Some((node_id, client))
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => None,
        })
        .min_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, client)| client)
        .ok_or_else(|| RelishError::ApiError {
            status: 404,
            body: format!("no running instance of {namespace}/{app} was found on a reachable node"),
        })
}

fn node_candidate(entry: &BunClient, node: NodeStatus) -> Option<(String, BunClient)> {
    let node_id = node.node_id.clone();
    super::wtf::node_client(entry, &node)
        .ok()
        .map(|client| (node_id, client))
}

fn outcome(verdict: &TraceVerdict) -> CommandOutcome {
    match verdict {
        TraceVerdict::Pass => CommandOutcome::Clean,
        TraceVerdict::Fail { .. } => CommandOutcome::Problems,
        TraceVerdict::Unknown { .. } | TraceVerdict::Degraded { .. } => CommandOutcome::Warnings,
    }
}

fn render_result(result: &TraceResult, output: OutputFormat) -> Result<String, RelishError> {
    match output {
        OutputFormat::Human => Ok(render_human(result)),
        OutputFormat::Json => {
            serde_json::to_string_pretty(result).map_err(RelishError::SerialiseJson)
        }
        OutputFormat::Yaml => serde_yaml::to_string(result).map_err(RelishError::SerialiseYaml),
    }
}

fn render_human(result: &TraceResult) -> String {
    let mut output = format!(
        "Trace {} -> {}:{} from node {}\n",
        result.source, result.destination, result.destination_port, result.source_node
    );
    for step in &result.steps {
        let _ = writeln!(
            output,
            "  {}. {} [{}; {}]",
            step.step_number,
            step.name,
            verdict_name(&step.verdict),
            evidence_name(step.evidence)
        );
        for detail in &step.details {
            let _ = writeln!(output, "     {detail}");
        }
        match &step.verdict {
            TraceVerdict::Fail { reason }
            | TraceVerdict::Unknown { reason }
            | TraceVerdict::Degraded { reason } => {
                let _ = writeln!(output, "     reason: {reason}");
            }
            TraceVerdict::Pass => {}
        }
    }
    let _ = write!(output, "Overall: {}", verdict_name(&result.overall_result));
    match (&result.connects, result.latency_ms) {
        (Some(connects), Some(latency_ms)) => {
            let _ = write!(
                output,
                " ({}/{} connects, median connect {latency_ms:.1} ms)",
                connects.succeeded, connects.attempted
            );
        }
        (Some(connects), None) => {
            let _ = write!(
                output,
                " ({}/{} connects)",
                connects.succeeded, connects.attempted
            );
        }
        (None, Some(latency_ms)) => {
            let _ = write!(output, " (median connect {latency_ms:.1} ms)");
        }
        (None, None) => {}
    }
    if let TraceVerdict::Fail { reason } | TraceVerdict::Degraded { reason } =
        &result.overall_result
    {
        let _ = write!(output, "\n  because {reason}");
    }
    output
}

fn verdict_name(verdict: &TraceVerdict) -> &'static str {
    match verdict {
        TraceVerdict::Pass => "PASS",
        TraceVerdict::Fail { .. } => "FAIL",
        TraceVerdict::Unknown { .. } => "UNKNOWN",
        TraceVerdict::Degraded { .. } => "DEGRADED",
    }
}

fn evidence_name(evidence: TraceEvidence) -> &'static str {
    match evidence {
        TraceEvidence::Observed => "observed",
        TraceEvidence::Inferred => "inferred",
        TraceEvidence::Unavailable => "unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onion::trace::{TRACE_SCHEMA_VERSION, TraceStep};

    fn result(verdict: TraceVerdict) -> TraceResult {
        TraceResult {
            schema_version: TRACE_SCHEMA_VERSION,
            source: "default/api".to_string(),
            destination: "default/db".to_string(),
            destination_port: 5432,
            source_node: "node-a".to_string(),
            steps: vec![TraceStep {
                step_number: 1,
                name: "DNS query".to_string(),
                evidence: TraceEvidence::Inferred,
                details: vec!["service map predicts 127.128.0.2".to_string()],
                verdict: verdict.clone(),
            }],
            overall_result: verdict,
            latency_ms: None,
            connects: None,
        }
    }

    #[test]
    fn inferred_evidence_is_labelled_in_human_output() {
        let output = render_human(&result(TraceVerdict::Pass));
        assert!(output.contains("[PASS; inferred]"));
    }

    #[test]
    fn unknown_maps_to_exit_two_and_machine_contract_keeps_it() {
        let result = result(TraceVerdict::Unknown {
            reason: "probe tool missing".to_string(),
        });
        assert_eq!(outcome(&result.overall_result), CommandOutcome::Warnings);
        let json = render_result(&result, OutputFormat::Json).unwrap();
        assert!(json.contains("\"verdict\": \"unknown\""));
    }

    #[test]
    fn a_degraded_path_maps_to_exit_two() {
        let result = result(TraceVerdict::Degraded {
            reason: "fault 3 (delay 300ms from frontend) is active on this path".to_string(),
        });
        assert_eq!(outcome(&result.overall_result), CommandOutcome::Warnings);
    }

    /// The tour's `relish trace frontend --to redis --count 10` under a
    /// delay: the fault is named, with its live evidence, and the connect
    /// figures come from inside the container.
    #[test]
    fn a_trace_through_a_delay_shows_the_fault_and_the_connect_times() {
        use crate::onion::trace::{ConnectSummary, TraceStep};
        let step = |number: u32, name: &str, details: &[&str], verdict: TraceVerdict| TraceStep {
            step_number: number,
            name: name.to_string(),
            evidence: TraceEvidence::Observed,
            details: details.iter().map(|detail| detail.to_string()).collect(),
            verdict,
        };
        let degraded = TraceVerdict::Degraded {
            reason: "fault 3 (delay 300ms from frontend) is active on this path".to_string(),
        };
        let result = TraceResult {
            schema_version: TRACE_SCHEMA_VERSION,
            source: "default/frontend".to_string(),
            destination: "default/redis".to_string(),
            destination_port: 6379,
            source_node: "node-1".to_string(),
            steps: vec![
                step(
                    1,
                    "DNS query",
                    &["redis.default.internal -> 127.128.202.174 (resolver 10.202.142.1)"],
                    TraceVerdict::Pass,
                ),
                step(
                    2,
                    "Service and eBPF state",
                    &[
                        "userspace service map: VIP 127.128.202.174, 1 of 1 backends healthy",
                        "  backend default__redis-0 at 10.202.142.7:6379 (healthy)",
                        "the VIP sends every connect to default__redis-0 at 10.202.142.7:6379",
                        "live backend_map: 1 entries, 1 healthy",
                        "  kernel backend 10.202.142.7:6379 (healthy)",
                    ],
                    TraceVerdict::Pass,
                ),
                step(
                    3,
                    "Firewall state",
                    &[
                        "live maps: source cgroup 4242, source namespace Some(7), destination namespace 7, action None",
                    ],
                    TraceVerdict::Pass,
                ),
                step(
                    4,
                    "Active faults",
                    &[
                        "fault 3: delay 300ms from frontend (571s left)",
                        "live netem on the source's eth0: delay 300ms",
                    ],
                    degraded.clone(),
                ),
                step(
                    5,
                    "TCP probe",
                    &[
                        "10/10 connects to 127.128.202.174:6379 succeeded (connect time min 300.9 ms, median 301.6 ms)",
                    ],
                    TraceVerdict::Pass,
                ),
            ],
            overall_result: degraded,
            latency_ms: Some(301.6),
            connects: Some(ConnectSummary {
                attempted: 10,
                succeeded: 10,
                min_ms: Some(300.9),
                median_ms: Some(301.6),
                clock_resolution_ms: None,
            }),
        };
        insta::assert_snapshot!(render_human(&result));
    }

    #[test]
    fn failed_trace_maps_to_exit_one() {
        let result = result(TraceVerdict::Fail {
            reason: "firewall denied".to_string(),
        });
        assert_eq!(outcome(&result.overall_result), CommandOutcome::Problems);
    }
}
