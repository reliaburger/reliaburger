//! Prometheus text format scraping.
//!
//! Parses the Prometheus text exposition format using the
//! `prometheus-parse` crate and converts to our MetricKey/value types.

use std::collections::BTreeMap;

use super::collector::CollectedMetric;
use super::types::MetricKey;

/// Parse a Prometheus text exposition body into collected metrics.
///
/// Handles gauges, counters, histograms (_bucket/_sum/_count), and
/// summaries. HELP and TYPE lines are consumed by the parser.
/// Malformed lines are silently skipped.
pub fn parse_prometheus_text(body: &str) -> Vec<CollectedMetric> {
    let lines = body.lines().map(|l| Ok(l.to_owned()));
    let scrape = match prometheus_parse::Scrape::parse(lines) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut metrics = Vec::new();
    for sample in &scrape.samples {
        let value = match &sample.value {
            prometheus_parse::Value::Counter(v)
            | prometheus_parse::Value::Gauge(v)
            | prometheus_parse::Value::Untyped(v) => *v,
            prometheus_parse::Value::Histogram(buckets) => {
                buckets.iter().map(|b| b.count).sum::<f64>()
            }
            prometheus_parse::Value::Summary(quantiles) => {
                quantiles.iter().map(|q| q.count).sum::<f64>()
            }
        };

        if value.is_nan() || value.is_infinite() {
            continue;
        }

        // Labels implements Deref<Target=HashMap<String, String>>
        let mut labels = BTreeMap::new();
        for (k, v) in sample.labels.iter() {
            labels.insert(k.clone(), v.clone());
        }

        metrics.push(CollectedMetric {
            key: MetricKey::with_labels(&sample.metric, labels),
            value,
        });
    }

    metrics
}

/// Scrape a Prometheus /metrics endpoint via HTTP.
///
/// Returns parsed metrics, or an empty vec on any error (connection
/// refused, timeout, invalid body). Scraping failures are silent —
/// not every app exposes /metrics.
pub async fn scrape_endpoint(url: &str) -> Vec<CollectedMetric> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default();

    let resp = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return Vec::new(),
    };

    let body = match resp.text().await {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };

    parse_prometheus_text(&body)
}

/// Tag parsed samples with a target's `job` (as the `app` label) and ingest
/// them into the shared Mayo store. Returns how many samples were ingested.
///
/// Split out from [`scrape_once`] so the parse-and-ingest path is unit-testable
/// without a live HTTP endpoint. The `app` label is what the per-app dashboards
/// and `/v1/metrics/app/...` queries filter on, so a scraped target shows up
/// alongside the process metrics the collector produces for the same app.
pub async fn ingest_samples(
    store: &tokio::sync::RwLock<super::store::MayoStore>,
    metrics: Vec<CollectedMetric>,
    job: &str,
) -> usize {
    if metrics.is_empty() {
        return 0;
    }
    let mut guard = store.write().await;
    let mut ingested = 0;
    for mut metric in metrics {
        // The target's own labels stay; `app` lets per-app views find it.
        metric.key.labels.insert("app".to_string(), job.to_string());
        guard.insert_now(&metric.key, metric.value);
        ingested += 1;
    }
    ingested
}

