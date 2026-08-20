//! Service-discovery cases: VIPs and backend registration.

use std::time::{Duration, Instant};

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::{TestCase, unknown};
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// A resolved service has a VIP and its healthy backends registered.
async fn resolve_returns_vip_and_healthy_backends(ctx: TestContext) -> Result<(), String> {
    let app = "svc-two";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 2)).await?;
    ctx.wait_running_cluster(app, 2).await?;

    // Discovery can lag the running transition, so poll to the deadline.
    let deadline = Instant::now() + ctx.timeout;
    loop {
        if let Ok(resolved) = ctx.client.resolve(app).await
            && !resolved.vip.is_empty()
            && resolved.healthy_backends >= 2
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("resolve never reported a VIP with 2 healthy backends".to_string());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Scaling an app up is reflected in its backend list.
async fn resolve_reflects_scale_up(ctx: TestContext) -> Result<(), String> {
    let app = "svc-scale";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 1)).await?;
    ctx.wait_running_cluster(app, 1).await?;
    // Scale to three.
    ctx.apply(&ctx.testapp_spec(app, "healthy", 3)).await?;

    let deadline = Instant::now() + ctx.timeout;
    loop {
        if let Ok(resolved) = ctx.client.resolve(app).await
            && resolved.healthy_backends >= 3
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("backends did not reach 3 after scaling up".to_string());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Stopping an app removes it from the backend list (no healthy backends, or
/// the service resolves to nothing at all).
async fn stopped_instance_leaves_the_backend_list(ctx: TestContext) -> Result<(), String> {
    let app = "svc-stop";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 1)).await?;
    ctx.wait_running_cluster(app, 1).await?;
    ctx.client
        .stop(app, &ctx.namespace)
        .await
        .map_err(|error| format!("stop failed: {error}"))?;

    let deadline = Instant::now() + ctx.timeout;
    loop {
        // The service vanishing from the catalogue entirely (404) is as good
        // as zero backends. Any error used to count, so a transport blip
        // passed this case while the backend was still routable — a blip is
        // now no verdict at all.
        let transport_error = match ctx.client.resolve(app).await {
            Ok(resolved) if resolved.healthy_backends == 0 => return Ok(()),
            Err(crate::relish::RelishError::ApiError { status: 404, .. }) => return Ok(()),
            Err(
                error @ (crate::relish::RelishError::AgentUnreachable
                | crate::relish::RelishError::RequestTimeout),
            ) => Some(error.to_string()),
            Err(error) => return Err(format!("resolve failed: {error}")),
            _ => None,
        };
        if Instant::now() >= deadline {
            return match transport_error {
                Some(error) => unknown(format!(
                    "could not resolve {app} ({error}); backend removal unproven"
                )),
                None => Err("a stopped app still has healthy backends".to_string()),
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "resolve_returns_vip_and_healthy_backends",
            group: TestGroup::ServiceDiscovery,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ProcessRuntime,
            ],
            run: testkit_case!(resolve_returns_vip_and_healthy_backends),
        },
        TestCase {
            name: "resolve_reflects_scale_up",
            group: TestGroup::ServiceDiscovery,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ProcessRuntime,
            ],
            run: testkit_case!(resolve_reflects_scale_up),
        },
        TestCase {
            name: "stopped_instance_leaves_the_backend_list",
            group: TestGroup::ServiceDiscovery,
            requires: &[Capability::Cluster, Capability::ProcessRuntime],
            run: testkit_case!(stopped_instance_leaves_the_backend_list),
        },
    ]
}
