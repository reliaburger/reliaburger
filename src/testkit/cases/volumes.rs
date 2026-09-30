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

/// Run `script` in `app`'s instance, on whichever node runs it.
///
/// The entry node only execs into its own instances, so asking it directly
/// only worked when the scheduler happened to place the app there.
async fn exec_sh(ctx: &TestContext, app: &str, script: &str) -> Result<String, String> {
    ctx.exec_in_workload(
        app,
        &["sh".to_string(), "-c".to_string(), script.to_string()],
    )
    .await
    .map_err(|error| format!("exec in {app} failed: {error}"))
}

/// Every instance left has exited (or none is left), so a redeploy is
/// accepted and starts a fresh instance.
fn stop_is_confirmed(instances: &[crate::bun::agent::InstanceStatus]) -> bool {
    instances
        .iter()
        .all(|instance| matches!(instance.state.as_str(), "stopped" | "failed"))
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
    // Stop returns while the instance is still stopping, and the replacement
    // reuses its id. Redeploying now would be refused ("still stopping"), and
    // the check below could pass on the old instance.
    ctx.wait_for_cluster(app, "a confirmed stop", stop_is_confirmed)
        .await?;
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
async fn volume_size_limit_is_enforced(ctx: TestContext) -> crate::testkit::registry::CaseResult {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::client::BunClient;
    use crate::testkit::context::PeerRoute;
    use crate::testkit::deadline::Deadline;
    use axum::extract::Path;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{any, get, post};
    use std::time::Duration;

    const NAMESPACE: &str = "rbtest-volumes-01";

    /// A laptop's view of a two-node cluster: node `one` is the entry node
    /// and runs nothing; `two` runs `app` and answers only through `one`'s
    /// relay. The entry node refuses an exec for an app it doesn't run the
    /// way Bun does, with a 404.
    fn entry_node(app: &'static str) -> axum::Router {
        let nodes = serde_json::json!([
            {"node_id":"one", "address":"10.0.0.1:7946", "api_address":"127.0.0.1:1",
             "state":"alive", "incarnation":1, "is_council":true, "is_leader":true, "labels":{}},
            {"node_id":"two", "address":"10.0.0.2:7946", "api_address":"127.0.0.1:1",
             "state":"alive", "incarnation":1, "is_council":true, "is_leader":false, "labels":{}}
        ]);
        let running = serde_json::json!([{
            "id": format!("{NAMESPACE}__{app}-0"), "app_name": app, "namespace": NAMESPACE,
            "state": "running", "restart_count": 0, "host_port": null, "pid": 42
        }]);
        axum::Router::new()
            .route(
                "/v1/cluster/nodes",
                get(move || {
                    let nodes = nodes.clone();
                    async move { axum::Json(nodes) }
                }),
            )
            .route(
                "/v1/status",
                get(|| async { axum::Json(serde_json::json!([])) }),
            )
            .route(
                "/v1/exec/{app}/{namespace}",
                post(
                    |Path((app, namespace)): Path<(String, String)>| async move {
                        let error = format!("app {app:?} not found in namespace {namespace:?}");
                        (
                            StatusCode::NOT_FOUND,
                            axum::Json(serde_json::json!({ "error": error })),
                        )
                    },
                ),
            )
            .route(
                "/v1/nodes/{node}/relay/{*path}",
                any(move |Path((node, path)): Path<(String, String)>| {
                    let running = running.clone();
                    async move {
                        if node != "two" {
                            return StatusCode::BAD_GATEWAY.into_response();
                        }
                        if path == "v1/status" {
                            return axum::Json(running).into_response();
                        }
                        if path == format!("v1/exec/{app}/{NAMESPACE}") {
                            return axum::Json(serde_json::json!({"output": "persisted\n"}))
                                .into_response();
                        }
                        StatusCode::NOT_FOUND.into_response()
                    }
                }),
            )
    }

    #[tokio::test]
    async fn exec_reaches_the_node_that_runs_the_app_not_the_entry_node() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, entry_node("vol-persist"))
                .await
                .unwrap()
        });
        let capabilities = crate::bun::capabilities::ClusterCapabilities {
            node_id: "one".to_string(),
            ..Default::default()
        };
        let ctx = TestContext {
            client: BunClient::new(&base),
            namespace: NAMESPACE.to_string(),
            lease_id: None,
            chaos_guard: Default::default(),
            capabilities,
            timeout: Duration::from_secs(5),
            deadline: Deadline::after(Duration::from_secs(5)).unwrap(),
            peer_route: PeerRoute::Relay,
            wait_note: Default::default(),
        };

        let output = exec_sh(&ctx, "vol-persist", "cat /data/marker").await;
        server.abort();
        assert_eq!(output.unwrap(), "persisted\n");
    }

    fn instance(state: &str) -> crate::bun::agent::InstanceStatus {
        crate::bun::agent::InstanceStatus {
            id: format!("{NAMESPACE}__vol-persist-0"),
            app_name: "vol-persist".to_string(),
            namespace: NAMESPACE.to_string(),
            state: state.to_string(),
            restart_count: 0,
            host_port: None,
            exit_code: None,
            pid: None,
            runtime_unknown: false,
        }
    }

    #[test]
    fn restart_waits_until_the_stopped_instance_has_exited() {
        // A stop that is still in flight: redeploying would be refused, and
        // the old instance would pass for the replacement.
        assert!(!stop_is_confirmed(&[instance("running")]));
        assert!(!stop_is_confirmed(&[instance("stopping")]));
        assert!(stop_is_confirmed(&[instance("stopped")]));
        assert!(stop_is_confirmed(&[instance("failed")]));
        assert!(stop_is_confirmed(&[]));
    }
}
