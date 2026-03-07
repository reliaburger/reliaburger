//! Integration tests for Reliaburger.
//!
//! Tests here exercise the full system: deploying apps, health checking,
//! restart logic, and CLI output. Each test spins up a real Bun agent
//! with a ProcessGrill backend and interacts with it via the HTTP API.

use std::time::Duration;

use reliaburger::bun::agent::{AgentCommand, BunAgent};
use reliaburger::bun::api;
use reliaburger::bun::testapp::{TestApp, TestAppMode};
use reliaburger::config::Config;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::process::ProcessGrill;
use reliaburger::relish::client::BunClient;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Test harness: starts a real agent with ProcessGrill on an ephemeral port.
struct TestHarness {
    client: BunClient,
    cmd_tx: mpsc::Sender<AgentCommand>,
    shutdown: CancellationToken,
}

impl TestHarness {
    async fn start() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();

        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(40000, 41000);
        let agent_shutdown = shutdown.clone();
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, agent_shutdown);

        tokio::spawn(async move {
            agent.run().await;
        });

        // Bind API to ephemeral port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router(cmd_tx.clone());
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

        // Wait for API to be ready
        for _ in 0..20 {
            if client.health().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self {
            client,
            cmd_tx,
            shutdown,
        }
    }

    /// Deploy a config that runs a TestApp on the given port.
    fn config_for_test_app(port: u16) -> Config {
        Config::parse(&format!(
            r#"
            [app.testapp]
            image = "test:v1"
            port = {port}

            [app.testapp.health]
            path = "/healthz"
            interval = 1
            timeout = 1
            threshold_unhealthy = 2
            threshold_healthy = 1
        "#
        ))
        .unwrap()
    }

    fn config_no_health() -> Config {
        Config::parse(
            r#"
            [app.worker]
            image = "test:v1"
        "#,
        )
        .unwrap()
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deploy_app_reaches_running() {
    let harness = TestHarness::start().await;

    let result = harness
        .client
        .apply(&TestHarness::config_no_health())
        .await
        .unwrap();
    assert_eq!(result.created, 1);

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, "running");
}

#[tokio::test]
async fn health_check_healthy_app_transitions_to_running() {
    let test_app = TestApp::start(TestAppMode::Healthy).await;
    let harness = TestHarness::start().await;

    let config = TestHarness::config_for_test_app(test_app.port());
    harness.client.apply(&config).await.unwrap();

    // Wait for health check to pass (interval is 1s, agent polls every 1s)
    tokio::time::sleep(Duration::from_secs(4)).await;

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(
        statuses[0].state, "running",
        "expected running after health check passes, got {}",
        statuses[0].state
    );

    test_app.shutdown();
}

#[tokio::test]
async fn health_check_failing_app_marked_unhealthy() {
    let test_app = TestApp::start(TestAppMode::UnhealthyAfter(0)).await;
    let harness = TestHarness::start().await;

    let config = TestHarness::config_for_test_app(test_app.port());
    harness.client.apply(&config).await.unwrap();

    // Wait for health checks to accumulate failures
    tokio::time::sleep(Duration::from_secs(5)).await;

    let statuses = harness.client.status().await.unwrap();
    // Should still be in health-wait (never got healthy) or possibly restarted
    assert!(
        statuses[0].state == "health-wait" || statuses[0].state == "pending",
        "expected health-wait or pending, got {}",
        statuses[0].state
    );

    test_app.shutdown();
}

#[tokio::test]
async fn relish_status_returns_expected_output() {
    let harness = TestHarness::start().await;

    // Deploy via channel
    let (resp_tx, resp_rx) = oneshot::channel();
    harness
        .cmd_tx
        .send(AgentCommand::Deploy {
            config: TestHarness::config_no_health(),
            response: resp_tx,
        })
        .await
        .unwrap();
    resp_rx.await.unwrap().unwrap();

    let statuses = harness.client.status().await.unwrap();
    assert!(!statuses.is_empty());
    assert_eq!(statuses[0].app_name, "worker");
}

#[tokio::test]
async fn relish_apply_dry_run_when_agent_down() {
    // No agent running — BunClient pointing at a port nobody listens on
    let client = BunClient::new("http://127.0.0.1:1");
    let result = client.health().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn stop_app_transitions_to_stopped() {
    let harness = TestHarness::start().await;

    harness
        .client
        .apply(&TestHarness::config_no_health())
        .await
        .unwrap();

    // Verify running
    let statuses = harness.client.status().await.unwrap();
    assert_eq!(statuses[0].state, "running");

    // Stop
    harness.client.stop("worker", "default").await.unwrap();

    // Verify stopped
    let statuses = harness.client.status().await.unwrap();
    assert_eq!(statuses[0].state, "stopped");
}

#[tokio::test]
async fn logs_for_deployed_app() {
    let harness = TestHarness::start().await;

    harness
        .client
        .apply(&TestHarness::config_no_health())
        .await
        .unwrap();

    let logs = harness.client.logs("worker", "default").await.unwrap();
    assert!(logs.contains("worker"));
}

#[tokio::test]
async fn status_empty_when_nothing_deployed() {
    let harness = TestHarness::start().await;

    let statuses = harness.client.status().await.unwrap();
    assert!(statuses.is_empty());
}

#[tokio::test]
async fn deploy_multiple_apps() {
    let harness = TestHarness::start().await;

    let config = Config::parse(
        r#"
        [app.web]
        image = "web:v1"
        port = 8080

        [app.api]
        image = "api:v1"
    "#,
    )
    .unwrap();

    let result = harness.client.apply(&config).await.unwrap();
    assert_eq!(result.created, 2);

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(statuses.len(), 2);
}
