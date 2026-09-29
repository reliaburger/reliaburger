/// Built-in test app for integration tests.
///
/// A configurable HTTP server whose behaviour is controlled by
/// constructor args. Returns the bound port so callers know where
/// to probe. Runs as a `tokio::spawn`ed task.
///
/// The server is deliberately hand-rolled rather than axum: it ships inside
/// `bun` and runs as a *workload* on real clusters, so it must stay tiny and
/// have no opinion about the runtime it's dropped into.
///
/// Most paths are answered by the [`TestAppMode`], which is the app's whole
/// identity — a `Hang` app hangs on everything. Two paths are special and
/// answered the same way in every mode except `Hang`, because tools need
/// them regardless of what behaviour the app is simulating:
///
/// - `GET /payload?bytes=N` — exactly `N` bytes of body, for throughput
///   measurement.
/// - `GET /env/NAME` — the value of an environment variable, or 404. This is
///   how a test proves a secret was decrypted inside the workload rather
///   than trusting the API's word for it.
///
/// `GET /metrics` answers too, with Prometheus text: `http_requests_total`
/// and an `http_request_duration_seconds` histogram over the requests the
/// mode answered. It's what lets a laptop demo show `relish metrics` and the
/// dashboard's request and latency charts without a real app.
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Ceiling on a single `/payload` response.
///
/// The size comes off the wire, so it's attacker-controlled in exactly the
/// sense the Raft frame reader was (O6): without a bound, `?bytes=` is a
/// request to allocate whatever the caller names. 256 MiB is far above any
/// throughput sample and far below trouble.
const MAX_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;

/// What behaviour the test app should exhibit.
#[derive(Debug, Clone)]
pub enum TestAppMode {
    /// Always returns 200 on any path.
    Healthy,
    /// Returns 200 for the first `n` requests, then 500.
    UnhealthyAfter(u32),
    /// Accepts connections but never responds.
    Hang,
    /// Exits cleanly after `n` requests.
    ExitAfter(u32),
    /// Responds after a delay.
    Slow(Duration),
    /// Holds `n` MiB of resident memory and otherwise behaves as healthy.
    ///
    /// For memory-pressure faults and capacity benchmarks: an app that says
    /// it wants 256 MiB but touches none of it isn't under pressure when you
    /// squeeze it.
    Alloc(usize),
}

/// The path from an HTTP request line, e.g. `GET /payload?bytes=32 HTTP/1.1`.
///
/// Returns `None` for anything that isn't a request line. Kept separate and
/// pure so the routing below is testable without a socket.
pub fn parse_request_path(request_line: &str) -> Option<&str> {
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    parts.next()
}

/// Build an HTTP response for the two special paths, if this is one of them.
///
/// `None` means "not a special path — let the mode decide". Pure: the only
/// outside state it reads is the process environment, which is what
/// `/env/NAME` is for.
pub fn special_route_response(path: &str) -> Option<String> {
    let (route, query) = match path.split_once('?') {
        Some((route, query)) => (route, Some(query)),
        None => (path, None),
    };

    if route == "/payload" {
        let requested = query
            .and_then(|query| {
                query.split('&').find_map(|pair| {
                    pair.strip_prefix("bytes=")
                        .and_then(|value| value.parse::<usize>().ok())
                })
            })
            .unwrap_or(1024);
        let bytes = requested.min(MAX_PAYLOAD_BYTES);
        // A repeating byte rather than random data: the point is to move a
        // known number of bytes, and a compressible body measures the same
        // over a raw TCP socket while costing nothing to generate.
        let body = "x".repeat(bytes);
        return Some(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {bytes}\r\n\r\n{body}"
        ));
    }

    if let Some(name) = route.strip_prefix("/env/") {
        return Some(match std::env::var(name) {
            Ok(value) => format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{value}",
                value.len()
            ),
            Err(_) => "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found".to_string(),
        });
    }

    None
}

/// Upper bounds (seconds) of the latency histogram's buckets.
const LATENCY_BUCKETS: [f64; 8] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0];

/// Request count and latency histogram behind `GET /metrics`.
///
/// Atomics rather than a mutex: every connection task records into the
/// same counters, and a relaxed increment is all a counter needs.
#[derive(Debug, Default)]
struct RequestMetrics {
    /// Cumulative count per bucket in `LATENCY_BUCKETS`, as Prometheus
    /// histograms are: a 20 ms request lands in every bucket from 25 ms up.
    buckets: [AtomicU64; LATENCY_BUCKETS.len()],
    count: AtomicU64,
    sum_micros: AtomicU64,
}

