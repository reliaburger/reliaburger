//! Versioned connectivity-trace contract and pure verdict helpers.

use serde::{Deserialize, Serialize};

/// Current trace response schema. Version 2 added the active-faults step,
/// the `degraded` verdict, `--count` and connect times measured inside the
/// source container.
pub const TRACE_SCHEMA_VERSION: u32 = 2;

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
    /// How many TCP connects to make (1 when absent, at most
    /// [`MAX_TRACE_CONNECTS`]).
    #[serde(default)]
    pub count: Option<u32>,
}

/// Complete five-step trace response.
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
    /// Ordered DNS, service, firewall, active-fault and TCP evidence.
    pub steps: Vec<TraceStep>,
    /// Worst verdict across all steps.
    pub overall_result: TraceVerdict,
    /// Median successful connect time, measured inside the source container
    /// (not the time to exec the probe), when its clock could measure it.
    pub latency_ms: Option<f64>,
    /// How the TCP connects went, when the probe reported them.
    #[serde(default)]
    pub connects: Option<ConnectSummary>,
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
    /// The layer works, but worse than it should: a delay or a partial drop
    /// is on the path, or only some connects succeeded.
    Degraded { reason: String },
}

/// Parsed output from one fixed shell probe.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeOutput {
    /// Exit status printed by the fixed wrapper.
    pub status: i32,
    /// Bounded diagnostic lines without the private markers.
    pub lines: Vec<String>,
    /// Timed connects the TCP probe reported (empty for other probes).
    pub attempts: Vec<ConnectAttempt>,
}

impl ProbeOutput {
    /// Extract exact IP addresses from nslookup's answer section. Resolver
    /// addresses and names containing address text are not answers.
    pub fn dns_answers(&self) -> Vec<std::net::IpAddr> {
        let mut in_answer = false;
        let mut addresses = Vec::new();
        for line in &self.lines {
            let line = line.trim();
            if line.starts_with("Name:") {
                in_answer = true;
                continue;
            }
            if !in_answer {
                continue;
            }
            let Some((label, values)) = line.split_once(':') else {
                continue;
            };
            let address_label = label == "Address"
                || label == "Addresses"
                || label
                    .strip_prefix("Address ")
                    .is_some_and(|index| index.parse::<u32>().is_ok());
            if address_label {
                addresses.extend(values.split_whitespace().filter_map(|value| {
                    value
                        .trim_matches(['[', ']', ','])
                        .parse::<std::net::IpAddr>()
                        .ok()
                }));
            }
        }
        addresses
    }
}

/// Parse the private exit marker emitted by the fixed workload probe.
pub fn parse_probe_output(output: &str, marker: &str) -> Option<ProbeOutput> {
    const MAX_DETAIL_LINES: usize = 16;
    let marker_prefix = format!("{marker}=");
    let attempt_prefix = format!("{CONNECT_ATTEMPT_MARKER}=");
    let mut status = None;
    let mut lines = Vec::new();
    let mut attempts = Vec::new();
    for line in output.lines() {
        if let Some(value) = line.trim().strip_prefix(&marker_prefix) {
            status = value.parse::<i32>().ok();
        } else if let Some(value) = line.trim().strip_prefix(&attempt_prefix) {
            if attempts.len() < MAX_TRACE_CONNECTS as usize {
                attempts.extend(ConnectAttempt::parse(value));
            }
        } else if lines.len() < MAX_DETAIL_LINES {
            lines.push(line.to_string());
        }
    }
    status.map(|status| ProbeOutput {
        status,
        lines,
        attempts,
    })
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
///
/// Fail beats everything. Degraded beats Unknown: it is a positive
/// observation that the path is impaired (usually by a fault someone
/// injected), which is more useful to report than a gap in the evidence.
pub fn overall_verdict(steps: &[TraceStep]) -> TraceVerdict {
    if let Some(reason) = steps.iter().find_map(|step| match &step.verdict {
        TraceVerdict::Fail { reason } => Some(reason.clone()),
        _ => None,
    }) {
        return TraceVerdict::Fail { reason };
    }
    if let Some(reason) = steps.iter().find_map(|step| match &step.verdict {
        TraceVerdict::Degraded { reason } => Some(reason.clone()),
        _ => None,
    }) {
        return TraceVerdict::Degraded { reason };
    }
    if let Some(reason) = steps.iter().find_map(|step| match &step.verdict {
        TraceVerdict::Unknown { reason } => Some(reason.clone()),
        _ => None,
    }) {
        return TraceVerdict::Unknown { reason };
    }
    TraceVerdict::Pass
}

/// The most TCP connects one trace may make (`--count`).
pub const MAX_TRACE_CONNECTS: u32 = 10;

/// Private marker prefixing each connect attempt the TCP probe reports.
pub const CONNECT_ATTEMPT_MARKER: &str = "__RB_TRACE_TCP_ATTEMPT__";

/// One timed TCP connect made inside the source workload.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConnectAttempt {
    /// Whether the connect succeeded.
    pub succeeded: bool,
    /// Connect time measured inside the container, when its clock could say.
    pub elapsed_ms: Option<f64>,
    /// Whether that time came from `/proc/uptime`, good to 10 ms only.
    pub coarse: bool,
}

