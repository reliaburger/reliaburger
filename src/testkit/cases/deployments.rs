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

/// Both distinct deployed commands must appear in completed history records.
async fn deploy_history_records_each_version(ctx: TestContext) -> Result<(), String> {
    let app = "history";
    let first = ctx.testapp_spec(app, "healthy", 1);
    let second = ctx.testapp_spec_args(app, "slow", 1, &["--delay", "100"]);
    let first_command = crate::config::Config::parse(&first)
        .map_err(|error| error.to_string())?
        .app[app]
        .command
        .clone();
    let second_command = crate::config::Config::parse(&second)
        .map_err(|error| error.to_string())?
        .app[app]
        .command
        .clone();
    ctx.apply(&first).await?;
    ctx.wait_running_cluster(app, 1).await?;
    wait_for_recorded_versions(&ctx, app, std::slice::from_ref(&first_command)).await?;
    ctx.apply(&second).await?;
    ctx.wait_running_cluster(app, 1).await?;
    wait_for_recorded_versions(&ctx, app, &[first_command, second_command]).await
}

async fn wait_for_recorded_versions(
    ctx: &TestContext,
    app: &str,
    commands: &[Vec<String>],
) -> Result<(), String> {
    ctx.deadline
        .run("completed history for every deployed version", async {
            loop {
                let mut history = Vec::new();
                for (node, client) in ctx.node_clients().await? {
                    history.extend(
                        client
                            .deploy_history(app, &ctx.namespace)
                            .await
                            .map_err(|error| format!("deploy history on {node}: {error}"))?,
                    );
                }
                let complete = commands.iter().all(|command| {
                    history.iter().any(|entry| {
                        entry["result"] == "Completed"
                            && entry["spec"]["command"] == serde_json::json!(command)
                    })
                });
                if complete {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|error| error.to_string())?
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn run_history_case(record_every_version: bool) -> Result<(), String> {
        let recorded = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let writes = Arc::clone(&recorded);
        let reads = Arc::clone(&recorded);
        let router = axum::Router::new()
            .route("/v1/apply", axum::routing::post(move |body: String| {
                let writes = Arc::clone(&writes);
                async move {
                    let config = crate::config::Config::parse(&body).unwrap();
                    let mut history = writes.lock().await;
                    if history.is_empty() || record_every_version {
                        history.push(serde_json::json!({
                            "result": "Completed", "spec": config.app["history"]
                        }));
                    }
                    format!("data: {}\n\n", serde_json::to_string(
                        &crate::bun::agent::ApplyEvent::Complete { created: 1, instances: vec![] }
                    ).unwrap())
                }
            }))
            .route("/v1/cluster/nodes", axum::routing::get(|| async { axum::Json(serde_json::json!([])) }))
            .route("/v1/status", axum::routing::get(|| async { axum::Json(serde_json::json!([
                {"id":"history-0", "app_name":"history", "namespace":"rbtest-history-00", "state":"running", "restart_count":0,"host_port":null,"pid":null}
            ])) }))
            .route("/v1/deploys/history/history", axum::routing::get(move || {
                let reads = Arc::clone(&reads);
                async move { axum::Json(serde_json::json!({"history": reads.lock().await.clone()})) }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let timeout = Duration::from_millis(500);
        let context = TestContext {
            client: crate::relish::client::BunClient::new_with_token(
                &format!("http://{address}"),
                None,
            ),
            namespace: "rbtest-history-00".into(),
            lease_id: None,
            chaos_guard: crate::testkit::chaos::ChaosGuard::default(),
            capabilities: crate::bun::capabilities::ClusterCapabilities::default(),
            timeout,
            deadline: crate::testkit::deadline::Deadline::after(timeout).unwrap(),
            peer_route: crate::testkit::context::PeerRoute::Direct,
            wait_note: Default::default(),
        };
        let case = cases()
            .into_iter()
            .find(|case| case.name == "deploy_history_records_each_version")
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), (case.run)(context))
            .await
            .unwrap();
        server.abort();
        result.map_err(|error| error.to_string())
    }

    #[tokio::test]
    async fn history_case_rejects_a_second_version_missing_from_history() {
        assert!(
            run_history_case(false).await.is_err(),
            "one completed version must not prove both versions were recorded"
        );
    }

    #[tokio::test]
    async fn history_case_accepts_both_completed_versions() {
        run_history_case(true).await.unwrap();
    }
}
