//! Run-to-completion job cases.

use crate::bun::capabilities::Capability;
use crate::relish::client::LogOptions;
use crate::testkit::TestContext;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A batch job runs to completion and reports a success exit.
async fn job_runs_to_completion_and_reports_exit(ctx: TestContext) -> Result<(), String> {
    let job = "batch";
    ctx.apply(&ctx.process_job_spec(job, &["/bin/sh", "-c", "echo done"]))
        .await?;
    ctx.wait_for_cluster(job, "stopped with exit 0", |instances| {
        instances
            .iter()
            .any(|i| i.state == "stopped" && i.exit_code == Some(0))
    })
    .await
}

/// A `schedule`d job fires on its own within a minute or so.
async fn scheduled_job_fires_on_its_schedule(ctx: TestContext) -> Result<(), String> {
    let job = "cron";
    // Every minute (the finest cron granularity). The default per-case timeout
    // gives it two windows to fire.
    let spec = format!(
        "[job.{job}]\n\
         image = \"proc-grill:image-ignored\"\n\
         command = [\"/bin/sh\", \"-c\", \"echo tick\"]\n\
         schedule = \"* * * * *\"\n\
         namespace = \"{ns}\"\n",
        ns = ctx.namespace,
    );
    ctx.apply(&spec).await?;
    // A fired schedule leaves at least one instance behind.
    ctx.wait_for_cluster(job, "at least one scheduled run", |instances| {
        !instances.is_empty()
    })
    .await
}

/// A job's stdout is retrievable from the log store after it finished (guards
/// H10 — container output actually reaches a log store).
async fn job_logs_are_retrievable_after_completion(ctx: TestContext) -> Result<(), String> {
    let job = "logged";
    ctx.apply(&ctx.process_job_spec(job, &["/bin/sh", "-c", "echo hello-from-job"]))
        .await?;
    ctx.wait_for_cluster(job, "stopped", |instances| {
        instances.iter().any(|i| i.state == "stopped")
    })
    .await?;

    let options = LogOptions {
        tail: Some(50),
        ..LogOptions::default()
    };
    let logs = ctx
        .client
        .logs(job, &ctx.namespace, &options)
        .await
        .map_err(|error| format!("could not fetch job logs: {error}"))?;
    if !logs.contains("hello-from-job") {
        return Err(format!("job stdout not found in logs:\n{logs}"));
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "job_runs_to_completion_and_reports_exit",
            group: TestGroup::Jobs,
            requires: &[Capability::ProcessRuntime],
            run: testkit_case!(job_runs_to_completion_and_reports_exit),
        },
        TestCase {
            name: "scheduled_job_fires_on_its_schedule",
            group: TestGroup::Jobs,
            requires: &[Capability::ProcessRuntime],
            run: testkit_case!(scheduled_job_fires_on_its_schedule),
        },
        TestCase {
            name: "job_logs_are_retrievable_after_completion",
            group: TestGroup::Jobs,
            requires: &[Capability::Logs, Capability::ProcessRuntime],
            run: testkit_case!(job_logs_are_retrievable_after_completion),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn completion_case(exit_code: Option<i32>) -> Result<(), String> {
        let router = axum::Router::new()
            .route(
                "/v1/apply",
                axum::routing::post(|| async {
                    format!(
                        "data: {}\n\n",
                        serde_json::to_string(&crate::bun::agent::ApplyEvent::Complete {
                            created: 1,
                            instances: vec![]
                        })
                        .unwrap()
                    )
                }),
            )
            .route(
                "/v1/status",
                axum::routing::get(move || async move {
                    axum::Json(serde_json::json!([{
                        "id":"batch-0", "app_name":"batch", "namespace":"rbtest-node-exit",
                        "state":"stopped", "restart_count":0, "host_port":null,
                        "exit_code": exit_code,
                    }]))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let timeout = Duration::from_millis(150);
        let context = TestContext {
            client: crate::relish::client::BunClient::new_with_token(
                &format!("http://{address}"),
                None,
            ),
            namespace: "rbtest-node-exit".into(),
            lease_id: Some("node-jobs-exit".into()),
            chaos_guard: crate::testkit::chaos::ChaosGuard::default(),
            capabilities: crate::bun::capabilities::ClusterCapabilities::default(),
            timeout,
            deadline: crate::testkit::deadline::Deadline::after(timeout).unwrap(),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            job_runs_to_completion_and_reports_exit(context),
        )
        .await
        .unwrap();
        server.abort();
        let _ = server.await;
        result
    }

    #[tokio::test]
    async fn completion_case_requires_an_observed_zero_exit() {
        assert!(
            completion_case(None).await.is_err(),
            "missing exit evidence passed the catalogue"
        );
        assert!(
            completion_case(Some(7)).await.is_err(),
            "failed job passed the catalogue"
        );
        completion_case(Some(0)).await.unwrap();
    }
}
