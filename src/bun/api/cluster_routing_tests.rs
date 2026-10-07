//! Multi-node API tests: several real routers on loopback listeners, each
//! with a scripted agent, sharing one membership table. They exercise the
//! cross-node routing paths without starting gossip or Raft.

use super::faults::ClusterFaultList;
use super::nodes::relay_allows;
use super::*;
use crate::smoker::types::{FaultRequest, FaultSummary, FaultType, ReplicaEvidence};
use tokio_util::sync::CancellationToken;

const SERVICE_TOKEN: &str = "cluster-routing-internal";

type Injected = Arc<tokio::sync::Mutex<Vec<(FaultRequest, Option<ReplicaEvidence>)>>>;
/// The snapshot operations one fake agent carried out, as `"create db/a"`.
type SnapshotLog = Arc<tokio::sync::Mutex<Vec<String>>>;

struct FakeNode {
    url: String,
    injected: Injected,
    snapshots: SnapshotLog,
}

struct FakeCluster {
    nodes: Vec<FakeNode>,
    membership: Arc<RwLock<Vec<NodeMembershipInfo>>>,
    known: KnownMembers,
    operator: String,
    /// A read-only token confined to the `api` app.
    api_reader: String,
    stop: CancellationToken,
}

impl FakeCluster {
    /// List a member whose address has nothing listening, like a node
    /// that died before gossip noticed.
    async fn add_unreachable_member(&self, name: &str) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        self.membership.write().await.push(NodeMembershipInfo {
            node_id: crate::meat::NodeId::new(name),
            address,
            api_advertised: true,
        });
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, entry: usize, path: &str) -> T {
        reqwest::Client::new()
            .get(format!("{}{path}", self.nodes[entry].url))
            .bearer_auth(&self.operator)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}

impl Drop for FakeCluster {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn instance(id: &str, app: &str, state: &str) -> InstanceStatus {
    InstanceStatus {
        id: id.to_string(),
        app_name: app.to_string(),
        namespace: "default".to_string(),
        state: state.to_string(),
        restart_count: 0,
        host_port: None,
        exit_code: None,
        pid: Some(4242),
        runtime_unknown: false,
        status_age_ms: None,
    }
}

fn summary_of(id: u64, request: &FaultRequest) -> FaultSummary {
    FaultSummary {
        id,
        fault_type: request.fault_type.to_string(),
        target_service: request.target_service.clone(),
        target_instance: request.target_instance.clone(),
        target_node: request.target_node.clone(),
        remaining_secs: 60,
        injected_by: request.injected_by.clone(),
        node: None,
        routed: Vec::new(),
    }
}

/// One snapshot of `/data` called `name`, as a fake agent reports it.
fn snapshot_meta(namespace: &str, app: &str, name: &str) -> crate::grill::snapshot::SnapshotMeta {
    crate::grill::snapshot::SnapshotMeta {
        schema: 1,
        namespace: namespace.to_string(),
        app: app.to_string(),
        volume_path: "/data".to_string(),
        name: name.to_string(),
        created_at: std::time::SystemTime::UNIX_EPOCH,
        size_bytes: 23,
        exports: Vec::new(),
    }
}

/// Answer the agent commands the routing paths use from a fixed script.
fn spawn_fake_agent(
    name: String,
    instances: Vec<InstanceStatus>,
    desired: Vec<crate::bun::diagnostics::DesiredAppEvidence>,
    injected: Injected,
    snapshots: SnapshotLog,
    mut commands: mpsc::Receiver<AgentCommand>,
    stop: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let command = tokio::select! {
                () = stop.cancelled() => return,
                command = commands.recv() => match command {
                    Some(command) => command,
                    None => return,
                },
            };
            match command {
                AgentCommand::Status { response } => {
                    let _ = response.send(instances.clone());
                }
                AgentCommand::DesiredApps { response } => {
                    let _ = response.send(desired.clone());
                }
                AgentCommand::SnapshotCreate {
                    namespace,
                    app_name,
                    name: snapshot,
                    response,
                    ..
                } => {
                    let snapshot = snapshot.unwrap_or_default();
                    snapshots
                        .lock()
                        .await
                        .push(format!("create {app_name}/{snapshot}"));
                    let _ =
                        response.send(Ok(vec![snapshot_meta(&namespace, &app_name, &snapshot)]));
                }
                AgentCommand::SnapshotList {
                    namespace,
                    app_name,
                    response,
                } => {
                    snapshots.lock().await.push(format!("list {app_name}"));
                    // Each node's listing names the node, so a test can tell whose copy it read.
                    let _ = response.send(Ok(vec![snapshot_meta(&namespace, &app_name, &name)]));
                }
                AgentCommand::SnapshotRestore {
                    app_name,
                    name: snapshot,
                    response,
                    ..
                } => {
                    snapshots
                        .lock()
                        .await
                        .push(format!("restore {app_name}/{snapshot}"));
                    let _ = response.send(Ok(()));
                }
                AgentCommand::SnapshotDelete {
                    app_name,
                    name: snapshot,
                    response,
                    ..
                } => {
                    snapshots
                        .lock()
                        .await
                        .push(format!("delete {app_name}/{snapshot}"));
                    let _ = response.send(Ok(()));
                }
                AgentCommand::ListFaults { response } => {
                    let faults = injected
                        .lock()
                        .await
                        .iter()
                        .enumerate()
                        .map(|(index, (request, _))| summary_of(index as u64 + 1, request))
                        .collect();
                    let _ = response.send(faults);
                }
                AgentCommand::InjectFault {
                    request,
                    replica_evidence,
                    response,
                    ..
                } => {
                    let mut injected = injected.lock().await;
                    let summary = summary_of(injected.len() as u64 + 1, &request);
                    injected.push((request, replica_evidence));
                    let _ = response.send(Ok(summary));
                }
                AgentCommand::ClearAllFaults { response } => {
                    let count = injected.lock().await.drain(..).count();
                    let _ = response.send(Ok(format!("{name} cleared {count}")));
                }
                AgentCommand::ClearFault {
                    fault_id, response, ..
                } => {
                    let mut injected = injected.lock().await;
                    let result = match usize::try_from(fault_id) {
                        Ok(index) if (1..=injected.len()).contains(&index) => {
                            injected.remove(index - 1);
                            Ok(crate::bun::agent::FaultClearance {
                                message: format!("{name} cleared fault {fault_id}"),
                                reservation: None,
                            })
                        }
                        _ => Err(crate::bun::BunError::FaultRejected {
                            reason: format!("no fault {fault_id}"),
                        }),
                    };
                    let _ = response.send(result);
                }
                _ => {}
            }
        }
    });
}