impl ConnectAttempt {
    /// Parse `STATUS START_NS END_NS START_UPTIME END_UPTIME` from the probe.
    ///
    /// `date +%s%N` gives nanoseconds where the image's `date` supports `%N`.
    /// BusyBox builds without it print seconds (or `%N` literally), so a value
    /// that isn't plausibly nanoseconds since 1970 is ignored and the
    /// `/proc/uptime` readings, good to 10 ms, are used instead. With neither,
    /// the time is unknown rather than wrong.
    fn parse(value: &str) -> Option<Self> {
        // Nanoseconds since the epoch passed 10^18 in 2001; seconds are ~10^9.
        const PLAUSIBLE_EPOCH_NS: u128 = 1_000_000_000_000_000_000;
        let mut fields = value.split_whitespace();
        let status: i32 = fields.next()?.parse().ok()?;
        let nanos = |text: Option<&str>| {
            text.and_then(|text| text.parse::<u128>().ok())
                .filter(|value| *value >= PLAUSIBLE_EPOCH_NS)
        };
        let uptime = |text: Option<&str>| text.and_then(|text| text.parse::<f64>().ok());
        let (started, ended) = (nanos(fields.next()), nanos(fields.next()));
        let (up_started, up_ended) = (uptime(fields.next()), uptime(fields.next()));
        let (elapsed_ms, coarse) = match (started, ended, up_started, up_ended) {
            (Some(started), Some(ended), _, _) if ended >= started => {
                (Some((ended - started) as f64 / 1_000_000.0), false)
            }
            (_, _, Some(started), Some(ended)) if ended >= started => {
                (Some((ended - started) * 1000.0), true)
            }
            _ => (None, false),
        };
        Some(Self {
            succeeded: status == 0,
            elapsed_ms,
            coarse,
        })
    }
}

/// What `--count` connects added up to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectSummary {
    /// Connects attempted.
    pub attempted: u32,
    /// Connects that succeeded.
    pub succeeded: u32,
    /// Fastest successful connect, measured inside the container.
    pub min_ms: Option<f64>,
    /// Median successful connect, measured inside the container.
    pub median_ms: Option<f64>,
    /// How fine the container's clock was: 10 ms when only `/proc/uptime`
    /// could time the connects.
    #[serde(default)]
    pub clock_resolution_ms: Option<f64>,
}

/// Summarise connect attempts; `None` when the probe reported none.
pub fn summarise_connects(attempts: &[ConnectAttempt]) -> Option<ConnectSummary> {
    if attempts.is_empty() {
        return None;
    }
    let mut times: Vec<f64> = attempts
        .iter()
        .filter(|attempt| attempt.succeeded)
        .filter_map(|attempt| attempt.elapsed_ms)
        .collect();
    times.sort_by(f64::total_cmp);
    Some(ConnectSummary {
        attempted: attempts.len() as u32,
        succeeded: attempts.iter().filter(|attempt| attempt.succeeded).count() as u32,
        min_ms: times.first().copied(),
        median_ms: times.get(times.len() / 2).copied(),
        clock_resolution_ms: attempts
            .iter()
            .any(|attempt| attempt.coarse)
            .then_some(10.0),
    })
}