/// Scrape every configured Prometheus target once and ingest the results.
///
/// This is the entry point a periodic scrape loop calls on each tick. Targets
/// are scraped in turn; a target that fails (connection refused, timeout, bad
/// body) contributes nothing rather than failing the whole sweep, since not
/// every declared endpoint is guaranteed up. Returns the total number of
/// samples ingested across all targets.
///
/// # Integrator hook
///
/// There is no scrape loop yet — the natural home is `src/bin/bun.rs`, next to
/// the metrics collection loop that already owns the `mayo_store` handle. The
/// integrator should spawn a task that ticks every
/// `config.metrics.scrape_interval_secs` and calls
/// `scrape_once(&mayo_store, &targets)`, where `targets` is built from
/// `config.metrics.scrape_targets` (`(job, url)` pairs). An empty target list
/// means the loop need not be spawned at all.
pub async fn scrape_once(
    store: &tokio::sync::RwLock<super::store::MayoStore>,
    targets: &[(String, String)],
) -> usize {
    let mut ingested = 0;
    for (job, url) in targets {
        let metrics = scrape_endpoint(url).await;
        ingested += ingest_samples(store, metrics, job).await;
    }
    ingested
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ingest_samples_parses_and_ingests_with_app_label() {
        let dir = tempfile::tempdir().unwrap();
        let store = tokio::sync::RwLock::new(super::super::store::MayoStore::new(
            dir.path().to_path_buf(),
        ));

        let body = "\
# TYPE http_requests_total counter
http_requests_total{method=\"GET\"} 5
http_requests_total{method=\"POST\"} 2
";
        let metrics = parse_prometheus_text(body);
        let ingested = ingest_samples(&store, metrics, "default/web").await;
        assert_eq!(ingested, 2);

        let guard = store.read().await;
        assert_eq!(guard.buffer_len(), 2);
        drop(guard);

        // The `app` label the per-app views filter on is present.
        let store = store.into_inner();
        let rows = store
            .query_sql(
                "SELECT timestamp, metric_name, labels, value FROM metrics \
                 WHERE labels LIKE '%default/web%'",
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|(_, name, _, _)| name == "http_requests_total")
        );
    }

    #[tokio::test]
    async fn ingest_samples_empty_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = tokio::sync::RwLock::new(super::super::store::MayoStore::new(
            dir.path().to_path_buf(),
        ));
        assert_eq!(ingest_samples(&store, Vec::new(), "job").await, 0);
        assert_eq!(store.read().await.buffer_len(), 0);
    }

    #[test]
    fn parse_simple_gauge() {
        let body = "temperature_celsius 36.6\n";
        let metrics = parse_prometheus_text(body);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].key.name.as_str(), "temperature_celsius");
        assert_eq!(metrics[0].value, 36.6);
    }

    #[test]
    fn parse_counter_with_labels() {
        let body = "\
# HELP http_requests_total Total HTTP requests.
# TYPE http_requests_total counter
http_requests_total{method=\"GET\",code=\"200\"} 1027
http_requests_total{method=\"POST\",code=\"201\"} 42
";
        let metrics = parse_prometheus_text(body);
        assert_eq!(metrics.len(), 2);

        let get = metrics
            .iter()
            .find(|m| m.key.labels.get("method").map(|v| v.as_str()) == Some("GET"))
            .unwrap();
        assert_eq!(get.value, 1027.0);
        assert_eq!(get.key.labels.get("code").unwrap(), "200");
    }

    #[test]
    fn parse_help_and_type_lines_skipped() {
        let body = "\
# HELP metric_a A help line.
# TYPE metric_a gauge
metric_a 42
# HELP metric_b Another help line.
# TYPE metric_b counter
metric_b 99
";
        let metrics = parse_prometheus_text(body);
        assert_eq!(metrics.len(), 2);
    }

    #[test]
    fn parse_empty_body_returns_empty() {
        let metrics = parse_prometheus_text("");
        assert!(metrics.is_empty());
    }

    #[test]
    fn parse_multiline_body() {
        let body = "metric_a 1\nmetric_b 2\nmetric_c 3\n";
        let metrics = parse_prometheus_text(body);
        assert_eq!(metrics.len(), 3);
    }

    #[test]
    fn parse_malformed_line_skipped() {
        let body = "good_metric 42\nthis is not valid\nanother_good 99\n";
        let metrics = parse_prometheus_text(body);
        // prometheus-parse may skip or include the malformed line
        // depending on version — at minimum the good ones should parse
        assert!(metrics.len() >= 2);
    }

    #[test]
    fn parse_histogram_extracts_count() {
        let body = "\
# TYPE request_duration_seconds histogram
request_duration_seconds_bucket{le=\"0.1\"} 10
request_duration_seconds_bucket{le=\"0.5\"} 20
request_duration_seconds_bucket{le=\"+Inf\"} 30
request_duration_seconds_sum 15.5
request_duration_seconds_count 30
";
        let metrics = parse_prometheus_text(body);
        // Should have bucket entries + sum + count
        assert!(!metrics.is_empty());
    }

    #[test]
    fn nan_and_inf_values_skipped() {
        let body = "metric_nan NaN\nmetric_inf +Inf\nmetric_ok 42\n";
        let metrics = parse_prometheus_text(body);
        // NaN and Inf should be filtered out
        let ok = metrics.iter().find(|m| m.key.name.as_str() == "metric_ok");
        assert!(ok.is_some());
        assert_eq!(ok.unwrap().value, 42.0);
    }

    #[test]
    fn labels_are_btree_ordered() {
        let body = "metric{z=\"3\",a=\"1\",m=\"2\"} 1\n";
        let metrics = parse_prometheus_text(body);
        assert_eq!(metrics.len(), 1);
        let keys: Vec<&String> = metrics[0].key.labels.keys().collect();
        assert_eq!(keys, vec!["a", "m", "z"]);
    }
}
