//! Types for Ketchup log collection.

use serde::{Deserialize, Serialize};

/// Which output stream a log line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// A single log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Seconds since Unix epoch.
    pub timestamp: u64,
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
    /// Grep pattern (substring match).
    pub grep: Option<String>,
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
    /// A node did not respond within the query timeout.
    NodeUnresponsive { node_id: String },
}

/// Errors from Ketchup operations.
#[derive(Debug, thiserror::Error)]
pub enum KetchupError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("log not found for {app} in {namespace}")]
    NotFound { app: String, namespace: String },
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
                stream: LogStream::Stdout,
                line: "hello".to_string(),
            }],
            node_count: 3,
            warnings: vec![LogQueryWarning::NodeUnresponsive {
                node_id: "node-2".to_string(),
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: LogQueryResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.entries.len(), 1);
        assert_eq!(decoded.node_count, 3);
        assert_eq!(decoded.warnings.len(), 1);
    }

    #[test]
    fn log_query_warning_variants_serialize_distinctly() {
        let w = LogQueryWarning::NodeUnresponsive {
            node_id: "n1".to_string(),
        };
        let json = serde_json::to_string(&w).unwrap();
        assert!(json.contains("NodeUnresponsive"));
        assert!(json.contains("n1"));
    }

    #[test]
    fn log_entry_json_round_trip() {
        let entry = LogEntry {
            timestamp: 42,
            stream: LogStream::Stderr,
            line: "error msg".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let decoded: LogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.timestamp, 42);
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