/// Start one router per `(name, instances)` pair, all sharing a
/// membership table, a service token and one operator token.
async fn start_cluster(layout: Vec<(&str, Vec<InstanceStatus>)>) -> FakeCluster {
    start_cluster_with_desired(layout, Vec::new()).await
}

/// [`start_cluster`] whose agents all report `desired` as the cluster's
/// desired apps, as a council-less node answers the desired-apps read.
async fn start_cluster_with_desired(
    layout: Vec<(&str, Vec<InstanceStatus>)>,
    desired: Vec<crate::bun::diagnostics::DesiredAppEvidence>,
) -> FakeCluster {
    let created = crate::sesame::token::create_token(
        "operator",
        crate::sesame::types::ApiRole::Admin,
        crate::sesame::types::TokenScope::default(),
        None,
    )
    .unwrap();
    let api_reader = crate::sesame::token::create_token(
        "api-reader",
        crate::sesame::types::ApiRole::ReadOnly,
        crate::sesame::types::TokenScope {
            apps: Some(vec!["api".to_string()]),
            namespaces: None,
        },
        None,
    )
    .unwrap();
    let stop = CancellationToken::new();
    let mut listeners = Vec::new();
    let mut membership = Vec::new();
    for (name, _) in &layout {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        membership.push(NodeMembershipInfo {
            node_id: crate::meat::NodeId::new(*name),
            address: listener.local_addr().unwrap(),
            api_advertised: true,
        });
        listeners.push(listener);
    }
    let known = KnownMembers::default();
    known
        .refresh(
            roster_of(&membership, crate::mustard::state::NodeState::Alive),
            &Default::default(),
            std::time::Instant::now(),
        )
        .await;
    let membership = Arc::new(RwLock::new(membership));
    let mut nodes = Vec::new();
    for ((name, instances), listener) in layout.into_iter().zip(listeners) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let injected: Injected = Arc::default();
        let snapshots: SnapshotLog = Arc::default();
        spawn_fake_agent(
            name.to_string(),
            instances,
            desired.clone(),
            Arc::clone(&injected),
            Arc::clone(&snapshots),
            cmd_rx,
            stop.clone(),
        );
        let store = crate::sesame::auth::new_token_store();
        *store.write().await = vec![created.token.clone(), api_reader.token.clone()];
        let static_capabilities = crate::bun::capabilities::StaticCapabilities {
            test_policy: crate::testkit::safety::ClusterTestPolicy {
                safety_class: crate::testkit::safety::ClusterSafetyClass::Development,
                allowed_operations: std::collections::BTreeSet::from([
                    crate::testkit::safety::OperationPermission::InjectWorkloadFaults,
                ]),
                ..crate::testkit::safety::ClusterTestPolicy::default()
            },
            ..crate::bun::capabilities::StaticCapabilities::default()
        };
        let app = router_with_upgrade(
            cmd_tx,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(store),
            Some(SERVICE_TOKEN.to_string()),
            None,
            Some(Arc::clone(&membership)),
            None,
            None,
            listener.local_addr().unwrap().port(),
            None,
            None,
            None,
            "default".to_string(),
            Some(name.to_string()),
            crate::bun::build_runner::BuildSettings::with_timeout(900),
            crate::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            static_capabilities,
            super::super::readiness::ReadinessTracker::new(),
            None,
            None,
            None,
            None,
        )
        .layer(axum::Extension(known.clone()));
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancelled = stop.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { cancelled.cancelled().await })
                .await
                .ok();
        });
        nodes.push(FakeNode {
            url,
            injected,
            snapshots,
        });
    }
    FakeCluster {
        nodes,
        membership,
        known,
        operator: created.plaintext,
        api_reader: api_reader.plaintext,
        stop,
    }
}

