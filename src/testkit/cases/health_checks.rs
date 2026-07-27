//! Health-check behaviour cases.

use std::time::Duration;

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// An app that turns unhealthy is restarted — and the restarted instance is a
/// live, re-created one, not a counter bump on a wedged instance (guards H1).
async fn unhealthy_app_is_restarted(ctx: TestContext) -> Result<(), String> {
    let app = "unhealthy";
    // Healthy for three requests, then 500 forever → the health check trips.
    ctx.apply(&ctx.testapp_spec_args(app, "unhealthy-after", 1, &["--count", "3"]))
        .await?;
    ctx.wait_for_cluster(app, "a restart into a live state", |instances| {
        let restarted = instances.iter().any(|i| i.restart_count >= 1);
        let live = instances
            .iter()
            .any(|i| matches!(i.state.as_str(), "running" | "health-wait" | "unhealthy"));
        // Both: the count rose AND something is genuinely re-created and
        // driving, never stuck in `preparing` (the H1 failure mode).
        restarted && live
    })
    .await
}

/// A health check against a hanging app marks it not-healthy without hanging
/// the agent — `/v1/health` still answers throughout (guards H3).
async fn hanging_health_check_marks_instance_unhealthy(ctx: TestContext) -> Result<(), String> {
    let app = "hang";
    ctx.apply(&ctx.testapp_spec(app, "hang", 1)).await?;
    ctx.wait_for_cluster(app, "unhealthy, failed, or restarted", |instances| {
        instances
            .iter()
            .any(|i| matches!(i.state.as_str(), "unhealthy" | "failed") || i.restart_count >= 1)
    })
    .await?;
    ctx.client
        .health()
        .await
        .map_err(|error| format!("agent stopped answering /v1/health (guards H3): {error}"))
}

/// An app that responds slowly but within the probe timeout keeps running and
/// does not flap.
async fn slow_but_within_timeout_app_stays_running(ctx: TestContext) -> Result<(), String> {
    let app = "slow";
    // Responds after 500 ms; the probe timeout is 2 s, so every probe passes.
    ctx.apply(&ctx.testapp_spec_args(app, "slow", 1, &["--delay", "500"]))
        .await?;
    ctx.wait_running_cluster(app, 1).await?;

    // Watch a while: a within-timeout app must not restart.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let instances = ctx.cluster_instances(app).await?;
    if instances.iter().any(|i| i.restart_count > 0) {
        return Err("a slow-but-within-timeout app flapped (restarted)".to_string());
    }
    if !instances.iter().any(|i| i.state == "running") {
        return Err("slow app is not running".to_string());
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "unhealthy_app_is_restarted",
            group: TestGroup::HealthChecks,
            requires: &[Capability::ProcessRuntime],
            run: testkit_case!(unhealthy_app_is_restarted),
        },
        TestCase {
            name: "hanging_health_check_marks_instance_unhealthy",
            group: TestGroup::HealthChecks,
            requires: &[Capability::ProcessRuntime],
            run: testkit_case!(hanging_health_check_marks_instance_unhealthy),
        },
        TestCase {
            name: "slow_but_within_timeout_app_stays_running",
            group: TestGroup::HealthChecks,
            requires: &[Capability::ProcessRuntime],
            run: testkit_case!(slow_but_within_timeout_app_stays_running),
        },
    ]
}