impl RequestMetrics {
    fn observe(&self, elapsed: Duration) {
        let seconds = elapsed.as_secs_f64();
        for (bound, bucket) in LATENCY_BUCKETS.iter().zip(&self.buckets) {
            if seconds <= *bound {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }

    /// The Prometheus text exposition of these metrics.
    fn render(&self) -> String {
        let count = self.count.load(Ordering::Relaxed);
        let mut body = String::from(
            "# HELP http_requests_total Requests the test app answered.\n\
             # TYPE http_requests_total counter\n",
        );
        body.push_str(&format!("http_requests_total {count}\n"));
        body.push_str(
            "# HELP http_request_duration_seconds Time to answer a request.\n\
             # TYPE http_request_duration_seconds histogram\n",
        );
        for (bound, bucket) in LATENCY_BUCKETS.iter().zip(&self.buckets) {
            body.push_str(&format!(
                "http_request_duration_seconds_bucket{{le=\"{bound}\"}} {}\n",
                bucket.load(Ordering::Relaxed)
            ));
        }
        body.push_str(&format!(
            "http_request_duration_seconds_bucket{{le=\"+Inf\"}} {count}\n"
        ));
        let sum = self.sum_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        body.push_str(&format!("http_request_duration_seconds_sum {sum}\n"));
        body.push_str(&format!("http_request_duration_seconds_count {count}\n"));
        body
    }

    fn response(&self) -> String {
        let body = self.render();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }
}

/// A test HTTP server for integration tests.
pub struct TestApp {
    port: u16,
    shutdown: CancellationToken,
}

impl TestApp {
    /// Start the test app on an ephemeral port. Returns immediately.
    pub async fn start(mode: TestAppMode) -> std::io::Result<Self> {
        Self::start_on_port(mode, 0).await
    }

    /// Start the test app on a specific port (0 = ephemeral). Returns immediately.
    ///
    /// Binds `0.0.0.0`, not loopback: as a cluster workload this runs inside
    /// its own network namespace, and a loopback-only bind would be
    /// unreachable from the node that needs to health-check it. Loopback
    /// callers are unaffected — `0.0.0.0` accepts on every interface.
    pub async fn start_on_port(mode: TestAppMode, port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(format!("0.0.0.0:{port}")).await?;
        let port = listener.local_addr()?.port();
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let request_count = Arc::new(AtomicU32::new(0));
        let request_metrics = Arc::new(RequestMetrics::default());

        // Hold the ballast for the lifetime of the server task. Written to
        // once so the pages are actually resident — an untouched allocation
        // may never be faulted in, and then the fault you inject squeezes
        // nothing.
        let ballast: Option<Vec<u8>> = match &mode {
            TestAppMode::Alloc(mib) => Some(vec![7u8; mib * 1024 * 1024]),
            _ => None,
        };

        tokio::spawn(async move {
            let _ballast = ballast;
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    accept = listener.accept() => {
                        match accept {
                            Ok((mut socket, _)) => {
                                let count = request_count.fetch_add(1, Ordering::SeqCst);
                                let mode = mode.clone();
                                let token = token.clone();
                                let request_metrics = Arc::clone(&request_metrics);

                                tokio::spawn(async move {
                                    let started = Instant::now();
                                    // Read the request (must consume it before responding)
                                    let mut buf = vec![0u8; 4096];
                                    let n = socket.read(&mut buf).await.unwrap_or(0);

                                    // Extract method and path from the request line
                                    let request_line = std::str::from_utf8(&buf[..n])
                                        .unwrap_or("")
                                        .lines()
                                        .next()
                                        .unwrap_or("?");
                                    let path = parse_request_path(request_line).unwrap_or("/");

                                    // `Hang` hangs on everything — the mode is
                                    // the app's identity, and an app that
                                    // answers /payload isn't hanging.
                                    if !matches!(mode, TestAppMode::Hang) {
                                        let special = if path == "/metrics" {
                                            Some(request_metrics.response())
                                        } else {
                                            special_route_response(path)
                                        };
                                        if let Some(response) = special {
                                            let _ = socket.write_all(response.as_bytes()).await;
                                            return;
                                        }
                                    }

                                    let response = match &mode {
                                        TestAppMode::Healthy | TestAppMode::Alloc(_) => {
                                            println!("{request_line} -> 200");
                                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                                                .to_string()
                                        }
                                        TestAppMode::UnhealthyAfter(n) => {
                                            if count < *n {
                                                println!("{request_line} -> 200 ({}/{})", count + 1, n);
                                                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                                                    .to_string()
                                            } else {
                                                println!("{request_line} -> 500");
                                                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\n\r\nerror"
                                                    .to_string()
                                            }
                                        }
                                        TestAppMode::Hang => {
                                            println!("{request_line} -> (hang)");
                                            // Never respond
                                            tokio::time::sleep(Duration::from_secs(3600)).await;
                                            return;
                                        }
                                        TestAppMode::ExitAfter(n) => {
                                            println!("{request_line} -> 200 ({}/{})", count + 1, n);
                                            if count >= *n {
                                                token.cancel();
                                            }
                                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                                                .to_string()
                                        }
                                        TestAppMode::Slow(delay) => {
                                            println!("{request_line} -> 200 (after {}ms)", delay.as_millis());
                                            tokio::time::sleep(*delay).await;
                                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                                                .to_string()
                                        }
                                    };

                                    let _ = socket.write_all(response.as_bytes()).await;
                                    request_metrics.observe(started.elapsed());
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Ok(Self { port, shutdown })
    }

    /// The port the test app is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Shut down the test app.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

/// Parse a `--mode` string into a [`TestAppMode`].
///
/// Shared by the standalone `testapp` binary and `bun testapp`, so the two
/// can't drift — the previous standalone parser silently lacked
/// `exit-after`, which the library had supported for some time.
pub fn parse_mode(
    mode: &str,
    count: u32,
    delay_ms: u64,
    alloc_mib: usize,
) -> Result<TestAppMode, String> {
    match mode {
        "healthy" => Ok(TestAppMode::Healthy),
        "unhealthy-after" => Ok(TestAppMode::UnhealthyAfter(count)),
        "hang" => Ok(TestAppMode::Hang),
        "exit-after" => Ok(TestAppMode::ExitAfter(count)),
        "slow" => Ok(TestAppMode::Slow(Duration::from_millis(delay_ms))),
        "alloc" => Ok(TestAppMode::Alloc(alloc_mib)),
        other => Err(format!(
            "unknown mode {other:?}; valid modes: healthy, unhealthy-after, hang, \
             exit-after, slow, alloc"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_path_is_extracted_from_the_request_line() {
        assert_eq!(parse_request_path("GET /health HTTP/1.1"), Some("/health"));
        assert_eq!(
            parse_request_path("POST /payload?bytes=64 HTTP/1.1"),
            Some("/payload?bytes=64")
        );
        assert_eq!(parse_request_path(""), None);
        assert_eq!(parse_request_path("garbage"), None);
    }

    #[test]
    fn payload_serves_exactly_the_requested_bytes() {
        let response = special_route_response("/payload?bytes=2048").expect("payload route");
        assert!(response.contains("Content-Length: 2048"));
        let body = response.split("\r\n\r\n").nth(1).expect("body");
        assert_eq!(body.len(), 2048);
    }

    #[test]
    fn payload_defaults_when_no_size_is_given() {
        let response = special_route_response("/payload").expect("payload route");
        assert!(response.contains("Content-Length: 1024"));
    }

    /// The size comes off the wire, so it is a claim, not a fact — the same
    /// lesson as the Raft frame reader.
    #[test]
    fn payload_size_is_capped() {
        let response =
            special_route_response("/payload?bytes=999999999999").expect("payload route");
        assert!(
            response.contains(&format!("Content-Length: {MAX_PAYLOAD_BYTES}")),
            "an absurd size must be clamped, not allocated"
        );
    }

    #[test]
    fn env_route_returns_the_variable_or_404() {
        // SAFETY: single-threaded test process, and the variable name is
        // unique to this test.
        unsafe { std::env::set_var("RB_TESTAPP_FIXTURE", "decrypted-value") };
        let response = special_route_response("/env/RB_TESTAPP_FIXTURE").expect("env route");
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("decrypted-value"));

        let missing =
            special_route_response("/env/RB_TESTAPP_DEFINITELY_UNSET").expect("env route");
        assert!(missing.starts_with("HTTP/1.1 404"));
        unsafe { std::env::remove_var("RB_TESTAPP_FIXTURE") };
    }

    #[test]
    fn ordinary_paths_are_left_to_the_mode() {
        assert!(special_route_response("/").is_none());
        assert!(special_route_response("/health").is_none());
        // A near-miss must not be captured by the payload route.
        assert!(special_route_response("/payloadish").is_none());
    }

    #[test]
    fn request_metrics_render_a_counter_and_a_cumulative_histogram() {
        let metrics = RequestMetrics::default();
        metrics.observe(Duration::from_millis(3));
        metrics.observe(Duration::from_millis(40));
        metrics.observe(Duration::from_secs(2));
        let body = metrics.render();
        // The scraper reads what it wrote: one parser, both directions.
        let parsed = crate::mayo::scrape::parse_prometheus_text(&body);
        let value = |name: &str, le: Option<&str>| {
            parsed
                .iter()
                .find(|m| {
                    m.key.name.as_str() == name && m.key.labels.get("le").map(String::as_str) == le
                })
                .map(|m| m.value)
        };
        assert_eq!(value("http_requests_total", None), Some(3.0));
        assert_eq!(
            value("http_request_duration_seconds_bucket", Some("0.005")),
            Some(1.0)
        );
        assert_eq!(
            value("http_request_duration_seconds_bucket", Some("0.05")),
            Some(2.0)
        );
        assert_eq!(
            value("http_request_duration_seconds_bucket", Some("1")),
            Some(2.0)
        );
        assert_eq!(
            value("http_request_duration_seconds_bucket", Some("+Inf")),
            Some(3.0)
        );
        assert_eq!(
            value("http_request_duration_seconds_count", None),
            Some(3.0)
        );
        let sum = value("http_request_duration_seconds_sum", None).unwrap();
        assert!((sum - 2.043).abs() < 1e-6, "{sum}");
    }

    #[tokio::test]
    async fn a_running_app_counts_the_requests_it_answers_on_metrics() {
        let app = TestApp::start(TestAppMode::Healthy).await.unwrap();
        let address = format!("127.0.0.1:{}", app.port());
        fetch(&address, "/").await;
        fetch(&address, "/healthz").await;
        // Recording happens after the write; give the task a moment.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let body = fetch(&address, "/metrics").await;
            if body.contains("http_requests_total 2\n") {
                break;
            }
            assert!(Instant::now() < deadline, "{body}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        app.shutdown();
    }

    #[test]
    fn every_documented_mode_parses() {
        for mode in [
            "healthy",
            "unhealthy-after",
            "hang",
            "exit-after",
            "slow",
            "alloc",
        ] {
            assert!(parse_mode(mode, 3, 100, 8).is_ok(), "{mode} must parse");
        }
        let error = parse_mode("banana", 3, 100, 8).unwrap_err();
        assert!(error.contains("banana"), "{error}");
        assert!(
            error.contains("exit-after"),
            "the error must list valid modes: {error}"
        );
    }

    #[tokio::test]
    async fn occupied_port_returns_an_error_without_panicking() {
        let listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let result = TestApp::start_on_port(TestAppMode::Healthy, port).await;
        assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::AddrInUse));
    }

    #[tokio::test]
    async fn a_running_app_serves_payload_and_falls_through_to_the_mode() {
        let app = TestApp::start(TestAppMode::Healthy).await.unwrap();
        let address = format!("127.0.0.1:{}", app.port());

        let payload = fetch(&address, "/payload?bytes=512").await;
        assert!(payload.contains("Content-Length: 512"), "{payload}");

        let root = fetch(&address, "/").await;
        assert!(root.starts_with("HTTP/1.1 200 OK"), "{root}");

        app.shutdown();
    }

    /// `Hang` is the app's identity — a hanging app that helpfully answers
    /// /payload is not hanging.
    #[tokio::test]
    async fn a_hanging_app_hangs_on_the_special_routes_too() {
        let app = TestApp::start(TestAppMode::Hang).await.unwrap();
        let address = format!("127.0.0.1:{}", app.port());
        let result = tokio::time::timeout(
            Duration::from_millis(300),
            fetch(&address, "/payload?bytes=16"),
        )
        .await;
        assert!(result.is_err(), "hang mode answered a special route");
        app.shutdown();
    }

    async fn fetch(address: &str, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = Vec::new();
        // Read until the peer closes or we have the head plus a little body;
        // the server writes one response and leaves the socket open.
        let mut buf = vec![0u8; 8192];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    response.extend_from_slice(&buf[..n]);
                    if response.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&response).to_string()
    }
}
