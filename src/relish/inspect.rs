//! `relish inspect`: every instance of an app across the cluster.
//!
//! Collection asks each node for its own instances through the entry node's
//! relay (the same path `relish wtf` uses), so a node that doesn't answer
//! becomes a visible gap rather than silently shrinking the list. Rendering
//! is a pure function of what was collected.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::Duration;

use crate::bun::agent::{InstanceStatus, NodeStatus};
use crate::bun::diagnostics::DesiredAppEvidence;
use crate::relish::RelishError;
use crate::relish::client::BunClient;

/// How long one node may take to list its instances.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Name used for the node of a standalone agent, matching `/v1/status?cluster=true`.
const STANDALONE_NODE: &str = "local";

/// What one node said when asked for its instances.
#[derive(Debug, Clone)]
pub enum NodeAnswer {
    /// The node listed its instances.
    Answered(Vec<InstanceStatus>),
    /// The node should have answered but didn't.
    Silent {
        /// Why the request failed.
        reason: String,
    },
    /// Gossip has given up on the node (dead or left), so it wasn't asked.
    Down {
        /// Membership state as gossip reports it.
        state: String,
    },
}

/// One node's contribution to an inspection.
#[derive(Debug, Clone)]
pub struct NodeReport {
    /// Cluster node name, or `local` for a standalone agent.
    pub node: String,
    /// What the node said.
    pub answer: NodeAnswer,
}

/// Everything `relish inspect` knows about one app name.
#[derive(Debug, Clone)]
pub struct Inspection {
    /// The app name the operator asked about.
    pub name: String,
    /// Desired replicas for every namespace holding an app of that name, or
    /// why they couldn't be read.
    pub desired: Result<Vec<DesiredAppEvidence>, String>,
    /// One entry per node, in node order.
    pub nodes: Vec<NodeReport>,
}

/// Ask every node for its instances of `name`, plus the council for how many
/// there should be.
///
/// Only an unreachable entry node is an error; any other failure becomes part
/// of the returned [`Inspection`].
pub async fn collect(client: &BunClient, name: &str) -> Result<Inspection, RelishError> {
    let members = match bounded(client.nodes()).await {
        Ok(members) => members,
        Err(RelishError::AgentUnreachable) => return Err(RelishError::AgentUnreachable),
        // An agent without cluster membership (or one too old to list it)
        // can still describe its own instances.
        Err(_) => Vec::new(),
    };

    if members.is_empty() {
        let (desired, local) = tokio::join!(bounded(client.desired_apps()), async {
            bounded(client.status()).await
        });
        let answer = match local {
            Ok(instances) => NodeAnswer::Answered(instances),
            Err(RelishError::AgentUnreachable) => return Err(RelishError::AgentUnreachable),
            Err(error) => NodeAnswer::Silent {
                reason: describe_failure(&error),
            },
        };
        return Ok(Inspection {
            name: name.to_string(),
            desired: desired_for(desired, name),
            nodes: vec![NodeReport {
                node: STANDALONE_NODE.to_string(),
                answer,
            }],
        });
    }

    let (desired, nodes) = tokio::join!(
        desired_from_leader(client, &members),
        futures_util::future::join_all(members.iter().map(|member| ask_node(client, member))),
    );
    Ok(Inspection {
        name: name.to_string(),
        desired: desired_for(desired, name),
        nodes,
    })
}

/// Desired state lives with the council, so ask the leader when gossip names
/// one, and the entry node otherwise.
async fn desired_from_leader(
    client: &BunClient,
    members: &[NodeStatus],
) -> Result<Vec<DesiredAppEvidence>, RelishError> {
    let leader = members
        .iter()
        .find(|member| member.is_leader && !member.is_down())
        .and_then(|member| client.via_node(&member.node_id).ok());
    match leader {
        Some(leader) => bounded(leader.desired_apps()).await,
        None => bounded(client.desired_apps()).await,
    }
}

