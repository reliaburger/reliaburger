//! Turning one app's raw metric rows into per-instance series.
//!
//! Both `relish metrics` and the dashboard's charts start from the rows the
//! per-app endpoint returns, `(timestamp, name, labels, value)`, and need
//! the same few steps: group by instance, add up the series an instance
//! reports under one name (one per status code, say), turn counters into
//! per-second rates, and divide one rate by another for a mean latency.
//! Keeping that arithmetic here, away from HTTP and rendering, keeps it
//! testable.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::rollup::MetricsQueryRow;

/// A sample: unix seconds and a value.
pub type Point = (u64, f64);

/// Label that names an instance in scraped and process metrics.
pub const INSTANCE_LABEL: &str = "instance";

/// Label naming the node that scraped or collected a sample.
pub const NODE_LABEL: &str = "node";

/// One instance's view of one metric.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceSeries {
    /// Instance id, or `-` for samples with no instance label.
    pub instance: String,
    /// Node that reported it, when the samples say.
    pub node: Option<String>,
    /// How many distinct label sets were added together.
    pub series_count: usize,
    /// Values summed across those label sets, oldest first.
    pub points: Vec<Point>,
}

/// Whether `name` holds a counter, judged by Prometheus naming.
///
/// `_total` and `_bucket` are always counters. `_sum` and `_count` are when
/// their sibling exists too, as a histogram or summary exposes both; alone
/// they could be anything (`queue_count` is usually a gauge).
pub fn is_counter(name: &str, all_names: &BTreeSet<&str>) -> bool {
    if name.ends_with("_total") || name.ends_with("_bucket") {
        return true;
    }
    if let Some(base) = name.strip_suffix("_sum") {
        return all_names.contains(format!("{base}_count").as_str());
    }
    if let Some(base) = name.strip_suffix("_count") {
        return all_names.contains(format!("{base}_sum").as_str());
    }
    false
}

/// Group rows by instance and node, adding up samples that share a
/// timestamp.
///
/// Every sample from one scrape of one instance carries the same timestamp,
/// so the sum at a timestamp is that instance's total at that moment. The
/// node is part of the key because instance ids are only unique per node:
/// three frontend replicas on three nodes are each `default__frontend-0`.
/// Instances come back sorted by id, then node.
pub fn per_instance(rows: &[MetricsQueryRow]) -> Vec<InstanceSeries> {
    struct Accumulator {
        label_sets: BTreeSet<String>,
        sums: BTreeMap<u64, f64>,
    }
    let mut by_instance: BTreeMap<(String, Option<String>), Accumulator> = BTreeMap::new();
    for row in rows {
        let labels: BTreeMap<String, String> =
            serde_json::from_str(&row.labels).unwrap_or_default();
        let instance = labels
            .get(INSTANCE_LABEL)
            .cloned()
            .unwrap_or_else(|| "-".to_string());
        let node = labels.get(NODE_LABEL).cloned();
        let entry = by_instance
            .entry((instance, node))
            .or_insert_with(|| Accumulator {
                label_sets: BTreeSet::new(),
                sums: BTreeMap::new(),
            });
        entry.label_sets.insert(row.labels.clone());
        *entry.sums.entry(row.timestamp).or_insert(0.0) += row.value;
    }
    by_instance
        .into_iter()
        .map(|((instance, node), accumulator)| InstanceSeries {
            instance,
            node,
            series_count: accumulator.label_sets.len(),
            points: accumulator.sums.into_iter().collect(),
        })
        .collect()
}

impl InstanceSeries {
    /// What identifies this instance across the cluster: its id and node.
    pub fn key(&self) -> (String, Option<String>) {
        (self.instance.clone(), self.node.clone())
    }

    /// A chart legend entry: the instance id, and the node when known.
    pub fn label(&self) -> String {
        match &self.node {
            Some(node) => format!("{} on {node}", self.instance),
            None => self.instance.clone(),
        }
    }
}

