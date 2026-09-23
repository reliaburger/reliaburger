//! `relish metrics`: an app's own Prometheus metrics, scraped by the nodes
//! running it, without a Prometheus install.
//!
//! Without `--name` it lists every metric the app exposes with one number
//! each: the latest value summed across instances for a gauge, the
//! per-second rate for a counter, the mean observation for a histogram.
//! With `--name` it shows one metric per instance, with a sparkline.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Serialize;

use super::client::BunClient;
use super::output::sparkline;
use super::{OutputFormat, RelishError};
use crate::mayo::rollup::MetricsQueryRow;
use crate::mayo::series::{self, InstanceSeries, Point};

/// Samples fetched per series for the overview: two, so a counter has a rate.
const OVERVIEW_SAMPLES_PER_SERIES: u32 = 2;

/// Most points one sparkline shows (the newest ones).
const SPARKLINE_WIDTH: usize = 30;

/// How a metric's values are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricType {
    /// A value that goes up and down; shown as the latest value.
    Gauge,
    /// A value that only grows; shown as a per-second rate.
    Counter,
    /// `_bucket`, `_sum` and `_count` series; shown as the mean observation.
    Histogram,
}

impl MetricType {
    fn as_str(self) -> &'static str {
        match self {
            MetricType::Gauge => "gauge",
            MetricType::Counter => "counter",
            MetricType::Histogram => "histogram",
        }
    }
}

/// One line of the overview.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricSummary {
    /// Metric name; a histogram's base name without `_bucket`/`_sum`/`_count`.
    pub name: String,
    /// How [`MetricSummary::value`] was computed.
    pub kind: MetricType,
    /// Distinct label sets across all instances.
    pub series: usize,
    /// Instances reporting it.
    pub instances: usize,
    /// Gauge: latest value summed across instances. Counter: per-second
    /// rate summed across instances. Histogram: mean observation over the
    /// last scrape interval. `None` until there are enough samples.
    pub value: Option<f64>,
}

/// One instance's line in the `--name` view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InstanceMetric {
    /// Instance id.
    pub instance: String,
    /// Node that scraped it.
    pub node: Option<String>,
    /// Label sets added together for this instance.
    pub series: usize,
    /// Latest value (gauge, counter) or mean observation (histogram).
    pub latest: Option<f64>,
    /// Per-second rate (counter) or observations per second (histogram).
    pub rate: Option<f64>,
    /// What the sparkline draws: values for a gauge, rates for a counter,
    /// means for a histogram. Oldest first.
    pub trend: Vec<Point>,
}

/// `relish metrics <app>`: fetch through the entry node and print.
pub async fn metrics(
    app: &str,
    namespace: &str,
    name: Option<&str>,
    since: &str,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let window = super::fault::parse_duration(since)?;
    let now = crate::mayo::types::Sample::now(0.0).timestamp;
    let start = now.saturating_sub(window.as_secs());
    let client = BunClient::default_local();

    let Some(name) = name else {
        let result = client
            .app_metrics_since(
                app,
                namespace,
                None,
                start,
                Some(OVERVIEW_SAMPLES_PER_SERIES),
            )
            .await?;
        print_warnings(&result.warnings);
        let summaries = summarise(&result.data);
        return match output {
            OutputFormat::Human => {
                print!("{}", render_overview(namespace, app, since, &summaries));
                Ok(())
            }
            _ => print_structured(&summaries, output),
        };
    };

    let result = client
        .app_metrics_since(app, namespace, Some(name), start, None)
        .await?;
    print_warnings(&result.warnings);
    let (kind, instances) = if result.data.is_empty() {
        // `--name http_request_duration_seconds` names a histogram by its
        // base; its samples live under `_sum` and `_count`.
        let sum = client
            .app_metrics_since(app, namespace, Some(&format!("{name}_sum")), start, None)
            .await?;
        let count = client
            .app_metrics_since(app, namespace, Some(&format!("{name}_count")), start, None)
            .await?;
        if sum.data.is_empty() || count.data.is_empty() {
            (counter_or_gauge(name), Vec::new())
        } else {
            (
                MetricType::Histogram,
                histogram_instances(&sum.data, &count.data),
            )
        }
    } else {
        let kind = counter_or_gauge(name);
        (kind, instance_metrics(kind, &result.data))
    };
    match output {
        OutputFormat::Human => {
            print!(
                "{}",
                render_detail(namespace, app, name, since, kind, &instances)
            );
            Ok(())
        }
        _ => print_structured(&instances, output),
    }
}

