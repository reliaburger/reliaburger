//! Prometheus text format scraping.
//!
//! Parses the Prometheus text exposition format using the
//! `prometheus-parse` crate and converts to our MetricKey/value types.
//!
//! Two kinds of target are scraped:
//!
//! - **App instances.** An app that declares `metrics = { port, path }` is
//!   scraped on every instance by the node running it, at the instance's
//!   own address. Samples carry `app` (`namespace/app`, the same value the
//!   process metrics use), `namespace`, `instance` and `node`, plus an `up`
//!   gauge recording whether the scrape worked.
//! - **Static targets.** `[[metrics.scrape_targets]]` in the node config,
//!   labelled with the target's `job` as `app`.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;

use super::collector::CollectedMetric;
use super::types::{MetricKey, Sample};
use crate::config::app::AppSpec;

/// Labels the scraper owns. An app's own label with one of these names is
/// kept as `exported_<name>`, the way Prometheus handles the clash.
const TARGET_LABELS: [&str; 4] = ["app", "namespace", "instance", "node"];

/// Largest response body accepted from one scrape (8 MiB).
///
/// A metrics endpoint is app-controlled; without a cap a broken or
/// hostile app could make the node buffer an arbitrary body.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Most samples accepted from one scrape. Like Prometheus's
/// `sample_limit`, a scrape over the limit fails rather than filling the
/// store with an app's runaway label cardinality.
const MAX_SAMPLES_PER_SCRAPE: usize = 20_000;

/// How many instances one node scrapes at the same time.
const MAX_CONCURRENT_SCRAPES: usize = 16;

/// Parse a Prometheus text exposition body into collected metrics.
///
/// Counters, gauges and untyped samples keep their name and labels. A
/// histogram becomes the same series Prometheus stores: one
/// `<name>_bucket{le="..."}` per bucket plus `<name>_sum` and
/// `<name>_count`, and a summary becomes `<name>{quantile="..."}` plus its
/// `_sum` and `_count`. NaN and infinite values are skipped, and so are
/// malformed lines.
pub fn parse_prometheus_text(body: &str) -> Vec<CollectedMetric> {
    let lines = body.lines().map(|l| Ok(l.to_owned()));
    let scrape = match prometheus_parse::Scrape::parse(lines) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let mut metrics = Vec::new();
    for sample in &scrape.samples {
        // Labels implements Deref<Target=HashMap<String, String>>
        let labels: BTreeMap<String, String> = sample
            .labels
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        match &sample.value {
            prometheus_parse::Value::Counter(v)
            | prometheus_parse::Value::Gauge(v)
            | prometheus_parse::Value::Untyped(v) => {
                push_finite(&mut metrics, sample.metric.clone(), labels, *v);
            }
            // The parser folds a histogram's `_bucket` lines into one value;
            // its `_sum` and `_count` lines arrive as separate samples.
            prometheus_parse::Value::Histogram(buckets) => {
                for bucket in buckets {
                    let mut labels = labels.clone();
                    labels.insert("le".to_string(), format_bound(bucket.less_than));
                    push_finite(
                        &mut metrics,
                        format!("{}_bucket", sample.metric),
                        labels,
                        bucket.count,
                    );
                }
            }
            prometheus_parse::Value::Summary(quantiles) => {
                for quantile in quantiles {
                    let mut labels = labels.clone();
                    labels.insert("quantile".to_string(), format_bound(quantile.quantile));
                    push_finite(&mut metrics, sample.metric.clone(), labels, quantile.count);
                }
            }
        }
    }

    metrics
}

/// A bucket bound or quantile as Prometheus writes it (`0.5`, `+Inf`).
fn format_bound(bound: f64) -> String {
    if bound == f64::INFINITY {
        "+Inf".to_string()
    } else {
        bound.to_string()
    }
}