async fn ask_node(entry: &BunClient, member: &NodeStatus) -> NodeReport {
    let node = member.node_id.clone();
    if member.is_down() {
        return NodeReport {
            node,
            answer: NodeAnswer::Down {
                state: member.state.clone(),
            },
        };
    }
    let answer = match entry.via_node(&member.node_id) {
        Ok(relayed) => match bounded(relayed.status()).await {
            Ok(instances) => NodeAnswer::Answered(instances),
            Err(error) => NodeAnswer::Silent {
                reason: describe_failure(&error),
            },
        },
        Err(error) => NodeAnswer::Silent {
            reason: describe_failure(&error),
        },
    };
    NodeReport { node, answer }
}

async fn bounded<T>(
    future: impl Future<Output = Result<T, RelishError>>,
) -> Result<T, RelishError> {
    tokio::time::timeout(REQUEST_TIMEOUT, future)
        .await
        .unwrap_or(Err(RelishError::RequestTimeout))
}

fn desired_for(
    desired: Result<Vec<DesiredAppEvidence>, RelishError>,
    name: &str,
) -> Result<Vec<DesiredAppEvidence>, String> {
    desired
        .map(|apps| apps.into_iter().filter(|app| app.app == name).collect())
        .map_err(|error| describe_failure(&error))
}

/// A short reason for one failed request. A timeout here is ours, not the
/// agent's, so the generic "may still be running" wording would mislead.
fn describe_failure(error: &RelishError) -> String {
    match error {
        RelishError::RequestTimeout => format!("timed out after {}s", REQUEST_TIMEOUT.as_secs()),
        other => other.to_string(),
    }
}

