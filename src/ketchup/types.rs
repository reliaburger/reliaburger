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

/// Errors from Ketchup operations.
#[derive(Debug, thiserror::Error)]
pub enum KetchupError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("log not found for {app} in {namespace}")]
    NotFound { app: String, namespace: String },
}
