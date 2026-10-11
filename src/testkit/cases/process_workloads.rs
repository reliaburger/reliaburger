//! Process-workload cases: apps and jobs that run as host processes.

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A plain process-workload app reaches Running.
async fn process_workload_reports_running(ctx: TestContext) -> Result<(), String> {
    let app = "proc-app";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 1)).await?;
    ctx.wait_running_cluster(app, 1).await
}

/// A job that exits zero ends in the stopped state with a success exit code.
async fn process_job_with_exit_zero_succeeds(ctx: TestContext) -> Result<(), String> {
    let job = "job-ok";
    ctx.apply(&ctx.process_job_spec(job, &["/bin/sh", "-c", "exit 0"]))
        .await?;
    // A run is "stopped" only once its task exited zero; one that failed
    // says "failed".
    ctx.wait_for_job_runs(job, "a run stopped with exit 0", |rows| {
        rows.iter()
            .any(|row| row.instance_id.starts_with("run-") && row.state == "stopped")
    })
    .await
}

/// A job that keeps exiting non-zero eventually reaches the terminal failed
/// state (retry-count is not TOML-configurable, so this rides the run's
/// default attempts).
async fn failing_job_retries_then_fails(ctx: TestContext) -> Result<(), String> {
    let job = "job-fail";
    ctx.apply(&ctx.process_job_spec(job, &["/bin/sh", "-c", "exit 1"]))
        .await?;
    ctx.wait_for_job_runs(job, "a failed run", |rows| {
        rows.iter()
            .any(|row| row.instance_id.starts_with("run-") && row.state == "failed")
    })
    .await
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "process_workload_reports_running",
            group: TestGroup::ProcessWorkloads,
            requires: &[Capability::ProcessRuntime],
            run: testkit_case!(process_workload_reports_running),
        },
        TestCase {
            name: "process_job_with_exit_zero_succeeds",
            group: TestGroup::ProcessWorkloads,
            requires: &[Capability::ProcessRuntime],
            run: testkit_case!(process_job_with_exit_zero_succeeds),
        },
        TestCase {
            name: "failing_job_retries_then_fails",
            group: TestGroup::ProcessWorkloads,
            requires: &[Capability::ProcessRuntime],
            run: testkit_case!(failing_job_retries_then_fails),
        },
    ]
}