fn push_finite(
    metrics: &mut Vec<CollectedMetric>,
    name: String,
    labels: BTreeMap<String, String>,
    value: f64,
) {
    if value.is_finite() {
        metrics.push(CollectedMetric {
            key: MetricKey::with_labels(name, labels),
            value,
        });
    }
}

/// Why one scrape produced no samples.
#[derive(Debug, thiserror::Error)]
pub enum ScrapeError {
    #[error("request failed: {0}")]
    Request(String),
    #[error("endpoint answered HTTP {0}")]
    Status(u16),
    #[error("scrape timed out after {0:?}")]
    Timeout(Duration),
    #[error("response body exceeds {MAX_BODY_BYTES} bytes")]
    BodyTooLarge,
    #[error("{0} samples exceed the per-scrape limit of {MAX_SAMPLES_PER_SCRAPE}")]
    TooManySamples(usize),
}

/// Fetch and parse one Prometheus endpoint, bounded by `timeout` and by
/// the body and sample limits.
pub async fn fetch_metrics(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<Vec<CollectedMetric>, ScrapeError> {
    let body = tokio::time::timeout(timeout, fetch_body(client, url))
        .await
        .map_err(|_| ScrapeError::Timeout(timeout))??;
    let metrics = parse_prometheus_text(&body);
    if metrics.len() > MAX_SAMPLES_PER_SCRAPE {
        return Err(ScrapeError::TooManySamples(metrics.len()));
    }
    Ok(metrics)
}

async fn fetch_body(client: &reqwest::Client, url: &str) -> Result<String, ScrapeError> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| ScrapeError::Request(e.to_string()))?;
    if !response.status().is_success() {
        return Err(ScrapeError::Status(response.status().as_u16()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ScrapeError::Request(e.to_string()))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(ScrapeError::BodyTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Scrape a Prometheus /metrics endpoint via HTTP.
///
/// Returns parsed metrics, or an empty vec on any error (connection
/// refused, timeout, invalid body). Used for the static
/// `[[metrics.scrape_targets]]`, whose failures stay silent.
pub async fn scrape_endpoint(url: &str) -> Vec<CollectedMetric> {
    let client = reqwest::Client::new();
    fetch_metrics(&client, url, Duration::from_secs(5))
        .await
        .unwrap_or_default()
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
/// Bun's scrape task calls this at the configured
/// `metrics.scrape_interval_secs` with `(job, url)` pairs from
/// `metrics.scrape_targets`. Bun omits the task when that list is empty.
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

/// One local instance's metrics endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppScrapeTarget {
    /// App name, without the namespace.
    pub app: String,
    /// Namespace the app lives in.
    pub namespace: String,
    /// Instance id, e.g. `default__web-0`.
    pub instance: String,
    /// Full URL to scrape.
    pub url: String,
}

impl AppScrapeTarget {
    /// The endpoint to scrape for one instance of `spec`, or `None` when the
    /// app declares no metrics.
    ///
    /// An instance with its own network namespace is scraped at its
    /// container IP, the way health checks probe it; a process workload
    /// shares the host network and is scraped on loopback.
    pub fn for_instance(
        instance: &str,
        app: &str,
        namespace: &str,
        container_ip: Option<std::net::Ipv4Addr>,
        spec: &AppSpec,
    ) -> Option<Self> {
        let (port, path) = spec.metrics_endpoint()?;
        let host = container_ip.unwrap_or(std::net::Ipv4Addr::LOCALHOST);
        Some(Self {
            app: app.to_string(),
            namespace: namespace.to_string(),
            instance: instance.to_string(),
            url: format!("http://{host}:{port}{path}"),
        })
    }

    /// The labels every sample from this target carries.
    fn labels(&self, node: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "app".to_string(),
                format!("{}/{}", self.namespace, self.app),
            ),
            ("namespace".to_string(), self.namespace.clone()),
            ("instance".to_string(), self.instance.clone()),
            ("node".to_string(), node.to_string()),
        ])
    }
}

