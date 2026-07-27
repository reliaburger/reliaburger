//! Cluster-coordination cases: membership, council, and per-node health.
//!
//! These use only the cluster control-plane APIs, so they run on any runtime
//! (they don't deploy a workload) — no `ProcessRuntime` requirement.

use crate::bun::capabilities::Capability;
use crate::testkit::TestContext;
use crate::testkit::registry::TestCase;
use crate::testkit::report::TestGroup;
use crate::testkit_case;

/// Every node the cluster knows about reports itself alive, and the count
/// matches what capabilities advertised.
async fn all_nodes_report_alive(ctx: TestContext) -> Result<(), String> {
    let nodes = ctx
        .client
        .nodes()
        .await
        .map_err(|error| format!("could not list nodes: {error}"))?;
    if nodes.len() as u32 != ctx.capabilities.node_count {
        return Err(format!(
            "node count {} disagrees with capabilities' {}",
            nodes.len(),
            ctx.capabilities.node_count
        ));
    }
    for node in &nodes {
        if !node.state.eq_ignore_ascii_case("alive") {
            return Err(format!(
                "node {} is {}, not alive",
                node.node_id, node.state
            ));
        }
    }
    Ok(())
}

/// The council has exactly one leader and a formed membership.
async fn council_has_leader_and_quorum(ctx: TestContext) -> Result<(), String> {
    let council = ctx
        .client
        .council()
        .await
        .map_err(|error| format!("could not read council: {error}"))?;
    if council.leader.is_none() {
        return Err("council has no leader".to_string());
    }
    if council.members.is_empty() {
        return Err("council has no members".to_string());
    }
    Ok(())
}

/// Every member answers its own `/v1/health` directly.
async fn every_node_answers_health(ctx: TestContext) -> Result<(), String> {
    let clients = ctx.node_clients().await?;
    for (node_id, client) in clients {
        client
            .health()
            .await
            .map_err(|error| format!("node {node_id} did not answer health: {error}"))?;
    }
    Ok(())
}

pub fn cases() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "all_nodes_report_alive",
            group: TestGroup::ClusterCoordination,
            requires: &[Capability::Cluster, Capability::MultiNode],
            run: testkit_case!(all_nodes_report_alive),
        },
        TestCase {
            name: "council_has_leader_and_quorum",
            group: TestGroup::ClusterCoordination,
            requires: &[
                Capability::Cluster,
                Capability::Council,
                Capability::MultiNode,
            ],
            run: testkit_case!(council_has_leader_and_quorum),
        },
        TestCase {
            name: "every_node_answers_health",
            group: TestGroup::ClusterCoordination,
            requires: &[Capability::Cluster, Capability::MultiNode],
            run: testkit_case!(every_node_answers_health),
        },
    ]
}
