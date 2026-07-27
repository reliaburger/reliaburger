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
            .any(|i| i.state == "stopped" && matches!(i.exit_code, Some(0) | None))
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
