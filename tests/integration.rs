//! Integration tests for Reliaburger.
//!
//! Tests here exercise the full system: deploying apps, health checking,
//! restart logic, and CLI output. Each test spins up a real Bun agent
//! with a ProcessGrill backend and interacts with it via the HTTP API.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::StreamExt;
use reliaburger::bun::agent::AgentCommand;
use reliaburger::bun::testapp::{TestApp, TestAppMode};
use reliaburger::config::Config;
use reliaburger::relish::client::{BunClient, LogOptions};
use reliaburger::relish::tui::app::{DetailTab, TuiApp, View};
use reliaburger::relish::tui::data::{DataProvider, HttpDataProvider};
use reliaburger::relish::tui::fixtures::render_to_string;
use reliaburger::relish::tui::msg::{DataUpdate, Msg};
use tokio::sync::mpsc;

#[path = "support/cluster_harness.rs"]
mod cluster_harness;
use cluster_harness::TestHarness;

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_check_healthy_app_transitions_to_running() {
    let test_app = TestApp::start(TestAppMode::Healthy).await;
    let harness = TestHarness::start().await;

    let config = TestHarness::config_for_test_app(test_app.port());
    harness.client.apply(&config).await.unwrap();

    let status = harness
        .wait_for_instance("testapp", Duration::from_secs(6), |status| {
            status.state == "running"
        })
        .await;
    assert_eq!(
        status.state, "running",
        "expected running after health check passes, got {}",
        status.state
    );

    test_app.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "wall-clock health probe acceptance; run with make test-slow"]