fn print_warnings(warnings: &[crate::mayo::rollup::QueryWarning]) {
    for warning in warnings {
        eprintln!("warning: {warning:?}");
    }
}

fn print_structured<T: Serialize>(value: &T, output: OutputFormat) -> Result<(), RelishError> {
    match output {
        OutputFormat::Yaml => {
            print!(
                "{}",
                serde_yaml::to_string(value).map_err(RelishError::SerialiseYaml)?
            );
        }
        _ => println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(RelishError::SerialiseJson)?
        ),
    }
    Ok(())
}

/// A single metric's type from its name alone, for `--name`: a counter by
/// Prometheus naming (`_total`, or a histogram's `_bucket`/`_sum`/`_count`).
fn counter_or_gauge(name: &str) -> MetricType {
    let counter = ["_total", "_bucket", "_sum", "_count"]
        .iter()
        .any(|suffix| name.ends_with(suffix));
    if counter {
        MetricType::Counter
    } else {
        MetricType::Gauge
    }
}

/// The newest per-second rate of a counter's points.
fn last_rate(points: &[Point]) -> Option<f64> {
    series::rates(points).last().map(|(_, rate)| *rate)
}

fn last_value(points: &[Point]) -> Option<f64> {
    points.last().map(|(_, value)| *value)
}

/// Add up optional per-instance numbers; `None` if no instance has one.
fn sum_present(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values
        .flatten()
        .fold(None, |total, value| Some(total.unwrap_or(0.0) + value))
}

/// Reduce an app's rows to one summary per metric, sorted by name.
pub fn summarise(rows: &[MetricsQueryRow]) -> Vec<MetricSummary> {
    let mut by_name: BTreeMap<&str, Vec<MetricsQueryRow>> = BTreeMap::new();
    for row in rows {
        by_name
            .entry(row.metric_name.as_str())
            .or_default()
            .push(row.clone());
    }
    let names: BTreeSet<&str> = by_name.keys().copied().collect();
    let histograms: BTreeSet<&str> = names
        .iter()
        .filter_map(|name| name.strip_suffix("_bucket"))
        .filter(|base| {
            names.contains(format!("{base}_sum").as_str())
                && names.contains(format!("{base}_count").as_str())
        })
        .collect();
    let part_of_histogram = |name: &str| {
        ["_bucket", "_sum", "_count"].iter().any(|suffix| {
            name.strip_suffix(suffix)
                .is_some_and(|base| histograms.contains(base))
        })
    };

    let mut summaries = Vec::new();
    for base in &histograms {
        let sum = series::per_instance(&by_name[format!("{base}_sum").as_str()]);
        let count = series::per_instance(&by_name[format!("{base}_count").as_str()]);
        let sum_rate = sum_present(sum.iter().map(|s| last_rate(&s.points)));
        let count_rate = sum_present(count.iter().map(|s| last_rate(&s.points)));
        let mean = match (sum_rate, count_rate) {
            (Some(sum), Some(count)) if count > 0.0 => Some(sum / count),
            _ => None,
        };
        summaries.push(MetricSummary {
            name: base.to_string(),
            kind: MetricType::Histogram,
            series: count.iter().map(|s| s.series_count).sum(),
            instances: count.len(),
            value: mean,
        });
    }
    for (name, rows) in &by_name {
        if part_of_histogram(name) {
            continue;
        }
        let instances = series::per_instance(rows);
        let kind = if series::is_counter(name, &names) {
            MetricType::Counter
        } else {
            MetricType::Gauge
        };
        let value = match kind {
            MetricType::Counter => sum_present(instances.iter().map(|s| last_rate(&s.points))),
            _ => sum_present(instances.iter().map(|s| last_value(&s.points))),
        };
        summaries.push(MetricSummary {
            name: name.to_string(),
            kind,
            series: instances.iter().map(|s| s.series_count).sum(),
            instances: instances.len(),
            value,
        });
    }
    summaries.sort_by(|left, right| left.name.cmp(&right.name));
    summaries
}