/// What one sweep over the local instances achieved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppScrapeSummary {
    /// Samples written to the store, `up` gauges included.
    pub ingested: usize,
    /// Instances whose scrape failed (`up` = 0).
    pub failed: usize,
}

/// Scrape every target concurrently and store the results.
///
/// At most [`MAX_CONCURRENT_SCRAPES`] requests are in flight, each bounded
/// by `timeout`. Every sample from one sweep shares a timestamp, so the
/// series of one instance line up when they're summed or divided later.
/// Each target also gets an `up` sample: 1 when the scrape worked, 0 when
/// it didn't.
pub async fn scrape_app_targets(
    store: &tokio::sync::RwLock<super::store::MayoStore>,
    client: &reqwest::Client,
    targets: &[AppScrapeTarget],
    node: &str,
    timeout: Duration,
) -> AppScrapeSummary {
    // Each future owns its target and a client handle (a cheap `Arc`
    // clone). Futures that borrowed them would tie the stream to this
    // function's borrows, which the compiler can't prove `Send` for once
    // the whole sweep runs inside a spawned task.
    let results: Vec<(AppScrapeTarget, Result<Vec<CollectedMetric>, ScrapeError>)> =
        futures_util::stream::iter(targets.to_vec())
            .map(|target| {
                let client = client.clone();
                async move {
                    let result = fetch_metrics(&client, &target.url, timeout).await;
                    (target, result)
                }
            })
            .buffer_unordered(MAX_CONCURRENT_SCRAPES)
            .collect()
            .await;

    let timestamp = Sample::now(0.0).timestamp;
    let mut summary = AppScrapeSummary::default();
    let mut guard = store.write().await;
    for (target, result) in results {
        let target_labels = target.labels(node);
        let up = match result {
            Ok(metrics) => {
                for metric in metrics {
                    let key = relabel(metric.key, &target_labels);
                    guard.insert(&key, Sample::at(timestamp, metric.value));
                    summary.ingested += 1;
                }
                1.0
            }
            Err(error) => {
                eprintln!(
                    "mayo: scraping {}/{} instance {} at {}: {error}",
                    target.namespace, target.app, target.instance, target.url
                );
                summary.failed += 1;
                0.0
            }
        };
        let key = MetricKey::with_labels("up", target_labels);
        guard.insert(&key, Sample::at(timestamp, up));
        summary.ingested += 1;
    }
    summary
}

