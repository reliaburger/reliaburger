//! Rolling-deploy, rollback and deploy-history cases.

use std::time::Duration;

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A redeploy never leaves the service with zero running backends (guards
/// H2): while the roll is in flight the cluster-wide running count stays
/// above zero, and afterwards every replica is back.
async fn rolling_deploy_keeps_the_app_running(ctx: TestContext) -> Result<(), String> {
    let app = "roll";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 2)).await?;
    ctx.wait_running_cluster(app, 2).await?;

    // Redeploy a changed-but-still-healthy spec and watch the running count as
    // it rolls. `slow` with a small delay stays comfortably inside the health
    // timeout, so a lost backend means a real gap, not a slow probe.
    let redeploy_ctx = ctx.clone();
    let redeploy_spec = ctx.testapp_spec_args(app, "slow", 2, &["--delay", "100"]);
    let redeploy = tokio::spawn(async move { redeploy_ctx.apply(&redeploy_spec).await });

    let mut min_running = u32::MAX;
    loop {
        let running = ctx
            .cluster_instances(app)
            .await
            .map(|v| v.iter().filter(|i| i.state == "running").count() as u32)
            .unwrap_or(0);
        min_running = min_running.min(running);
        if redeploy.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    redeploy
        .await
        .map_err(|error| format!("redeploy task panicked: {error}"))??;
    ctx.wait_running_cluster(app, 2).await?;

    if min_running == 0 {
        return Err("rolling deploy dropped to 0 running backends (guards H2)".to_string());
    }
    Ok(())
}

/// A deploy that never becomes healthy is rolled back automatically, leaving
/// the previous healthy version running.
async fn failed_deploy_rolls_back_automatically(ctx: TestContext) -> Result<(), String> {
    let app = "rollback";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 1)).await?;
    ctx.wait_running_cluster(app, 1).await?;

    // Unhealthy from the very first request (`--count 0`): this deploy can
    // never pass its health gate. The deploy call itself may error or return
    // after rolling back — either way the app must end up healthy again.
    let bad = ctx.testapp_spec_args(app, "unhealthy-after", 1, &["--count", "0"]);
    let _ = ctx.apply(&bad).await;

    ctx.wait_running_cluster(app, 1).await?;
    let running_only = ctx
        .cluster_instances(app)
        .await?
        .iter()
        .all(|i| i.state == "running");
    if !running_only {
        return Err("app did not return to a clean running state after auto-rollback".to_string());
    }
    Ok(())
}

/// After two deploys of an app, its history lists a completed deploy.
async fn deploy_history_records_each_version(ctx: TestContext) -> Result<(), String> {
    let app = "history";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 1)).await?;
    ctx.wait_running_cluster(app, 1).await?;
    ctx.apply(&ctx.testapp_spec_args(app, "slow", 1, &["--delay", "100"]))
        .await?;
    ctx.wait_running_cluster(app, 1).await?;

    let history = ctx
        .client
        .deploy_history(app, &ctx.namespace)
        .await
        .map_err(|error| format!("deploy history: {error}"))?;
    if history.is_empty() {
        return Err("deploy history is empty after two deploys".to_string());
    }
    // DeployResult serialises PascalCase; a successful deploy is "Completed".
    let has_completed = history
        .iter()
        .any(|entry| entry.get("result").and_then(|r| r.as_str()) == Some("Completed"));
    if !has_completed {
        return Err(format!("no Completed deploy in history: {history:?}"));
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "rolling_deploy_keeps_the_app_running",
            group: TestGroup::Deployments,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ProcessRuntime,
            ],
            run: testkit_case!(rolling_deploy_keeps_the_app_running),
        },
        TestCase {
            name: "failed_deploy_rolls_back_automatically",
            group: TestGroup::Deployments,
            requires: &[Capability::Cluster, Capability::ProcessRuntime],
            run: testkit_case!(failed_deploy_rolls_back_automatically),
        },
        TestCase {
            name: "deploy_history_records_each_version",
            group: TestGroup::Deployments,
            requires: &[Capability::Cluster, Capability::ProcessRuntime],
            run: testkit_case!(deploy_history_records_each_version),
        },
    ]
}
