//! Metric types for Mayo.
//!
//! Defines the data model: metric names, labels, samples, and kinds.
//! These types are used for collection and insertion. Queries go
//! through DataFusion SQL and return Arrow RecordBatches.

use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// A metric name (e.g. `node_cpu_usage_percent`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricName(pub String);

impl MetricName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MetricName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A metric key: name + a set of labels.
///
/// Labels are stored in a `BTreeMap` for deterministic ordering,
/// which ensures that the same metric with the same labels always
/// produces the same key regardless of insertion order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricKey {
    pub name: MetricName,
    pub labels: BTreeMap<String, String>,
}

impl MetricKey {
    /// Create a metric key with no labels.
    pub fn simple(name: impl Into<String>) -> Self {
        Self {
            name: MetricName::new(name),
            labels: BTreeMap::new(),
        }
    }

    /// Create a metric key with labels.
    pub fn with_labels(name: impl Into<String>, labels: BTreeMap<String, String>) -> Self {
        Self {
            name: MetricName::new(name),
            labels,
        }
    }

    /// Serialise labels to a JSON string for Arrow storage.
    pub fn labels_json(&self) -> String {
        serde_json::to_string(&self.labels).unwrap_or_else(|_| "{}".to_string())
    }
}

impl fmt::Display for MetricKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if !self.labels.is_empty() {
            write!(f, "{{")?;
            for (i, (k, v)) in self.labels.iter().enumerate() {
                if i > 0 {
                    write!(f, ",")?;
                }
                write!(f, "{k}=\"{v}\"")?;
            }
            write!(f, "}}")?;
        }
        Ok(())
    }
}

/// A single metric data point.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Seconds since Unix epoch.
    pub timestamp: u64,
    /// The metric value.
    pub value: f64,
}

impl Sample {
    /// Create a sample with the current timestamp.
    pub fn now(value: f64) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self { timestamp, value }
    }

    /// Create a sample with an explicit timestamp.
    pub fn at(timestamp: u64, value: f64) -> Self {
        Self { timestamp, value }
    }
}

/// The kind of metric (affects how values are interpreted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricKind {
    /// A gauge measures an instantaneous value (e.g. temperature, memory usage).
    Gauge,
    /// A counter is monotonically increasing (e.g. request count, bytes sent).
    Counter,
}

/// Errors from Mayo operations.
#[derive(Debug, thiserror::Error)]
pub enum MayoError {
    #[error("query failed: {0}")]
    QueryFailed(String),
    #[error("arrow error: {0}")]
    Arrow(String),
    #[error("datafusion error: {0}")]
    DataFusion(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_key_simple() {
        let key = MetricKey::simple("cpu_usage");
        assert_eq!(key.name.as_str(), "cpu_usage");
        assert!(key.labels.is_empty());
    }

    #[test]
    fn metric_key_with_labels() {
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), "web".to_string());
        labels.insert("namespace".to_string(), "prod".to_string());
        let key = MetricKey::with_labels("requests_total", labels);
        assert_eq!(key.labels.len(), 2);
    }

    #[test]
    fn metric_key_label_order_independent() {
        let mut labels1 = BTreeMap::new();
        labels1.insert("b".to_string(), "2".to_string());
        labels1.insert("a".to_string(), "1".to_string());

        let mut labels2 = BTreeMap::new();
        labels2.insert("a".to_string(), "1".to_string());
        labels2.insert("b".to_string(), "2".to_string());

        let key1 = MetricKey::with_labels("test", labels1);
        let key2 = MetricKey::with_labels("test", labels2);
        assert_eq!(key1, key2);
    }

    #[test]
    fn metric_key_display_no_labels() {
        let key = MetricKey::simple("cpu");
        assert_eq!(key.to_string(), "cpu");
    }

    #[test]
    fn metric_key_display_with_labels() {
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), "web".to_string());
        let key = MetricKey::with_labels("requests", labels);
        assert_eq!(key.to_string(), "requests{app=\"web\"}");
    }

    #[test]
    fn metric_key_labels_json() {
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), "web".to_string());
        let key = MetricKey::with_labels("test", labels);
        let json = key.labels_json();
        assert!(json.contains("\"app\":\"web\""));
    }

    #[test]
    fn sample_at_explicit_timestamp() {
        let s = Sample::at(1000, 42.5);
        assert_eq!(s.timestamp, 1000);
        assert_eq!(s.value, 42.5);
    }

    #[test]
    fn sample_now_has_recent_timestamp() {
        let s = Sample::now(1.0);
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(s.timestamp <= now);
        assert!(s.timestamp >= now - 1);
    }

    #[test]
    fn metric_kind_serde_round_trip() {
        let gauge = MetricKind::Gauge;
        let json = serde_json::to_string(&gauge).unwrap();
        let decoded: MetricKind = serde_json::from_str(&json).unwrap();
        assert_eq!(gauge, decoded);
    }
}