fn kill(count: u32) -> FaultRequest {
    FaultRequest {
        fault_type: FaultType::Kill { count },
        target_service: "web".to_string(),
        namespace: None,
        target_instance: None,
        target_node: None,
        duration: std::time::Duration::from_secs(0),
        injected_by: String::new(),
        reason: None,
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    }
}

async fn inject(
    cluster: &FakeCluster,
    entry: usize,
    request: &FaultRequest,
) -> (StatusCode, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/fault", cluster.nodes[entry].url))
        .bearer_auth(&cluster.operator)
        .json(request)
        .send()
        .await
        .unwrap();
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    let text = response.text().await.unwrap();
    let body = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
    (status, body)
}

async fn injected_count(cluster: &FakeCluster) -> usize {
    let mut total = 0;
    for node in &cluster.nodes {
        total += node.injected.lock().await.len();
    }
    total
}

#[tokio::test]
async fn a_workload_fault_reaches_the_node_that_runs_its_target() {
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        (
            "node-2",
            vec![
                instance("default/web-0", "web", "running"),
                instance("default/web-1", "web", "running"),
            ],
        ),
    ])
    .await;

    let (status, body) = inject(&cluster, 0, &kill(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let summary: FaultSummary = serde_json::from_value(body).unwrap();
    assert_eq!(summary.node.as_deref(), Some("node-2"));
    assert_eq!(summary.target_node.as_deref(), Some("node-2"));

    assert!(cluster.nodes[0].injected.lock().await.is_empty());
    let owner = cluster.nodes[1].injected.lock().await;
    assert_eq!(owner.len(), 1);
    let (request, evidence) = &owner[0];
    assert_eq!(request.fault_type, FaultType::Kill { count: 1 });
    // The owner recorded the caller's token, not a node identity.
    assert_eq!(request.injected_by, "operator");
    assert_eq!(
        *evidence,
        Some(ReplicaEvidence {
            replicas: 2,
            faulted_replicas: 0,
        })
    );
}

#[tokio::test]
async fn the_replica_rail_counts_replicas_on_every_node() {
    // One replica on each node: killing both leaves nothing, even though
    // each node alone would think it was only losing its own copy.
    let cluster = start_cluster(vec![
        ("node-1", vec![instance("default/web-0", "web", "running")]),
        ("node-2", vec![instance("default/web-0", "web", "running")]),
    ])
    .await;

    let (status, body) = inject(&cluster, 0, &kill(2)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("replica"), "{body}");
    assert_eq!(injected_count(&cluster).await, 0);

    let (status, body) = inject(&cluster, 0, &kill(1)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(injected_count(&cluster).await, 1);
}

#[tokio::test]
async fn a_fault_on_several_owners_reports_every_fault_it_created() {
    let cluster = start_cluster(vec![
        ("node-1", vec![instance("default/web-0", "web", "running")]),
        ("node-2", vec![instance("default/web-0", "web", "running")]),
        ("node-3", vec![instance("default/web-0", "web", "running")]),
    ])
    .await;

    let (status, body) = inject(&cluster, 1, &kill(2)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let summary: FaultSummary = serde_json::from_value(body).unwrap();
    let mut nodes: Vec<_> = std::iter::once(&summary)
        .chain(&summary.routed)
        .map(|fault| fault.node.clone().unwrap())
        .collect();
    nodes.sort();
    assert_eq!(nodes, vec!["node-1", "node-2"]);
    assert!(cluster.nodes[2].injected.lock().await.is_empty());
}

#[tokio::test]
async fn a_fault_with_no_running_target_is_refused_before_anything_runs() {
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        ("node-2", vec![instance("default/web-0", "web", "stopped")]),
    ])
    .await;
    let (status, body) = inject(&cluster, 0, &kill(1)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("no running instances"), "{body}");
    assert_eq!(injected_count(&cluster).await, 0);
}

fn network(fault_type: FaultType) -> FaultRequest {
    FaultRequest {
        fault_type,
        duration: std::time::Duration::from_secs(60),
        ..kill(0)
    }
}

async fn nodes_that_got_a_fault(cluster: &FakeCluster) -> Vec<usize> {
    let mut nodes = Vec::new();
    for (index, node) in cluster.nodes.iter().enumerate() {
        if !node.injected.lock().await.is_empty() {
            nodes.push(index);
        }
    }
    nodes
}

#[tokio::test]
async fn a_network_fault_on_every_caller_lands_on_every_node() {
    // The target runs on node-2 only, but its callers could be anywhere:
    // the connect hook and the DNS responder act on the caller's node.
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        ("node-2", vec![instance("default/web-0", "web", "running")]),
        ("node-3", vec![]),
    ])
    .await;

    let (status, body) = inject(&cluster, 0, &network(FaultType::DnsNxdomain)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let summary: FaultSummary = serde_json::from_value(body).unwrap();
    let mut holders: Vec<_> = std::iter::once(&summary)
        .chain(&summary.routed)
        .map(|fault| fault.node.clone().unwrap())
        .collect();
    holders.sort();
    assert_eq!(holders, vec!["node-1", "node-2", "node-3"]);
    assert_eq!(nodes_that_got_a_fault(&cluster).await, vec![0, 1, 2]);
    for node in &cluster.nodes {
        let injected = node.injected.lock().await;
        // Each node's share names that node, and the owner re-plans it
        // as a network fault rather than a target-owner fault.
        assert_eq!(injected[0].0.fault_type, FaultType::DnsNxdomain);
        assert_eq!(injected[0].1, None, "no replica evidence for traffic");
    }
}

