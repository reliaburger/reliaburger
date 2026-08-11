//! Marker-wrapped workload probes shared by the benchmark suites.
//!
//! The public `/v1/exec` endpoint returns only a child's combined output and
//! throws its exit status away, so a broken data plane can still print a
//! plausible-looking line and fool a naive `contains` check. Every bench probe
//! therefore runs inside a fixed `sh -c` wrapper that prints a private marker
//! carrying the real exit code. Parsing reuses Onion's `parse_probe_output`, so
//! there is exactly one marker parser in the codebase.

pub(crate) use crate::onion::trace::ProbeOutput;

/// Private marker the wrapper prints after the probe command.
const BENCH_PROBE_MARKER: &str = "__RB_BENCH_PROBE_STATUS__";

/// Wrap a fixed probe snippet so its exit status survives `exec`.
///
/// `snippet` references its arguments as `$1`, `$2`, … exactly like the trace
/// probes, keeping operator-derived values out of the script body. `$0` is a
/// fixed label, never caller input. The wrapper prints the private marker with
/// the snippet's exit status on its own line.
pub(crate) fn wrap_probe(snippet: &str, args: &[&str]) -> Vec<String> {
    let script = format!("{snippet}\nprintf '{BENCH_PROBE_MARKER}=%s\\n' \"$?\"");
    let mut command = vec![
        "sh".to_string(),
        "-c".to_string(),
        script,
        "reliaburger-bench".to_string(),
    ];
    command.extend(args.iter().map(|arg| (*arg).to_string()));
    command
}

/// Parse the private marker emitted by [`wrap_probe`].
///
/// Returns `None` when the marker is absent, which means the image lacked a
/// usable POSIX shell and the sample must be rejected rather than trusted.
pub(crate) fn parse_probe(output: &str) -> Option<ProbeOutput> {
    crate::onion::trace::parse_probe_output(output, BENCH_PROBE_MARKER)
}

/// Whether a busybox `nslookup` reply actually resolved `queried_name`.
///
/// A bare "Address" match is unsafe: busybox echoes the resolver's own address
/// in the reply header even for an NXDOMAIN, so we require a `Name:` answer line
/// that names the queried host.
pub(crate) fn dns_answer_resolved(lines: &[String], queried_name: &str) -> bool {
    lines.iter().any(|line| {
        let trimmed = line.trim();
        trimmed.to_ascii_lowercase().starts_with("name:") && trimmed.contains(queried_name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_probe_appends_marker_and_positional_arguments() {
        let command = wrap_probe("nslookup \"$1\" 2>&1", &["api.ns.internal"]);

        assert_eq!(command[0], "sh");
        assert_eq!(command[1], "-c");
        assert!(command[2].contains("nslookup"));
        assert!(command[2].contains("__RB_BENCH_PROBE_STATUS__=%s"));
        assert_eq!(command[3], "reliaburger-bench");
        assert_eq!(command[4], "api.ns.internal");
    }

    #[test]
    fn nxdomain_nslookup_output_is_rejected() {
        // busybox prints the resolver's own Address even for an NXDOMAIN and
        // exits non-zero. Both the status and the missing answer line reject it.
        let output = "Server:\t10.0.0.1\nAddress:\t10.0.0.1:53\n\n\
             *** Can't find api.ns.internal: No answer\n\
             __RB_BENCH_PROBE_STATUS__=1\n";
        let probe = parse_probe(output).unwrap();

        assert_eq!(probe.status, 1);
        assert!(!dns_answer_resolved(&probe.lines, "api.ns.internal"));
    }

    #[test]
    fn missing_wget_output_is_rejected() {
        let output = "sh: wget: not found\n__RB_BENCH_PROBE_STATUS__=127\n";
        let probe = parse_probe(output).unwrap();

        assert_ne!(probe.status, 0);
    }

    #[test]
    fn successful_nslookup_output_is_accepted() {
        let output = "Server:\t10.0.0.1\nAddress:\t10.0.0.1:53\n\n\
             Name:\tapi.ns.internal\nAddress:\t10.42.0.9\n\
             __RB_BENCH_PROBE_STATUS__=0\n";
        let probe = parse_probe(output).unwrap();

        assert_eq!(probe.status, 0);
        assert!(dns_answer_resolved(&probe.lines, "api.ns.internal"));
    }

    #[test]
    fn output_without_a_marker_is_unparseable() {
        assert!(parse_probe("Name:\tapi.ns.internal\n").is_none());
    }
}
