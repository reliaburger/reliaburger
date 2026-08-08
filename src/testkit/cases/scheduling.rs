//! Scheduling and placement cases.

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::{TestCase, unknown};
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// Three replicas of one app land on at least two distinct nodes (guards the
/// H8 "all replicas on one node" regression), and every one is running.
async fn schedule_fixed_replicas_across_nodes(ctx: TestContext) -> Result<(), String> {
    let app = "sched-spread";
    ctx.apply(&ctx.testapp_spec(app, "healthy", 3)).await?;
    ctx.wait_running_cluster(app, 3).await?;

    let mut nodes_hosting = 0;
    for (_id, client) in ctx.node_clients().await? {
        let hosts_a_replica = client
            .status()
            .await
            .map(|statuses| {
                statuses.iter().any(|i| {
                    i.app_name == app && i.namespace == ctx.namespace && i.state == "running"
                })
            })
            .unwrap_or(false);
        if hosts_a_replica {
            nodes_hosting += 1;
        }
    }
    if nodes_hosting < 2 {
        return Err(format!(
            "3 replicas landed on {nodes_hosting} node(s); expected at least 2 (guards H8)"
        ));
    }
    Ok(())
}

/// An app with a required placement label runs only on a node carrying that
/// label. Skips if no node advertises a label to target.
async fn schedule_respects_required_placement_label(ctx: TestContext) -> Result<(), String> {
    let nodes = ctx
        .client
        .nodes()
        .await
        .map_err(|error| format!("could not list nodes: {error}"))?;
    let Some((target_node, key, value)) = nodes.iter().find_map(|node| {
        node.labels
            .iter()
            .next()
            .map(|(k, v)| (node.node_id.clone(), k.clone(), v.clone()))
    }) else {
        return unknown("no node advertises a label to target");
    };

    let app = "sched-pinned";
    let spec = format!(
        "{}\n[app.{app}.placement]\nrequired = [\"{key}={value}\"]\n",
        ctx.testapp_spec(app, "healthy", 1),
    );
    ctx.apply(&spec).await?;
    ctx.wait_running_cluster(app, 1).await?;

    // The one replica must be on the node that carries the label.
    for (node_id, client) in ctx.node_clients().await? {
        let hosts = client
            .status()
            .await
            .map(|s| {
                s.iter()
                    .any(|i| i.app_name == app && i.namespace == ctx.namespace)
            })
            .unwrap_or(false);
        if hosts && node_id != target_node {
            return Err(format!(
                "app pinned to {key}={value} ran on {node_id}, not the labelled node {target_node}"
            ));
        }
    }
    Ok(())
}

/// A namespace with `max_apps = 1` rejects a second app; the first is
/// untouched.
async fn schedule_rejects_app_exceeding_namespace_quota(ctx: TestContext) -> Result<(), String> {
    let quota = format!(
        "[namespace.\"{ns}\"]\nmax_apps = 1\n\n{app}",
        ns = ctx.namespace,
        app = ctx.testapp_spec("quota-a", "healthy", 1),
    );
    ctx.apply(&quota).await?;
    ctx.wait_running_cluster("quota-a", 1).await?;

    match ctx.apply(&ctx.testapp_spec("quota-b", "healthy", 1)).await {
        Err(_) => {}
        Ok(()) => return Err("a second app was accepted despite max_apps = 1".to_string()),
    }

    // The first app must be untouched by the rejected second.
    ctx.wait_running_cluster("quota-a", 1).await?;
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "schedule_fixed_replicas_across_nodes",
            group: TestGroup::Scheduling,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ProcessRuntime,
            ],
            run: testkit_case!(schedule_fixed_replicas_across_nodes),
        },
        TestCase {
            name: "schedule_respects_required_placement_label",
            group: TestGroup::Scheduling,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ProcessRuntime,
            ],
            run: testkit_case!(schedule_respects_required_placement_label),
        },
        TestCase {
            name: "schedule_rejects_app_exceeding_namespace_quota",
            group: TestGroup::Scheduling,
            requires: &[Capability::Cluster, Capability::ProcessRuntime],
            run: testkit_case!(schedule_rejects_app_exceeding_namespace_quota),
        },
    ]
}