#[tokio::test]
async fn a_network_fault_from_one_source_lands_only_where_that_source_runs() {
    let mut other_tenant = instance("team-b/frontend-0", "frontend", "running");
    other_tenant.namespace = "team-b".to_string();
    let cluster = start_cluster(vec![
        ("node-1", vec![instance("default/web-0", "web", "running")]),
        ("node-2", vec![other_tenant]),
        (
            "node-3",
            vec![instance("default/frontend-0", "frontend", "running")],
        ),
    ])
    .await;

    let partition = network(FaultType::Partition {
        source_app: Some("frontend".to_string()),
    });
    let (status, body) = inject(&cluster, 0, &partition).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(nodes_that_got_a_fault(&cluster).await, vec![2]);

    // A source with no running instance anywhere is refused up front.
    let nowhere = network(FaultType::Delay {
        delay_ns: 300_000_000,
        jitter_ns: 0,
        source_app: Some("worker".to_string()),
    });
    let (status, body) = inject(&cluster, 0, &nowhere).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("worker"), "{body}");
    assert_eq!(injected_count(&cluster).await, 1);
}

#[tokio::test]
async fn the_cluster_fault_list_and_clear_reach_every_node() {
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        (
            "node-2",
            vec![
                instance("default/web-0", "web", "running"),
                instance("default/web-1", "web", "running"),
            ],
        ),
    ])
    .await;
    assert_eq!(inject(&cluster, 0, &kill(1)).await.0, StatusCode::OK);

    let client = reqwest::Client::new();
    let listing: ClusterFaultList = client
        .get(format!("{}/v1/fault?cluster=true", cluster.nodes[0].url))
        .bearer_auth(&cluster.operator)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listing.warnings.is_empty(), "{:?}", listing.warnings);
    assert_eq!(listing.faults.len(), 1);
    assert_eq!(listing.faults[0].node.as_deref(), Some("node-2"));

    let response = client
        .delete(format!("{}/v1/fault", cluster.nodes[0].url))
        .bearer_auth(&cluster.operator)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let message = response.text().await.unwrap();
    assert!(message.contains("node-2: node-2 cleared 1"), "{message}");
    assert!(cluster.nodes[1].injected.lock().await.is_empty());
}

#[tokio::test]
async fn top_merges_every_node_and_warns_about_the_missing_one() {
    let cluster = start_cluster(vec![
        ("node-1", vec![instance("default/web-0", "web", "running")]),
        (
            "node-2",
            vec![
                instance("default/web-0", "web", "running"),
                instance("default/api-0", "api", "running"),
            ],
        ),
    ])
    .await;
    cluster.add_unreachable_member("node-3").await;

    let top: crate::bun::top::ClusterTop = cluster.get_json(0, "/v1/top?cluster=true").await;
    let rows: Vec<_> = top
        .rows
        .iter()
        .map(|row| (row.node.as_str(), row.instance.app_name.as_str()))
        .collect();
    assert_eq!(
        rows,
        vec![("node-1", "web"), ("node-2", "api"), ("node-2", "web")]
    );
    assert_eq!(top.warnings.len(), 1, "{:?}", top.warnings);
    assert!(
        top.warnings[0].starts_with("node node-3"),
        "{:?}",
        top.warnings
    );

    // Without `cluster`, a node answers for itself only.
    let local: Vec<crate::bun::top::TopRow> = cluster.get_json(1, "/v1/top").await;
    assert!(local.iter().all(|row| row.node == "node-2"));
    assert_eq!(local.len(), 2);
}

async fn relay(
    cluster: &FakeCluster,
    method: reqwest::Method,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, String) {
    let mut request =
        reqwest::Client::new().request(method, format!("{}{path}", cluster.nodes[0].url));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.unwrap();
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    (status, response.text().await.unwrap())
}

