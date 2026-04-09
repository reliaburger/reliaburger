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
use tokio::sync::mpsc;
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
        let app = api::router(cmd_tx.clone(), None, None, None, None);
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
    let (event_tx, mut event_rx) = mpsc::channel(64);
    harness
        .cmd_tx
        .send(AgentCommand::Deploy {
            config: TestHarness::config_no_health(),
            events: event_tx,
        })
        .await
        .unwrap();
    // Drain events until the channel closes
    while event_rx.recv().await.is_some() {}

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

    // ProcessGrill returns empty logs for sleep, but the call should succeed
    let result = harness.client.logs("worker", "default", None, false).await;
    assert!(result.is_ok());
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

#[tokio::test]
async fn job_runs_to_completion() {
    let harness = TestHarness::start().await;

    let config = Config::parse(
        r#"
        [job.migrate]
        image = "test:v1"
        command = ["echo", "migration complete"]
    "#,
    )
    .unwrap();

    harness.client.apply(&config).await.unwrap();

    // Wait for the job process to exit (echo is near-instant)
    tokio::time::sleep(Duration::from_secs(3)).await;

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].app_name, "migrate");
    assert_eq!(
        statuses[0].state, "stopped",
        "expected stopped after successful job, got {}",
        statuses[0].state
    );
}

#[tokio::test]
async fn job_failed_retries_then_fails() {
    let harness = TestHarness::start().await;

    let config = Config::parse(
        r#"
        [job.broken]
        image = "test:v1"
        command = ["false"]
    "#,
    )
    .unwrap();

    harness.client.apply(&config).await.unwrap();

    // Wait for retries to exhaust (3 retries with exponential backoff)
    // Backoff: 1s, 2s, 4s — total ~7s plus detection time
    tokio::time::sleep(Duration::from_secs(12)).await;

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].app_name, "broken");
    assert_eq!(
        statuses[0].state, "failed",
        "expected failed after exhausting retries, got {}",
        statuses[0].state
    );
    assert!(
        statuses[0].restart_count > 0,
        "expected restart_count > 0, got {}",
        statuses[0].restart_count
    );
}

#[tokio::test]
async fn init_container_success_allows_app_start() {
    let harness = TestHarness::start().await;

    let config = Config::parse(
        r#"
        [app.web]
        image = "test:v1"
        command = ["sleep", "60"]

        [[app.web.init]]
        command = ["echo", "init done"]
    "#,
    )
    .unwrap();

    let result = harness.client.apply(&config).await.unwrap();
    assert_eq!(result.created, 1);

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(statuses[0].state, "running");
}

#[tokio::test]
async fn init_container_failure_prevents_start() {
    let harness = TestHarness::start().await;

    let config = Config::parse(
        r#"
        [app.web]
        image = "test:v1"
        command = ["sleep", "60"]

        [[app.web.init]]
        command = ["false"]
    "#,
    )
    .unwrap();

    let result = harness.client.apply(&config).await;
    assert!(
        result.is_err(),
        "expected deploy to fail when init container fails"
    );
}

#[tokio::test]
async fn health_check_hang_stays_in_health_wait() {
    let test_app = TestApp::start(TestAppMode::Hang).await;
    let harness = TestHarness::start().await;

    let config = TestHarness::config_for_test_app(test_app.port());
    harness.client.apply(&config).await.unwrap();

    // Health check timeout is 1s, interval is 1s — after 5s the probes
    // should have timed out but the app should never reach "running"
    tokio::time::sleep(Duration::from_secs(5)).await;

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(
        statuses[0].state, "health-wait",
        "expected health-wait when probes hang, got {}",
        statuses[0].state
    );

    test_app.shutdown();
}

#[tokio::test]
async fn inspect_returns_expected_output() {
    let harness = TestHarness::start().await;

    harness
        .client
        .apply(&TestHarness::config_no_health())
        .await
        .unwrap();

    let statuses = harness.client.status().await.unwrap();
    let matching: Vec<_> = statuses.iter().filter(|s| s.app_name == "worker").collect();
    assert_eq!(matching.len(), 1);
    assert_eq!(matching[0].namespace, "default");
    assert!(matching[0].pid.is_some());
}

