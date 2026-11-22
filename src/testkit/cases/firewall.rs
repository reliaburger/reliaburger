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

use std::collections::BTreeSet;
use std::time::Duration;

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::lease::LeaseScope;
use crate::testkit::registry::{CaseResult, TestCase, unknown};
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

/// Probe until `from_app` reaches `vip:port`, or the case's deadline ends.
///
/// A changed `allow_from` takes effect with the redeployed target's new
/// instance, and the old one, with the old rule, keeps the VIP healthy until
/// the rollout replaces it: up to about 40 s on the Wyse cluster.
/// Returns the last probe, reached or not, so a failure can show it.
async fn probe_until_reached(
    from: &TestContext,
    from_app: &str,
    vip: &str,
    port: u16,
) -> Result<Probe, String> {
    loop {
        let seen = probe(from, from_app, vip, port).await?;
        if seen.reached {
            return Ok(seen);
        }
        let waited = from
            .deadline
            .run(
                "allow_from change",
                tokio::time::sleep(Duration::from_secs(2)),
            )
            .await;
        if waited.is_err() {
            return Ok(seen);
        }
    }
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

/// Adding an app to `allow_from` and redeploying opens the connection,
/// once the change has reached the caller's node.
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
    let after = probe_until_reached(&peer, second, &vip, port).await?;
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

/// The nodes running a `running` instance of `app` in `ctx`'s namespace.
/// A node whose status can't be read is an error, not "not hosting": the
/// instance we're looking for may be exactly there.
async fn nodes_running(
    ctx: &TestContext,
    nodes: &[(String, crate::relish::client::BunClient)],
    app: &str,
) -> Result<BTreeSet<String>, String> {
    let mut hosting = BTreeSet::new();
    for (node_id, client) in nodes {
        let statuses = client
            .status()
            .await
            .map_err(|error| format!("could not inspect node {node_id}: {error}"))?;
        if statuses
            .iter()
            .any(|i| i.app_name == app && i.namespace == ctx.namespace && i.state == "running")
        {
            hosting.insert(node_id.clone());
        }
    }
    Ok(hosting)
}

/// A node that runs every caller and none of the target, if there is one.
fn node_apart(target_on: &BTreeSet<String>, callers_on: &[&BTreeSet<String>]) -> Option<String> {
    let (first, rest) = callers_on.split_first()?;
    first
        .iter()
        .find(|node| !target_on.contains(*node) && rest.iter().all(|on| on.contains(*node)))
        .cloned()
}

/// A listed caller on another node than the target reaches it, and an
/// unlisted one there is refused. Each caller's node enforces `allow_from`,
/// so it needs the target's list from the catalogue (#2216). The other
/// cases let the scheduler pack callers beside the target, which is how
/// they missed that remote callers were refused whatever the list said.
async fn allow_from_reaches_callers_on_other_nodes(ctx: TestContext) -> CaseResult {
    let peer = peer(&ctx)?;
    let nodes = ctx.node_clients().await?;
    if nodes.len() < 2 {
        return unknown("one node can't run a caller apart from the target");
    }
    let (target, listed, unlisted) = ("fw-target5", "fw-far", "fw-stranger");
    // One replica per node, so each caller runs beside the target and
    // away from it alike: replicas of one app spread across nodes. The
    // callers serve HTTP only to publish a port: a node whose containers
    // publish none has no masquerade rule yet, and its containers can't
    // reach another node at all, firewall or not (#2219).
    let everywhere = |app: &str| peer.container_http_spec(app, nodes.len() as u32);
    ctx.apply(&target_allowing(&ctx, target, &[qualified(&peer, listed)]))
        .await?;
    peer.apply(&everywhere(listed)).await?;
    peer.apply(&everywhere(unlisted)).await?;
    ctx.wait_running_cluster(target, 1).await?;
    peer.wait_running_cluster(listed, nodes.len() as u32)
        .await?;
    peer.wait_running_cluster(unlisted, nodes.len() as u32)
        .await?;
    let (vip, port) = healthy_vip(&ctx, target).await?;

    let target_on = nodes_running(&ctx, &nodes, target).await?;
    let listed_on = nodes_running(&peer, &nodes, listed).await?;
    let unlisted_on = nodes_running(&peer, &nodes, unlisted).await?;
    let Some(apart) = node_apart(&target_on, &[&listed_on, &unlisted_on]) else {
        return Err(format!(
            "no node runs both callers apart from the target, so this can't test a remote \
             caller: {target} on {target_on:?}, {listed} on {listed_on:?}, {unlisted} on {unlisted_on:?}"
        )
        .into());
    };
    let Some((_, client)) = nodes.iter().find(|(node_id, _)| *node_id == apart) else {
        return Err(format!("node {apart} left the listing").into());
    };
    let remote = TestContext {
        client: client.clone(),
        ..peer.clone()
    };

    // A healthy backend on the entry node doesn't mean the caller's node
    // has it yet: its view comes with the next catalogue.
    let seen = probe_until_reached(&remote, listed, &vip, port).await?;
    if !seen.reached {
        return Err(format!(
            "a listed caller on {apart}, away from the target on {target_on:?}, was refused: {}",
            seen.detail
        )
        .into());
    }
    let seen = probe(&remote, unlisted, &vip, port).await?;
    if seen.reached {
        return Err(format!(
            "an unlisted caller on {apart} reached the target despite allow_from: {}",
            seen.detail
        )
        .into());
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
        TestCase {
            name: "allow_from_reaches_callers_on_other_nodes",
            group: TestGroup::Firewall,
            requires: &[Capability::ContainerRuntime, Capability::Ebpf],
            lease: LeaseScope::Applications,
            peer_namespace: true,
            run: testkit_case!(allow_from_reaches_callers_on_other_nodes),
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

    #[test]
    fn a_remote_caller_runs_on_a_node_without_the_target() {
        let set = |nodes: &[&str]| nodes.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>();
        let target = set(&["a"]);
        assert_eq!(
            node_apart(&target, &[&set(&["a", "b", "c"]), &set(&["a", "c"])]),
            Some("c".to_string())
        );
        // Packed beside the target, or split across nodes: nothing to test.
        assert_eq!(node_apart(&target, &[&set(&["a"]), &set(&["a"])]), None);
        assert_eq!(node_apart(&target, &[&set(&["b"]), &set(&["c"])]), None);
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