#[tokio::test]
async fn the_relay_reaches_a_peer_with_the_callers_own_credential() {
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        (
            "node-2",
            vec![
                instance("default/web-0", "web", "running"),
                instance("default/api-0", "api", "running"),
            ],
        ),
    ])
    .await;
    let operator = Some(cluster.operator.as_str());

    let (status, body) = relay(
        &cluster,
        reqwest::Method::GET,
        "/v1/nodes/node-2/relay/v1/status",
        operator,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let statuses: Vec<InstanceStatus> = serde_json::from_str(&body).unwrap();
    assert_eq!(statuses.len(), 2);

    // A scoped caller stays scoped on the far side: the peer filtered with
    // the caller's token, not a node identity that sees everything.
    let (status, body) = relay(
        &cluster,
        reqwest::Method::GET,
        "/v1/nodes/node-2/relay/v1/status",
        Some(&cluster.api_reader),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let statuses: Vec<InstanceStatus> = serde_json::from_str(&body).unwrap();
    let apps: Vec<_> = statuses.iter().map(|s| s.app_name.as_str()).collect();
    assert_eq!(apps, vec!["api"]);

    // No credential, no relay.
    let (status, _) = relay(
        &cluster,
        reqwest::Method::GET,
        "/v1/nodes/node-2/relay/v1/status",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A node-kill fault leaves the target's API open while gossip calls it
/// dead. The relay must still reach it, or nobody outside the cluster
/// network can watch it or clear the fault.
#[tokio::test]
async fn the_relay_reaches_a_member_gossip_no_longer_counts_as_alive() {
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        ("node-2", vec![instance("default/web-0", "web", "running")]),
    ])
    .await;
    cluster
        .membership
        .write()
        .await
        .retain(|member| member.node_id != crate::meat::NodeId::new("node-2"));

    let (status, body) = relay(
        &cluster,
        reqwest::Method::GET,
        "/v1/nodes/node-2/relay/v1/status",
        Some(cluster.operator.as_str()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let statuses: Vec<InstanceStatus> = serde_json::from_str(&body).unwrap();
    assert_eq!(statuses.len(), 1);
    cluster.stop.cancel();
}

/// Gossip stops publishing a member once it declares it dead, which is
/// exactly when a node-kill fault needs clearing. The entry node must
/// still reach the killed node's open API, or the clear that would heal
/// it is refused and the node stays dead until the fault expires.
#[tokio::test]
async fn a_node_fault_clear_reaches_a_member_gossip_has_declared_dead() {
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        (
            "node-2",
            vec![
                instance("default/web-0", "web", "running"),
                instance("default/web-1", "web", "running"),
            ],
        ),
    ])
    .await;
    assert_eq!(inject(&cluster, 0, &kill(1)).await.0, StatusCode::OK);
    assert_eq!(cluster.nodes[1].injected.lock().await.len(), 1);

    // Gossip declares node-2 dead: it vanishes from the published view.
    let dead = crate::meat::NodeId::new("node-2");
    let live: Vec<_> = cluster
        .membership
        .read()
        .await
        .iter()
        .filter(|member| member.node_id != dead)
        .cloned()
        .collect();
    *cluster.membership.write().await = live.clone();
    cluster
        .known
        .refresh(
            roster_of(&live, crate::mustard::state::NodeState::Alive),
            &Default::default(),
            std::time::Instant::now(),
        )
        .await;

    let (status, body) = relay(
        &cluster,
        reqwest::Method::DELETE,
        "/v1/fault/1?node=node-2",
        Some(cluster.operator.as_str()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("node-2 cleared fault 1"), "{body}");
    assert!(cluster.nodes[1].injected.lock().await.is_empty());
    cluster.stop.cancel();
}

/// Gossip's roster for `members`, every one in `state`.
fn roster_of(
    members: &[NodeMembershipInfo],
    state: crate::mustard::state::NodeState,
) -> Vec<RosterMember> {
    members
        .iter()
        .map(|info| RosterMember {
            info: info.clone(),
            gossip_address: info.address,
            state,
            incarnation: 1,
            labels: Default::default(),
        })
        .collect()
}

fn known_member(name: &str, port: u16) -> NodeMembershipInfo {
    NodeMembershipInfo {
        node_id: crate::meat::NodeId::new(name),
        address: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        api_advertised: true,
    }
}

async fn known_ports(known: &KnownMembers) -> Vec<(String, u16)> {
    let mut ports = Vec::new();
    for name in ["node-1", "node-2", "node-3"] {
        if let Some(address) = known.api_address(&crate::meat::NodeId::new(name)).await {
            ports.push((name.to_string(), address.port()));
        }
    }
    ports
}

#[tokio::test]
async fn known_members_take_a_returning_members_new_address_once() {
    use crate::mustard::state::NodeState::Alive;
    let known = KnownMembers::default();
    let none = Default::default();
    let now = std::time::Instant::now();
    let both = [known_member("node-1", 1001), known_member("node-2", 1002)];
    known.refresh(roster_of(&both, Alive), &none, now).await;
    let one = [known_member("node-1", 1001)];
    known.refresh(roster_of(&one, Alive), &none, now).await;
    let returned = [known_member("node-1", 1001), known_member("node-2", 2002)];
    known.refresh(roster_of(&returned, Alive), &none, now).await;

    assert_eq!(
        known_ports(&known).await,
        vec![("node-1".to_string(), 1001), ("node-2".to_string(), 2002)]
    );
}

/// The #244 behaviour: a node gossip declared dead, then reaped, keeps
/// its last address so a node-kill fault on it can still be cleared.
#[tokio::test]
async fn known_members_keep_a_dead_member_after_gossip_reaps_it() {
    use crate::mustard::state::NodeState::{Alive, Dead};
    let known = KnownMembers::default();
    let none = Default::default();
    let now = std::time::Instant::now();
    let alive = [known_member("node-1", 1001), known_member("node-2", 1002)];
    known.refresh(roster_of(&alive, Alive), &none, now).await;
    let mut roster = roster_of(&alive[..1], Alive);
    roster.extend(roster_of(&alive[1..], Dead));
    known.refresh(roster, &none, now).await;
    known
        .refresh(roster_of(&alive[..1], Alive), &none, now)
        .await;

    assert_eq!(
        known_ports(&known).await,
        vec![("node-1".to_string(), 1001), ("node-2".to_string(), 1002)]
    );
    let down = known.down().await;
    assert_eq!(down.len(), 1);
    assert_eq!(down[0].info.node_id.0, "node-2");
    assert_eq!(down[0].state, Dead);
}

#[tokio::test]
async fn known_members_forget_a_member_that_left() {
    use crate::mustard::state::NodeState::{Alive, Left};
    let known = KnownMembers::default();
    let none = Default::default();
    let now = std::time::Instant::now();
    let alive = [known_member("node-1", 1001), known_member("node-2", 1002)];
    known.refresh(roster_of(&alive, Alive), &none, now).await;
    let mut roster = roster_of(&alive[..1], Alive);
    roster.extend(roster_of(&alive[1..], Left));
    known.refresh(roster, &none, now).await;
    // Gossip reaps the Left entry; it must not come back as dead.
    known
        .refresh(roster_of(&alive[..1], Alive), &none, now)
        .await;

    assert_eq!(
        known_ports(&known).await,
        vec![("node-1".to_string(), 1001)]
    );
    assert!(known.down().await.is_empty());
}

#[tokio::test]
async fn known_members_forget_a_retired_member_live_or_remembered() {
    use crate::mustard::state::NodeState::{Alive, Dead};
    let known = KnownMembers::default();
    let none = std::collections::BTreeSet::new();
    let now = std::time::Instant::now();
    let all = [
        known_member("node-1", 1001),
        known_member("node-2", 1002),
        known_member("node-3", 1003),
    ];
    known.refresh(roster_of(&all, Alive), &none, now).await;
    // node-2 dies and is reaped; node-3 is still dead in gossip's table.
    let mut roster = roster_of(&all[..1], Alive);
    roster.extend(roster_of(&all[2..], Dead));
    known.refresh(roster.clone(), &none, now).await;

    let retired = std::collections::BTreeSet::from(["node-2".to_string(), "node-3".to_string()]);
    known.refresh(roster, &retired, now).await;

    assert_eq!(
        known_ports(&known).await,
        vec![("node-1".to_string(), 1001)]
    );
}

#[tokio::test]
async fn known_members_forget_a_dead_member_unheard_of_for_the_retention() {
    use crate::mustard::state::NodeState::Alive;
    let known = KnownMembers::default();
    let none = Default::default();
    let start = std::time::Instant::now();
    let alive = [known_member("node-1", 1001), known_member("node-2", 1002)];
    known.refresh(roster_of(&alive, Alive), &none, start).await;
    // node-2 dies and is reaped at once.
    let later = start + std::time::Duration::from_secs(60);
    known
        .refresh(roster_of(&alive[..1], Alive), &none, later)
        .await;

    let just_inside = start + KNOWN_MEMBER_RETENTION;
    known
        .refresh(roster_of(&alive[..1], Alive), &none, just_inside)
        .await;
    assert_eq!(known_ports(&known).await.len(), 2);

    let past = just_inside + std::time::Duration::from_secs(1);
    known
        .refresh(roster_of(&alive[..1], Alive), &none, past)
        .await;
    assert_eq!(
        known_ports(&known).await,
        vec![("node-1".to_string(), 1001)]
    );
}

#[test]
fn known_member_retention_covers_the_longest_fault() {
    assert_eq!(
        KNOWN_MEMBER_RETENTION,
        std::time::Duration::from_secs(24 * 3600)
    );
}

#[test]
fn the_relay_forwards_one_apps_deploy_history_and_nothing_nested() {
    let get = axum::http::Method::GET;
    assert!(relay_allows(&get, "v1/deploys/history/web"));
    assert!(!relay_allows(&get, "v1/deploys/history/"));
    assert!(!relay_allows(&get, "v1/deploys/history/web/extra"));
    assert!(!relay_allows(&get, "v1/deploys/history"));
    assert!(!relay_allows(
        &axum::http::Method::POST,
        "v1/deploys/history/web"
    ));
}

#[test]
fn the_relay_forwards_each_nodes_version_for_wtf() {
    let get = axum::http::Method::GET;
    assert!(relay_allows(&get, "v1/version"));
    assert!(!relay_allows(&axum::http::Method::POST, "v1/version"));
}

#[test]
fn the_relay_forwards_exec_to_one_app_and_nothing_nested() {
    let post = axum::http::Method::POST;
    assert!(relay_allows(&post, "v1/exec/web/default"));
    assert!(!relay_allows(&post, "v1/exec/web/default/extra"));
    assert!(!relay_allows(&post, "v1/exec/web"));
    assert!(!relay_allows(&post, "v1/exec//default"));
    assert!(!relay_allows(
        &axum::http::Method::GET,
        "v1/exec/web/default"
    ));
}

#[tokio::test]
async fn the_relay_forwards_only_the_diagnostic_reads() {
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        ("node-2", vec![instance("default/web-0", "web", "running")]),
    ])
    .await;
    let operator = Some(cluster.operator.as_str());
    for (method, path) in [
        (reqwest::Method::POST, "/v1/nodes/node-2/relay/v1/fault"),
        (reqwest::Method::GET, "/v1/nodes/node-2/relay/v1/token/list"),
        (reqwest::Method::DELETE, "/v1/nodes/node-2/relay/v1/fault"),
        (
            reqwest::Method::GET,
            "/v1/nodes/node-2/relay/v1/nodes/node-1/relay/v1/status",
        ),
    ] {
        let (status, body) = relay(&cluster, method.clone(), path, operator).await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path} was relayed: {status} {body}"
        );
    }
    assert_eq!(injected_count(&cluster).await, 0);

    let (status, body) = relay(
        &cluster,
        reqwest::Method::GET,
        "/v1/nodes/node-9/relay/v1/status",
        operator,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(body.contains("node-9"), "{body}");
}

