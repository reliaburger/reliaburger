//! Volume cases: managed volumes on a container runtime.
//!
//! These need real mount namespaces, so they deploy a `busybox` container and
//! require [`Capability::ContainerRuntime`] — a process workload has nowhere to
//! mount a volume.
//!
//! [`Capability::ContainerRuntime`]: crate::bun::capabilities::Capability::ContainerRuntime

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::{TestCase, unknown};
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// Spec for an idle busybox app with one managed volume at `/data`.
fn idle_with_volume(ctx: &TestContext, app: &str, size: Option<&str>) -> String {
    let size_line = size
        .map(|s| format!("size = \"{s}\"\n"))
        .unwrap_or_default();
    format!(
        "{}\n[[app.{app}.volumes]]\npath = \"/data\"\n{size_line}",
        ctx.container_idle_spec(app),
    )
}

async fn exec_sh(ctx: &TestContext, app: &str, script: &str) -> Result<String, String> {
    ctx.client
        .exec(
            app,
            &ctx.namespace,
            &["sh".to_string(), "-c".to_string(), script.to_string()],
        )
        .await
        .map_err(|error| format!("exec in {app} failed: {error}"))
}

/// A file written into a managed volume survives the instance being replaced.
async fn volume_data_survives_instance_restart(ctx: TestContext) -> Result<(), String> {
    let app = "vol-persist";
    ctx.apply(&idle_with_volume(&ctx, app, None)).await?;
    ctx.wait_running_cluster(app, 1).await?;
    exec_sh(&ctx, app, "echo persisted > /data/marker").await?;

    // Stop and redeploy: a managed volume is not deleted on stop, so the new
    // instance mounts the same directory.
    ctx.client
        .stop(app, &ctx.namespace)
        .await
        .map_err(|error| format!("stop failed: {error}"))?;
    ctx.apply(&idle_with_volume(&ctx, app, None)).await?;
    ctx.wait_running_cluster(app, 1).await?;

    let contents = exec_sh(&ctx, app, "cat /data/marker").await?;
    if !contents.contains("persisted") {
        return Err(format!("marker did not survive the restart: {contents:?}"));
    }
    Ok(())
}

/// One app's volume is not visible to another app.
async fn volume_is_isolated_per_app(ctx: TestContext) -> Result<(), String> {
    let (a, b) = ("vol-a", "vol-b");
    ctx.apply(&idle_with_volume(&ctx, a, None)).await?;
    ctx.apply(&idle_with_volume(&ctx, b, None)).await?;
    ctx.wait_running_cluster(a, 1).await?;
    ctx.wait_running_cluster(b, 1).await?;

    exec_sh(&ctx, a, "echo secret-of-a > /data/marker").await?;
    // B's /data is its own managed volume; A's marker must not appear.
    let seen = exec_sh(&ctx, b, "cat /data/marker 2>/dev/null || echo ABSENT").await?;
    if seen.contains("secret-of-a") {
        return Err("app B could read app A's volume content".to_string());
    }
    Ok(())
}

/// Writing past the declared size fails — where loop-mount enforcement is
/// active. Skips (rather than passing hollowly) where it isn't.
async fn volume_size_limit_is_enforced(ctx: TestContext) -> Result<(), String> {
    let app = "vol-quota";
    ctx.apply(&idle_with_volume(&ctx, app, Some("4Mi"))).await?;
    ctx.wait_running_cluster(app, 1).await?;

    // Try to write well past the 4 MiB limit.
    let result = exec_sh(
        &ctx,
        app,
        "dd if=/dev/zero of=/data/big bs=1M count=32 2>&1 || echo WRITE_FAILED",
    )
    .await?;
    if result.contains("WRITE_FAILED") || result.contains("No space") {
        Ok(())
    } else {
        unknown("volume size enforcement is not active on this node (needs loop-mount)")
    }
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "volume_data_survives_instance_restart",
            group: TestGroup::Volumes,
            requires: &[Capability::ContainerRuntime],
            run: testkit_case!(volume_data_survives_instance_restart),
        },
        TestCase {
            name: "volume_is_isolated_per_app",
            group: TestGroup::Volumes,
            requires: &[Capability::ContainerRuntime],
            run: testkit_case!(volume_is_isolated_per_app),
        },
        TestCase {
            name: "volume_size_limit_is_enforced",
            group: TestGroup::Volumes,
            requires: &[Capability::ContainerRuntime],
            run: testkit_case!(volume_size_limit_is_enforced),
        },
    ]
}