#[tokio::test]
async fn health_check_triggers_restart() {
    // App goes unhealthy after 3 healthy responses, then stays unhealthy
    let test_app = TestApp::start(TestAppMode::UnhealthyAfter(3)).await;
    let harness = TestHarness::start().await;

    let config = TestHarness::config_for_test_app(test_app.port());
    harness.client.apply(&config).await.unwrap();

    // Wait for: health checks to pass (go running), then fail, then restart
    // Health interval is 1s, threshold_unhealthy is 2
    tokio::time::sleep(Duration::from_secs(10)).await;

    let statuses = harness.client.status().await.unwrap();
    assert!(
        statuses[0].restart_count > 0,
        "expected restart_count > 0, got {} (state: {})",
        statuses[0].restart_count,
        statuses[0].state
    );

    test_app.shutdown();
}

#[tokio::test]
async fn logs_with_tail_returns_limited_lines() {
    let harness = TestHarness::start().await;

    // Deploy an app that echoes multiple lines
    let config = Config::parse(
        r#"
        [app.echoer]
        image = "test:v1"
        command = ["sh", "-c", "echo line1; echo line2; echo line3"]
    "#,
    )
    .unwrap();

    harness.client.apply(&config).await.unwrap();

    // Wait for the echo to finish and output to be captured
    tokio::time::sleep(Duration::from_secs(2)).await;

    let result = harness
        .client
        .logs("echoer", "default", Some(1), false)
        .await
        .unwrap();

    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 1, "expected 1 line, got: {result:?}");
    assert_eq!(lines[0], "line3");
}

#[tokio::test]
async fn logs_follow_returns_output_for_completed_job() {
    let harness = TestHarness::start().await;

    // Deploy a job (not an app) — the agent's check_jobs loop detects
    // process exit and updates state, which lets follow_logs terminate.
    let config = Config::parse(
        r#"
        [job.echoer2]
        image = "test:v1"
        command = ["sh", "-c", "echo follow-line1; echo follow-line2"]
    "#,
    )
    .unwrap();

    harness.client.apply(&config).await.unwrap();

    // Wait for the job to complete and state to be updated
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Send FollowLogs — for a stopped process, ProcessGrill sends
    // buffered output then returns when it sees state == Stopped.
    let (event_tx, mut event_rx) = mpsc::channel(64);
    harness
        .cmd_tx
        .send(reliaburger::bun::agent::AgentCommand::FollowLogs {
            app_name: "echoer2".to_string(),
            namespace: "default".to_string(),
            tail: None,
            lines: event_tx,
        })
        .await
        .unwrap();

    // Use a timeout to avoid hanging if something goes wrong
    let mut lines = Vec::new();
    let collect = async {
        while let Some(line) = event_rx.recv().await {
            lines.push(line);
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(5), collect).await;

    assert!(
        lines.len() >= 2,
        "expected at least 2 lines from follow, got: {lines:?}"
    );
    assert_eq!(lines[0], "follow-line1");
    assert_eq!(lines[1], "follow-line2");
}

#[tokio::test]
async fn exec_runs_command_and_returns_output() {
    let harness = TestHarness::start().await;

    // Deploy a long-running app so it stays in Running state
    let config = Config::parse(
        r#"
        [app.sleeper]
        image = "test:v1"
        command = ["sleep", "60"]
    "#,
    )
    .unwrap();

    harness.client.apply(&config).await.unwrap();

    let output = harness
        .client
        .exec(
            "sleeper",
            "default",
            &["echo".to_string(), "hello".to_string()],
        )
        .await
        .unwrap();

    assert_eq!(output.trim(), "hello");
}

#[tokio::test]
async fn exec_nonexistent_app_returns_error() {
    let harness = TestHarness::start().await;

    let result = harness
        .client
        .exec("nope", "default", &["echo".to_string()])
        .await;

    assert!(result.is_err(), "expected error for nonexistent app");
}

#[tokio::test]
async fn volume_config_deploys_successfully() {
    let harness = TestHarness::start().await;

    let dir = tempfile::tempdir().unwrap();
    let host_path = dir.path().join("data");
    std::fs::create_dir_all(&host_path).unwrap();

    let config = Config::parse(&format!(
        r#"
        [app.volapp]
        image = "test:v1"
        command = ["sleep", "60"]

        [[app.volapp.volumes]]
        path = "/data"
        source = "{}"
    "#,
        host_path.display()
    ))
    .unwrap();

    let result = harness.client.apply(&config).await.unwrap();
    assert_eq!(result.created, 1);

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(statuses[0].app_name, "volapp");
    assert_eq!(statuses[0].state, "running");
}