#[tokio::test]
async fn the_relay_keeps_the_query_string() {
    let cluster = start_cluster(vec![
        ("node-1", vec![]),
        (
            "node-2",
            vec![
                instance("default/web-0", "web", "running"),
                instance("default/web-1", "web", "running"),
            ],
        ),
    ])
    .await;
    assert_eq!(inject(&cluster, 1, &kill(1)).await.0, StatusCode::OK);
    let (status, body) = relay(
        &cluster,
        reqwest::Method::GET,
        "/v1/nodes/node-2/relay/v1/fault?cluster=true",
        Some(&cluster.operator),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listing: ClusterFaultList = serde_json::from_str(&body).unwrap();
    assert_eq!(listing.faults.len(), 1);
}

/// `default/db`, a managed-volume app whose volumes live on `homes`.
fn volume_app(homes: &[&str]) -> crate::bun::diagnostics::DesiredAppEvidence {
    crate::bun::diagnostics::DesiredAppEvidence {
        app: "db".to_string(),
        namespace: "default".to_string(),
        desired_replicas: u32::try_from(homes.len()).unwrap(),
        scheduled_replicas: u32::try_from(homes.len()).unwrap(),
        placements: homes.iter().map(|home| (home.to_string(), 1)).collect(),
        service_port: None,
        blocked: None,
        volume_home_away: None,
        volume_homes: homes.iter().map(ToString::to_string).collect(),
    }
}

/// Send one snapshot request to the node at `entry` as the operator.
async fn snapshot_call(
    cluster: &FakeCluster,
    entry: usize,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, String) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", cluster.nodes[entry].url))
        .bearer_auth(&cluster.operator);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
    (status, response.text().await.unwrap())
}

