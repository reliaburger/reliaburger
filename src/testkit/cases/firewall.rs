//! Firewall cases: `allow_from` enforcement between apps.
//!
//! These need eBPF, both to route a VIP to a backend and to enforce the
//! per-app `allow_from` policy in the connect hook, so they require
//! [`Capability::Ebpf`] on top of a container runtime and skip where it's off.
//!
//! [`Capability::Ebpf`]: crate::bun::capabilities::Capability::Ebpf

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// An HTTP target app whose firewall allows exactly `allowed`.
fn target_allowing(ctx: &TestContext, app: &str, allowed: &[&str]) -> String {
    let list = allowed
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{}\n[app.{app}.firewall]\nallow_from = [{list}]\n",
        ctx.container_http_spec(app, 1),
    )
}

/// Whether `from_app` can reach `vip:port` over HTTP (via an in-container wget).
async fn can_reach(
    ctx: &TestContext,
    from_app: &str,
    vip: &str,
    port: u16,
) -> Result<bool, String> {
    let script =
        format!("wget -q -O- -T 3 http://{vip}:{port}/hostname && echo REACHED || echo BLOCKED");
    let out = ctx
        .client
        .exec(
            from_app,
            &ctx.namespace,
            &["sh".to_string(), "-c".to_string(), script],
        )
        .await
        .map_err(|error| format!("exec in {from_app} failed: {error}"))?;
    Ok(out.contains("REACHED"))
}

async fn target_vip(ctx: &TestContext, app: &str) -> Result<(String, u16), String> {
    let resolved = ctx
        .client
        .resolve(app)
        .await
        .map_err(|error| format!("resolve {app} failed: {error}"))?;
    Ok((resolved.vip, resolved.port))
}

/// An app listed in `allow_from` can connect to the target.
async fn allow_from_permits_listed_app(ctx: TestContext) -> Result<(), String> {
    let (target, allowed) = ("fw-target", "fw-allowed");
    ctx.apply(&target_allowing(&ctx, target, &[allowed]))
        .await?;
    ctx.apply(&ctx.container_idle_spec(allowed)).await?;
    ctx.wait_running_cluster(target, 1).await?;
    ctx.wait_running_cluster(allowed, 1).await?;

    let (vip, port) = target_vip(&ctx, target).await?;
    if !can_reach(&ctx, allowed, &vip, port).await? {
        return Err("an allowed app could not reach the target".to_string());
    }
    Ok(())
}

/// An app not listed in `allow_from` is denied.
async fn unlisted_app_is_denied(ctx: TestContext) -> Result<(), String> {
    let (target, allowed, blocked) = ("fw-target2", "fw-ok", "fw-blocked");
    ctx.apply(&target_allowing(&ctx, target, &[allowed]))
        .await?;
    ctx.apply(&ctx.container_idle_spec(blocked)).await?;
    ctx.wait_running_cluster(target, 1).await?;
    ctx.wait_running_cluster(blocked, 1).await?;

    let (vip, port) = target_vip(&ctx, target).await?;
    if can_reach(&ctx, blocked, &vip, port).await? {
        return Err("an unlisted app reached the target despite allow_from".to_string());
    }
    Ok(())
}

/// Adding an app to `allow_from` and redeploying opens the connection.
async fn firewall_reflects_allow_from_change(ctx: TestContext) -> Result<(), String> {
    let (target, first, second) = ("fw-target3", "fw-first", "fw-second");
    ctx.apply(&target_allowing(&ctx, target, &[first])).await?;
    ctx.apply(&ctx.container_idle_spec(second)).await?;
    ctx.wait_running_cluster(target, 1).await?;
    ctx.wait_running_cluster(second, 1).await?;

    let (vip, port) = target_vip(&ctx, target).await?;
    if can_reach(&ctx, second, &vip, port).await? {
        return Err("second app reached the target before being allowed".to_string());
    }
    // Now allow it and redeploy the target.
    ctx.apply(&target_allowing(&ctx, target, &[first, second]))
        .await?;
    ctx.wait_running_cluster(target, 1).await?;
    if !can_reach(&ctx, second, &vip, port).await? {
        return Err("second app still blocked after being added to allow_from".to_string());
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "allow_from_permits_listed_app",
            group: TestGroup::Firewall,
            requires: &[Capability::ContainerRuntime, Capability::Ebpf],
            run: testkit_case!(allow_from_permits_listed_app),
        },
        TestCase {
            name: "unlisted_app_is_denied",
            group: TestGroup::Firewall,
            requires: &[Capability::ContainerRuntime, Capability::Ebpf],
            run: testkit_case!(unlisted_app_is_denied),
        },
        TestCase {
            name: "firewall_reflects_allow_from_change",
            group: TestGroup::Firewall,
            requires: &[Capability::ContainerRuntime, Capability::Ebpf],
            run: testkit_case!(firewall_reflects_allow_from_change),
        },
    ]
}