/// Per-instance lines for a gauge or counter.
pub fn instance_metrics(kind: MetricType, rows: &[MetricsQueryRow]) -> Vec<InstanceMetric> {
    series::per_instance(rows)
        .into_iter()
        .map(|instance| {
            let (rate, trend) = match kind {
                MetricType::Counter => {
                    let rates = series::rates(&instance.points);
                    (rates.last().map(|(_, rate)| *rate), rates)
                }
                _ => (None, instance.points.clone()),
            };
            InstanceMetric {
                latest: last_value(&instance.points),
                instance: instance.instance,
                node: instance.node,
                series: instance.series_count,
                rate,
                trend,
            }
        })
        .collect()
}

/// Per-instance lines for a histogram: mean observation and observations
/// per second, from its `_sum` and `_count` rows.
pub fn histogram_instances(
    sum_rows: &[MetricsQueryRow],
    count_rows: &[MetricsQueryRow],
) -> Vec<InstanceMetric> {
    let sums: BTreeMap<String, InstanceSeries> = series::per_instance(sum_rows)
        .into_iter()
        .map(|s| (s.instance.clone(), s))
        .collect();
    series::per_instance(count_rows)
        .into_iter()
        .map(|count| {
            let count_rates = series::rates(&count.points);
            let means = sums
                .get(&count.instance)
                .map(|sum| series::ratio(&series::rates(&sum.points), &count_rates))
                .unwrap_or_default();
            InstanceMetric {
                latest: last_value(&means),
                rate: count_rates.last().map(|(_, rate)| *rate),
                trend: means,
                instance: count.instance,
                node: count.node,
                series: count.series_count,
            }
        })
        .collect()
}

