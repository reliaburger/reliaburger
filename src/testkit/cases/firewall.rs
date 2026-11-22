//! Firewall cases: `allow_from` enforcement between apps.
//!
//! `allow_from` only opens cross-namespace paths: callers in the target's own
//! namespace are never restricted by it (`docs/manual/03_networking.md`). So
//! the callers these cases restrict run in the case's peer namespace and are
//! named as `namespace/app` (#2206), and one case pins the same-namespace rule.
//!
//! These need eBPF, both to route a VIP to a backend and to enforce the
//! policy in the connect hook, so they require [`Capability::Ebpf`] on top of
//! a container runtime and skip where it's off.
//!
//! [`Capability::Ebpf`]: crate::bun::capabilities::Capability::Ebpf

use std::time::Duration;

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::lease::LeaseScope;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// An HTTP target app whose firewall allows exactly `allowed`, each written
/// as `namespace/app`.
fn target_allowing(ctx: &TestContext, app: &str, allowed: &[String]) -> String {
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

/// The case's peer namespace, which every case here except the
/// same-namespace one declares.
fn peer(ctx: &TestContext) -> Result<TestContext, String> {
    ctx.peer()
        .ok_or_else(|| "the case has no peer namespace".to_string())
}

/// `app` in `ctx`'s namespace, as `allow_from` names it.
fn qualified(ctx: &TestContext, app: &str) -> String {
    format!("{}/{app}", ctx.namespace)
}

/// What one probe saw, for the verdict and for a failure message.
struct Probe {
    reached: bool,
    detail: String,
}

/// Whether `from_app`, in `from`'s namespace, can reach `vip:port` over HTTP
/// (an in-container wget).
async fn probe(from: &TestContext, from_app: &str, vip: &str, port: u16) -> Result<Probe, String> {
    let script = format!(
        "wget -q -O- -T 3 http://{vip}:{port}/hostname 2>&1 && echo REACHED || echo BLOCKED"
    );
    let out = from
        .client
        .exec(
            from_app,
            &from.namespace,
            &["sh".to_string(), "-c".to_string(), script],
        )
        .await
        .map_err(|error| format!("exec in {}/{from_app} failed: {error}", from.namespace))?;
    let output = out.output.trim().replace('\n', " ");
    Ok(Probe {
        reached: out.output.contains("REACHED"),
        detail: format!("{}/{from_app} -> {vip}:{port}: {output}", from.namespace),
    })
}

/// The target's VIP once it has a healthy backend. Waiting for the instance
/// to run isn't enough: on a slow node the service map lags it, and a probe
/// sent before then is refused for want of a backend, not by the firewall.
async fn healthy_vip(ctx: &TestContext, app: &str) -> Result<(String, u16), String> {
    loop {
        let last = match ctx.client.resolve(app).await {
            Ok(resolved) if !resolved.vip.is_empty() && resolved.healthy_backends > 0 => {
                return Ok((resolved.vip, resolved.port));
            }
            Ok(resolved) => format!(
                "{}/{} healthy backends",
                resolved.healthy_backends, resolved.total_backends
            ),
            Err(error) => error.to_string(),
        };
        ctx.deadline
            .run(
                "healthy backend",
                tokio::time::sleep(Duration::from_millis(500)),
            )
            .await
            .map_err(|error| format!("{app} never had a healthy backend ({last}): {error}"))?;
    }
}

/// An app listed in `allow_from` can connect to the target from another
/// namespace.
async fn allow_from_permits_listed_app(ctx: TestContext) -> Result<(), String> {
    let peer = peer(&ctx)?;
    let (target, allowed) = ("fw-target", "fw-allowed");
    ctx.apply(&target_allowing(&ctx, target, &[qualified(&peer, allowed)]))
        .await?;
    peer.apply(&peer.container_idle_spec(allowed)).await?;
    ctx.wait_running_cluster(target, 1).await?;
    peer.wait_running_cluster(allowed, 1).await?;

    let (vip, port) = healthy_vip(&ctx, target).await?;
    let seen = probe(&peer, allowed, &vip, port).await?;
    if !seen.reached {
        return Err(format!(
            "an allowed app could not reach the target: {}",
            seen.detail
        ));
    }
    Ok(())
}

/// An app in another namespace that `allow_from` doesn't list is denied.
async fn unlisted_app_is_denied(ctx: TestContext) -> Result<(), String> {
    let peer = peer(&ctx)?;
    let (target, allowed, blocked) = ("fw-target2", "fw-ok", "fw-blocked");
    ctx.apply(&target_allowing(&ctx, target, &[qualified(&peer, allowed)]))
        .await?;
    peer.apply(&peer.container_idle_spec(blocked)).await?;
    ctx.wait_running_cluster(target, 1).await?;
    peer.wait_running_cluster(blocked, 1).await?;

    let (vip, port) = healthy_vip(&ctx, target).await?;
    let seen = probe(&peer, blocked, &vip, port).await?;
    if seen.reached {
        return Err(format!(
            "an unlisted app reached the target despite allow_from: {}",
            seen.detail
        ));
    }
    Ok(())
}

/// Adding an app to `allow_from` and redeploying opens the connection.
async fn firewall_reflects_allow_from_change(ctx: TestContext) -> Result<(), String> {
    let peer = peer(&ctx)?;
    let (target, first, second) = ("fw-target3", "fw-first", "fw-second");
    ctx.apply(&target_allowing(&ctx, target, &[qualified(&peer, first)]))
        .await?;
    peer.apply(&peer.container_idle_spec(second)).await?;
    ctx.wait_running_cluster(target, 1).await?;
    peer.wait_running_cluster(second, 1).await?;

    let (vip, port) = healthy_vip(&ctx, target).await?;
    let before = probe(&peer, second, &vip, port).await?;
    if before.reached {
        return Err(format!(
            "second app reached the target before being allowed: {}",
            before.detail
        ));
    }
    // Now allow it and redeploy the target.
    ctx.apply(&target_allowing(
        &ctx,
        target,
        &[qualified(&peer, first), qualified(&peer, second)],
    ))
    .await?;
    ctx.wait_running_cluster(target, 1).await?;
    let (vip, port) = healthy_vip(&ctx, target).await?;
    let after = probe(&peer, second, &vip, port).await?;
    if !after.reached {
        return Err(format!(
            "second app still blocked after being added to allow_from: {}",
            after.detail
        ));
    }
    Ok(())
}

/// `allow_from` doesn't restrict callers in the target's own namespace: an
/// unlisted app there still connects.
async fn same_namespace_callers_ignore_allow_from(ctx: TestContext) -> Result<(), String> {
    let (target, listed, neighbour) = ("fw-target4", "fw-listed", "fw-neighbour");
    ctx.apply(&target_allowing(&ctx, target, &[qualified(&ctx, listed)]))
        .await?;
    ctx.apply(&ctx.container_idle_spec(neighbour)).await?;
    ctx.wait_running_cluster(target, 1).await?;
    ctx.wait_running_cluster(neighbour, 1).await?;

    let (vip, port) = healthy_vip(&ctx, target).await?;
    let seen = probe(&ctx, neighbour, &vip, port).await?;
    if !seen.reached {
        return Err(format!(
            "an app in the target's namespace was blocked by allow_from: {}",
            seen.detail
        ));
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "allow_from_permits_listed_app",
            group: TestGroup::Firewall,
            requires: &[Capability::ContainerRuntime, Capability::Ebpf],
            lease: LeaseScope::Applications,
            peer_namespace: true,
            run: testkit_case!(allow_from_permits_listed_app),
        },
        TestCase {
            name: "unlisted_app_is_denied",
            group: TestGroup::Firewall,
            requires: &[Capability::ContainerRuntime, Capability::Ebpf],
            lease: LeaseScope::Applications,
            peer_namespace: true,
            run: testkit_case!(unlisted_app_is_denied),
        },
        TestCase {
            name: "firewall_reflects_allow_from_change",
            group: TestGroup::Firewall,
            requires: &[Capability::ContainerRuntime, Capability::Ebpf],
            lease: LeaseScope::Applications,
            peer_namespace: true,
            run: testkit_case!(firewall_reflects_allow_from_change),
        },
        TestCase {
            name: "same_namespace_callers_ignore_allow_from",
            group: TestGroup::Firewall,
            requires: &[Capability::ContainerRuntime, Capability::Ebpf],
            lease: LeaseScope::Applications,
            peer_namespace: false,
            run: testkit_case!(same_namespace_callers_ignore_allow_from),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_targets_pass_the_validation_the_api_applies() {
        let ctx = TestContext::offline("rbtest-firewall-00");
        let peer = "rbtest-firewall-00-peer";
        for allowed in [
            vec![format!("{peer}/fw-client")],
            vec![format!("{peer}/fw-first"), format!("{peer}/fw-second")],
            vec![qualified(&ctx, "fw-listed")],
        ] {
            let spec = target_allowing(&ctx, "fw-target", &allowed);
            let config = crate::config::Config::parse(&spec).unwrap();
            if let Err(error) = config.validate_intrinsic() {
                panic!("the API would refuse this firewall target: {error}\n{spec}");
            }
        }
    }

    /// #2206: the cases that restrict a caller put it in another namespace,
    /// because `allow_from` ignores callers in the target's own.
    #[test]
    fn cases_that_restrict_callers_have_a_peer_namespace() {
        for case in cases() {
            let restricts = case.name != "same_namespace_callers_ignore_allow_from";
            assert_eq!(case.peer_namespace, restricts, "{}", case.name);
        }
    }
}
