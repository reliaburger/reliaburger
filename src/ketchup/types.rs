//! Types for Ketchup log collection.

use serde::{Deserialize, Serialize};

/// Which output stream a log line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// Where a captured line ended in the runtime's capture file.
///
/// Capture files are append-only for as long as they exist, so the byte
/// offset just past a line's newline names that line for good. The log store
/// remembers the highest offset it has ingested from each file and skips any
/// line at or below it, which is what stops a restarted agent re-ingesting
/// output it already stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePosition {
    /// The capture file the line was read from.
    pub file: std::path::PathBuf,
    /// Byte offset just past the line's terminating newline.
    pub end_offset: u64,
}

/// Where the log store has already read each capture file up to.
///
/// A restarted agent hands these to its log forwarders so they resume each
/// capture file at the offset the store checkpointed, instead of reading
/// (and discarding) everything before it again. A file with no entry starts
/// at byte 0.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureOffsets(pub std::collections::BTreeMap<std::path::PathBuf, u64>);

impl CaptureOffsets {
    /// The checkpointed offset for `file`, if the store holds lines from it.
    pub fn get(&self, file: &std::path::Path) -> Option<u64> {
        self.0.get(file).copied()
    }
}

/// One line of workload output as a runtime captured it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedLine {
    /// Which stream produced the line.
    pub stream: LogStream,
    /// The line, without its newline.
    pub line: String,
    /// Where the line sits in its capture file. `None` when the runtime
    /// captures to memory or to a stream with no stable offsets.
    pub position: Option<CapturePosition>,
}

/// A container log line tagged with its source app, namespace and instance.
///
/// Emitted by the agent's per-instance log forwarders and drained into the
/// `LogStore` so container output is queryable via `/v1/logs/entries`.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub app: String,
    pub namespace: String,
    /// The instance that wrote the line.
    pub instance: String,
    pub stream: LogStream,
    pub line: String,
    /// Where the line sits in its capture file, for exactly-once ingestion.
    pub position: Option<CapturePosition>,
}

/// A single log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Seconds since Unix epoch.
    pub timestamp: u64,
    /// Ingest order on the node that stored the line: nanoseconds since the
    /// Unix epoch, bumped so it rises strictly with every line. Sorting by it
    /// gives emission order per instance, which one-second `timestamp`s can't.
    pub sequence: u64,
    /// The instance that wrote the line, when a workload wrote it.
    pub instance: Option<String>,
    /// The node that stored the line. A cross-node query fills it in; a
    /// node answering for itself leaves it out. An instance that moves
    /// keeps its name, so only the node tells its two runs apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Which stream produced this line.
    pub stream: LogStream,
    /// The log line content.
    pub line: String,
}

/// Parameters for a log query.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// App name.
    pub app: String,
    /// Namespace.
    pub namespace: String,
    /// Start time (inclusive, seconds since epoch).
    pub start: Option<u64>,
    /// End time (inclusive, seconds since epoch).
    pub end: Option<u64>,
    /// Grep pattern (a regular expression).
    pub grep: Option<String>,
    /// Only this instance's lines.
    pub instance: Option<String>,
    /// JSON field filter (key=value).
    pub json_field: Option<(String, String)>,
    /// Return only the last N lines.
    pub tail: Option<usize>,
}

/// Result of a cross-node log query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogQueryResult {
    /// Merged log entries from all queried nodes.
    pub entries: Vec<LogEntry>,
    /// Number of nodes that were queried.
    pub node_count: usize,
    /// Warnings about partial results.
    pub warnings: Vec<LogQueryWarning>,
}

/// Warning annotation on log query results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogQueryWarning {
    /// A node the query needed contributed no lines, and why.
    NodeFailed {
        node_id: String,
        reason: NodeFailureReason,
    },
}

impl std::fmt::Display for LogQueryWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NodeFailed { node_id, reason } => {
                write!(f, "no logs from node {node_id}: {reason}")
            }
        }
    }
}

/// Why a node contributed nothing to a cross-node log query.
///
/// The kinds point at different culprits: a node missing from membership is
/// a gossip question, a timeout a slow or stuck node, a transport error the
/// network or certificates, and an HTTP status or bad body the node's own API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeFailureReason {
    /// The app is placed there, but the node has no live membership entry,
    /// so there's no address to ask.
    NotInMembership,
    /// No complete answer (headers and body) within the per-node deadline.
    TimedOut { after_ms: u64 },
    /// The request never got an HTTP answer: refused or reset connection,
    /// failed TLS handshake, DNS.
    Transport { detail: String },
    /// The node answered with a non-success HTTP status.
    HttpStatus { status: u16, body: String },
    /// The node answered 2xx with a body that isn't a list of log entries.
    BadBody { detail: String },
    /// The coordinator's own per-node query task failed.
    Internal { detail: String },
}