/// Render an inspection for people: a replica summary per namespace, a line
/// for every node that didn't answer, then every instance with its node.
pub fn render(inspection: &Inspection) -> String {
    let name = &inspection.name;
    let instances: Vec<(&str, &InstanceStatus)> = inspection
        .nodes
        .iter()
        .filter_map(|report| match &report.answer {
            NodeAnswer::Answered(instances) => Some((report.node.as_str(), instances)),
            NodeAnswer::Silent { .. } | NodeAnswer::Down { .. } => None,
        })
        .flat_map(|(node, instances)| {
            instances
                .iter()
                .filter(|instance| instance.app_name == *name)
                .map(move |instance| (node, instance))
        })
        .collect();
    let gaps: Vec<String> = inspection
        .nodes
        .iter()
        .filter_map(|report| match &report.answer {
            NodeAnswer::Answered(_) => None,
            NodeAnswer::Silent { reason } => Some(format!(
                "node {} did not answer ({reason}); its instances are not listed",
                report.node
            )),
            NodeAnswer::Down { state } => Some(format!(
                "node {} is {state}; its instances are not listed",
                report.node
            )),
        })
        .collect();
    let desired = inspection.desired.as_deref().unwrap_or_default();

    let mut out = String::new();
    if instances.is_empty() && desired.is_empty() && gaps.is_empty() {
        let _ = writeln!(out, "no instances found for {name}");
        if let Err(reason) = &inspection.desired {
            let _ = writeln!(out, "desired replicas unknown: {reason}");
        }
        return out;
    }

    let namespaces: BTreeSet<&str> = desired
        .iter()
        .map(|app| app.namespace.as_str())
        .chain(
            instances
                .iter()
                .map(|(_, instance)| instance.namespace.as_str()),
        )
        .collect();
    for namespace in namespaces {
        let running = instances
            .iter()
            .filter(|(_, instance)| instance.namespace == namespace && instance.state == "running")
            .count();
        let _ = writeln!(out, "App: {name} (namespace {namespace})");
        let wanted = desired.iter().find(|app| app.namespace == namespace);
        match (wanted, &inspection.desired) {
            (Some(app), _) => {
                let _ = writeln!(
                    out,
                    "  Replicas:  {} desired, {running} running",
                    app.desired_replicas
                );
                if !app.placements.is_empty() {
                    let placed: Vec<String> = app
                        .placements
                        .iter()
                        .map(|(node, count)| format!("{node} {count}"))
                        .collect();
                    let _ = writeln!(out, "  Placed:    {}", placed.join(", "));
                }
            }
            (None, Err(reason)) => {
                let _ = writeln!(
                    out,
                    "  Replicas:  {running} running; desired unknown ({reason})"
                );
            }
            (None, Ok(_)) => {
                let _ = writeln!(
                    out,
                    "  Replicas:  {running} running; not in the desired state"
                );
            }
        }
    }
    for gap in &gaps {
        let _ = writeln!(out, "warning: {gap}");
    }
    out.push('\n');

    if instances.is_empty() {
        let _ = writeln!(out, "no instances found for {name}");
    }
    for (node, instance) in &instances {
        let _ = writeln!(out, "Instance: {}", instance.id);
        let _ = writeln!(out, "  Node:      {node}");
        let _ = writeln!(out, "  Namespace: {}", instance.namespace);
        let _ = writeln!(out, "  State:     {}", instance.state);
        let _ = writeln!(out, "  Restarts:  {}", instance.restart_count);
        if let Some(pid) = instance.pid {
            let _ = writeln!(out, "  PID:       {pid}");
        } else if instance.runtime_unknown {
            let _ = writeln!(out, "  PID:       unknown (the runtime was busy)");
        }
        if let Some(port) = instance.host_port {
            let _ = writeln!(out, "  Port:      {port}");
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::extract::Path;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Json, Router};

    use super::*;

    fn instance(id: &str, namespace: &str) -> InstanceStatus {
        InstanceStatus {
            id: id.to_string(),
            app_name: "hello".to_string(),
            namespace: namespace.to_string(),
            state: "running".to_string(),
            restart_count: 0,
            host_port: None,
            exit_code: None,
            pid: Some(100),
            runtime_unknown: false,
        }
    }

    fn member(node_id: &str, state: &str, is_leader: bool) -> NodeStatus {
        NodeStatus {
            node_id: node_id.to_string(),
            address: "127.0.0.1:7946".to_string(),
            api_address: None,
            state: state.to_string(),
            incarnation: 1,
            is_council: true,
            is_leader,
            labels: BTreeMap::new(),
        }
    }

    fn desired(replicas: u32, placements: &[(&str, u32)]) -> DesiredAppEvidence {
        DesiredAppEvidence {
            app: "hello".to_string(),
            namespace: "default".to_string(),
            desired_replicas: replicas,
            scheduled_replicas: replicas,
            placements: placements
                .iter()
                .map(|(node, count)| (node.to_string(), *count))
                .collect(),
            service_port: None,
        }
    }

    /// A three-node cluster's entry node (node-1). Its own `/v1/status`
    /// holds two of the three replicas, which is all the old `inspect` saw;
    /// node-2 holds the third, and node-3 answers every relay with 502.
    async fn three_node_entry(node_3_state: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let node_1 = vec![
            instance("default__hello-0", "default"),
            instance("default__hello-1", "default"),
        ];
        let node_2 = vec![instance("default__hello-2", "default")];
        let local = node_1.clone();
        let app = Router::new()
            .route(
                "/v1/status",
                get(move || async move { Json(local.clone()) }),
            )
            .route(
                "/v1/cluster/nodes",
                get(move || async move {
                    Json(vec![
                        member("node-1", "alive", true),
                        member("node-2", "alive", false),
                        member("node-3", node_3_state, false),
                    ])
                }),
            )
            .route(
                "/v1/nodes/{node}/relay/v1/diagnostics/apps",
                get(|Path(node): Path<String>| async move {
                    assert_eq!(node, "node-1", "desired state comes from the leader");
                    Json(vec![desired(3, &[("node-1", 2), ("node-2", 1)])])
                }),
            )
            .route(
                "/v1/nodes/{node}/relay/v1/status",
                get(move |Path(node): Path<String>| {
                    let node_1 = node_1.clone();
                    let node_2 = node_2.clone();
                    async move {
                        match node.as_str() {
                            "node-1" => Json(node_1).into_response(),
                            "node-2" => Json(node_2).into_response(),
                            _ => (StatusCode::BAD_GATEWAY, "no answer").into_response(),
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), server)
    }

    #[tokio::test]
    async fn every_replica_is_listed_with_its_node_not_just_the_local_ones() {
        let (url, server) = three_node_entry("alive").await;
        let inspection = collect(&BunClient::new(&url), "hello").await.unwrap();
        server.abort();

        let output = render(&inspection);
        assert!(
            output.contains("Replicas:  3 desired, 3 running"),
            "{output}"
        );
        for (id, node) in [
            ("default__hello-0", "node-1"),
            ("default__hello-1", "node-1"),
            ("default__hello-2", "node-2"),
        ] {
            assert!(
                output.contains(&format!("Instance: {id}\n  Node:      {node}\n")),
                "{id} on {node} missing:\n{output}"
            );
        }
    }

    #[tokio::test]
    async fn a_node_that_does_not_answer_is_named_instead_of_omitted() {
        let (url, server) = three_node_entry("alive").await;
        let inspection = collect(&BunClient::new(&url), "hello").await.unwrap();
        server.abort();

        let output = render(&inspection);
        assert!(
            output.contains("warning: node node-3 did not answer"),
            "{output}"
        );
        assert!(!output.contains("node node-1 did not answer"), "{output}");
    }

    #[tokio::test]
    async fn a_dead_node_is_marked_and_not_asked() {
        let (url, server) = three_node_entry("dead").await;
        let inspection = collect(&BunClient::new(&url), "hello").await.unwrap();
        server.abort();

        assert!(matches!(
            &inspection.nodes[2].answer,
            NodeAnswer::Down { state } if state == "dead"
        ));
        let output = render(&inspection);
        assert!(
            output.contains("warning: node node-3 is dead; its instances are not listed"),
            "{output}"
        );
    }

    #[test]
    fn missing_replicas_show_as_fewer_running_than_desired() {
        let inspection = Inspection {
            name: "hello".to_string(),
            desired: Ok(vec![desired(3, &[("node-1", 3)])]),
            nodes: vec![NodeReport {
                node: "node-1".to_string(),
                answer: NodeAnswer::Answered(vec![
                    instance("default__hello-0", "default"),
                    InstanceStatus {
                        state: "failed".to_string(),
                        ..instance("default__hello-1", "default")
                    },
                ]),
            }],
        };

        let output = render(&inspection);
        assert!(
            output.contains("Replicas:  3 desired, 1 running"),
            "{output}"
        );
        assert!(output.contains("Placed:    node-1 3"), "{output}");
    }

    #[test]
    fn unknown_desired_state_is_said_not_guessed() {
        let inspection = Inspection {
            name: "hello".to_string(),
            desired: Err("council unavailable".to_string()),
            nodes: vec![NodeReport {
                node: STANDALONE_NODE.to_string(),
                answer: NodeAnswer::Answered(vec![instance("default__hello-0", "default")]),
            }],
        };

        let output = render(&inspection);
        assert!(
            output.contains("Replicas:  1 running; desired unknown (council unavailable)"),
            "{output}"
        );
        assert!(output.contains("  Node:      local\n"), "{output}");
    }

    #[test]
    fn an_app_nobody_runs_or_wants_says_so() {
        let inspection = Inspection {
            name: "hello".to_string(),
            desired: Ok(Vec::new()),
            nodes: vec![NodeReport {
                node: "node-1".to_string(),
                answer: NodeAnswer::Answered(Vec::new()),
            }],
        };

        assert_eq!(render(&inspection), "no instances found for hello\n");
    }

    #[tokio::test]
    async fn a_standalone_agent_answers_for_itself() {
        let app = Router::new()
            .route(
                "/v1/status",
                get(|| async { Json(vec![instance("default__hello-0", "default")]) }),
            )
            .route(
                "/v1/cluster/nodes",
                get(|| async { Json(Vec::<NodeStatus>::new()) }),
            )
            .route(
                "/v1/diagnostics/apps",
                get(|| async { Json(vec![desired(1, &[])]) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let inspection = collect(&BunClient::new(&format!("http://{address}")), "hello")
            .await
            .unwrap();
        server.abort();

        let output = render(&inspection);
        assert!(
            output.contains("Replicas:  1 desired, 1 running"),
            "{output}"
        );
        assert!(output.contains("  Node:      local\n"), "{output}");
    }
}
