//! Integration tests for Onion service discovery.
//!
//! Tests exercise the service map lifecycle through the Bun agent
//! and HTTP API: deploy registers services, resolve returns VIPs
//! and backends, stop unregisters.
//!
//! These tests use ProcessGrill (no eBPF, no Linux dependency).
//! eBPF-specific tests are gated behind RELIABURGER_EBPF_TESTS=1
//! and require Linux with root.

use std::time::Duration;

use reliaburger::bun::agent::BunAgent;
use reliaburger::bun::api;
use reliaburger::config::Config;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::process::ProcessGrill;
use reliaburger::onion::vip::VirtualIP;
use reliaburger::relish::client::BunClient;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Test harness: starts a real agent with ProcessGrill on an ephemeral port.
struct TestHarness {
    client: BunClient,
    shutdown: CancellationToken,
}

impl TestHarness {
    async fn start() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();

        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(42000, 43000);
        let agent_shutdown = shutdown.clone();
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, agent_shutdown);

        tokio::spawn(async move {
            agent.run().await;
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router(cmd_tx, None, None);
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

        Self { client, shutdown }
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn app_with_port_config() -> Config {
    Config::parse(
        r#"
        [app.redis]
        image = "test:v1"
        port = 6379
        command = ["sleep", "86400"]
    "#,
    )
    .unwrap()
}

fn app_without_port_config() -> Config {
    Config::parse(
        r#"
        [app.worker]
        image = "test:v1"
        command = ["sleep", "86400"]
    "#,
    )
    .unwrap()
}

fn multi_app_config() -> Config {
    Config::parse(
        r#"
        [app.redis]
        image = "test:v1"
        port = 6379
        command = ["sleep", "86400"]

        [app.web]
        image = "test:v1"
        port = 8080
        command = ["sleep", "86400"]

        [app.api]
        image = "test:v1"
        port = 3000
        command = ["sleep", "86400"]
    "#,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Resolve tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deploy_app_with_port_registers_in_service_map() {
    let harness = TestHarness::start().await;
    harness.client.apply(&app_with_port_config()).await.unwrap();

    // Give the agent a moment to process
    tokio::time::sleep(Duration::from_millis(200)).await;

    let info = harness.client.resolve("redis").await.unwrap();
    assert_eq!(info.app_name, "redis");
    assert_eq!(info.port, 6379);

    let expected_vip = VirtualIP::from_app_name("redis");
    assert_eq!(info.vip, expected_vip.to_string());
}

#[tokio::test]
async fn deploy_app_without_port_not_in_service_map() {
    let harness = TestHarness::start().await;
    harness
        .client
        .apply(&app_without_port_config())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let err = harness.client.resolve("worker").await.unwrap_err();
    // Should be a 404
    match err {
        reliaburger::relish::RelishError::ApiError { status, .. } => {
            assert_eq!(status, 404);
        }
        other => panic!("expected ApiError(404), got: {other:?}"),
    }
}

#[tokio::test]
async fn resolve_nonexistent_returns_not_found() {
    let harness = TestHarness::start().await;

    let err = harness.client.resolve("nonexistent").await.unwrap_err();
    match err {
        reliaburger::relish::RelishError::ApiError { status, .. } => {
            assert_eq!(status, 404);
        }
        other => panic!("expected ApiError(404), got: {other:?}"),
    }
}

#[tokio::test]
async fn deploy_app_with_port_has_backend() {
    let harness = TestHarness::start().await;
    harness.client.apply(&app_with_port_config()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let info = harness.client.resolve("redis").await.unwrap();
    // ProcessGrill apps get a backend after reaching Running
    assert_eq!(info.total_backends, 1);
    assert_eq!(info.backends[0].instance_id, "redis-0");
}

#[tokio::test]
async fn resolve_all_lists_deployed_services() {
    let harness = TestHarness::start().await;
    harness.client.apply(&multi_app_config()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let all = harness.client.resolve_all().await.unwrap();
    assert_eq!(all.len(), 3);

    let names: Vec<&str> = all.iter().map(|r| r.app_name.as_str()).collect();
    assert!(names.contains(&"redis"));
    assert!(names.contains(&"web"));
    assert!(names.contains(&"api"));
}

#[tokio::test]
async fn resolve_all_empty_when_nothing_deployed() {
    let harness = TestHarness::start().await;

    let all = harness.client.resolve_all().await.unwrap();
    assert!(all.is_empty());
}

#[tokio::test]
async fn stop_app_removes_from_service_map() {
    let harness = TestHarness::start().await;
    harness.client.apply(&app_with_port_config()).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Verify it's registered
    let info = harness.client.resolve("redis").await.unwrap();
    assert_eq!(info.app_name, "redis");

    // Stop it
    harness.client.stop("redis", "default").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Should be gone
    let err = harness.client.resolve("redis").await.unwrap_err();
    match err {
        reliaburger::relish::RelishError::ApiError { status, .. } => {
            assert_eq!(status, 404);
        }
        other => panic!("expected ApiError(404), got: {other:?}"),
    }
}

#[tokio::test]
async fn vip_is_deterministic_across_agents() {
    // Two independent agents should assign the same VIP to the same app name
    let harness1 = TestHarness::start().await;
    let harness2 = TestHarness::start().await;

    harness1
        .client
        .apply(&app_with_port_config())
        .await
        .unwrap();
    harness2
        .client
        .apply(&app_with_port_config())
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    let vip1 = harness1.client.resolve("redis").await.unwrap().vip;
    let vip2 = harness2.client.resolve("redis").await.unwrap().vip;

    assert_eq!(vip1, vip2);
}