/// The TCP probe step, judged from its connect attempts.
///
/// Every connect succeeding passes; some failing is `Degraded` (the path
/// works, unreliably); all failing fails. A missing probe tool is `Unknown`,
/// never a network failure.
pub fn tcp_probe_step(
    step_number: u32,
    target: &str,
    probe: Result<ProbeOutput, String>,
) -> TraceStep {
    let name = "TCP probe".to_string();
    let probe = match probe {
        Ok(probe) => probe,
        Err(reason) => {
            return TraceStep {
                step_number,
                name,
                evidence: TraceEvidence::Unavailable,
                details: vec![format!("connect to {target}")],
                verdict: TraceVerdict::Unknown { reason },
            };
        }
    };
    let summary = summarise_connects(&probe.attempts);
    let mut details = vec![match &summary {
        Some(summary) => format!(
            "{}/{} connects to {target} succeeded{}",
            summary.succeeded,
            summary.attempted,
            match (
                summary.min_ms,
                summary.median_ms,
                summary.clock_resolution_ms
            ) {
                (Some(min), Some(median), None) => {
                    format!(" (connect time min {min:.1} ms, median {median:.1} ms)")
                }
                (Some(min), Some(median), Some(resolution)) => format!(
                    " (connect time min {min:.0} ms, median {median:.0} ms, {resolution:.0} ms clock)"
                ),
                _ => String::new(),
            }
        ),
        None => format!("connect to {target}"),
    }];
    details.extend(
        probe
            .lines
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .map(str::to_string),
    );
    let verdict = match summary {
        _ if probe.status == 126 || probe.status == 127 => TraceVerdict::Unknown {
            reason: "source image does not provide the fixed TCP probe tool".to_string(),
        },
        Some(summary) if summary.succeeded == summary.attempted => TraceVerdict::Pass,
        Some(summary) if summary.succeeded > 0 => TraceVerdict::Degraded {
            reason: format!(
                "only {}/{} connects succeeded",
                summary.succeeded, summary.attempted
            ),
        },
        Some(summary) => TraceVerdict::Fail {
            reason: format!("all {} connects failed", summary.attempted),
        },
        None if probe.status == 0 => TraceVerdict::Pass,
        None => TraceVerdict::Fail {
            reason: format!("TCP probe exited with status {}", probe.status),
        },
    };
    TraceStep {
        step_number,
        name,
        evidence: TraceEvidence::Observed,
        details,
        verdict,
    }
}

/// The DNS step's details: the answer and resolver on success, and only the
/// lines that explain a failure otherwise, instead of nslookup's full output.
pub fn dns_details(name: &str, probe: &ProbeOutput) -> Vec<String> {
    let answers = probe.dns_answers();
    if probe.status == 0 && !answers.is_empty() {
        let answers: Vec<String> = answers.iter().map(ToString::to_string).collect();
        let resolver = probe.lines.iter().find_map(|line| {
            let line = line.trim();
            let value = line.strip_prefix("Server:")?.trim();
            (!value.is_empty()).then(|| value.to_string())
        });
        return vec![match resolver {
            Some(resolver) => format!("{name} -> {} (resolver {resolver})", answers.join(", ")),
            None => format!("{name} -> {}", answers.join(", ")),
        }];
    }
    let mut details = vec![format!("{name} did not resolve")];
    details.extend(
        probe
            .lines
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .filter(|line| !line.starts_with("Server:") && !line.starts_with("Address:"))
            .map(str::to_string),
    );
    details
}

/// What kind of fault is on the path, as far as its verdict goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFaultKind {
    /// Every connect is refused.
    Partition,
    /// A share of connects is refused.
    Drop {
        /// 0-100.
        probability: u8,
    },
    /// Packets are held back.
    Delay,
    /// The name doesn't resolve.
    DnsNxdomain,
    /// Anything else that acts on callers.
    Other,
}

/// An active fault that applies to this source's calls to the destination.
#[derive(Debug, Clone, PartialEq)]
pub struct PathFault {
    /// Node-local fault id.
    pub id: u64,
    /// The fault's kind, for the verdict.
    pub kind: PathFaultKind,
    /// Human description with parameters, e.g. `delay 300ms from frontend`.
    pub description: String,
    /// Seconds until it expires.
    pub remaining_secs: u64,
}