/// Add the target's labels to a scraped key, keeping any clashing label
/// the app set itself as `exported_<name>`.
fn relabel(mut key: MetricKey, target_labels: &BTreeMap<String, String>) -> MetricKey {
    for name in TARGET_LABELS {
        if let Some(value) = key.labels.remove(name) {
            key.labels.insert(format!("exported_{name}"), value);
        }
    }
    key.labels.extend(
        target_labels
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (
        tempfile::TempDir,
        tokio::sync::RwLock<super::super::store::MayoStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = tokio::sync::RwLock::new(super::super::store::MayoStore::new(
            dir.path().to_path_buf(),
        ));
        (dir, store)
    }

    /// Serve `body` at `/metrics` on an ephemeral loopback port.
    async fn serve_metrics(body: &'static str) -> std::net::SocketAddr {
        let router =
            axum::Router::new().route("/metrics", axum::routing::get(move || async move { body }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        address
    }

    const APP_BODY: &str = "\
# HELP http_requests_total Requests served.
# TYPE http_requests_total counter
http_requests_total{status=\"200\"} 7
http_requests_total{status=\"500\"} 1
# TYPE http_request_duration_seconds histogram
http_request_duration_seconds_bucket{le=\"0.1\"} 5
http_request_duration_seconds_bucket{le=\"0.5\"} 7
http_request_duration_seconds_bucket{le=\"+Inf\"} 8
http_request_duration_seconds_sum 1.25
http_request_duration_seconds_count 8
# TYPE instance gauge
instance{instance=\"self-reported\"} 1
";

    async fn rows(
        store: tokio::sync::RwLock<super::super::store::MayoStore>,
    ) -> Vec<(u64, String, BTreeMap<String, String>, f64)> {
        let store = store.into_inner();
        store
            .query_sql("SELECT timestamp, metric_name, labels, value FROM metrics")
            .await
            .unwrap()
            .into_iter()
            .map(|(ts, name, labels, value)| {
                (ts, name, serde_json::from_str(&labels).unwrap(), value)
            })
            .collect()
    }

    fn target(address: std::net::SocketAddr, instance: &str) -> AppScrapeTarget {
        AppScrapeTarget {
            app: "web".to_string(),
            namespace: "default".to_string(),
            instance: instance.to_string(),
            url: format!("http://{address}/metrics"),
        }
    }

    #[tokio::test]
    async fn app_scrape_labels_every_sample_with_app_namespace_instance_and_node() {
        let address = serve_metrics(APP_BODY).await;
        let (_dir, store) = store();
        let client = reqwest::Client::new();
        let summary = scrape_app_targets(
            &store,
            &client,
            &[target(address, "web-0")],
            "node-a",
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(summary.failed, 0);

        let rows = rows(store).await;
        assert_eq!(rows.len(), summary.ingested);
        for (_, name, labels, _) in &rows {
            assert_eq!(labels["app"], "default/web", "{name}");
            assert_eq!(labels["namespace"], "default", "{name}");
            assert_eq!(labels["instance"], "web-0", "{name}");
            assert_eq!(labels["node"], "node-a", "{name}");
        }
        // One sweep, one timestamp: sums and ratios line up later.
        let first = rows[0].0;
        assert!(rows.iter().all(|(ts, ..)| *ts == first));

        let value = |metric: &str, extra: &[(&str, &str)]| {
            rows.iter()
                .find(|(_, name, labels, _)| {
                    name == metric
                        && extra
                            .iter()
                            .all(|(k, v)| labels.get(*k).map(String::as_str) == Some(*v))
                })
                .map(|(.., value)| *value)
        };
        assert_eq!(
            value("http_requests_total", &[("status", "200")]),
            Some(7.0)
        );
        assert_eq!(value("up", &[]), Some(1.0));
        // Histograms are stored as Prometheus stores them, not summed.
        assert_eq!(
            value("http_request_duration_seconds_bucket", &[("le", "0.1")]),
            Some(5.0)
        );
        assert_eq!(
            value("http_request_duration_seconds_bucket", &[("le", "+Inf")]),
            Some(8.0)
        );
        assert_eq!(value("http_request_duration_seconds_sum", &[]), Some(1.25));
        assert_eq!(value("http_request_duration_seconds_count", &[]), Some(8.0));
        // The app's own `instance` label survives under another name.
        assert_eq!(
            value("instance", &[("exported_instance", "self-reported")]),
            Some(1.0)
        );
    }

    #[tokio::test]
    async fn a_failing_scrape_records_up_zero_and_no_samples() {
        // Bind then drop, so the port is (almost certainly) closed.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let (_dir, store) = store();
        let summary = scrape_app_targets(
            &store,
            &reqwest::Client::new(),
            &[target(closed, "web-1")],
            "node-a",
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(
            summary,
            AppScrapeSummary {
                ingested: 1,
                failed: 1
            }
        );
        let rows = rows(store).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "up");
        assert_eq!(rows[0].2["instance"], "web-1");
        assert_eq!(rows[0].3, 0.0);
    }

    #[tokio::test]
    async fn a_hung_endpoint_times_out_instead_of_stalling_the_sweep() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hung = listener.local_addr().unwrap();
        // Accept and never answer.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let healthy = serve_metrics(APP_BODY).await;
        let (_dir, store) = store();
        let started = std::time::Instant::now();
        let summary = scrape_app_targets(
            &store,
            &reqwest::Client::new(),
            &[target(hung, "web-0"), target(healthy, "web-1")],
            "node-a",
            Duration::from_millis(300),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(summary.failed, 1);
        let rows = rows(store).await;
        assert!(
            rows.iter()
                .any(|(_, name, labels, _)| name == "http_requests_total"
                    && labels["instance"] == "web-1")
        );
    }

    #[tokio::test]
    async fn an_error_status_fails_the_scrape() {
        let router = axum::Router::new().route(
            "/metrics",
            axum::routing::get(|| async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "no") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let error = fetch_metrics(
            &reqwest::Client::new(),
            &format!("http://{address}/metrics"),
            Duration::from_secs(2),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(error, ScrapeError::Status(500)), "{error}");
    }

    fn app_spec(toml: &str) -> AppSpec {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn a_container_instance_is_scraped_at_its_own_address() {
        let spec = app_spec("image = \"podinfo\"\nport = 9898\nmetrics = { port = 9797 }");
        let target = AppScrapeTarget::for_instance(
            "frontend-1",
            "frontend",
            "default",
            Some("10.0.2.7".parse().unwrap()),
            &spec,
        )
        .unwrap();
        assert_eq!(target.url, "http://10.0.2.7:9797/metrics");
        assert_eq!(target.instance, "frontend-1");
    }

    #[test]
    fn a_process_instance_is_scraped_on_loopback_at_the_app_port() {
        let spec = app_spec("image = \"x\"\nport = 8080\nmetrics = { path = \"/prom\" }");
        let target = AppScrapeTarget::for_instance("web-0", "web", "team", None, &spec).unwrap();
        assert_eq!(target.url, "http://127.0.0.1:8080/prom");
    }

    #[test]
    fn an_app_without_metrics_has_no_scrape_target() {
        let spec = app_spec("image = \"x\"\nport = 8080");
        assert!(AppScrapeTarget::for_instance("web-0", "web", "team", None, &spec).is_none());
    }

    #[test]
    fn parse_histogram_emits_bucket_sum_and_count_series() {
        let body = "\
# TYPE request_duration_seconds histogram
request_duration_seconds_bucket{path=\"/\",le=\"0.1\"} 10
request_duration_seconds_bucket{path=\"/\",le=\"0.5\"} 20
request_duration_seconds_bucket{path=\"/\",le=\"+Inf\"} 30
request_duration_seconds_sum{path=\"/\"} 15.5
request_duration_seconds_count{path=\"/\"} 30
";
        let metrics = parse_prometheus_text(body);
        let series: Vec<String> = {
            let mut series: Vec<String> = metrics
                .iter()
                .map(|m| format!("{} {}", m.key, m.value))
                .collect();
            series.sort();
            series
        };
        assert_eq!(
            series,
            vec![
                "request_duration_seconds_bucket{le=\"+Inf\",path=\"/\"} 30",
                "request_duration_seconds_bucket{le=\"0.1\",path=\"/\"} 10",
                "request_duration_seconds_bucket{le=\"0.5\",path=\"/\"} 20",
                "request_duration_seconds_count{path=\"/\"} 30",
                "request_duration_seconds_sum{path=\"/\"} 15.5",
            ]
        );
    }

    #[test]
    fn parse_summary_emits_quantile_series() {
        let body = "\
# TYPE rpc_seconds summary
rpc_seconds{quantile=\"0.5\"} 0.2
rpc_seconds{quantile=\"0.99\"} 0.9
rpc_seconds_sum 12
rpc_seconds_count 40
";
        let metrics = parse_prometheus_text(body);
        let median = metrics
            .iter()
            .find(|m| m.key.labels.get("quantile").map(String::as_str) == Some("0.5"))
            .unwrap();
        assert_eq!(median.key.name.as_str(), "rpc_seconds");
        assert_eq!(median.value, 0.2);
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "rpc_seconds_count" && m.value == 40.0)
        );
    }

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
