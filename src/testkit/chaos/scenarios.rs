//! Live five-scenario chaos catalogue.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::bun::capabilities::Capability;
use crate::bun::deploy_operations::DeployOperationOutcome;
use crate::config::Config;
use crate::relish::client::BunClient;
use crate::smoker::types::{FaultRequest, FaultSummary, FaultType};
use crate::testkit::TestContext;
use crate::testkit::context::{PINNED_TEST_WORKLOAD_IMAGE, container_http_script};
use crate::testkit::registry::{TestCase, unknown};
use crate::testkit::report::{CleanupOutcome, TestGroup};

const FAULT_EXPIRY_MARGIN: Duration = Duration::from_secs(30);

fn fault_duration(timeout: Duration) -> Duration {
    // Expiry is a final safety net, not a way for a slow recovery assertion to
    // pass. A server with a lower configured maximum refuses the injection.
    timeout.saturating_add(FAULT_EXPIRY_MARGIN)
}

fn container_spec(context: &TestContext, app: &str, replicas: u32, delayed: bool) -> String {
    let port = context.container_port(app);
    let script = container_http_script(port, if delayed { 3 } else { 0 });
    format!(
        "[app.{app}]\n\
         image = \"{PINNED_TEST_WORKLOAD_IMAGE}\"\n\
         command = [\"/bin/sh\", \"-c\", \"{script}\"]\n\
         port = {port}\n\
         replicas = {replicas}\n\
         namespace = \"{namespace}\"\n\
         \n\
         [app.{app}.health]\n\
         path = \"/hostname\"\n\
         interval = 1\n\
         timeout = 2\n\
         threshold_unhealthy = 3\n\
         threshold_healthy = 1\n",
        namespace = context.namespace,
    )
}

async fn clients_by_node(context: &TestContext) -> Result<BTreeMap<String, BunClient>, String> {
    Ok(context.node_clients().await?.into_iter().collect())
}

fn non_leader<'a>(
    clients: &'a BTreeMap<String, BunClient>,
    leader: &str,
) -> Result<(&'a str, BunClient), String> {
    clients
        .iter()
        .find(|(node, _)| node.as_str() != leader)
        .map(|(node, client)| (node.as_str(), client.clone()))
        .ok_or_else(|| "no live non-leader node is available".to_string())
}

fn node_request(
    context: &TestContext,
    fault_type: FaultType,
    target: &str,
    include_leader: bool,
    reason: &str,
) -> FaultRequest {
    FaultRequest {
        fault_type,
        target_service: String::new(),
        namespace: None,
        target_instance: None,
        target_node: Some(target.to_string()),
        duration: fault_duration(context.timeout),
        injected_by: String::new(),
        reason: Some(reason.to_string()),
        include_leader,
        override_safety: false,
        acknowledged: true,
    }
}

async fn inject_node_fault(
    context: &TestContext,
    target_client: BunClient,
    request: FaultRequest,
) -> Result<FaultSummary, String> {
    context
        .chaos()
        .inject_fault(context.fault_owner(target_client), &request)
        .await
}

async fn clear_owned_faults(context: &TestContext) -> Result<(), String> {
    match context.chaos().cleanup(context.deadline).await {
        CleanupOutcome::Confirmed | CleanupOutcome::NotRequired => Ok(()),
        CleanupOutcome::Failed { reason } | CleanupOutcome::Unknown { reason } => Err(reason),
    }
}