/// A number short enough for a table column.
fn format_number(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude >= 1e9 {
        format!("{:.1}G", value / 1e9)
    } else if magnitude >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if magnitude >= 1e4 {
        format!("{:.1}k", value / 1e3)
    } else if value.fract() == 0.0 {
        format!("{value:.0}")
    } else if magnitude >= 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

/// A per-second rate: two decimals while small, so 7/s and 7.5/s line up,
/// then the same short form as other numbers.
fn format_rate(rate: f64) -> String {
    if rate.abs() < 100.0 {
        format!("{rate:.2}")
    } else {
        format_number(rate)
    }
}

/// A duration in seconds as µs, ms or s.
fn format_seconds(seconds: f64) -> String {
    if seconds >= 1.0 {
        format!("{seconds:.2}s")
    } else if seconds >= 0.001 {
        format!("{:.1}ms", seconds * 1e3)
    } else {
        format!("{:.0}µs", seconds * 1e6)
    }
}

/// An observation in the histogram's unit: seconds by the naming
/// convention (`_seconds`), anything else as a plain number.
fn format_observation(name: &str, value: f64) -> String {
    if name.ends_with("_seconds") {
        format_seconds(value)
    } else {
        format_number(value)
    }
}

fn format_summary_value(summary: &MetricSummary) -> String {
    let Some(value) = summary.value else {
        return "-".to_string();
    };
    match summary.kind {
        MetricType::Gauge => format_number(value),
        MetricType::Counter => format!("{}/s", format_rate(value)),
        MetricType::Histogram => format!("mean {}", format_observation(&summary.name, value)),
    }
}

/// The overview table.
pub fn render_overview(
    namespace: &str,
    app: &str,
    since: &str,
    summaries: &[MetricSummary],
) -> String {
    if summaries.is_empty() {
        return format!(
            "no metrics for {namespace}/{app} in the last {since}; does the app declare \
             `metrics = {{}}` (or prometheus.io/scrape)?\n"
        );
    }
    let width = summaries
        .iter()
        .map(|summary| summary.name.len())
        .max()
        .unwrap_or(0)
        .max("METRIC".len());
    let mut output = format!(
        "{:<width$}  {:<9}  {:>6}  {:>9}  VALUE\n",
        "METRIC", "TYPE", "SERIES", "INSTANCES"
    );
    for summary in summaries {
        let _ = writeln!(
            output,
            "{:<width$}  {:<9}  {:>6}  {:>9}  {}",
            summary.name,
            summary.kind.as_str(),
            summary.series,
            summary.instances,
            format_summary_value(summary)
        );
    }
    output
}

/// The `--name` table: one line per instance plus a total.
pub fn render_detail(
    namespace: &str,
    app: &str,
    name: &str,
    since: &str,
    kind: MetricType,
    instances: &[InstanceMetric],
) -> String {
    if instances.is_empty() {
        return format!("no samples of {name} for {namespace}/{app} in the last {since}\n");
    }
    let (latest_header, rate_header) = match kind {
        MetricType::Histogram => ("MEAN", "OBS/S"),
        _ => ("LATEST", "RATE/S"),
    };
    let latest = |value: Option<f64>| match (value, kind) {
        (None, _) => "-".to_string(),
        (Some(value), MetricType::Histogram) => format_observation(name, value),
        (Some(value), _) => format_number(value),
    };
    let rate = |value: Option<f64>| value.map_or_else(|| "-".to_string(), format_rate);
    let instance_width = instances
        .iter()
        .map(|i| i.instance.len())
        .max()
        .unwrap_or(0)
        .max("INSTANCE".len());
    let node_width = instances
        .iter()
        .map(|i| i.node.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(0)
        .max("NODE".len());

    let mut output = format!(
        "{namespace}/{app} {name} ({}, last {since})\n",
        kind.as_str()
    );
    let _ = writeln!(
        output,
        "{:<instance_width$}  {:<node_width$}  {:>6}  {:>9}  {:>8}  TREND",
        "INSTANCE", "NODE", "SERIES", latest_header, rate_header
    );
    for instance in instances {
        let values: Vec<f64> = instance
            .trend
            .iter()
            .rev()
            .take(SPARKLINE_WIDTH)
            .rev()
            .map(|(_, value)| *value)
            .collect();
        let _ = writeln!(
            output,
            "{:<instance_width$}  {:<node_width$}  {:>6}  {:>9}  {:>8}  {}",
            instance.instance,
            instance.node.as_deref().unwrap_or("-"),
            instance.series,
            latest(instance.latest),
            rate(instance.rate),
            sparkline(&values)
        );
    }
    if instances.len() > 1 && kind != MetricType::Histogram {
        let _ = writeln!(
            output,
            "{:<instance_width$}  {:<node_width$}  {:>6}  {:>9}  {:>8}",
            "total",
            "",
            instances.iter().map(|i| i.series).sum::<usize>(),
            latest(sum_present(instances.iter().map(|i| i.latest))),
            rate(sum_present(instances.iter().map(|i| i.rate))),
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(timestamp: u64, name: &str, instance: &str, extra: &str, value: f64) -> MetricsQueryRow {
        let node = if instance.ends_with('0') {
            "node-a"
        } else {
            "node-b"
        };
        MetricsQueryRow {
            timestamp,
            metric_name: name.to_string(),
            labels: format!(
                r#"{{"app":"default/web","instance":"{instance}","node":"{node}"{extra}}}"#
            ),
            value,
        }
    }

    /// Two scrapes, ten seconds apart, of two instances.
    fn fixture() -> Vec<MetricsQueryRow> {
        let mut rows = Vec::new();
        for (instance, offset) in [("default__web-0", 0.0), ("default__web-1", 100.0)] {
            for (at, step) in [(100, 0.0), (110, 1.0)] {
                rows.push(row(
                    at,
                    "http_requests_total",
                    instance,
                    r#","status":"200""#,
                    offset + 40.0 * step,
                ));
                rows.push(row(
                    at,
                    "http_requests_total",
                    instance,
                    r#","status":"500""#,
                    2.0 * step,
                ));
                rows.push(row(
                    at,
                    "http_request_duration_seconds_bucket",
                    instance,
                    r#","le":"0.1""#,
                    offset + 40.0 * step,
                ));
                rows.push(row(
                    at,
                    "http_request_duration_seconds_sum",
                    instance,
                    "",
                    0.5 * step,
                ));
                rows.push(row(
                    at,
                    "http_request_duration_seconds_count",
                    instance,
                    "",
                    offset + 42.0 * step,
                ));
                rows.push(row(at, "up", instance, "", 1.0));
                rows.push(row(
                    at,
                    "process_memory_bytes",
                    instance,
                    "",
                    52_428_800.0 + step,
                ));
            }
        }
        rows
    }

    #[test]
    fn the_overview_lists_each_metric_once_with_one_number() {
        let summaries = summarise(&fixture());
        insta::assert_snapshot!(render_overview("default", "web", "15m", &summaries));
    }

    #[test]
    fn counters_are_summed_rates_and_histograms_are_means() {
        let summaries = summarise(&fixture());
        let by_name = |name: &str| summaries.iter().find(|s| s.name == name).unwrap();
        let requests = by_name("http_requests_total");
        assert_eq!(requests.kind, MetricType::Counter);
        // Each instance grew by 42 in 10 s.
        assert_eq!(requests.value, Some(8.4));
        assert_eq!(requests.series, 4);
        assert_eq!(requests.instances, 2);
        let latency = by_name("http_request_duration_seconds");
        assert_eq!(latency.kind, MetricType::Histogram);
        // 1 s of latency over 84 requests.
        assert!((latency.value.unwrap() - 1.0 / 84.0).abs() < 1e-9);
        assert_eq!(by_name("up").value, Some(2.0));
        assert!(
            !summaries.iter().any(|s| s.name.ends_with("_bucket")),
            "histogram parts are folded into one line"
        );
    }

    #[test]
    fn the_detail_view_shows_each_instance_with_a_rate_and_sparkline() {
        let rows: Vec<MetricsQueryRow> = (0..6u64)
            .flat_map(|step| {
                [
                    row(
                        100 + step * 10,
                        "http_requests_total",
                        "default__web-0",
                        "",
                        (step * step * 10) as f64,
                    ),
                    row(
                        105 + step * 10,
                        "http_requests_total",
                        "default__web-1",
                        "",
                        (step * 20) as f64,
                    ),
                ]
            })
            .collect();
        let instances = instance_metrics(MetricType::Counter, &rows);
        insta::assert_snapshot!(render_detail(
            "default",
            "web",
            "http_requests_total",
            "15m",
            MetricType::Counter,
            &instances
        ));
    }

    #[test]
    fn a_histogram_by_its_base_name_shows_mean_latency_per_instance() {
        let rows = fixture();
        let pick = |name: &str| -> Vec<MetricsQueryRow> {
            rows.iter()
                .filter(|row| row.metric_name == name)
                .cloned()
                .collect()
        };
        let instances = histogram_instances(
            &pick("http_request_duration_seconds_sum"),
            &pick("http_request_duration_seconds_count"),
        );
        insta::assert_snapshot!(render_detail(
            "default",
            "web",
            "http_request_duration_seconds",
            "15m",
            MetricType::Histogram,
            &instances
        ));
    }

    #[test]
    fn nothing_scraped_says_what_to_check() {
        let output = render_overview("default", "web", "15m", &[]);
        assert!(output.contains("metrics = {}"), "{output}");
    }

    #[test]
    fn numbers_and_durations_fit_a_column() {
        assert_eq!(format_number(3.0), "3");
        assert_eq!(format_number(8.4), "8.40");
        assert_eq!(format_number(52_428_801.0), "52.4M");
        assert_eq!(format_seconds(0.0119), "11.9ms");
        assert_eq!(format_seconds(2.0), "2.00s");
        assert_eq!(format_seconds(0.00025), "250µs");
    }
}
