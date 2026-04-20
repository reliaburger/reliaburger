//! Rollup types for hierarchical metrics aggregation.
//!
//! Each node periodically generates a `NodeRollup` containing 1-minute
//! aggregation summaries. These are pushed to the assigned council
//! aggregator for cluster-wide queries.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::meat::NodeId;

/// Statistical summary of metric samples within an aggregation window.
///
/// A rollup captures min, max, sum, and count so the receiving
/// aggregator can compute any standard aggregate (avg = sum/count)
/// without needing the raw samples.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RollupAggregate {
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub count: u32,
}

impl RollupAggregate {
    /// Compute the average value, or `None` if count is zero.
    pub fn avg(&self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            Some(self.sum / self.count as f64)
        }
    }
}

/// A single metric's rollup for one aggregation window.
///
/// Preserves the original metric name and labels so the aggregator
/// can answer label-filtered queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollupEntry {
    /// Metric name (e.g. `node_cpu_usage_percent`).
    pub metric_name: String,
    /// Labels from the original metric.
    pub labels: BTreeMap<String, String>,
    /// Aggregated statistics.
    pub aggregate: RollupAggregate,
}

/// Pre-aggregated rollup pushed from a worker node to its council
/// aggregator every 60 seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRollup {
    /// Identity of the reporting node.
    pub node_id: NodeId,
    /// Start of the 1-minute aggregation window (unix seconds).
    pub timestamp: u64,
    /// Rollup entries for each distinct (metric_name, labels) pair.
    pub entries: Vec<RollupEntry>,
}

/// A cluster-wide or single-app metrics query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsQuery {
    /// Filter by metric name. `None` means all metrics.
    pub metric_name: Option<String>,
    /// Start of query range (unix seconds, inclusive).
    pub start: u64,
    /// End of query range (unix seconds, inclusive).
    pub end: u64,
    /// Filter by app label. `None` means all apps.
    pub app: Option<String>,
}

/// Warning annotation on query results.
///
/// Queries degrade gracefully: unresponsive nodes or data gaps produce
/// warnings rather than errors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QueryWarning {
    /// A node or aggregator did not respond within the query timeout.
    NodeUnresponsive { node_id: String },
    /// Rollup data is unavailable for a time window (e.g. after
    /// aggregator reassignment).
    DataUnavailable { node_id: String, from: u64, to: u64 },
}

/// Result of a metrics query, including data and any warnings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsQueryResult {
    /// Query result rows: (timestamp, metric_name, labels_json, value).
    pub data: Vec<MetricsQueryRow>,
    /// Warnings about partial results or data gaps.
    pub warnings: Vec<QueryWarning>,
}

/// A single row in a metrics query result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsQueryRow {
    pub timestamp: u64,
    pub metric_name: String,
    pub labels: String,
    pub value: f64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_rollup() -> NodeRollup {
        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), "web".to_string());

        NodeRollup {
            node_id: NodeId::new("node-1"),
            timestamp: 1000,
            entries: vec![RollupEntry {
                metric_name: "cpu_usage".to_string(),
                labels,
                aggregate: RollupAggregate {
                    min: 10.0,
                    max: 90.0,
                    sum: 300.0,
                    count: 6,
                },
            }],
        }
    }

    #[test]
    fn rollup_aggregate_avg() {
        let agg = RollupAggregate {
            min: 10.0,
            max: 50.0,
            sum: 120.0,
            count: 4,
        };
        assert_eq!(agg.avg(), Some(30.0));
    }

    #[test]
    fn rollup_aggregate_avg_zero_count() {
        let agg = RollupAggregate {
            min: 0.0,
            max: 0.0,
            sum: 0.0,
            count: 0,
        };
        assert_eq!(agg.avg(), None);
    }

    #[test]
    fn rollup_aggregate_is_copy() {
        let agg = RollupAggregate {
            min: 1.0,
            max: 2.0,
            sum: 3.0,
            count: 1,
        };
        let copy = agg;
        // Both are still usable — Copy semantics
        assert_eq!(agg, copy);
    }

    #[test]
    fn node_rollup_json_round_trip() {
        let rollup = sample_rollup();
        let json = serde_json::to_string(&rollup).unwrap();
        let decoded: NodeRollup = serde_json::from_str(&json).unwrap();
        assert_eq!(rollup, decoded);
    }

    #[test]
    fn node_rollup_bincode_round_trip() {
        let rollup = sample_rollup();
        let encoded = bincode::serialize(&rollup).unwrap();
        let decoded: NodeRollup = bincode::deserialize(&encoded).unwrap();
        assert_eq!(rollup, decoded);
    }

    #[test]
    fn metrics_query_result_empty_warnings() {
        let result = MetricsQueryResult {
            data: vec![MetricsQueryRow {
                timestamp: 1000,
                metric_name: "cpu".to_string(),
                labels: "{}".to_string(),
                value: 42.0,
            }],
            warnings: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: MetricsQueryResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, decoded);
    }

    #[test]
    fn query_warning_variants_serialize_distinctly() {
        let unresponsive = QueryWarning::NodeUnresponsive {
            node_id: "n1".to_string(),
        };
        let unavailable = QueryWarning::DataUnavailable {
            node_id: "n2".to_string(),
            from: 100,
            to: 200,
        };
        let json1 = serde_json::to_string(&unresponsive).unwrap();
        let json2 = serde_json::to_string(&unavailable).unwrap();
        assert_ne!(json1, json2);
    }

    #[test]
    fn metrics_query_serde_round_trip() {
        let query = MetricsQuery {
            metric_name: Some("cpu_usage".to_string()),
            start: 1000,
            end: 2000,
            app: Some("web".to_string()),
        };
        let json = serde_json::to_string(&query).unwrap();
        let decoded: MetricsQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(query, decoded);
    }

    #[test]
    fn metrics_query_none_fields() {
        let query = MetricsQuery {
            metric_name: None,
            start: 0,
            end: 9999,
            app: None,
        };
        let json = serde_json::to_string(&query).unwrap();
        let decoded: MetricsQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(query, decoded);
    }

    #[test]
    fn rollup_entry_preserves_label_ordering() {
        let mut labels = BTreeMap::new();
        labels.insert("z".to_string(), "last".to_string());
        labels.insert("a".to_string(), "first".to_string());

        let entry = RollupEntry {
            metric_name: "test".to_string(),
            labels: labels.clone(),
            aggregate: RollupAggregate {
                min: 1.0,
                max: 1.0,
                sum: 1.0,
                count: 1,
            },
        };

        let json = serde_json::to_string(&entry).unwrap();
        let decoded: RollupEntry = serde_json::from_str(&json).unwrap();
        // BTreeMap ordering is preserved
        let keys: Vec<&String> = decoded.labels.keys().collect();
        assert_eq!(keys, vec!["a", "z"]);
    }
}
