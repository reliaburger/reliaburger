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

    // The one replica must be on the labelled node — and only there. A node
    // whose status read fails is unknown evidence, not "not hosting": the
    // misplaced replica is likeliest to sit on exactly the node that errors,
    // and the old `unwrap_or(false)` let a placement violation pass green.
    let mut hosting: Vec<String> = Vec::new();
    for (node_id, client) in ctx.node_clients().await? {
        match client.status().await {
            Ok(statuses) => {
                if statuses
                    .iter()
                    .any(|i| i.app_name == app && i.namespace == ctx.namespace)
                {
                    hosting.push(node_id);
                }
            }
            Err(error) => {
                return unknown(format!(
                    "could not inspect node {node_id}: {error}; placement unproven"
                ));
            }
        }
    }
    if hosting.is_empty() {
        // wait_running_cluster saw it running, but no reachable node admits
        // to hosting it — the fan-out missed a node; no verdict either way.
        return unknown("the running replica was not visible on any reachable node");
    }
    if hosting != vec![target_node.clone()] {
        return Err(format!(
            "app pinned to {key}={value} must run only on the labelled node {target_node}, \
             found on {hosting:?}"
        ));
    }
    Ok(())
}

/// A namespace with `max_apps = 1` schedules only the first app; the second
/// is admitted to desired state but never acquires a placement, and the
/// first is untouched.
///
/// Quota is enforced at *scheduling* time — the leader's placement pass
/// skips an over-quota app and logs — not at apply time; there is no council
/// refusal reason for quota. The old case expected the second `apply` to
/// error, a rejection that doesn't exist, and then accepted *any* error
/// (including a network blip) as proof of enforcement.
async fn schedule_rejects_app_exceeding_namespace_quota(ctx: TestContext) -> Result<(), String> {
    let quota = format!(
        "[namespace.\"{ns}\"]\nmax_apps = 1\n\n{app}",
        ns = ctx.namespace,
        app = ctx.testapp_spec("quota-a", "healthy", 1),
    );
    ctx.apply(&quota).await?;
    ctx.wait_running_cluster("quota-a", 1).await?;

    // Admission succeeds — apply only writes desired state.
    ctx.apply(&ctx.testapp_spec("quota-b", "healthy", 1))
        .await?;

    // The scheduler must keep quota-b at zero scheduled replicas. Poll until
    // the evidence covers both apps, failing the moment quota-b is granted.
    let scheduled_replicas = |evidence: &[crate::bun::diagnostics::DesiredAppEvidence],
                              name: &str| {
        evidence
            .iter()
            .find(|e| e.app == name && e.namespace == ctx.namespace)
            .map(|e| e.scheduled_replicas)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let evidence = ctx
            .client
            .desired_apps()
            .await
            .map_err(|error| format!("could not read scheduling evidence: {error}"))?;
        if evidence.is_empty() {
            return unknown("no scheduling evidence available; quota enforcement unproven");
        }
        if let Some(granted) = scheduled_replicas(&evidence, "quota-b")
            && granted > 0
        {
            return Err(format!(
                "quota-b acquired {granted} scheduled replica(s) despite max_apps = 1"
            ));
        }
        // Wait until quota-a is scheduled and quota-b is visible in desired
        // state — then the scheduler has demonstrably considered both.
        if scheduled_replicas(&evidence, "quota-a").is_some_and(|n| n >= 1)
            && scheduled_replicas(&evidence, "quota-b").is_some()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return unknown(
                "scheduling evidence never covered both apps within 30s; \
                 quota enforcement unproven",
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    // Hold a settle window: a *late* grant to quota-b is exactly the bug.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let evidence = ctx
        .client
        .desired_apps()
        .await
        .map_err(|error| format!("could not re-read scheduling evidence: {error}"))?;
    if let Some(granted) = scheduled_replicas(&evidence, "quota-b")
        && granted > 0
    {
        return Err(format!(
            "quota-b was granted {granted} replica(s) after a settle window despite max_apps = 1"
        ));
    }

    // The first app must be untouched by the unschedulable second.
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