/// Run create, list, restore and delete of `db` through the node at `entry`.
async fn every_snapshot_command(cluster: &FakeCluster, entry: usize) -> Vec<(StatusCode, String)> {
    vec![
        snapshot_call(
            cluster,
            entry,
            reqwest::Method::POST,
            "/v1/snapshots/default/db",
            Some(serde_json::json!({ "volume": "/data", "name": "before" })),
        )
        .await,
        snapshot_call(
            cluster,
            entry,
            reqwest::Method::GET,
            "/v1/snapshots/default/db",
            None,
        )
        .await,
        snapshot_call(
            cluster,
            entry,
            reqwest::Method::POST,
            "/v1/snapshots/default/db/restore",
            Some(serde_json::json!({ "name": "before" })),
        )
        .await,
        snapshot_call(
            cluster,
            entry,
            reqwest::Method::DELETE,
            "/v1/snapshots/default/db/before",
            None,
        )
        .await,
    ]
}

async fn snapshot_log(cluster: &FakeCluster, entry: usize) -> Vec<String> {
    cluster.nodes[entry].snapshots.lock().await.clone()
}

const EVERY_SNAPSHOT_COMMAND: [&str; 4] = [
    "create db/before",
    "list db",
    "restore db/before",
    "delete db/before",
];