/// Per-second rate between consecutive points of a counter.
///
/// A value lower than the one before is a restart (counters only reset,
/// they never go down), so the rate for that step counts from zero, as
/// Prometheus's `rate()` does. Steps with no elapsed time are skipped.
pub fn rates(points: &[Point]) -> Vec<Point> {
    points
        .windows(2)
        .filter_map(|pair| {
            let [(before_at, before), (at, value)] = [pair[0], pair[1]];
            let elapsed = at.checked_sub(before_at).filter(|elapsed| *elapsed > 0)?;
            let increase = if value >= before {
                value - before
            } else {
                value
            };
            Some((at, increase / elapsed as f64))
        })
        .collect()
}

/// `numerator / denominator` at every timestamp both have, skipping zero
/// denominators. Mean latency is `ratio(rates(sum), rates(count))`.
pub fn ratio(numerator: &[Point], denominator: &[Point]) -> Vec<Point> {
    let denominators: BTreeMap<u64, f64> = denominator.iter().copied().collect();
    numerator
        .iter()
        .filter_map(|(at, value)| {
            let divisor = *denominators.get(at)?;
            (divisor != 0.0).then(|| (*at, value / divisor))
        })
        .collect()
}

/// Series lined up on one shared time axis, the shape a chart library
/// draws: `values[i]` belongs to `timestamps[i]`, `None` where a series
/// has no sample at that time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChartData {
    /// Every timestamp any series has, ascending.
    pub timestamps: Vec<u64>,
    /// One entry per line on the chart.
    pub series: Vec<ChartSeries>,
}

/// One line on a chart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartSeries {
    /// Legend label, usually the instance id.
    pub label: String,
    /// Values aligned with [`ChartData::timestamps`].
    pub values: Vec<Option<f64>>,
}

/// Line labelled series up on the union of their timestamps.
pub fn align(series: Vec<(String, Vec<Point>)>) -> ChartData {
    let timestamps: Vec<u64> = series
        .iter()
        .flat_map(|(_, points)| points.iter().map(|(at, _)| *at))
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();
    let series = series
        .into_iter()
        .map(|(label, points)| {
            let by_time: BTreeMap<u64, f64> = points.into_iter().collect();
            ChartSeries {
                label,
                values: timestamps
                    .iter()
                    .map(|at| by_time.get(at).copied())
                    .collect(),
            }
        })
        .collect();
    ChartData { timestamps, series }
}

/// How a chart turns one app's rows into lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChartKind {
    /// Values as they are (CPU, memory, queue depth).
    Gauge,
    /// A counter's per-second rate.
    Rate,
    /// A histogram's mean observation, from its `_sum` and `_count`.
    Mean,
}

/// One line per instance for a gauge or a counter's rate.
///
/// A [`ChartKind::Mean`] chart needs two metrics; see [`mean_chart`]. Given
/// one here, it draws the values unchanged.
pub fn instance_chart(kind: ChartKind, rows: &[MetricsQueryRow]) -> ChartData {
    align(
        per_instance(rows)
            .into_iter()
            .map(|instance| {
                let label = instance.label();
                let points = match kind {
                    ChartKind::Rate => rates(&instance.points),
                    ChartKind::Gauge | ChartKind::Mean => instance.points,
                };
                (label, points)
            })
            .collect(),
    )
}

