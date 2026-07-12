//! Integration tests for batch job submission (Phase 12, F1).
//!
//! A real agent + API router on an ephemeral port (the TestHarness
//! pattern), driving `/v1/batch` end to end: submit → schedule →
//! local dispatch → job completion → tracker summary. The remote
//! dispatch endpoint and the report callback are exercised directly —
//! the full leader→peer loop needs a live cluster (acceptance runbook).

use std::time::Duration;

use reliaburger::bun::agent::BunAgent;
use reliaburger::bun::api;
use reliaburger::config::Config;
use reliaburger::grill::{PortAllocator, ProcessGrill};
use reliaburger::relish::client::BunClient;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The internal node-to-node endpoints (`/v1/batch/run`, `/report`) require the
/// cluster service token; the harness configures one so the dispatch path (and
/// the direct tests below) can present it.
const TEST_SERVICE_TOKEN: &str = "rbrg_test_service_token";

struct Harness {
    client: BunClient,
    base_url: String,
    shutdown: CancellationToken,
}

impl Harness {
    async fn start() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();

        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(42000, 43000);
        let agent_shutdown = shutdown.clone();
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, agent_shutdown);
        let deploy_history = agent.deploy_history_handle();

        tokio::spawn(async move {
            agent.run().await;
        });

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
            Some(TEST_SERVICE_TOKEN.to_string()),
            None,
            None,
            None,
            9117,
            None,
        );
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
                .await
                .ok();
        });

        let base_url = format!("http://127.0.0.1:{port}");
        let client = BunClient::new(&base_url);
        for _ in 0..20 {
            if client.health().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Self {
            client,
            base_url,
            shutdown,
        }
    }

    /// Poll the batch summary until `done` or the deadline.
    async fn wait_done(&self, batch_id: u64, secs: u64) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let summary = self.client.batch_status(batch_id).await.unwrap();
            if summary["done"].as_bool().unwrap_or(false) {
                return summary;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "batch {batch_id} not done in {secs}s: {summary}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn jobs_from(toml: &str) -> std::collections::BTreeMap<String, reliaburger::config::job::JobSpec> {
    Config::parse(toml).unwrap().job
}

/// Roadmap (Phase 12): submit a batch of process jobs; all run to
/// completion and the tracker reports them done.
#[tokio::test]
async fn batch_of_proc_jobs_completes_locally() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.quick-1]
        image = "proc-grill:image-ignored"
        command = ["echo", "one"]

        [job.quick-2]
        image = "proc-grill:image-ignored"
        command = ["echo", "two"]

        [job.quick-3]
        image = "proc-grill:image-ignored"
        command = ["echo", "three"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
    assert_eq!(response["assigned"].as_u64(), Some(3));
    assert!(
        response["unschedulable"].as_array().unwrap().is_empty(),
        "single-node fallback capacity must schedule everything"
    );

    let batch_id = response["batch_id"].as_u64().unwrap();
    let summary = harness.wait_done(batch_id, 30).await;
    assert_eq!(summary["completed"].as_u64(), Some(3), "{summary}");
    assert_eq!(summary["failed"].as_u64(), Some(0), "{summary}");
}

/// A job that runs and exits non-zero exhausts its retries (jobs get
/// `RestartPolicy::for_job(3)`) and is reported failed — the tracker
/// reaches a terminal state rather than pending forever.
#[tokio::test]
async fn batch_failing_job_reports_failed() {
    let harness = Harness::start().await;

    let jobs = jobs_from(
        r#"
        [job.doomed]
        image = "proc-grill:image-ignored"
        command = ["sh", "-c", "exit 1"]
    "#,
    );
    let response = harness.client.submit_batch(&jobs).await.unwrap();
    let batch_id = response["batch_id"].as_u64().unwrap();

    // 3 retries with 1s/2s/4s backoff — well inside 60s.
    let summary = harness.wait_done(batch_id, 60).await;
    assert_eq!(summary["failed"].as_u64(), Some(1), "{summary}");
    assert_eq!(summary["completed"].as_u64(), Some(0), "{summary}");
}

/// An empty batch is a client error, not a mysterious empty success.
#[tokio::test]
async fn empty_batch_is_rejected() {
    let harness = Harness::start().await;
    let result = harness
        .client
        .submit_batch(&std::collections::BTreeMap::new())
        .await;
    assert!(result.is_err());
}

/// The node-to-node half of dispatch: `/v1/batch/run` accepts a job
/// group, runs it, and posts its completion report to the callback
/// URL. (The leader-side grouping that produces these requests is
/// covered by the submit tests; the full two-node loop runs in the
/// cluster acceptance runbook.)
#[tokio::test]
async fn batch_run_endpoint_runs_jobs_and_calls_back() {
    let submitter = Harness::start().await;
    let runner = Harness::start().await;

    let http = reqwest::Client::new();
    let run = serde_json::json!({
        "batch_id": 424242,
        "callback_base_url": submitter.base_url,
        "jobs": [{
            "name": "remote-1",
            "spec": { "image": "proc-grill:image-ignored", "command": ["echo", "remote"] },
        }],
    });
    let response = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&run)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);

    // The runner actually executes the job…
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let statuses = runner.client.status().await.unwrap();
        if statuses
            .iter()
            .any(|s| s.app_name == "remote-1" && s.state == "stopped")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "remote job never completed: {statuses:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // …and the report endpoint on the submitter accepts callbacks
    // (unknown batch ids are recorded as no-ops, by design — a leader
    // restart must not turn late reports into errors).
    let report = http
        .post(format!("{}/v1/batch/424242/report", submitter.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({ "job_name": "remote-1", "status": "completed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(report.status().as_u16(), 200);
}

/// JOB1: the internal `/v1/batch/run` and `/report` endpoints reject a caller
/// that is not the system principal, so a ReadOnly/anonymous caller cannot run
/// work or forge completion (and never reaches the service-token callback).
#[tokio::test]
async fn batch_internal_endpoints_require_the_system_principal() {
    let runner = Harness::start().await;
    let http = reqwest::Client::new();
    let run = serde_json::json!({
        "batch_id": 1,
        "jobs": [{
            "name": "x",
            "spec": { "image": "proc-grill:image-ignored", "command": ["echo", "x"] },
        }],
    });

    // No token at all → not the system principal → 403.
    let no_token = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .json(&run)
        .send()
        .await
        .unwrap();
    assert_eq!(no_token.status().as_u16(), 403);

    // A non-service bearer token is also refused.
    let wrong = http
        .post(format!("{}/v1/batch/run", runner.base_url))
        .bearer_auth("rbrg_not_the_service_token")
        .json(&run)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status().as_u16(), 403);

    // The report endpoint is equally guarded.
    let report = http
        .post(format!("{}/v1/batch/1/report", runner.base_url))
        .json(&serde_json::json!({ "job_name": "x", "status": "completed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(report.status().as_u16(), 403);
}