/// #482: a node with a stale copy of an app's volume (or none) must not
/// answer snapshot requests from it. Whichever node receives them, they act
/// on the node that holds the app's volume.
#[tokio::test]
async fn snapshot_commands_reach_the_node_that_holds_the_apps_volume() {
    let cluster = start_cluster_with_desired(
        vec![("node-1", vec![]), ("node-2", vec![]), ("node-3", vec![])],
        vec![volume_app(&["node-2"])],
    )
    .await;
    for entry in [0, 2] {
        let answers = every_snapshot_command(&cluster, entry).await;
        let statuses: Vec<StatusCode> = answers.iter().map(|(status, _)| *status).collect();
        assert_eq!(
            statuses,
            [
                StatusCode::CREATED,
                StatusCode::OK,
                StatusCode::OK,
                StatusCode::OK
            ],
            "{answers:?}"
        );
        let listed: Vec<crate::grill::snapshot::SnapshotMeta> =
            serde_json::from_str(&answers[1].1).unwrap();
        assert_eq!(listed[0].name, "node-2", "listed another node's snapshots");
    }
    assert!(snapshot_log(&cluster, 0).await.is_empty());
    assert!(snapshot_log(&cluster, 2).await.is_empty());
    let mut expected = EVERY_SNAPSHOT_COMMAND.to_vec();
    expected.extend(EVERY_SNAPSHOT_COMMAND);
    assert_eq!(snapshot_log(&cluster, 1).await, expected);
}

/// A replica's own node acts on its own volume: each replica of a
/// multi-replica volume app has its own copy, and the node that holds one is
/// the right place for it.
#[tokio::test]
async fn a_snapshot_request_on_one_of_the_volume_homes_stays_there() {
    let cluster = start_cluster_with_desired(
        vec![("node-1", vec![]), ("node-2", vec![]), ("node-3", vec![])],
        vec![volume_app(&["node-1", "node-2"])],
    )
    .await;
    for entry in [0, 1] {
        for (status, body) in every_snapshot_command(&cluster, entry).await {
            assert!(status.is_success(), "{status} {body}");
        }
        assert_eq!(snapshot_log(&cluster, entry).await, EVERY_SNAPSHOT_COMMAND);
    }
    assert!(snapshot_log(&cluster, 2).await.is_empty());
}

/// Away from every home of a multi-replica app there is no one right copy,
/// so the request is refused with the nodes that hold one.
#[tokio::test]
async fn a_snapshot_request_away_from_several_volume_homes_names_them() {
    let cluster = start_cluster_with_desired(
        vec![("node-1", vec![]), ("node-2", vec![]), ("node-3", vec![])],
        vec![volume_app(&["node-1", "node-2"])],
    )
    .await;
    for (status, body) in every_snapshot_command(&cluster, 2).await {
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("node-1") && body.contains("node-2"), "{body}");
    }
    for entry in 0..3 {
        assert!(snapshot_log(&cluster, entry).await.is_empty());
    }
}

/// An app the desired state gives no volume home (a standalone node, an
/// unknown app) is handled where the request lands, as before.
#[tokio::test]
async fn a_snapshot_request_for_an_app_without_a_volume_home_stays_local() {
    let cluster =
        start_cluster_with_desired(vec![("node-1", vec![]), ("node-2", vec![])], Vec::new()).await;
    for (status, body) in every_snapshot_command(&cluster, 0).await {
        assert!(status.is_success(), "{status} {body}");
    }
    assert_eq!(snapshot_log(&cluster, 0).await, EVERY_SNAPSHOT_COMMAND);
    assert!(snapshot_log(&cluster, 1).await.is_empty());
}

/// A forwarded snapshot request is answered where it arrives, so two nodes
/// that disagree about the volume's home can't pass it back and forth.
#[tokio::test]
async fn a_forwarded_snapshot_request_is_never_forwarded_again() {
    let cluster = start_cluster_with_desired(
        vec![("node-1", vec![]), ("node-2", vec![])],
        vec![volume_app(&["node-2"])],
    )
    .await;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/snapshots/default/db", cluster.nodes[0].url))
        .bearer_auth(&cluster.operator)
        .header(super::snapshots::SNAPSHOT_FORWARDED_HEADER, "1")
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(snapshot_log(&cluster, 0).await, ["list db"]);
    assert!(snapshot_log(&cluster, 1).await.is_empty());
}