async fn health_check_failing_app_never_reaches_running() {
    let test_app = TestApp::start(TestAppMode::UnhealthyAfter(0)).await;
    let harness = TestHarness::start().await;

    let config = TestHarness::config_for_test_app(test_app.port());
    harness.client.apply(&config).await.unwrap();

    // This is intentionally a wall-clock acceptance test: wait for two
    // production-interval probes before asserting that readiness was denied.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let statuses = harness.client.status().await.unwrap();
    // Should still be in health-wait (never got healthy) or possibly restarted
    assert!(
        statuses[0].state == "health-wait" || statuses[0].state == "pending",
        "expected health-wait or pending, got {}",
        statuses[0].state
    );

    test_app.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_status_returns_deployed_worker() {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bun_client_reports_unreachable_agent() {
    // No agent running — BunClient pointing at a port nobody listens on
    let client = BunClient::new("http://127.0.0.1:1");
    let result = client.health().await;
    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_for_deployed_app() {
    let harness = TestHarness::start().await;

    harness
        .client
        .apply(&TestHarness::config_no_health())
        .await
        .unwrap();

    // ProcessGrill returns empty logs for sleep, but the call should succeed
    let result = harness
        .client
        .logs("worker", "default", &LogOptions::default())
        .await;
    assert!(result.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_empty_when_nothing_deployed() {
    let harness = TestHarness::start().await;

    let statuses = harness.client.status().await.unwrap();
    assert!(statuses.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    let status = harness
        .wait_for_instance("migrate", Duration::from_secs(3), |status| {
            status.state == "stopped"
        })
        .await;
    assert_eq!(
        status.state, "stopped",
        "expected stopped after successful job, got {}",
        status.state
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_events_endpoint_records_successful_deploy() {
    let harness = TestHarness::start().await;
    harness
        .client
        .apply(&TestHarness::config_no_health())
        .await
        .unwrap();

    let events = harness.client.events(20).await.unwrap();
    assert!(events.iter().any(|event| {
        event.kind == reliaburger::bun::events::EventKind::Deploy
            && event.app.as_deref() == Some("worker")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_jobs_endpoint_lists_supervisor_job_state() {
    let harness = TestHarness::start().await;
    let config = Config::parse(
        r#"
        [job.listed]
        image = "test:v1"
        command = ["echo", "done"]
    "#,
    )
    .unwrap();
    harness.client.apply(&config).await.unwrap();

    let jobs = harness.client.jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "listed");
    assert_eq!(jobs[0].namespace, "default");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_fetches_renders_and_navigates_real_api_data() {
    let harness = TestHarness::start().await;
    harness
        .client
        .apply(&TestHarness::config_no_health())
        .await
        .unwrap();
    let provider = HttpDataProvider {
        client: harness.client.clone(),
    };
    let mut app = TuiApp::new();
    app.update(Msg::Data(DataUpdate::Status(provider.status().await)));
    app.update(Msg::Data(DataUpdate::Nodes(provider.nodes().await)));
    app.update(Msg::Data(DataUpdate::Council(provider.council().await)));
    app.update(Msg::Data(DataUpdate::EventsSeed(
        provider.recent_events(100).await,
    )));

    let dashboard = render_to_string(&app, 120, 40);
    assert!(dashboard.contains("worker"));
    assert!(dashboard.contains("nodes"));

    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::NONE,
    )));
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(matches!(app.view(), View::AppDetail { app, .. } if app == "worker"));
    app.view_stack.pop();
    app.view_stack.push(View::AppDetail {
        app: "worker".into(),
        namespace: "default".into(),
        tab: DetailTab::Instances,
    });
    assert!(render_to_string(&app, 120, 40).contains("running"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_search_filters_and_jumps_to_app() {
    let mut app =
        TuiApp::with_test_data(reliaburger::relish::tui::fixtures::TestScenario::HealthyCluster);
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::NONE,
    )));
    for character in "web".chars() {
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::NONE,
        )));
    }
    assert!(app.search.results.iter().any(|hit| hit.name == "web"));
    assert!(app.search.results.iter().all(|hit| hit.name != "worker"));
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert!(matches!(app.view(), View::AppDetail { app, .. } if app == "web"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_websocket_events_replay_deploy_event() {
    let harness = TestHarness::start().await;
    harness
        .client
        .apply(&TestHarness::config_no_health())
        .await
        .unwrap();
    let mut socket = harness.client.ws_events().await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let event: reliaburger::bun::events::ClusterEvent =
        serde_json::from_str(frame.to_text().unwrap()).unwrap();
    assert_eq!(event.kind, reliaburger::bun::events::EventKind::Deploy);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_websocket_logs_replay_known_line() {
    let harness = TestHarness::start().await;
    let config = Config::parse(
        r#"
        [app.streamer]
        image = "test:v1"
        command = ["sh", "-c", "echo phase-thirteen-line; sleep 2"]
    "#,
    )
    .unwrap();
    harness.client.apply(&config).await.unwrap();
    harness
        .wait_for_logs("streamer", &LogOptions::default(), 1)
        .await;

    let mut socket = harness
        .client
        .ws_logs("streamer", "default", 100)
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(10), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(frame.to_text().unwrap().contains("phase-thirteen-line"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "production retry backoff acceptance; run with make test-slow"]
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

    // The backoff itself is covered with virtual time in unit tests. This
    // ignored acceptance test keeps one check of the production durations.
    let status = harness
        .wait_for_instance("broken", Duration::from_secs(15), |status| {
            status.state == "failed"
        })
        .await;
    assert_eq!(
        status.state, "failed",
        "expected failed after exhausting retries, got {}",
        status.state
    );
    assert!(
        status.restart_count > 0,
        "expected restart_count > 0, got {}",
        status.restart_count
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "wall-clock health timeout acceptance; run with make test-slow"]
async fn health_check_hang_stays_in_health_wait() {
    let test_app = TestApp::start(TestAppMode::Hang).await;
    let harness = TestHarness::start().await;

    let config = TestHarness::config_for_test_app(test_app.port());
    harness.client.apply(&config).await.unwrap();

    // Intentionally exercise one production 1s request timeout.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let statuses = harness.client.status().await.unwrap();
    assert_eq!(
        statuses[0].state, "health-wait",
        "expected health-wait when probes hang, got {}",
        statuses[0].state
    );

    test_app.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_includes_process_details() {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "wall-clock health restart acceptance; run with make test-slow"]
async fn health_check_triggers_restart() {
    // App goes unhealthy after 3 healthy responses, then stays unhealthy
    let test_app = TestApp::start(TestAppMode::UnhealthyAfter(3)).await;
    let harness = TestHarness::start().await;

    let config = TestHarness::config_for_test_app(test_app.port());
    harness.client.apply(&config).await.unwrap();

    let status = harness
        .wait_for_instance("testapp", Duration::from_secs(15), |status| {
            status.restart_count > 0
        })
        .await;
    assert!(
        status.restart_count > 0,
        "expected restart_count > 0, got {} (state: {})",
        status.restart_count,
        status.state
    );
    // The old bug: re-drive rejected the same-id create and left the instance
    // wedged in Preparing (old process leaked). A working re-drive tears down
    // the old container, re-creates, and re-starts — so the instance reaches a
    // live post-create state, never stuck in Preparing/Pending.
    assert!(
        matches!(
            status.state.as_str(),
            "running" | "health-wait" | "unhealthy"
        ),
        "restart re-drive stalled: instance stuck in {}",
        status.state
    );

    test_app.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    let options = LogOptions {
        tail: Some(1),
        ..LogOptions::default()
    };
    let result = harness.wait_for_logs("echoer", &options, 1).await;

    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 1, "expected 1 line, got: {result:?}");
    assert_eq!(lines[0], "line3");
}

/// X4 regression: `--grep` used to be parsed then silently discarded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_grep_filters_lines() {
    let harness = TestHarness::start().await;

    let config = Config::parse(
        r#"
        [app.grepper]
        image = "test:v1"
        command = ["sh", "-c", "echo alpha-one; echo beta-two; echo alpha-three"]
    "#,
    )
    .unwrap();

    harness.client.apply(&config).await.unwrap();
    let options = LogOptions {
        grep: Some("alpha".to_string()),
        ..LogOptions::default()
    };
    let result = harness.wait_for_logs("grepper", &options, 2).await;

    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 alpha lines, got: {result:?}");
    assert!(lines.iter().all(|l| l.contains("alpha")), "got: {result:?}");
}

/// X4: `--json-field key=value` keeps only matching JSON lines.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_json_field_filters_structured_lines() {
    let harness = TestHarness::start().await;

    let config = Config::parse(
        r#"
        [app.jsonlogger]
        image = "test:v1"
        command = ["sh", "-c", "echo '{\"level\":\"error\",\"msg\":\"boom\"}'; echo '{\"level\":\"info\",\"msg\":\"fine\"}'; echo plain-text"]
    "#,
    )
    .unwrap();

    harness.client.apply(&config).await.unwrap();
    let options = LogOptions {
        json_field: Some(("level".to_string(), "error".to_string())),
        ..LogOptions::default()
    };
    let result = harness.wait_for_logs("jsonlogger", &options, 1).await;

    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 1, "expected 1 error line, got: {result:?}");
    assert!(lines[0].contains("boom"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    harness
        .wait_for_instance("echoer2", Duration::from_secs(3), |status| {
            status.state == "stopped"
        })
        .await;

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
    tokio::time::timeout(Duration::from_secs(5), collect)
        .await
        .expect("follow stream did not close after the completed job");

    assert!(
        lines.len() >= 2,
        "expected at least 2 lines from follow, got: {lines:?}"
    );
    assert_eq!(lines[0], "follow-line1");
    assert_eq!(lines[1], "follow-line2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_nonexistent_app_returns_error() {
    let harness = TestHarness::start().await;

    let result = harness
        .client
        .exec("nope", "default", &["echo".to_string()])
        .await;

    assert!(result.is_err(), "expected error for nonexistent app");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

/// X3 regression: `relish rollback` used to only print advice. The
/// server now redeploys the previous successful spec, and the deploy
/// history carries the spec needed to do so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_redeploys_the_previous_spec() {
    let harness = TestHarness::start().await;
    let http = reqwest::Client::new();

    // Deploy v1, then v2 (a redeploy of the same app, new command).
    let v1 = Config::parse(
        r#"
        [app.svc]
        image = "test:v1"
        command = ["sh", "-c", "echo VERSION-ONE; sleep 300"]
    "#,
    )
    .unwrap();
    harness.client.apply(&v1).await.unwrap();

    let v2 = Config::parse(
        r#"
        [app.svc]
        image = "test:v2"
        command = ["sh", "-c", "echo VERSION-TWO; sleep 300"]
    "#,
    )
    .unwrap();
    harness.client.apply(&v2).await.unwrap();

    // History should hold both, with specs attached.
    let history: serde_json::Value = http
        .get(format!(
            "{}/v1/deploys/history/svc",
            harness.client.base_url()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = history["history"].as_array().unwrap();
    assert!(entries.len() >= 2, "expected v1 + v2 in history");
    assert!(
        entries.iter().all(|e| e["spec"].is_object()),
        "history entries must carry the spec for rollback"
    );

    // Roll back: redeploys v1's spec (test:v1).
    harness.client.rollback("svc", "default").await.unwrap();

    // The newest history entry is now v1 again.
    let history: serde_json::Value = http
        .get(format!(
            "{}/v1/deploys/history/svc",
            harness.client.base_url()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = history["history"].as_array().unwrap();
    let newest = entries.last().unwrap();
    assert_eq!(
        newest["image"].as_str(),
        Some("test:v1"),
        "rollback should have redeployed v1"
    );
}

/// Rollback with no prior version is a clean 404, not a panic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rollback_without_history_returns_not_found() {
    let harness = TestHarness::start().await;

    let config = Config::parse(
        r#"
        [app.only]
        image = "test:v1"
        command = ["sleep", "300"]
    "#,
    )
    .unwrap();
    harness.client.apply(&config).await.unwrap();

    // Only one deploy: nothing to roll back to.
    let err = harness
        .client
        .rollback("only", "default")
        .await
        .unwrap_err();
    match err {
        reliaburger::relish::RelishError::ApiError { status, .. } => {
            assert_eq!(status, 404);
        }
        other => panic!("expected 404 ApiError, got {other:?}"),
    }
}

/// L14 regression: a Kill fault used to only insert a registry entry —
/// it killed nothing. Now it actually sends SIGKILL to the instance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_fault_actually_kills_the_instance() {
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let harness = TestHarness::start().await;

    // Two replicas so killing one leaves a survivor — the replica-minimum
    // safety rail (M1) refuses a Kill that would take out a service's last
    // replica, so a single-replica victim can't be killed by design.
    let config = Config::parse(
        r#"
        [app.victim]
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
        replicas = 2
    "#,
    )
    .unwrap();
    harness.client.apply(&config).await.unwrap();

    // Wait until both replicas have live PIDs.
    let mut pids_before: Vec<u32> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        pids_before = harness
            .client
            .status()
            .await
            .unwrap()
            .iter()
            .filter(|s| s.app_name == "victim")
            .filter_map(|s| s.pid)
            .collect();
        if pids_before.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        pids_before.len(),
        2,
        "both victim replicas should have pids"
    );

    // Kill one of the two replicas — within the safety budget (one survives).
    let fault = FaultRequest {
        fault_type: FaultType::Kill { count: 1 },
        target_service: "victim".into(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(5),
        injected_by: "test".into(),
        reason: Some("kill test".into()),
        include_leader: false,
        override_safety: false,
    };
    harness.client.inject_fault(&fault).await.unwrap();

    // At least one of the original PIDs must be dead (the supervisor may
    // restart with a NEW pid, but the killed process is gone).
    let killed = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut ok = false;
        while tokio::time::Instant::now() < deadline {
            // kill -0 to an old pid: an error means it's gone.
            let any_dead = pids_before.iter().any(|pid| libc_kill(*pid as i32, 0) != 0);
            if any_dead {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        ok
    };
    assert!(
        killed,
        "no victim process among {pids_before:?} died after the Kill fault"
    );
}

/// L14: eBPF-only network faults are rejected honestly when eBPF isn't
/// loaded, instead of being recorded as active while injecting nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_fault_without_ebpf_is_rejected_not_faked() {
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let harness = TestHarness::start().await;
    let config = Config::parse(
        r#"
        [app.svc]
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
    "#,
    )
    .unwrap();
    harness.client.apply(&config).await.unwrap();
    harness
        .wait_for_instance("svc", Duration::from_secs(2), |status| {
            status.state == "running"
        })
        .await;

    let fault = FaultRequest {
        fault_type: FaultType::Delay {
            delay_ns: 100_000_000,
            jitter_ns: 0,
        },
        target_service: "svc".into(),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(5),
        injected_by: "test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
    };
    let result = harness.client.inject_fault(&fault).await;
    assert!(
        result.is_err(),
        "a network fault without eBPF should be rejected, not silently accepted"
    );

    // And it must NOT be recorded as an active fault.
    let active = harness.client.list_faults().await.unwrap();
    assert!(
        active.is_empty(),
        "rejected fault must not appear in the active list: {active:?}"
    );
}

// Minimal FFI to `kill(2)` for the liveness probe above (no extra crate).
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}