async fn wait_for_leader(
    context: &TestContext,
    observer: &BunClient,
    different_from: Option<&str>,
) -> Result<String, String> {
    loop {
        if let Ok(status) = observer.council().await
            && let Some(leader) = status.leader
            && different_from.is_none_or(|old| old != leader)
        {
            context.wait_note.clear().await;
            return Ok(leader);
        }
        if context.deadline.remaining().is_zero() {
            return Err(match different_from {
                Some(old) => format!("no leader different from {old} was elected before deadline"),
                None => "no council leader was observed before deadline".to_string(),
            });
        }
        let waiting = match different_from {
            Some(old) => format!("waiting for a council leader other than {old}"),
            None => "waiting for a council leader".to_string(),
        };
        context.wait_note.record(waiting).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_node_state(
    context: &TestContext,
    observer: &BunClient,
    node_id: &str,
    wanted: &[&str],
) -> Result<(), String> {
    let mut last = None;
    loop {
        if let Ok(nodes) = observer.nodes().await {
            last = nodes
                .iter()
                .find(|node| node.node_id == node_id)
                .map(|node| node.state.clone());
            if last.as_deref().is_some_and(|state| wanted.contains(&state)) {
                context.wait_note.clear().await;
                return Ok(());
            }
        }
        if context.deadline.remaining().is_zero() {
            return Err(format!(
                "node {node_id} did not reach one of {wanted:?}; last state was {last:?}"
            ));
        }
        context
            .wait_note
            .record(format!(
                "waiting for node {node_id} to reach one of {wanted:?}; last state was {last:?}"
            ))
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn apply_with_client(
    context: &TestContext,
    client: &BunClient,
    spec: &str,
) -> Result<(), String> {
    let config = Config::parse(spec).map_err(|error| format!("config does not parse: {error}"))?;
    let lease_id = context
        .lease_id
        .as_deref()
        .ok_or_else(|| "chaos case has no server-owned resource lease".to_string())?;
    client
        .apply_with_lease(&config, lease_id)
        .await
        .map(|_| ())
        .map_err(|error| format!("leased apply failed: {error}"))
}

async fn leader_failure_elects_new_leader_and_cluster_recovers(
    context: TestContext,
) -> Result<(), String> {
    let council = context
        .client
        .council()
        .await
        .map_err(|error| format!("could not read council: {error}"))?;
    let old_leader = council
        .leader
        .ok_or_else(|| "council reported no leader".to_string())?;
    let clients = clients_by_node(&context).await?;
    let leader_client = clients
        .get(&old_leader)
        .cloned()
        .ok_or_else(|| format!("leader {old_leader} has no direct API client"))?;
    let (_, observer) = non_leader(&clients, &old_leader)?;

    inject_node_fault(
        &context,
        leader_client,
        node_request(
            &context,
            FaultType::NodeKill {
                kill_containers: false,
            },
            &old_leader,
            true,
            "phase15 C1 leader failure",
        ),
    )
    .await?;

    let _new_leader = wait_for_leader(&context, &observer, Some(&old_leader)).await?;
    let canary = "chaos-c1-canary";
    apply_with_client(
        &context,
        &observer,
        &container_spec(&context, canary, 1, false),
    )
    .await?;
    context.wait_running_cluster(canary, 1).await?;

    clear_owned_faults(&context).await?;
    wait_for_node_state(&context, &observer, &old_leader, &["alive"]).await?;
    let _recovered_leader = wait_for_leader(&context, &observer, None).await?;
    Ok(())
}

async fn dead_worker_node_has_workloads_rescheduled(context: TestContext) -> Result<(), String> {
    let app = "chaos-c2-reschedule";
    context
        .apply(&container_spec(&context, app, 3, false))
        .await?;
    context.wait_running_cluster(app, 3).await?;

    let leader = context
        .client
        .council()
        .await
        .map_err(|error| format!("could not read council: {error}"))?
        .leader
        .ok_or_else(|| "council reported no leader".to_string())?;
    let clients = clients_by_node(&context).await?;
    let mut target = None;
    for (node, client) in &clients {
        if node == &leader {
            continue;
        }
        let hosts_app = client
            .status()
            .await
            .map(|instances| {
                instances.iter().any(|instance| {
                    instance.app_name == app && instance.namespace == context.namespace
                })
            })
            .unwrap_or(false);
        if hosts_app {
            target = Some((node.clone(), client.clone()));
            break;
        }
    }
    let (target_node, target_client) =
        target.ok_or_else(|| "no non-leader node hosts the three-replica workload".to_string())?;
    let (_, observer) = clients
        .iter()
        .find(|(node, _)| node.as_str() != target_node)
        .map(|(node, client)| (node.as_str(), client.clone()))
        .ok_or_else(|| "no surviving observer node".to_string())?;

    inject_node_fault(
        &context,
        target_client,
        node_request(
            &context,
            FaultType::NodeKill {
                kill_containers: true,
            },
            &target_node,
            false,
            "phase15 C2 worker failure",
        ),
    )
    .await?;
    wait_for_node_state(&context, &observer, &target_node, &["suspect", "dead"]).await?;
    context.wait_running_cluster(app, 3).await?;

    clear_owned_faults(&context).await?;
    wait_for_node_state(&context, &observer, &target_node, &["alive"]).await
}

async fn minority_partition_degrades_and_heals(context: TestContext) -> Result<(), String> {
    let council = context
        .client
        .council()
        .await
        .map_err(|error| format!("could not read council: {error}"))?;
    let leader = council
        .leader
        .ok_or_else(|| "council reported no leader".to_string())?;
    let clients = clients_by_node(&context).await?;
    let (target, target_client) = council
        .members
        .iter()
        .filter(|member| member.name != leader)
        .find_map(|member| {
            clients
                .get(&member.name)
                .cloned()
                .map(|client| (member.name.clone(), client))
        })
        .ok_or_else(|| "no non-leader council member has a direct API client".to_string())?;
    let observer = clients
        .get(&leader)
        .cloned()
        .ok_or_else(|| format!("council leader {leader} has no direct API client"))?;
    let peers: Vec<String> = council
        .members
        .iter()
        .map(|member| member.name.clone())
        .filter(|node| node != &target)
        .collect();
    if peers.len() < 2 {
        return Err(format!(
            "council partition needs at least two peers, found {}",
            peers.len()
        ));
    }

    inject_node_fault(
        &context,
        target_client,
        node_request(
            &context,
            FaultType::CouncilPartition { peers },
            &target,
            true,
            "phase15 C3 minority partition",
        ),
    )
    .await?;
    let canary = "chaos-c3-majority";
    apply_with_client(
        &context,
        &observer,
        &container_spec(&context, canary, 1, false),
    )
    .await?;
    context.wait_running_cluster(canary, 1).await?;
    let majority_leader = wait_for_leader(&context, &observer, None).await?;
    if majority_leader == target {
        return Err(format!(
            "isolated minority node {target} remained the majority's leader"
        ));
    }

    clear_owned_faults(&context).await?;
    wait_for_node_state(&context, &observer, &target, &["alive"]).await
}

async fn resource_exhaustion_degrades_gracefully(context: TestContext) -> Result<(), String> {
    let leader = context
        .client
        .council()
        .await
        .map_err(|error| format!("could not read council: {error}"))?
        .leader
        .ok_or_else(|| "council reported no leader".to_string())?;
    let clients = clients_by_node(&context).await?;
    let (target, target_client) = non_leader(&clients, &leader)?;
    let target = target.to_string();
    let (_, observer) = clients
        .iter()
        .find(|(node, _)| node.as_str() != target)
        .map(|(node, client)| (node.as_str(), client.clone()))
        .ok_or_else(|| "no observer outside the pressured node".to_string())?;
    let cpu = context
        .capabilities
        .test_policy
        .max_node_pressure_cpu_percent
        .min(80);
    let memory = context
        .capabilities
        .test_policy
        .max_node_pressure_memory_percent
        .min(90);
    if cpu == 0 || memory == 0 {
        return Err("node-pressure ceilings became zero after preflight".to_string());
    }

    let summary = inject_node_fault(
        &context,
        target_client.clone(),
        node_request(
            &context,
            FaultType::NodePressure {
                cpu_percentage: cpu,
                memory_percentage: memory,
            },
            &target,
            false,
            "phase15 C4 bounded node pressure",
        ),
    )
    .await?;
    for _ in 0..3 {
        observer
            .health()
            .await
            .map_err(|error| format!("cluster API failed under node pressure: {error}"))?;
        let nodes = observer
            .nodes()
            .await
            .map_err(|error| format!("membership failed under node pressure: {error}"))?;
        if nodes
            .iter()
            .find(|node| node.node_id == target)
            .is_some_and(|node| node.state == "dead")
        {
            return Err("bounded node pressure falsely marked its node dead".to_string());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if !target_client
        .list_faults()
        .await
        .map_err(|error| format!("could not inspect pressure ownership: {error}"))?
        .iter()
        .any(|fault| fault.id == summary.id)
    {
        return Err("node-pressure fault disappeared before explicit cleanup".to_string());
    }

    clear_owned_faults(&context).await?;
    if target_client
        .list_faults()
        .await
        .map_err(|error| format!("could not verify pressure cleanup: {error}"))?
        .iter()
        .any(|fault| fault.id == summary.id)
    {
        return Err("node-pressure fault remains after owned cleanup".to_string());
    }
    Ok(())
}

async fn node_death_during_deploy_ends_clean(
    context: TestContext,
) -> crate::testkit::registry::CaseResult {
    let app = "chaos-c5-deploy";
    context
        .apply(&container_spec(&context, app, 4, false))
        .await?;
    context.wait_running_cluster(app, 4).await?;

    let leader = context
        .client
        .council()
        .await
        .map_err(|error| format!("could not read council: {error}"))?
        .leader
        .ok_or_else(|| "council reported no leader".to_string())?;
    let clients = clients_by_node(&context).await?;
    let mut target = None;
    for (node, client) in &clients {
        if node == &leader {
            continue;
        }
        if client.status().await.is_ok_and(|instances| {
            instances
                .iter()
                .any(|instance| instance.app_name == app && instance.namespace == context.namespace)
        }) {
            target = Some((node.clone(), client.clone()));
            break;
        }
    }
    let (target_node, target_client) =
        target.ok_or_else(|| "no non-leader node hosts the deployment".to_string())?;
    let (_, observer) = clients
        .iter()
        .find(|(node, _)| node.as_str() != target_node)
        .map(|(node, client)| (node.as_str(), client.clone()))
        .ok_or_else(|| "no deployment observer node".to_string())?;

    let redeploy_context = context.clone();
    let changed = container_spec(&context, app, 4, true);
    let redeploy = tokio::spawn(async move { redeploy_context.apply(&changed).await });
    let mut observed_operation = None;
    while !redeploy.is_finished() && !context.deadline.remaining().is_zero() {
        if let Ok(operations) = observer.deploy_operations().await
            && let Some(operation) = operations.active_deploys.iter().find(|operation| {
                operation
                    .targets
                    .iter()
                    .any(|target| target.name == app && target.namespace == context.namespace)
            })
        {
            observed_operation = Some(operation.id.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let Some(observed_operation) = observed_operation else {
        let _ = redeploy.await;
        return unknown("the rolling deploy completed before a live operation could be observed");
    };

    inject_node_fault(
        &context,
        target_client,
        node_request(
            &context,
            FaultType::NodeKill {
                kill_containers: true,
            },
            &target_node,
            false,
            "phase15 C5 node death during deploy",
        ),
    )
    .await?;
    let overlap = observer
        .deploy_operations()
        .await
        .map_err(|error| format!("could not verify deploy/fault overlap: {error}"))?
        .active_deploys
        .iter()
        .any(|operation| operation.id == observed_operation);
    if !overlap {
        let _ = redeploy.await;
        clear_owned_faults(&context).await?;
        return unknown(
            "the deploy ended before the active operation and node fault could be observed together",
        );
    }
    let _deploy_result = redeploy
        .await
        .map_err(|error| format!("redeploy task panicked: {error}"))?;
    clear_owned_faults(&context).await?;
    wait_for_node_state(&context, &observer, &target_node, &["alive"]).await?;
    context.wait_running_cluster(app, 4).await?;

    loop {
        let operations = observer
            .deploy_operations()
            .await
            .map_err(|error| format!("could not inspect deploy outcome: {error}"))?;
        if !operations
            .active_deploys
            .iter()
            .any(|operation| operation.id == observed_operation)
        {
            let outcome = operations
                .history
                .iter()
                .find(|operation| operation.id == observed_operation)
                .and_then(|entry| entry.outcome);
            if outcome == Some(DeployOperationOutcome::Unknown) || outcome.is_none() {
                return Err((format!(
                    "deploy {observed_operation} ended without a defined terminal outcome: {outcome:?}"
                )).into());
            }
            return Ok(());
        }
        if context.deadline.remaining().is_zero() {
            return Err(("deploy remained active after node recovery".to_string()).into());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// All required chaos scenarios in acceptance order.
pub fn all() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "leader_failure_elects_new_leader_and_cluster_recovers",
            group: TestGroup::Chaos,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ContainerRuntime,
                Capability::NodeKill,
            ],
            run: crate::testkit_case!(leader_failure_elects_new_leader_and_cluster_recovers),
        },
        TestCase {
            name: "dead_worker_node_has_workloads_rescheduled",
            group: TestGroup::Chaos,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ContainerRuntime,
                Capability::NodeKill,
            ],
            run: crate::testkit_case!(dead_worker_node_has_workloads_rescheduled),
        },
        TestCase {
            name: "minority_partition_degrades_and_heals",
            group: TestGroup::Chaos,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ContainerRuntime,
                Capability::NodeKill,
            ],
            run: crate::testkit_case!(minority_partition_degrades_and_heals),
        },
        TestCase {
            name: "resource_exhaustion_degrades_gracefully",
            group: TestGroup::Chaos,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ContainerRuntime,
                Capability::NodePressure,
            ],
            run: crate::testkit_case!(resource_exhaustion_degrades_gracefully),
        },
        TestCase {
            name: "node_death_during_deploy_ends_clean",
            group: TestGroup::Chaos,
            requires: &[
                Capability::Cluster,
                Capability::MultiNode,
                Capability::ContainerRuntime,
                Capability::NodeKill,
            ],
            run: crate::testkit_case!(node_death_during_deploy_ends_clean),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::context::SIGTERM_TRAP;

    /// From a laptop the target's own API is out of reach, and the relay
    /// refuses fault requests. The node-kill must go to the entry node, which
    /// routes it (and the reversal) to the target it names.
    #[tokio::test]
    async fn relayed_node_faults_are_injected_and_reversed_through_the_entry_node() {
        use axum::extract::{Query, State};
        use axum::routing::{delete, post};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        type Seen = Arc<Mutex<Vec<String>>>;
        async fn inject(
            State(seen): State<Seen>,
            axum::Json(request): axum::Json<FaultRequest>,
        ) -> axum::Json<FaultSummary> {
            let target = request.target_node.clone().unwrap_or_default();
            seen.lock().await.push(format!("inject {target}"));
            axum::Json(FaultSummary {
                id: 7,
                fault_type: "node-kill".to_string(),
                target_service: String::new(),
                target_instance: None,
                target_node: request.target_node,
                remaining_secs: 30,
                injected_by: "test".to_string(),
                node: None,
                routed: Vec::new(),
            })
        }
        async fn clear(
            State(seen): State<Seen>,
            axum::extract::Path(id): axum::extract::Path<u64>,
            Query(query): Query<std::collections::HashMap<String, String>>,
        ) -> axum::Json<serde_json::Value> {
            let node = query.get("node").cloned().unwrap_or_default();
            seen.lock().await.push(format!("clear {id} {node}"));
            axum::Json(serde_json::json!({ "message": "cleared" }))
        }

        let seen: Seen = Arc::default();
        let entry = axum::Router::new()
            .route("/v1/fault", post(inject))
            .route("/v1/fault/{id}", delete(clear))
            .with_state(Arc::clone(&seen));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, entry).await.unwrap() });

        let timeout = Duration::from_secs(5);
        let context = TestContext {
            client: BunClient::new_with_token(&base, None),
            namespace: "rbtest-chaos-00".to_string(),
            lease_id: None,
            chaos_guard: crate::testkit::chaos::ChaosGuard::default(),
            capabilities: crate::bun::capabilities::ClusterCapabilities::default(),
            timeout,
            deadline: crate::testkit::deadline::Deadline::after(timeout).unwrap(),
            peer_route: crate::testkit::context::PeerRoute::Relay,
            wait_note: Default::default(),
        };
        let relayed_worker = context.client.via_node("worker-2").unwrap();

        inject_node_fault(
            &context,
            relayed_worker,
            node_request(
                &context,
                FaultType::NodeKill {
                    kill_containers: true,
                },
                "worker-2",
                false,
                "relay test",
            ),
        )
        .await
        .unwrap();
        clear_owned_faults(&context).await.unwrap();
        server.abort();

        assert_eq!(
            *seen.lock().await,
            vec![
                "inject worker-2".to_string(),
                "clear 7 worker-2".to_string()
            ]
        );
    }

    fn offline_context() -> TestContext {
        let timeout = Duration::from_secs(5);
        TestContext {
            client: BunClient::new_with_token("http://127.0.0.1:9", None),
            namespace: "rbtest-chaos-00".to_string(),
            lease_id: None,
            chaos_guard: crate::testkit::chaos::ChaosGuard::default(),
            capabilities: crate::bun::capabilities::ClusterCapabilities::default(),
            timeout,
            deadline: crate::testkit::deadline::Deadline::after(timeout).unwrap(),
            peer_route: crate::testkit::context::PeerRoute::Relay,
            wait_note: Default::default(),
        }
    }

    /// A workload answers its health check only from a file it wrote itself,
    /// and names every external program by absolute path. The pinned BusyBox image
    /// has no `/etc/hostname` and no `PATH`, so anything else never turns
    /// healthy on a real container runtime.
    fn assert_self_served(spec: &str, app: &str) {
        let config = Config::parse(spec).unwrap();
        let app_spec = &config.app[app];
        let health = app_spec.health.as_ref().expect("a health check");
        assert_eq!(
            &app_spec.command[..2],
            ["/bin/sh", "-c"],
            "{:?}",
            app_spec.command
        );
        let wrapped = &app_spec.command[2];
        let script = wrapped
            .strip_prefix(SIGTERM_TRAP)
            .and_then(|body| body.strip_suffix(" & wait"))
            .unwrap_or_else(|| panic!("{wrapped:?} doesn't exit on SIGTERM"));
        let root = script
            .rsplit(" -h ")
            .next()
            .expect("httpd serves a named directory")
            .trim();
        assert!(
            script.contains(&format!("> {root}{}", health.path)),
            "{script:?} never writes the {} it is health-checked on",
            health.path
        );
        for step in script.split(';') {
            let program = step
                .split_whitespace()
                .find(|word| *word != "exec")
                .expect("no empty steps");
            // `printf` is built into BusyBox's shell and needs no PATH.
            assert!(
                program.starts_with('/') || program == "printf",
                "{program:?} relies on the image's PATH in {script:?}"
            );
        }
    }

    /// V02 soak: C2 waited all 600 s for three running replicas because its
    /// httpd served `/etc`, which has no `hostname` in the pinned image.
    #[test]
    fn chaos_workloads_answer_their_health_check_without_image_defaults() {
        let context = offline_context();
        for delayed in [false, true] {
            assert_self_served(
                &container_spec(&context, "chaos-c2-reschedule", 3, delayed),
                "chaos-c2-reschedule",
            );
        }
        assert_self_served(&context.container_http_spec("web", 1), "web");
    }

    #[test]
    fn fault_expiry_outlives_the_case_deadline() {
        assert_eq!(
            fault_duration(Duration::from_secs(120)),
            Duration::from_secs(150)
        );
    }
}