impl std::fmt::Display for NodeFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInMembership => write!(f, "not in the membership table"),
            Self::TimedOut { after_ms } if after_ms % 1000 == 0 => {
                write!(f, "timed out after {}s", after_ms / 1000)
            }
            Self::TimedOut { after_ms } => write!(f, "timed out after {after_ms}ms"),
            Self::Transport { detail } => write!(f, "request failed (connect or TLS): {detail}"),
            Self::HttpStatus { status, body } if body.is_empty() => {
                write!(f, "answered HTTP {status}")
            }
            Self::HttpStatus { status, body } => write!(f, "answered HTTP {status}: {body}"),
            Self::BadBody { detail } => write!(f, "answered with an unreadable body: {detail}"),
            Self::Internal { detail } => write!(f, "query task failed: {detail}"),
        }
    }
}

/// Errors from Ketchup operations.
#[derive(Debug, thiserror::Error)]
pub enum KetchupError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("log not found for {app} in {namespace}")]
    NotFound { app: String, namespace: String },
    #[error("query rejected: {reason}")]
    QueryRejected { reason: String },
    /// Another exporter holds this directory's export checkpoint lock.
    #[error("export checkpoint is busy: another export is in flight")]
    ExportBusy,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_query_result_json_round_trip() {
        let result = LogQueryResult {
            entries: vec![LogEntry {
                timestamp: 1000,
                sequence: 1,
                instance: None,
                node: None,
                stream: LogStream::Stdout,
                line: "hello".to_string(),
            }],
            node_count: 3,
            warnings: vec![LogQueryWarning::NodeFailed {
                node_id: "node-2".to_string(),
                reason: NodeFailureReason::TimedOut { after_ms: 10_000 },
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: LogQueryResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.node_count, 3);
        assert_eq!(decoded.warnings, result.warnings);
    }

    #[test]
    fn log_query_warning_carries_its_reason_on_the_wire() {
        let w = LogQueryWarning::NodeFailed {
            node_id: "n1".to_string(),
            reason: NodeFailureReason::HttpStatus {
                status: 503,
                body: "draining".to_string(),
            },
        };
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"NodeFailed": {
                "node_id": "n1",
                "reason": {"kind": "http_status", "status": 503, "body": "draining"},
            }})
        );
    }

    /// #282: `relish logs` said "node wolf4 did not respond" whatever went
    /// wrong. Each failure kind now reads differently.
    #[test]
    fn each_failure_kind_produces_its_own_warning_text() {
        let cases = [
            (
                NodeFailureReason::NotInMembership,
                "no logs from node wolf4: not in the membership table",
            ),
            (
                NodeFailureReason::TimedOut { after_ms: 10_000 },
                "no logs from node wolf4: timed out after 10s",
            ),
            (
                NodeFailureReason::TimedOut { after_ms: 250 },
                "no logs from node wolf4: timed out after 250ms",
            ),
            (
                NodeFailureReason::Transport {
                    detail: "invalid peer certificate: UnknownIssuer".to_string(),
                },
                "no logs from node wolf4: request failed (connect or TLS): invalid peer certificate: UnknownIssuer",
            ),
            (
                NodeFailureReason::HttpStatus {
                    status: 403,
                    body: "forbidden".to_string(),
                },
                "no logs from node wolf4: answered HTTP 403: forbidden",
            ),
            (
                NodeFailureReason::HttpStatus {
                    status: 500,
                    body: String::new(),
                },
                "no logs from node wolf4: answered HTTP 500",
            ),
            (
                NodeFailureReason::BadBody {
                    detail: "expected value at line 1 column 1".to_string(),
                },
                "no logs from node wolf4: answered with an unreadable body: expected value at line 1 column 1",
            ),
            (
                NodeFailureReason::Internal {
                    detail: "task panicked".to_string(),
                },
                "no logs from node wolf4: query task failed: task panicked",
            ),
        ];
        for (reason, expected) in cases {
            let warning = LogQueryWarning::NodeFailed {
                node_id: "wolf4".to_string(),
                reason,
            };
            assert_eq!(warning.to_string(), expected);
        }
    }

    #[test]
    fn log_entry_json_round_trip() {
        let entry = LogEntry {
            timestamp: 42,
            sequence: 42_000_000_001,
            instance: Some("web-0".to_string()),
            node: Some("node-2".to_string()),
            stream: LogStream::Stderr,
            line: "error msg".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: LogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.timestamp, 42);
        assert_eq!(decoded.sequence, 42_000_000_001);
        assert_eq!(decoded.instance.as_deref(), Some("web-0"));
        assert_eq!(decoded.node.as_deref(), Some("node-2"));
        assert_eq!(decoded.stream, LogStream::Stderr);
        assert_eq!(decoded.line, "error msg");
    }

    #[test]
    fn empty_log_query_result() {
        let result = LogQueryResult {
            entries: vec![],
            node_count: 0,
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: LogQueryResult = serde_json::from_str(&json).unwrap();
        assert!(decoded.entries.is_empty());
        assert!(decoded.warnings.is_empty());
    }
}
