//! Shared in-process cluster harness for integration tests.
//!
//! Starts a real `BunAgent` over `ProcessGrill` with its HTTP API on an
//! ephemeral port, and hands back a `BunClient` pointed at it. Everything a
//! test starts is owned here and torn down on `Drop`, including while
//! unwinding from a panic.
//!
//! Lives in `tests/support/` rather than inside one test file because
//! Phase 15's `relish test` machinery needs the same fixture: a real agent,
//! a real API, no cluster. Included with
//! `#[path = "support/cluster_harness.rs"] mod cluster_harness;`.
//!
//! Some helpers are used by one caller today and exist for the next; the
//! module allows dead code rather than making every consumer import
//! everything.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::agent::InstanceStatus;
use reliaburger::bun::agent::{AgentCommand, BunAgent};
use reliaburger::bun::api;
use reliaburger::config::Config;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::process::ProcessGrill;
use reliaburger::relish::client::{BunClient, LogOptions};
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Test harness: starts a real agent with ProcessGrill on an ephemeral port.
pub struct TestHarness {
    pub client: BunClient,
    pub cmd_tx: mpsc::Sender<AgentCommand>,
    shutdown: CancellationToken,
    agent_task: Option<JoinHandle<()>>,
    server_task: Option<JoinHandle<()>>,
}

impl TestHarness {
    pub async fn start() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();

        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(40000, 41000);
        let agent_shutdown = shutdown.clone();
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, agent_shutdown);
        let deploy_history = agent.deploy_history_handle();
        let event_store = Arc::new(RwLock::new(reliaburger::bun::events::EventStore::new()));
        agent.set_event_store(Arc::clone(&event_store));

        let agent_task = tokio::spawn(async move {
            agent.run().await;
        });

        // Bind API to ephemeral port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router(
            cmd_tx.clone(),
            None,
            None,
            Some(deploy_history),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            9117,
            Some(event_store),
        );
        let server_shutdown = shutdown.clone();

        let server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    server_shutdown.cancelled().await;
                })
                .await
                .ok();
        });

        let client = BunClient::new(&format!("http://127.0.0.1:{port}"));

        // Wait for API readiness. A bounded predicate reports a useful
        // failure instead of letting the first unrelated request fail.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            if client.health().await.is_ok() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "test API did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        Self {
            client,
            cmd_tx,
            shutdown,
            agent_task: Some(agent_task),
            server_task: Some(server_task),
        }
    }

    pub async fn wait_for_instance(
        &self,
        app_name: &str,
        timeout: Duration,
        predicate: impl Fn(&InstanceStatus) -> bool,
    ) -> InstanceStatus {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let statuses = self.client.status().await.unwrap();
            if let Some(status) = statuses
                .into_iter()
                .find(|status| status.app_name == app_name && predicate(status))
            {
                return status;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{app_name} did not reach the expected state before {timeout:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn wait_for_logs(
        &self,
        app_name: &str,
        options: &LogOptions,
        minimum_lines: usize,
    ) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(logs) = self.client.logs(app_name, "default", options).await
                && logs.lines().count() >= minimum_lines
            {
                return logs;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "{app_name} did not produce {minimum_lines} log line(s)"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Deploy a config that runs a TestApp on the given port.
    pub fn config_for_test_app(port: u16) -> Config {
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

    pub fn config_no_health() -> Config {
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

        // Every test in this file uses Tokio's multi-thread runtime. Waiting
        // here gives Bun time to stop ProcessGrill children before the runtime
        // disappears; this also runs while a test is unwinding after a panic.
        for mut task in [self.agent_task.take(), self.server_task.take()]
            .into_iter()
            .flatten()
        {
            let completed = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    tokio::time::timeout(Duration::from_secs(5), &mut task)
                        .await
                        .is_ok()
                })
            });
            if !completed {
                task.abort();
            }
        }
    }
}
