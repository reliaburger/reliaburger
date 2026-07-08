//! Integration tests for the Wrapper ingress listener (Stage 4 W2, L7).
//!
//! These drive the binary path: a real `BunAgent`, the HTTP API, and
//! the bound proxy listeners, with requests flowing end-to-end from an
//! HTTP client through the proxy to a live backend.

use std::net::SocketAddr;
use std::time::Duration;

use reliaburger::bun::agent::{AgentCommand, BunAgent};
use reliaburger::bun::api;
use reliaburger::bun::testapp::{TestApp, TestAppMode};
use reliaburger::config::Config;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::process::ProcessGrill;
use reliaburger::relish::client::BunClient;
use reliaburger::wrapper::proxy::bind_proxy;
use reliaburger::wrapper::types::WrapperConfig;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Agent + API + bound ingress listeners, all on ephemeral ports.
struct IngressHarness {
    client: BunClient,
    http_addr: SocketAddr,
    https_addr: SocketAddr,
    #[allow(dead_code)]
    cmd_tx: mpsc::Sender<AgentCommand>,
    shutdown: CancellationToken,
}

impl IngressHarness {
    async fn start() -> Self {
        Self::start_with_port_range(42000, 43000).await
    }

    /// Start with the allocator pinned to exactly one port.
    ///
    /// ProcessGrill runs the process directly on the host: there is no
    /// port mapping, so the "allocated host port" the agent registers
    /// as the backend address is a fiction unless the process really
    /// listens there. Pinning the allocator to the TestApp's port makes
    /// the registered backend genuinely reachable.
    async fn start_reaching(test_app_port: u16) -> Self {
        Self::start_with_port_range(test_app_port, test_app_port + 1).await
    }

    async fn start_with_port_range(range_start: u16, range_end: u16) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();

        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(range_start, range_end);
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, shutdown.clone());

        // The proxy shares the routing table the agent rebuilds on deploys.
        let routing_table = agent.routing_table_handle();
        tokio::spawn(async move {
            agent.run().await;
        });

        // Bind ingress on port 0 so the OS assigns free ports.
        let wrapper_config = WrapperConfig {
            http_port: 0,
            https_port: 0,
            ..WrapperConfig::default()
        };
        let bound = bind_proxy(wrapper_config, routing_table, shutdown.clone())
            .await
            .unwrap();
        let http_addr = bound.http_addr;
        let https_addr = bound.https_addr;
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        // API server for deploys.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router(
            cmd_tx.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    server_shutdown.cancelled().await;
                })
                .await
                .ok();
        });

        let client = BunClient::new(&format!("http://127.0.0.1:{port}"));
        for _ in 0..20 {
            if client.health().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self {
            client,
            http_addr,
            https_addr,
            cmd_tx,
            shutdown,
        }
    }

    fn http_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.http_addr.port())
    }

    fn https_url(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.https_addr.port())
    }
}

impl Drop for IngressHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Poll the proxy until a request for `host` returns `expected`, or fail.
///
/// The routing table rebuilds asynchronously after a deploy, so the
/// first requests may 404 while the route is still being registered.
async fn wait_for_status(
    http_client: &reqwest::Client,
    url: &str,
    host: &str,
    expected: u16,
) -> reqwest::Response {
    for _ in 0..40 {
        if let Ok(response) = http_client.get(url).header("host", host).send().await
            && response.status().as_u16() == expected
        {
            return response;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("proxy never returned {expected} for host {host} at {url}");
}

#[tokio::test]
async fn ingress_routes_request_to_healthy_backend() {
    let test_app = TestApp::start(TestAppMode::Healthy).await;
    let harness = IngressHarness::start_reaching(test_app.port()).await;

    let config = Config::parse(&format!(
        r#"
        [app.web]
        image = "test:v1"
        port = {}

        [app.web.ingress]
        host = "web.test"
    "#,
        test_app.port()
    ))
    .unwrap();
    harness.client.apply(&config).await.unwrap();

    let http_client = reqwest::Client::new();
    let response =
        wait_for_status(&http_client, &harness.http_url("/healthz"), "web.test", 200).await;
    assert_eq!(response.status().as_u16(), 200);

    test_app.shutdown();
}

#[tokio::test]
async fn ingress_returns_404_for_unknown_host() {
    let harness = IngressHarness::start().await;

    let http_client = reqwest::Client::new();
    let response = http_client
        .get(harness.http_url("/"))
        .header("host", "nobody.test")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
}

#[tokio::test]
async fn ingress_returns_502_when_backend_unreachable() {
    let harness = IngressHarness::start().await;

    // Port 1 is never listening: the route exists, the forward fails.
    let config = Config::parse(
        r#"
        [app.dead]
        image = "test:v1"
        port = 1

        [app.dead.ingress]
        host = "dead.test"
    "#,
    )
    .unwrap();
    harness.client.apply(&config).await.unwrap();

    let http_client = reqwest::Client::new();
    let response = wait_for_status(&http_client, &harness.http_url("/"), "dead.test", 502).await;
    assert_eq!(response.status().as_u16(), 502);
}

#[tokio::test]
async fn ingress_rate_limit_returns_429_with_retry_after() {
    let test_app = TestApp::start(TestAppMode::Healthy).await;
    let harness = IngressHarness::start_reaching(test_app.port()).await;

    let config = Config::parse(&format!(
        r#"
        [app.api]
        image = "test:v1"
        port = {}

        [app.api.ingress]
        host = "api.test"
        rate_limit_rps = 1
        rate_limit_burst = 1
    "#,
        test_app.port()
    ))
    .unwrap();
    harness.client.apply(&config).await.unwrap();

    let http_client = reqwest::Client::new();
    // First request drains the single-token bucket.
    wait_for_status(&http_client, &harness.http_url("/healthz"), "api.test", 200).await;

    // An immediate burst must hit the limiter.
    let mut saw_429 = false;
    for _ in 0..5 {
        let response = http_client
            .get(harness.http_url("/healthz"))
            .header("host", "api.test")
            .send()
            .await
            .unwrap();
        if response.status().as_u16() == 429 {
            assert!(
                response.headers().contains_key("retry-after"),
                "429 must carry a Retry-After header"
            );
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "burst of requests never hit the rate limit");

    test_app.shutdown();
}

#[tokio::test]
async fn ingress_serves_https_with_self_signed_cert() {
    let test_app = TestApp::start(TestAppMode::Healthy).await;
    let harness = IngressHarness::start_reaching(test_app.port()).await;

    let config = Config::parse(&format!(
        r#"
        [app.secure]
        image = "test:v1"
        port = {}

        [app.secure.ingress]
        host = "secure.test"
    "#,
        test_app.port()
    ))
    .unwrap();
    harness.client.apply(&config).await.unwrap();

    // Self-signed cert: the client must opt out of verification.
    let http_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let response = wait_for_status(
        &http_client,
        &harness.https_url("/healthz"),
        "secure.test",
        200,
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);

    test_app.shutdown();
}