/// One line per instance of a histogram's mean observation:
/// `rate(_sum) / rate(_count)` over each scrape interval.
pub fn mean_chart(sum_rows: &[MetricsQueryRow], count_rows: &[MetricsQueryRow]) -> ChartData {
    let sums: BTreeMap<(String, Option<String>), Vec<Point>> = per_instance(sum_rows)
        .into_iter()
        .map(|instance| (instance.key(), rates(&instance.points)))
        .collect();
    align(
        per_instance(count_rows)
            .into_iter()
            .map(|count| {
                let means = sums
                    .get(&count.key())
                    .map(|sum_rates| ratio(sum_rates, &rates(&count.points)))
                    .unwrap_or_default();
                (count.label(), means)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_chart_draws_one_rate_line_per_instance() {
        let rows = vec![
            row(10, "req_total", r#"{"instance":"a"}"#, 0.0),
            row(20, "req_total", r#"{"instance":"a"}"#, 100.0),
            row(12, "req_total", r#"{"instance":"b"}"#, 0.0),
            row(22, "req_total", r#"{"instance":"b"}"#, 20.0),
        ];
        let chart = instance_chart(ChartKind::Rate, &rows);
        assert_eq!(chart.timestamps, vec![20, 22]);
        assert_eq!(chart.series[0].label, "a");
        assert_eq!(chart.series[0].values, vec![Some(10.0), None]);
        assert_eq!(chart.series[1].values, vec![None, Some(2.0)]);
    }

    #[test]
    fn a_mean_chart_divides_sum_rate_by_count_rate() {
        let sum = vec![
            row(10, "lat_sum", r#"{"instance":"a"}"#, 0.0),
            row(20, "lat_sum", r#"{"instance":"a"}"#, 2.0),
        ];
        let count = vec![
            row(10, "lat_count", r#"{"instance":"a"}"#, 0.0),
            row(20, "lat_count", r#"{"instance":"a"}"#, 40.0),
        ];
        let chart = mean_chart(&sum, &count);
        assert_eq!(chart.timestamps, vec![20]);
        assert_eq!(chart.series[0].values, vec![Some(0.05)]);
    }

    fn row(timestamp: u64, name: &str, labels: &str, value: f64) -> MetricsQueryRow {
        MetricsQueryRow {
            timestamp,
            metric_name: name.to_string(),
            labels: labels.to_string(),
            value,
        }
    }

    #[test]
    fn counters_are_recognised_by_name_and_by_sibling() {
        let names: BTreeSet<&str> = [
            "http_requests_total",
            "latency_seconds_bucket",
            "latency_seconds_sum",
            "latency_seconds_count",
            "queue_count",
            "go_goroutines",
        ]
        .into_iter()
        .collect();
        assert!(is_counter("http_requests_total", &names));
        assert!(is_counter("latency_seconds_bucket", &names));
        assert!(is_counter("latency_seconds_sum", &names));
        assert!(is_counter("latency_seconds_count", &names));
        assert!(!is_counter("queue_count", &names));
        assert!(!is_counter("go_goroutines", &names));
    }

    #[test]
    fn per_instance_sums_label_sets_at_each_timestamp() {
        let rows = vec![
            row(
                10,
                "req",
                r#"{"instance":"a","node":"n1","status":"200"}"#,
                5.0,
            ),
            row(
                10,
                "req",
                r#"{"instance":"a","node":"n1","status":"500"}"#,
                1.0,
            ),
            row(
                20,
                "req",
                r#"{"instance":"a","node":"n1","status":"200"}"#,
                8.0,
            ),
            row(
                20,
                "req",
                r#"{"instance":"a","node":"n1","status":"500"}"#,
                2.0,
            ),
            row(
                12,
                "req",
                r#"{"instance":"b","node":"n2","status":"200"}"#,
                3.0,
            ),
        ];
        let series = per_instance(&rows);
        assert_eq!(
            series,
            vec![
                InstanceSeries {
                    instance: "a".to_string(),
                    node: Some("n1".to_string()),
                    series_count: 2,
                    points: vec![(10, 6.0), (20, 10.0)],
                },
                InstanceSeries {
                    instance: "b".to_string(),
                    node: Some("n2".to_string()),
                    series_count: 1,
                    points: vec![(12, 3.0)],
                },
            ]
        );
    }

    /// Z6.7: each node numbers its own replicas, so three frontends on three
    /// nodes are all `default__frontend-0`. Grouping by id alone merged them
    /// into one instance whose points jumped between replicas.
    #[test]
    fn replicas_with_the_same_id_on_different_nodes_stay_apart() {
        let rows = vec![
            row(10, "req_total", r#"{"instance":"f-0","node":"n1"}"#, 100.0),
            row(20, "req_total", r#"{"instance":"f-0","node":"n1"}"#, 150.0),
            row(11, "req_total", r#"{"instance":"f-0","node":"n2"}"#, 7.0),
            row(21, "req_total", r#"{"instance":"f-0","node":"n2"}"#, 17.0),
        ];
        let series = per_instance(&rows);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].node.as_deref(), Some("n1"));
        assert_eq!(series[0].points, vec![(10, 100.0), (20, 150.0)]);
        assert_eq!(series[1].node.as_deref(), Some("n2"));
        assert_eq!(series[1].points, vec![(11, 7.0), (21, 17.0)]);

        let chart = instance_chart(ChartKind::Rate, &rows);
        let labels: Vec<&str> = chart.series.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["f-0 on n1", "f-0 on n2"]);
        assert_eq!(chart.series[0].values, vec![Some(5.0), None]);
        assert_eq!(chart.series[1].values, vec![None, Some(1.0)]);
    }

    #[test]
    fn a_mean_chart_pairs_sum_and_count_by_instance_and_node() {
        let sum = vec![
            row(10, "lat_sum", r#"{"instance":"f-0","node":"n1"}"#, 0.0),
            row(20, "lat_sum", r#"{"instance":"f-0","node":"n1"}"#, 1.0),
            row(10, "lat_sum", r#"{"instance":"f-0","node":"n2"}"#, 0.0),
            row(20, "lat_sum", r#"{"instance":"f-0","node":"n2"}"#, 30.0),
        ];
        let count = vec![
            row(10, "lat_count", r#"{"instance":"f-0","node":"n1"}"#, 0.0),
            row(20, "lat_count", r#"{"instance":"f-0","node":"n1"}"#, 100.0),
            row(10, "lat_count", r#"{"instance":"f-0","node":"n2"}"#, 0.0),
            row(20, "lat_count", r#"{"instance":"f-0","node":"n2"}"#, 100.0),
        ];
        let chart = mean_chart(&sum, &count);
        assert_eq!(chart.series[0].values, vec![Some(0.01)]);
        assert_eq!(chart.series[1].values, vec![Some(0.3)]);
    }

    #[test]
    fn samples_without_an_instance_group_under_a_dash() {
        let series = per_instance(&[row(1, "m", r#"{"app":"x"}"#, 1.0)]);
        assert_eq!(series[0].instance, "-");
        assert_eq!(series[0].node, None);
    }

    #[test]
    fn rates_divide_increase_by_elapsed_seconds() {
        assert_eq!(
            rates(&[(0, 0.0), (10, 50.0), (20, 150.0)]),
            vec![(10, 5.0), (20, 10.0)]
        );
    }

    #[test]
    fn a_counter_reset_counts_from_zero() {
        // 100 → 30 means the process restarted and has served 30 since.
        assert_eq!(rates(&[(0, 100.0), (10, 30.0)]), vec![(10, 3.0)]);
    }

    #[test]
    fn rates_skip_repeated_timestamps_and_need_two_points() {
        assert_eq!(rates(&[(5, 1.0), (5, 2.0)]), vec![]);
        assert_eq!(rates(&[(5, 1.0)]), vec![]);
        assert_eq!(rates(&[]), vec![]);
    }

    #[test]
    fn ratio_is_mean_latency_from_sum_and_count_rates() {
        let sum_rate = vec![(10, 0.5), (20, 0.0), (30, 1.0)];
        let count_rate = vec![(10, 10.0), (20, 0.0), (30, 4.0)];
        assert_eq!(ratio(&sum_rate, &count_rate), vec![(10, 0.05), (30, 0.25)]);
    }

    #[test]
    fn align_fills_missing_points_with_none() {
        let data = align(vec![
            ("a".to_string(), vec![(10, 1.0), (20, 2.0)]),
            ("b".to_string(), vec![(15, 5.0)]),
        ]);
        assert_eq!(data.timestamps, vec![10, 15, 20]);
        assert_eq!(data.series[0].values, vec![Some(1.0), None, Some(2.0)]);
        assert_eq!(data.series[1].values, vec![None, Some(5.0), None]);
    }
}