/// The "Active faults" step: which faults act on this path, with live kernel
/// evidence where the node could read it.
///
/// A partition, an NXDOMAIN or a 100% drop fails the path; a partial drop or
/// a delay degrades it. `evidence` lines come from the live eBPF map and the
/// source's qdisc; without any, a fault listing is only what Bun's registry
/// says, so it is labelled inferred.
pub fn path_faults_step(
    step_number: u32,
    faults: &[PathFault],
    evidence: Vec<String>,
) -> TraceStep {
    let name = "Active faults".to_string();
    if faults.is_empty() {
        return TraceStep {
            step_number,
            name,
            evidence: TraceEvidence::Observed,
            details: vec!["no fault acts on this path".to_string()],
            verdict: TraceVerdict::Pass,
        };
    }
    let mut details: Vec<String> = faults
        .iter()
        .map(|fault| {
            format!(
                "fault {}: {} ({}s left)",
                fault.id, fault.description, fault.remaining_secs
            )
        })
        .collect();
    let observed = !evidence.is_empty();
    details.extend(evidence);
    let blocking = faults.iter().find(|fault| {
        matches!(
            fault.kind,
            PathFaultKind::Partition
                | PathFaultKind::DnsNxdomain
                | PathFaultKind::Drop { probability: 100 }
        )
    });
    let verdict = match blocking {
        Some(fault) => TraceVerdict::Fail {
            reason: format!(
                "fault {} ({}) blocks this path",
                fault.id, fault.description
            ),
        },
        None => TraceVerdict::Degraded {
            reason: format!(
                "fault {} ({}) is active on this path",
                faults[0].id, faults[0].description
            ),
        },
    };
    TraceStep {
        step_number,
        name,
        evidence: if observed {
            TraceEvidence::Observed
        } else {
            TraceEvidence::Inferred
        },
        details,
        verdict,
    }
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
    fn dns_answers_parse_exact_ipv4_and_ipv6_values_only() {
        let probe = parse_probe_output("Server: resolver\nAddress: 10.0.0.1#53\nName: 10.0.0.1.invalid\nAddress 1: 10.0.0.10 name\nAddress 2: 2001:0db8:0:0:0:0:0:1\nAddress: [2001:db8::2]\nSTATUS=0", "STATUS").unwrap();
        assert_eq!(
            probe.dns_answers(),
            [
                "10.0.0.10".parse::<std::net::IpAddr>().unwrap(),
                "2001:db8::1".parse().unwrap(),
                "2001:db8::2".parse().unwrap()
            ]
        );
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
    fn a_degraded_path_outranks_missing_evidence_but_not_a_failure() {
        let degraded = step(TraceVerdict::Degraded {
            reason: "delay".to_string(),
        });
        let unknown = step(TraceVerdict::Unknown {
            reason: "no maps".to_string(),
        });
        assert!(matches!(
            overall_verdict(&[unknown.clone(), degraded.clone()]),
            TraceVerdict::Degraded { .. }
        ));
        assert!(matches!(
            overall_verdict(&[
                degraded,
                step(TraceVerdict::Fail {
                    reason: "partition".to_string(),
                }),
            ]),
            TraceVerdict::Fail { .. }
        ));
    }

    fn fault(id: u64, kind: PathFaultKind, description: &str) -> PathFault {
        PathFault {
            id,
            kind,
            description: description.to_string(),
            remaining_secs: 540,
        }
    }

    #[test]
    fn a_partition_fails_the_path_and_names_the_fault() {
        let step = path_faults_step(
            4,
            &[
                fault(2, PathFaultKind::Delay, "delay 300ms from frontend"),
                fault(3, PathFaultKind::Partition, "partition from frontend"),
            ],
            vec!["live fault_connect_map: partition for source cgroup 42".to_string()],
        );
        assert_eq!(step.evidence, TraceEvidence::Observed);
        assert_eq!(
            step.verdict,
            TraceVerdict::Fail {
                reason: "fault 3 (partition from frontend) blocks this path".to_string()
            }
        );
        assert!(step.details[0].contains("fault 2: delay 300ms from frontend (540s left)"));
    }

    #[test]
    fn a_delay_or_partial_drop_degrades_and_no_fault_passes() {
        let delayed = path_faults_step(4, &[fault(7, PathFaultKind::Delay, "delay 300ms")], vec![]);
        assert!(matches!(delayed.verdict, TraceVerdict::Degraded { .. }));
        assert_eq!(delayed.evidence, TraceEvidence::Inferred);
        let dropped = path_faults_step(
            4,
            &[fault(
                8,
                PathFaultKind::Drop { probability: 30 },
                "drop 30%",
            )],
            vec![],
        );
        assert!(matches!(dropped.verdict, TraceVerdict::Degraded { .. }));
        let everything = path_faults_step(
            4,
            &[fault(
                9,
                PathFaultKind::Drop { probability: 100 },
                "drop 100%",
            )],
            vec![],
        );
        assert!(matches!(everything.verdict, TraceVerdict::Fail { .. }));
        assert_eq!(path_faults_step(4, &[], vec![]).verdict, TraceVerdict::Pass);
    }

    #[test]
    fn connect_attempts_are_parsed_out_of_the_probe_and_summarised() {
        let output = "__RB_TRACE_TCP_ATTEMPT__=0 1727000000000000000 1727000000301000000\n\
                      __RB_TRACE_TCP_ATTEMPT__=1 1727000001400000000 1727000001400100000\n\
                      __RB_TRACE_TCP_ATTEMPT__=0 1727000002000000000 1727000002300500000\n\
                      __RB_TRACE_TCP_ATTEMPT__=0 1727000003000000000 1727000003299000000\n\
                      __RB_TRACE_TCP_STATUS__=0\n";
        let probe = parse_probe_output(output, "__RB_TRACE_TCP_STATUS__").unwrap();
        assert!(probe.lines.is_empty());
        let summary = summarise_connects(&probe.attempts).unwrap();
        assert_eq!((summary.attempted, summary.succeeded), (4, 3));
        assert_eq!(summary.min_ms, Some(299.0));
        assert_eq!(summary.median_ms, Some(300.5));

        let step = tcp_probe_step(5, "127.128.0.9:6379", Ok(probe));
        assert_eq!(
            step.verdict,
            TraceVerdict::Degraded {
                reason: "only 3/4 connects succeeded".to_string()
            }
        );
        assert_eq!(
            step.details[0],
            "3/4 connects to 127.128.0.9:6379 succeeded (connect time min 299.0 ms, median 300.5 ms)"
        );
    }

    #[test]
    fn a_date_without_nanoseconds_falls_back_to_uptime_or_to_unknown() {
        // BusyBox without %N prints whole seconds: never read those as ns.
        let probe = parse_probe_output(
            "__RB_TRACE_TCP_ATTEMPT__=0 1727000000 1727000000 5021.40 5021.71\n\
             __RB_TRACE_TCP_ATTEMPT__=0 1727000000%N 1727000001%N\n\
             __RB_TRACE_TCP_STATUS__=0\n",
            "__RB_TRACE_TCP_STATUS__",
        )
        .unwrap();
        let times: Vec<Option<f64>> = probe
            .attempts
            .iter()
            .map(|attempt| attempt.elapsed_ms.map(f64::round))
            .collect();
        assert_eq!(times, vec![Some(310.0), None]);
        let summary = summarise_connects(&probe.attempts).unwrap();
        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.clock_resolution_ms, Some(10.0));
        let step = tcp_probe_step(5, "x:1", Ok(probe.clone()));
        assert_eq!(
            step.details[0],
            "2/2 connects to x:1 succeeded (connect time min 310 ms, median 310 ms, 10 ms clock)"
        );
        assert_eq!(
            tcp_probe_step(5, "x:1", Ok(probe)).verdict,
            TraceVerdict::Pass
        );
    }

    #[test]
    fn every_connect_failing_fails_and_a_missing_tool_is_unknown() {
        let refused = parse_probe_output(
            "nc: 127.128.0.9 (127.128.0.9:6379): Operation not permitted\n\
             __RB_TRACE_TCP_ATTEMPT__=1 1 2\n__RB_TRACE_TCP_STATUS__=1\n",
            "__RB_TRACE_TCP_STATUS__",
        )
        .unwrap();
        let step = tcp_probe_step(5, "127.128.0.9:6379", Ok(refused));
        assert!(matches!(step.verdict, TraceVerdict::Fail { .. }));
        assert!(
            step.details[1].contains("Operation not permitted"),
            "{:?}",
            step.details
        );

        let missing = ProbeOutput {
            status: 127,
            lines: vec!["sh: nc: not found".to_string()],
            attempts: vec![ConnectAttempt {
                succeeded: false,
                elapsed_ms: None,
                coarse: false,
            }],
        };
        assert!(matches!(
            tcp_probe_step(5, "x:1", Ok(missing)).verdict,
            TraceVerdict::Unknown { .. }
        ));
    }

    #[test]
    fn dns_details_keep_the_answer_and_drop_the_resolver_noise() {
        let answered = parse_probe_output(
            "Server:\t\t10.202.142.1\nAddress:\t10.202.142.1:53\n\nName:\tredis.default.internal\nAddress: 127.128.202.174\n\n__RB_TRACE_DNS_STATUS__=0\n",
            "__RB_TRACE_DNS_STATUS__",
        )
        .unwrap();
        assert_eq!(
            dns_details("redis.default.internal", &answered),
            vec!["redis.default.internal -> 127.128.202.174 (resolver 10.202.142.1)"]
        );
        let missing = parse_probe_output(
            "Server:\t\t10.202.142.1\nAddress:\t10.202.142.1:53\n\n** server can't find redis.default.internal: NXDOMAIN\n\n__RB_TRACE_DNS_STATUS__=1\n",
            "__RB_TRACE_DNS_STATUS__",
        )
        .unwrap();
        assert_eq!(
            dns_details("redis.default.internal", &missing),
            vec![
                "redis.default.internal did not resolve",
                "** server can't find redis.default.internal: NXDOMAIN",
            ]
        );
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
            connects: None,
        };
        let mut value = serde_json::to_value(result).unwrap();
        value["surprise"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TraceResult>(value).is_err());
    }
}
