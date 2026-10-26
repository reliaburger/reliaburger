use super::commands::{AgentCommand, ApplyEvent};
use super::deploy_ops::{DeployOp, DeployOps, retry_while_release_pending};
use super::deploy_worker::DeployWorker;
use super::health_checks::probe_host;
use super::status::{CouncilMemberInfo, CouncilStatus, InstanceStatus, NodeStatus};
use super::trace::{MAX_CONCURRENT_TRACES, trace_dns_command, trace_dns_step, trace_tcp_command};
use super::*;
use crate::grill::mock::MockGrill;

#[cfg(target_os = "linux")]
mod cgroup_faults;
mod fault_coverage;
mod job_faults;
mod loop_harness;
mod loop_rule;
mod namespace_secrets;
mod published_status;
mod restart_ownership;
mod runtime_knobs;
mod runtime_metadata;
mod secret_providers;
mod sidecars;
mod stop_hooks;
mod upgrade_answer;

#[test]
fn trace_targets_are_positional_arguments_not_shell_source() {
    let hostile = "api; touch /tmp/never";
    let dns = trace_dns_command(hostile);
    let tcp = trace_tcp_command(hostile, 443, 3, 2);
    assert!(!dns[2].contains(hostile));
    assert_eq!(dns[4], hostile);
    assert!(!tcp[2].contains(hostile));
    assert_eq!(tcp[4], hostile);
    assert_eq!(tcp[5], "443");
    assert_eq!(tcp[6], "3");
}

#[test]
fn missing_workload_probe_tool_is_unknown_not_a_network_failure() {
    let step = trace_dns_step(
        "api.internal",
        Ok(crate::onion::trace::ProbeOutput {
            status: 127,
            lines: vec!["nslookup: not found".to_string()],
            attempts: Vec::new(),
        }),
        None,
    );
    assert!(matches!(
        step.verdict,
        crate::onion::trace::TraceVerdict::Unknown { .. }
    ));
}

#[tokio::test]
async fn trace_runs_fixed_dns_and_tcp_probes_from_the_source_workload() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let handle = tokio::spawn(async move { agent.run().await });
    let config = Config::parse(
        r#"
            [app.source]
            image = "source:v1"

            [app.destination]
            image = "destination:v1"
            port = 8080
            "#,
    )
    .unwrap();
    expect_complete(&send_deploy(&tx, config).await);

    let vip = crate::onion::vip::VirtualIP::from_qualified("default__destination");
    grill.set_exec_outputs([
            format!(
                "Name: destination.default.internal\nAddress: {vip}\n__RB_TRACE_DNS_STATUS__=0\n"
            ),
            "__RB_TRACE_TCP_ATTEMPT__=0 1727000000000000000 1727000000002500000\n__RB_TRACE_TCP_STATUS__=0\n"
                .to_string(),
        ]);
    grill.block_execs();
    let (response, receiver) = oneshot::channel();
    tx.send(AgentCommand::Trace {
        request: crate::onion::trace::TraceRequest {
            source: "source".to_string(),
            source_namespace: "default".to_string(),
            destination: "destination".to_string(),
            destination_namespace: "default".to_string(),
            port: None,
            count: None,
        },
        internal_destination: true,
        source_node: "node-a".to_string(),
        response,
    })
    .await
    .unwrap();

    grill.wait_for_execs(1).await;
    let (status_response, status_receiver) = oneshot::channel();
    tx.send(AgentCommand::Status {
        response: status_response,
    })
    .await
    .unwrap();
    let statuses = tokio::time::timeout(std::time::Duration::from_secs(1), status_receiver)
        .await
        .expect("a workload trace must not block the agent command loop")
        .unwrap();
    assert_eq!(statuses.len(), 2);
    grill.release_execs(1);

    let result = receiver.await.unwrap().unwrap();

    assert_eq!(result.steps.len(), 5);
    assert_eq!(
        result.steps[0].verdict,
        crate::onion::trace::TraceVerdict::Pass
    );
    assert_eq!(
        result.steps[1].evidence,
        crate::onion::trace::TraceEvidence::Inferred
    );
    assert_eq!(
        result.steps[4].verdict,
        crate::onion::trace::TraceVerdict::Pass
    );
    assert!(matches!(
        result.steps[2].verdict,
        crate::onion::trace::TraceVerdict::Unknown { .. }
    ));
    assert!(matches!(
        result.overall_result,
        crate::onion::trace::TraceVerdict::Unknown { .. }
    ));
    assert_eq!(result.latency_ms, Some(2.5));

    shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn a_trace_lists_only_the_faults_that_act_on_its_own_path() {
    use crate::smoker::types::{FaultRequest, FaultType};
    let (mut agent, _tx, _shutdown) = test_agent();
    let mut inject = |fault_type: FaultType, service: &str, namespace: &str| {
        agent
            .fault_registry
            .insert(&FaultRequest {
                workload: Default::default(),
                task_index: None,
                fault_type,
                target_service: service.to_string(),
                namespace: Some(namespace.to_string()),
                target_instance: None,
                target_node: None,
                duration: std::time::Duration::from_secs(60),
                injected_by: "test".to_string(),
                reason: None,
                include_leader: false,
                override_safety: false,
                acknowledged: true,
            })
            .id
            .0
    };
    let partition = inject(
        FaultType::Partition {
            source_app: Some("frontend".to_string()),
        },
        "redis",
        "default",
    );
    let delay = inject(
        FaultType::Delay {
            delay_ns: 300_000_000,
            jitter_ns: 0,
            source_app: None,
        },
        "redis",
        "default",
    );
    inject(
        FaultType::Partition {
            source_app: Some("backend".to_string()),
        },
        "redis",
        "default",
    );
    inject(FaultType::Drop { probability: 50 }, "redis", "team-b");
    inject(FaultType::DnsNxdomain, "backend", "default");
    inject(FaultType::Pause, "redis", "default");

    let faults = agent.path_faults(&crate::onion::trace::TraceRequest {
        source: "frontend".to_string(),
        source_namespace: "default".to_string(),
        destination: "redis".to_string(),
        destination_namespace: "default".to_string(),
        port: None,
        count: None,
    });
    let listed: Vec<(u64, &str)> = faults
        .iter()
        .map(|fault| (fault.id, fault.description.as_str()))
        .collect();
    assert_eq!(
        listed,
        vec![
            (partition, "partition from frontend"),
            (delay, "delay 300ms"),
        ]
    );
}

#[tokio::test]
async fn trace_requires_an_exact_dns_answer_not_server_or_name_text() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let handle = tokio::spawn(async move { agent.run().await });
    expect_complete(
        &send_deploy(
            &tx,
            Config::parse(
                r#"
            [app.source]
            image = "source:v1"
            [app.destination]
            image = "destination:v1"
            port = 8080
        "#,
            )
            .unwrap(),
        )
        .await,
    );
    let vip = crate::onion::vip::VirtualIP::from_qualified("default__destination");
    let mut verdicts = Vec::new();
    for answer in [
        format!(
            "Server: resolver\nAddress: {vip}#53\nName: destination.default.internal\nAddress: 127.0.0.1"
        ),
        format!("Name: {vip}.invalid\nAddress: {vip}0"),
    ] {
        grill.set_exec_outputs([
            format!("{answer}\n__RB_TRACE_DNS_STATUS__=0\n"),
            "__RB_TRACE_TCP_STATUS__=0\n".into(),
        ]);
        let (response, receiver) = oneshot::channel();
        tx.send(AgentCommand::Trace {
            request: crate::onion::trace::TraceRequest {
                source: "source".into(),
                source_namespace: "default".into(),
                destination: "destination".into(),
                destination_namespace: "default".into(),
                port: None,
                count: None,
            },
            internal_destination: true,
            source_node: "node-a".into(),
            response,
        })
        .await
        .unwrap();
        verdicts.push(receiver.await.unwrap().unwrap().steps[0].verdict.clone());
    }
    shutdown.cancel();
    handle.await.unwrap();
    assert!(
        verdicts
            .iter()
            .all(|verdict| matches!(verdict, crate::onion::trace::TraceVerdict::Fail { .. })),
        "{verdicts:?}"
    );
}

#[tokio::test]
async fn trace_concurrency_is_bounded_without_queueing_more_workload_processes() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let handle = tokio::spawn(async move { agent.run().await });
    let config = Config::parse(
        r#"
            [app.source]
            image = "source:v1"

            [app.destination]
            image = "destination:v1"
            port = 8080
            "#,
    )
    .unwrap();
    expect_complete(&send_deploy(&tx, config).await);

    grill.set_exec_outputs(
        (0..16).map(|_| "__RB_TRACE_DNS_STATUS__=0\n__RB_TRACE_TCP_STATUS__=0\n".to_string()),
    );
    grill.block_execs();
    let request = crate::onion::trace::TraceRequest {
        source: "source".to_string(),
        source_namespace: "default".to_string(),
        destination: "destination".to_string(),
        destination_namespace: "default".to_string(),
        port: None,
        count: None,
    };
    let mut active_receivers = Vec::new();
    for _ in 0..MAX_CONCURRENT_TRACES {
        let (response, receiver) = oneshot::channel();
        tx.send(AgentCommand::Trace {
            request: request.clone(),
            internal_destination: true,
            source_node: "node-a".to_string(),
            response,
        })
        .await
        .unwrap();
        active_receivers.push(receiver);
    }
    grill
        .wait_for_execs(MAX_CONCURRENT_TRACES.try_into().unwrap())
        .await;

    let (response, receiver) = oneshot::channel();
    tx.send(AgentCommand::Trace {
        request,
        internal_destination: true,
        source_node: "node-a".to_string(),
        response,
    })
    .await
    .unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), receiver)
        .await
        .expect("the excess trace must be refused without joining a queue")
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, BunError::TraceBusy));

    grill.release_execs(MAX_CONCURRENT_TRACES);
    for receiver in active_receivers {
        let _ = receiver.await;
    }
    shutdown.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn shutdown_cancels_an_in_flight_workload_trace() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let handle = tokio::spawn(async move { agent.run().await });
    let config = Config::parse(
        r#"
            [app.source]
            image = "source:v1"

            [app.destination]
            image = "destination:v1"
            port = 8080
            "#,
    )
    .unwrap();
    expect_complete(&send_deploy(&tx, config).await);

    grill.block_execs();
    let (response, receiver) = oneshot::channel();
    tx.send(AgentCommand::Trace {
        request: crate::onion::trace::TraceRequest {
            source: "source".to_string(),
            source_namespace: "default".to_string(),
            destination: "destination".to_string(),
            destination_namespace: "default".to_string(),
            port: None,
            count: None,
        },
        internal_destination: true,
        source_node: "node-a".to_string(),
        response,
    })
    .await
    .unwrap();
    grill.wait_for_execs(1).await;

    shutdown.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), receiver)
        .await
        .expect("shutdown must cancel the trace probe")
        .unwrap()
        .unwrap();
    assert!(matches!(
        result.overall_result,
        crate::onion::trace::TraceVerdict::Unknown { .. }
    ));
    handle.await.unwrap();
}

#[cfg(all(feature = "ebpf", target_os = "linux"))]
#[tokio::test]
async fn egress_health_waits_for_preparation_but_fences_unbound_execution() {
    for stage in [
        ContainerState::Pending,
        ContainerState::Preparing,
        ContainerState::Initialising,
        ContainerState::Starting,
        ContainerState::HealthWait,
        ContainerState::Running,
        ContainerState::Unhealthy,
        ContainerState::Stopping,
    ] {
        let (mut agent, _tx, _shutdown) = test_agent();
        let mut spec = Config::parse("[app.web]\nimage = 'mock:image'\n")
            .unwrap()
            .app
            .remove("web")
            .unwrap();
        let ids = agent
            .supervisor
            .deploy_app("web", "default", &spec, Instant::now())
            .await
            .unwrap();
        spec.egress = Config::parse(
            "[app.web]\nimage = 'mock:image'\n[app.web.egress]\nallow = ['203.0.113.9:443']\n",
        )
        .unwrap()
        .app
        .remove("web")
        .unwrap()
        .egress;
        agent
            .deployed_specs
            .insert(("web".into(), "default".into()), spec);
        agent.supervisor.get_instance_mut(&ids[0]).unwrap().state = stage;
        // Missing kernel ownership is expected before pre-start programming,
        // but must remain a fail-closed condition from init/start onwards.
        agent.enforce_live_egress_or_stop().await;
        let actual = agent.supervisor.get_instance(&ids[0]).unwrap().state;
        if matches!(stage, ContainerState::Pending | ContainerState::Preparing) {
            assert_eq!(
                actual, stage,
                "preparation was stopped before policy installation"
            );
            assert!(agent.egress_affected_workloads.is_empty());
        } else {
            assert!(
                matches!(actual, ContainerState::Stopping | ContainerState::Stopped),
                "{stage:?} remained {actual:?}"
            );
            assert!(
                agent
                    .egress_affected_workloads
                    .contains(&("web".into(), "default".into()))
            );
        }
    }
}

#[tokio::test]
async fn health_tick_reuses_egress_evidence_but_later_reports_refresh_it() {
    use std::sync::atomic::Ordering;
    let (mut agent, _tx, _shutdown) = test_agent();
    let readiness = crate::bun::readiness::ReadinessTracker::new();
    agent.set_readiness_tracker(readiness.clone());
    readiness
        .set_capabilities(crate::meat::cluster_state::NodeCapabilities {
            egress: crate::sesame::egress::EgressEnforcementCapability {
                connect_ipv4: true,
                connect_ipv6: true,
                udp_ipv4: true,
                udp_ipv6: true,
                pre_start: true,
            },
            ..Default::default()
        })
        .await;
    agent.refresh_egress_readiness().await;
    assert!(
        !readiness
            .capability_snapshot()
            .await
            .egress
            .can_enforce_allowlist()
    );
    assert_eq!(agent.egress_observation_count.load(Ordering::Relaxed), 1);
    agent.refresh_egress_readiness().await;
    assert_eq!(agent.egress_observation_count.load(Ordering::Relaxed), 2);
    let (capabilities, _) = agent.live_egress_report_state().await;
    assert!(!capabilities.egress.can_enforce_allowlist());
    assert_eq!(agent.egress_observation_count.load(Ordering::Relaxed), 3);
}

struct TestAgent {
    agent: BunAgent<MockGrill>,
    // Fields drop in declaration order: keep filesystem ownership through
    // agent teardown, including when the fixture moves into a spawned task.
    _volumes: tempfile::TempDir,
}

impl std::ops::Deref for TestAgent {
    type Target = BunAgent<MockGrill>;

    fn deref(&self) -> &Self::Target {
        &self.agent
    }
}

impl std::ops::DerefMut for TestAgent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.agent
    }
}

fn test_agent() -> (TestAgent, mpsc::Sender<AgentCommand>, CancellationToken) {
    let (agent, tx, shutdown, _grill) = test_agent_with_grill();
    (agent, tx, shutdown)
}

#[tokio::test]
async fn service_registration_refuses_a_closed_agent_channel() {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let result = DeployOps { tx }
        .register_service_app("api", "default", 8080, None)
        .await;
    assert!(matches!(result, Err(BunError::BackendPublication { .. })));
}

#[tokio::test]
async fn reporting_binds_execution_to_the_original_runtime_specification() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let spec = agent
        .supervisor
        .get_instance(&id)
        .unwrap()
        .oci_spec
        .clone()
        .unwrap();
    for token in [
        "first-original-generation",
        "replacement-original-generation",
    ] {
        let launch = crate::grill::RuntimeLaunch {
            instance_id: id.clone(),
            spec: spec.clone(),
            generation: crate::grill::RuntimeGeneration::process(token),
            network_reference: None,
        };
        let expected = crate::grill::RuntimeExecution {
            instance_id: id.clone(),
            generation: launch.generation.clone(),
        };
        grill.set_launch_inventory(vec![launch.clone()]).await;
        let (tx, rx) = oneshot::channel();
        agent
            .handle_snapshot_request(CollectSnapshotRequest { response: tx })
            .await;
        assert_eq!(rx.await.unwrap().instances[0].execution, Some(expected));
        let mut wrong_spec = launch.clone();
        wrong_spec
            .spec
            .process
            .args
            .push("different-runtime-spec".into());
        for invalid in [vec![wrong_spec], vec![launch.clone(), launch], vec![]] {
            grill.set_launch_inventory(invalid).await;
            let (tx, rx) = oneshot::channel();
            agent
                .handle_snapshot_request(CollectSnapshotRequest { response: tx })
                .await;
            let report = rx.await.unwrap();
            assert_eq!(
                report.instances.len(),
                1,
                "missing identity must not hide resource commitments"
            );
            assert!(report.instances[0].execution.is_none());
        }
    }
}

/// #476: a retired Runc intent keeps environment names, not values. Its
/// launch must still bind to the instance that started it.
#[tokio::test]
async fn reporting_binds_a_retired_launch_whose_environment_values_were_scrubbed() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let config = Config::parse(
        "[app.web]\nimage = 'web:v1'\nport = 8080\n[app.web.env]\nAPI_TOKEN = 'decrypted'\n",
    )
    .unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    let id = InstanceId("default__web-0".into());
    let spec = agent
        .supervisor
        .get_instance(&id)
        .unwrap()
        .oci_spec
        .clone()
        .unwrap();
    assert!(spec.process.env.contains(&"API_TOKEN=decrypted".into()));
    let launch = crate::grill::RuntimeLaunch {
        instance_id: id.clone(),
        spec: spec.without_environment_values(),
        generation: crate::grill::RuntimeGeneration::process("retired"),
        network_reference: None,
    };
    let expected = crate::grill::RuntimeExecution {
        instance_id: id.clone(),
        generation: launch.generation.clone(),
    };
    grill.set_launch_inventory(vec![launch.clone()]).await;
    let (tx, rx) = oneshot::channel();
    agent
        .handle_snapshot_request(CollectSnapshotRequest { response: tx })
        .await;
    assert_eq!(rx.await.unwrap().instances[0].execution, Some(expected));

    // Dropping a variable is still a different request.
    let mut other = launch;
    other.spec.process.env.clear();
    grill.set_launch_inventory(vec![other]).await;
    let (tx, rx) = oneshot::channel();
    agent
        .handle_snapshot_request(CollectSnapshotRequest { response: tx })
        .await;
    assert!(rx.await.unwrap().instances[0].execution.is_none());
}

/// Job leases and app replicas share one budget. When jobs hold it, a
/// fresh deploy fails for capacity, and the state report says so (and how
/// much the jobs hold) so the leader can place the replica elsewhere.
#[tokio::test]
async fn capacity_refusal_and_job_reservations_reach_the_state_snapshot() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    // Requests come with limits, which only a container runtime enforces.
    grill.set_runtime_kind(crate::grill::records::RuntimeKind::Runc);
    let budget = agent.execution_budget();
    budget.set_capacity(crate::meat::Resources::new(1000, 1 << 30, 0));
    let held = crate::meat::Resources::new(1000, 1 << 29, 0);
    let attempt = budget
        .acquire(held, &CancellationToken::new())
        .await
        .unwrap();
    let config =
        Config::parse("[app.web]\nimage = 'web:v1'\ncpu = '500m'\nmemory = '64Mi'\n").unwrap();

    let events = drain_deploy(&mut agent, config.clone()).await;
    assert!(
        matches!(events.last(), Some(ApplyEvent::Error { .. })),
        "{events:?}"
    );
    let (tx, rx) = oneshot::channel();
    agent
        .handle_snapshot_request(CollectSnapshotRequest { response: tx })
        .await;
    let snapshot = rx.await.unwrap();
    assert_eq!(
        snapshot.job_reservations,
        crate::reporting::types::JobReservations {
            cpu_millicores: 1000,
            memory_bytes: 1 << 29,
        }
    );
    assert_eq!(
        snapshot.capacity_refusals,
        vec![crate::reporting::types::CapacityRefusal {
            app_name: "web".into(),
            namespace: "default".into(),
            ordinals: vec![0],
        }]
    );

    drop(attempt);
    expect_complete(&drain_deploy(&mut agent, config).await);
    let (tx, rx) = oneshot::channel();
    agent
        .handle_snapshot_request(CollectSnapshotRequest { response: tx })
        .await;
    let snapshot = rx.await.unwrap();
    assert!(snapshot.capacity_refusals.is_empty());
    assert_eq!(snapshot.job_reservations, Default::default());
}

/// The apps the agent's next state report says it refused for capacity,
/// with the replica ordinals it couldn't start.
async fn reported_refusals(agent: &mut TestAgent) -> Vec<(String, String, Vec<u32>)> {
    let (tx, rx) = oneshot::channel();
    agent
        .handle_snapshot_request(CollectSnapshotRequest { response: tx })
        .await;
    rx.await
        .unwrap()
        .capacity_refusals
        .into_iter()
        .map(|refusal| (refusal.app_name, refusal.namespace, refusal.ordinals))
        .collect()
}

/// An agent whose 1000m budget runs one 500m replica of `web`, with a job
/// lease holding the other 500m. Returns the lease, which frees the room.
async fn agent_with_web_beside_a_job_lease() -> (
    TestAgent,
    MockGrill,
    crate::bun::execution_budget::ResourceLease,
) {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    // Requests come with limits, which only a container runtime enforces.
    grill.set_runtime_kind(crate::grill::records::RuntimeKind::Runc);
    let budget = agent.execution_budget();
    budget.set_capacity(crate::meat::Resources::new(1000, 1 << 30, 0));
    expect_complete(&drain_deploy(&mut agent, budgeted_web("web:v1", 1)).await);
    let job = budget
        .acquire(
            crate::meat::Resources::new(500, 1 << 29, 0),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    (agent, grill, job)
}

/// An agent whose 1000m budget a job lease holds whole, so a fresh deploy
/// of `web` is refused. Returns the lease.
async fn agent_with_a_full_budget() -> (
    TestAgent,
    MockGrill,
    crate::bun::execution_budget::ResourceLease,
) {
    let (agent, _, _, grill) = test_agent_with_grill();
    grill.set_runtime_kind(crate::grill::records::RuntimeKind::Runc);
    let budget = agent.execution_budget();
    budget.set_capacity(crate::meat::Resources::new(1000, 1 << 30, 0));
    let job = budget
        .acquire(
            crate::meat::Resources::new(1000, 1 << 29, 0),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    (agent, grill, job)
}

fn budgeted_web(image: &str, replicas: u32) -> Config {
    Config::parse(&format!(
        "[app.web]\nimage = '{image}'\nreplicas = {replicas}\ncpu = '500m'\nmemory = '64Mi'\n"
    ))
    .unwrap()
}

/// `web`'s refusal, naming the replica ordinals it couldn't start.
fn web_refused(ordinals: &[u32]) -> Vec<(String, String, Vec<u32>)> {
    vec![("web".to_string(), "default".to_string(), ordinals.to_vec())]
}

/// #967: a replica added beside running ones leases from the same budget as
/// a fresh deploy. When job leases fill it, the leader must hear about the
/// refusal, or it keeps the replica on a node that can never start it. The
/// refusal names only the replica the scale-up tried to add (#971).
#[tokio::test]
async fn capacity_refusal_while_adding_replicas_reaches_the_state_snapshot() {
    let (mut agent, _grill, job) = agent_with_web_beside_a_job_lease().await;

    let events = drain_deploy(&mut agent, budgeted_web("web:v1", 2)).await;
    assert!(
        events.iter().any(|event| matches!(event,
            ApplyEvent::Progress { message } if message.contains("beside the running ones"))),
        "the scale-up must take the add-replicas path: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(ApplyEvent::Error { message }) if message.contains("capacity")),
        "{events:?}"
    );
    assert_eq!(reported_refusals(&mut agent).await, web_refused(&[1]));
    drop(job);
}

/// #971: the refused scale-up started nothing, so its retry adds the same
/// replica in place again. It used to find its own spec already stored and
/// roll the app instead, which needs room for both new replicas beside the
/// old one: more than the scale-up ever asked for.
#[tokio::test]
async fn a_refused_scale_up_retries_in_place_with_the_room_it_needed() {
    let (mut agent, _grill, job) = agent_with_web_beside_a_job_lease().await;
    let before = live_web_ids(&agent);
    drain_deploy(&mut agent, budgeted_web("web:v1", 2)).await;
    assert_eq!(reported_refusals(&mut agent).await, web_refused(&[1]));

    // Room for the one added replica, and no more.
    drop(job);
    let events = drain_deploy(&mut agent, budgeted_web("web:v1", 2)).await;

    expect_complete(&events);
    assert!(
        events.iter().any(|event| matches!(event,
            ApplyEvent::Progress { message } if message.contains("beside the running ones"))),
        "the retry must add in place, not roll: {events:?}"
    );
    let after = live_web_ids(&agent);
    assert_eq!(after.len(), 2, "{after:?}");
    assert!(
        after.contains(&before[0]),
        "the serving replica was replaced"
    );
    assert!(reported_refusals(&mut agent).await.is_empty());
}

/// #971: once the leader moves the replica this node refused, the node's
/// share is what it already runs. That deploy changes nothing, so it must
/// not roll the healthy replica, and the refusal it answered goes away.
#[tokio::test]
async fn the_share_left_after_a_refused_replica_moves_is_not_rolled() {
    let (mut agent, _grill, _job) = agent_with_web_beside_a_job_lease().await;
    let before = live_web_ids(&agent);
    drain_deploy(&mut agent, budgeted_web("web:v1", 2)).await;
    assert_eq!(reported_refusals(&mut agent).await, web_refused(&[1]));

    let events = drain_deploy(&mut agent, budgeted_web("web:v1", 1)).await;

    expect_complete(&events);
    assert_eq!(
        live_web_ids(&agent),
        before,
        "the serving replica was replaced"
    );
    assert!(reported_refusals(&mut agent).await.is_empty());
}

/// #967: a rolling replacement reserves its new instance from the budget
/// before the old one leaves, so job leases can refuse it too.
#[tokio::test]
async fn capacity_refusal_while_reserving_a_rolling_replacement_reaches_the_state_snapshot() {
    let (mut agent, _grill, job) = agent_with_web_beside_a_job_lease().await;

    let events = drain_deploy(&mut agent, budgeted_web("web:v2", 1)).await;
    assert!(
        events.iter().any(|event| matches!(event,
            ApplyEvent::Error { message } if message.contains("capacity"))),
        "{events:?}"
    );
    assert_eq!(reported_refusals(&mut agent).await, web_refused(&[0]));

    drop(job);
    expect_complete(&drain_deploy(&mut agent, budgeted_web("web:v2", 1)).await);
    assert!(reported_refusals(&mut agent).await.is_empty());
}

/// #967: the node's deploy retries back off to five minutes. A refusal that
/// lapsed after a minute was missing from most reports in between, and a
/// leader pass in that gap kept the replica where it couldn't start.
#[tokio::test]
async fn capacity_refusal_stays_reported_while_the_deploy_backs_off() {
    let (mut agent, _grill, _job) = agent_with_a_full_budget().await;
    drain_deploy(&mut agent, budgeted_web("web:v1", 1)).await;
    assert_eq!(reported_refusals(&mut agent).await, web_refused(&[0]));

    tokio::time::pause();
    tokio::time::advance(
        crate::cluster::deploy_backoff::MAX_DEPLOY_BACKOFF + std::time::Duration::from_secs(60),
    )
    .await;
    tokio::time::resume();

    assert_eq!(reported_refusals(&mut agent).await, web_refused(&[0]));
}

/// #967: once the leader moves the app off this node, the node retires it,
/// and the refusal goes with it. It was about a placement that no longer
/// exists, and would otherwise move the app off this node again later.
#[tokio::test]
async fn retiring_a_refused_app_withdraws_its_capacity_refusal() {
    let (mut agent, _grill, _job) = agent_with_a_full_budget().await;
    drain_deploy(&mut agent, budgeted_web("web:v1", 1)).await;
    assert_eq!(reported_refusals(&mut agent).await, web_refused(&[0]));

    let (response, _answer) = oneshot::channel();
    agent
        .request_app_stop(
            "web".into(),
            "default".into(),
            StopPurpose::Retire,
            response,
        )
        .await;

    assert!(reported_refusals(&mut agent).await.is_empty());
}

#[tokio::test]
async fn late_discovery_subscribers_receive_the_latest_service_snapshot() {
    let (mut agent, _commands, _shutdown) = test_agent();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let view = agent.service_map_watch();
    let snapshot = view.borrow();
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let entry = snapshot
        .resolve(&service)
        .expect("late subscriber lost the completed deployment");
    assert_eq!(entry.backends.len(), 1);
    assert_eq!(entry.backends[0].instance_id, "default__web-0");
    assert!(entry.backends[0].healthy);
}

#[tokio::test]
async fn fresh_discovery_refuses_adoption_without_original_inventory() {
    let (mut original, _, _, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    original.set_records_dir(records.path().to_owned());
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut original, basic_config()).await);
    let (mut replacement, _, _, recovered) = test_agent_with_grill();
    replacement.set_records_dir(records.path().to_owned());
    replacement
        .enable_fresh_discovery_ownership(&records.path().join("discovery"))
        .await
        .unwrap();
    let result = replacement.adopt_recorded_instances().await;
    assert!(
        matches!(result, Err(BunError::AdoptionState(_))),
        "missing discovery recovery was accepted: {result:?}"
    );
    assert!(
        !recovered
            .calls()
            .iter()
            .any(|(operation, _)| operation == "adopt" || operation == "kill"),
        "runtime recovery ran before original discovery reconciliation"
    );
    assert!(crate::grill::records::record_path(records.path(), "default__web-0").exists());
}

#[tokio::test]
async fn fresh_discovery_enablement_refuses_existing_consumer_obligations() {
    let (mut agent, _, _) = test_agent();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    let journal = crate::bun::discovery_owners::DiscoveryJournal::open_async(&path)
        .await
        .unwrap();
    let inventory = serde_json::from_value(serde_json::json!({
        "services": [], "references": [], "consumer": {
            "identity": {"node_id": "reader", "cluster_identity": vec![42_u8; 32]},
            "publications": []
        }
    }))
    .unwrap();
    drop(journal.persist(inventory).await.unwrap());
    assert!(agent.enable_fresh_discovery_ownership(&path).await.is_err());
    assert!(matches!(
        agent.discovery_ownership,
        discovery_ownership::DiscoveryOwnership::Uncertain
    ));
}

#[tokio::test]
async fn producer_agent_retains_process_port_and_owner_without_remote_confirmation() {
    for has_inventory in [true, false] {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        grill.set_pid(std::process::id());
        let root = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(root.path().join("volumes"));
        agent.set_records_dir(root.path().join("records"));
        agent
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let original = agent.supervisor.get_instance(&id).unwrap();
        let original_port = original.host_port;
        let original_spec = original.oci_spec.clone().unwrap();
        if has_inventory {
            grill
                .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                    instance_id: id.clone(),
                    generation: crate::grill::RuntimeGeneration::process("original"),
                    spec: original_spec,
                    network_reference: None,
                }])
                .await;
        }
        let (mut clustered, _, _) = test_cluster_fault_agent().await;
        agent.cluster = clustered.cluster.take();
        agent.kill_and_wait_for_exit(&id).await.unwrap();
        assert!(
            agent.finish_retire_bookkeeping(&id).await.is_err(),
            "unconfirmed producer released its host port"
        );
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().host_port,
            original_port
        );
        assert!(crate::grill::records::record_path(&root.path().join("records"), &id.0).exists());
    }
}

/// Z6.7: with a node stopped, the old instance's release waited for that
/// node's receipt. The rollout failed, the orchestrator retried it, and
/// every retry took the retained replacements for "existing" instances
/// and stopped a healthy one. A rollout now finishes and leaves the
/// release to the agent loop.
#[tokio::test]
async fn a_rollout_finishes_while_the_old_instance_waits_for_remote_release() {
    let grill = MockGrill::new();
    grill.set_pid(std::process::id());
    let allocator = PortAllocator::new(30000, 30010);
    let (_, receiver) = mpsc::channel(8);
    let mut agent = BunAgent::new(
        grill.clone(),
        allocator.clone(),
        receiver,
        CancellationToken::new(),
    );
    let root = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_records_dir(root.path().join("records"));
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let old = InstanceId("default__web-0".into());
    let original = agent.supervisor.get_instance(&old).unwrap();
    let old_port = original.host_port.unwrap();
    let execution = crate::grill::RuntimeExecution {
        instance_id: old.clone(),
        generation: crate::grill::RuntimeGeneration::process("original"),
    };
    grill
        .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
            instance_id: old.clone(),
            generation: execution.generation.clone(),
            spec: original.oci_spec.clone().unwrap(),
            network_reference: None,
        }])
        .await;
    let (mut clustered, _, _) = test_cluster_fault_agent().await;
    agent.cluster = clustered.cluster.take();
    // A stopped node never sends its receipt: the leader answers 202.
    let (client, pending) =
        crate::cluster::producer::test_fixture(axum::http::StatusCode::ACCEPTED, String::new())
            .await;
    agent.set_producer_release_client(client);

    let replacement = Config::parse("[app.web]\nimage = 'web:v2'\nport = 8080\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, replacement).await);

    assert!(agent.deferred_retirements.contains(&old));
    assert_eq!(
        agent.supervisor.get_instance(&old).unwrap().state,
        ContainerState::Stopped
    );
    assert!(allocator.is_allocated(old_port).await, "released too early");
    let (reply, existing) = oneshot::channel();
    agent
        .handle_deploy_op(DeployOp::ListExistingOwned {
            app_name: "web".into(),
            namespace: "default".into(),
            reply,
        })
        .await;
    let existing = existing.await.unwrap();
    assert!(
        !existing.contains(&old),
        "a later rollout must not retire the old instance again"
    );
    assert_eq!(existing.len(), 1, "{existing:?}");

    // Still pending: the agent loop keeps waiting, nothing else happens.
    agent.drive_deferred_retirements().await;
    assert!(agent.deferred_retirements.contains(&old));
    pending.abort();
    let _ = pending.await;

    // The leader confirms; the next tick releases the address.
    let confirmation = serde_json::json!({"node_id": "test", "execution": execution}).to_string();
    let (client, confirmed) =
        crate::cluster::producer::test_fixture(axum::http::StatusCode::OK, confirmation).await;
    agent.set_producer_release_client(client);
    agent.drive_deferred_retirements().await;
    assert!(agent.deferred_retirements.is_empty());
    assert!(agent.supervisor.get_instance(&old).is_none());
    assert!(!allocator.is_allocated(old_port).await);
    confirmed.abort();
    let _ = confirmed.await;
}

/// The states the node's next report would give every instance, by host port.
async fn reported_states(
    agent: &TestAgent,
) -> std::collections::BTreeMap<Option<u16>, ContainerState> {
    let (tx, rx) = oneshot::channel();
    agent
        .handle_snapshot_request(CollectSnapshotRequest { response: tx })
        .await;
    rx.await
        .unwrap()
        .instances
        .into_iter()
        .map(|instance| (instance.port, instance.container_state))
        .collect()
}

/// #1353: the leader builds the service catalogue from node reports, and a
/// report counts an instance healthy only when it's Running. A rolling
/// replacement stayed Starting until the rollout finalised, after the old
/// instance had drained and stopped, so for that whole stretch the report
/// had no healthy instance and every other node's connect hook refused the
/// service. Once its health wait passed and its backend is published, the
/// replacement serves traffic, and the report has to say so before the old
/// instance's withdrawal goes out.
#[tokio::test]
async fn a_published_rolling_replacement_reports_running_before_the_old_instance_retires() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let old = InstanceId("default__web-0".into());
    let old_port = agent.supervisor.get_instance(&old).unwrap().host_port;

    // What the deploy worker asks the loop for, step by step, up to the
    // moment it starts retiring the old instance.
    let spec = Config::parse("[app.web]\nimage = 'myapp:v2'\nport = 8080\n")
        .unwrap()
        .app
        .remove("web")
        .unwrap();
    let new = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
    let new_port = agent
        .reserve_rolling_instance(&new, "web", "default", &spec)
        .await
        .unwrap();
    agent
        .prepare_rolling_instance(&new, "web", "default", &spec, new_port)
        .await
        .unwrap();
    agent
        .transition_deploy_state(&new, ContainerState::Starting)
        .await
        .unwrap();
    assert_eq!(
        reported_states(&agent).await,
        std::collections::BTreeMap::from([
            (old_port, ContainerState::Running),
            (new_port, ContainerState::Starting),
        ]),
        "an unpublished replacement isn't serving yet"
    );

    agent
        .publish_new_backend("web", "default", &new, new_port, None, true)
        .await
        .unwrap();
    agent.begin_instance_retirement(&old).await.unwrap();
    assert_eq!(
        reported_states(&agent).await,
        std::collections::BTreeMap::from([
            (old_port, ContainerState::Stopping),
            (new_port, ContainerState::Running),
        ]),
        "the report must name a healthy instance while the old one drains"
    );
}

/// #1355: a retiring instance's drain waited only for this node's ingress
/// connections. Other nodes connect to its backend directly, and they only
/// drop it once their next view arrives, so a server that exits on SIGTERM
/// could stop while they still routed to it, and their callers got
/// `ECONNREFUSED`. The leader's producer release answers exactly "every
/// node has dropped this backend", but it was asked only after the stop.
/// Now the old instance keeps running until the leader confirms (or the
/// patience runs out), and only then gets its stop signal.
#[tokio::test]
async fn a_rolling_retirement_stops_the_old_instance_only_after_other_nodes_drop_it() {
    let grill = MockGrill::new();
    grill.set_pid(std::process::id());
    let allocator = PortAllocator::new(30000, 30010);
    let (_, receiver) = mpsc::channel(8);
    let mut agent = BunAgent::new(
        grill.clone(),
        allocator.clone(),
        receiver,
        CancellationToken::new(),
    );
    let root = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_records_dir(root.path().join("records"));
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let old = InstanceId("default__web-0".into());
    let original = agent.supervisor.get_instance(&old).unwrap();
    let execution = crate::grill::RuntimeExecution {
        instance_id: old.clone(),
        generation: crate::grill::RuntimeGeneration::process("original"),
    };
    grill
        .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
            instance_id: old.clone(),
            generation: execution.generation.clone(),
            spec: original.oci_spec.clone().unwrap(),
            network_reference: None,
        }])
        .await;
    let (mut clustered, _, _) = test_cluster_fault_agent().await;
    agent.cluster = clustered.cluster.take();

    // The leader waits for one other node's receipt (202), then confirms.
    // Each request notes whether the old instance had already been told to
    // stop when it arrived.
    let confirmation = serde_json::json!({"node_id": "test", "execution": execution}).to_string();
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));
    let (client, leader) = crate::cluster::producer::test_scripted_fixture({
        let requests = requests.clone();
        let grill = grill.clone();
        let old = old.clone();
        move || {
            let stopped = grill
                .calls()
                .iter()
                .any(|(operation, id)| operation == "stop" && *id == old);
            let mut requests = requests.lock().unwrap();
            requests.push(stopped);
            if requests.len() < 2 {
                (axum::http::StatusCode::ACCEPTED, String::new())
            } else {
                (axum::http::StatusCode::OK, confirmation.clone())
            }
        }
    })
    .await;
    agent.set_producer_release_client(client);

    let replacement = Config::parse("[app.web]\nimage = 'web:v2'\nport = 8080\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, replacement).await);

    let requests = requests.lock().unwrap().clone();
    assert!(
        requests.len() >= 2 && !requests[0] && !requests[1],
        "the old instance must keep serving until the leader confirms every \
         node has dropped it; stopped at each release request: {requests:?}"
    );
    assert!(
        grill
            .calls()
            .iter()
            .any(|(operation, id)| operation == "stop" && *id == old),
        "the old instance was never stopped"
    );
    assert!(agent.supervisor.get_instance(&old).is_none());
    leader.abort();
    let _ = leader.await;
}

#[tokio::test]
async fn slow_producer_release_does_not_stall_the_agent_loop() {
    let grill = MockGrill::new();
    grill.set_pid(std::process::id());
    let allocator = PortAllocator::new(30000, 30001);
    let (_, receiver) = mpsc::channel(8);
    let mut agent = BunAgent::new(
        grill.clone(),
        allocator.clone(),
        receiver,
        CancellationToken::new(),
    );
    let root = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_records_dir(root.path().join("records"));
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = reference.instance_id.clone();
    let original = agent.supervisor.get_instance(&id).unwrap();
    let host_port = original.host_port.unwrap();
    let execution = crate::grill::RuntimeExecution {
        instance_id: id.clone(),
        generation: crate::grill::RuntimeGeneration::runc(reference.generation.as_str()),
    };
    grill
        .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
            instance_id: id.clone(),
            generation: execution.generation.clone(),
            spec: original.oci_spec.clone().unwrap(),
            network_reference: Some(crate::grill::runc_intent::NetworkReferenceState::Held(
                reference.clone(),
            )),
        }])
        .await;
    let (mut clustered, _, _) = test_cluster_fault_agent().await;
    agent.cluster = clustered.cluster.take();
    agent.kill_and_wait_for_exit(&id).await.unwrap();
    let confirmation = serde_json::json!({"node_id": "test", "execution": execution}).to_string();
    // An overloaded or partitioned leader answers slowly.
    let (client, task) = crate::cluster::producer::test_delayed_fixture(
        axum::http::StatusCode::OK,
        confirmation,
        std::time::Duration::from_secs(3),
    )
    .await;
    agent.set_producer_release_client(client);
    let started = std::time::Instant::now();
    assert!(agent.finish_retire_bookkeeping(&id).await.is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "one retirement held the agent loop for {:?}",
        started.elapsed()
    );
    let retry = std::time::Instant::now();
    assert!(agent.finish_retire_bookkeeping(&id).await.is_err());
    assert!(
        retry.elapsed() < std::time::Duration::from_millis(500),
        "a retry waited for the leader again"
    );
    assert!(allocator.is_allocated(host_port).await);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    // The confirmation that arrived in the background completes retirement.
    agent.finish_retire_bookkeeping(&id).await.unwrap();
    assert!(!allocator.is_allocated(host_port).await);
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn producer_agent_releases_process_ports_and_exact_runc_addresses_only_after_confirmation() {
    for runc in [false, true] {
        let grill = MockGrill::new();
        grill.set_pid(std::process::id());
        let allocator = PortAllocator::new(30000, 30001);
        let (_, receiver) = mpsc::channel(8);
        let mut agent = BunAgent::new(
            grill.clone(),
            allocator.clone(),
            receiver,
            CancellationToken::new(),
        );
        let root = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(root.path().join("volumes"));
        agent.set_records_dir(root.path().join("records"));
        agent
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        let reference = original_test_network_reference();
        if runc {
            grill.set_network_reference(reference.clone()).await;
        }
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = reference.instance_id.clone();
        let original = agent.supervisor.get_instance(&id).unwrap();
        let host_port = original.host_port.unwrap();
        let execution = crate::grill::RuntimeExecution {
            instance_id: id.clone(),
            generation: if runc {
                crate::grill::RuntimeGeneration::runc(reference.generation.as_str())
            } else {
                crate::grill::RuntimeGeneration::process("original")
            },
        };
        grill
            .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                instance_id: id.clone(),
                generation: execution.generation.clone(),
                spec: original.oci_spec.clone().unwrap(),
                network_reference: runc.then(|| {
                    crate::grill::runc_intent::NetworkReferenceState::Held(reference.clone())
                }),
            }])
            .await;
        let (mut clustered, _, _) = test_cluster_fault_agent().await;
        agent.cluster = clustered.cluster.take();
        agent.kill_and_wait_for_exit(&id).await.unwrap();
        let (client, task) =
            crate::cluster::producer::test_fixture(axum::http::StatusCode::ACCEPTED, String::new())
                .await;
        agent.set_producer_release_client(client);
        assert!(agent.finish_retire_bookkeeping(&id).await.is_err());
        assert!(allocator.is_allocated(host_port).await);
        assert!(agent.supervisor.get_instance(&id).is_some());
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(operation, _)| operation == "release_network_reference")
        );
        task.abort();
        let _ = task.await;
        let confirmation =
            serde_json::json!({"node_id": "test", "execution": execution}).to_string();
        let (client, delayed_task) = crate::cluster::producer::test_delayed_fixture(
            axum::http::StatusCode::OK,
            confirmation.clone(),
            std::time::Duration::from_secs(1),
        )
        .await;
        agent.set_producer_release_client(client);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                agent.finish_retire_bookkeeping(&id)
            )
            .await
            .is_err()
        );
        assert!(allocator.is_allocated(host_port).await);
        assert!(agent.supervisor.get_instance(&id).is_some());
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(operation, _)| operation == "release_network_reference")
        );
        delayed_task.abort();
        let _ = delayed_task.await;
        let (client, task) =
            crate::cluster::producer::test_fixture(axum::http::StatusCode::OK, confirmation).await;
        agent.set_producer_release_client(client);
        agent.finish_retire_bookkeeping(&id).await.unwrap();
        assert!(!allocator.is_allocated(host_port).await);
        assert!(agent.supervisor.get_instance(&id).is_none());
        assert!(!crate::grill::records::record_path(&root.path().join("records"), &id.0).exists());
        assert_eq!(
            grill
                .calls()
                .iter()
                .any(|(operation, _)| operation == "release_network_reference"),
            runc
        );
        task.abort();
        let _ = task.await;
    }
}

fn pending_release() -> BunError {
    BunError::ProducerReleasePending {
        instance_id: InstanceId("default__web-0".into()),
        reason: "other nodes have not yet confirmed the endpoint's withdrawal",
    }
}

/// Z6.7: a rolling deploy on a three-node laptop cluster failed every
/// retirement on the leader's first "pending" answer and started a new
/// generation of replacements, forever.
#[tokio::test(start_paused = true)]
async fn a_pending_producer_release_is_asked_again_until_confirmed() {
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let outcome = retry_while_release_pending(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(1),
        || async {
            if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 {
                Err(pending_release())
            } else {
                Ok(())
            }
        },
    )
    .await;
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 4);
}

#[tokio::test(start_paused = true)]
async fn a_producer_release_still_pending_after_the_patience_fails_the_retirement() {
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let started = tokio::time::Instant::now();
    let outcome = retry_while_release_pending(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(1),
        || async {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(pending_release())
        },
    )
    .await;
    assert!(matches!(
        outcome,
        Err(BunError::ProducerReleasePending { .. })
    ));
    assert!(started.elapsed() <= std::time::Duration::from_secs(30));
    assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) >= 29);
}

#[tokio::test(start_paused = true)]
async fn a_refused_producer_release_is_not_retried() {
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let outcome = retry_while_release_pending(
        std::time::Duration::from_secs(30),
        std::time::Duration::from_secs(1),
        || async {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(BunError::RetirementState {
                instance_id: InstanceId("default__web-0".into()),
                reason: "producer release is unconfirmed (409 Conflict)".into(),
            })
        },
    )
    .await;
    assert!(matches!(outcome, Err(BunError::RetirementState { .. })));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
}

fn original_test_network_reference() -> crate::grill::runc_intent::NetworkReference {
    serde_json::from_value(serde_json::json!({
            "instance_id": "default__web-0", "generation": "1234567890abcdef1234567890abcdef", "container_index": 7
        })).unwrap()
}

#[tokio::test]
async fn failed_release_permission_checkpoint_preserves_the_runtime_hold() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let checkpoint = path.join("discovery.json");
    std::fs::remove_file(&checkpoint).unwrap();
    std::fs::create_dir(&checkpoint).unwrap();
    assert!(agent.stop_app("web", "default").await.is_err());
    assert!(
        !grill
            .calls()
            .iter()
            .any(|(operation, _)| operation == "release_network_reference")
    );
    assert_eq!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap(),
        Some(reference)
    );
}

#[tokio::test]
async fn standalone_release_persists_permission_before_runtime_acknowledgement() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    grill.block_network_releases();
    let mut task = tokio::spawn(async move {
        let result = agent.stop_app("web", "default").await;
        (agent, result)
    });
    tokio::select! {
        result = &mut task => panic!("standalone retirement returned before authorised release: {:?}", result.unwrap().1),
        result = tokio::time::timeout(std::time::Duration::from_secs(2), grill.wait_for_network_release()) => result.unwrap(),
    }
    let checkpoint: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path.join("discovery.json")).unwrap()).unwrap();
    let inventory: crate::bun::discovery_owners::DiscoveryInventory =
        serde_json::from_value(checkpoint["inventory"].clone()).unwrap();
    assert_eq!(inventory.references[0].reference, reference);
    assert_eq!(
        inventory.references[0].phase,
        crate::bun::discovery_owners::ReferencePhase::ReleaseAuthorised
    );
    assert!(
        !inventory.services[0]
            .entry
            .backends
            .iter()
            .any(|backend| backend.instance_id == reference.instance_id.0)
    );
    assert_eq!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap(),
        Some(reference.clone())
    );
    grill.resume_network_release();
    let (agent, result) = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
    drop(agent);
    let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
    assert!(journal.inventory().references.is_empty());
    assert!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn clustered_discovery_requires_remote_proof_before_release_permission() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&directory.path().join("discovery"))
        .await
        .unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    grill
        .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
            instance_id: reference.instance_id.clone(),
            generation: crate::grill::RuntimeGeneration::runc(reference.generation.as_str()),
            spec: agent
                .supervisor
                .get_instance(&reference.instance_id)
                .unwrap()
                .oci_spec
                .clone()
                .unwrap(),
            network_reference: Some(crate::grill::runc_intent::NetworkReferenceState::Held(
                reference.clone(),
            )),
        }])
        .await;
    let (mut cluster_agent, _, _) = test_cluster_fault_agent().await;
    agent.cluster = cluster_agent.cluster.take();
    let result = agent.stop_app("web", "default").await;
    assert!(
        matches!(result, Err(BunError::RetirementState { ref reason, .. }) if reason.contains("producer release transport")),
        "held reference was not fenced by its durable owner: {result:?}"
    );
    assert_eq!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap(),
        Some(reference)
    );
}

#[tokio::test]
async fn durable_discovery_captures_the_original_runtime_reference_before_launch() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    drop(agent);
    let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
    let references = &journal.inventory().references;
    assert_eq!(
        references.len(),
        1,
        "runtime started without a durable original reference"
    );
    assert_eq!(references[0].reference, reference);
    assert_eq!(
        references[0].service,
        crate::onion::service_id::ServiceId::new("default", "web")
    );
    assert_eq!(
        references[0].phase,
        crate::bun::discovery_owners::ReferencePhase::Held
    );
}

#[tokio::test]
async fn failed_runtime_reference_checkpoint_prevents_start_and_retains_the_address() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    grill.block_creates();
    let task = tokio::spawn(async move {
        let events = drain_deploy(&mut agent, basic_config()).await;
        (agent, events)
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), grill.wait_for_creates(1))
        .await
        .unwrap();
    let checkpoint = path.join("discovery.json");
    std::fs::remove_file(&checkpoint).unwrap();
    std::fs::create_dir(&checkpoint).unwrap();
    grill.release_creates(1);
    let (_agent, events) = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ApplyEvent::Error { .. }))
    );
    assert!(
        !grill
            .calls()
            .iter()
            .any(|(operation, _)| operation == "start"),
        "runtime started after original-reference persistence failed"
    );
    assert_eq!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap(),
        Some(reference)
    );
}

#[tokio::test]
async fn durable_publication_records_the_exact_service_before_acknowledgement() {
    let (mut agent, _commands, _shutdown) = test_agent();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let expected = agent.service_map.resolve(&service).unwrap().clone();
    drop(agent);
    let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
    let entries = &journal.inventory().services;
    assert_eq!(
        entries.len(),
        1,
        "acknowledged routing has no durable service owner"
    );
    assert_eq!(
        serde_json::to_value(&entries[0].entry).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    assert_eq!(
        entries[0].phase,
        crate::bun::discovery_owners::ServicePhase::Owned
    );
}

async fn discovery_recovery_fixture() -> (
    BunAgent<MockGrill>,
    MockGrill,
    tempfile::TempDir,
    crate::grill::runc_intent::NetworkReference,
) {
    let (mut original, _, _, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    original.set_records_dir(root.path().join("records"));
    original.set_volumes_dir(root.path().join("volumes"));
    original
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let reference = original_test_network_reference();
    grill.set_pid(std::process::id());
    grill.set_container_ip("10.0.2.5".parse().unwrap());
    grill.set_network_reference(reference.clone()).await;
    expect_complete(&drain_deploy(&mut original, basic_config()).await);
    let spec = original
        .supervisor
        .get_instance(&reference.instance_id)
        .unwrap()
        .oci_spec
        .clone()
        .unwrap();
    grill
        .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
            generation: crate::grill::RuntimeGeneration::runc(reference.generation.as_str()),
            instance_id: reference.instance_id.clone(),
            spec,
            network_reference: Some(crate::grill::runc_intent::NetworkReferenceState::Held(
                reference.clone(),
            )),
        }])
        .await;
    drop(original);
    let (_, receiver) = mpsc::channel(32);
    let mut recovered = BunAgent::new(
        grill.clone(),
        PortAllocator::new(30000, 31000),
        receiver,
        CancellationToken::new(),
    );
    recovered.set_records_dir(root.path().join("records"));
    recovered.set_volumes_dir(root.path().join("volumes"));
    (recovered, grill, root, reference)
}

#[tokio::test]
async fn discovery_recovery_gives_up_on_a_wedged_runtime_inventory() {
    let (mut agent, grill, root, _) = discovery_recovery_fixture().await;
    grill.set_inventory_delay(Some(std::time::Duration::from_secs(300)));
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        agent.recover_discovery_ownership(&root.path().join("discovery")),
    )
    .await
    .expect("discovery recovery hung on the runtime inventory");
    assert!(result.is_err(), "recovery proceeded without an inventory");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

#[tokio::test]
async fn discovery_recovery_reserves_original_vip_before_republishing_adopted_runtime() {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let saved =
        crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    let original_vip = saved.inventory().services[0].entry.vip;
    drop(saved);
    grill.set_adopt_result(&reference.instance_id, true);
    agent
        .recover_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    assert_eq!(
        agent.service_map.resolve(&service).unwrap().vip,
        original_vip
    );
    assert!(
        agent
            .service_map
            .resolve(&service)
            .unwrap()
            .backends
            .is_empty()
    );
    assert!(
        agent
            .service_map_watch()
            .borrow()
            .resolve(&service)
            .is_none()
    );
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
    let view = agent.service_map_watch();
    let map = view.borrow();
    let live = map.resolve(&service).unwrap();
    assert_eq!(live.vip, original_vip);
    assert_eq!(live.backends[0].node_ip.to_string(), "10.0.2.5");
    assert!(live.backends[0].healthy);
    drop(map);
    agent.retire_workload("web", "default").await.unwrap();
}

#[tokio::test]
async fn discovery_recovery_does_not_publish_historical_health() {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    let records = root.path().join("records");
    let mut record = crate::grill::records::load_records(&records)
        .unwrap()
        .remove(0);
    record.app_spec.as_mut().unwrap().health = config_with_health().app["web"].health.clone();
    crate::grill::records::write_record(&records, &record).unwrap();
    grill.set_adopt_result(&reference.instance_id, true);
    agent
        .recover_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    assert!(
        !agent
            .service_map_watch()
            .borrow()
            .resolve(&service)
            .unwrap()
            .backends[0]
            .healthy
    );
    assert_eq!(
        agent
            .supervisor
            .get_instance(&reference.instance_id)
            .unwrap()
            .state,
        ContainerState::HealthWait
    );
    agent.retire_workload("web", "default").await.unwrap();
}

#[tokio::test]
async fn discovery_recovery_retires_unrecorded_original_runtime_and_allocation() {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    crate::grill::records::remove_record(&root.path().join("records"), &reference.instance_id.0)
        .unwrap();
    agent
        .recover_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
    assert!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap()
            .is_none()
    );
    drop(agent);
    let journal =
        crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    assert!(journal.inventory().references.is_empty());
    assert!(journal.inventory().services.is_empty());
}

#[tokio::test]
async fn discovery_recovery_replays_original_permission_without_a_new_launch() {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    grill.set_state(&reference.instance_id, ContainerState::Stopped);
    let journal =
        crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    let mut inventory = journal.inventory().clone();
    inventory.references[0].phase = crate::bun::discovery_owners::ReferencePhase::ReleaseAuthorised;
    inventory.services[0].entry.backends.clear();
    drop(journal.persist(inventory).await.unwrap());
    let before = grill.calls().len();
    agent
        .recover_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
    assert!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !grill.calls()[before..]
            .iter()
            .any(|(op, _)| op == "create" || op == "start")
    );
}

#[tokio::test]
async fn clustered_recovery_replays_durable_release_without_contacting_leader() {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    grill.set_state(&reference.instance_id, ContainerState::Stopped);
    let journal =
        crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    let mut inventory = journal.inventory().clone();
    inventory.references[0].phase = crate::bun::discovery_owners::ReferencePhase::ReleaseAuthorised;
    inventory.services[0].entry.backends.clear();
    let identity = crate::bun::consumer_owners::ConsumerIdentity {
        node_id: crate::meat::NodeId::new("test"),
        cluster_identity: [42; 32],
    };
    inventory.consumer = Some(crate::bun::consumer_owners::ConsumerOwnership {
        identity: identity.clone(),
        publications: vec![],
        phase: crate::bun::consumer_owners::ConsumerPhase::Withdrawn,
        receipts: Default::default(),
    });
    drop(journal.persist(inventory).await.unwrap());
    let (mut clustered, _, _) = test_cluster_fault_agent().await;
    agent.cluster = clustered.cluster.take();
    agent
        .recover_consumer_ownership(&root.path().join("discovery"), identity)
        .await
        .unwrap();
    agent.replay_discovery_releases().await.unwrap();
    assert!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(agent.network_references.is_empty());
}

#[tokio::test]
async fn discovery_recovery_refuses_changed_runtime_before_adoption_or_cleanup() {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    let mut launches = grill.launch_inventory().await.unwrap().unwrap();
    let mut changed = reference.clone();
    changed.container_index += 1;
    launches[0].network_reference = Some(crate::grill::runc_intent::NetworkReferenceState::Held(
        changed,
    ));
    grill.set_launch_inventory(launches).await;
    let before = grill.calls().len();
    assert!(
        agent
            .recover_discovery_ownership(&root.path().join("discovery"))
            .await
            .is_err()
    );
    assert!(agent.adopt_recorded_instances().await.is_err());
    assert_eq!(grill.calls().len(), before);
    assert_eq!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap(),
        Some(reference)
    );
    assert!(agent.service_map_watch().borrow().resolve_all().is_empty());
}

#[tokio::test]
async fn confirmed_stop_forgets_durable_service_allocation() {
    let (mut agent, _, _) = test_agent();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    agent.stop_app("web", "default").await.unwrap();
    drop(agent);
    let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
    assert!(
        journal.inventory().services.is_empty(),
        "confirmed stop retained its allocation"
    );
}

#[tokio::test]
async fn failed_service_retirement_checkpoint_keeps_the_allocated_vip() {
    let (mut agent, _, _) = test_agent();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let original = agent.service_map.resolve(&service).unwrap().vip;
    let checkpoint = path.join("discovery.json");
    std::fs::remove_file(&checkpoint).unwrap();
    std::fs::create_dir(&checkpoint).unwrap();
    assert!(
        agent.stop_app("web", "default").await.is_err(),
        "stop acknowledged failed service retirement"
    );
    assert_eq!(agent.service_map.resolve(&service).unwrap().vip, original);
}

#[tokio::test]
async fn clustered_service_retirement_requires_remote_proof_without_runtime_references() {
    let (mut agent, _, _) = test_agent();
    let directory = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&directory.path().join("discovery"))
        .await
        .unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let (mut cluster_agent, _, _) = test_cluster_fault_agent().await;
    agent.cluster = cluster_agent.cluster.take();
    assert!(
        agent.stop_app("web", "default").await.is_err(),
        "clustered stop forgot an unconfirmed allocation"
    );
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    assert!(agent.service_map.resolve(&service).is_some());
}

#[tokio::test]
async fn later_publication_preserves_unretired_discovery_allocations() {
    let (mut agent, _commands, _shutdown) = test_agent();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let original = agent.service_map.resolve(&service).unwrap().clone();
    // Simulate lost private metadata without confirmed retirement.
    agent.service_map.unregister(&service).unwrap();
    assert!(agent.service_map.resolve(&service).is_none());
    let config = Config::parse("[app.other]\nimage = 'mock:image'\nport = 8081\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    drop(agent);
    let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
    assert_eq!(journal.inventory().services.len(), 2);
    let retained = journal
        .inventory()
        .services
        .iter()
        .find(|owner| owner.entry.app_name == "web")
        .unwrap();
    assert_eq!(retained.entry.vip, original.vip);
    assert_eq!(
        retained.phase,
        crate::bun::discovery_owners::ServicePhase::Owned
    );
}

#[tokio::test]
async fn failed_discovery_checkpoint_refuses_launch_and_fences_later_publication() {
    let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    let checkpoint = path.join("discovery.json");
    std::fs::remove_file(&checkpoint).unwrap();
    std::fs::create_dir(&checkpoint).unwrap();
    let events = drain_deploy(&mut agent, basic_config()).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ApplyEvent::Error { .. })),
        "deployment acknowledged a failed discovery checkpoint"
    );
    assert!(
        !grill
            .calls()
            .iter()
            .any(|(operation, _)| operation == "create" || operation == "start")
    );
    std::fs::remove_dir(&checkpoint).unwrap();
    // Repairing the path does not establish what an interrupted write published.
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    assert!(agent.publish_backend_ebpf(&service).await.is_err());
}

#[tokio::test]
async fn stopped_runtime_keeps_artifacts_until_captured_ingress_releases() {
    let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    agent.set_records_dir(records.path().to_owned());
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let record = crate::grill::records::record_path(records.path(), &id.0);
    let identity = agent.instance_identity_dir(&id);
    assert!(record.exists() && identity.exists());
    let drains = agent.drains.clone();
    let tokens = drains
        .capture_requests(std::slice::from_ref(&id.0), false)
        .await
        .unwrap();
    grill.set_state(&id, ContainerState::Stopped);
    let result = agent.retire_instance_artifacts(&id).await;
    assert!(
        matches!(result, Err(BunError::RetirementState { .. })),
        "stopped-runtime cleanup discarded ownership before request release: {result:?}"
    );
    assert!(record.exists() && identity.exists());
    assert!(
        tokens[0].is_cancelled(),
        "an absent runtime's captured requests were not cancelled"
    );
    drains.decrement_connections(&id.0).await;
    agent.retire_instance_artifacts(&id).await.unwrap();
    assert!(!record.exists() && !identity.exists());
    agent.retire_workload("web", "default").await.unwrap();
}

#[tokio::test]
async fn explicit_stop_waits_for_captured_ingress_before_runtime_retirement() {
    let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let drains = agent.drains.clone();
    let _tokens = drains
        .capture_requests(std::slice::from_ref(&id.0), false)
        .await
        .unwrap();
    let mut task = tokio::spawn(async move {
        let result = agent.stop_app("web", "default").await;
        (agent, result)
    });
    tokio::select! {
        result = &mut task => panic!("stop returned before captured request release: {:?}", result.unwrap().1),
        result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !drains.is_draining(&id.0).await { tokio::task::yield_now().await; }
        }) => result.expect("stop did not start an ingress drain"),
    }
    assert!(
        !grill
            .calls()
            .iter()
            .any(|(operation, instance)| instance == &id
                && matches!(operation.as_str(), "stop" | "kill")),
        "runtime retired before ingress request release"
    );
    drains.decrement_connections(&id.0).await;
    let (_agent, result) = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn automatic_restart_defers_runtime_retirement_until_captured_ingress_releases() {
    let (mut agent, grill, id, _directory) = failed_restart_fixture().await;
    let drains = agent.drains.clone();
    let view = agent.service_map_watch();
    let _tokens = drains
        .capture_requests(std::slice::from_ref(&id.0), false)
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        agent.drive_pending_restarts_to_completion(),
    )
    .await
    .expect("waiting for ingress blocked the agent loop");
    assert!(
        !grill
            .calls()
            .iter()
            .any(|(operation, instance)| instance == &id && operation == "kill"),
        "restart retired its predecessor before request release"
    );
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Pending
    );
    let service = crate::onion::service_id::ServiceId::new("default", "retry");
    assert!(view.borrow().resolve(&service).unwrap().backends.is_empty());
    assert!(drains.is_draining(&id.0).await);
    drains.decrement_connections(&id.0).await;
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running
    );
    agent.stop_app("retry", "default").await.unwrap();
}

#[tokio::test]
async fn refused_restart_keeps_cleanup_owed_when_stop_fails() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = agent.supervisor.list_instances()[0].id.clone();
    // Without its original cgroup path, egress preparation refuses the
    // restart after the replacement container has been created.
    grill.set_honours_cgroup_path(true);
    agent
        .supervisor
        .get_instance_mut(&id)
        .unwrap()
        .oci_spec
        .as_mut()
        .unwrap()
        .linux
        .cgroups_path = None;
    grill.set_state(&id, ContainerState::Stopped);
    agent.check_apps().await;
    grill.set_fail_stop(true);
    agent.drive_pending_restarts_to_completion().await;
    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_ne!(
        instance.state,
        ContainerState::Failed,
        "a refused restart abandoned its created container"
    );
    assert!(
        instance.retry_pending,
        "cleanup of the created container is no longer owed"
    );
}

/// The journal names a published backend's execution from what the
/// restart step read after starting it, and a publication never reads the
/// runtime's inventory inside the turn (#419).
#[tokio::test]
async fn publication_journals_the_execution_the_restart_step_read() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&directory.path().join("discovery"))
        .await
        .unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = agent.supervisor.list_instances()[0].id.clone();
    let generation = crate::grill::RuntimeGeneration::process("restarted-execution");
    grill
        .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
            instance_id: id.clone(),
            spec: agent
                .supervisor
                .get_instance(&id)
                .unwrap()
                .oci_spec
                .clone()
                .unwrap(),
            generation: generation.clone(),
            network_reference: None,
        }])
        .await;
    grill.set_state(&id, ContainerState::Stopped);
    agent.check_apps().await;
    agent.drive_pending_restarts_to_completion().await;
    let journalled = |agent: &TestAgent| {
        let DiscoveryOwnership::Ready(journal) = &agent.discovery_ownership else {
            panic!("discovery ownership is not ready");
        };
        journal.inventory().services[0]
            .executions
            .get(&id.0)
            .cloned()
    };
    assert_eq!(journalled(&agent), Some(generation.clone()));

    // A wedged inventory can't hold up a publication any more: it isn't read.
    grill.set_inventory_delay(Some(std::time::Duration::from_secs(300)));
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.publish_backend_ebpf(&service),
    )
    .await
    .expect("the publication waited on the runtime's inventory")
    .unwrap();
    assert_eq!(journalled(&agent), Some(generation));
}

/// A restart step that couldn't read the inventory fails the restart while
/// the discovery journal is on, as the publication that read it used to.
#[tokio::test]
async fn an_unreadable_execution_fails_the_restart_while_discovery_is_journalled() {
    let (mut agent, _, _, _) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&directory.path().join("discovery"))
        .await
        .unwrap();
    let id = InstanceId("default__web-0".into());
    let unknown =
        super::launch_evidence::LaunchExecution::Unknown("runtime inventory timed out".into());
    let refused = agent.record_launch_execution(&id, &unknown).unwrap_err();
    assert_eq!(
        refused.to_string(),
        BunError::AdoptionState("publication runtime inventory timed out".into()).to_string()
    );
    // Without the journal there is nothing to name, so nothing fails.
    let (mut plain, _, _, _) = test_agent_with_grill();
    plain.record_launch_execution(&id, &unknown).unwrap();
}

#[tokio::test]
async fn automatic_restart_releases_original_address_before_successor_creation() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    let original = original_test_network_reference();
    grill.set_network_reference(original.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    grill.set_state(&original.instance_id, ContainerState::Stopped);
    agent.check_apps().await;
    agent.drive_pending_restarts_to_completion().await;
    let calls = grill.calls();
    let release = calls.iter().position(|(operation, id)| {
        operation == "release_network_reference" && id == &original.instance_id
    });
    let successor = calls
        .iter()
        .rposition(|(operation, id)| operation == "create" && id == &original.instance_id)
        .unwrap();
    assert!(
        release.is_some_and(|release| release < successor),
        "successor creation preceded original address release: {calls:?}"
    );
    assert!(!agent.network_references.contains_key(&original.instance_id));
    assert_eq!(
        agent
            .supervisor
            .get_instance(&original.instance_id)
            .unwrap()
            .state,
        ContainerState::Running
    );
    agent.retire_workload("web", "default").await.unwrap();
}

#[tokio::test]
async fn automatic_restart_retains_original_address_when_release_permission_fails() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("discovery");
    agent.enable_fresh_discovery_ownership(&path).await.unwrap();
    let original = original_test_network_reference();
    grill.set_network_reference(original.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    grill.set_state(&original.instance_id, ContainerState::Stopped);
    agent.check_apps().await;
    let checkpoint = path.join("discovery.json");
    std::fs::remove_file(&checkpoint).unwrap();
    std::fs::create_dir(&checkpoint).unwrap();
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        grill
            .calls()
            .iter()
            .filter(|(operation, id)| operation == "create" && id == &original.instance_id)
            .count(),
        1,
        "successor created without release permission"
    );
    assert_eq!(
        grill
            .network_reference(&original.instance_id)
            .await
            .unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        agent
            .supervisor
            .get_instance(&original.instance_id)
            .unwrap()
            .state,
        ContainerState::Pending
    );
}

#[tokio::test]
async fn automatic_restart_publishes_the_replacement_address() {
    let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
    let old_ip = std::net::Ipv4Addr::new(10, 0, 2, 5);
    let new_ip = std::net::Ipv4Addr::new(10, 0, 2, 6);
    grill.set_container_ip(old_ip);
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let view = agent.service_map_watch();
    let id = InstanceId("default__web-0".into());
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    grill.set_state(&id, ContainerState::Stopped);
    agent.check_apps().await;
    grill.set_container_ip(new_ip);
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running
    );
    assert_eq!(
        view.borrow().resolve(&service).unwrap().backends[0].node_ip,
        new_ip
    );
    assert!(view.borrow().resolve(&service).unwrap().backends[0].healthy);
    agent.retire_workload("web", "default").await.unwrap();
}

#[tokio::test]
async fn automatic_restart_waits_for_health_before_publishing_a_healthy_backend() {
    let (mut agent, _commands, _shutdown) = test_agent();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let view = agent.service_map_watch();
    let id = InstanceId("default__web-0".into());
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let mut health = super::super::health::HealthCheckConfig::from_spec(
        config_with_health().app["web"].health.as_ref().unwrap(),
        8080,
    );
    health.threshold_unhealthy = 1;
    health.threshold_healthy = 1;
    let instance = agent.supervisor.get_instance_mut(&id).unwrap();
    instance.health_config = Some(health);
    let created_at = instance.created_at;
    agent
        .complete_health_probe(
            id.clone(),
            created_at,
            Ok(super::super::health::HealthStatus::Unhealthy),
        )
        .await;
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::HealthWait
    );
    assert!(
        !agent.service_map.resolve(&service).unwrap().backends[0].healthy,
        "restart published a healthy backend before its first successful probe"
    );
    assert!(!view.borrow().resolve(&service).unwrap().backends[0].healthy);
    let created_at = agent.supervisor.get_instance(&id).unwrap().created_at;
    agent
        .complete_health_probe(
            id,
            created_at,
            Ok(super::super::health::HealthStatus::Healthy),
        )
        .await;
    assert!(view.borrow().resolve(&service).unwrap().backends[0].healthy);
    agent.retire_workload("web", "default").await.unwrap();
}

#[tokio::test]
async fn a_probe_that_lands_after_a_kill_keeps_the_restarted_instance_probed() {
    let (mut agent, _commands, _shutdown) = test_agent();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let health = super::super::health::HealthCheckConfig::from_spec(
        config_with_health().app["web"].health.as_ref().unwrap(),
        8080,
    );
    let now = Instant::now();
    agent.supervisor.register_health(id.clone(), health, now);
    // The check is taken off the queue for a probe, as run_health_checks does.
    let far = now + std::time::Duration::from_secs(3600);
    while agent.supervisor.health_checker_mut().pop_due(far).is_some() {}
    // The process is killed while the probe is in flight.
    let instance = agent.supervisor.get_instance_mut(&id).unwrap();
    instance.state = ContainerState::Pending;
    let created_at = instance.created_at;
    agent
        .complete_health_probe(
            id.clone(),
            created_at,
            Ok(super::super::health::HealthStatus::Unhealthy),
        )
        .await;
    assert_eq!(
        agent
            .supervisor
            .health_checker_mut()
            .pop_due(far)
            .map(|(due, _)| due),
        Some(id.clone()),
        "the late probe dropped the check, so the restart would never be probed"
    );
    agent.retire_workload("web", "default").await.unwrap();
}

#[tokio::test]
async fn deployment_publishes_thirty_three_backends_and_retains_runtime_owners() {
    let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
    let mut config = basic_config();
    let app = config.app.get_mut("web").unwrap();
    let replicas = crate::onion::types::MAX_BACKENDS + 1;
    app.replicas = crate::config::Replicas::Fixed(replicas as u32);
    app.ingress = Some(toml::from_str("host = 'large.web.test'\ntls = 'disabled'").unwrap());
    let view = agent.service_map_watch();
    let routes = agent.routing_table_handle();
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let events = drain_deploy(&mut agent, config).await;
    let owners: std::collections::HashSet<_> = agent
        .supervisor
        .list_instances()
        .iter()
        .map(|instance| instance.id.clone())
        .collect();
    let unowned: Vec<_> = grill
        .calls()
        .into_iter()
        .filter(|(call, id)| call == "create" && !owners.contains(id))
        .collect();
    let published = view
        .borrow()
        .resolve(&service)
        .map(|entry| entry.backends.len());
    let routed = routes
        .read()
        .await
        .lookup("large.web.test", "/")
        .map(|route| route.backends.len());
    agent.retire_workload("web", "default").await.unwrap();
    assert!(
        unowned.is_empty(),
        "created runtimes lost their cleanup owner: {unowned:?}"
    );
    assert_eq!(expect_complete(&events).0, replicas);
    assert_eq!(owners.len(), replicas);
    assert_eq!(published, Some(replicas), "DNS must see the full catalogue");
    assert_eq!(
        routed,
        Some(replicas),
        "Wrapper must see the full catalogue"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ApplyEvent::Error { .. }))
    );
    assert!(view.borrow().resolve(&service).is_none());
    assert!(routes.read().await.lookup("large.web.test", "/").is_none());
    assert!(
        owners
            .iter()
            .all(|id| agent.supervisor.get_instance(id).is_none())
    );
}

#[tokio::test]
async fn health_transitions_publish_the_confirmed_userspace_view() {
    let (mut agent, _commands, _shutdown) = test_agent();
    let view = agent.service_map_watch();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let mut health = super::super::health::HealthCheckConfig::from_spec(
        config_with_health().app["web"].health.as_ref().unwrap(),
        8080,
    );
    health.threshold_unhealthy = 1;
    let instance = agent.supervisor.get_instance_mut(&id).unwrap();
    instance.health_config = Some(health);
    instance.restart_policy.max_restarts = Some(0);
    let created_at = instance.created_at;
    agent
        .complete_health_probe(
            id.clone(),
            created_at,
            Ok(super::super::health::HealthStatus::Unhealthy),
        )
        .await;
    assert!(
        !view.borrow().resolve(&service).unwrap().backends[0].healthy,
        "DNS/ingress retained a backend after its health withdrawal"
    );
    agent
        .complete_health_probe(
            id,
            created_at,
            Ok(super::super::health::HealthStatus::Healthy),
        )
        .await;
    assert!(view.borrow().resolve(&service).unwrap().backends[0].healthy);
}

#[tokio::test]
async fn a_later_probe_retries_publication_before_starting_restart() {
    let (mut agent, _commands, _shutdown) = test_agent();
    let view = agent.service_map_watch();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let mut health = super::super::health::HealthCheckConfig::from_spec(
        config_with_health().app["web"].health.as_ref().unwrap(),
        8080,
    );
    health.threshold_unhealthy = 1;
    let instance = agent.supervisor.get_instance_mut(&id).unwrap();
    instance.health_config = Some(health);
    let created_at = instance.created_at;
    let original = agent.service_map.clone();
    // Missing original allocation must refuse publication. Restore the same
    // evidence before retrying, rather than inventing a replacement VIP.
    agent.service_map = crate::onion::service_map::ServiceMap::new();
    agent
        .complete_health_probe(
            id.clone(),
            created_at,
            Ok(super::super::health::HealthStatus::Unhealthy),
        )
        .await;
    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Unhealthy);
    assert_eq!(instance.restart_count, 0);
    assert!(view.borrow().resolve(&service).unwrap().backends[0].healthy);
    agent.service_map = original;
    agent
        .complete_health_probe(
            id.clone(),
            created_at,
            Ok(super::super::health::HealthStatus::Unhealthy),
        )
        .await;
    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Pending);
    assert_eq!(instance.restart_count, 1);
    assert!(!view.borrow().resolve(&service).unwrap().backends[0].healthy);
}

#[tokio::test]
async fn replacement_publication_refuses_a_closed_agent_channel() {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let ops = DeployOps { tx };
    let result = ops
        .publish_new_backend(
            "api",
            "default",
            &InstanceId("default__api-g1-0".into()),
            Some(8080),
            None,
            true,
        )
        .await;
    assert!(matches!(result, Err(BunError::BackendPublication { .. })));
}

#[tokio::test]
async fn replacement_publication_refuses_missing_service_or_port() {
    let (mut agent, _commands, _shutdown) = test_agent();
    let id = InstanceId("default__api-g1-0".into());
    let service = crate::onion::service_id::ServiceId::new("default", "api");
    let view = agent.service_map_watch();
    assert!(
        agent
            .publish_new_backend("api", "default", &id, Some(8080), None, true)
            .await
            .is_err()
    );
    agent.service_map.register(&service, 8080, None).unwrap();
    assert!(
        agent
            .publish_new_backend("api", "default", &id, None, None, true)
            .await
            .is_err()
    );
    assert!(
        agent
            .service_map
            .resolve(&service)
            .unwrap()
            .backends
            .is_empty()
    );
    assert!(view.borrow().resolve(&service).is_none());
}

/// The stop-confirmation deadline test agents use: the pre-configuration
/// constant, well under the timeouts the stall tests assert against.
const TEST_STOP_CONFIRMATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

fn test_agent_with_grill() -> (
    TestAgent,
    mpsc::Sender<AgentCommand>,
    CancellationToken,
    MockGrill,
) {
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = MockGrill::new();
    let grill_handle = grill.clone();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    // MockGrill answers instantly unless a test stalls it, so a short
    // deadline keeps injected stalls fast without changing any outcome.
    agent.set_stop_confirmation_timeout(TEST_STOP_CONFIRMATION_TIMEOUT);
    let agent = TestAgent {
        agent,
        _volumes: volumes,
    };
    (agent, tx, shutdown, grill_handle)
}

/// Gossip's live view drops a dead member; the listing must still show
/// it, as dead, unless gossip lists it again.
#[tokio::test]
async fn cluster_nodes_list_down_members_gossip_no_longer_publishes() {
    let (agent, _, _) = test_cluster_fault_agent().await;
    let status = |name: &str, state: &str| NodeStatus {
        node_id: name.to_string(),
        address: "127.0.0.1:7946".to_string(),
        api_address: None,
        state: state.to_string(),
        incarnation: 1,
        is_council: true,
        is_leader: true,
        labels: Default::default(),
    };
    let nodes = agent.get_cluster_nodes(vec![status("gone", "dead")]);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].node_id, "gone");
    assert_eq!(nodes[0].state, "dead");
    // Without Raft metrics the flags come from gossip, which has nothing
    // to say about a member it no longer lists.
    assert!(!nodes[0].is_council && !nodes[0].is_leader);
}

#[tokio::test]
async fn cluster_nodes_prefer_gossips_live_entry_over_a_remembered_one() {
    let (_membership_tx, membership_rx) =
        tokio::sync::watch::channel(vec![crate::mustard::membership::MembershipSnapshot {
            node_id: crate::meat::NodeId::new("back"),
            address: "127.0.0.1:7946".parse().unwrap(),
            state: crate::mustard::state::NodeState::Alive,
            incarnation: 2,
            is_council: false,
            is_leader: false,
            labels: Default::default(),
            first_seen: std::time::Instant::now(),
            resources: None,
        }]);
    let (mut agent, _, _) = test_cluster_fault_agent().await;
    if let Some(cluster) = agent.cluster.as_mut() {
        cluster.membership_rx = membership_rx;
    }
    let remembered = NodeStatus {
        node_id: "back".to_string(),
        address: "127.0.0.1:7946".to_string(),
        api_address: None,
        state: "dead".to_string(),
        incarnation: 1,
        is_council: false,
        is_leader: false,
        labels: Default::default(),
    };
    let nodes = agent.get_cluster_nodes(vec![remembered]);
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].state, "alive");
    assert_eq!(nodes[0].incarnation, 2);
}

async fn test_cluster_fault_agent() -> (
    BunAgent<MockGrill>,
    crate::smoker::node_fault::NodeTransportGate,
    crate::bun::readiness::ReadinessTracker,
) {
    let (_membership_tx, membership_rx) = tokio::sync::watch::channel(Vec::new());
    let (_snapshot_tx, snapshot_rx) = mpsc::channel(1);
    let (_command_tx, command_rx) = mpsc::channel(8);
    let node_gate = crate::smoker::node_fault::NodeTransportGate::new();
    let cluster = ClusterHandle {
        local_node_id: crate::meat::NodeId::new("test"),
        membership_rx,
        raft_metrics_rx: None,
        council: None,
        snapshot_rx,
        wrapping_ikm: None,
        partition_blocklists: PartitionBlocklists {
            node_gate: node_gate.clone(),
            ..PartitionBlocklists::default()
        },
        crl_handle: Default::default(),
    };
    let mut agent = BunAgent::with_cluster(
        MockGrill::new(),
        PortAllocator::new(30000, 31000),
        command_rx,
        CancellationToken::new(),
        cluster,
        "test".to_string(),
    );
    let readiness = crate::bun::readiness::ReadinessTracker::new();
    readiness.register("agent:test", true).await;
    readiness.ready("agent:test").await;
    agent.set_readiness_tracker(readiness.clone());
    (agent, node_gate, readiness)
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Test-only: run a deploy to completion against an agent that is not
    /// yet on its `run` loop. Deploys now execute on a spawned task that
    /// drives `&mut self` steps back through `deploy_ops_rx`, so this pumps
    /// those ops inline until the deploy's events channel closes. Keeps the
    /// direct-`deploy` unit tests working without standing up a full loop.
    async fn deploy(&mut self, config: Config, events: &mpsc::Sender<ApplyEvent>) {
        let worker = DeployWorker {
            grill: self.supervisor.grill().clone(),
            ops: DeployOps {
                tx: self.deploy_ops_tx.clone(),
            },
            drains: self.drains.clone(),
            operation: None,
            stop_confirmation_timeout: self.stop_confirmation_timeout,
            egress: self.egress_resolver(),
            secrets: self.secret_providers.clone(),
        };
        let events = events.clone();
        let mut task = tokio::spawn(async move { worker.run_deploy(config, events).await });
        loop {
            tokio::select! {
                Some(op) = self.deploy_ops_rx.recv() => {
                    self.handle_deploy_op(op).await;
                }
                Some(outcome) = self.identity_signing_tasks.join_next_with_id(),
                    if !self.identity_signing_tasks.is_empty() => {
                    self.finish_identity_provision(outcome);
                }
                result = &mut task => {
                    let _ = result;
                    // Drain any ops queued right before the task finished.
                    while let Ok(op) = self.deploy_ops_rx.try_recv() {
                        self.handle_deploy_op(op).await;
                    }
                    break;
                }
            }
        }
    }
}

#[tokio::test]
async fn lease_storage_waits_for_confirmed_runtime_retirement() {
    use crate::testkit::lease::{LeasedResource, LocalLeaseStore, TestLease, cleanup_local_lease};
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    // The runtime ignores SIGTERM on purpose; a short grace reaches the
    // unconfirmed kill without waiting out the production ten seconds.
    agent.set_stop_grace(std::time::Duration::from_millis(200));
    let task = tokio::spawn(async move { agent.run().await });
    let config = Config::parse("[app.web]\nimage = 'test:v1'\nnamespace = 'rbtest-cleanup'\n[app.web.deploy]\ndrain_timeout = '0s'\n[[app.web.volumes]]\npath = '/data'\n").unwrap();
    expect_complete(&send_deploy(&tx, config).await);
    let marker = volumes.path().join("rbtest-cleanup/web/data/marker");
    std::fs::write(&marker, "live").unwrap();
    let store = LocalLeaseStore::in_memory();
    let mut lease = TestLease::new(
        "cleanup".into(),
        "owner".into(),
        "owner".into(),
        "rbtest-cleanup".into(),
        1,
        2,
    )
    .unwrap();
    lease.resources.insert(LeasedResource::App {
        app_id: crate::meat::AppId::new("web", "rbtest-cleanup"),
    });
    store.create(lease).await.unwrap();
    grill.set_ignore_stop(true);
    grill.set_ignore_kill(true);
    let refused = cleanup_local_lease(&store, &tx, None, "cleanup", Some("owner")).await;
    assert!(refused.is_err());
    assert!(store.get("cleanup").await.is_some());
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "live");
    assert!(
        volumes
            .path()
            .join(".test-storage/rbtest-cleanup__web.checkpoint")
            .exists()
    );
    grill.set_ignore_stop(false);
    grill.set_ignore_kill(false);
    cleanup_local_lease(&store, &tx, None, "cleanup", Some("owner"))
        .await
        .unwrap();
    assert!(!marker.exists());
    assert!(store.get("cleanup").await.is_none());
    shutdown.cancel();
    task.await.unwrap();
}

/// #386: storage provisioning that outlasts the turn answers
/// `StillRunning`, and the deploy worker asks again. The second ask is
/// the same step of the same incarnation, so it must carry on rather
/// than try to move a `Preparing` instance to `Preparing` again.
#[tokio::test]
async fn a_fresh_instance_asked_again_after_slow_storage_still_prepares() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let config = Config::parse(
            "[app.web]\nimage = 'test:v1'\nnamespace = 'rbtest-slow'\n[[app.web.volumes]]\npath = '/data'\n",
        )
        .unwrap();
    let spec = config.app["web"].clone();
    let ids = agent
        .supervisor
        .deploy_app("web", "rbtest-slow", &spec, Instant::now())
        .await
        .unwrap();
    let id = ids[0].clone();
    // A turn with no budget left: on this single-threaded runtime the
    // provisioning task can't even have started, so the first ask is
    // always `StillRunning`.
    agent.turn_deadline = Some(tokio::time::Instant::now());
    let first = agent
        .prepare_fresh_instance(&id, "web", "rbtest-slow", &spec)
        .await;
    assert!(
        matches!(first, Err(BunError::StillRunning { .. })),
        "expected StillRunning, got {:?}",
        first.err()
    );
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Preparing
    );
    let prepared = loop {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        match agent
            .prepare_fresh_instance(&id, "web", "rbtest-slow", &spec)
            .await
        {
            Err(BunError::StillRunning { .. }) => continue,
            outcome => break outcome,
        }
    };
    assert!(prepared.is_ok(), "retry failed: {:?}", prepared.err());
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Preparing
    );
    assert!(volumes.path().join("rbtest-slow/web/data").is_dir());
}

#[tokio::test]
async fn lease_retirement_removes_only_owned_test_volumes_after_stop_preserves_them() {
    use crate::testkit::lease::{LeasedResource, LocalLeaseStore, TestLease, cleanup_local_lease};
    let (mut agent, tx, shutdown) = test_agent();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let task = tokio::spawn(async move { agent.run().await });
    for namespace in ["rbtest-cleanup", "default"] {
        let config = Config::parse(&format!(
                "[app.web]\nimage = 'test:v1'\nnamespace = '{namespace}'\n[[app.web.volumes]]\npath = '/data'\n"
            )).unwrap();
        expect_complete(&send_deploy(&tx, config).await);
        std::fs::write(
            volumes.path().join(namespace).join("web/data/marker"),
            namespace,
        )
        .unwrap();
    }
    let (response, stopped) = oneshot::channel();
    tx.send(AgentCommand::Stop {
        app_name: "web".into(),
        namespace: "rbtest-cleanup".into(),
        response,
    })
    .await
    .unwrap();
    stopped.await.unwrap().unwrap();
    assert!(
        volumes
            .path()
            .join("rbtest-cleanup/web/data/marker")
            .is_file()
    );
    let store = LocalLeaseStore::in_memory();
    let mut lease = TestLease::new(
        "cleanup".into(),
        "owner".into(),
        "owner".into(),
        "rbtest-cleanup".into(),
        1,
        2,
    )
    .unwrap();
    lease.resources.insert(LeasedResource::App {
        app_id: crate::meat::AppId::new("web", "rbtest-cleanup"),
    });
    store.create(lease).await.unwrap();
    let cleanup = cleanup_local_lease(&store, &tx, None, "cleanup", Some("owner")).await;
    shutdown.cancel();
    task.await.unwrap();
    cleanup.unwrap();
    assert!(!volumes.path().join("rbtest-cleanup/web/data").exists());
    assert_eq!(
        std::fs::read_to_string(volumes.path().join("default/web/data/marker")).unwrap(),
        "default"
    );
    assert!(store.get("cleanup").await.is_none());
}

#[tokio::test]
async fn lease_cleanup_retires_owned_instances_without_erasing_another_namespace() {
    use crate::testkit::lease::{LeasedResource, LocalLeaseStore, TestLease, cleanup_local_lease};
    let (mut agent, tx, shutdown) = test_agent();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let task = tokio::spawn(async move { agent.run().await });
    for namespace in ["rbtest-cleanup", "rbtest-keep"] {
        let config = Config::parse(&format!(
            "[app.web]\nimage = 'test:v1'\nnamespace = '{namespace}'\n"
        ))
        .unwrap();
        expect_complete(&send_deploy(&tx, config).await);
    }
    let store = LocalLeaseStore::in_memory();
    let mut lease = TestLease::new(
        "cleanup".into(),
        "owner".into(),
        "owner".into(),
        "rbtest-cleanup".into(),
        1,
        2,
    )
    .unwrap();
    lease.resources.insert(LeasedResource::App {
        app_id: crate::meat::AppId::new("web", "rbtest-cleanup"),
    });
    store.create(lease).await.unwrap();
    cleanup_local_lease(&store, &tx, None, "cleanup", Some("owner"))
        .await
        .unwrap();
    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::Status { response }).await.unwrap();
    let instances = result.await.unwrap();
    shutdown.cancel();
    task.await.unwrap();
    assert!(store.get("cleanup").await.is_none());
    assert_eq!(
        instances.len(),
        1,
        "cleanup must retire its status record too"
    );
    assert_eq!(instances[0].namespace, "rbtest-keep");
    assert_eq!(instances[0].state, "running");
}

/// A standalone `relish stop` keeps the stopped replicas owned but
/// releases the app's service and ingress route. Applying the same spec
/// again rolls over those stopped replicas, so the rollout itself must
/// restore what the stop released: the service under its original VIP,
/// its backends and its ingress route.
#[tokio::test]
async fn apply_after_stop_restores_the_service_and_ingress_route() {
    let (mut agent, tx, shutdown) = test_agent();
    let view = agent.service_map_watch();
    let routes = agent.routing_table_handle();
    let task = tokio::spawn(async move { agent.run().await });
    let config = || {
        Config::parse(
            "[app.web]\nimage = 'myapp:v1'\nport = 8080\n\
                 [app.web.ingress]\nhost = 'web.example'\n",
        )
        .unwrap()
    };
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    expect_complete(&send_deploy(&tx, config()).await);
    let original_vip = view.borrow().resolve(&service).unwrap().vip;

    let (response, stopped) = oneshot::channel();
    tx.send(AgentCommand::Stop {
        app_name: "web".into(),
        namespace: "default".into(),
        response,
    })
    .await
    .unwrap();
    stopped.await.unwrap().unwrap();
    assert!(view.borrow().resolve(&service).is_none());
    assert!(!routes.read().await.contains_host("web.example"));

    let events = send_deploy(&tx, config()).await;
    let restored = view.borrow().resolve(&service).cloned();
    let routed = routes.read().await.contains_host("web.example");
    shutdown.cancel();
    task.await.unwrap();
    expect_complete(&events);
    let restored = restored.expect("the reapplied app has no service");
    assert_eq!(restored.vip, original_vip);
    assert_eq!(restored.backends.len(), 1, "{restored:?}");
    assert!(routed, "the reapplied app has no ingress route");
}

/// #307: redeploying a running app with a different ingress host moves
/// the route, under either deploy strategy. The new host reaches the
/// replacement instances and the old host stops routing. Dropping the
/// ingress section removes the route.
#[tokio::test]
async fn redeploy_with_a_changed_ingress_host_moves_the_route() {
    for strategy in ["rolling", "blue-green"] {
        redeploy_moves_the_ingress_route(strategy).await;
    }
}

/// #307 in cluster mode: the council's route catalogue carries the new
/// host, and a replica node's own stored route must not keep the old
/// one alive underneath it. Dropping the ingress from the spec and from
/// the catalogue leaves no route on the node.
#[tokio::test]
async fn cluster_redeploy_with_a_changed_ingress_host_moves_the_route() {
    let (mut agent, tx, shutdown) = test_agent();
    let routes = agent.routing_table_handle();
    let task = tokio::spawn(async move { agent.run().await });
    let config = |ingress: &str| {
        Config::parse(&format!(
            "[app.web]\nimage = 'myapp:v1'\nport = 8080\n{ingress}"
        ))
        .unwrap()
    };
    let catalogue = |generation: u64, host: Option<&str>| {
        let tx = tx.clone();
        let ingress = host
            .map(|host| crate::cluster::orchestrate::IngressAssignment {
                name: "web".into(),
                namespace: "default".into(),
                config: toml::from_str(&format!("host = '{host}'")).unwrap(),
            })
            .into_iter()
            .collect();
        async move {
            let (response, reply) = oneshot::channel();
            tx.send(AgentCommand::SyncClusterCatalog {
                generation,
                response,
                catalog: Box::default(),
                ingress,
            })
            .await
            .unwrap();
            reply.await.unwrap().unwrap();
        }
    };

    expect_complete(&send_deploy(&tx, config("[app.web.ingress]\nhost = 'a.test'\n")).await);
    catalogue(1, Some("a.test")).await;
    let first = routes.read().await.contains_host("a.test");

    expect_complete(&send_deploy(&tx, config("[app.web.ingress]\nhost = 'b.test'\n")).await);
    catalogue(2, Some("b.test")).await;
    let moved = routes.read().await.contains_host("b.test");
    let old_host_routes = routes.read().await.contains_host("a.test");

    expect_complete(&send_deploy(&tx, config("")).await);
    catalogue(3, None).await;
    let after_drop = routes.read().await.contains_host("b.test");
    shutdown.cancel();
    task.await.unwrap();

    assert!(first, "a.test never routed");
    assert!(moved, "the changed host b.test has no route");
    assert!(!old_host_routes, "the old host a.test still routes");
    assert!(!after_drop, "dropping the ingress left b.test routing");
}

async fn redeploy_moves_the_ingress_route(strategy: &str) {
    let (mut agent, tx, shutdown) = test_agent();
    let routes = agent.routing_table_handle();
    let task = tokio::spawn(async move { agent.run().await });
    let config = |ingress: &str| {
        Config::parse(&format!(
            "[app.web]\nimage = 'myapp:v1'\nport = 8080\n\
                 [app.web.deploy]\nstrategy = '{strategy}'\n{ingress}"
        ))
        .unwrap()
    };
    let backends = |table: &crate::wrapper::routing::RoutingTable, host: &str| {
        table.lookup(host, "/").map(|route| {
            route
                .backends
                .iter()
                .map(|backend| backend.instance_id.clone())
                .collect::<Vec<_>>()
        })
    };

    expect_complete(&send_deploy(&tx, config("[app.web.ingress]\nhost = 'a.test'\n")).await);
    let first = backends(&*routes.read().await, "a.test").expect("a.test has no route");

    let events = send_deploy(&tx, config("[app.web.ingress]\nhost = 'b.test'\n")).await;
    let moved_to = backends(&*routes.read().await, "b.test");
    let old_host_routes = routes.read().await.contains_host("a.test");

    let dropped = send_deploy(&tx, config("")).await;
    let after_drop = routes.read().await.contains_host("b.test");
    shutdown.cancel();
    task.await.unwrap();

    expect_complete(&events);
    expect_complete(&dropped);
    let moved_to = moved_to.unwrap_or_else(|| panic!("{strategy}: b.test has no route"));
    assert_eq!(moved_to.len(), 1, "{strategy}: {moved_to:?}");
    assert_ne!(
        moved_to, first,
        "{strategy}: b.test routes to the old instances"
    );
    assert!(
        !old_host_routes,
        "{strategy}: the old host a.test still routes"
    );
    assert!(
        !after_drop,
        "{strategy}: dropping the ingress left b.test routing"
    );
}

/// Send a Deploy command and collect all events. Returns the list
/// of events (the last one should be Complete or Error).
async fn send_deploy(tx: &mpsc::Sender<AgentCommand>, config: Config) -> Vec<ApplyEvent> {
    let (event_tx, mut event_rx) = mpsc::channel(64);
    tx.send(AgentCommand::Deploy {
        config,
        events: event_tx,
    })
    .await
    .unwrap();

    let mut events = Vec::new();
    while let Some(e) = event_rx.recv().await {
        events.push(e);
    }
    events
}

/// Extract the Complete event from a list of deploy events.
/// Panics if the last event is an Error or if there are no events.
fn expect_complete(events: &[ApplyEvent]) -> (usize, &[String]) {
    match events.last().expect("no events received") {
        ApplyEvent::Complete { created, instances } => (*created, instances),
        ApplyEvent::Error { message } => panic!("deploy failed: {message}"),
        other => panic!("unexpected final event: {other:?}"),
    }
}

fn basic_config() -> Config {
    let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#;
    Config::parse(toml_str).unwrap()
}

fn config_with_health() -> Config {
    let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [app.web.health]
            path = "/healthz"
        "#;
    Config::parse(toml_str).unwrap()
}

fn require_signatures_policy() -> crate::config::node::TrustPolicySection {
    crate::config::node::TrustPolicySection {
        require_signatures: true,
        keys: vec![],
        ..Default::default()
    }
}

/// Phase 12 E0 (review M21): deploying an app with a managed
/// volume creates the host directory before the container starts —
/// runc fails create on a bind mount whose source doesn't exist.
#[tokio::test]
async fn deploy_creates_managed_volume_directories() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    let handle = tokio::spawn(async move {
        agent.run().await;
    });

    let config = Config::parse(
        r#"
            [app.web]
            image = "myapp:v1"

            [[app.web.volumes]]
            path = "/data"
        "#,
    )
    .unwrap();
    let events = send_deploy(&tx, config).await;
    let (created, _) = expect_complete(&events);
    assert_eq!(created, 1);

    assert!(
        volumes_dir
            .path()
            .join("default")
            .join("web")
            .join("data")
            .is_dir(),
        "managed volume host directory must exist after deploy"
    );

    shutdown.cancel();
    let _ = handle.await;
}

/// The agent rechecks the full layout before creating the first directory.
#[tokio::test]
async fn overlapping_volume_layout_is_refused_before_any_directory_is_provisioned() {
    let volumes = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let handle = tokio::spawn(async move { agent.run().await });
    let config = Config::parse(
        "[app.web]\nimage='myapp:v1'\n[[app.web.volumes]]\npath='/data'\n[[app.web.volumes]]\npath='/data.img'\n",
    ).unwrap();
    // Direct agent dispatch deliberately bypasses HTTP configuration admission.
    let events = send_deploy(&tx, config).await;
    shutdown.cancel();
    handle.await.unwrap();
    assert!(
        events.iter().any(
            |event| matches!(event, ApplyEvent::Error { message } if message.contains("overlap"))
        ),
        "{events:?}"
    );
    assert!(!volumes.path().join("default/web").exists());
}

/// Host-path volumes are the operator's responsibility — deploys
/// must not create anything under the managed volumes directory.
#[tokio::test]
async fn deploy_leaves_hostpath_volumes_alone() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    agent.set_host_path_policy(crate::bun::host_paths::HostPathPolicy::new(
        vec![source_dir.path().to_path_buf()],
        vec![volumes_dir.path().to_path_buf()],
    ));
    let handle = tokio::spawn(async move {
        agent.run().await;
    });

    let toml = format!(
        r#"
            [app.web]
            image = "myapp:v1"

            [[app.web.volumes]]
            source = "{}"
            path = "/data"
        "#,
        source_dir.path().display()
    );
    let events = send_deploy(&tx, Config::parse(&toml).unwrap()).await;
    expect_complete(&events);

    assert!(
        !volumes_dir.path().join("default").exists(),
        "host-path volumes must not create managed directories"
    );

    shutdown.cancel();
    let _ = handle.await;
}

/// #797: the OCI spec a fresh instance is created from mounts the path the
/// host-path policy approved, with the symlink in the written source
/// already resolved, so runc mounts exactly what the node checked.
#[cfg(unix)]
#[tokio::test]
async fn a_fresh_instance_mounts_the_resolved_host_path_not_the_symlink() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let import = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(import.path().join("releases/v2")).unwrap();
    std::os::unix::fs::symlink(
        import.path().join("releases/v2"),
        import.path().join("current"),
    )
    .unwrap();
    agent.set_host_path_policy(crate::bun::host_paths::HostPathPolicy::new(
        vec![import.path().to_path_buf()],
        Vec::new(),
    ));
    let config = Config::parse(&format!(
        "[app.web]\nimage = 'web:v1'\n[[app.web.volumes]]\npath = '/data'\nsource = '{}'\n",
        import.path().join("current").display()
    ))
    .unwrap();
    let spec = config.app["web"].clone();
    let ids = agent
        .supervisor
        .deploy_app("web", "default", &spec, Instant::now())
        .await
        .unwrap();
    let prepared = loop {
        match agent
            .prepare_fresh_instance(&ids[0], "web", "default", &spec)
            .await
        {
            Err(BunError::StillRunning { .. }) => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            outcome => break outcome.unwrap(),
        }
    };
    let data = prepared
        .oci_spec
        .mounts
        .iter()
        .find(|mount| mount.destination == std::path::Path::new("/data"))
        .unwrap();
    assert_eq!(
        data.source.as_deref(),
        Some(
            std::fs::canonicalize(import.path().join("releases/v2"))
                .unwrap()
                .as_path()
        )
    );
}

/// #797: a redeploy that replaces running replicas checks host paths too.
/// Adding an unlisted `source` to an app that already runs is refused, and
/// no replacement is created with it.
#[tokio::test]
async fn a_rolling_redeploy_refuses_a_host_path_the_node_does_not_allow() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let unlisted = tempfile::tempdir().unwrap();
    let first = Config::parse("[app.web]\nimage = 'web:v1'\n").unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(first, &ev_tx).await;
    drop(ev_tx);
    let mut events = Vec::new();
    while let Some(event) = ev_rx.recv().await {
        events.push(event);
    }
    expect_complete(&events);

    let creates_before = grill
        .calls()
        .iter()
        .filter(|(call, _)| call == "create")
        .count();
    let second = Config::parse(&format!(
        "[app.web]\nimage = 'web:v2'\n[[app.web.volumes]]\npath = '/steal'\nsource = '{}'\n",
        unlisted.path().display()
    ))
    .unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(second, &ev_tx).await;
    drop(ev_tx);
    let mut events = Vec::new();
    while let Some(event) = ev_rx.recv().await {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            ApplyEvent::Error { message } if message.contains("allowed_host_paths")
        )),
        "{events:?}"
    );
    let creates_after = grill
        .calls()
        .iter()
        .filter(|(call, _)| call == "create")
        .count();
    assert_eq!(creates_before, creates_after, "{:?}", grill.calls());
    assert!(agent.supervisor.list_instances().iter().all(|instance| {
        instance.oci_spec.as_ref().is_none_or(|oci| {
            oci.mounts
                .iter()
                .all(|mount| mount.source.as_deref() != Some(unlisted.path()))
        })
    }));
}

/// Phase 12 E2: restoring a snapshot under a running app is
/// refused — the guard fires before any filesystem checks, so this
/// tests on every platform.
#[tokio::test]
async fn snapshot_restore_refused_while_app_runs() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    let handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    expect_complete(&events);

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::SnapshotRestore {
        namespace: "default".to_string(),
        app_name: "web".to_string(),
        name: "whatever".to_string(),
        volume: None,
        response: resp_tx,
    })
    .await
    .unwrap();
    let result = resp_rx.await.unwrap();
    assert!(
        matches!(
            result,
            Err(BunError::Snapshot(
                crate::grill::snapshot::SnapshotError::AppRunning { .. }
            ))
        ),
        "expected AppRunning, got {result:?}"
    );

    shutdown.cancel();
    let _ = handle.await;
}

/// #340: an operation hands its volumes back before it answers. A client
/// that sends its next snapshot request the moment it has the answer
/// must never be refused as "busy" by the operation it just finished.
/// The hook parks every task after its answer, so a lease still held at
/// that point is guaranteed to be seen.
#[tokio::test]
async fn a_snapshot_operation_releases_its_volumes_before_answering() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    let hold = std::sync::Arc::new(tokio::sync::RwLock::new(()));
    agent.snapshot_answered_hold = Some(hold.clone());
    let handle = tokio::spawn(async move {
        agent.run().await;
    });
    let parked = hold.write().await;

    let create = || {
        let (response, rx) = oneshot::channel();
        let command = AgentCommand::SnapshotCreate {
            namespace: "default".to_string(),
            app_name: "web".to_string(),
            volume: None,
            name: Some("first".to_string()),
            response,
        };
        (command, async move { rx.await.unwrap().map(|_| ()) })
    };
    let restore = || {
        let (response, rx) = oneshot::channel();
        let command = AgentCommand::SnapshotRestore {
            namespace: "default".to_string(),
            app_name: "web".to_string(),
            name: "first".to_string(),
            volume: None,
            response,
        };
        (command, async move { rx.await.unwrap() })
    };
    let delete = || {
        let (response, rx) = oneshot::channel();
        let command = AgentCommand::SnapshotDelete {
            namespace: "default".to_string(),
            app_name: "web".to_string(),
            name: "first".to_string(),
            volume: None,
            response,
        };
        (command, async move { rx.await.unwrap() })
    };
    let busy = |result: &Result<(), BunError>| {
        matches!(
            result,
            Err(BunError::Snapshot(
                crate::grill::snapshot::SnapshotError::Busy { .. }
            ))
        )
    };

    let (command, answer) = create();
    tx.send(command).await.unwrap();
    let first = answer.await;
    assert!(!busy(&first), "create: {first:?}");
    let (command, answer) = restore();
    tx.send(command).await.unwrap();
    let second = answer.await;
    assert!(!busy(&second), "restore right after create: {second:?}");
    let (command, answer) = delete();
    tx.send(command).await.unwrap();
    let third = answer.await;
    assert!(!busy(&third), "delete right after restore: {third:?}");
    let (command, answer) = create();
    tx.send(command).await.unwrap();
    let fourth = answer.await;
    assert!(!busy(&fourth), "create right after delete: {fourth:?}");

    drop(parked);
    shutdown.cancel();
    let _ = handle.await;
}

/// B03: once a restore is accepted, it owns the app's volumes until it
/// resolves. A deploy and a second restore sent while it's paused are
/// refused and never touch the volume directory; after the first
/// restore resolves, the deploy goes through.
#[tokio::test]
async fn an_accepted_restore_owns_the_volumes_until_it_resolves() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    let pause = std::sync::Arc::new(std::sync::Barrier::new(2));
    agent.restore_pause = Some(pause.clone());
    let handle = tokio::spawn(async move {
        agent.run().await;
    });

    let restore = |name: &str| {
        let (resp_tx, resp_rx) = oneshot::channel();
        let command = AgentCommand::SnapshotRestore {
            namespace: "default".to_string(),
            app_name: "web".to_string(),
            name: name.to_string(),
            volume: None,
            response: resp_tx,
        };
        (command, resp_rx)
    };
    let (first, first_rx) = restore("before-upgrade");
    tx.send(first).await.unwrap();

    let config = Config::parse(
        r#"
            [app.web]
            image = "myapp:v1"

            [[app.web.volumes]]
            path = "/data"
        "#,
    )
    .unwrap();
    let events = send_deploy(&tx, config.clone()).await;
    match events.last() {
        Some(ApplyEvent::Error { message }) => {
            assert!(message.contains("being restored"), "{message}")
        }
        other => panic!("a deploy during a restore was not refused: {other:?}"),
    }
    let (second, second_rx) = restore("other");
    tx.send(second).await.unwrap();
    assert!(
        matches!(
            second_rx.await.unwrap(),
            Err(BunError::Snapshot(
                crate::grill::snapshot::SnapshotError::Busy { .. }
            ))
        ),
        "a second restore must wait for the first"
    );
    assert!(
        !volumes_dir.path().join("default").exists(),
        "nothing may touch the volume while the restore owns it"
    );

    // Let the first restore run. There's no snapshot to restore, so it
    // fails, and in failing gives up its ownership.
    tokio::task::spawn_blocking(move || pause.wait())
        .await
        .unwrap();
    assert!(first_rx.await.unwrap().is_err());

    let events = send_deploy(&tx, config).await;
    let (created, _) = expect_complete(&events);
    assert_eq!(created, 1);
    assert!(volumes_dir.path().join("default/web/data").is_dir());

    shutdown.cancel();
    let _ = handle.await;
}

/// B03: a stopped instance waiting for its automatic restart will start
/// again on its own, so a restore must treat it as running. The old
/// check looked only at the state and accepted the restore.
#[tokio::test]
async fn a_restore_is_refused_while_an_instance_awaits_its_restart() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    let id = InstanceId("default__web-0".to_string());
    agent.supervisor.instances.insert(
        id.clone(),
        super::super::supervisor::WorkloadInstance {
            id: id.clone(),
            app_name: "web".into(),
            namespace: "default".into(),
            state: ContainerState::Stopped,
            health_counters: Default::default(),
            restart_count: 1,
            last_restart: Some(Instant::now()),
            host_port: None,
            container_ip: None,
            created_at: Instant::now(),
            // Keep the restart pending for the whole test.
            restart_policy: crate::bun::restart::RestartPolicy {
                initial_backoff: std::time::Duration::from_secs(3600),
                ..Default::default()
            },
            health_config: None,
            retry_pending: true,
            image: "myapp:v1".into(),
            oci_spec: None,
            identity: None,
            identity_mount: None,
            launch_failure: None,
        },
    );
    agent
        .supervisor
        .app_instances
        .entry(("web".into(), "default".into()))
        .or_default()
        .push(id);
    let handle = tokio::spawn(async move {
        agent.run().await;
    });

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::SnapshotRestore {
        namespace: "default".to_string(),
        app_name: "web".to_string(),
        name: "before-upgrade".to_string(),
        volume: None,
        response: resp_tx,
    })
    .await
    .unwrap();
    let result = resp_rx.await.unwrap();
    assert!(
        matches!(
            result,
            Err(BunError::Snapshot(
                crate::grill::snapshot::SnapshotError::AppRunning { .. }
            ))
        ),
        "expected AppRunning, got {result:?}"
    );

    shutdown.cancel();
    let _ = handle.await;
}

/// B03: a caller that gives up on an accepted restore doesn't end its
/// ownership; the restore still holds the volumes until it resolves.
#[tokio::test]
async fn a_restore_keeps_its_volumes_after_the_caller_goes_away() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    let pause = std::sync::Arc::new(std::sync::Barrier::new(2));
    agent.restore_pause = Some(pause.clone());
    let handle = tokio::spawn(async move {
        agent.run().await;
    });

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::SnapshotRestore {
        namespace: "default".to_string(),
        app_name: "web".to_string(),
        name: "before-upgrade".to_string(),
        volume: None,
        response: resp_tx,
    })
    .await
    .unwrap();
    drop(resp_rx);

    let events = send_deploy(&tx, basic_config()).await;
    assert!(
        matches!(events.last(), Some(ApplyEvent::Error { .. })),
        "{events:?}"
    );

    tokio::task::spawn_blocking(move || pause.wait())
        .await
        .unwrap();
    shutdown.cancel();
    let _ = handle.await;
}

/// Phase 12 E2: snapshotting an app with no provisioned volumes is
/// an honest NoVolumes error, not an empty success.
#[tokio::test]
async fn snapshot_create_without_volumes_errors() {
    let volumes_dir = tempfile::tempdir().unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_volumes_dir(volumes_dir.path().to_path_buf());
    let handle = tokio::spawn(async move {
        agent.run().await;
    });

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::SnapshotCreate {
        namespace: "default".to_string(),
        app_name: "ghost".to_string(),
        volume: None,
        name: None,
        response: resp_tx,
    })
    .await
    .unwrap();
    let result = resp_rx.await.unwrap();
    assert!(matches!(
        result,
        Err(BunError::Snapshot(
            crate::grill::snapshot::SnapshotError::NoVolumes { .. }
        ))
    ));

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn single_node_image_deploy_is_refused_without_trust_state() {
    // require_signatures is on and there's no council to consult, so a
    // standalone image deploy can't be verified. It fails CLOSED: no
    // instances come up (IMG2). The old behaviour let it through.
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_trust_policy(require_signatures_policy());
    let handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    assert!(
        events.iter().any(|e| matches!(e, ApplyEvent::Error { .. })),
        "an unverifiable image deploy must be refused, got: {events:?}"
    );
    let created = events.iter().find_map(|e| match e {
        ApplyEvent::Complete { created, .. } => Some(*created),
        _ => None,
    });
    assert_ne!(created, Some(1), "no instance should be created");
    let (snapshot_tx, snapshot_rx) = oneshot::channel();
    tx.send(AgentCommand::DeployOperations {
        response: snapshot_tx,
    })
    .await
    .unwrap();
    let snapshot = snapshot_rx.await.unwrap();
    assert!(snapshot.active_deploys.is_empty());
    assert_eq!(snapshot.history.len(), 1);
    assert_eq!(
        snapshot.history[0].outcome,
        Some(crate::bun::deploy_operations::DeployOperationOutcome::Failed)
    );

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn enforce_image_signature_fails_closed_without_council() {
    let (mut agent, _tx, _shutdown) = test_agent();
    agent.set_trust_policy(require_signatures_policy());
    let spec: AppSpec = toml::from_str(r#"image = "myapp:v1""#).unwrap();
    // No cluster/council → the gate can't obtain verification material, so
    // it must refuse rather than skip (IMG2 fail-closed).
    let result = agent.enforce_image_signature(spec.image.as_deref()).await;
    assert!(result.is_err(), "expected refusal, got {result:?}");
    assert!(
        result.unwrap_err().contains("requires a signature"),
        "the refusal should name the missing verification"
    );
}

#[tokio::test]
async fn enforce_image_signature_allows_a_process_workload_without_council() {
    let (mut agent, _tx, _shutdown) = test_agent();
    agent.set_trust_policy(require_signatures_policy());
    // A process workload has no image — nothing to verify, so it passes
    // even with require_signatures on and no council.
    let spec: AppSpec = toml::from_str(r#"command = ["echo", "hi"]"#).unwrap();
    assert!(
        agent
            .enforce_image_signature(spec.image.as_deref())
            .await
            .is_ok()
    );
}

// --- relish sign, end to end (operator key → attach → deploy gate) ---

/// A single-node leader council, enough to hold a manifest catalogue.
async fn catalogue_council(raft_port: u16) -> Arc<CouncilNode> {
    use crate::council::log_store::MemLogStore;
    use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use crate::council::state_machine::CouncilStateMachine;
    use crate::council::types::CouncilConfig;

    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let node = CouncilNode::new(
        1,
        CouncilConfig::default(),
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], raft_port));
    node.initialize(std::collections::BTreeMap::from([(
        1u64,
        CouncilNodeInfo::new(address, "node-1".to_string()),
    )]))
    .await
    .unwrap();
    for _ in 0..40 {
        if node.is_leader().await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Arc::new(node)
}

/// Push (commit) an unsigned manifest `repository:tag` with `digest_hex`.
async fn push_manifest(council: &CouncilNode, repository: &str, tag: &str, digest_hex: &str) {
    use crate::pickle::types::{Digest, ImageManifest, LayerDescriptor, ManifestCommit};
    let commit = ManifestCommit {
        observed_gc_generation: 0,
        manifest: ImageManifest {
            digest: Digest::from_sha256_hex(digest_hex),
            config: LayerDescriptor {
                digest: Digest::from_sha256_hex(&"c".repeat(64)),
                size: 100,
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                platform: None,
            },
            layers: vec![],
            repository: repository.to_string(),
            tags: std::collections::BTreeSet::new(),
            total_size: 100,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None,
        },
        tag: tag.to_string(),
        holder_nodes: std::collections::BTreeSet::from([1]),
    };
    let response = council
        .write(crate::council::RaftRequest::ManifestCommit(commit))
        .await
        .unwrap();
    assert!(
        matches!(
            response,
            crate::council::types::CouncilResponse::Applied { .. }
        ),
        "manifest commit: {response:?}"
    );
}

fn agent_with_council(council: Arc<CouncilNode>) -> BunAgent<MockGrill> {
    let (_membership_tx, membership_rx) = tokio::sync::watch::channel(Vec::new());
    let (_snapshot_tx, snapshot_rx) = mpsc::channel(1);
    let (_command_tx, command_rx) = mpsc::channel(8);
    let cluster = ClusterHandle {
        local_node_id: crate::meat::NodeId::new("node-1"),
        membership_rx,
        raft_metrics_rx: None,
        council: Some(council),
        snapshot_rx,
        wrapping_ikm: None,
        partition_blocklists: PartitionBlocklists::default(),
        crl_handle: Default::default(),
    };
    BunAgent::with_cluster(
        MockGrill::new(),
        PortAllocator::new(30000, 31000),
        command_rx,
        CancellationToken::new(),
        cluster,
        "test".to_string(),
    )
}

/// Do what `relish sign IMAGE --key KEY` does against this council:
/// resolve the reference through the image listing, sign the digest
/// locally, and hand the submission to the node.
async fn relish_sign(
    agent: &BunAgent<MockGrill>,
    council: &CouncilNode,
    image: &str,
    key: &crate::pickle::signing::SigningKey,
) -> Result<String, BunError> {
    let images = council.manifest_catalog().await.images();
    let digest = crate::relish::commands::resolve_image_digest(image, &images).unwrap();
    agent.handle_sign_image(key.sign(&digest).unwrap()).await
}

fn app(image: &str) -> AppSpec {
    toml::from_str(&format!("image = {image:?}")).unwrap()
}

#[tokio::test]
async fn relish_signed_image_is_admitted_only_under_a_policy_trusting_its_key() {
    let council = catalogue_council(9301).await;
    let signed = "1".repeat(64);
    push_manifest(&council, "myapp", "v1", &signed).await;
    push_manifest(&council, "unsigned", "v1", &"2".repeat(64)).await;
    push_manifest(&council, "stranger", "v1", &"3".repeat(64)).await;
    let mut agent = agent_with_council(council.clone());

    let operator = crate::pickle::signing::SigningKey::generate().unwrap();
    let stranger = crate::pickle::signing::SigningKey::generate().unwrap();
    let message = relish_sign(&agent, &council, "myapp:v1", &operator)
        .await
        .unwrap();
    assert!(message.contains(&format!("sha256:{signed}")), "{message}");
    assert!(
        message.contains("does not list this key"),
        "an untrusted key must be called out: {message}"
    );
    relish_sign(&agent, &council, "stranger:v1", &stranger)
        .await
        .unwrap();

    agent.set_trust_policy(crate::config::node::TrustPolicySection {
        require_signatures: true,
        keys: vec![operator.public_key_base64()],
        ..Default::default()
    });

    // Signed with the trusted key: admitted, pinned to the signed digest.
    let pinned = agent
        .enforce_image_signature(app("myapp:v1").image.as_deref())
        .await;
    assert_eq!(pinned, Ok(Some(format!("myapp@sha256:{signed}"))));
    // Never signed: refused.
    let unsigned = agent
        .enforce_image_signature(app("unsigned:v1").image.as_deref())
        .await;
    assert!(unsigned.is_err(), "unsigned image admitted: {unsigned:?}");
    // Signed, but by a key the policy doesn't list: refused.
    let untrusted = agent
        .enforce_image_signature(app("stranger:v1").image.as_deref())
        .await;
    assert!(
        untrusted
            .as_ref()
            .is_err_and(|reason| reason.contains("not in trust policy")),
        "other-key image admitted: {untrusted:?}"
    );

    // With the trusted key listed, signing reports no warning.
    let message = relish_sign(&agent, &council, "myapp:v1", &operator)
        .await
        .unwrap();
    assert!(!message.contains("warning"), "{message}");
}

#[tokio::test]
async fn moving_a_tag_after_signing_leaves_the_new_digest_unsigned() {
    let council = catalogue_council(9302).await;
    push_manifest(&council, "myapp", "v1", &"1".repeat(64)).await;
    let mut agent = agent_with_council(council.clone());
    let operator = crate::pickle::signing::SigningKey::generate().unwrap();
    relish_sign(&agent, &council, "myapp:v1", &operator)
        .await
        .unwrap();
    agent.set_trust_policy(crate::config::node::TrustPolicySection {
        require_signatures: true,
        keys: vec![operator.public_key_base64()],
        ..Default::default()
    });

    // Someone re-pushes v1 with different bytes: the signature covered
    // the old digest, not the tag, so the new content is refused.
    push_manifest(&council, "myapp", "v1", &"4".repeat(64)).await;
    let result = agent
        .enforce_image_signature(app("myapp:v1").image.as_deref())
        .await;
    assert!(result.is_err(), "re-tagged content admitted: {result:?}");
}

#[tokio::test]
async fn signing_a_digest_the_catalogue_does_not_hold_is_refused() {
    let council = catalogue_council(9303).await;
    let agent = agent_with_council(council);
    let operator = crate::pickle::signing::SigningKey::generate().unwrap();
    let digest = crate::pickle::types::Digest::from_sha256_hex(&"5".repeat(64));
    let result = agent
        .handle_sign_image(operator.sign(&digest).unwrap())
        .await;
    assert!(
        matches!(&result, Err(BunError::SecurityError { reason }) if reason.contains("refused")),
        "got: {result:?}"
    );
}

// --- upstream trust rules at deploy (F03 U2, #361) ---

/// `[images.trust_policy]` allowing only Docker Hub's official images.
fn official_images_only() -> crate::config::node::TrustPolicySection {
    crate::config::node::TrustPolicySection {
        upstream: vec![crate::config::node::UpstreamTrustRule {
            pattern: "docker.io/library/*".to_string(),
            require_signatures: false,
            cosign_keys: vec![],
        }],
        upstream_default: crate::config::node::UpstreamDefault { allow: false },
        ..Default::default()
    }
}

/// A council-backed agent whose runtime pulls images, under `policy`.
async fn pulling_agent(
    raft_port: u16,
    policy: crate::config::node::TrustPolicySection,
) -> (Arc<CouncilNode>, BunAgent<MockGrill>) {
    let council = catalogue_council(raft_port).await;
    let mut agent = agent_with_council(council.clone());
    agent
        .supervisor
        .grill()
        .set_runtime_kind(crate::grill::records::RuntimeKind::Runc);
    agent.set_trust_policy(policy);
    (council, agent)
}

/// Bun is the enforcement: an image the apply never judged (a spec already
/// in Raft, a node with a stricter policy) is refused at deploy, by name.
#[tokio::test]
async fn a_deploy_refuses_an_upstream_image_the_rules_do_not_allow() {
    let (_council, agent) = pulling_agent(9310, official_images_only()).await;
    let refused = agent
        .enforce_image_signature(app("ghcr.io/evil/miner:1").image.as_deref())
        .await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|reason| reason.contains("ghcr.io/evil/miner:1")),
        "{refused:?}"
    );
    let digest = format!("sha256:{}", "a".repeat(64));
    let allowed = agent
        .enforce_image_signature(app(&format!("nginx:1.27@{digest}")).image.as_deref())
        .await;
    assert_eq!(allowed, Ok(None));
}

/// The rules are for images from outside Pickle.
#[tokio::test]
async fn a_deploy_leaves_pickle_images_to_require_signatures() {
    let (council, agent) = pulling_agent(9311, official_images_only()).await;
    push_manifest(&council, "team/web", "v1", &"5".repeat(64)).await;
    let result = agent
        .enforce_image_signature(app("localhost:5050/team/web:v1").image.as_deref())
        .await;
    assert_eq!(result, Ok(None));
}

/// A process runtime never pulls an image: it refuses image apps (#914). So
/// there's nothing for the upstream rules to judge (decision 5).
#[tokio::test]
async fn upstream_rules_do_not_judge_images_a_process_runtime_never_pulls() {
    let council = catalogue_council(9312).await;
    let mut agent = agent_with_council(council);
    agent.set_trust_policy(official_images_only());
    let result = agent
        .enforce_image_signature(app("registry.example.com/web:1").image.as_deref())
        .await;
    assert_eq!(result, Ok(None));
}

// --- cosign signatures on upstream images at deploy (F03 U3, #361) ---

/// A standalone agent whose runtime pulls images, requiring a cosign
/// signature by `key` on `ghcr.io/acme/*`, reading signatures from the
/// fixture registry (which holds the `.sig` for the fixture digest only).
fn cosign_agent(key: &str) -> (TestAgent, mpsc::Sender<AgentCommand>, CancellationToken) {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    grill.set_runtime_kind(crate::grill::records::RuntimeKind::Runc);
    agent.set_trust_policy(crate::config::node::TrustPolicySection {
        upstream: vec![crate::config::node::UpstreamTrustRule {
            pattern: "ghcr.io/acme/*".to_string(),
            require_signatures: true,
            cosign_keys: vec![key.to_string()],
        }],
        ..Default::default()
    });
    let registry: Arc<dyn crate::pickle::upstream::UpstreamRegistry> =
        Arc::new(crate::pickle::cosign::fixture::FixtureRegistry { signed: true });
    agent.set_signature_source(crate::pickle::cosign::SignatureSource::new(
        registry.clone(),
        registry,
    ));
    (agent, tx, shutdown)
}

fn web_config(image: &str) -> Config {
    Config::parse(&format!("[app.web]\nimage = \"{image}\"\n")).unwrap()
}

/// Deploy `image` through the agent loop; the error event's message, if any.
async fn deploy_refusal(key: &str, image: &str) -> Option<String> {
    let (agent, tx, shutdown) = cosign_agent(key);
    let handle = tokio::spawn(async move {
        let mut agent = agent;
        agent.run().await;
    });
    let events = send_deploy(&tx, web_config(image)).await;
    shutdown.cancel();
    let _ = handle.await;
    events.into_iter().find_map(|event| match event {
        ApplyEvent::Error { message } => Some(message),
        _ => None,
    })
}

#[tokio::test]
async fn an_image_signed_by_a_trusted_cosign_key_deploys() {
    use crate::pickle::cosign::fixture::{PUBLIC_KEY, SIGNED_DIGEST};
    let image = format!("ghcr.io/acme/web:1.2@{SIGNED_DIGEST}");
    assert_eq!(deploy_refusal(PUBLIC_KEY, &image).await, None);
}

#[tokio::test]
async fn an_unsigned_image_is_refused_at_deploy_by_name() {
    use crate::pickle::cosign::fixture::PUBLIC_KEY;
    let image = format!("ghcr.io/acme/web:1.2@sha256:{}", "1".repeat(64));
    let refusal = deploy_refusal(PUBLIC_KEY, &image).await.expect("refused");
    assert!(refusal.contains(&image), "{refusal}");
    assert!(refusal.contains("no cosign signature"), "{refusal}");
}

#[tokio::test]
async fn an_image_signed_by_another_key_is_refused_at_deploy_by_name() {
    use crate::pickle::cosign::fixture::{OTHER_KEY, SIGNED_DIGEST};
    let image = format!("ghcr.io/acme/web:1.2@{SIGNED_DIGEST}");
    let refusal = deploy_refusal(OTHER_KEY, &image).await.expect("refused");
    assert!(refusal.contains(&image), "{refusal}");
    assert!(
        refusal.contains("verifies under the trusted keys"),
        "{refusal}"
    );
}

/// A signature covers a digest, so a tag the apply didn't bind can't be
/// checked; the deploy says so rather than guess.
#[tokio::test]
async fn an_unbound_image_under_a_signature_rule_is_refused() {
    use crate::pickle::cosign::fixture::PUBLIC_KEY;
    let refusal = deploy_refusal(PUBLIC_KEY, "ghcr.io/acme/web:1.2")
        .await
        .expect("refused");
    assert!(refusal.contains("ghcr.io/acme/web:1.2"), "{refusal}");
    assert!(refusal.contains("isn't bound"), "{refusal}");
}

/// Images no signature rule names deploy as before.
#[tokio::test]
async fn an_image_outside_the_signature_rules_needs_no_signature() {
    use crate::pickle::cosign::fixture::PUBLIC_KEY;
    assert_eq!(deploy_refusal(PUBLIC_KEY, "nginx:1.27").await, None);
}

#[tokio::test]
async fn shutdown_escalates_to_kill_when_stop_is_ignored() {
    let (_tx, rx) = mpsc::channel(8);
    let shutdown = CancellationToken::new();
    let grill = MockGrill::new();
    let grill_handle = grill.clone();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);
    // Escalation is under test, not the length of the production grace.
    agent.set_shutdown_grace(std::time::Duration::from_millis(200));

    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(basic_config(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    // Pin the instance to Running so stop() is effectively ignored (the
    // process refuses SIGTERM). shutdown_all must escalate to SIGKILL.
    let id = InstanceId("default__web-0".to_string());
    grill_handle.set_state(&id, ContainerState::Running);

    agent.shutdown_all().await;

    let calls = grill_handle.calls();
    assert!(
        calls
            .iter()
            .any(|(op, i)| op == "stop" && i.0 == "default__web-0"),
        "shutdown should SIGTERM first"
    );
    assert!(
        calls
            .iter()
            .any(|(op, i)| op == "kill" && i.0 == "default__web-0"),
        "shutdown should escalate to SIGKILL when the process ignores stop"
    );
}

#[tokio::test]
async fn deploy_command_creates_instances() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    let (created, instances) = expect_complete(&events);
    assert_eq!(created, 1);
    assert_eq!(instances, &["default__web-0"]);

    shutdown.cancel();
    agent_handle.await.unwrap();
}

/// #881: a binary the node can't run fails the deploy at once with the
/// reason, and the instance is `failed` with that reason in its status,
/// instead of sitting in `starting` while something retries.
#[tokio::test]
async fn a_refused_launch_fails_the_deploy_and_the_instance_with_its_reason() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    grill.set_refused_launch(Some(crate::grill::LaunchProblem::BinaryNotFound(
        "/opt/ghost/server".into(),
    )));
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    let Some(ApplyEvent::Error { message }) = events.last() else {
        panic!("the deploy must fail: {events:?}");
    };
    assert!(
        message.contains("binary not found: /opt/ghost/server"),
        "{message}"
    );

    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::Status { response }).await.unwrap();
    let instances = result.await.unwrap();
    shutdown.cancel();
    agent_handle.await.unwrap();
    let web = instances
        .iter()
        .find(|instance| instance.app_name == "web")
        .expect("the failed instance stays listed");
    assert_eq!(web.state, "failed");
    assert_eq!(
        web.reason.as_deref(),
        Some("binary not found: /opt/ghost/server")
    );
    assert!(
        !grill.calls().iter().any(|(call, _)| call == "start"),
        "a refused launch must never be started: {:?}",
        grill.calls()
    );
}

/// #905: a binary deleted between the pre-launch check and the owner gate's
/// `exec` fails the deploy and the instance with the gate's reason, as one
/// missing at `create` does, instead of leaving the instance in `starting`.
#[tokio::test]
async fn a_launch_refused_at_exec_fails_the_deploy_and_the_instance_with_its_reason() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    grill.set_refused_start(Some(crate::grill::LaunchProblem::BinaryNotFound(
        "/opt/ghost/server".into(),
    )));
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    let Some(ApplyEvent::Error { message }) = events.last() else {
        panic!("the deploy must fail: {events:?}");
    };
    assert!(
        message.contains("binary not found: /opt/ghost/server"),
        "{message}"
    );

    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::Status { response }).await.unwrap();
    let instances = result.await.unwrap();
    shutdown.cancel();
    agent_handle.await.unwrap();
    let web = instances
        .iter()
        .find(|instance| instance.app_name == "web")
        .expect("the failed instance stays listed");
    assert_eq!(web.state, "failed");
    assert_eq!(
        web.reason.as_deref(),
        Some("binary not found: /opt/ghost/server")
    );
}

/// What an instance looks like after restart ticks the runtime refused.
struct RefusedRestart {
    id: InstanceId,
    state: ContainerState,
    launch_failure: Option<String>,
    retry_pending: bool,
    restart_count: u32,
}

/// The instance after restart ticks that the runtime refused (#905).
async fn restart_refused_by(
    refuse: impl FnOnce(&MockGrill, crate::grill::LaunchProblem),
) -> (RefusedRestart, MockGrill) {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = agent.supervisor.list_instances()[0].id.clone();
    // The binary vanishes while the app runs, then the app exits.
    refuse(
        &grill,
        crate::grill::LaunchProblem::BinaryNotFound("/opt/ghost/server".into()),
    );
    grill.set_state(&id, ContainerState::Stopped);
    agent.check_apps().await;
    agent.drive_pending_restarts_to_completion().await;
    // Later ticks must not spend the restart budget on it.
    agent.check_apps().await;
    agent.drive_pending_restarts_to_completion().await;
    let instance = agent.supervisor.get_instance(&id).unwrap();
    let refused = RefusedRestart {
        id: id.clone(),
        state: instance.state,
        launch_failure: instance.launch_failure.clone(),
        retry_pending: instance.retry_pending,
        restart_count: instance.restart_count,
    };
    (refused, grill)
}

/// #905: a restart the runtime refuses at `create` because the binary has
/// gone is terminal, with the reason, as a fresh deploy's is. It doesn't
/// spend the restart budget and end `Failed` without saying why.
#[tokio::test]
async fn a_restart_after_the_binary_vanished_fails_at_once_with_binary_not_found() {
    let (instance, grill) =
        restart_refused_by(|grill, problem| grill.set_refused_launch(Some(problem))).await;
    assert_eq!(instance.state, ContainerState::Failed);
    assert_eq!(
        instance.launch_failure.as_deref(),
        Some("binary not found: /opt/ghost/server")
    );
    assert!(!instance.retry_pending, "a refused launch is not retried");
    assert_eq!(
        instance.restart_count, 1,
        "one attempt, not the whole budget"
    );
    let creates = grill
        .calls()
        .iter()
        .filter(|(call, id)| call == "create" && id == &instance.id)
        .count();
    assert_eq!(creates, 2, "the deploy's create and one restart's");
}

/// #905: a restart whose `exec` the owner gate reports failed is terminal
/// with the reason too.
#[tokio::test]
async fn a_restart_refused_at_exec_fails_at_once_with_binary_not_found() {
    let (instance, _grill) =
        restart_refused_by(|grill, problem| grill.set_refused_start(Some(problem))).await;
    assert_eq!(instance.state, ContainerState::Failed);
    assert_eq!(
        instance.launch_failure.as_deref(),
        Some("binary not found: /opt/ghost/server")
    );
    assert!(!instance.retry_pending, "a refused launch is not retried");
    assert_eq!(
        instance.restart_count, 1,
        "one attempt, not the whole budget"
    );
}

/// DEP4/codex-M3: a deploy that blocks on a slow image pull must not
/// wedge the command loop. While one deploy is stuck inside `create`,
/// a `Status` command on the running loop still answers promptly. With
/// the old serial deploy (awaited inline in the command arm) this
/// `Status` could not be serviced until the pull finished.
#[tokio::test]
async fn slow_health_probe_does_not_block_status_or_shutdown() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
    });
    let (mut agent, tx, shutdown) = test_agent();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let task = tokio::spawn(async move { agent.run().await });
    let config = Config::parse(&format!(
        r#"[app.web]
image = "test:v1"
port = {port}
[app.web.health]
path = "/health"
timeout = 3
interval = 1
"#
    ))
    .unwrap();
    send_deploy(&tx, config).await;
    tokio::time::timeout(std::time::Duration::from_secs(3), started_rx)
        .await
        .unwrap()
        .unwrap();
    let (response, received) = tokio::sync::oneshot::channel();
    tx.send(AgentCommand::Status { response }).await.unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_millis(500), received).await;
    shutdown.cancel();
    let mut task = task;
    let stopped = tokio::time::timeout(std::time::Duration::from_millis(500), &mut task).await;
    task.abort();
    server.abort();
    assert!(
        status.is_ok(),
        "a slow health probe blocked the command loop"
    );
    assert!(stopped.is_ok(), "a slow health probe blocked shutdown");
}

/// A cluster agent whose report-worker end of the snapshot channel stays
/// with the test, so a test can ask for snapshots as the worker does.
fn test_cluster_agent_with_snapshots() -> (
    TestAgent,
    mpsc::Sender<AgentCommand>,
    mpsc::Sender<CollectSnapshotRequest>,
    CancellationToken,
    MockGrill,
) {
    let (_membership_tx, membership_rx) = tokio::sync::watch::channel(Vec::new());
    let (snapshot_tx, snapshot_rx) = mpsc::channel(16);
    let (command_tx, command_rx) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let cluster = ClusterHandle {
        local_node_id: crate::meat::NodeId::new("test"),
        membership_rx,
        raft_metrics_rx: None,
        council: None,
        snapshot_rx,
        wrapping_ikm: None,
        partition_blocklists: PartitionBlocklists::default(),
        crl_handle: Default::default(),
    };
    let grill = MockGrill::new();
    let mut agent = BunAgent::with_cluster(
        grill.clone(),
        PortAllocator::new(30000, 31000),
        command_rx,
        shutdown.clone(),
        cluster,
        "test".to_string(),
    );
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    agent.set_stop_confirmation_timeout(TEST_STOP_CONFIRMATION_TIMEOUT);
    let agent = TestAgent {
        agent,
        _volumes: volumes,
    };
    (agent, command_tx, snapshot_tx, shutdown, grill)
}

async fn stop_agent_task(shutdown: CancellationToken, mut task: tokio::task::JoinHandle<()>) {
    shutdown.cancel();
    if tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
}

/// V02 final tier: during the `relish test` pulse the loop always had
/// work waiting, and the report worker's snapshot request sat behind all
/// of it. It missed its two-second deadline for over a minute, the leader
/// called the node stale, and healthy apps moved off it. A report must
/// wait for at most the one piece of work already running.
#[tokio::test]
async fn snapshot_request_is_answered_before_a_backlog_of_slow_commands() {
    let (mut agent, tx, snapshot_tx, shutdown, grill) = test_cluster_agent_with_snapshots();
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 3\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    // Three pid reads per Status at 100 ms each: twenty queued Status
    // commands are six seconds of loop work.
    grill.set_pid_delay(Some(std::time::Duration::from_millis(100)));
    let mut replies = Vec::new();
    for _ in 0..20 {
        let (response, reply) = oneshot::channel();
        tx.send(AgentCommand::Status { response }).await.unwrap();
        replies.push(reply);
    }
    let task = tokio::spawn(async move { agent.run().await });
    // Let the loop start on the backlog before the worker asks.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let (response, snapshot) = oneshot::channel();
    snapshot_tx
        .send(CollectSnapshotRequest { response })
        .await
        .unwrap();
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(2), snapshot)
        .await
        .expect("the snapshot waited behind the whole command backlog")
        .unwrap();
    assert_eq!(snapshot.instances.len(), 3);
    for reply in replies {
        tokio::time::timeout(std::time::Duration::from_secs(30), reply)
            .await
            .unwrap()
            .unwrap();
    }
    stop_agent_task(shutdown, task).await;
}

/// A request the worker already gave up on has nobody to answer. Building
/// it anyway (an inventory read of up to a second each) only pushed the
/// next live request further past its deadline.
#[tokio::test]
async fn abandoned_snapshot_requests_are_not_built() {
    let (agent, _tx, snapshot_tx, shutdown, grill) = test_cluster_agent_with_snapshots();
    let mut agent = agent;
    grill.set_inventory_delay(Some(std::time::Duration::from_secs(5)));
    for _ in 0..3 {
        let (response, abandoned) = oneshot::channel();
        drop(abandoned);
        snapshot_tx
            .send(CollectSnapshotRequest { response })
            .await
            .unwrap();
    }
    let (response, live) = oneshot::channel();
    snapshot_tx
        .send(CollectSnapshotRequest { response })
        .await
        .unwrap();
    let task = tokio::spawn(async move { agent.run().await });
    // The live request costs one bounded (1 s) inventory read; each
    // abandoned one built first would add another.
    tokio::time::timeout(std::time::Duration::from_millis(2500), live)
        .await
        .expect("abandoned requests were built before the live one")
        .unwrap();
    stop_agent_task(shutdown, task).await;
}

/// V02 soak: a node killed with `kill_containers` comes back with every
/// replica waiting for its restart, and each health tick spends a few
/// hundred milliseconds per replica on runtime cleanup it cannot finish
/// yet. Ticks then run back to back. Commands queued behind one of those
/// ticks (a fault clear, the leader's fence, the consumer view that would
/// let the restarts finish) must be answered before the next tick starts,
/// not raced against it one coin toss at a time.
#[tokio::test]
async fn queued_commands_are_answered_before_the_next_slow_health_tick() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 3\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    let ids: Vec<InstanceId> = agent
        .supervisor
        .list_instances()
        .iter()
        .map(|instance| instance.id.clone())
        .collect();
    assert_eq!(ids.len(), 3);
    for id in &ids {
        let instance = agent.supervisor.get_instance_mut(id).unwrap();
        instance.state = ContainerState::Pending;
        instance.restart_count = 1;
    }
    // Each pending restart spends 400 ms failing to clean up its old
    // runtime, so every tick lasts 1.2 s: longer than the 1 s interval.
    const PER_RESTART: std::time::Duration = std::time::Duration::from_millis(400);
    grill.set_kill_delay(Some(PER_RESTART));
    grill.set_fail_kill(true);
    let kills = |grill: &MockGrill| {
        grill
            .calls()
            .iter()
            .filter(|(operation, _)| operation == "kill")
            .count()
    };
    let task = tokio::spawn(async move { agent.run().await });
    // Wait until a slow tick is under way, so the commands queue behind it.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while kills(&grill) < ids.len() + 1 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let kills_when_queued = kills(&grill);
    let mut replies = Vec::new();
    for _ in 0..8 {
        let (response, reply) = oneshot::channel();
        tx.send(AgentCommand::Status { response }).await.unwrap();
        replies.push(reply);
    }
    for reply in replies {
        tokio::time::timeout(std::time::Duration::from_secs(30), reply)
            .await
            .unwrap()
            .unwrap();
    }
    let kills_while_queued = kills(&grill) - kills_when_queued;
    shutdown.cancel();
    let mut task = task;
    if tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
    // At most the rest of the tick that was already running.
    assert!(
        kills_while_queued <= ids.len(),
        "{kills_while_queued} restart cleanups ran while 8 commands waited: \
             later health ticks overtook queued commands"
    );
}

/// Put `count` replicas into a pending restart whose runtime cleanup
/// spends `per_restart` and then fails, as after `kill_containers`.
async fn slow_pending_restarts(
    count: u32,
    per_restart: std::time::Duration,
) -> (TestAgent, MockGrill, Vec<InstanceId>) {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let config = Config::parse(&format!(
        "[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = {count}\n"
    ))
    .unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    let ids: Vec<InstanceId> = agent
        .supervisor
        .list_instances()
        .iter()
        .map(|instance| instance.id.clone())
        .collect();
    for id in &ids {
        let instance = agent.supervisor.get_instance_mut(id).unwrap();
        instance.state = ContainerState::Pending;
        instance.restart_count = 1;
    }
    grill.set_kill_delay(Some(per_restart));
    grill.set_fail_kill(true);
    (agent, grill, ids)
}

fn kills_per_instance(grill: &MockGrill) -> std::collections::HashMap<InstanceId, usize> {
    let mut kills = std::collections::HashMap::new();
    for (operation, id) in grill.calls() {
        if operation == "kill" {
            *kills.entry(id).or_insert(0) += 1;
        }
    }
    kills
}

/// PR #260's journal: every pending restart spends ~400 ms on cleanup it
/// can't finish yet, and one tick walked all of them. Eight of them made
/// one tick 3.2 s long, and every command queued behind it waited that
/// long. A tick now only starts restarts: their kills run in tasks, and
/// no more than `RESTARTS_IN_FLIGHT_LIMIT` run at once.
#[tokio::test]
async fn one_tick_starts_restarts_without_waiting_for_their_runtime() {
    const PER_RESTART: std::time::Duration = std::time::Duration::from_millis(400);
    let count = restarts::RESTARTS_IN_FLIGHT_LIMIT as u32 + 4;
    let (mut agent, _grill, ids) = slow_pending_restarts(count, PER_RESTART).await;
    let started = std::time::Instant::now();
    agent.drive_pending_restarts().await;
    let took = started.elapsed();
    assert!(
        took < PER_RESTART,
        "one tick spent {took:?} starting {} pending restarts",
        ids.len()
    );
    assert_eq!(
        agent.restarts.len(),
        restarts::RESTARTS_IN_FLIGHT_LIMIT,
        "the tick didn't stop at the in-flight limit"
    );
    agent.settle_restart_steps().await;
}

/// A tick that stops at the limit must not keep retrying the same few
/// restarts: the next tick carries on where the last one stopped, so
/// every pending restart gets its turn.
#[tokio::test]
async fn bounded_ticks_rotate_through_every_pending_restart() {
    const PER_RESTART: std::time::Duration = std::time::Duration::from_millis(100);
    let count = restarts::RESTARTS_IN_FLIGHT_LIMIT as u32 + 4;
    let (mut agent, grill, ids) = slow_pending_restarts(count, PER_RESTART).await;
    for _ in 0..3 {
        agent.drive_pending_restarts().await;
        agent.settle_restart_steps().await;
    }
    let kills = kills_per_instance(&grill);
    for id in &ids {
        assert!(
            kills.get(id).copied().unwrap_or(0) >= 1,
            "{id} never got a restart attempt: {kills:?}"
        );
    }
    let most = kills.values().copied().max().unwrap();
    let least = kills.values().copied().min().unwrap();
    assert!(
        most - least <= 1,
        "restart attempts were not shared fairly: {kills:?}"
    );
}

/// Callers that ask for status again as soon as they get an answer, the
/// way `relish test` cases poll every node while they wait for a replica.
/// Together they keep the agent's command queue from ever emptying.
fn spawn_status_pollers(
    tx: &mpsc::Sender<AgentCommand>,
    count: usize,
    stop: &CancellationToken,
) -> Vec<tokio::task::JoinHandle<()>> {
    (0..count)
        .map(|_| {
            let tx = tx.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                while !stop.is_cancelled() {
                    let (response, reply) = oneshot::channel();
                    if tx.send(AgentCommand::Status { response }).await.is_err() {
                        break;
                    }
                    let _ = reply.await;
                }
            })
        })
        .collect()
}

async fn stop_agent_and_pollers(
    shutdown: CancellationToken,
    pollers_stop: CancellationToken,
    pollers: Vec<tokio::task::JoinHandle<()>>,
    mut task: tokio::task::JoinHandle<()>,
) {
    pollers_stop.cancel();
    shutdown.cancel();
    for poller in pollers {
        poller.abort();
    }
    if tokio::time::timeout(std::time::Duration::from_secs(10), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
}

/// V02 soak, candidate 11: after the leader's bun was killed, the soak's
/// apps piled onto one node and the bin-packer put every `relish test`
/// workload there too. The cases polled that node's status without pause,
/// so a command always waited. The loop served commands before deploy
/// steps, so the deploy worker's first step never ran: no instance
/// appeared for 300 s, pulse after pulse, while status kept answering.
#[tokio::test]
async fn deploy_steps_progress_while_status_queries_keep_the_queue_busy() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let resident = Config::parse("[app.resident]\nimage = 'resident:v1'\nreplicas = 2\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, resident).await);
    // Every status answer reads each instance's runtime.
    grill.set_pid_delay(Some(std::time::Duration::from_millis(20)));
    let task = tokio::spawn(async move { agent.run().await });
    let pollers_stop = CancellationToken::new();
    let pollers = spawn_status_pollers(&tx, 4, &pollers_stop);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (events, mut received) = mpsc::channel(64);
    tx.send(AgentCommand::Deploy {
        config: Config::parse("[app.fresh]\nimage = 'fresh:v1'\n").unwrap(),
        events,
    })
    .await
    .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = received.recv().await {
            match event {
                ApplyEvent::Complete { .. } => return Ok(()),
                ApplyEvent::Error { message } => return Err(message),
                _ => {}
            }
        }
        Err("the deploy's event stream closed without an outcome".to_string())
    })
    .await;
    stop_agent_and_pollers(shutdown, pollers_stop, pollers, task).await;

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(message)) => panic!("the deploy failed: {message}"),
        Err(_) => panic!(
            "the deploy made no progress in 10 s while status queries kept \
                 the command queue busy: its steps were starved"
        ),
    }
}

/// The same flood must not stop the health tick either. In the soak a
/// test workload sat in health-wait for the whole case deadline: its
/// probes run from the tick, and the tick only ran when no command
/// waited. Restarts, retirements and health checks all live there.
#[tokio::test]
async fn health_tick_runs_while_status_queries_keep_the_queue_busy() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let config = Config::parse("[app.web]\nimage = 'web:v1'\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    grill.set_pid_delay(Some(std::time::Duration::from_millis(20)));
    let task = tokio::spawn(async move { agent.run().await });
    let pollers_stop = CancellationToken::new();
    let pollers = spawn_status_pollers(&tx, 4, &pollers_stop);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // A replica dies. Only the health tick notices and restarts it.
    let starts = |grill: &MockGrill| {
        grill
            .calls()
            .iter()
            .filter(|(operation, _)| operation == "start")
            .count()
    };
    let starts_before = starts(&grill);
    let id = InstanceId("default__web-0".to_string());
    grill.set_state(&id, ContainerState::Stopped);
    grill.set_exit_code(&id, Some(1));
    let restarted = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while starts(&grill) == starts_before {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    stop_agent_and_pollers(shutdown, pollers_stop, pollers, task).await;

    assert!(
        restarted.is_ok(),
        "no health tick ran for 15 s while status queries kept the command \
             queue busy: the dead replica was never restarted"
    );
}

#[tokio::test]
async fn slow_deploy_does_not_block_the_command_loop() {
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = MockGrill::new();
    let grill_handle = grill.clone();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let handle = tokio::spawn(async move { agent.run().await });

    // Hold create() at a deterministic barrier, simulating a slow image
    // pull without making the test wait for wall-clock time.
    grill_handle.block_creates();
    let (ev_tx, _ev_rx) = mpsc::channel(64);
    tx.send(AgentCommand::Deploy {
        config: basic_config(),
        events: ev_tx,
    })
    .await
    .unwrap();

    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        grill_handle.wait_for_creates(1),
    )
    .await
    .expect("deploy never entered create");

    // A Status command must round-trip well before the 3s pull finishes.
    // If the loop were blocked inside create() this would not be answered
    // until the pull completed, blowing the 500ms timeout.
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let answered = tokio::time::timeout(std::time::Duration::from_millis(500), resp_rx).await;
    assert!(
        answered.is_ok(),
        "status was not answered while a slow deploy was in flight — the deploy blocked the loop"
    );

    grill_handle.release_creates(1);
    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn cancellation_waits_for_in_flight_create_before_releasing_ownership() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let task = tokio::spawn(async move { agent.run().await });
    grill.block_creates();
    let (events, mut stream) = mpsc::channel(64);
    tx.send(AgentCommand::Deploy {
        config: basic_config(),
        events,
    })
    .await
    .unwrap();
    let ApplyEvent::Accepted { operation_id } = stream.recv().await.unwrap() else {
        panic!("no ID")
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), grill.wait_for_creates(1))
        .await
        .unwrap();
    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::CancelDeploy {
        operation_id: operation_id.clone().into(),
        response,
    })
    .await
    .unwrap();
    let receipt = result.await.unwrap().unwrap();
    assert!(receipt.cancellation_requested_at.is_some());
    assert!(receipt.outcome.is_none());
    let conflict = send_deploy(&tx, basic_config()).await;
    assert!(conflict.iter().any(
        |event| matches!(event, ApplyEvent::Error { message } if message.contains(&operation_id))
    ));
    grill.release_creates(1);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while stream.recv().await.is_some() {}
    })
    .await
    .unwrap();
    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::DeployOperations { response })
        .await
        .unwrap();
    assert_eq!(
        result.await.unwrap().history[0].outcome,
        Some(crate::bun::deploy_operations::DeployOperationOutcome::Cancelled)
    );
    expect_complete(&send_deploy(&tx, basic_config()).await);
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn cancellation_interrupts_health_wait_and_holds_ownership_through_rollback() {
    for strategy in ["rolling", "blue-green"] {
        let port = spawn_health_responder(500).await;
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let task = tokio::spawn(async move { agent.run().await });
        expect_complete(&send_deploy(&tx, no_health_config(port)).await);
        let mut config = health_gated_config(port, strategy);
        config
            .app
            .get_mut("web")
            .unwrap()
            .deploy
            .as_mut()
            .unwrap()
            .health_timeout = Some("30s".into());
        let (events, mut stream) = mpsc::channel(64);
        tx.send(AgentCommand::Deploy { config, events })
            .await
            .unwrap();
        let ApplyEvent::Accepted { operation_id } = stream.recv().await.unwrap() else {
            panic!("no ID")
        };
        let canary = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !grill
                .calls()
                .iter()
                .any(|(call, id)| call == "start" && id == &canary)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        grill.block_kills();
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::CancelDeploy {
            operation_id: operation_id.clone().into(),
            response,
        })
        .await
        .unwrap();
        assert!(
            result
                .await
                .unwrap()
                .unwrap()
                .cancellation_requested_at
                .is_some()
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), grill.wait_for_kills(1))
            .await
            .expect("cancellation did not interrupt the 30-second health wait");
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::DeployOperations { response })
            .await
            .unwrap();
        let snapshot = result.await.unwrap();
        grill.release_kills(1);
        assert!(
            snapshot
                .active_deploys
                .iter()
                .any(|op| op.id.as_str() == operation_id)
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while stream.recv().await.is_some() {}
        })
        .await
        .unwrap();
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::DeployOperations { response })
            .await
            .unwrap();
        assert_eq!(
            result.await.unwrap().history[0].outcome,
            Some(crate::bun::deploy_operations::DeployOperationOutcome::Cancelled)
        );
        expect_complete(&send_deploy(&tx, no_health_config(port)).await);
        shutdown.cancel();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn failed_deploy_keeps_target_ownership_until_rollback_finishes() {
    for strategy in ["rolling", "blue-green"] {
        let port = spawn_health_responder(500).await;
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let task = tokio::spawn(async move { agent.run().await });
        expect_complete(&send_deploy(&tx, no_health_config(port)).await);
        grill.block_kills();
        let (events, mut event_rx) = mpsc::channel(64);
        tx.send(AgentCommand::Deploy {
            config: health_gated_config(port, strategy),
            events,
        })
        .await
        .unwrap();
        let operation_id = match event_rx.recv().await.unwrap() {
            ApplyEvent::Accepted { operation_id } => operation_id,
            event => panic!("expected acceptance, got {event:?}"),
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while let Some(event) = event_rx.recv().await {
                if matches!(event, ApplyEvent::Error { .. }) {
                    break;
                }
            }
            grill.wait_for_kills(1).await;
        })
        .await
        .expect("failed replacement did not enter cleanup");
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::DeployOperations { response })
            .await
            .unwrap();
        let snapshot = result.await.unwrap();
        // Release the fixture even when the assertion below fails.
        grill.release_kills(1);
        assert!(
            snapshot
                .active_deploys
                .iter()
                .any(|op| op.id.as_str() == operation_id),
            "{strategy} released target ownership while rollback still owned its runtime mutation"
        );
        assert!(
            !snapshot
                .history
                .iter()
                .any(|op| op.id.as_str() == operation_id)
        );
        while event_rx.recv().await.is_some() {}
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::DeployOperations { response })
            .await
            .unwrap();
        let snapshot = result.await.unwrap();
        assert_eq!(
            snapshot
                .history
                .iter()
                .find(|op| op.id.as_str() == operation_id)
                .unwrap()
                .outcome,
            Some(crate::bun::deploy_operations::DeployOperationOutcome::Failed)
        );
        expect_complete(&send_deploy(&tx, no_health_config(port)).await);
        shutdown.cancel();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn stalled_event_consumer_cannot_pin_deployment_ownership() {
    let (mut agent, tx, shutdown, _) = test_agent_with_grill();
    let task = tokio::spawn(async move { agent.run().await });
    // Acceptance fills this queue. Keep its receiver alive without reading.
    let (events, _event_rx) = mpsc::channel(1);
    tx.send(AgentCommand::Deploy {
        config: basic_config(),
        events,
    })
    .await
    .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::DeployOperations { response })
                .await
                .unwrap();
            let snapshot = result.await.unwrap();
            if let Some(operation) = snapshot.history.first() {
                break operation.outcome;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    shutdown.cancel();
    task.await.unwrap();
    assert_eq!(
        outcome.expect("client backpressure prevented terminal history"),
        Some(crate::bun::deploy_operations::DeployOperationOutcome::Completed)
    );
}

#[tokio::test]
async fn deploy_operations_track_live_phase_conflicts_and_disconnected_clients() {
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = MockGrill::new();
    let grill_handle = grill.clone();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let handle = tokio::spawn(async move { agent.run().await });

    grill_handle.block_creates();
    let (events, mut event_rx) = mpsc::channel(64);
    tx.send(AgentCommand::Deploy {
        config: basic_config(),
        events,
    })
    .await
    .unwrap();
    let operation_id = match event_rx.recv().await.unwrap() {
        ApplyEvent::Accepted { operation_id } => operation_id,
        event => panic!("first event was not acceptance: {event:?}"),
    };
    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        grill_handle.wait_for_creates(1),
    )
    .await
    .expect("deploy never entered create");

    let (snapshot_tx, snapshot_rx) = oneshot::channel();
    tx.send(AgentCommand::DeployOperations {
        response: snapshot_tx,
    })
    .await
    .unwrap();
    let snapshot = snapshot_rx.await.unwrap();
    assert_eq!(snapshot.active_deploys.len(), 1);
    let active = &snapshot.active_deploys[0];
    assert_eq!(active.id.as_str(), operation_id);
    assert_eq!(
        active.phase,
        crate::bun::deploy_operations::DeployOperationPhase::DeployingApps
    );
    assert_eq!(
        active
            .current_target
            .as_ref()
            .map(|target| target.name.as_str()),
        Some("web")
    );

    let (conflict_events, mut conflict_rx) = mpsc::channel(8);
    tx.send(AgentCommand::Deploy {
        config: basic_config(),
        events: conflict_events,
    })
    .await
    .unwrap();
    match conflict_rx.recv().await.unwrap() {
        ApplyEvent::Error { message } => {
            assert!(message.contains(&operation_id));
            assert!(message.contains("already being changed"));
        }
        event => panic!("overlapping deploy was not refused: {event:?}"),
    }

    // Losing the SSE consumer must not lose the operation outcome.
    drop(event_rx);
    grill_handle.release_creates(1);
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let (snapshot_tx, snapshot_rx) = oneshot::channel();
            tx.send(AgentCommand::DeployOperations {
                response: snapshot_tx,
            })
            .await
            .unwrap();
            let snapshot = snapshot_rx.await.unwrap();
            if let Some(operation) = snapshot
                .history
                .into_iter()
                .find(|operation| operation.id.as_str() == operation_id)
            {
                break operation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("deploy never reached terminal operation history");
    assert_eq!(
        terminal.outcome,
        Some(crate::bun::deploy_operations::DeployOperationOutcome::Completed)
    );
    assert!(terminal.finished_at.is_some());
    assert!(terminal.finished_at.unwrap() >= terminal.started_at);

    shutdown.cancel();
    let _ = handle.await;
}

/// DEP4/codex-M3: two concurrent deploys interleave rather than
/// serialise. With both apps' create() sleeping, the second deploy's
/// first grill call happens before the first deploy's create returns —
/// impossible if deploys ran one-after-another on the command loop.
#[tokio::test]
async fn concurrent_deploys_interleave() {
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = MockGrill::new();
    let grill_handle = grill.clone();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
    let handle = tokio::spawn(async move { agent.run().await });

    grill_handle.block_creates();

    let config_a = Config::parse("[app.alpha]\nimage = \"a:v1\"\n").unwrap();
    let config_b = Config::parse("[app.beta]\nimage = \"b:v1\"\n").unwrap();

    let (ev_a, _ra) = mpsc::channel(64);
    let (ev_b, _rb) = mpsc::channel(64);
    tx.send(AgentCommand::Deploy {
        config: config_a,
        events: ev_a,
    })
    .await
    .unwrap();
    tx.send(AgentCommand::Deploy {
        config: config_b,
        events: ev_b,
    })
    .await
    .unwrap();

    // If deploys were serial, the first blocked create would prevent the
    // second from reaching this barrier.
    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        grill_handle.wait_for_creates(2),
    )
    .await
    .expect("both deploys did not enter create concurrently");

    let created: std::collections::HashSet<String> = grill_handle
        .calls()
        .into_iter()
        .filter(|(op, _)| op == "create")
        .map(|(_, id)| id.0)
        .collect();
    assert!(
        created.contains("default__alpha-0") && created.contains("default__beta-0"),
        "both deploys should be in flight together, got: {created:?}"
    );

    grill_handle.release_creates(2);
    shutdown.cancel();
    let _ = handle.await;
}

#[test]
fn probe_host_prefers_container_ip() {
    assert_eq!(probe_host(None), "127.0.0.1");
    assert_eq!(
        probe_host(Some(std::net::Ipv4Addr::new(10, 0, 2, 2))),
        "10.0.2.2"
    );
}

/// Let `agent` run host workloads: `script` apps through `/bin/sh`, and
/// `exec` apps for each of `binaries`, unisolated. The process runtime runs
/// nothing else (#914), so every real-process test here needs it.
fn allow_host_workloads<G: Grill + Clone + 'static>(
    agent: &mut BunAgent<G>,
    binaries: &[&std::path::Path],
) {
    let mut allowed_binaries = vec![std::path::PathBuf::from("/bin/sh")];
    allowed_binaries.extend(binaries.iter().map(|binary| binary.to_path_buf()));
    agent
        .supervisor
        .set_process_config(crate::config::process_workloads::ProcessWorkloadsConfig {
            allowed_binaries,
            mount_isolation: false,
            ..Default::default()
        });
}

#[tokio::test]
async fn container_logs_forwarded_to_log_sink() {
    // Use a real ProcessGrill so follow_logs actually streams output.
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = crate::grill::process::ProcessGrill::new();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
    allow_host_workloads(&mut agent, &[]);

    let (log_tx, mut log_rx) = mpsc::channel(64);
    agent.set_log_sink(log_tx, Default::default());
    let handle = tokio::spawn(async move { agent.run().await });

    let config = Config::parse("[app.printer]\nscript = \"echo hello-logs\"\n").unwrap();
    let _ = send_deploy(&tx, config).await;

    // The per-instance forwarder should stream the echoed line into the sink.
    let record = tokio::time::timeout(std::time::Duration::from_secs(5), log_rx.recv())
        .await
        .expect("timed out waiting for a forwarded log record")
        .expect("log channel closed");
    assert_eq!(record.app, "printer");
    assert_eq!(record.line, "hello-logs");

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test]
async fn crashed_app_without_health_check_is_restarted() {
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = crate::grill::process::ProcessGrill::new();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
    allow_host_workloads(&mut agent, &[]);
    let handle = tokio::spawn(async move { agent.run().await });

    // An app with no health check whose process exits immediately. Nothing
    // probes it, so only crash detection can notice and restart it.
    let config = Config::parse("[app.crasher]\nscript = \"true\"\n").unwrap();
    let _ = send_deploy(&tx, config).await;

    let crasher = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            tx.send(AgentCommand::Status { response: resp_tx })
                .await
                .unwrap();
            if let Some(crasher) = resp_rx
                .await
                .unwrap()
                .into_iter()
                .find(|status| status.app_name == "crasher" && status.restart_count > 0)
            {
                return crasher;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("crashed app was not restarted");
    assert!(
        crasher.restart_count > 0,
        "crashed app was never restarted (state: {})",
        crasher.state
    );

    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), handle).await;
}

#[tokio::test]
async fn redeploy_registers_backends_health_and_port() {
    // The rolling gate (M5) probes the app's health endpoint before the
    // redeploy may complete, so give it one that answers 200.
    let port = spawn_health_responder(200).await;
    let (_tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = crate::grill::process::ProcessGrill::new();
    let port_allocator = PortAllocator::new(30000, 31000);
    let volumes = tempfile::tempdir().unwrap();
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);
    agent.set_volumes_dir(volumes.path().to_path_buf());
    allow_host_workloads(&mut agent, &[]);

    let config = Config::parse(&format!(
            "[app.web]\nscript = \"exec sleep 60\"\nport = {port}\n\n[app.web.health]\npath = \"/healthz\"\n\n[app.web.deploy]\nhealth_timeout = \"5s\"\n",
        ))
        .unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);

    // Fresh deploy, then redeploy (existing instances → rolling path).
    agent.deploy(config.clone(), &ev_tx).await;
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    // The service must have backends after the redeploy (was left empty).
    let entry = agent
        .service_map
        .resolve(&crate::onion::service_id::ServiceId::new("default", "web"))
        .expect("web missing from service map");
    assert!(
        !entry.backends.is_empty(),
        "redeploy left the service with zero backends"
    );

    // A redeployed instance keeps its port and health-check registration.
    let inst = agent
        .supervisor
        .list_instances()
        .into_iter()
        .find(|i| i.app_name == "web")
        .expect("no web instance after redeploy");
    assert!(inst.host_port.is_some(), "redeploy dropped the host port");
    assert!(
        inst.health_config.is_some(),
        "redeploy dropped the health check"
    );

    agent.stop_app("web", "default").await.unwrap();
}

async fn restart_preserves_uncertain_cleanup(inject: fn(&MockGrill)) {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    let directory = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(directory.path().join("volumes"));
    let records = directory.path().join("instances");
    agent.set_records_dir(records.clone());
    let config = Config::parse("[app.restart]\nimage = \"mock:image\"\nport = 8080\n").unwrap();
    let (events, _received) = mpsc::channel(256);
    agent.deploy(config, &events).await;
    assert_eq!(
        agent.supervisor.list_instances()[0].state,
        ContainerState::Running
    );
    let id = agent.supervisor.list_instances()[0].id.clone();
    let port = agent.supervisor.get_instance(&id).unwrap().host_port;
    assert!(port.is_some());
    std::fs::create_dir_all(&records).unwrap();
    let record = crate::grill::records::record_path(&records, &id.0);
    std::fs::write(&record, "retained ownership").unwrap();
    agent.supervisor.get_instance_mut(&id).unwrap().state = ContainerState::Unhealthy;
    assert!(
        agent
            .supervisor
            .maybe_restart(&id, Instant::now())
            .await
            .unwrap()
    );
    let before = grill.calls().len();
    inject(&grill);
    tokio::time::timeout(
        std::time::Duration::from_secs(6),
        agent.drive_pending_restarts_to_completion(),
    )
    .await
    .expect("restart cleanup stalled the agent");
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Pending
    );
    assert_eq!(agent.supervisor.get_instance(&id).unwrap().host_port, port);
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        "retained ownership"
    );
    assert!(
        !grill.calls()[before..]
            .iter()
            .any(|(operation, _)| operation == "create" || operation == "start")
    );

    grill.set_fail_kill(false);
    grill.set_ignore_kill(false);
    grill.set_fail_state(false);
    grill.release_kills(1);
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running
    );
    assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 1);
    agent.stop_app("restart", "default").await.unwrap();
}

#[tokio::test]
async fn restart_retains_owner_after_failed_kill() {
    restart_preserves_uncertain_cleanup(|grill| grill.set_fail_kill(true)).await;
}

#[tokio::test]
async fn restart_retains_owner_after_unconfirmed_kill() {
    restart_preserves_uncertain_cleanup(|grill| grill.set_ignore_kill(true)).await;
}

#[tokio::test]
async fn restart_retains_owner_after_failed_observation() {
    restart_preserves_uncertain_cleanup(|grill| grill.set_fail_state(true)).await;
}

#[tokio::test]
async fn restart_retains_owner_after_stalled_kill() {
    restart_preserves_uncertain_cleanup(MockGrill::block_kills).await;
}

async fn failed_restart_fixture() -> (TestAgent, MockGrill, InstanceId, tempfile::TempDir) {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let directory = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(directory.path().join("volumes"));
    let config = Config::parse("[app.retry]\nimage = \"mock:image\"\nport = 8080\n").unwrap();
    let (events, _received) = mpsc::channel(256);
    agent.deploy(config, &events).await;
    let id = agent.supervisor.list_instances()[0].id.clone();
    agent.supervisor.get_instance_mut(&id).unwrap().state = ContainerState::Unhealthy;
    assert!(
        agent
            .supervisor
            .maybe_restart(&id, Instant::now())
            .await
            .unwrap()
    );
    (agent, grill, id, directory)
}

async fn failed_restart_recovers(inject: fn(&MockGrill)) {
    let (mut agent, grill, id, _directory) = failed_restart_fixture().await;
    let port = agent.supervisor.get_instance(&id).unwrap().host_port;
    inject(&grill);
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Stopping
    );
    assert_eq!(agent.supervisor.get_instance(&id).unwrap().host_port, port);
    let creates = grill
        .calls()
        .iter()
        .filter(|(op, _)| op == "create")
        .count();
    grill.set_fail_kill(true);
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Stopping
    );
    assert_eq!(
        grill
            .calls()
            .iter()
            .filter(|(op, _)| op == "create")
            .count(),
        creates
    );
    grill.set_fail_kill(false);
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Stopped
    );
    assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 1);
    assert_eq!(
        grill
            .calls()
            .iter()
            .filter(|(op, _)| op == "create")
            .count(),
        creates
    );
    grill.set_fail_create(false);
    grill.set_fail_start(false);
    agent.supervisor.get_instance_mut(&id).unwrap().last_restart =
        Some(Instant::now() - std::time::Duration::from_secs(600));
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running
    );
    assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 2);
    agent.stop_app("retry", "default").await.unwrap();
}

#[tokio::test]
async fn restart_create_failure_recovers_after_cleanup_and_backoff() {
    failed_restart_recovers(|grill| grill.set_fail_create(true)).await;
}

#[tokio::test]
async fn restart_start_failure_recovers_after_cleanup_and_backoff() {
    failed_restart_recovers(|grill| grill.set_fail_start(true)).await;
}

#[tokio::test]
async fn explicit_stop_cancels_failed_restart_recovery() {
    let (mut agent, grill, id, _directory) = failed_restart_fixture().await;
    grill.set_fail_start(true);
    agent.drive_pending_restarts_to_completion().await;
    agent.stop_app("retry", "default").await.unwrap();
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Stopped
    );
    let calls = grill.calls().len();
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(grill.calls().len(), calls);
}

#[tokio::test]
async fn explicit_stop_cancels_pending_restart_before_creation() {
    let (mut agent, grill, id, _directory) = failed_restart_fixture().await;
    agent.stop_app("retry", "default").await.unwrap();
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Stopped
    );
    let calls = grill.calls().len();
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(grill.calls().len(), calls);
}

/// A real process crashing during backoff stays eligible for a later tick.
/// Once its executable is gone, the restart fails at once with the reason
/// instead of retrying until the binary returns (#905). Transient restart
/// failures still retry: see `failed_restart_recovers`.
#[tokio::test]
async fn real_process_restart_stays_eligible_and_fails_at_once_without_its_executable() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("worker");
    std::fs::write(&program, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (_tx, rx) = mpsc::channel(32);
    let grill = crate::grill::process::ProcessGrill::new();
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(30000, 31000),
        rx,
        CancellationToken::new(),
    );
    agent.set_volumes_dir(directory.path().join("volumes"));
    allow_host_workloads(&mut agent, &[&program]);
    let config = Config::parse(&format!(
        "[app.retry]\nexec = {:?}\n",
        program.to_str().unwrap()
    ))
    .unwrap();
    let (events, _received) = mpsc::channel(256);
    agent.deploy(config, &events).await;
    let id = agent.supervisor.list_instances()[0].id.clone();
    grill.kill(&id).await.unwrap();
    agent.check_apps().await;
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Running);
    assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 1);
    // A second crash during backoff must stay eligible for a later tick.
    grill.kill(&id).await.unwrap();
    agent.check_apps().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Stopped
    );
    agent.supervisor.get_instance_mut(&id).unwrap().last_restart =
        Some(Instant::now() - std::time::Duration::from_secs(600));
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Running);
    assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 2);

    // Now the executable goes, and the process with it.
    std::fs::remove_file(&program).unwrap();
    grill.kill(&id).await.unwrap();
    agent.check_apps().await;
    agent.supervisor.get_instance_mut(&id).unwrap().last_restart =
        Some(Instant::now() - std::time::Duration::from_secs(600));
    agent.drive_pending_restarts_to_completion().await;
    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Failed);
    assert_eq!(
        instance.launch_failure,
        Some(format!("binary not found: {}", program.display()))
    );
    assert_eq!(instance.restart_count, 3);
    agent.stop_app("retry", "default").await.unwrap();
}

#[tokio::test]
async fn redeployed_instance_restarts_after_a_crash() {
    let (_tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = crate::grill::process::ProcessGrill::new();
    let port_allocator = PortAllocator::new(30000, 31000);
    let volumes = tempfile::tempdir().unwrap();
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);
    agent.set_volumes_dir(volumes.path().to_path_buf());
    allow_host_workloads(&mut agent, &[]);

    let config = Config::parse("[app.web]\nscript = \"exec sleep 60\"\n").unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);

    // Fresh deploy, then redeploy (existing instances → rolling path).
    agent.deploy(config.clone(), &ev_tx).await;
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let id = agent
        .supervisor
        .list_instances()
        .into_iter()
        .find(|i| i.app_name == "web")
        .expect("no web instance after redeploy")
        .id
        .clone();

    // The redeploy must have stored the OCI spec — without it the
    // crash-restart driver silently skips the instance (it filters on
    // `oci_spec.is_some()`), wedging it in Pending forever.
    assert!(
        agent
            .supervisor
            .get_instance(&id)
            .unwrap()
            .oci_spec
            .is_some(),
        "redeploy left the instance with no OCI spec, so it can never restart"
    );

    // Simulate a crash and drive one restart cycle.
    let now = std::time::Instant::now();
    agent.supervisor.get_instance_mut(&id).unwrap().state =
        crate::grill::state::ContainerState::Stopped;
    let _ = agent.supervisor.maybe_restart(&id, now).await;
    agent.drive_pending_restarts_to_completion().await;

    let state = agent.supervisor.get_instance(&id).unwrap().state;
    assert_ne!(
        state,
        crate::grill::state::ContainerState::Pending,
        "redeployed instance stayed wedged in Pending instead of re-creating"
    );

    agent.stop_app("web", "default").await.unwrap();
}

fn cluster_publication_fixture() -> (
    crate::onion::catalog::EndpointCatalog,
    Vec<crate::cluster::orchestrate::IngressAssignment>,
) {
    let config: crate::config::app::IngressSpec =
        toml::from_str("host = \"remote.local\"\ntls = \"disabled\"").unwrap();
    let catalog = crate::onion::catalog::EndpointCatalog::rebuild([(
        crate::onion::service_id::ServiceId::new("default", "remote"),
        8080,
        vec![crate::onion::catalog::CatalogBackend {
            execution: None,
            node_id: "other-node".into(),
            node_ip: "192.168.1.2".parse().unwrap(),
            host_port: 30001,
            healthy: true,
        }],
    )])
    .unwrap();
    (
        catalog,
        vec![crate::cluster::orchestrate::IngressAssignment {
            namespace: "default".into(),
            name: "remote".into(),
            config,
        }],
    )
}

#[tokio::test]
async fn cluster_consumer_refuses_stale_and_rewritten_generations_without_changing_views() {
    let (mut agent, _, _) = test_agent();
    let (original, ingress) = cluster_publication_fixture();
    agent
        .publish_cluster_catalogue(4, original.clone(), ingress.clone())
        .await
        .unwrap();
    let mut changed = original.clone();
    changed
        .services
        .get_mut("default__remote")
        .unwrap()
        .backends[0]
        .host_port = 30002;
    for (generation, catalog) in [
        (3, original.clone()),
        (3, changed.clone()),
        (4, changed.clone()),
    ] {
        assert!(
            agent
                .publish_cluster_catalogue(generation, catalog, ingress.clone())
                .await
                .is_err(),
            "accepted stale or rewritten generation {generation}"
        );
        assert_eq!(agent.cluster_catalog, original);
        assert_eq!(
            agent
                .routing_table
                .read()
                .await
                .lookup("remote.local", "/")
                .unwrap()
                .backends[0]
                .addr
                .port(),
            30001
        );
        assert_eq!(
            agent.service_map_tx.borrow().resolve_all()[0].backends[0].host_port,
            30001
        );
    }
    agent
        .publish_cluster_catalogue(5, changed.clone(), ingress)
        .await
        .unwrap();
    assert_eq!(agent.cluster_catalog, changed);
}

#[tokio::test]
async fn cluster_consumer_advances_identical_generations_and_does_not_consume_refused_updates() {
    let (mut agent, _, _) = test_agent();
    let (original, ingress) = cluster_publication_fixture();
    agent
        .publish_cluster_catalogue(1, original.clone(), ingress.clone())
        .await
        .unwrap();
    agent
        .publish_cluster_catalogue(4, original.clone(), ingress.clone())
        .await
        .unwrap();
    assert!(
        agent
            .publish_cluster_catalogue(3, original.clone(), ingress.clone())
            .await
            .is_err()
    );
    let mut changed = original.clone();
    changed
        .services
        .get_mut("default__remote")
        .unwrap()
        .backends[0]
        .host_port = 30002;
    agent
        .publish_cluster_catalogue(5, changed.clone(), ingress.clone())
        .await
        .unwrap();
    agent
        .publish_cluster_catalogue(5, changed.clone(), vec![])
        .await
        .unwrap();
    assert!(
        agent
            .routing_table
            .read()
            .await
            .lookup("remote.local", "/")
            .is_none()
    );
    agent
        .publish_cluster_catalogue(5, changed, ingress)
        .await
        .unwrap();
}

/// `base` plus a second remote service: `remote` keeps its valid ingress
/// route and `broken` gets one naming a TLS mode no node supports.
fn with_one_bad_route(
    base: &crate::onion::catalog::EndpointCatalog,
) -> (
    crate::onion::catalog::EndpointCatalog,
    Vec<crate::cluster::orchestrate::IngressAssignment>,
) {
    let (_, mut ingress) = cluster_publication_fixture();
    let remote = base.services["default__remote"].clone();
    let catalog = base
        .reconcile([
            (
                crate::onion::service_id::ServiceId::new("default", "remote"),
                remote.port,
                remote.backends,
            ),
            (
                crate::onion::service_id::ServiceId::new("default", "broken"),
                9090,
                vec![crate::onion::catalog::CatalogBackend {
                    execution: None,
                    node_id: "other-node".into(),
                    node_ip: "192.168.1.3".parse().unwrap(),
                    host_port: 30010,
                    healthy: true,
                }],
            ),
        ])
        .unwrap();
    ingress.push(crate::cluster::orchestrate::IngressAssignment {
        namespace: "default".into(),
        name: "broken".into(),
        config: toml::from_str("host = \"broken.local\"\ntls = \"acme\"").unwrap(),
    });
    (catalog, ingress)
}

/// Assert that both services resolve, `remote.local` routes, and the bad
/// route was skipped rather than installed.
async fn assert_only_the_bad_route_is_skipped(agent: &BunAgent<MockGrill>) {
    let view = agent.service_map_tx.borrow().clone();
    for name in ["remote", "broken"] {
        assert!(
            view.resolve(&crate::onion::service_id::ServiceId::new("default", name))
                .is_some(),
            "{name} must still resolve"
        );
    }
    let table = agent.routing_table.read().await;
    assert!(table.lookup("remote.local", "/").is_some());
    assert!(table.lookup("broken.local", "/").is_none());
}

/// #700: one app's invalid ingress route made every node refuse every new
/// catalogue, freezing discovery and ingress for the whole cluster. The bad
/// route is skipped and logged; everything else publishes.
#[tokio::test]
async fn one_invalid_cluster_route_does_not_block_the_catalogue() {
    let (mut agent, _, _) = test_agent();
    let (catalog, ingress) = with_one_bad_route(&cluster_publication_fixture().0);
    agent
        .publish_cluster_catalogue(1, catalog.clone(), ingress)
        .await
        .unwrap();
    assert_eq!(agent.cluster_catalog, catalog);
    assert_eq!(agent.cluster_catalog_generation, Some(1));
    assert_only_the_bad_route_is_skipped(&agent).await;
}

/// #700, the discovery-owning consumer path: the same bad route must not
/// refuse the publication either.
#[tokio::test]
async fn one_invalid_consumer_route_does_not_block_the_view() {
    let (mut agent, _root, base) = clustered_allocation_fixture().await;
    let (committed, ingress) = with_one_bad_route(&base);
    agent
        .synchronise_consumer(2, committed, ingress, vec![])
        .await
        .unwrap();
    assert_eq!(agent.cluster_catalog_generation, Some(2));
    assert_only_the_bad_route_is_skipped(&agent).await;
}

#[tokio::test]
async fn cluster_consumer_zero_generation_requires_an_empty_catalogue() {
    let (mut agent, _, _) = test_agent();
    let (catalog, ingress) = cluster_publication_fixture();
    assert!(
        agent
            .publish_cluster_catalogue(0, catalog, ingress)
            .await
            .is_err()
    );
    assert!(agent.cluster_catalog.is_empty());
    agent
        .publish_cluster_catalogue(0, Default::default(), vec![])
        .await
        .unwrap();
}

#[tokio::test]
async fn cluster_consumer_refuses_collisions_in_the_effective_local_and_remote_view() {
    let (mut agent, _, _) = test_agent();
    let local = crate::onion::service_id::ServiceId::new("default", "local");
    let vip = agent.service_map.register(&local, 8080, None).unwrap();
    let (mut catalog, ingress) = cluster_publication_fixture();
    catalog.services.get_mut("default__remote").unwrap().vip = vip;
    catalog.validate_allocations().unwrap();
    assert!(
        agent
            .publish_cluster_catalogue(1, catalog, ingress)
            .await
            .is_err()
    );
    assert!(agent.cluster_catalog.is_empty());
    assert!(agent.service_map_tx.borrow().resolve_all().is_empty());
    assert!(
        agent
            .routing_table
            .read()
            .await
            .lookup("remote.local", "/")
            .is_none()
    );
}

#[tokio::test]
async fn cluster_publication_refusal_preserves_the_confirmed_catalogue_dns_and_ingress() {
    let (mut agent, _, _) = test_agent();
    let (original, ingress) = cluster_publication_fixture();
    let (response, reply) = oneshot::channel();
    agent
        .handle_command(AgentCommand::SyncClusterCatalog {
            generation: 1,
            response,
            catalog: Box::new(original.clone()),
            ingress: ingress.clone(),
        })
        .await;
    reply.await.unwrap().unwrap();
    let mut changed = original.clone();
    changed
        .services
        .get_mut("default__remote")
        .unwrap()
        .backends[0]
        .host_port = 30002;
    // A rewritten generation 1 conflicts with the confirmed one. (An invalid
    // route no longer refuses a generation; it's skipped, #700.)
    let (response, reply) = oneshot::channel();
    agent
        .handle_command(AgentCommand::SyncClusterCatalog {
            generation: 1,
            response,
            catalog: Box::new(changed.clone()),
            ingress: ingress.clone(),
        })
        .await;
    assert!(reply.await.unwrap().is_err());
    assert_eq!(
        agent.cluster_catalog, original,
        "a refused publication changed the installed catalogue"
    );
    assert_eq!(
        agent
            .service_map_tx
            .borrow()
            .resolve(&crate::onion::service_id::ServiceId::new(
                "default", "remote"
            ))
            .unwrap()
            .backends[0]
            .host_port,
        30001
    );
    let table = agent.routing_table.read().await;
    let route = table
        .lookup("remote.local", "/")
        .expect("last confirmed ingress was lost");
    assert_eq!(route.backends[0].addr.port(), 30001);
    assert!(route.rate_limit.is_none());
    drop(table);
    let (response, reply) = oneshot::channel();
    agent
        .handle_command(AgentCommand::SyncClusterCatalog {
            generation: 2,
            catalog: Box::new(changed.clone()),
            ingress,
            response,
        })
        .await;
    reply.await.unwrap().unwrap();
    assert_eq!(agent.cluster_catalog, changed);
    assert_eq!(
        agent
            .service_map_tx
            .borrow()
            .resolve(&crate::onion::service_id::ServiceId::new(
                "default", "remote"
            ))
            .unwrap()
            .backends[0]
            .host_port,
        30002
    );
    assert_eq!(
        agent
            .routing_table
            .read()
            .await
            .lookup("remote.local", "/")
            .unwrap()
            .backends[0]
            .addr
            .port(),
        30002
    );
}

#[tokio::test]
async fn cluster_publication_refuses_invalid_allocations_before_any_view_changes() {
    let (mut agent, _, _) = test_agent();
    let (mut invalid, ingress) = cluster_publication_fixture();
    invalid.services.get_mut("default__remote").unwrap().vip.0 = std::net::Ipv4Addr::LOCALHOST;
    let (response, reply) = oneshot::channel();
    agent
        .handle_command(AgentCommand::SyncClusterCatalog {
            generation: 1,
            response,
            catalog: Box::new(invalid),
            ingress,
        })
        .await;
    assert!(reply.await.unwrap().is_err());
    assert!(agent.cluster_catalog.is_empty());
    assert!(agent.service_map_tx.borrow().resolve_all().is_empty());
    assert!(
        agent
            .routing_table
            .read()
            .await
            .lookup("remote.local", "/")
            .is_none()
    );
}

#[tokio::test]
async fn ingress_routes_are_installed_without_a_local_replica_and_removed_on_update() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let config = Config::parse(
        r#"[app.remote]
image = "example:v1"
port = 8080
[app.remote.ingress]
host = "remote.local"
"#,
    )
    .unwrap();
    let catalog = crate::onion::catalog::EndpointCatalog::rebuild([(
        crate::onion::service_id::ServiceId::new("default", "remote"),
        8080,
        vec![crate::onion::catalog::CatalogBackend {
            execution: None,
            node_id: "other-node".into(),
            node_ip: "192.168.1.2".parse().unwrap(),
            host_port: 30001,
            healthy: true,
        }],
    )])
    .unwrap();
    let (response, reply) = oneshot::channel();
    agent
        .handle_command(AgentCommand::SyncClusterCatalog {
            generation: 1,
            response,
            catalog: Box::new(catalog.clone()),
            ingress: vec![crate::cluster::orchestrate::IngressAssignment {
                name: "remote".into(),
                namespace: "default".into(),
                config: config.app["remote"].ingress.clone().unwrap(),
            }],
        })
        .await;
    reply.await.unwrap().unwrap();
    assert!(
        agent
            .routing_table
            .read()
            .await
            .lookup("remote.local", "/")
            .is_some()
    );
    assert!(agent.supervisor.list_instances().is_empty());
    let (response, reply) = oneshot::channel();
    agent
        .handle_command(AgentCommand::SyncClusterCatalog {
            generation: 1,
            response,
            catalog: Box::new(catalog),
            ingress: Vec::new(),
        })
        .await;
    reply.await.unwrap().unwrap();
    assert!(
        agent
            .routing_table
            .read()
            .await
            .lookup("remote.local", "/")
            .is_none()
    );
}

#[tokio::test]
async fn local_probe_failure_does_not_restart_a_healthy_workload() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    let (events, _receiver) = mpsc::channel(64);
    agent.deploy(config_with_health(), &events).await;
    let instance = agent.supervisor.list_instances()[0];
    let id = instance.id.clone();
    let created_at = instance.created_at;
    for _ in 0..3 {
        agent
            .complete_health_probe(
                id.clone(),
                created_at,
                Ok(super::super::health::HealthStatus::Healthy),
            )
            .await;
    }
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running
    );
    for _ in 0..3 {
        agent
            .complete_health_probe(
                id.clone(),
                created_at,
                Err(super::super::probe::ProbeError::Client(
                    "local TLS setup failed".into(),
                )),
            )
            .await;
    }
    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Running);
    assert_eq!(instance.health_counters.consecutive_unhealthy, 0);
    assert_eq!(instance.restart_count, 0);
}

#[tokio::test]
async fn deploy_records_the_grills_container_ip_on_instance_and_backend() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let ip = std::net::Ipv4Addr::new(10, 0, 2, 5);
    grill.set_container_ip(ip);

    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(basic_config(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let inst = agent
        .supervisor
        .list_instances()
        .into_iter()
        .find(|i| i.app_name == "web")
        .expect("no web instance after deploy");
    assert_eq!(
        inst.container_ip,
        Some(ip),
        "the runtime's container IP was not recorded on the instance"
    );

    let entry = agent
        .service_map
        .resolve(&crate::onion::service_id::ServiceId::new("default", "web"))
        .expect("web not in map");
    assert!(
        entry.backends.iter().any(|b| b.node_ip == ip),
        "backend registered with loopback instead of the container IP"
    );
    assert!(
        entry
            .backends
            .iter()
            .all(|backend| backend.host_port == 8080),
        "a container IP must use its declared port, not the allocated host port"
    );
}

#[tokio::test]
async fn scrape_targets_name_each_running_instance_of_apps_with_metrics() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_container_ip(std::net::Ipv4Addr::new(10, 0, 2, 5));
    let config = Config::parse(
        r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
            metrics = { port = 9797 }

            [app.quiet]
            image = "myapp:v1"
            port = 8081
            "#,
    )
    .unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let (response, receiver) = oneshot::channel();
    agent
        .handle_command(AgentCommand::ScrapeTargets { response })
        .await;
    let targets = receiver.await.unwrap();
    assert_eq!(
        targets,
        vec![crate::mayo::scrape::AppScrapeTarget {
            app: "web".to_string(),
            namespace: "default".to_string(),
            instance: "default__web-0".to_string(),
            url: "http://10.0.2.5:9797/metrics".to_string(),
        }]
    );
}

/// F07 part 2: `relish logs -f --instance` follows one replica, not the app.
#[tokio::test]
async fn follow_logs_keeps_only_the_asked_instance() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, web_with_replicas(2)).await);
    grill.set_logs(&InstanceId("default__web-0".into()), "from zero\n");
    grill.set_logs(&InstanceId("default__web-1".into()), "from one\n");

    let (lines_tx, mut lines_rx) = mpsc::channel(16);
    agent.spawn_logs_follow(
        "web",
        "default",
        Some(10),
        Some("default__web-1".into()),
        None,
        lines_tx,
    );
    let mut seen = Vec::new();
    while let Ok(Some(line)) =
        tokio::time::timeout(std::time::Duration::from_millis(500), lines_rx.recv()).await
    {
        seen.push(line);
    }
    assert!(
        seen.iter().any(|line| line.contains("from one")),
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|line| line.contains("from zero")),
        "{seen:?}"
    );
}

#[tokio::test]
async fn follow_logs_does_not_block_the_event_loop() {
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = crate::grill::process::ProcessGrill::new();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
    allow_host_workloads(&mut agent, &[]);
    let handle = tokio::spawn(async move { agent.run().await });

    // A long-running process whose follow would block the loop for 60s if
    // handled inline.
    let config = Config::parse("[app.sleeper]\nscript = \"exec sleep 60\"\n").unwrap();
    let _ = send_deploy(&tx, config).await;

    // Start following logs; never drain them.
    let (line_tx, _line_rx) = mpsc::channel(16);
    tx.send(AgentCommand::FollowLogs {
        app_name: "sleeper".into(),
        namespace: "default".into(),
        tail: None,
        instance: None,
        label: None,
        lines: line_tx,
    })
    .await
    .unwrap();

    // A subsequent command must still be answered promptly.
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(3), resp_rx)
        .await
        .expect("event loop blocked by FollowLogs")
        .unwrap();
    assert!(!status.is_empty(), "sleeper should be listed");

    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), handle).await;
}

// ---------------------------------------------------------------------
// Stops whose SIGTERM is ignored wait off the command loop.
// ---------------------------------------------------------------------

/// The stop grace the stubborn-workload tests use: long enough that a
/// loop blocked on it is unmistakable, short enough to keep them quick.
const STUBBORN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// A process-runtime workload whose whole process group ignores SIGTERM,
/// as busybox `sleep` and many shells do as PID 1. It touches
/// `<dir>/<name>.trapped` once the trap is in place.
fn stubborn_app(name: &str, dir: &std::path::Path) -> String {
    let trapped = dir.join(format!("{name}.trapped"));
    format!(
        "[app.{name}]\nscript = \"trap '' TERM; touch '{}'; sleep 60\"\n",
        trapped.display()
    )
}

/// Deploy stubborn apps and wait until each has installed its trap, so
/// a SIGTERM can't land before the shell gets to ignore it.
async fn deploy_stubborn(tx: &mpsc::Sender<AgentCommand>, dir: &std::path::Path, names: &[&str]) {
    let config: String = names.iter().map(|name| stubborn_app(name, dir)).collect();
    expect_complete(&send_deploy(tx, Config::parse(&config).unwrap()).await);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !names
            .iter()
            .all(|name| dir.join(format!("{name}.trapped")).exists())
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("stubborn workloads never installed their trap");
}

/// Run a process-runtime agent with `STUBBORN_GRACE`, keeping a grill
/// handle so tests can see whether the process is really gone.
fn stubborn_agent() -> (
    mpsc::Sender<AgentCommand>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
    crate::grill::process::ProcessGrill,
    tempfile::TempDir,
) {
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let grill = crate::grill::process::ProcessGrill::new();
    let grill_handle = grill.clone();
    let port_allocator = PortAllocator::new(30000, 31000);
    let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    allow_host_workloads(&mut agent, &[]);
    agent.set_stop_grace(STUBBORN_GRACE);
    agent.set_shutdown_grace(std::time::Duration::from_millis(200));
    let handle = tokio::spawn(async move { agent.run().await });
    (tx, shutdown, handle, grill_handle, volumes)
}

/// Ask for status and return it with how long the loop took to answer.
async fn timed_status(
    tx: &mpsc::Sender<AgentCommand>,
) -> (Vec<InstanceStatus>, std::time::Duration) {
    let started = Instant::now();
    let (response, reply) = oneshot::channel();
    tx.send(AgentCommand::Status { response }).await.unwrap();
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), reply)
        .await
        .expect("status never answered")
        .unwrap();
    (status, started.elapsed())
}

fn send_stop(
    tx: &mpsc::Sender<AgentCommand>,
    app_name: &str,
) -> oneshot::Receiver<Result<(), BunError>> {
    let (response, reply) = oneshot::channel();
    tx.try_send(AgentCommand::Stop {
        app_name: app_name.into(),
        namespace: "default".into(),
        response,
    })
    .unwrap();
    reply
}

/// The V02 soak's stall: a workload that ignores SIGTERM made the agent
/// wait its whole grace inside the command loop, so `/v1/status` and the
/// report worker timed out behind it. Status must answer at once while
/// the stop is still waiting, and the stop must still end in SIGKILL.
#[tokio::test]
async fn status_answers_promptly_while_a_sigterm_ignoring_stop_waits() {
    let (tx, shutdown, handle, grill, volumes) = stubborn_agent();
    deploy_stubborn(&tx, volumes.path(), &["stubborn"]).await;
    let id = InstanceId("default__stubborn-0".into());

    let started = Instant::now();
    let stopped = send_stop(&tx, "stubborn");
    let (status, answered_in) = timed_status(&tx).await;

    assert!(
        answered_in < std::time::Duration::from_secs(1),
        "status waited {answered_in:?} behind the stop"
    );
    let instance = status.iter().find(|i| i.id == id.0).unwrap();
    assert_eq!(instance.state, ContainerState::Stopping.to_string());
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopping);

    tokio::time::timeout(std::time::Duration::from_secs(15), stopped)
        .await
        .expect("stop never finished")
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() >= STUBBORN_GRACE,
        "the workload must get its full grace before SIGKILL"
    );
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
    let (status, _) = timed_status(&tx).await;
    let instance = status.iter().find(|i| i.id == id.0).unwrap();
    assert_eq!(instance.state, ContainerState::Stopped.to_string());

    shutdown.cancel();
    handle.await.unwrap();
}

/// A retirement keeps ownership (status, port) while the process lives,
/// and releases it only once the runtime has confirmed the exit.
#[tokio::test]
async fn retirement_releases_ownership_only_after_the_process_exits() {
    let (tx, shutdown, handle, grill, volumes) = stubborn_agent();
    deploy_stubborn(&tx, volumes.path(), &["stubborn"]).await;
    let id = InstanceId("default__stubborn-0".into());

    let (response, retired) = oneshot::channel();
    tx.send(AgentCommand::Retire {
        app_name: "stubborn".into(),
        namespace: "default".into(),
        response,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let (status, answered_in) = timed_status(&tx).await;
    assert!(answered_in < std::time::Duration::from_secs(1));
    assert!(
        status.iter().any(|i| i.id == id.0),
        "ownership was released before the process exited"
    );
    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopping);

    tokio::time::timeout(std::time::Duration::from_secs(15), retired)
        .await
        .expect("retirement never finished")
        .unwrap()
        .unwrap();
    // The runtime then forgets the instance (#738), which it does only
    // for one it has seen stop.
    let state = grill.state(&id).await;
    assert!(
        matches!(
            state,
            Ok(ContainerState::Stopped) | Err(crate::grill::GrillError::NotFound { .. })
        ),
        "{state:?}"
    );
    let (status, _) = timed_status(&tx).await;
    assert!(status.is_empty(), "retirement must release ownership");

    shutdown.cancel();
    handle.await.unwrap();
}

/// Two stubborn stops overlap: together they cost one grace, not two.
/// Serial stops take at least two graces; the 1.8 bound leaves a
/// loaded runner room without admitting them.
#[tokio::test]
async fn concurrent_sigterm_ignoring_stops_overlap() {
    let (tx, shutdown, handle, grill, volumes) = stubborn_agent();
    deploy_stubborn(&tx, volumes.path(), &["first", "second"]).await;

    let started = Instant::now();
    let first = send_stop(&tx, "first");
    let second = send_stop(&tx, "second");
    for stopped in [first, second] {
        tokio::time::timeout(std::time::Duration::from_secs(15), stopped)
            .await
            .expect("stop never finished")
            .unwrap()
            .unwrap();
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < STUBBORN_GRACE * 9 / 5,
        "stops serialised: {elapsed:?} for two {STUBBORN_GRACE:?} graces"
    );
    for app in ["first", "second"] {
        let id = InstanceId(format!("default__{app}-0"));
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
    }

    shutdown.cancel();
    handle.await.unwrap();
}

/// A second stop of a workload that is already stopping joins the first
/// rather than signalling again, and both callers learn the outcome.
#[tokio::test]
async fn a_second_stop_joins_the_pending_one() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    agent.set_stop_grace(std::time::Duration::from_millis(500));
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    grill.set_ignore_stop(true);
    grill.set_state(
        &InstanceId("default__web-0".into()),
        ContainerState::Running,
    );
    let handle = tokio::spawn(async move { agent.run().await });

    let first = send_stop(&tx, "web");
    let second = send_stop(&tx, "web");
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();

    let id = InstanceId("default__web-0".into());
    let stops = grill
        .calls()
        .iter()
        .filter(|(op, i)| op == "stop" && i == &id)
        .count();
    assert_eq!(stops, 1, "a joined stop must not signal again");

    shutdown.cancel();
    handle.await.unwrap();
}

/// A retirement that arrives while an operator stop is pending joins it,
/// then forgets ownership once the shared stop confirms the exit.
#[tokio::test]
async fn a_retire_joining_a_pending_stop_releases_ownership() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    agent.set_stop_grace(std::time::Duration::from_millis(500));
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    grill.set_ignore_stop(true);
    grill.set_state(&id, ContainerState::Running);
    let handle = tokio::spawn(async move { agent.run().await });

    let stopped = send_stop(&tx, "web");
    let (response, retired) = oneshot::channel();
    tx.send(AgentCommand::Retire {
        app_name: "web".into(),
        namespace: "default".into(),
        response,
    })
    .await
    .unwrap();
    stopped.await.unwrap().unwrap();
    retired.await.unwrap().unwrap();

    let (status, _) = timed_status(&tx).await;
    assert!(
        status.is_empty(),
        "the joined retirement must release ownership"
    );
    let stops = grill
        .calls()
        .iter()
        .filter(|(op, i)| op == "stop" && i == &id)
        .count();
    assert_eq!(stops, 1, "the retirement must not signal again");

    shutdown.cancel();
    handle.await.unwrap();
}

/// The egress fence's stop returns before any grace passes and leaves the
/// wait to `stop_waits`, which still ends in SIGKILL and Stopped.
#[tokio::test]
async fn an_unattended_stop_returns_before_its_grace() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_stop_grace(std::time::Duration::from_millis(500));
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    grill.set_ignore_stop(true);
    grill.set_state(&id, ContainerState::Running);

    let started = Instant::now();
    agent.stop_app_unattended("web", "default").await.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_millis(400));
    assert_eq!(
        agent.supervisor.get_instance(&id).map(|i| i.state),
        Some(ContainerState::Stopping)
    );
    // A second fence joins the pending stop rather than starting another.
    agent.stop_app_unattended("web", "default").await.unwrap();
    assert_eq!(agent.stop_waits.len(), 1);

    let outcome = agent.stop_waits.join_next_with_id().await.unwrap();
    agent.complete_app_stop(outcome).await;
    assert_eq!(
        agent.supervisor.get_instance(&id).map(|i| i.state),
        Some(ContainerState::Stopped)
    );
    assert!(
        grill.calls().iter().any(|(op, i)| op == "kill" && i == &id),
        "a stubborn workload must still be force-killed"
    );
}

/// When a stop the egress fence relied on fails, its completion fences
/// execution at once instead of leaving it to a later tick.
#[tokio::test]
async fn a_failed_stop_the_egress_fence_relies_on_fences_at_once() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_stop_grace(std::time::Duration::from_millis(200));
    let config = Config::parse("[app.web]\nimage = \"myapp:v1\"\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    let id = InstanceId("default__web-0".into());
    grill.set_ignore_stop(true);
    grill.set_ignore_kill(true);
    grill.set_state(&id, ContainerState::Running);

    // An operator stop is pending when the egress fence arrives.
    let (response, stopped) = oneshot::channel();
    agent
        .request_app_stop("web".into(), "default".into(), StopPurpose::Stop, response)
        .await;
    agent.stop_app_unattended("web", "default").await.unwrap();

    let outcome = agent.stop_waits.join_next_with_id().await.unwrap();
    agent.complete_app_stop(outcome).await;

    assert!(
        stopped.await.unwrap().is_err(),
        "the stop must report its failure"
    );
    let kills = grill
        .calls()
        .iter()
        .filter(|(op, i)| op == "kill" && i == &id)
        .count();
    assert_eq!(
        kills, 2,
        "the fence must force-kill again after the failed stop"
    );
}

/// The execution fence kills only an instance whose runtime still names the
/// address the agent retained for it (#357). Moving that read off the loop
/// (#393) must not let the kill start before the answer is in.
#[tokio::test]
async fn the_execution_fence_kills_nothing_while_the_runtime_names_another_address() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_container_ip(std::net::Ipv4Addr::new(10, 0, 0, 7));
    let original = original_test_network_reference();
    grill.set_network_reference(original.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let mut other = serde_json::to_value(&original).unwrap();
    other["container_index"] = serde_json::json!(8);
    agent
        .network_references
        .insert(id.clone(), serde_json::from_value(other).unwrap());

    let fenced = agent.fence_app_execution("web", "default").await;

    assert!(
        matches!(fenced, Err(BunError::RetirementState { .. })),
        "{fenced:?}"
    );
    assert!(
        !grill.calls().iter().any(|(op, i)| op == "kill" && i == &id),
        "the fence killed an instance whose address it couldn't confirm"
    );
}

/// A deploy must not replace instances a pending stop still owns.
#[tokio::test]
async fn deploy_is_refused_while_the_workload_is_stopping() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    agent.set_stop_grace(std::time::Duration::from_secs(1));
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    grill.set_ignore_stop(true);
    grill.set_state(
        &InstanceId("default__web-0".into()),
        ContainerState::Running,
    );
    let handle = tokio::spawn(async move { agent.run().await });

    let stopped = send_stop(&tx, "web");
    let events = send_deploy(&tx, basic_config()).await;
    match events.last() {
        Some(ApplyEvent::Error { message }) => {
            assert!(message.contains("still stopping"), "{message}")
        }
        other => panic!("deploy over a pending stop was not refused: {other:?}"),
    }
    stopped.await.unwrap().unwrap();

    shutdown.cancel();
    handle.await.unwrap();
}

/// Shutdown doesn't wait out a pending stop's grace; the caller learns
/// the stop is unconfirmed and keeps what it owns.
#[tokio::test]
async fn shutdown_reports_pending_stops_unconfirmed() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    agent.set_stop_grace(std::time::Duration::from_secs(60));
    agent.set_shutdown_grace(std::time::Duration::from_millis(200));
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    grill.set_ignore_stop(true);
    grill.set_state(
        &InstanceId("default__web-0".into()),
        ContainerState::Running,
    );
    let handle = tokio::spawn(async move { agent.run().await });

    let stopped = send_stop(&tx, "web");
    let _ = timed_status(&tx).await;
    shutdown.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), stopped)
        .await
        .expect("shutdown waited out the stop grace")
        .unwrap();
    assert!(
        matches!(result, Err(BunError::StopIncomplete { .. })),
        "{result:?}"
    );
    handle.await.unwrap();
}

#[tokio::test]
async fn deploy_fails_closed_on_encrypted_secret_without_key() {
    // Single-node agent has no cluster security state, so it cannot decrypt
    // ENC[AGE:...] secrets. It must refuse to start the workload rather than
    // pass ciphertext into the container environment.
    let (mut agent, tx, shutdown) = test_agent();
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let config = Config::parse(
        r#"
            [app.web]
            image = "myapp:v1"
            [app.web.env]
            SECRET = "ENC[AGE:abc123]"
        "#,
    )
    .unwrap();
    let events = send_deploy(&tx, config).await;

    match events.last().expect("no events received") {
        ApplyEvent::Error { message } => {
            assert!(
                message.contains("encrypted secrets"),
                "unexpected error: {message}"
            );
        }
        other => panic!("expected fail-closed Error, got {other:?}"),
    }

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn deploy_streams_progress_events() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;

    // Should have progress events before the final Complete
    let progress_count = events
        .iter()
        .filter(|e| matches!(e, ApplyEvent::Progress { .. }))
        .count();
    assert!(progress_count >= 1, "expected progress events");

    let instance_created = events
        .iter()
        .any(|e| matches!(e, ApplyEvent::InstanceCreated { id, .. } if id == "default__web-0"));
    assert!(instance_created, "expected InstanceCreated for web-0");

    expect_complete(&events);

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn status_returns_all_instances() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    // Deploy first
    let events = send_deploy(&tx, basic_config()).await;
    expect_complete(&events);

    // Then get status
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();

    let statuses = resp_rx.await.unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].app_name, "web");
    // Without health checks, goes straight to Running
    assert_eq!(statuses[0].state, "running");

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn stop_command_stops_instances() {
    let (mut agent, tx, shutdown) = test_agent();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    // Deploy
    let events = send_deploy(&tx, basic_config()).await;
    expect_complete(&events);

    // Stop
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Stop {
        app_name: "web".to_string(),
        namespace: "default".to_string(),
        response: resp_tx,
    })
    .await
    .unwrap();
    resp_rx.await.unwrap().unwrap();

    // Verify stopped
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let statuses = resp_rx.await.unwrap();
    assert_eq!(statuses[0].state, "stopped");

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn deploy_with_health_check_starts_in_health_wait() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, config_with_health()).await;
    expect_complete(&events);

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let statuses = resp_rx.await.unwrap();
    // The instance should be in health-wait (awaiting first health check)
    // or running (if the mock health check resolved before we queried status).
    // Both are correct — it's a race between the status query and the
    // health check timer.
    let state = &statuses[0].state;
    assert!(
        state == "health-wait" || state == "running",
        "expected health-wait or running, got {state}"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn shutdown_stops_all_instances() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    expect_complete(&events);

    shutdown.cancel();
    agent_handle.await.unwrap();
    // Agent ran shutdown_all — grill.stop() was called
}

#[tokio::test]
async fn logs_returns_result_for_deployed_app() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    expect_complete(&events);

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Logs {
        app_name: "web".to_string(),
        namespace: "default".to_string(),
        tail: None,
        response: resp_tx,
    })
    .await
    .unwrap();
    let result = resp_rx.await.unwrap();
    // MockGrill returns empty logs, but the call should succeed
    assert!(result.is_ok());

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn logs_for_unknown_app_errors() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Logs {
        app_name: "nope".to_string(),
        namespace: "default".to_string(),
        tail: None,
        response: resp_tx,
    })
    .await
    .unwrap();
    let result = resp_rx.await.unwrap();
    assert!(result.is_err());

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn retirement_retains_ownership_until_durable_artifacts_are_removed() {
    for block_identity in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let records = root.path().join("records");
        std::fs::create_dir(&records).unwrap();
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        grill.set_pid(std::process::id());
        agent.set_records_dir(records.clone());
        agent.set_volumes_dir(root.path().join("volumes"));
        let id = InstanceId("default__web-0".into());
        let identity = agent.instance_identity_dir(&id);
        let task = tokio::spawn(async move {
            agent.run().await;
            agent
        });
        expect_complete(&send_deploy(&tx, basic_config()).await);
        let record = crate::grill::records::record_path(&records, &id.0);
        if block_identity {
            crate::sesame::identity::cleanup_identity_dir(&identity).unwrap();
            std::fs::write(&identity, "blocked identity cleanup").unwrap();
            std::fs::write(&record, "owned until cleanup succeeds").unwrap();
        } else {
            std::fs::remove_file(&record).unwrap();
            std::fs::create_dir(&record).unwrap();
        }
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::Retire {
            app_name: "web".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        let outcome = result.await.unwrap();
        let durable_owner_retained = record.exists();
        let (response, status) = oneshot::channel();
        tx.send(AgentCommand::Status { response }).await.unwrap();
        let retained = status.await.unwrap();
        // Restore the injected filesystem fault before stopping the fixture.
        if block_identity {
            std::fs::remove_file(&identity).unwrap();
        } else {
            std::fs::remove_dir(&record).unwrap();
        }
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::Retire {
            app_name: "web".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        let retry = result.await.unwrap();
        shutdown.cancel();
        let agent = task.await.unwrap();
        assert!(
            outcome.is_err(),
            "retirement succeeded despite failed artifact removal"
        );
        assert!(
            durable_owner_retained,
            "failed artifact cleanup discarded the adoption record"
        );
        assert_eq!(
            retained.len(),
            1,
            "uncertain cleanup lost runtime ownership"
        );
        assert!(retry.is_ok(), "{retry:?}");
        assert!(agent.supervisor.list_instances().is_empty());
        assert!(!record.exists());
    }
}

#[tokio::test]
async fn stop_unknown_app_errors() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Stop {
        app_name: "nope".to_string(),
        namespace: "default".to_string(),
        response: resp_tx,
    })
    .await
    .unwrap();
    let result = resp_rx.await.unwrap();
    assert!(result.is_err());

    shutdown.cancel();
    agent_handle.await.unwrap();
}

// ---- per-instance workload identity lifecycle (PKI7/D9) ----

/// Names of the entries under `{volumes}/.identity`, sorted.
fn identity_dir_names(volumes: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(volumes.join(".identity"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// PKI7: deploying two replicas prepares one identity directory per
/// instance, and stopping the app removes them (key material never
/// outlives the instance).
#[tokio::test]
async fn deploy_prepares_and_stop_removes_per_instance_identity_dirs() {
    let (mut agent, tx, shutdown) = test_agent();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let config = Config::parse(
        r#"
            [app.web]
            image = "myapp:v1"
            replicas = 2
        "#,
    )
    .unwrap();
    let events = send_deploy(&tx, config).await;
    expect_complete(&events);

    assert_eq!(
        identity_dir_names(volumes.path()),
        vec!["default__web-0".to_string(), "default__web-1".to_string()],
        "one identity dir per instance"
    );

    // Simulate provisioned key material so the stop has something to
    // scrub (single-node mode never reaches the council).
    std::fs::write(
        volumes.path().join(".identity/default__web-0/key.pem"),
        b"PRIVATE KEY",
    )
    .unwrap();

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Stop {
        app_name: "web".to_string(),
        namespace: "default".to_string(),
        response: resp_tx,
    })
    .await
    .unwrap();
    resp_rx.await.unwrap().unwrap();

    assert!(
        identity_dir_names(volumes.path()).is_empty(),
        "stop removes every instance identity dir"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

/// PKI7: a rolling redeploy leaves exactly the live (new) instances'
/// identity dirs — the retired generation's key material is gone.
#[tokio::test]
async fn rolling_redeploy_leaves_only_live_instances_identity_dirs() {
    let (mut agent, tx, shutdown) = test_agent();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    expect_complete(&events);
    assert_eq!(
        identity_dir_names(volumes.path()),
        vec!["default__web-0".to_string()]
    );

    // Redeploy: the rolling path replaces web-0 with web-g1-0.
    let events = send_deploy(&tx, basic_config()).await;
    let (_, instances) = expect_complete(&events);
    let mut expected: Vec<String> = instances.to_vec();
    expected.sort();

    assert_eq!(
        identity_dir_names(volumes.path()),
        expected,
        "exactly the live instances' dirs survive the redeploy"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn halted_rollout_keeps_healthy_replacements_in_ordinary_supervision() {
    for strategy in ["rolling", "blue-green"] {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        let config =
            Config::parse("[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 2\n").unwrap();
        expect_complete(&drain_deploy(&mut agent, config).await);
        let failed = InstanceId("default__web-g1-1".into());
        grill.set_state(&failed, ContainerState::Failed);
        let replacement = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\nreplicas = 2\n[app.web.deploy]\nstrategy = '{strategy}'\nauto_rollback = false\nhealth_timeout = '1s'\n")).unwrap();
        let outcome = drain_deploy(&mut agent, replacement).await;
        assert!(
            matches!(outcome.last(), Some(ApplyEvent::Error { message }) if message.contains("halted")),
            "{outcome:?}"
        );
        let healthy = InstanceId("default__web-g1-0".into());
        let owner = agent.supervisor.get_instance(&healthy).unwrap();
        assert_eq!(owner.state, ContainerState::Running);
        assert!(owner.oci_spec.is_some());
        assert!(agent.supervisor.get_instance(&failed).is_none());
        assert!(crate::grill::records::record_path(records.path(), &healthy.0).exists());
        agent.retire_workload("web", "default").await.unwrap();
        assert!(agent.supervisor.instances.is_empty());
        assert_eq!(agent.supervisor.port_allocator.allocated_count().await, 0);
        assert!(
            crate::grill::records::load_records(records.path())
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn rolling_redeploy_halts_without_reverting_when_auto_rollback_is_false() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let history = agent.deploy_history_handle();
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    fn halt_config() -> Config {
        Config::parse(
            r#"
                [app.web]
                image = "myapp:v1"
                port = 8080

                [app.web.deploy]
                auto_rollback = false
                health_timeout = "1s"
            "#,
        )
        .unwrap()
    }

    // First deploy: web-0 comes up healthy (MockGrill defaults to Running).
    expect_complete(&send_deploy(&tx, halt_config()).await);

    // The next rolling redeploy's new instance (generation 1) never becomes
    // healthy, so the rollout fails after the 1s health wait.
    let new_id = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
    grill.set_state(&new_id, crate::grill::state::ContainerState::Failed);

    let events = send_deploy(&tx, halt_config()).await;
    match events.last().expect("no events received") {
        ApplyEvent::Error { message } => {
            assert!(
                message.contains("halted"),
                "expected a halt, got: {message}"
            );
            assert!(
                !message.contains("rolled back"),
                "must not revert: {message}"
            );
        }
        other => panic!("expected an Error (halt) event, got {other:?}"),
    }

    // Halt keeps the old instance and tears down only the failed new one.
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
    assert!(
        ids.iter().any(|id| id == "default__web-0"),
        "the old instance survives a halt, got {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id.contains("web-g1")),
        "the failed new instance was torn down, got {ids:?}"
    );

    // The deploy is recorded as Halted, not RolledBack.
    let hist = history.read().await;
    assert!(
        hist.iter()
            .any(|e| e.result == crate::meat::deploy_types::DeployResult::Halted),
        "a Halted deploy-history entry was recorded, got {:?}",
        hist.iter().map(|e| &e.result).collect::<Vec<_>>()
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

async fn drain_deploy(agent: &mut BunAgent<MockGrill>, config: Config) -> Vec<ApplyEvent> {
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    let mut events = Vec::new();
    while let Some(e) = ev_rx.recv().await {
        events.push(e);
    }
    events
}

#[tokio::test]
async fn failed_rollout_retains_every_owner_until_cleanup_is_confirmed() {
    for strategy in ["rolling", "blue-green"] {
        for auto_rollback in [true, false] {
            for fault in [
                "kill error",
                "kill ignored",
                "kill stalled",
                "inspection",
                "record",
                "identity",
            ] {
                let root = tempfile::tempdir().unwrap();
                let records = root.path().join("records");
                let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
                agent.set_volumes_dir(root.path().join("volumes"));
                agent.set_records_dir(records.clone());
                grill.set_pid(std::process::id());
                let new_id = InstanceId("default__web-g1-0".into());
                let record = crate::grill::records::record_path(&records, &new_id.0);
                let identity = agent.instance_identity_dir(&new_id);
                let history = agent.deploy_history_handle();
                let task = tokio::spawn(async move {
                    agent.run().await;
                    agent
                });
                expect_complete(&send_deploy(&tx, basic_config()).await);
                grill.set_state(&new_id, ContainerState::Failed);
                let config = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\n[app.web.deploy]\nstrategy = '{strategy}'\nauto_rollback = {auto_rollback}\nhealth_timeout = '30s'\n")).unwrap();
                let (events, mut stream) = mpsc::channel(64);
                tx.send(AgentCommand::Deploy { config, events })
                    .await
                    .unwrap();
                let ApplyEvent::Accepted { operation_id } = stream.recv().await.unwrap() else {
                    panic!("missing operation id")
                };
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while !record.exists() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let original_record = std::fs::read(&record).unwrap();
                match fault {
                    "kill error" => grill.set_fail_kill(true),
                    "kill ignored" => grill.set_ignore_kill(true),
                    "kill stalled" => grill.block_kills(),
                    "inspection" => grill.set_instance_inspection_failure(&new_id, true),
                    "record" => {
                        std::fs::remove_file(&record).unwrap();
                        std::fs::create_dir(&record).unwrap();
                    }
                    _ => {
                        crate::sesame::identity::cleanup_identity_dir(&identity).unwrap();
                        std::fs::write(&identity, "blocked").unwrap();
                    }
                }
                let (response, cancelled) = oneshot::channel();
                tx.send(AgentCommand::CancelDeploy {
                    operation_id: operation_id.into(),
                    response,
                })
                .await
                .unwrap();
                cancelled.await.unwrap().unwrap();
                let outcome = tokio::time::timeout(std::time::Duration::from_secs(6), async {
                    let mut events = Vec::new();
                    while let Some(event) = stream.recv().await {
                        events.push(event);
                    }
                    events
                })
                .await;
                let (response, status) = oneshot::channel();
                tx.send(AgentCommand::Status { response }).await.unwrap();
                let retained =
                    tokio::time::timeout(std::time::Duration::from_secs(1), status).await;
                let record_retained = record.exists();
                let claimed_rollback = history.read().await.iter().any(|entry| {
                    entry.result == crate::meat::deploy_types::DeployResult::RolledBack
                });
                grill.set_fail_kill(false);
                grill.set_ignore_kill(false);
                grill.set_instance_inspection_failure(&new_id, false);
                grill.release_kills(4);
                if fault == "record" {
                    std::fs::remove_dir(&record).unwrap();
                    std::fs::write(&record, original_record).unwrap();
                }
                if fault == "identity" {
                    std::fs::remove_file(&identity).unwrap();
                }
                grill.kill(&new_id).await.unwrap();
                let (response, retired) = oneshot::channel();
                tx.send(AgentCommand::Retire {
                    app_name: "web".into(),
                    namespace: "default".into(),
                    response,
                })
                .await
                .unwrap();
                let recovery = retired.await.unwrap();
                shutdown.cancel();
                let agent = task.await.unwrap();
                let outcome = outcome.expect("rollback runtime cleanup must be bounded");
                let retained = retained
                    .expect("rollback must leave the agent responsive")
                    .unwrap();
                assert!(
                    outcome
                        .iter()
                        .any(|event| matches!(event, ApplyEvent::Error { .. }))
                );
                assert_eq!(
                    retained.len(),
                    2,
                    "{strategy}/{auto_rollback}/{fault} discarded a cleanup owner: {outcome:?}"
                );
                assert!(record_retained, "{fault} discarded adoption ownership");
                assert!(
                    !claimed_rollback,
                    "unconfirmed cleanup was recorded as rolled back"
                );
                assert!(recovery.is_ok(), "{recovery:?}");
                assert!(agent.supervisor.list_instances().is_empty());
                assert_eq!(agent.supervisor.port_allocator.allocated_count().await, 0);
                assert!(
                    crate::grill::records::load_records(&records)
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
}

#[tokio::test]
async fn rollout_retirement_fences_the_crash_restart_driver() {
    for (strategy, replicas) in [("rolling", 1), ("rolling", 2), ("blue-green", 2)] {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let mut initial = basic_config();
        initial.app.get_mut("web").unwrap().replicas = crate::config::Replicas::Fixed(replicas);
        expect_complete(&drain_deploy(&mut agent, initial).await);
        grill.set_ignore_stop(true);
        grill.block_kills();
        let config = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\nreplicas = {replicas}\n[app.web.deploy]\nstrategy = '{strategy}'\ndrain_timeout = '0s'\n")).unwrap();
        let (events, mut stream) = mpsc::channel(64);
        agent.begin_deploy(config, events).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    Some(op) = agent.deploy_ops_rx.recv() => agent.handle_deploy_op(op).await,
                    () = grill.wait_for_kills(1) => break,
                }
            }
        })
        .await
        .unwrap();
        let old_id = grill
            .calls()
            .into_iter()
            .rev()
            .find(|(call, _)| call == "kill")
            .unwrap()
            .1;
        // Runtime exit can become observable before its request completes.
        // Run the actual periodic restart driver at that exact boundary.
        grill.set_state(&old_id, ContainerState::Stopped);
        agent.check_apps().await;
        let old = agent.supervisor.get_instance(&old_id).unwrap();
        let observed = (old.state, old.restart_count, old.retry_pending);
        grill.release_kills(1);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let mut events = Vec::new();
            loop {
                tokio::select! {
                    Some(op) = agent.deploy_ops_rx.recv() => agent.handle_deploy_op(op).await,
                    event = stream.recv() => match event {
                        Some(event) => events.push(event),
                        None => break events,
                    }
                }
            }
        })
        .await
        .unwrap();
        grill.set_ignore_stop(false);
        agent.stop_app("web", "default").await.unwrap();
        expect_complete(&outcome);
        assert_eq!(
            observed,
            (ContainerState::Stopping, 0, false),
            "{strategy} restarted a retiring instance"
        );
    }
}

#[tokio::test]
async fn rollout_retains_old_owner_when_runtime_retirement_is_unconfirmed() {
    for strategy in ["rolling", "blue-green"] {
        for fault in [
            "kill error",
            "kill ignored",
            "inspection error",
            "kill stalled",
        ] {
            let volumes = tempfile::tempdir().unwrap();
            let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
            agent.set_volumes_dir(volumes.path().to_path_buf());
            let task = tokio::spawn(async move {
                agent.run().await;
                agent
            });
            expect_complete(&send_deploy(&tx, basic_config()).await);
            let old_id = InstanceId("default__web-0".into());
            grill.set_ignore_stop(true);
            grill.set_state(&old_id, ContainerState::Running);
            match fault {
                "kill error" => grill.set_fail_kill(true),
                "kill ignored" => grill.set_ignore_kill(true),
                "kill stalled" => grill.block_kills(),
                _ => grill.set_instance_inspection_failure(&old_id, true),
            }
            let config = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\n[app.web.deploy]\nstrategy = '{strategy}'\ndrain_timeout = '0s'\nhealth_timeout = '100ms'\n")).unwrap();
            let events =
                tokio::time::timeout(std::time::Duration::from_secs(10), send_deploy(&tx, config))
                    .await
                    .expect("runtime retirement must have a deadline");
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::Status { response }).await.unwrap();
            let retained = result.await.unwrap();
            grill.set_fail_kill(false);
            grill.set_ignore_kill(false);
            grill.set_instance_inspection_failure(&old_id, false);
            grill.set_ignore_stop(false);
            grill.release_kills(1);
            grill.kill(&old_id).await.unwrap();
            let (response, retired) = oneshot::channel();
            tx.send(AgentCommand::Retire {
                app_name: "web".into(),
                namespace: "default".into(),
                response,
            })
            .await
            .unwrap();
            assert!(retired.await.unwrap().is_ok());
            let (response, status) = oneshot::channel();
            tx.send(AgentCommand::Status { response }).await.unwrap();
            assert!(
                status.await.unwrap().is_empty(),
                "recovery left a replacement unowned"
            );
            shutdown.cancel();
            task.await.unwrap();
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, ApplyEvent::Complete { .. })),
                "{strategy} accepted {fault}: {events:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ApplyEvent::Error { .. })),
                "missing retirement failure"
            );
            assert_eq!(
                retained.len(),
                2,
                "{strategy} must retain both the old owner and the started replacement after {fault}"
            );
            assert!(
                retained.iter().any(|instance| instance.id == old_id.0),
                "{strategy} forgot the unconfirmed owner after {fault}"
            );
        }
    }
}

#[tokio::test]
async fn rollout_retains_owners_when_artifact_retirement_fails() {
    for strategy in ["rolling", "blue-green"] {
        for block_identity in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let records = root.path().join("records");
            let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
            agent.set_volumes_dir(root.path().join("volumes"));
            agent.set_records_dir(records.clone());
            grill.set_pid(std::process::id());
            let artifacts: Vec<_> = (0..2)
                .map(|index| {
                    let id = InstanceId(format!("default__web-{index}"));
                    (
                        agent.instance_identity_dir(&id),
                        crate::grill::records::record_path(&records, &id.0),
                    )
                })
                .collect();
            let task = tokio::spawn(async move {
                agent.run().await;
                agent
            });
            let mut initial = basic_config();
            initial.app.get_mut("web").unwrap().replicas = crate::config::Replicas::Fixed(2);
            expect_complete(&send_deploy(&tx, initial).await);
            let mut original_records = Vec::new();
            // Block either possible first owner; HashMap iteration order
            // must not decide whether this exercises the whole old fleet.
            for (identity, record) in &artifacts {
                original_records.push(std::fs::read(record).unwrap());
                if block_identity {
                    crate::sesame::identity::cleanup_identity_dir(identity).unwrap();
                    std::fs::write(identity, "blocked identity cleanup").unwrap();
                } else {
                    std::fs::remove_file(record).unwrap();
                    std::fs::create_dir(record).unwrap();
                }
            }
            let config = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\nreplicas = 2\n[app.web.deploy]\nstrategy = '{strategy}'\ndrain_timeout = '0s'\n")).unwrap();
            let events = send_deploy(&tx, config).await;
            let durable_owner_retained = artifacts.iter().all(|(_, record)| record.exists());
            let (response, status) = oneshot::channel();
            tx.send(AgentCommand::Status { response }).await.unwrap();
            let retained = status.await.unwrap();
            for ((identity, record), original_record) in artifacts.iter().zip(original_records) {
                if block_identity {
                    std::fs::remove_file(identity).unwrap();
                } else {
                    std::fs::remove_dir(record).unwrap();
                    std::fs::write(record, original_record).unwrap();
                }
            }
            let (response, retired) = oneshot::channel();
            tx.send(AgentCommand::Retire {
                app_name: "web".into(),
                namespace: "default".into(),
                response,
            })
            .await
            .unwrap();
            let recovery = retired.await.unwrap();
            shutdown.cancel();
            let agent = task.await.unwrap();
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, ApplyEvent::Complete { .. })),
                "{strategy} ignored artifact failure: {events:?}"
            );
            assert!(durable_owner_retained);
            assert_eq!(
                retained.len(),
                if strategy == "rolling" { 3 } else { 4 },
                "both generations need a cleanup owner"
            );
            if strategy == "blue-green" {
                assert!(
                    retained
                        .iter()
                        .filter(|instance| !instance.id.contains("-g"))
                        .all(|instance| instance.state == "stopped"),
                    "every retired blue instance must be stopped: {retained:?}"
                );
            }
            assert!(
                retained
                    .iter()
                    .any(|instance| !instance.id.contains("-g") && instance.state == "stopped"),
                "the old instance must not be eligible for crash restart"
            );
            assert!(recovery.is_ok(), "{recovery:?}");
            assert!(agent.supervisor.list_instances().is_empty());
            assert!(
                crate::grill::records::load_records(&records)
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

#[tokio::test]
async fn blue_green_redeploy_swaps_to_the_green_fleet() {
    let (mut agent, tx, shutdown, _grill) = test_agent_with_grill();
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    fn bg_config() -> Config {
        Config::parse(
            r#"
                [app.web]
                image = "myapp:v1"
                port = 8080

                [app.web.deploy]
                strategy = "blue-green"
                health_timeout = "1s"
            "#,
        )
        .unwrap()
    }

    // First deploy: no existing instances, so the fresh path brings up the
    // blue fleet (web-0). MockGrill defaults every container to Running.
    expect_complete(&send_deploy(&tx, bg_config()).await);

    // Redeploy: existing instances present, so the strategy dispatch routes
    // to blue-green — the green fleet (generation 1) comes up and swaps.
    expect_complete(&send_deploy(&tx, bg_config()).await);

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids.len(),
        1,
        "exactly one green instance is live, got {ids:?}"
    );
    assert!(
        !ids.iter().any(|id| id == "default__web-0"),
        "the blue instance must be retired after the swap, got {ids:?}"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

/// M7 ordering guarantee: the blue-green cut-over publishes the green
/// backends *before* draining and stopping the blue fleet. An in-flight
/// request holds the blue drain open; while it does, the green backend
/// must already be routable and blue must still be unstopped — and the
/// whole wait runs on the deploy worker, so the command loop keeps
/// answering (asserted via a Status round-trip mid-drain).
#[tokio::test]
async fn blue_green_publishes_green_before_stopping_blue() {
    let (agent, tx, shutdown, grill) = test_agent_with_grill();
    let drains = agent.drains_handle();
    let mut service_maps = agent.service_map_watch();
    let mut agent = agent;
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    fn bg_config() -> Config {
        Config::parse(
            r#"
                [app.web]
                image = "myapp:v1"
                port = 8080

                [app.web.deploy]
                strategy = "blue-green"
                health_timeout = "1s"
            "#,
        )
        .unwrap()
    }

    expect_complete(&send_deploy(&tx, bg_config()).await);
    let blue_id = InstanceId("default__web-0".to_string());

    // Simulate the proxy holding an in-flight request on blue: pre-start
    // its drain and bump the connection count. The worker's own
    // `start_drain` is a no-op on an already-draining instance, so the
    // cut-over blocks on this connection.
    drains
        .start_drain(&crate::wrapper::draining::DrainCommand {
            app_name: "web".to_string(),
            instance_id: blue_id.0.clone(),
            timeout: std::time::Duration::from_secs(30),
        })
        .await;
    drains.increment_connections(&blue_id.0).await;

    let redeploy_tx = tx.clone();
    let redeploy = tokio::spawn(async move { send_deploy(&redeploy_tx, bg_config()).await });

    // Wait until the green backend is published (the service-map watch
    // publishes on every rebuild).
    let green_id = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
    let published = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let map = service_maps.borrow_and_update().clone();
            let has_green = map
                .resolve_by_name("web")
                .is_some_and(|e| e.backends.iter().any(|b| b.instance_id == green_id.0));
            if has_green {
                return;
            }
            if service_maps.changed().await.is_err() {
                panic!("service map channel closed before green was published");
            }
        }
    })
    .await;
    assert!(published.is_ok(), "green backend was never published");

    // Green is routable, blue's drain is still held open: blue must not
    // have been stopped or killed yet.
    assert!(
        !grill
            .calls()
            .iter()
            .any(|(op, i)| (op == "stop" || op == "kill") && i == &blue_id),
        "blue was stopped before its in-flight request drained"
    );

    // The wait runs on the deploy worker, not the command loop: the agent
    // still answers commands mid-drain.
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), resp_rx)
        .await
        .expect("command loop was blocked during the blue drain")
        .unwrap();

    // The request finishes; the cut-over completes and blue is stopped.
    drains.decrement_connections(&blue_id.0).await;
    let events = tokio::time::timeout(std::time::Duration::from_secs(5), redeploy)
        .await
        .expect("redeploy did not complete after the drain released")
        .unwrap();
    expect_complete(&events);
    assert!(
        grill
            .calls()
            .iter()
            .any(|(op, i)| op == "stop" && i == &blue_id),
        "blue must be stopped after the drain"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

/// A minimal HTTP responder that answers every connection with `status`,
/// standing in for the app's health endpoint. Returns the bound port.
/// MockGrill reports no container IP, so the deploy gate probes
/// `127.0.0.1:{spec.port}` — binding the responder there and pointing
/// `port` at it exercises the real `probe_health` path end to end.
async fn spawn_health_responder(status: u16) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
            let response = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
        }
    });
    port
}

fn no_health_config(port: u16) -> Config {
    Config::parse(&format!(
        r#"
            [app.web]
            image = "myapp:v1"
            port = {port}
        "#
    ))
    .unwrap()
}

fn health_gated_config(port: u16, strategy: &str) -> Config {
    Config::parse(&format!(
        r#"
            [app.web]
            image = "myapp:v2"
            port = {port}

            [app.web.health]
            path = "/healthz"

            [app.web.deploy]
            strategy = "{strategy}"
            health_timeout = "1s"
        "#
    ))
    .unwrap()
}

/// M5: a replacement whose container runs but whose HTTP health check
/// fails must NOT replace the old instance — the deploy rolls back and
/// the old instance keeps serving. Before the gate, the rolling wait
/// only polled `grill.state == Running` (which MockGrill always
/// satisfies), so this exact scenario replaced a healthy v1 with a
/// broken v2.
#[tokio::test]
async fn rolling_redeploy_rolls_back_when_the_probe_fails() {
    let port = spawn_health_responder(500).await;
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    expect_complete(&send_deploy(&tx, no_health_config(port)).await);

    let events = send_deploy(&tx, health_gated_config(port, "rolling")).await;
    let error = events
        .iter()
        .find_map(|e| match e {
            ApplyEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("a probe-failing redeploy must produce an error event");
    assert!(
        error.contains("failed its health check"),
        "error should name the failed probe, got: {error}"
    );

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        vec!["default__web-0".to_string()],
        "the old instance must keep serving after the rollback"
    );
    let old_id = InstanceId("default__web-0".to_string());
    assert!(
        !grill
            .calls()
            .iter()
            .any(|(op, i)| (op == "stop" || op == "kill") && i == &old_id),
        "the old instance must never be stopped when the replacement fails its probe"
    );
    let canary = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
    assert!(
        grill
            .calls()
            .iter()
            .any(|(op, i)| op == "kill" && i == &canary),
        "the probe-failing replacement must be torn down"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

/// Run `first`, then `second` with the given replacements failing their
/// health wait, on a running agent. Returns the agent once it has shut
/// down, every event of the second deploy, and its deploy history.
async fn deploy_over_failing_replacements(
    first: &str,
    second: &str,
    failing: &[InstanceId],
) -> (
    TestAgent,
    Vec<ApplyEvent>,
    Vec<crate::meat::deploy_types::DeployHistoryEntry>,
) {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    let history = agent.deploy_history_handle();
    let task = tokio::spawn(async move {
        agent.run().await;
        agent
    });
    expect_complete(&send_deploy(&tx, Config::parse(first).unwrap()).await);
    for id in failing {
        grill.set_state(id, ContainerState::Failed);
    }
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        send_deploy(&tx, Config::parse(second).unwrap()),
    )
    .await
    .expect("the aborted rollout must finish");
    let history = history.read().await.clone();
    shutdown.cancel();
    (task.await.unwrap(), events, history)
}

/// The live instances of `web`, sorted, with the image each runs.
fn web_instances(agent: &BunAgent<MockGrill>) -> Vec<(String, String)> {
    let mut instances: Vec<(String, String)> = agent
        .supervisor
        .list_instances()
        .iter()
        .filter(|instance| instance.app_name == "web")
        .filter(|instance| {
            !matches!(
                instance.state,
                ContainerState::Stopped | ContainerState::Failed
            )
        })
        .map(|instance| (instance.id.0.clone(), instance.image.clone()))
        .collect();
    instances.sort();
    instances
}

/// #692 scenario A: three replicas on one node, defaults. The first
/// replacement goes healthy and an old instance retires for it, then the
/// second replacement fails. Auto-rollback used to kill the healthy
/// replacement and never restart the retired old instance, leaving two of
/// three. It must end with three replicas, all on the old spec.
#[tokio::test]
async fn auto_rollback_after_a_retirement_restores_old_capacity() {
    let canary = |generation, ordinal| {
        crate::grill::InstanceIdentity::canary("default", "web", generation, ordinal).instance_id()
    };
    let (agent, events, history) = deploy_over_failing_replacements(
        "[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 3\n\
         [app.web.deploy]\ndrain_timeout = '0s'\n",
        "[app.web]\nimage = 'web:v2'\nport = 8080\nreplicas = 3\n\
         [app.web.deploy]\ndrain_timeout = '0s'\nhealth_timeout = '1s'\n",
        &[canary(1, 1)],
    )
    .await;

    let message = events
        .iter()
        .rev()
        .find_map(|event| match event {
            ApplyEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("the failed rollout must report an error");
    assert!(message.contains("rolled back"), "{message}");
    let instances = web_instances(&agent);
    assert_eq!(
        instances.len(),
        3,
        "the abort must end with the capacity it started from: {instances:?}"
    );
    assert!(
        instances.iter().all(|(_, image)| image == "web:v1"),
        "every replica must run the old spec again: {instances:?}"
    );
    assert!(
        instances.iter().all(|(id, _)| !id.contains("-g1-")),
        "no replacement of the failed spec may survive a rollback: {instances:?}"
    );
    assert_eq!(
        instances
            .iter()
            .filter(|(id, _)| id.contains("-g2-"))
            .count(),
        1,
        "the retired old instance comes back as one old-spec replacement: {instances:?}"
    );
    assert_eq!(
        history.last().map(|entry| entry.result),
        Some(crate::meat::deploy_types::DeployResult::RolledBack)
    );
    assert_eq!(
        agent
            .deployed_specs
            .get(&("web".to_string(), "default".to_string()))
            .and_then(|spec| spec.image.clone())
            .as_deref(),
        Some("web:v1"),
        "a rolled-back app's deployed spec is the old one again"
    );
}

/// #692 scenario B: a managed volume forces stop-first, so the only old
/// instance retires before its replacement starts. When the replacement
/// fails, the old spec must run again rather than leave nothing.
#[tokio::test]
async fn auto_rollback_of_a_stop_first_volume_app_restarts_the_old_instance() {
    let (agent, events, history) = deploy_over_failing_replacements(
        "[app.web]\nimage = 'web:v1'\n[[app.web.volumes]]\npath = '/data'\n\
         [app.web.deploy]\ndrain_timeout = '0s'\n",
        "[app.web]\nimage = 'web:v2'\n[[app.web.volumes]]\npath = '/data'\n\
         [app.web.deploy]\ndrain_timeout = '0s'\nhealth_timeout = '1s'\n",
        &[crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id()],
    )
    .await;

    assert!(
        matches!(events.last(), Some(ApplyEvent::Error { .. })),
        "{events:?}"
    );
    let instances = web_instances(&agent);
    assert_eq!(
        instances,
        vec![(
            crate::grill::InstanceIdentity::canary("default", "web", 2, 0)
                .instance_id()
                .0,
            "web:v1".to_string()
        )],
        "the volume app must run its old spec again, not nothing"
    );
    assert_eq!(
        history.last().map(|entry| entry.result),
        Some(crate::meat::deploy_types::DeployResult::RolledBack)
    );
}

/// When the old spec can't come back either, the abort keeps the healthy
/// replacement serving and says so, rather than killing it.
#[tokio::test]
async fn abort_keeps_healthy_new_instances_when_old_capacity_cannot_return() {
    let canary = |generation, ordinal| {
        crate::grill::InstanceIdentity::canary("default", "web", generation, ordinal).instance_id()
    };
    let (agent, events, history) = deploy_over_failing_replacements(
        "[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 3\n\
         [app.web.deploy]\ndrain_timeout = '0s'\nhealth_timeout = '1s'\n",
        "[app.web]\nimage = 'web:v2'\nport = 8080\nreplicas = 3\n\
         [app.web.deploy]\ndrain_timeout = '0s'\nhealth_timeout = '1s'\n",
        &[canary(1, 1), canary(2, 0), canary(2, 1), canary(2, 2)],
    )
    .await;

    let message = events
        .iter()
        .rev()
        .find_map(|event| match event {
            ApplyEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("the failed rollout must report an error");
    assert!(message.contains("halted"), "{message}");
    let instances = web_instances(&agent);
    assert_eq!(
        instances.len(),
        3,
        "healthy new + remaining old must survive the abort: {instances:?}"
    );
    assert!(
        instances.contains(&(canary(1, 0).0, "web:v2".to_string())),
        "the healthy replacement must keep serving: {instances:?}"
    );
    assert_eq!(
        instances
            .iter()
            .filter(|(_, image)| image == "web:v1")
            .count(),
        2,
        "{instances:?}"
    );
    assert_eq!(
        history.last().map(|entry| entry.result),
        Some(crate::meat::deploy_types::DeployResult::Halted)
    );
}

/// The gate must not break healthy deploys: with the responder answering
/// 200, the rolling redeploy completes through the real probe path and
/// the replacement takes over.
#[tokio::test]
async fn rolling_redeploy_completes_when_the_probe_passes() {
    let port = spawn_health_responder(200).await;
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    expect_complete(&send_deploy(&tx, no_health_config(port)).await);
    expect_complete(&send_deploy(&tx, health_gated_config(port, "rolling")).await);

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
    let canary = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
    assert_eq!(
        ids,
        vec![canary.0.clone()],
        "the probe-passing replacement must take over"
    );
    let old_id = InstanceId("default__web-0".to_string());
    assert!(
        grill
            .calls()
            .iter()
            .any(|(op, i)| op == "stop" && i == &old_id),
        "the old instance must be retired after the healthy replacement"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

/// M5 on the blue-green path: a green fleet that starts but fails its
/// probe must leave blue serving untouched.
#[tokio::test]
async fn blue_green_rolls_back_when_green_fails_its_probe() {
    let port = spawn_health_responder(500).await;
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    expect_complete(&send_deploy(&tx, no_health_config(port)).await);

    let events = send_deploy(&tx, health_gated_config(port, "blue-green")).await;
    assert!(
        matches!(events.last(), Some(ApplyEvent::Error { .. })),
        "a probe-failing green fleet must fail the deploy, got: {events:?}"
    );

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(
        ids,
        vec!["default__web-0".to_string()],
        "blue must keep serving when green fails its probe"
    );
    let old_id = InstanceId("default__web-0".to_string());
    assert!(
        !grill
            .calls()
            .iter()
            .any(|(op, i)| (op == "stop" || op == "kill") && i == &old_id),
        "blue must never be stopped when green fails its probe"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn application_deploy_refuses_failed_adoption_record_write() {
    let records = tempfile::tempdir().unwrap();
    let (mut agent, _, _, grill) = test_agent_with_grill();
    agent.set_records_dir(records.path().to_path_buf());
    grill.set_pid(std::process::id());
    std::fs::create_dir(records.path().join("default__web-0.json")).unwrap();
    let events = drain_deploy(
        &mut agent,
        Config::parse("[app.web]\nimage = 'test:v1'\n").unwrap(),
    )
    .await;
    assert!(
        matches!(events.last(), Some(ApplyEvent::Error { .. })),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ApplyEvent::Complete { .. }))
    );
}

#[tokio::test]
async fn application_deploy_refuses_missing_runtime_identity_for_adoption() {
    let records = tempfile::tempdir().unwrap();
    let (mut agent, _, _, _) = test_agent_with_grill();
    agent.set_records_dir(records.path().to_path_buf());
    let events = drain_deploy(
        &mut agent,
        Config::parse("[app.web]\nimage = 'test:v1'\n").unwrap(),
    )
    .await;
    assert!(
        matches!(events.last(), Some(ApplyEvent::Error { .. })),
        "{events:?}"
    );
}

/// Z6.7: runc holds an instance's lifecycle lock for its whole create,
/// image pull included, so asking it for a creating instance's PID held
/// the agent loop for the length of the pull. Status timed out, reports
/// went stale, and the leader moved the node's workloads elsewhere.
#[tokio::test]
async fn status_does_not_ask_the_runtime_about_an_instance_being_created() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    grill.set_pid(4242);
    let mut config = basic_config();
    config.app.get_mut("web").unwrap().replicas = crate::config::types::Replicas::Fixed(2);
    expect_complete(&drain_deploy(&mut agent, config).await);
    let creating = InstanceId("default__web-1".into());
    agent.supervisor.get_instance_mut(&creating).unwrap().state = ContainerState::Preparing;

    let statuses = agent.get_status().await;
    let pid_of = |id: &str| {
        statuses
            .iter()
            .find(|status| status.id == id)
            .map(|status| status.pid)
    };
    assert_eq!(pid_of("default__web-0"), Some(Some(4242)));
    assert_eq!(pid_of("default__web-1"), Some(None));
}

#[tokio::test]
async fn fresh_workloads_cannot_replace_another_apps_generation_owner() {
    for kind in ["app", "job"] {
        for state in [ContainerState::Running, ContainerState::Stopped] {
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            let records = tempfile::tempdir().unwrap();
            agent.set_records_dir(records.path().to_path_buf());
            grill.set_pid(std::process::id());
            for image in ["worker:v1", "worker:v2"] {
                let config =
                    Config::parse(&format!("[app.worker]\nimage = '{image}'\nport = 8080\n"))
                        .unwrap();
                expect_complete(&drain_deploy(&mut agent, config).await);
            }
            let id = InstanceId("default__worker-g1-0".into());
            agent.supervisor.get_instance_mut(&id).unwrap().state = state;
            grill.set_state(&id, state);
            let original_port = agent.supervisor.get_instance(&id).unwrap().host_port;
            let original_records = crate::grill::records::load_records(records.path()).unwrap();
            let original_ports = agent.supervisor.port_allocator.allocated_count().await;
            let calls_before = grill.calls().len();
            let collision =
                Config::parse(&format!("[{kind}.worker-g1]\nimage = 'intruder:v1'\n")).unwrap();
            let events = drain_deploy(&mut agent, collision).await;
            assert!(
                matches!(events.last(), Some(ApplyEvent::Error { .. })),
                "{kind}/{state:?}: {events:?}"
            );
            let owner = agent.supervisor.get_instance(&id).unwrap();
            assert_eq!(owner.app_name, "worker");
            assert_eq!(owner.image, "worker:v2");
            assert_eq!(owner.host_port, original_port);
            assert_eq!(
                agent.supervisor.port_allocator.allocated_count().await,
                original_ports
            );
            assert_eq!(
                crate::grill::records::load_records(records.path()).unwrap(),
                original_records
            );
            assert!(
                !grill.calls()[calls_before..]
                    .iter()
                    .any(|(operation, _)| matches!(
                        operation.as_str(),
                        "create" | "start" | "stop" | "kill"
                    ))
            );
        }
    }
}

#[tokio::test]
async fn workload_labels_are_refused_before_runtime_mutation() {
    for (name, namespace) in [("Bad", "default"), ("web", "bad__namespace")] {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let mut config = basic_config();
        let mut spec = config.app.remove("web").unwrap();
        spec.namespace = Some(namespace.into());
        config.app.insert(name.into(), spec);
        let handle = tokio::spawn(async move { agent.run().await });
        let events = send_deploy(&tx, config).await;
        shutdown.cancel();
        handle.await.unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Error { .. })),
            "invalid label was deployed: {events:?}"
        );
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(operation, _)| operation == "create")
        );
    }
}

fn config_with_init_container() -> Config {
    let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [[app.web.init]]
            command = ["echo", "init"]
        "#;
    Config::parse(toml_str).unwrap()
}

#[tokio::test]
async fn initialiser_identity_cannot_replace_an_ordinary_application() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let task = tokio::spawn(async move { agent.run().await });
    let foreign = InstanceId("default__web-0-init-0".into());
    let reserved = InstanceId("default__web-0__init-0".into());
    let config =
        Config::parse("[app.web-0-init]\nimage = 'foreign:image'\ncommand = ['sleep', '60']\n")
            .unwrap();
    expect_complete(&send_deploy(&tx, config).await);
    grill.set_state(&reserved, ContainerState::Stopped);
    grill.set_exit_code(&reserved, Some(0));
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        send_deploy(&tx, config_with_init_container()),
    )
    .await;
    let foreign_creates = grill
        .calls()
        .iter()
        .filter(|(operation, id)| operation == "create" && id == &foreign)
        .count();
    shutdown.cancel();
    task.await.unwrap();
    expect_complete(&events.expect("initialiser reused the running application's identity"));
    assert_eq!(
        foreign_creates, 1,
        "initialiser reused an ordinary workload identity"
    );
}

#[tokio::test]
async fn uncertain_initialiser_keeps_parent_retirement_pending() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let old = InstanceId("default__web-0-init-0".into());
    let reserved = InstanceId("default__web-0__init-0".into());
    grill.set_instance_inspection_failure(&old, true);
    grill.set_instance_inspection_failure(&reserved, true);
    let task = tokio::spawn(async move { agent.run().await });
    let events = send_deploy(&tx, config_with_init_container()).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ApplyEvent::Error { .. }))
    );
    let initialiser = grill
        .calls()
        .into_iter()
        .find_map(|(operation, id)| {
            (operation == "create" && id.0 != "default__web-0").then_some(id)
        })
        .unwrap();
    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::Retire {
        app_name: "web".into(),
        namespace: "default".into(),
        response,
    })
    .await
    .unwrap();
    let first = result.await.unwrap();
    grill.set_instance_inspection_failure(&old, false);
    grill.set_instance_inspection_failure(&reserved, false);
    let (response, result) = oneshot::channel();
    tx.send(AgentCommand::Retire {
        app_name: "web".into(),
        namespace: "default".into(),
        response,
    })
    .await
    .unwrap();
    let second = result.await.unwrap();
    let stopped = grill.state(&initialiser).await.unwrap() == ContainerState::Stopped;
    shutdown.cancel();
    task.await.unwrap();
    assert!(
        first.is_err(),
        "parent retired without observing its initialiser"
    );
    assert!(second.is_ok(), "confirmed retry failed: {second:?}");
    assert!(
        stopped,
        "initialiser still owns execution after parent retirement"
    );
}

#[tokio::test]
async fn rolling_replacement_runs_initialisers_and_refuses_main_after_init_failure() {
    for exit_code in [0, 7] {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let task = tokio::spawn(async move { agent.run().await });
        let mut fresh = config_with_init_container();
        fresh.app.get_mut("web").unwrap().init.clear();
        expect_complete(&send_deploy(&tx, fresh).await);
        let main = InstanceId("default__web-g1-0".into());
        let init = InstanceId(format!("{}__init-0", main.0));
        grill.set_state(&init, ContainerState::Stopped);
        grill.set_exit_code(&init, Some(exit_code));
        let before = grill.calls().len();
        let events = send_deploy(&tx, config_with_init_container()).await;
        let calls = grill.calls()[before..].to_vec();
        let init_started = calls
            .iter()
            .position(|(op, id)| op == "start" && id == &init);
        let main_started = calls
            .iter()
            .position(|(op, id)| op == "start" && id == &main);
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            init_started.is_some(),
            "rolling replacement skipped its initialiser"
        );
        if exit_code == 0 {
            expect_complete(&events);
            assert!(main_started.is_some() && init_started < main_started);
        } else {
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ApplyEvent::Error { .. }))
            );
            assert!(
                main_started.is_none(),
                "failed init allowed the main payload to start"
            );
            assert!(
                !calls
                    .iter()
                    .any(|(op, id)| (op == "stop" || op == "kill") && id.0 == "default__web-0"),
                "failed initialiser retired the original serving workload"
            );
        }
    }
}

#[tokio::test]
async fn deploy_with_init_container_succeeds() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();

    // Pre-configure: init container exits successfully
    let init_id = InstanceId("default__web-0__init-0".to_string());
    grill.set_state(&init_id, ContainerState::Stopped);
    grill.set_exit_code(&init_id, Some(0));

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, config_with_init_container()).await;
    let (created, _instances) = expect_complete(&events);
    assert_eq!(created, 1);

    // App should reach running after successful init
    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Status { response: resp_tx })
        .await
        .unwrap();
    let statuses = resp_rx.await.unwrap();
    assert_eq!(statuses[0].state, "running");

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn deploy_with_failing_init_container_fails() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();

    // Pre-configure: init container exits with failure
    let init_id = InstanceId("default__web-0__init-0".to_string());
    grill.set_state(&init_id, ContainerState::Stopped);
    grill.set_exit_code(&init_id, Some(1));

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, config_with_init_container()).await;
    let last = events.last().expect("no events");
    assert!(
        matches!(last, ApplyEvent::Error { message } if message.contains("exited with code 1")),
        "expected an Error event naming the exit code, got {last:?}"
    );

    shutdown.cancel();
    agent_handle.await.unwrap();
}

#[tokio::test]
async fn failing_init_container_reports_the_runtimes_stderr() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let init_id = InstanceId("default__web-0__init-0".to_string());
    grill.set_state(&init_id, ContainerState::Stopped);
    grill.set_exit_code(&init_id, Some(1));
    let owner = tempfile::tempdir().unwrap();
    let stem = owner.path().join("output");
    let reason = "runc run failed: container's cgroup is not empty: 1 process(es) found";
    // Enough earlier noise that only a bounded tail can reach the error.
    let noise = "EARLY-NOISE ".repeat(1_000);
    std::fs::write(
        stem.with_extension("stderr"),
        format!("{noise}\n{reason}\n"),
    )
    .unwrap();
    grill.set_log_stem(&init_id, stem);

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });
    let events = send_deploy(&tx, config_with_init_container()).await;
    shutdown.cancel();
    agent_handle.await.unwrap();

    let message = events
        .iter()
        .find_map(|event| match event {
            ApplyEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("the failed initialiser produced no Error event");
    assert!(
        message.contains(reason),
        "the runtime's reason is missing: {message}"
    );
    assert!(
        message.contains("exited with code 1"),
        "the exit code is missing: {message}"
    );
    assert!(
        message.len() < 1_024,
        "the stderr tail is unbounded ({} bytes)",
        message.len()
    );
}

/// #701: an init container runs its own image, with the app's env and the
/// app's volume and config-file mounts. It used to run in the app's image
/// with no env and only the standard mounts, so a migration couldn't see
/// `DATABASE_URL` and a config generator couldn't write a volume.
#[tokio::test]
async fn init_container_runs_its_own_image_with_the_app_env_and_mounts() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let own = InstanceId("default__web-0__init-0".to_string());
    let inherited = InstanceId("default__web-0__init-1".to_string());
    for init in [&own, &inherited] {
        grill.set_state(init, ContainerState::Stopped);
        grill.set_exit_code(init, Some(0));
    }
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let config = Config::parse(
        r#"
            [app.web]
            image = "myapp:v1"
            env = { DATABASE_URL = "postgres://db/app" }

            [[app.web.volumes]]
            path = "/data"

            [[app.web.config_file]]
            path = "/etc/web/app.conf"
            content = "key = value"

            [[app.web.init]]
            image = "migrate:v2"
            command = ["migrate"]

            [[app.web.init]]
            command = ["echo", "inherits the app image"]
        "#,
    )
    .unwrap();
    let events = send_deploy(&tx, config).await;
    shutdown.cancel();
    agent_handle.await.unwrap();
    expect_complete(&events);

    let own = grill
        .created_spec(&own)
        .expect("the first init was created");
    assert_eq!(own.root.path, "migrate:v2");
    assert_eq!(own.process.args, vec!["migrate".to_string()]);
    assert!(
        own.process
            .env
            .contains(&"DATABASE_URL=postgres://db/app".to_string()),
        "{:?}",
        own.process.env
    );
    for destination in ["/data", "/etc/web/app.conf"] {
        assert!(
            own.mounts
                .iter()
                .any(|m| m.destination == std::path::Path::new(destination)),
            "init is missing {destination}: {:?}",
            own.mounts
        );
    }
    let inherited = grill
        .created_spec(&inherited)
        .expect("the second init was created");
    assert_eq!(inherited.root.path, "myapp:v1");
}

/// #701: each instance reads its own copy of an inline config file, and the
/// copy goes when the instance does.
#[tokio::test]
async fn stop_removes_each_instances_inline_config_files() {
    let (mut agent, tx, shutdown) = test_agent();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let config = Config::parse(
        r#"
            [app.web]
            image = "myapp:v1"
            replicas = 2

            [[app.web.config_file]]
            path = "/etc/web/app.conf"
            content = "key = value"
        "#,
    )
    .unwrap();
    expect_complete(&send_deploy(&tx, config).await);
    for instance in ["default__web-0", "default__web-1"] {
        let dir = crate::grill::oci::inline_config_dir(volumes.path(), "default", "web", instance);
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "{instance} has its own copy");
    }

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Stop {
        app_name: "web".to_string(),
        namespace: "default".to_string(),
        response: resp_tx,
    })
    .await
    .unwrap();
    resp_rx.await.unwrap().unwrap();
    shutdown.cancel();
    agent_handle.await.unwrap();

    for instance in ["default__web-0", "default__web-1"] {
        let dir = crate::grill::oci::inline_config_dir(volumes.path(), "default", "web", instance);
        assert!(!dir.exists(), "{} survived the stop", dir.display());
    }
}

/// #701: `health.port` defaults the probe's port, not the app's `port`.
/// The rollout gate used to skip the probe when the app had no `port`, so
/// a failing replacement was announced healthy.
#[tokio::test]
async fn rollout_gate_probes_health_port_without_app_port() {
    let port = spawn_health_responder(500).await;
    let (mut agent, tx, shutdown, _grill) = test_agent_with_grill();
    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let v1 = Config::parse("[app.web]\nimage = \"myapp:v1\"\n").unwrap();
    expect_complete(&send_deploy(&tx, v1).await);
    let v2 = Config::parse(&format!(
        r#"
            [app.web]
            image = "myapp:v2"

            [app.web.health]
            path = "/healthz"
            port = {port}

            [app.web.deploy]
            strategy = "rolling"
            health_timeout = "1s"
        "#
    ))
    .unwrap();
    let events = send_deploy(&tx, v2).await;
    shutdown.cancel();
    agent_handle.await.unwrap();

    let error = events
        .iter()
        .find_map(|e| match e {
            ApplyEvent::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("the gate must probe health.port and refuse the failing replacement");
    assert!(error.contains("failed its health check"), "{error}");
}

#[test]
fn tail_lines_empty_string() {
    assert_eq!(super::tail_lines("", 5), "");
}

#[test]
fn rolling_health_wait_honours_the_configured_timeout_not_a_5s_cap() {
    // M7: a configured 60s health_timeout must be used in full, not
    // clamped to 5s (which would fail a slow-starting container).
    let config = crate::meat::deploy_types::DeployConfig {
        health_timeout: std::time::Duration::from_secs(60),
        ..Default::default()
    };
    assert_eq!(
        super::deploy_worker::effective_health_wait(&config),
        std::time::Duration::from_secs(60)
    );

    let short = crate::meat::deploy_types::DeployConfig {
        health_timeout: std::time::Duration::from_secs(2),
        ..Default::default()
    };
    assert_eq!(
        super::deploy_worker::effective_health_wait(&short),
        std::time::Duration::from_secs(2),
        "a short timeout is still honoured exactly"
    );
}

#[test]
fn tail_lines_fewer_than_n() {
    assert_eq!(super::tail_lines("a\nb\n", 5), "a\nb\n");
}

#[test]
fn tail_lines_exactly_n() {
    assert_eq!(super::tail_lines("a\nb\nc\n", 3), "a\nb\nc\n");
}

#[test]
fn tail_lines_more_than_n() {
    assert_eq!(super::tail_lines("a\nb\nc\nd\n", 2), "c\nd\n");
}

#[test]
fn tail_lines_zero_returns_empty() {
    assert_eq!(super::tail_lines("a\nb\nc\n", 0), "");
}

#[test]
fn tail_lines_no_trailing_newline() {
    assert_eq!(super::tail_lines("a\nb\nc", 2), "b\nc");
}

#[test]
fn node_status_serialisation_round_trip() {
    let status = NodeStatus {
        node_id: "node-1".to_string(),
        address: "192.168.1.1:9116".to_string(),
        api_address: None,
        state: "alive".to_string(),
        incarnation: 42,
        is_council: true,
        is_leader: false,
        labels: BTreeMap::from([("zone".to_string(), "us-east-1a".to_string())]),
    };
    let json = serde_json::to_string(&status).unwrap();
    let decoded: NodeStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.node_id, "node-1");
    assert_eq!(decoded.incarnation, 42);
    assert!(decoded.is_council);
}

#[test]
fn council_status_serialisation_round_trip() {
    let status = CouncilStatus {
        members: vec![CouncilMemberInfo {
            raft_id: 1,
            name: "node-1".to_string(),
            address: "192.168.1.1:9200".to_string(),
            voter: true,
        }],
        leader: Some("node-1".to_string()),
        term: 5,
        last_applied_log: Some(42),
        app_count: 3,
        ..Default::default()
    };
    let json = serde_json::to_string(&status).unwrap();
    let decoded: CouncilStatus = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.term, 5);
    assert_eq!(decoded.leader, Some("node-1".to_string()));
    assert_eq!(decoded.members.len(), 1);
}

fn council_metrics(
    id: u64,
    voters: &[u64],
    learners: &[u64],
    state: openraft::ServerState,
) -> openraft::RaftMetrics<u64, crate::council::types::CouncilNodeInfo> {
    let nodes: std::collections::BTreeMap<u64, crate::council::types::CouncilNodeInfo> = voters
        .iter()
        .chain(learners)
        .map(|id| {
            (
                *id,
                crate::council::types::CouncilNodeInfo {
                    addr: "127.0.0.1:9444".parse().unwrap(),
                    name: format!("node-{id}"),
                },
            )
        })
        .collect();
    let membership = openraft::Membership::new(vec![voters.iter().copied().collect()], nodes);
    let mut metrics = openraft::RaftMetrics::new_initial(id);
    metrics.membership_config =
        std::sync::Arc::new(openraft::StoredMembership::new(None, membership));
    metrics.state = state;
    metrics
}

fn fence(state: crate::council::fence::FenceState) -> Option<crate::council::fence::FenceSnapshot> {
    Some(crate::council::fence::FenceSnapshot {
        epoch: 0,
        recovery_id: None,
        state,
    })
}

#[test]
fn council_role_reads_raft_state_for_a_serving_member() {
    use crate::bun::agent::{CouncilRole, council_role};
    use crate::council::fence::FenceState;
    use openraft::ServerState;
    let serving = fence(FenceState::Serving);
    let leader = council_metrics(1, &[1, 2, 3], &[], ServerState::Leader);
    assert_eq!(council_role(&leader, serving), CouncilRole::Leader);
    let follower = council_metrics(2, &[1, 2, 3], &[], ServerState::Follower);
    assert_eq!(council_role(&follower, serving), CouncilRole::Follower);
    let candidate = council_metrics(2, &[1, 2, 3], &[], ServerState::Candidate);
    assert_eq!(council_role(&candidate, serving), CouncilRole::Candidate);
    let learner = council_metrics(4, &[1, 2, 3], &[4], ServerState::Learner);
    assert_eq!(council_role(&learner, serving), CouncilRole::Learner);
    let worker = council_metrics(9, &[1, 2, 3], &[], ServerState::Learner);
    assert_eq!(council_role(&worker, None), CouncilRole::Worker);
}

#[test]
fn council_role_puts_the_fence_before_what_raft_says() {
    use crate::bun::agent::{CouncilRole, council_role};
    use crate::council::fence::FenceState;
    use openraft::ServerState;
    // A fenced old leader may still think it leads; it must say fenced.
    let stale_leader = council_metrics(1, &[1, 2, 3], &[], ServerState::Leader);
    assert_eq!(
        council_role(&stale_leader, fence(FenceState::Fenced { newer_epoch: 1 })),
        CouncilRole::Fenced
    );
    // #694: a split fences the leader of either half just the same.
    let split = FenceState::Split {
        other_id: crate::council::types::RecoveryId(7),
    };
    assert_eq!(
        council_role(&stale_leader, fence(split)),
        CouncilRole::Fenced
    );
    assert_eq!(
        council_role(&stale_leader, fence(FenceState::Probing)),
        CouncilRole::Starting
    );
}

#[tokio::test]
async fn logs_with_tail_truncates_output() {
    let (mut agent, tx, shutdown) = test_agent();

    let agent_handle = tokio::spawn(async move {
        agent.run().await;
    });

    let events = send_deploy(&tx, basic_config()).await;
    expect_complete(&events);

    let (resp_tx, resp_rx) = oneshot::channel();
    tx.send(AgentCommand::Logs {
        app_name: "web".to_string(),
        namespace: "default".to_string(),
        tail: Some(1),
        response: resp_tx,
    })
    .await
    .unwrap();
    let result = resp_rx.await.unwrap();
    // MockGrill returns empty logs, so tail of empty is still ok
    assert!(result.is_ok());

    shutdown.cancel();
    agent_handle.await.unwrap();
}

// ---- workload adoption (Phase 14) ----

fn adoption_record(
    instance: &str,
    app: &str,
    with_health: bool,
) -> crate::grill::records::InstanceRecord {
    let spec_toml = if with_health {
        "image = \"myapp:v1\"\nport = 8080\n[health]\npath = \"/health\"\n"
    } else {
        "image = \"myapp:v1\"\n"
    };
    let app_spec: AppSpec = toml::from_str(spec_toml).unwrap();
    crate::grill::records::InstanceRecord {
        schema: crate::grill::records::RECORD_SCHEMA,
        instance_id: instance.to_string(),
        namespace: "default".to_string(),
        app_name: app.to_string(),
        replica_index: 0,
        image: "myapp:v1".to_string(),
        runtime: crate::grill::records::RuntimeKind::Process,
        pid: 4242,
        pid_started_at: 1000,
        boot_id: crate::grill::records::current_boot(),
        runc_container_id: None,
        log_stem: None,
        host_port: Some(30123),
        app_spec: Some(app_spec),
        oci_spec: crate::grill::oci::OciSpec {
            reusable_executor: false,
            host_process: false,
            initialiser: false,
            port_mapping: None,
            stop: None,
            root: crate::grill::oci::OciRoot {
                path: "/tmp/test".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                rlimits: Vec::new(),
                args: vec!["sleep".to_string(), "60".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
                capabilities: None,
                oom_score_adj: None,
                overrides: None,
            },
            mounts: vec![],
            linux: crate::grill::oci::OciLinux {
                namespaces: vec![],
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
                sysctl: Default::default(),
                devices: Vec::new(),
            },
            runtime_knobs: None,
        },
        rootless_network: None,
    }
}

#[tokio::test]
async fn portless_redeploy_after_adoption_never_reuses_an_owned_generation() {
    for (runtime_id, generation) in [("default__web-g1-0", 1), ("default__web-g17-0", 17)] {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        let mut record = adoption_record(runtime_id, "web", false);
        record.host_port = None;
        crate::grill::records::write_record(records.path(), &record).unwrap();
        grill.set_adopt_result(&InstanceId(runtime_id.into()), true);
        grill.set_pid(std::process::id());
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
        // Generation continuity needs no service recovery or guessed VIP.
        let mut config = basic_config();
        config.app.get_mut("web").unwrap().port = None;
        let events = drain_deploy(&mut agent, config).await;
        let expected = format!("default__web-g{}-0", generation + 1);
        let created: Vec<_> = grill
            .calls()
            .into_iter()
            .filter(|(call, _)| call == "create")
            .map(|(_, id)| id.0)
            .collect();
        let persisted = crate::grill::records::load_records(records.path()).unwrap();
        agent.stop_app("web", "default").await.unwrap();
        expect_complete(&events);
        assert_eq!(
            created,
            vec![expected.clone()],
            "adoption must advance rollout identity"
        );
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].instance_id, expected);
    }
}

#[tokio::test]
async fn exhausted_rollout_generation_refuses_without_mutating_the_old_instance() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    agent.next_deploy_gen = u64::MAX;
    let before = grill.calls().len();
    let events = drain_deploy(&mut agent, basic_config()).await;
    let calls = grill.calls();
    agent.stop_app("web", "default").await.unwrap();
    assert!(events.iter().any(|event| matches!(event, ApplyEvent::Error { message } if message.contains("generation exhausted"))), "{events:?}");
    assert_eq!(calls.len(), before);
}

#[tokio::test]
async fn redeploying_the_same_spec_over_a_stopped_replica_runs_one_again() {
    // A retirement whose stop finished but whose address release is
    // still waiting on other nodes leaves the replica stopped and owned,
    // with its service still registered. When the leader hands the
    // placement back, the placement reconciler redeploys the identical
    // spec, and that must converge on a running replica.
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let stop = agent.begin_app_stop("web", "default").await.unwrap();
    agent.app_exit_wait(&stop).await.unwrap();
    for id in &stop.instances {
        let instance = agent.supervisor.get_instance_mut(id).unwrap();
        instance.state = instance
            .state
            .transition_to(ContainerState::Stopped)
            .unwrap();
    }
    let running = |agent: &BunAgent<MockGrill>| {
        agent
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| instance.state == ContainerState::Running)
            .count()
    };
    assert_eq!(running(&agent), 0);

    expect_complete(&drain_deploy(&mut agent, basic_config()).await);

    assert_eq!(running(&agent), 1, "the redeploy left no running replica");
}

fn web_with_replicas(replicas: u32) -> Config {
    let mut config = basic_config();
    config.app.get_mut("web").unwrap().replicas = crate::config::Replicas::Fixed(replicas);
    config
}

fn live_web_ids(agent: &BunAgent<MockGrill>) -> Vec<String> {
    let mut ids: Vec<String> = agent
        .supervisor
        .list_instances()
        .iter()
        .filter(|instance| instance.state == ContainerState::Running)
        .map(|instance| instance.id.0.clone())
        .collect();
    ids.sort();
    ids
}

/// #346: when a node dies, a survivor's share of an app grows by one.
/// The placement reconciler deploys the same spec with a higher replica
/// count, and that used to roll every replica on the node: the healthy
/// ones stopped, a new generation started, and the stopped ones lingered
/// until the dead node's view lease ran out. Only the new replica should
/// start.
#[tokio::test]
async fn raising_the_replica_count_starts_only_the_new_replicas() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, web_with_replicas(1)).await);
    let before = live_web_ids(&agent);
    assert_eq!(before.len(), 1);

    expect_complete(&drain_deploy(&mut agent, web_with_replicas(3)).await);

    let after = live_web_ids(&agent);
    assert_eq!(after.len(), 3, "{after:?}");
    assert!(
        after.contains(&before[0]),
        "the serving replica was replaced"
    );
    assert_eq!(
        agent.supervisor.list_instances().len(),
        3,
        "nothing stopped is left behind"
    );
    let stopped: Vec<_> = grill
        .calls()
        .into_iter()
        .filter(|(call, id)| (call == "stop" || call == "kill") && id.0 == before[0])
        .collect();
    assert!(stopped.is_empty(), "{stopped:?}");
    let backends = agent
        .service_map
        .resolve(&crate::onion::service_id::ServiceId::new("default", "web"))
        .map(|entry| entry.backends.len())
        .unwrap_or(0);
    assert_eq!(backends, 3, "every replica serves");

    // A changed spec still rolls.
    let mut changed = web_with_replicas(3);
    changed.app.get_mut("web").unwrap().image = Some("myapp:v2".into());
    expect_complete(&drain_deploy(&mut agent, changed).await);
    assert!(
        live_web_ids(&agent).iter().all(|id| !after.contains(id)),
        "a new image must replace every replica"
    );
}

/// This node's share of `web`, under the cluster-wide ordinals the leader
/// assigned, the way the placement reconciler hands it over (#398).
fn web_with_ordinals(ordinals: &[u32]) -> Config {
    let mut config = web_with_replicas(ordinals.len() as u32);
    config.app.get_mut("web").unwrap().ordinals = Some(ordinals.to_vec());
    config
}

/// #398: every node used to number its own replicas from 0, so three
/// replicas on three nodes were all `web-0`. An instance takes the ordinal
/// its placement was assigned, keeps it through a rolling deploy, and a
/// node whose share grows starts the new ordinal beside the old ones.
#[tokio::test]
async fn instances_are_created_with_their_assigned_ordinals() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());

    expect_complete(&drain_deploy(&mut agent, web_with_ordinals(&[2, 5])).await);
    assert_eq!(live_web_ids(&agent), ["default__web-2", "default__web-5"]);

    // A replacement for a lost replica lands here and takes its ordinal.
    expect_complete(&drain_deploy(&mut agent, web_with_ordinals(&[2, 4, 5])).await);
    assert_eq!(
        live_web_ids(&agent),
        ["default__web-2", "default__web-4", "default__web-5"]
    );

    // A rolling deploy moves every replica to a new generation under the
    // same ordinals.
    let mut changed = web_with_ordinals(&[2, 4, 5]);
    changed.app.get_mut("web").unwrap().image = Some("myapp:v2".into());
    expect_complete(&drain_deploy(&mut agent, changed).await);
    let rolled = live_web_ids(&agent);
    assert_eq!(rolled.len(), 3, "{rolled:?}");
    let ordinals: Vec<u32> = rolled
        .iter()
        .map(|id| {
            let identity = crate::grill::InstanceIdentity::parse(id).unwrap();
            assert!(identity.generation.is_some(), "{id} was not rolled");
            identity.ordinal
        })
        .collect();
    assert_eq!(ordinals, [2, 4, 5], "{rolled:?}");
}

/// The calls `grill` saw that stop or kill one of `ids`.
fn stops_of(grill: &MockGrill, ids: &[&str]) -> Vec<(String, InstanceId)> {
    grill
        .calls()
        .into_iter()
        .filter(|(call, id)| (call == "stop" || call == "kill") && ids.contains(&id.0.as_str()))
        .collect()
}

/// Whether any of the deploy's progress lines says it rolled the app.
fn rolled(events: &[ApplyEvent]) -> bool {
    events.iter().any(|event| {
        matches!(event, ApplyEvent::Progress { message } if message.contains("rolling redeploy"))
    })
}

/// #1111: when the leader moves one of a node's replicas elsewhere and
/// gives it another, the version is the same, so only the two ordinals
/// change hands. It used to roll every replica on the node to a new
/// generation of the same spec.
#[tokio::test]
async fn a_moved_replica_starts_the_arriving_ordinal_and_retires_the_departing_one() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, web_with_ordinals(&[0, 2])).await);

    let events = drain_deploy(&mut agent, web_with_ordinals(&[0, 3])).await;

    expect_complete(&events);
    assert!(!rolled(&events), "a share move must not roll: {events:?}");
    assert_eq!(live_web_ids(&agent), ["default__web-0", "default__web-3"]);
    assert_eq!(
        agent.supervisor.list_instances().len(),
        2,
        "the departing replica is gone"
    );
    assert!(stops_of(&grill, &["default__web-0"]).is_empty());
    assert!(!stops_of(&grill, &["default__web-2"]).is_empty());
}

/// #1111: a share that loses a replica stops only that one.
#[tokio::test]
async fn a_shrunk_share_retires_only_the_departing_replica() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, web_with_ordinals(&[0, 1, 2])).await);

    let events = drain_deploy(&mut agent, web_with_ordinals(&[0, 2])).await;

    expect_complete(&events);
    assert!(
        !rolled(&events),
        "a smaller share must not roll: {events:?}"
    );
    assert_eq!(live_web_ids(&agent), ["default__web-0", "default__web-2"]);
    assert!(stops_of(&grill, &["default__web-0", "default__web-2"]).is_empty());
    let backends = agent
        .service_map
        .resolve(&crate::onion::service_id::ServiceId::new("default", "web"))
        .map(|entry| entry.backends.len())
        .unwrap_or(0);
    assert_eq!(backends, 2, "the retired replica stops serving");
}

/// #1111: a node whose whole share is swapped for other ordinals of the
/// same version starts the new ones under their own ordinals, without a
/// generation.
#[tokio::test]
async fn a_swapped_share_starts_the_new_ordinals_without_a_new_generation() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, web_with_ordinals(&[0, 2])).await);

    let events = drain_deploy(&mut agent, web_with_ordinals(&[1, 3])).await;

    expect_complete(&events);
    assert!(
        !rolled(&events),
        "a swapped share must not roll: {events:?}"
    );
    assert_eq!(live_web_ids(&agent), ["default__web-1", "default__web-3"]);
    assert_eq!(agent.supervisor.list_instances().len(), 2);
}

/// #1111: a share that moves in the same deploy as a new version still
/// rolls every replica, under the new assignment.
#[tokio::test]
async fn a_moved_share_with_a_new_version_still_rolls() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, web_with_ordinals(&[0, 2])).await);

    let mut changed = web_with_ordinals(&[0, 3]);
    changed.app.get_mut("web").unwrap().image = Some("myapp:v2".into());
    let events = drain_deploy(&mut agent, changed).await;

    expect_complete(&events);
    assert!(rolled(&events), "{events:?}");
    let ordinals: Vec<u32> = live_web_ids(&agent)
        .iter()
        .map(|id| {
            let identity = crate::grill::InstanceIdentity::parse(id).unwrap();
            assert!(identity.generation.is_some(), "{id} was not rolled");
            identity.ordinal
        })
        .collect();
    assert_eq!(ordinals, [0, 3]);
}

/// `budgeted_web` under the ordinals the leader assigned this node.
fn budgeted_web_with_ordinals(image: &str, ordinals: &[u32]) -> Config {
    let mut config = budgeted_web(image, ordinals.len() as u32);
    config.app.get_mut("web").unwrap().ordinals = Some(ordinals.to_vec());
    config
}

/// #1111, from the 0.2.1 jobs soak (#1110): on a full node, a rolling
/// redeploy needs room for a new replica before an old one goes, so moving
/// a replica failed and rolled back. Retiring the departing replica first
/// frees the room the arriving one needs.
#[tokio::test]
async fn a_replica_moves_onto_a_full_node_where_a_redeploy_would_not_fit() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    grill.set_runtime_kind(crate::grill::records::RuntimeKind::Runc);
    let budget = agent.execution_budget();
    budget.set_capacity(crate::meat::Resources::new(1000, 1 << 30, 0));
    expect_complete(&drain_deploy(&mut agent, budgeted_web_with_ordinals("web:v1", &[0, 2])).await);

    let events = drain_deploy(&mut agent, budgeted_web_with_ordinals("web:v1", &[0, 3])).await;

    expect_complete(&events);
    assert_eq!(live_web_ids(&agent), ["default__web-0", "default__web-3"]);
    assert!(stops_of(&grill, &["default__web-0"]).is_empty());
    assert!(reported_refusals(&mut agent).await.is_empty());
}

#[tokio::test]
async fn redeploy_does_not_overwrite_a_stopped_or_failed_cleanup_owner() {
    for state in [ContainerState::Stopped, ContainerState::Failed] {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let old = InstanceId("default__web-0".into());
        let path = crate::grill::records::record_path(records.path(), &old.0);
        let original = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(agent.stop_app("web", "default").await.is_err());
        agent.supervisor.get_instance_mut(&old).unwrap().state = state;
        let events = drain_deploy(&mut agent, basic_config()).await;
        let owned = agent.supervisor.list_instances().len();
        let old_creates = grill
            .calls()
            .iter()
            .filter(|(call, id)| call == "create" && id == &old)
            .count();
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, original).unwrap();
        agent.retire_workload("web", "default").await.unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Complete { .. })),
            "{state}: {events:?}"
        );
        assert_eq!(owned, 2, "must retain both owners after cleanup fails");
        assert_eq!(old_creates, 1, "the original identity was created twice");
        assert_eq!(agent.supervisor.port_allocator.allocated_count().await, 0);
    }
}

#[tokio::test]
async fn rolling_replacement_persists_its_launch_spec_for_adoption() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_volumes_dir(volumes.path().to_path_buf());
    grill.set_pid(std::process::id());
    let (events, _receiver) = mpsc::channel(128);
    agent.deploy(basic_config(), &events).await;
    let mut replacement = basic_config();
    replacement.app.get_mut("web").unwrap().image = Some("web:v2".to_string());
    let (rolling_events, mut rolling_rx) = mpsc::channel(1);
    let mut deployment = Box::pin(agent.deploy(replacement, &rolling_events));
    let mut observed_during_rollout = false;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                Some(event) = rolling_rx.recv() => {
                    if let ApplyEvent::Progress { message } = event
                        && message.contains("healthy")
                    {
                        assert!(crate::grill::records::load_records(records.path()).unwrap().iter()
                            .any(|record| record.image == "web:v2"));
                        observed_during_rollout = true;
                    }
                }
                _ = &mut deployment => break,
            }
        }
    })
    .await
    .unwrap();
    drop(deployment);
    assert!(observed_during_rollout);
    let persisted = crate::grill::records::load_records(records.path()).unwrap();
    assert_eq!(
        persisted.len(),
        1,
        "the replacement must retain an adoption record"
    );
    assert_eq!(persisted[0].image, "web:v2");
    assert_eq!(
        persisted[0].app_spec.as_ref().unwrap().image.as_deref(),
        Some("web:v2")
    );
    let (mut restarted, _tx, _shutdown, runtime) = test_agent_with_grill();
    restarted.set_records_dir(records.path().to_path_buf());
    restarted.set_volumes_dir(volumes.path().to_path_buf());
    let id = InstanceId(persisted[0].instance_id.clone());
    runtime.set_adopt_result(&id, true);
    assert_eq!(restarted.adopt_recorded_instances().await.unwrap(), 1);
    assert!(restarted.supervisor.get_instance(&id).is_some());
    assert!(
        !runtime
            .calls()
            .iter()
            .any(|(operation, _)| operation == "create")
    );
}

#[tokio::test]
async fn rolling_record_failure_retains_cleanup_ownership_until_directory_recovery() {
    for strategy in ["rolling", "blue-green"] {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        grill.set_pid(std::process::id());
        let initial = drain_deploy(&mut agent, basic_config()).await;
        assert!(matches!(initial.last(), Some(ApplyEvent::Complete { .. })));
        let allocated_before = agent.supervisor.port_allocator.allocated_count().await;
        let blocked = tempfile::NamedTempFile::new().unwrap();
        agent.set_records_dir(blocked.path().to_path_buf());
        let replacement = Config::parse(&format!(
                "[app.web]\nimage = \"web:v2\"\nport = 8080\n[app.web.deploy]\nstrategy = \"{strategy}\"\n"
            )).unwrap();
        let outcome = drain_deploy(&mut agent, replacement).await;
        assert!(outcome.iter().any(|event| matches!(event, ApplyEvent::Error { message } if message.contains("persist replacement record"))), "{outcome:?}");
        assert!(
            agent
                .supervisor
                .get_instance(&InstanceId("default__web-0".into()))
                .is_some()
        );
        assert_eq!(
            agent.supervisor.port_allocator.allocated_count().await,
            allocated_before + 1
        );
        let created: Vec<_> = grill
            .calls()
            .into_iter()
            .filter(|(op, id)| op == "create" && id.0 != "default__web-0")
            .collect();
        assert_eq!(created.len(), 1);
        let owner = agent.supervisor.get_instance(&created[0].1).unwrap();
        assert_eq!(owner.state, ContainerState::Stopped);
        agent.set_records_dir(records.path().to_path_buf());
        agent
            .finish_retire_bookkeeping(&created[0].1)
            .await
            .unwrap();
        assert_eq!(
            agent.supervisor.port_allocator.allocated_count().await,
            allocated_before
        );
        assert!(agent.supervisor.get_instance(&created[0].1).is_none());
        assert!(
            agent
                .supervisor
                .get_instance(&InstanceId("default__web-0".into()))
                .is_some()
        );

        assert!(
            grill
                .calls()
                .contains(&("kill".to_string(), created[0].1.clone()))
        );
    }
}

#[tokio::test]
async fn apple_launches_persist_records_without_a_host_workload_pid() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_volumes_dir(volumes.path().to_path_buf());
    grill.set_runtime_kind(crate::grill::records::RuntimeKind::Apple);
    let (events, _receiver) = mpsc::channel(128);
    agent.deploy(basic_config(), &events).await;
    assert_eq!(
        crate::grill::records::load_records(records.path())
            .unwrap()
            .len(),
        1
    );
    agent.deploy(basic_config(), &events).await;
    let saved = crate::grill::records::load_records(records.path()).unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].runtime, crate::grill::records::RuntimeKind::Apple);
    assert_eq!(saved[0].pid, std::process::id());
    assert!(saved[0].instance_id.contains("-g"));
}

#[tokio::test]
async fn started_rootless_instance_persists_network_recreation_state() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_volumes_dir(volumes.path().to_path_buf());
    grill.set_runtime_kind(crate::grill::records::RuntimeKind::Runc);
    grill.set_pid(std::process::id());
    let rootless_network = crate::grill::records::RootlessNetworkRecord {
        api_socket: records.path().join("slirp4netns.sock"),
        owner_pid: 4243,
        owner_pid_started_at: 1001,
        container_pid: 4244,
        port_mapping: Some(crate::grill::oci::PortMapping {
            host_port: 30000,
            container_port: 8080,
        }),
    };
    grill.set_rootless_network(rootless_network.clone());

    let (events, mut event_rx) = mpsc::channel(64);
    agent.deploy(basic_config(), &events).await;
    drop(events);
    while event_rx.recv().await.is_some() {}

    let persisted = crate::grill::records::load_records(records.path()).unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].schema, crate::grill::records::RECORD_SCHEMA);
    assert_eq!(persisted[0].boot_id, crate::grill::records::current_boot());
    assert_eq!(persisted[0].rootless_network, Some(rootless_network));
}

/// An adoption record whose workload has a cgroup and resource limits, as
/// a runc instance's has.
pub(super) fn limited_adoption_record(
    instance: &str,
    app: &str,
) -> crate::grill::records::InstanceRecord {
    let mut record = adoption_record(instance, app, false);
    record.oci_spec.linux.cgroups_path = Some(format!("/reliaburger/default/{app}/0"));
    record.oci_spec.linux.resources = Some(crate::grill::oci::OciResources {
        pids: None,
        cpu: Some(crate::grill::oci::OciCpuResources {
            quota: 50_000,
            period: 100_000,
        }),
        memory: Some(crate::grill::oci::OciMemoryResources {
            limit: 128 * 1024 * 1024,
        }),
        unified: std::collections::BTreeMap::from([(
            "memory.high".to_string(),
            "33554432".to_string(),
        )]),
        devices: Vec::new(),
    });
    record
}

/// Whether `ps` reports `pid` as stopped (state `T`).
pub(super) fn process_is_stopped(pid: u32) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with('T')
}

/// Wait up to five seconds for `pid` to be stopped, or running.
pub(super) async fn wait_for_stopped(pid: u32, stopped: bool) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while process_is_stopped(pid) != stopped {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pid {pid} never became stopped={stopped}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Bun keeps a CPU fault's saved `cpu.max` only in memory. A Bun that was
/// killed mid-fault left the cap behind, and the next one adopted the
/// instance without lifting it (#699): the recorded specification says
/// what the limits should be, so adoption writes them back.
#[tokio::test]
async fn adopting_an_instance_restores_its_recorded_cpu_max() {
    let records = tempfile::tempdir().unwrap();
    let mount = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_cgroup_mount(mount.path().to_path_buf());
    let record = limited_adoption_record("default__web-0", "web");
    let cgroup = mount.path().join("reliaburger/default/web/0");
    std::fs::create_dir_all(&cgroup).unwrap();
    // What a 99% CPU fault, a memory fault and a disk-I/O fault left.
    std::fs::write(cgroup.join("cpu.max"), "1000 100000").unwrap();
    std::fs::write(cgroup.join("memory.high"), "1024").unwrap();
    std::fs::write(
        cgroup.join("io.max"),
        "8:0 rbps=1048576 wbps=1048576 riops=max wiops=max\n",
    )
    .unwrap();
    crate::grill::records::write_record(records.path(), &record).unwrap();
    grill.set_adopt_result(&InstanceId(record.instance_id.clone()), true);

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let read = |name: &str| std::fs::read_to_string(cgroup.join(name)).unwrap();
    assert_eq!(read("cpu.max"), "50000 100000");
    assert_eq!(read("memory.high"), "33554432");
    assert_eq!(read("io.max"), "8:0 rbps=max wbps=max riops=max wiops=max");
}

/// A pause fault's frozen pids lived only in the old Bun's registry, so an
/// instance paused when Bun died stayed frozen for good (#699).
#[tokio::test]
async fn adopting_an_instance_sends_sigcont_to_its_pids() {
    let records = tempfile::tempdir().unwrap();
    let mount = tempfile::tempdir().unwrap();
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_cgroup_mount(mount.path().to_path_buf());
    let mut main = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let mut worker = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    for child in [&main, &worker] {
        crate::smoker::process::pause_process(child.id() as i32).unwrap();
        wait_for_stopped(child.id(), true).await;
    }
    // The runtime names the main process; the cgroup lists the rest.
    grill.set_pid(main.id());
    let record = limited_adoption_record("default__web-0", "web");
    let cgroup = mount.path().join("reliaburger/default/web/0");
    std::fs::create_dir_all(&cgroup).unwrap();
    std::fs::write(cgroup.join("cgroup.procs"), format!("{}\n", worker.id())).unwrap();
    crate::grill::records::write_record(records.path(), &record).unwrap();
    grill.set_adopt_result(&InstanceId(record.instance_id.clone()), true);

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    wait_for_stopped(main.id(), false).await;
    wait_for_stopped(worker.id(), false).await;
    for child in [&mut main, &mut worker] {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[tokio::test]
async fn adoption_refuses_conflicting_port_ownership() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    for app in ["web", "api"] {
        let record = adoption_record(&format!("default__{app}-0"), app, false);
        crate::grill::records::write_record(records.path(), &record).unwrap();
        grill.set_adopt_result(&InstanceId(record.instance_id), true);
    }
    assert!(agent.adopt_recorded_instances().await.is_err());
    assert_eq!(agent.supervisor.list_instances().len(), 1);
    assert_eq!(
        crate::grill::records::load_records(records.path())
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn adoption_refuses_a_different_runtime_without_touching_the_record() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let mut record = adoption_record("default__web-0", "web", false);
    record.runtime = crate::grill::records::RuntimeKind::Runc;
    crate::grill::records::write_record(records.path(), &record).unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    assert!(agent.adopt_recorded_instances().await.is_err());
    assert!(grill.calls().is_empty());
    assert_eq!(
        crate::grill::records::load_records(records.path()).unwrap(),
        vec![record]
    );
}

#[tokio::test]
async fn adoption_retains_dead_owner_record_until_identity_cleanup_succeeds() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    let record = adoption_record("default__web-0", "web", false);
    crate::grill::records::write_record(records.path(), &record).unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_volumes_dir(volumes.path().to_path_buf());
    let identity =
        crate::sesame::identity::instance_identity_dir(volumes.path(), &record.instance_id);
    std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
    std::fs::write(&identity, b"not a directory").unwrap();
    assert!(agent.adopt_recorded_instances().await.is_err());
    assert!(crate::grill::records::record_path(records.path(), &record.instance_id).exists());
    std::fs::remove_file(identity).unwrap();
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
    assert!(
        crate::grill::records::load_records(records.path())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn uncertain_adoption_preserves_the_record_and_identity_for_retry() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    let record = adoption_record("default__web-0", "web", false);
    crate::grill::records::write_record(records.path(), &record).unwrap();
    write_test_identity(volumes.path(), &record.instance_id);
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_volumes_dir(volumes.path().to_path_buf());
    grill.set_fail_state(true);
    assert!(agent.adopt_recorded_instances().await.is_err());
    assert!(
        crate::grill::records::record_path(records.path(), &record.instance_id).exists(),
        "uncertain runtime inspection discarded durable ownership"
    );
    let identity =
        crate::sesame::identity::instance_identity_dir(volumes.path(), &record.instance_id);
    assert!(
        crate::sesame::identity::load_identity(&identity)
            .unwrap()
            .is_some(),
        "uncertain runtime inspection swept a surviving owner's identity"
    );
    grill.set_fail_state(false);
    grill.set_adopt_result(&InstanceId(record.instance_id.clone()), true);
    agent.adopt_recorded_instances().await.unwrap();
    assert!(
        agent
            .supervisor
            .get_instance(&InstanceId(record.instance_id))
            .is_some()
    );
}

#[tokio::test]
async fn corrupt_adoption_record_never_sweeps_its_workload_identity() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    let instance = "default__web-0";
    std::fs::write(
        records.path().join(format!("{instance}.json")),
        b"{incomplete",
    )
    .unwrap();
    write_test_identity(volumes.path(), instance);
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_volumes_dir(volumes.path().to_path_buf());
    assert!(agent.adopt_recorded_instances().await.is_err());
    let identity = crate::sesame::identity::instance_identity_dir(volumes.path(), instance);
    assert!(
        crate::sesame::identity::load_identity(&identity)
            .unwrap()
            .is_some(),
        "unreadable ownership was incorrectly treated as absence"
    );
}

#[tokio::test]
async fn a_retired_stale_record_does_not_stop_the_other_instances_adopting() {
    // #607: the runtime retires a generation whose record names another
    // process; startup carries on with everything else on the node.
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    for (index, app) in ["frontend", "redis"].into_iter().enumerate() {
        let mut record = adoption_record(&format!("default__{app}-0"), app, false);
        record.host_port = Some(30123 + index as u16);
        crate::grill::records::write_record(records.path(), &record).unwrap();
    }
    let stale = InstanceId("default__frontend-0".into());
    let healthy = InstanceId("default__redis-0".into());
    grill.set_adopt_result(&stale, false);
    grill.set_adopt_result(&healthy, true);

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
    assert!(agent.supervisor.get_instance(&stale).is_none());
    assert_eq!(
        agent.supervisor.get_instance(&healthy).unwrap().state,
        ContainerState::Running
    );
    let kept: Vec<_> = crate::grill::records::load_records(records.path())
        .unwrap()
        .into_iter()
        .map(|record| record.instance_id)
        .collect();
    assert_eq!(kept, vec![healthy.0]);
}

#[tokio::test]
async fn startup_adopts_recorded_instances_instead_of_restarting() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let dir = tempfile::tempdir().unwrap();
    let record = adoption_record("default__web-0", "web", false);
    crate::grill::records::write_record(dir.path(), &record).unwrap();
    agent.set_records_dir(dir.path().to_path_buf());

    let id = InstanceId("default__web-0".to_string());
    grill.set_adopt_result(&id, true);

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Running);
    assert_eq!(instance.app_name, "web");
    // Adopted, never created or started by this process.
    let calls = grill.calls();
    assert!(calls.contains(&("adopt".to_string(), id.clone())));
    assert!(!calls.contains(&("create".to_string(), id.clone())));
    assert!(!calls.contains(&("start".to_string(), id)));
}

#[tokio::test]
async fn adoption_refuses_unsupported_or_inconsistent_identities_before_mutation() {
    for (instance, app, namespace, ordinal) in [
        ("web-0", "web", "default", 0),
        ("web-g9-0", "web", "default", 0),
        ("default__web-0", "other", "default", 0),
        ("default__web-0", "web", "other", 0),
        ("default__web-0", "web", "default", 1),
        ("default__Bad-0", "Bad", "default", 0),
        ("bad__namespace__web-0", "web", "bad__namespace", 0),
        ("default__web-g01-0", "web", "default", 0),
        ("payments__web-0", "web", "payments", 0),
    ] {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let mut record = adoption_record(instance, app, false);
        record.namespace = namespace.into();
        record.replica_index = ordinal;
        if namespace == "payments" {
            record.app_spec.as_mut().unwrap().namespace = Some("other".into());
        }
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());
        let runtime_id = InstanceId(instance.into());
        grill.set_adopt_result(&runtime_id, true);
        let result = agent.adopt_recorded_instances().await;
        assert!(result.is_err(), "accepted {record:?}: {result:?}");
        assert!(
            grill.calls().is_empty(),
            "invalid ownership reached the runtime"
        );
        assert!(agent.supervisor.instances.is_empty());
        assert_eq!(
            crate::grill::records::load_records(dir.path()).unwrap(),
            vec![record]
        );
    }
}

#[tokio::test]
async fn adoption_uses_structured_names_to_validate_generation_like_suffixes() {
    for instance in ["default__worker-g9-0", "default__worker-g9-g17-0"] {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let record = adoption_record(instance, "worker-g9", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());
        let id = InstanceId(instance.into());
        grill.set_adopt_result(&id, true);
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
        let owner = agent.supervisor.get_instance(&id).unwrap();
        assert_eq!(owner.app_name, "worker-g9");
        assert_eq!(owner.namespace, "default");
        assert_eq!(
            crate::grill::records::load_records(dir.path()).unwrap(),
            vec![record]
        );
    }
}

#[tokio::test]
async fn startup_deletes_stale_records_and_reschedules() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    let dir = tempfile::tempdir().unwrap();
    // MockGrill declines adoption by default (dead process).
    let record = adoption_record("default__web-0", "web", false);
    crate::grill::records::write_record(dir.path(), &record).unwrap();
    agent.set_records_dir(dir.path().to_path_buf());
    agent.set_volumes_dir(dir.path().join("volumes"));

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);

    // The stale record is gone and nothing was seeded: the normal
    // reconcile path is free to reschedule the instance.
    assert!(
        crate::grill::records::load_records(dir.path())
            .unwrap()
            .is_empty()
    );
    assert!(
        agent
            .supervisor
            .get_instance(&InstanceId("default__web-0".to_string()))
            .is_none()
    );
}

/// Write a real identity bundle into `volumes/.identity/{instance}`
/// and return it, so adoption tests have on-disk state to restore.
fn write_test_identity(
    volumes: &std::path::Path,
    instance: &str,
) -> crate::sesame::types::WorkloadIdentity {
    let uri = crate::sesame::types::SpiffeUri {
        trust_domain: "default".to_string(),
        namespace: "default".to_string(),
        workload_type: crate::sesame::types::WorkloadType::App,
        name: "web".to_string(),
    };
    let hierarchy =
        crate::sesame::ca::generate_ca_hierarchy("default", b"test-ikm-32-bytes!").unwrap();
    let (csr_der, private_key_der) = crate::sesame::identity::create_workload_csr(&uri).unwrap();
    let cert_der = crate::sesame::identity::validate_and_sign_csr(
        &csr_der,
        &uri,
        crate::sesame::types::SerialNumber(42),
        crate::sesame::identity::CertUsage::Mtls,
        &hierarchy.workload.signing_keypair,
        &hierarchy.workload.certificate_params,
        SystemTime::now(),
    )
    .unwrap();
    let identity = crate::sesame::identity::build_identity_bundle(
        uri,
        cert_der,
        private_key_der,
        &[
            hierarchy.workload.ca.certificate_der.clone(),
            hierarchy.root.ca.certificate_der.clone(),
        ],
        "adopted-jwt".to_string(),
    );
    let dir = crate::sesame::identity::instance_identity_dir(volumes, instance);
    crate::sesame::identity::write_identity_files(&identity, &dir, None).unwrap();
    identity
}

/// D9: adoption rebuilds the identity and its rotation schedule from
/// the per-instance directory — no `identity: None`, no fresh CSR.
/// The restored schedule means the next rotation fires exactly when
/// the pre-restart one would have.
#[tokio::test]
async fn adoption_restores_identity_and_rotation_schedule_from_disk() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    agent.set_records_dir(records.path().to_path_buf());

    let written = write_test_identity(volumes.path(), "default__web-0");
    let record = adoption_record("default__web-0", "web", false);
    crate::grill::records::write_record(records.path(), &record).unwrap();
    let id = InstanceId("default__web-0".to_string());
    grill.set_adopt_result(&id, true);

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let instance = agent.supervisor.get_instance(&id).unwrap();
    let restored = instance
        .identity
        .as_ref()
        .expect("adopted instance keeps its identity");
    assert_eq!(restored.spiffe_uri, written.spiffe_uri);
    assert_eq!(restored.private_key_der, written.private_key_der);
    assert_eq!(
        restored.next_rotation, written.next_rotation,
        "the rotation schedule is the disk one, not a fresh clock"
    );
    assert_eq!(
        instance.identity_mount.as_deref(),
        Some(
            crate::sesame::identity::instance_identity_dir(volumes.path(), "default__web-0")
                .as_path()
        )
    );

    // The rotation loop fires on the restored schedule: fresh now,
    // then due once the recorded next_rotation passes.
    assert_eq!(
        crate::sesame::identity::rotation_state(restored, written.issued_at),
        crate::sesame::identity::RotationState::Valid
    );
    assert_eq!(
        crate::sesame::identity::rotation_state(
            restored,
            written.next_rotation + std::time::Duration::from_secs(1)
        ),
        crate::sesame::identity::RotationState::NeedsRotation
    );
}

/// PKI7: identity directories with no live owner (an instance that died
/// while bun was down) are swept at adoption, so stale key material
/// never lingers.
#[tokio::test]
async fn adoption_sweeps_orphaned_identity_dirs() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    agent.set_records_dir(records.path().to_path_buf());

    // A live instance's dir and a dead instance's leftovers.
    write_test_identity(volumes.path(), "default__web-0");
    let dead = volumes.path().join(".identity/old-app-0");
    std::fs::create_dir_all(&dead).unwrap();
    std::fs::write(dead.join("key.pem"), b"dead key").unwrap();

    let record = adoption_record("default__web-0", "web", false);
    crate::grill::records::write_record(records.path(), &record).unwrap();
    let id = InstanceId("default__web-0".to_string());
    grill.set_adopt_result(&id, true);

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    assert_eq!(
        identity_dir_names(volumes.path()),
        vec!["default__web-0".to_string()],
        "only the adopted instance's identity dir survives"
    );
}

#[tokio::test]
async fn adopted_instances_resume_health_checks() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let dir = tempfile::tempdir().unwrap();
    let record = adoption_record("default__web-0", "web", true);
    crate::grill::records::write_record(dir.path(), &record).unwrap();
    agent.set_records_dir(dir.path().to_path_buf());

    let id = InstanceId("default__web-0".to_string());
    grill.set_adopt_result(&id, true);
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert!(instance.health_config.is_some());
}

#[tokio::test]
async fn adopted_instance_port_is_reserved() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let dir = tempfile::tempdir().unwrap();
    let record = adoption_record("default__web-0", "web", false);
    crate::grill::records::write_record(dir.path(), &record).unwrap();
    agent.set_records_dir(dir.path().to_path_buf());

    let id = InstanceId("default__web-0".to_string());
    grill.set_adopt_result(&id, true);
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

    // The adopted instance's port must not be handed out again.
    assert!(agent.supervisor.port_allocator.is_allocated(30123).await);
}

#[tokio::test]
async fn adoption_never_clobbers_a_tracked_instance() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let dir = tempfile::tempdir().unwrap();
    agent.set_records_dir(dir.path().to_path_buf());

    // Deploy an instance in THIS process, then drop a record for the
    // same id (as if left behind) — adoption must skip it.
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(basic_config(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let id = InstanceId("default__web-0".to_string());
    assert!(agent.supervisor.get_instance(&id).is_some());
    let record = adoption_record("default__web-0", "web", false);
    crate::grill::records::write_record(dir.path(), &record).unwrap();
    grill.set_adopt_result(&id, true);

    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
}

// ---------------------------------------------------------------------
// DEP6: exit-aware stop.
// ---------------------------------------------------------------------

/// A stop must SIGTERM, wait for the runtime to confirm exit, and record
/// Stopped only then. If the process ignores SIGTERM the stop escalates
/// to SIGKILL rather than lying that the app is down.
#[tokio::test]
async fn stop_escalates_to_kill_when_process_ignores_sigterm() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    // Escalation, not the length of the grace, is under test here;
    // `stop_reports_stopped_after_exit_without_kill` keeps the default.
    agent.set_stop_grace(std::time::Duration::from_millis(200));

    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(basic_config(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    // The workload refuses SIGTERM: stop() records the call but leaves the
    // state Running. The exit-aware stop must therefore kill().
    grill.set_ignore_stop(true);
    let id = InstanceId("default__web-0".to_string());
    grill.set_state(&id, ContainerState::Running);

    agent.stop_app("web", "default").await.unwrap();

    let calls = grill.calls();
    assert!(
        calls.iter().any(|(op, i)| op == "stop" && i == &id),
        "stop must SIGTERM first"
    );
    assert!(
        calls.iter().any(|(op, i)| op == "kill" && i == &id),
        "stop must escalate to SIGKILL when the process ignores SIGTERM"
    );
}

/// `runc kill` on a loaded host can take seconds to answer. The default
/// confirmation deadline must wait that out rather than report an
/// unconfirmed stop and leave the workload owned for another retry.
#[tokio::test(start_paused = true)]
async fn force_kill_waits_out_a_slow_runtime_within_the_default_deadline() {
    let grill = MockGrill::new();
    let id = InstanceId("default__web-0".to_string());
    grill.set_state(&id, ContainerState::Running);
    grill.set_kill_delay(Some(std::time::Duration::from_secs(5)));

    let deadline = crate::config::node::RuntimeSection::default().stop_confirmation_timeout();
    kill_runtime_instance(&grill, &id, deadline).await.unwrap();

    assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
}

/// A runtime slower than the configured deadline still yields an
/// unconfirmed stop, so ownership is kept rather than guessed away.
#[tokio::test(start_paused = true)]
async fn force_kill_is_unconfirmed_when_the_runtime_outlasts_the_deadline() {
    let grill = MockGrill::new();
    let id = InstanceId("default__web-0".to_string());
    grill.set_state(&id, ContainerState::Running);
    grill.set_kill_delay(Some(std::time::Duration::from_secs(5)));

    let error = kill_runtime_instance(&grill, &id, std::time::Duration::from_secs(2))
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            BunError::StopUnconfirmed {
                reason: "force-kill request timed out",
                ..
            }
        ),
        "expected an unconfirmed force-kill, got {error:?}"
    );
}

/// The agent's kill path uses the configured deadline, not a constant:
/// a kill that outlasts a short configured deadline is unconfirmed.
#[tokio::test]
async fn kill_uses_the_configured_confirmation_deadline() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let id = InstanceId("default__web-0".to_string());
    grill.set_state(&id, ContainerState::Running);
    grill.set_kill_delay(Some(std::time::Duration::from_millis(500)));
    agent.set_stop_confirmation_timeout(std::time::Duration::from_millis(50));

    let error = agent.kill_and_wait_for_exit(&id).await.unwrap_err();

    assert!(
        error.to_string().contains("force-kill request timed out"),
        "expected the configured deadline to expire, got {error}"
    );
}

/// A cooperative stop reports Stopped once the runtime confirms exit, and
/// does not needlessly escalate to kill.
#[tokio::test]
async fn stop_reports_stopped_after_exit_without_kill() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());

    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(basic_config(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    agent.stop_app("web", "default").await.unwrap();

    // The instance is recorded Stopped, and stop did not need to
    // force-kill a cooperative process.
    let id = InstanceId("default__web-0".to_string());
    assert_eq!(
        agent.supervisor.get_instance(&id).map(|i| i.state),
        Some(ContainerState::Stopped),
        "stopped instance should be recorded Stopped after exit"
    );
    let calls = grill.calls();
    assert!(
        calls.iter().any(|(op, i)| op == "stop" && i == &id),
        "stop must SIGTERM"
    );
    assert!(
        !calls.iter().any(|(op, i)| op == "kill" && i == &id),
        "a cooperative stop must not escalate to SIGKILL"
    );
}

// ---------------------------------------------------------------------
// DEP5: drain / surge / max_unavailable.
// ---------------------------------------------------------------------

/// A retire waits for an in-flight request (tracked through the shared
/// drain tracker, as the live Wrapper proxy would report it) to finish
/// before the old container is killed.
#[tokio::test]
async fn retire_waits_for_in_flight_request_before_kill() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(config_with_health(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let id = InstanceId("default__web-0".to_string());
    let drains = agent.drains_handle();

    // Simulate the proxy holding an in-flight request open on the backend
    // that is about to be retired: start the drain and bump its count.
    drains
        .start_drain(&crate::wrapper::draining::DrainCommand {
            app_name: "web".to_string(),
            instance_id: id.0.clone(),
            timeout: std::time::Duration::from_secs(30),
        })
        .await;
    drains.increment_connections(&id.0).await;

    // Kick off the retire on a task; it must block on the drain.
    let stop = InstanceStop::for_policy(None, std::time::Duration::from_secs(30));
    let retire = drain_and_stop_instance(
        &drains,
        &grill,
        &id,
        std::time::Duration::from_secs(30),
        &stop,
        std::time::Duration::from_secs(10),
    );
    tokio::pin!(retire);

    // While the request is in flight, the retire has not killed anything.
    let early = tokio::time::timeout(std::time::Duration::from_millis(200), &mut retire).await;
    assert!(
        early.is_err(),
        "retire finished before the in-flight request drained"
    );
    assert!(
        !grill.calls().iter().any(|(op, i)| op == "kill" && i == &id),
        "old instance killed while a request was still in flight"
    );

    // The request finishes: the drain completes and the retire proceeds.
    drains.decrement_connections(&id.0).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut retire)
        .await
        .expect("retire did not complete after the request drained")
        .unwrap();
    let calls = grill.calls();
    assert!(
        calls.iter().any(|(op, i)| op == "stop" && i == &id),
        "retire must stop the drained instance"
    );
}

/// ING4: a retire waits for an in-flight *WebSocket* splice, not just a
/// plain HTTP request. The WebSocket bumps both counters; the HTTP part of
/// the splice finishes first, but the live WebSocket must keep the drain
/// open until it closes, so the old container isn't killed mid-splice.
#[tokio::test]
async fn retire_waits_for_in_flight_websocket_before_kill() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(config_with_health(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let id = InstanceId("default__web-0".to_string());
    let drains = agent.drains_handle();

    // The proxy would bump both counters at the 101 for a WebSocket.
    drains
        .start_drain(&crate::wrapper::draining::DrainCommand {
            app_name: "web".to_string(),
            instance_id: id.0.clone(),
            timeout: std::time::Duration::from_secs(30),
        })
        .await;
    drains.increment_connections(&id.0).await;
    drains.increment_websocket(&id.0).await;

    let stop = InstanceStop::for_policy(None, std::time::Duration::from_secs(30));
    let retire = drain_and_stop_instance(
        &drains,
        &grill,
        &id,
        std::time::Duration::from_secs(30),
        &stop,
        std::time::Duration::from_secs(10),
    );
    tokio::pin!(retire);

    // The HTTP half of the splice completes, but the WebSocket is still
    // open, so the retire must not proceed.
    drains.decrement_connections(&id.0).await;
    let early = tokio::time::timeout(std::time::Duration::from_millis(200), &mut retire).await;
    assert!(
        early.is_err(),
        "retire finished while a WebSocket splice was still open"
    );
    assert!(
        !grill.calls().iter().any(|(op, i)| op == "kill" && i == &id),
        "old instance killed while a WebSocket was still spliced"
    );

    // The WebSocket closes: the drain completes and the retire proceeds.
    drains.decrement_websocket(&id.0).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut retire)
        .await
        .expect("retire did not complete after the WebSocket closed")
        .unwrap();
    assert!(
        grill.calls().iter().any(|(op, i)| op == "stop" && i == &id),
        "retire must stop the drained instance once the WebSocket closed"
    );
}

/// A rolling redeploy with `max_unavailable = 1` surges the new instances
/// up before retiring the old, so the serving-instance count never drops
/// below `replicas - max_unavailable`. With surge-first, the old instance
/// is only stopped after the new one is healthy.
#[tokio::test]
async fn rolling_redeploy_never_drops_below_target_availability() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

    let config = Config::parse(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 1\n\n[app.web.deploy]\nmax_unavailable = 1\ndrain_timeout = \"1s\"\n",
        )
        .unwrap();

    // Fresh deploy.
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config.clone(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let calls_before = grill.calls().len();

    // Redeploy: rolling path. The new instance is created and started
    // before the old one is stopped/killed.
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

    // The first "start" of a new (gen-tagged) instance must come before the
    // first "stop"/"kill" of the old default__web-0 — surge-first ordering.
    let first_new_start = calls
        .iter()
        .position(|(op, i)| op == "start" && i.0.contains("-g") && i.0.starts_with("default__web"));
    let first_old_retire = calls
        .iter()
        .position(|(op, i)| (op == "stop" || op == "kill") && i.0 == "default__web-0");
    assert!(
        first_new_start.is_some(),
        "rolling redeploy never started a new instance"
    );
    assert!(
        first_old_retire.is_some(),
        "rolling redeploy never retired the old instance"
    );
    assert!(
        first_new_start < first_old_retire,
        "old instance was retired before the new one started — availability dropped below target"
    );
}

/// M7: `max_surge` bounds how many containers exist at once during a
/// rollout. It used to parse, validate and change nothing — the rollout
/// started every replacement and only then retired every old instance, so
/// a 3-replica app peaked at 6 containers whatever the config said.
///
/// Replay the grill's call log to reconstruct how many instances were live
/// at each moment, and assert the peak.
#[tokio::test]
async fn rolling_redeploy_honours_max_surge() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

    // 3 replicas, default max_surge = 1, max_unavailable = 0.
    let config = Config::parse(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 3\n\n[app.web.deploy]\nmax_surge = 1\nmax_unavailable = 0\ndrain_timeout = \"0s\"\n",
        )
        .unwrap();

    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config.clone(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let calls_before = grill.calls().len();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}
    let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

    // Replay: a `start` adds a live instance, a `stop`/`kill` removes one.
    // Three old instances are live when the rollout begins.
    let mut live: std::collections::HashSet<String> =
        (0..3).map(|i| format!("default__web-{i}")).collect();
    let mut peak = live.len();
    for (op, id) in &calls {
        match op.as_str() {
            "start" => {
                live.insert(id.0.clone());
                peak = peak.max(live.len());
            }
            "stop" | "kill" => {
                live.remove(&id.0);
            }
            _ => {}
        }
    }

    assert_eq!(
        peak, 4,
        "peaked at {peak} live instances; max_surge = 1 on 3 replicas allows 4 \
             (the old behaviour peaked at 6)"
    );
}

/// The mirror: `max_surge = 0` with `max_unavailable = 1` must never
/// exceed the replica target, retiring before replacing.
#[tokio::test]
async fn rolling_redeploy_honours_zero_surge() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

    let config = Config::parse(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 2\n\n[app.web.deploy]\nmax_surge = 0\nmax_unavailable = 1\ndrain_timeout = \"0s\"\n",
        )
        .unwrap();

    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config.clone(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let calls_before = grill.calls().len();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}
    let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

    let mut live: std::collections::HashSet<String> =
        (0..2).map(|i| format!("default__web-{i}")).collect();
    let mut peak = live.len();
    for (op, id) in &calls {
        match op.as_str() {
            "start" => {
                live.insert(id.0.clone());
                peak = peak.max(live.len());
            }
            "stop" | "kill" => {
                live.remove(&id.0);
            }
            _ => {}
        }
    }

    assert_eq!(
        peak, 2,
        "peaked at {peak}; max_surge = 0 must never exceed the 2-replica target"
    );
}

/// Replay a grill call log and return the most instances of `app` that
/// were live at once, starting from `initially_live`.
fn peak_live_instances(
    calls: &[(String, InstanceId)],
    app_prefix: &str,
    initially_live: &[&str],
) -> usize {
    let mut live: std::collections::HashSet<String> =
        initially_live.iter().map(|id| id.to_string()).collect();
    let mut peak = live.len();
    for (op, id) in calls {
        if !id.0.starts_with(app_prefix) {
            continue;
        }
        match op.as_str() {
            "start" => {
                live.insert(id.0.clone());
                peak = peak.max(live.len());
            }
            "stop" | "kill" => {
                live.remove(&id.0);
            }
            _ => {}
        }
    }
    peak
}

/// V02 soak, 28 Sep 2026: after a power cut mid-upgrade, node 2 redeployed
/// the writer with the default rolling bounds. The replacement
/// (`soak-writer-g1-0`) started on the same managed volume while the old
/// instance was still appending, so two processes wrote `/data/seq` at
/// once and the file got `21926` twice. A managed volume has one writer:
/// an app that has one must retire the old instance before its
/// replacement starts, whatever `max_surge` says.
#[tokio::test]
async fn rolling_redeploy_of_a_volume_app_never_overlaps_writers() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

    let config = Config::parse(
            "[app.db]\nimage = \"db:v1\"\n\n[[app.db.volumes]]\npath = \"/data\"\n\n[app.db.deploy]\ndrain_timeout = \"0s\"\n",
        )
        .unwrap();

    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config.clone(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}
    assert!(
        grill
            .calls()
            .iter()
            .any(|(op, id)| op == "start" && id.0 == "default__db-0"),
        "the first deploy never started the app"
    );

    let calls_before = grill.calls().len();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    let mut errors = Vec::new();
    while let Some(event) = ev_rx.recv().await {
        if let ApplyEvent::Error { message } = event {
            errors.push(message);
        }
    }
    assert!(errors.is_empty(), "redeploy failed: {errors:?}");
    let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

    assert!(
        calls
            .iter()
            .any(|(op, id)| op == "start" && id.0.starts_with("default__db-g")),
        "the redeploy never started a replacement: {calls:?}"
    );
    let peak = peak_live_instances(&calls, "default__db", &["default__db-0"]);
    assert_eq!(
        peak, 1,
        "two instances of a managed-volume app ran at once, both writing the \
             same volume: {calls:?}"
    );
}

/// Blue-green stands the whole new fleet up beside the old one, which
/// for a managed volume means two writers. A volume app rolls
/// stop-first instead.
#[tokio::test]
async fn blue_green_volume_app_redeploys_stop_first() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

    let config = Config::parse(
            "[app.db]\nimage = \"db:v1\"\n\n[[app.db.volumes]]\npath = \"/data\"\n\n[app.db.deploy]\nstrategy = \"blue-green\"\ndrain_timeout = \"0s\"\nhealth_timeout = \"1s\"\n",
        )
        .unwrap();

    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config.clone(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let calls_before = grill.calls().len();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}
    let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

    assert!(
        calls
            .iter()
            .any(|(op, id)| op == "start" && id.0.starts_with("default__db-g")),
        "the redeploy never started a replacement: {calls:?}"
    );
    let peak = peak_live_instances(&calls, "default__db", &["default__db-0"]);
    assert_eq!(peak, 1, "blue and green both ran on one volume: {calls:?}");
}

/// A host-path volume is the operator's to share, so an app with only
/// that keeps its configured surge-first rollout.
#[tokio::test]
async fn host_path_volume_app_keeps_surge_first_rollout() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let shared = tempfile::tempdir().unwrap();
    agent.set_host_path_policy(crate::bun::host_paths::HostPathPolicy::new(
        vec![shared.path().to_path_buf()],
        Vec::new(),
    ));

    let config = Config::parse(&format!(
            "[app.web]\nimage = \"web:v1\"\n\n[[app.web.volumes]]\npath = \"/srv\"\nsource = \"{}\"\n\n[app.web.deploy]\ndrain_timeout = \"0s\"\n",
            shared.path().display()
        ))
        .unwrap();

    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config.clone(), &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let calls_before = grill.calls().len();
    let (ev_tx, mut ev_rx) = mpsc::channel(256);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}
    let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

    let peak = peak_live_instances(&calls, "default__web", &["default__web-0"]);
    assert_eq!(peak, 2, "surge-first rollout changed: {calls:?}");
}

/// A deploy config with no room to move in either direction is refused at
/// validation rather than wedging a live rollout (M7).
#[test]
fn both_deploy_bounds_zero_is_rejected_at_validation() {
    let config = Config::parse(
            "[app.web]\nimage = \"web:v1\"\nreplicas = 2\n\n[app.web.deploy]\nmax_surge = 0\nmax_unavailable = 0\n",
        )
        .unwrap();
    let error = config
        .validate()
        .expect_err("both bounds at zero must not validate");
    let message = error.to_string();
    assert!(
        message.contains("max_surge") && message.contains("max_unavailable"),
        "unhelpful error: {message}"
    );
}

// -- Smoker effects and cleanup (CHAOS1) ----------------------------------

fn fault_rule(fault_type: crate::smoker::types::FaultType) -> crate::smoker::types::FaultRule {
    crate::smoker::types::FaultRule::new(
        crate::smoker::types::FaultId(1),
        fault_type,
        "web".into(),
        std::time::Duration::from_secs(30),
        "test".into(),
    )
}

fn register_fault(
    agent: &mut BunAgent<MockGrill>,
    fault_type: crate::smoker::types::FaultType,
    duration: std::time::Duration,
) -> crate::smoker::types::FaultRule {
    agent
        .fault_registry
        .insert(&crate::smoker::types::FaultRequest {
            workload: Default::default(),
            task_index: None,
            fault_type,
            target_service: String::new(),
            namespace: None,
            target_instance: None,
            target_node: Some("node-a".to_string()),
            duration,
            injected_by: "test".to_string(),
            reason: Some("node fault test".to_string()),
            include_leader: false,
            override_safety: false,
            acknowledged: true,
        })
}

#[tokio::test]
async fn dns_fault_refuses_unknown_namespace_or_instance_scope_without_recording_it() {
    let (mut agent, _tx, _shutdown) = test_agent();
    for (namespace, target_instance) in [(None, None), (Some("red".into()), Some("redis-0".into()))]
    {
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::InjectFault {
                reservation: None,
                replica_evidence: None,
                request: crate::smoker::types::FaultRequest {
                    workload: Default::default(),
                    task_index: None,
                    fault_type: crate::smoker::types::FaultType::DnsNxdomain,
                    target_service: "redis".into(),
                    namespace,
                    target_instance,
                    target_node: None,
                    duration: std::time::Duration::from_secs(60),
                    injected_by: "test".into(),
                    reason: None,
                    include_leader: false,
                    override_safety: false,
                    acknowledged: true,
                },
                response,
            })
            .await;
        assert!(result.await.unwrap().is_err());
        assert_eq!(agent.fault_registry.iter().count(), 0);
    }
}

#[tokio::test]
async fn workload_fault_without_a_namespace_is_refused_before_recording() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let (response, result) = oneshot::channel();
    agent
        .handle_command(AgentCommand::InjectFault {
            reservation: None,
            replica_evidence: None,
            request: crate::smoker::types::FaultRequest {
                workload: Default::default(),
                task_index: None,
                fault_type: crate::smoker::types::FaultType::Pause,
                target_service: "web".into(),
                namespace: None,
                target_instance: None,
                target_node: None,
                duration: std::time::Duration::from_secs(60),
                injected_by: "test".into(),
                reason: None,
                include_leader: false,
                override_safety: false,
                acknowledged: true,
            },
            response,
        })
        .await;
    let error = result.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("require a namespace"), "{error}");
    assert_eq!(agent.fault_registry.iter().count(), 0);
}

#[tokio::test]
async fn dns_fault_keeps_its_namespace_and_each_owner_until_clear() {
    use crate::onion::dns::{BoundDnsResponder, DnsConfig};
    let (mut agent, _tx, shutdown) = test_agent();
    let mut map = crate::onion::service_map::ServiceMap::new();
    map.register_app("redis", "red", 6379, None).unwrap();
    map.register_app("redis", "blue", 6379, None).unwrap();
    let (_map_tx, map_rx) = tokio::sync::watch::channel(map);
    let responder = BoundDnsResponder::bind(DnsConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        ..Default::default()
    })
    .await
    .unwrap();
    let address = responder.local_addr().unwrap();
    let task = tokio::spawn(responder.run(map_rx, agent.dns_faults_watch(), shutdown.clone()));
    async fn query(address: std::net::SocketAddr, name: &str) -> u8 {
        let mut packet = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.extend_from_slice(&[0, 0, 1, 0, 1]);
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.send_to(&packet, address).await.unwrap();
        let mut answer = [0; 1500];
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            socket.recv_from(&mut answer),
        )
        .await
        .unwrap()
        .unwrap();
        answer[3] & 0xf
    }
    let mut owners = Vec::new();
    for _ in 0..2 {
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::InjectFault {
                reservation: None,
                replica_evidence: None,
                request: crate::smoker::types::FaultRequest {
                    workload: Default::default(),
                    task_index: None,
                    fault_type: crate::smoker::types::FaultType::DnsNxdomain,
                    target_service: "redis".into(),
                    namespace: Some("red".into()),
                    target_instance: None,
                    target_node: None,
                    duration: std::time::Duration::from_secs(60),
                    injected_by: "test".into(),
                    reason: None,
                    include_leader: false,
                    override_safety: false,
                    acknowledged: true,
                },
                response,
            })
            .await;
        owners.push(result.await.unwrap().unwrap().id);
    }
    assert_eq!(query(address, "redis.red.internal").await, 3);
    assert_eq!(query(address, "redis.blue.internal").await, 0);
    for (index, id) in owners.into_iter().enumerate() {
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::ClearFault {
                fault_id: id,
                allow_workload_fault: true,
                allow_node_fault: false,
                allow_node_pressure: false,
                response,
            })
            .await;
        result.await.unwrap().unwrap();
        assert_eq!(
            query(address, "redis.red.internal").await,
            if index == 0 { 3 } else { 0 }
        );
        assert_eq!(query(address, "redis.blue.internal").await, 0);
    }
    shutdown.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn node_fault_fence_reverses_before_acknowledging_and_blocks_late_activation() {
    use crate::smoker::{
        reservation::NodeFaultReservation,
        types::{FaultRequest, FaultType},
    };
    let (mut agent, gate, _) = test_cluster_fault_agent().await;
    let request = FaultRequest {
        workload: Default::default(),
        task_index: None,
        fault_type: FaultType::NodeKill {
            kill_containers: false,
        },
        target_service: String::new(),
        namespace: None,
        target_instance: None,
        target_node: Some("node-a".into()),
        duration: std::time::Duration::from_secs(30),
        injected_by: "operator".into(),
        reason: None,
        include_leader: true,
        override_safety: true,
        acknowledged: true,
    };
    let mut grant = NodeFaultReservation {
        sequence: 1,
        boot_id: agent.node_fault_fence.boot_id.clone(),
        cleanup_after_unix_ms: 30_000,
        request: request.clone(),
    };
    assert!(agent.fence_node_fault(&grant, true).await.is_err());
    let (response, result) = oneshot::channel();
    agent
        .handle_command(AgentCommand::InjectFault {
            reservation: Some(Box::new(grant.clone())),
            replica_evidence: None,
            request: request.clone(),
            response,
        })
        .await;
    result.await.unwrap().unwrap();
    assert!(gate.is_quiesced());
    assert!(agent.fence_node_fault(&grant, true).await.is_err());
    agent.fence_node_fault(&grant, false).await.unwrap();
    assert!(!gate.is_quiesced());
    assert!(agent.fault_registry.iter().next().is_none());
    let (response, result) = oneshot::channel();
    agent
        .handle_command(AgentCommand::InjectFault {
            reservation: Some(Box::new(grant.clone())),
            replica_evidence: None,
            request: request.clone(),
            response,
        })
        .await;
    assert!(result.await.unwrap().is_err());
    assert!(!gate.is_quiesced());
    let old = grant.clone();
    grant.sequence = 2;
    let (response, result) = oneshot::channel();
    agent
        .handle_command(AgentCommand::InjectFault {
            reservation: Some(Box::new(grant.clone())),
            replica_evidence: None,
            request,
            response,
        })
        .await;
    result.await.unwrap().unwrap();
    agent.fence_node_fault(&old, false).await.unwrap();
    assert!(
        gate.is_quiesced(),
        "an old fence must not reverse a later operation"
    );
    agent.fence_node_fault(&grant, false).await.unwrap();
    assert!(!gate.is_quiesced());
}

#[tokio::test]
async fn clearing_a_reserved_node_fault_reports_its_reservation_until_fenced() {
    use crate::smoker::{
        reservation::NodeFaultReservation,
        types::{FaultRequest, FaultType},
    };
    let (mut agent, gate, _) = test_cluster_fault_agent().await;
    let request = FaultRequest {
        workload: Default::default(),
        task_index: None,
        fault_type: FaultType::NodeKill {
            kill_containers: false,
        },
        target_service: String::new(),
        namespace: None,
        target_instance: None,
        target_node: Some("node-a".into()),
        duration: std::time::Duration::from_secs(30),
        injected_by: "operator".into(),
        reason: None,
        include_leader: true,
        override_safety: true,
        acknowledged: true,
    };
    let grant = NodeFaultReservation {
        sequence: 7,
        boot_id: agent.node_fault_fence.boot_id.clone(),
        cleanup_after_unix_ms: 30_000,
        request: request.clone(),
    };
    let (response, result) = oneshot::channel();
    agent
        .handle_command(AgentCommand::InjectFault {
            reservation: Some(Box::new(grant.clone())),
            replica_evidence: None,
            request,
            response,
        })
        .await;
    let fault_id = result.await.unwrap().unwrap().id;
    let clear = async |agent: &mut BunAgent<MockGrill>| {
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::ClearFault {
                fault_id,
                allow_workload_fault: false,
                allow_node_fault: true,
                allow_node_pressure: false,
                response,
            })
            .await;
        result.await.unwrap().unwrap()
    };

    // The API waits on this sequence, so "cleared" can mean the cluster
    // has released the slot, not just that this node reopened its gate.
    assert_eq!(clear(&mut agent).await.reservation, Some(7));
    assert!(!gate.is_quiesced());
    assert_eq!(
        clear(&mut agent).await.reservation,
        Some(7),
        "a retried clear must keep waiting until the leader fences the grant"
    );
    agent.fence_node_fault(&grant, true).await.unwrap();
    assert_eq!(clear(&mut agent).await.reservation, None);
}

#[tokio::test]
async fn node_drain_stops_scheduling_but_keeps_cluster_transports() {
    let (mut agent, gate, readiness) = test_cluster_fault_agent().await;
    let rule = register_fault(
        &mut agent,
        crate::smoker::types::FaultType::NodeDrain,
        std::time::Duration::from_secs(30),
    );

    agent.apply_fault(&rule).await.unwrap();
    assert!(!gate.is_quiesced(), "drain must keep gossip and Raft alive");
    assert!(
        !readiness.snapshot().await.ready,
        "drain must withdraw scheduler readiness"
    );

    let stored = agent.fault_registry.get(rule.id).cloned().unwrap();
    agent.reverse_fault(&stored).await;
    assert!(readiness.snapshot().await.ready);
}

#[tokio::test]
async fn node_kill_quiesces_all_cluster_transports_and_restores() {
    let (mut agent, gate, _readiness) = test_cluster_fault_agent().await;
    let rule = register_fault(
        &mut agent,
        crate::smoker::types::FaultType::NodeKill {
            kill_containers: false,
        },
        std::time::Duration::from_secs(30),
    );

    agent.apply_fault(&rule).await.unwrap();
    assert!(gate.is_quiesced());

    let stored = agent.fault_registry.get(rule.id).cloned().unwrap();
    agent.reverse_fault(&stored).await;
    assert!(!gate.is_quiesced());
}

#[tokio::test]
async fn node_fault_expiry_restores_the_transport_gate() {
    let (mut agent, gate, _) = test_cluster_fault_agent().await;
    let rule = register_fault(
        &mut agent,
        crate::smoker::types::FaultType::NodeKill {
            kill_containers: false,
        },
        std::time::Duration::from_millis(1),
    );
    agent.apply_fault(&rule).await.unwrap();
    assert!(gate.is_quiesced());
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    agent.expire_faults().await;
    assert!(agent.fault_registry.is_empty());
    assert!(!gate.is_quiesced());
}

#[tokio::test]
async fn expired_fault_records_fault_expired() {
    let (mut agent, _gate, _) = test_cluster_fault_agent().await;
    let dir = tempfile::tempdir().unwrap();
    let log = crate::bun::audit::AuditLog::open(
        dir.path().join("audit"),
        crate::bun::audit::AuditLimits::from_total(1 << 20),
    )
    .unwrap();
    let mut store = crate::bun::events::EventStore::new();
    store.set_audit_log(log.clone());
    let events = Arc::new(tokio::sync::RwLock::new(store));
    agent.set_event_store(Arc::clone(&events));
    let rule = register_fault(
        &mut agent,
        crate::smoker::types::FaultType::NodeKill {
            kill_containers: false,
        },
        std::time::Duration::from_millis(1),
    );
    agent.apply_fault(&rule).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    agent.expire_faults().await;

    let ring = events.read().await.recent(10, None, None);
    let expired = ring
        .iter()
        .find(|event| event.action.as_deref() == Some("fault.expired"))
        .expect("expiry is in the event ring");
    assert_eq!(expired.principal.as_deref(), Some("system"));
    assert_eq!(expired.details.get("fault_id"), Some(&rule.id.to_string()));
    // The agent loop doesn't wait for the disk, so the durable copy lands
    // shortly after.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let filter = crate::bun::audit::AuditFilter {
            action: Some("fault.expired".to_string()),
            ..crate::bun::audit::AuditFilter::default()
        };
        if log.query(filter).await.unwrap().len() == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fault.expired never reached the audit log"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn node_fault_refuses_without_a_duration() {
    let (mut agent, _gate, _readiness) = test_cluster_fault_agent().await;
    let rule = register_fault(
        &mut agent,
        crate::smoker::types::FaultType::NodeKill {
            kill_containers: false,
        },
        std::time::Duration::ZERO,
    );

    let error = agent
        .apply_fault(&rule)
        .await
        .expect_err("node faults must always be reversible by a deadline");
    assert!(error.contains("duration"));
}

#[tokio::test]
async fn node_pressure_refuses_when_server_limits_are_disabled() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let rule = register_fault(
        &mut agent,
        crate::smoker::types::FaultType::NodePressure {
            cpu_percentage: 80,
            memory_percentage: 90,
        },
        std::time::Duration::from_secs(30),
    );
    let error = agent
        .apply_fault(&rule)
        .await
        .expect_err("pressure must not claim success while server limits are zero");
    assert!(error.contains("configured maximum of 0%"), "{error}");
}

#[tokio::test]
async fn node_fault_clear_needs_explicit_node_authorisation() {
    let (mut agent, gate, _readiness) = test_cluster_fault_agent().await;
    let rule = register_fault(
        &mut agent,
        crate::smoker::types::FaultType::NodeKill {
            kill_containers: false,
        },
        std::time::Duration::from_secs(30),
    );
    agent.apply_fault(&rule).await.unwrap();

    let (response, result) = oneshot::channel();
    agent
        .handle_command(AgentCommand::ClearFault {
            fault_id: rule.id.0,
            allow_workload_fault: false,
            allow_node_fault: false,
            allow_node_pressure: false,
            response,
        })
        .await;
    assert!(result.await.unwrap().is_err());
    assert!(agent.fault_registry.get(rule.id).is_some());
    assert!(gate.is_quiesced());

    let (response, result) = oneshot::channel();
    agent
        .handle_command(AgentCommand::ClearFault {
            fault_id: rule.id.0,
            allow_workload_fault: false,
            allow_node_fault: true,
            allow_node_pressure: false,
            response,
        })
        .await;
    assert!(result.await.unwrap().is_ok());
    assert!(agent.fault_registry.get(rule.id).is_none());
    assert!(!gate.is_quiesced());
}

#[tokio::test]
async fn service_partition_without_ebpf_is_refused_not_recorded_as_success() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let rule = fault_rule(crate::smoker::types::FaultType::Partition {
        source_app: Some("web".to_string()),
    });
    let error = agent
        .apply_fault(&rule)
        .await
        .expect_err("partition must not claim success without a loaded eBPF path");
    assert!(error.contains("eBPF data path"), "{error}");
}

#[tokio::test]
async fn delay_without_runc_namespaces_and_bandwidth_are_refused_honestly() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let delay = fault_rule(crate::smoker::types::FaultType::Delay {
        delay_ns: 10_000_000,
        jitter_ns: 0,
        source_app: None,
    });
    let error = agent.apply_fault(&delay).await.unwrap_err();
    assert!(
        error.contains("runc runtime") || error.contains("Linux traffic control"),
        "{error}"
    );

    let bandwidth = fault_rule(crate::smoker::types::FaultType::Bandwidth {
        bytes_per_sec: 125_000,
    });
    let error = agent.apply_fault(&bandwidth).await.unwrap_err();
    assert!(error.contains("not implemented"), "{error}");
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn resource_faults_reject_off_linux() {
    // Off Linux there are no cgroups, so a resource fault reports an
    // honest error instead of recording a fake success.
    let (mut agent, _tx, _shutdown) = test_agent();
    let rule = fault_rule(crate::smoker::types::FaultType::CpuStress {
        percentage: 80,
        cores: None,
    });
    let err = agent
        .apply_fault(&rule)
        .await
        .expect_err("cpu stress must reject without cgroups");
    assert!(err.contains("Linux cgroups"), "unexpected reason: {err}");
}

/// Reversing a Pause fault SIGCONTs the frozen process.
///
/// We freeze a real child with SIGSTOP, then drive `reverse_fault` with
/// the same `Pause` reversal the apply path records. If reversal resumes
/// the process it exits and `waitpid` reaps it; if it doesn't, the child
/// stays stopped and the bounded wait never sees an exit — the test fails
/// on the assertion, not a sleep.
#[cfg(unix)]
#[tokio::test]
async fn clearing_a_pause_resumes_the_process() {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::Pid;

    // A child that exits immediately once it's allowed to run. We reap it
    // via `waitpid` below rather than `Child::wait`, so drop the handle's
    // reaping responsibility to avoid the double-wait clippy flags.
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .spawn()
        .expect("spawn child");
    let pid = child.id() as i32;
    std::mem::forget(child);
    let nix_pid = Pid::from_raw(pid);

    // Freeze it before it can finish.
    crate::smoker::process::pause_process(pid).expect("pause");

    let mut rule = fault_rule(crate::smoker::types::FaultType::Pause);
    rule.reversal = crate::smoker::types::FaultReversal::Pause(vec![pid]);

    let (mut agent, _tx, _shutdown) = test_agent();
    agent.reverse_fault(&rule).await;

    // Bounded observable wait: poll waitpid until the resumed child exits.
    let mut exited = false;
    for _ in 0..200 {
        match waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => {
                exited = true;
                break;
            }
            Ok(WaitStatus::StillAlive) | Ok(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            Err(_) => break,
        }
    }
    assert!(
        exited,
        "paused process was never resumed by reverse_fault — it stayed frozen"
    );
}

/// A Pause fault with no reversal recorded (e.g. cleared before it ever
/// applied) is a no-op, not a panic.
#[cfg(unix)]
#[tokio::test]
async fn reversing_a_pause_without_state_is_a_noop() {
    let (mut agent, _tx, _shutdown) = test_agent();
    let rule = fault_rule(crate::smoker::types::FaultType::Pause);
    // reversal defaults to None; must not panic or error.
    agent.reverse_fault(&rule).await;
}

/// M1: the replica-minimum rail must run even with no cluster handle. The
/// old `build_safety_context` returned `None` there, so `InjectFault`
/// skipped safety entirely and `fault kill --count 0` could take out a
/// service's last replica. With a locally-known replica count the rail
/// fires and the fault is rejected.
#[tokio::test]
async fn kill_all_is_refused_for_the_last_replica_without_a_cluster() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();

    // One running replica of "web".
    let config =
        Config::parse("[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 1\n").unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    // `--count 0` means "all replicas"; killing all of a single-replica
    // service leaves zero survivors.
    let request = crate::smoker::types::FaultRequest {
        workload: Default::default(),
        task_index: None,
        fault_type: crate::smoker::types::FaultType::Kill { count: 0 },
        target_service: "web".into(),
        namespace: Some("default".into()),
        target_instance: None,
        target_node: None,
        duration: std::time::Duration::from_secs(30),
        injected_by: "test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
        acknowledged: false,
    };
    let context = agent.build_safety_context(&request, None).await;
    let check = crate::smoker::safety::evaluate_safety(&request, &context);
    assert!(
        !check.approved,
        "killing the last replica must be refused even with no cluster handle"
    );
    assert!(matches!(
        check.violation,
        Some(crate::smoker::types::SafetyViolation::ReplicaMinimum { .. })
    ));
}

/// Z2.1: a routed kill of the only replica this node holds is judged
/// against the cluster-wide count the API gathered, not the local one.
#[tokio::test]
async fn cluster_replica_evidence_replaces_the_local_count() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    let config =
        Config::parse("[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 1\n").unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    agent.deploy(config, &ev_tx).await;
    drop(ev_tx);
    while ev_rx.recv().await.is_some() {}

    let request = crate::smoker::types::FaultRequest {
        workload: Default::default(),
        task_index: None,
        fault_type: crate::smoker::types::FaultType::Kill { count: 1 },
        target_service: "web".into(),
        namespace: Some("default".into()),
        target_instance: None,
        target_node: None,
        duration: std::time::Duration::from_secs(0),
        injected_by: "test".into(),
        reason: None,
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    };
    let local = agent.build_safety_context(&request, None).await;
    assert!(!crate::smoker::safety::evaluate_safety(&request, &local).approved);

    let evidence = crate::smoker::types::ReplicaEvidence {
        replicas: 3,
        faulted_replicas: 0,
    };
    let cluster = agent.build_safety_context(&request, Some(evidence)).await;
    assert_eq!(cluster.target_service_replicas, 3);
    assert!(crate::smoker::safety::evaluate_safety(&request, &cluster).approved);
}

#[tokio::test]
async fn application_restart_retires_predecessor_artifacts_before_successor_create() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_volumes_dir(volumes.path().to_path_buf());
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let record = crate::grill::records::record_path(records.path(), &id.0);
    let identity = agent.instance_identity_dir(&id);
    std::fs::write(identity.join("old-generation"), b"old identity material").unwrap();
    let instance = agent.supervisor.get_instance_mut(&id).unwrap();
    instance.state = ContainerState::Pending;
    instance.restart_count = 1;
    grill.block_creates();
    let task = tokio::spawn(async move {
        agent.drive_pending_restarts_to_completion().await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), grill.wait_for_creates(1))
        .await
        .unwrap();
    let predecessor_record_retired = !record.exists();
    let logical_identity_retained = identity.join("old-generation").exists();
    let successor_identity_prepared = identity.is_dir();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    crate::sesame::identity::cleanup_identity_dir(&identity).unwrap();
    assert!(
        predecessor_record_retired,
        "successor creation retained the predecessor adoption record"
    );
    assert!(
        logical_identity_retained,
        "automatic restart discarded the logical workload identity"
    );
    assert!(
        successor_identity_prepared,
        "successor creation has no identity mount source"
    );
}

#[tokio::test]
async fn application_restart_refuses_successor_creation_until_artifact_cleanup_succeeds() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_records_dir(records.path().to_path_buf());
    agent.set_volumes_dir(volumes.path().to_path_buf());
    grill.set_pid(std::process::id());
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = InstanceId("default__web-0".into());
    let record = crate::grill::records::record_path(records.path(), &id.0);
    let identity = agent.instance_identity_dir(&id);
    let original = std::fs::read(&record).unwrap();
    std::fs::remove_file(&record).unwrap();
    std::fs::create_dir(&record).unwrap();
    let instance = agent.supervisor.get_instance_mut(&id).unwrap();
    instance.state = ContainerState::Pending;
    instance.restart_count = 1;
    agent.drive_pending_restarts_to_completion().await;
    let creates = grill
        .calls()
        .iter()
        .filter(|(operation, instance)| operation == "create" && instance == &id)
        .count();
    let predecessor_retained = record.exists();
    let pending = agent.supervisor.get_instance(&id).unwrap().state == ContainerState::Pending;
    std::fs::remove_dir(&record).unwrap();
    std::fs::write(&record, original).unwrap();
    assert_eq!(
        creates, 1,
        "a successor was created despite failed artifact cleanup"
    );
    assert!(
        predecessor_retained && pending,
        "restart lost the predecessor cleanup obligation"
    );
    agent.drive_pending_restarts_to_completion().await;
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running
    );
    assert!(record.exists() && identity.is_dir());
    agent.retire_workload("web", "default").await.unwrap();
}

#[tokio::test]
async fn rollout_generations_have_independent_cgroup_paths() {
    for strategy in ["rolling", "blue-green"] {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        grill.set_pid(std::process::id());
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let previous = agent.supervisor.list_instances()[0]
            .oci_spec
            .as_ref()
            .unwrap()
            .linux
            .cgroups_path
            .clone();
        let replacement = Config::parse(&format!(
            "[app.web]\nimage = 'web:v2'\nport = 8080\n[app.web.deploy]\nstrategy = '{strategy}'\n"
        ))
        .unwrap();
        expect_complete(&drain_deploy(&mut agent, replacement).await);
        let current = agent.supervisor.list_instances()[0]
            .oci_spec
            .as_ref()
            .unwrap()
            .linux
            .cgroups_path
            .clone();
        agent.retire_workload("web", "default").await.unwrap();
        assert!(previous.is_some() && current.is_some());
        assert_ne!(
            previous, current,
            "{strategy} reused its predecessor's cgroup"
        );
    }
}
/// Z6.7: the leader may stop waiting for a node that has been silent past
/// its view lease. That's only safe if the node has stopped routing to
/// other nodes by then. Its own backends are different: only this agent
/// can release their addresses, so they keep serving through a lapse, and
/// the agent refuses to release one while any view it published names it.
#[tokio::test]
async fn a_lapsed_view_lease_keeps_local_backends_until_the_leader_answers() {
    use crate::bun::consumer_owners::{ConsumerIdentity, ConsumerPhase};
    use crate::onion::service_id::ServiceId;
    let root = tempfile::tempdir().unwrap();
    let identity = ConsumerIdentity {
        node_id: crate::meat::NodeId::new("test"),
        cluster_identity: [42; 32],
    };
    let (mut agent, _, _) = test_cluster_fault_agent().await;
    agent.set_records_dir(root.path().to_owned());
    let local = InstanceId("default__web-0".into());
    let execution = crate::grill::RuntimeExecution {
        instance_id: local.clone(),
        generation: crate::grill::RuntimeGeneration::process("original"),
    };
    let spec: crate::grill::OciSpec = serde_json::from_value(serde_json::json!({
        "root": {"path": "/fixture", "readonly": true},
        "process": {"args": ["/app"], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
        "mounts": [], "linux": {"namespaces": []},
    }))
    .unwrap();
    agent
        .supervisor
        .grill()
        .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
            instance_id: local.clone(),
            generation: execution.generation.clone(),
            spec,
            network_reference: None,
        }])
        .await;
    let (mut catalog, ingress) = cluster_publication_fixture();
    let web = ServiceId::new("default", "web");
    let with_web = crate::onion::catalog::EndpointCatalog::rebuild(
        catalog
            .services
            .iter()
            .map(|(qualified, service)| {
                (
                    ServiceId::parse(qualified).unwrap(),
                    service.port,
                    service.backends.clone(),
                )
            })
            .chain([(
                web.clone(),
                8080,
                vec![crate::onion::catalog::CatalogBackend {
                    execution: Some(execution),
                    node_id: "test".into(),
                    node_ip: "192.168.1.1".parse().unwrap(),
                    host_port: 30002,
                    healthy: true,
                }],
            )]),
    )
    .unwrap();
    catalog = with_web;
    let vip = catalog.resolve(&web).unwrap().vip;
    let lease = agent.view_lease_handle();
    assert!(lease.is_valid(), "a standalone view never lapses");
    agent
        .recover_consumer_ownership(&root.path().join("discovery"), identity)
        .await
        .unwrap();
    assert!(!lease.is_valid(), "nothing routes before the first answer");
    // The instance this node runs, as adoption would register it.
    let own = agent.local_backend(&local, &web, Some("10.0.2.2".parse().unwrap()), 30002, true);
    agent.service_map = crate::onion::service_map::ServiceMap::from_snapshot(&[
        crate::onion::types::ServiceEntry {
            app_name: "web".into(),
            namespace: "default".into(),
            namespace_id: crate::onion::vip::name_to_id("default"),
            app_id: u32::from(vip.0),
            vip,
            port: 8080,
            backends: vec![own.clone()],
            firewall_allow_from: None,
        },
    ])
    .unwrap();

    let answer = |generation, response| AgentCommand::SyncClusterConsumer {
        generation,
        catalog: Box::new(catalog.clone()),
        ingress: ingress.clone(),
        withdrawals: vec![],
        requested_at_ns: crate::onion::lease::boot_clock_ns(),
        response,
    };
    let backends = |agent: &BunAgent<MockGrill>, app: &str| {
        agent
            .service_map_tx
            .borrow()
            .resolve(&ServiceId::new("default", app))
            .map(|entry| entry.backends.clone())
    };
    let (response, reply) = oneshot::channel();
    agent.handle_command(answer(1, response)).await;
    agent.finish_consumer_syncs().await;
    assert!(reply.await.unwrap().unwrap().published);
    assert!(lease.is_valid(), "publishing the leader's answer renews it");
    assert_eq!(backends(&agent, "web"), Some(vec![own.clone()]));
    assert_eq!(backends(&agent, "remote").unwrap().len(), 1);

    // The leader stops answering for longer than the lease.
    lease.expire();
    agent.fence_lapsed_view().await.unwrap();
    assert_eq!(
        backends(&agent, "web"),
        Some(vec![own.clone()]),
        "this node's own backend keeps serving"
    );
    assert_eq!(
        backends(&agent, "remote"),
        Some(vec![]),
        "another node's backend stops"
    );
    assert_eq!(agent.consumer_owner().unwrap().phase, ConsumerPhase::Active);
    let routes = agent.routing_table.read().await.list_routes();
    assert!(routes.iter().all(|route| route.healthy_backends == 0));
    assert!(
        matches!(
            agent.confirm_producer_release(&local).await,
            Err(BunError::ProducerReleasePending { .. })
        ),
        "a routed local address must not be released"
    );

    // A local change still reaches the local view, but never remote ones.
    agent.service_map.remove_backend(&web, &local.0).unwrap();
    agent.consumer_view_stale = true;
    agent.refresh_consumer_view().await.unwrap();
    assert_eq!(backends(&agent, "web"), Some(vec![]));
    assert_eq!(backends(&agent, "remote"), Some(vec![]));

    // The next answer, even for the same catalogue, restores the rest.
    let (response, reply) = oneshot::channel();
    agent.handle_command(answer(1, response)).await;
    agent.finish_consumer_syncs().await;
    assert!(reply.await.unwrap().unwrap().published);
    assert!(lease.is_valid());
    assert_eq!(agent.consumer_owner().unwrap().phase, ConsumerPhase::Active);
    assert_eq!(backends(&agent, "remote").unwrap().len(), 1);
    assert_eq!(backends(&agent, "web"), Some(vec![]));
}

#[tokio::test]
async fn durable_consumer_waits_for_http_and_websocket_release_then_recovers_receipt_retry() {
    use crate::bun::consumer_owners::ConsumerIdentity;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("discovery");
    let identity = ConsumerIdentity {
        node_id: crate::meat::NodeId::new("test"),
        cluster_identity: [42; 32],
    };
    let (mut agent, _, _) = test_cluster_fault_agent().await;
    agent.set_records_dir(root.path().to_owned());
    agent.supervisor.grill().set_launch_inventory(vec![]).await;
    agent
        .recover_consumer_ownership(&path, identity.clone())
        .await
        .unwrap();
    let (catalog, ingress) = cluster_publication_fixture();
    let result = agent
        .synchronise_consumer(1, catalog.clone(), ingress.clone(), vec![])
        .await
        .unwrap();
    assert!(result.published && result.receipts.is_empty());
    let backend = agent.service_map_tx.borrow().resolve_all()[0].backends[0]
        .instance_id
        .clone();
    let http = agent
        .drains
        .capture_requests(std::slice::from_ref(&backend), false)
        .await
        .unwrap();
    let websocket = agent
        .drains
        .capture_requests(std::slice::from_ref(&backend), true)
        .await
        .unwrap();
    let instruction = crate::onion::withdrawal::EndpointWithdrawalInstruction {
        generation: 1,
        services: catalog
            .services
            .iter()
            .map(|(id, service)| {
                (
                    id.clone(),
                    crate::onion::withdrawal::ServiceWithdrawal {
                        service: service.clone(),
                        retire_vip: true,
                    },
                )
            })
            .collect(),
    };
    let result = agent
        .synchronise_consumer(2, Default::default(), vec![], vec![instruction.clone()])
        .await
        .unwrap();
    // The new view publishes at once; the withdrawn backend drains gracefully.
    assert!(result.published && result.receipts.is_empty());
    assert!(
        http.iter()
            .chain(&websocket)
            .all(|token| !token.is_cancelled()),
        "withdrawal cancelled requests before their drain deadline"
    );
    assert!(agent.service_map_tx.borrow().resolve_all().is_empty());
    agent.drains.decrement_connections(&backend).await;
    let result = agent
        .synchronise_consumer(2, Default::default(), vec![], vec![instruction.clone()])
        .await
        .unwrap();
    assert!(result.published && result.receipts.is_empty());
    agent.drains.decrement_connections(&backend).await;
    agent.drains.decrement_websocket(&backend).await;
    let result = agent
        .synchronise_consumer(2, Default::default(), vec![], vec![instruction.clone()])
        .await
        .unwrap();
    assert!(result.published);
    assert_eq!(result.receipts, vec![1]);
    drop(agent);
    let (mut recovered, _, _) = test_cluster_fault_agent().await;
    recovered.set_records_dir(root.path().to_owned());
    recovered
        .supervisor
        .grill()
        .set_launch_inventory(vec![])
        .await;
    recovered
        .recover_consumer_ownership(&path, identity)
        .await
        .unwrap();
    assert!(recovered.service_map_tx.borrow().resolve_all().is_empty());
    assert!(
        recovered
            .synchronise_consumer(1, catalog, ingress, vec![])
            .await
            .is_err()
    );
    let result = recovered
        .synchronise_consumer(2, Default::default(), vec![], vec![instruction])
        .await
        .unwrap();
    assert_eq!(result.receipts, vec![1]);
    let (response, reply) = oneshot::channel();
    recovered
        .handle_command(AgentCommand::SyncClusterConsumer {
            generation: 0,
            catalog: Box::default(),
            ingress: vec![],
            withdrawals: vec![],
            requested_at_ns: crate::onion::lease::boot_clock_ns(),
            response,
        })
        .await;
    recovered.finish_consumer_syncs().await;
    let retry = reply.await.unwrap().unwrap();
    assert!(!retry.published);
    assert_eq!(retry.receipts, vec![1]);
    // Confirmed twice: the second finds it already forgotten.
    for _ in 0..2 {
        let (response, reply) = oneshot::channel();
        recovered
            .handle_command(AgentCommand::ConfirmConsumerReceipt {
                generation: 1,
                response,
            })
            .await;
        reply.await.unwrap().unwrap();
    }
    drop(recovered);
    let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
    let consumer = journal.inventory().consumer.as_ref().unwrap();
    assert!(consumer.receipts.is_empty());
    assert_eq!(consumer.publications.len(), 1);
    assert_eq!(consumer.publications[0].generation, 2);
}

async fn fresh_discovery_agent() -> (TestAgent, tempfile::TempDir) {
    let (mut agent, _, _, _) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent.set_records_dir(root.path().join("records"));
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    (agent, root)
}

async fn discovery_journal_state(
    readiness: &crate::bun::readiness::ReadinessTracker,
) -> Option<crate::bun::readiness::SubsystemState> {
    readiness
        .snapshot()
        .await
        .subsystems
        .into_iter()
        .find(|subsystem| subsystem.name == "discovery:journal")
        .map(|subsystem| subsystem.state)
}

#[tokio::test]
async fn failed_discovery_write_recovers_by_reopening_the_journal() {
    let (mut agent, _root) = fresh_discovery_agent().await;
    let readiness = crate::bun::readiness::ReadinessTracker::new();
    agent.set_readiness_tracker(readiness.clone());
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let DiscoveryOwnership::Ready(journal) = &mut agent.discovery_ownership else {
        panic!("fresh discovery ownership is not ready");
    };
    journal.fail_next_write();
    let map = crate::onion::service_map::ServiceMap::new();
    assert!(
        agent
            .persist_discovery_publication(&service, &map)
            .await
            .is_err()
    );
    assert_eq!(
        discovery_journal_state(&readiness).await,
        Some(crate::bun::readiness::SubsystemState::Degraded),
        "a fenced journal must be visible"
    );
    // A transient ENOSPC or EIO must not fence discovery until restart.
    agent
        .persist_discovery_publication(&service, &map)
        .await
        .unwrap();
    assert!(matches!(
        agent.discovery_ownership,
        DiscoveryOwnership::Ready(_)
    ));
    assert_eq!(
        discovery_journal_state(&readiness).await,
        Some(crate::bun::readiness::SubsystemState::Ready)
    );
}

#[tokio::test]
async fn refused_discovery_update_does_not_fence_the_journal() {
    let (mut agent, _root) = fresh_discovery_agent().await;
    let service = crate::onion::service_id::ServiceId::new("system", "discovery");
    // An invalid consumer identity fails validation before any disk write.
    let refused = agent
        .update_discovery_inventory(&service, |next| {
            next.consumer = Some(crate::bun::consumer_owners::ConsumerOwnership {
                identity: crate::bun::consumer_owners::ConsumerIdentity {
                    node_id: crate::meat::NodeId::new(""),
                    cluster_identity: [0; 32],
                },
                publications: vec![],
                phase: crate::bun::consumer_owners::ConsumerPhase::Withdrawn,
                receipts: Default::default(),
            })
        })
        .await;
    assert!(refused.is_err());
    assert!(
        matches!(agent.discovery_ownership, DiscoveryOwnership::Ready(_)),
        "a refusal that never reached disk fenced discovery"
    );
}

#[tokio::test]
async fn durable_consumer_catalogue_change_keeps_captured_requests_and_view() {
    let (mut agent, _root, catalog) = clustered_allocation_fixture().await;
    let (_, ingress) = cluster_publication_fixture();
    let backend = agent.service_map_tx.borrow().resolve_all()[0].backends[0]
        .instance_id
        .clone();
    let captured = agent
        .drains
        .capture_requests(std::slice::from_ref(&backend), false)
        .await
        .unwrap();
    // A deploy anywhere in the cluster commits a new generation. This
    // node's backends are unchanged, so its requests must not notice.
    let result = agent
        .synchronise_consumer(2, catalog, ingress, vec![])
        .await
        .unwrap();
    assert!(result.published);
    assert!(
        captured.iter().all(|token| !token.is_cancelled()),
        "a catalogue change cancelled requests to an unchanged backend"
    );
    assert!(!agent.drains.is_draining(&backend).await);
    assert_eq!(
        agent.service_map_tx.borrow().resolve_all()[0].backends[0].instance_id,
        backend
    );
    agent.drains.decrement_connections(&backend).await;
}

/// #478: a council recovered from a backup restores the catalogue
/// generation the backup holds, older than what the dead council went on to
/// publish. The node must follow the recovered council's new epoch, and
/// still refuse a leader of the council it replaced.
#[tokio::test]
async fn durable_consumer_follows_a_recovered_council_and_still_refuses_the_replaced_one() {
    let (mut agent, _root, catalog) = clustered_allocation_fixture().await;
    let (_, ingress) = cluster_publication_fixture();
    // The last backup was taken at generation 2; the old council published
    // generation 3 before every voter died.
    agent
        .synchronise_consumer(3, catalog.clone(), ingress.clone(), vec![])
        .await
        .unwrap();
    let mut backup = crate::council::types::DesiredState::default();
    backup.endpoint_withdrawals.generation = 2;
    let recovered =
        crate::council::state_machine::CouncilStateMachine::from_recovered_state(backup)
            .desired_state()
            .await
            .endpoint_withdrawals
            .generation;
    let result = agent
        .synchronise_consumer(recovered, catalog.clone(), ingress.clone(), vec![])
        .await
        .expect("the recovered council's publication was refused");
    assert!(result.published);
    assert!(
        agent
            .synchronise_consumer(4, catalog, ingress, vec![])
            .await
            .is_err(),
        "a leader of the replaced council was accepted after recovery"
    );
}

#[tokio::test]
async fn durable_consumer_history_compacts_once_a_long_capture_releases() {
    let (mut agent, _root, catalog) = clustered_allocation_fixture().await;
    let (_, ingress) = cluster_publication_fixture();
    let backend = agent.service_map_tx.borrow().resolve_all()[0].backends[0]
        .instance_id
        .clone();
    agent
        .drains
        .capture_requests(std::slice::from_ref(&backend), false)
        .await
        .unwrap();
    // The backend leaves the catalogue while a request still holds it, and
    // the cluster keeps publishing. Every change stays retained...
    let empty = crate::onion::catalog::EndpointCatalog::default();
    for generation in 2..=40 {
        let next = if generation % 2 == 0 {
            &empty
        } else {
            &catalog
        };
        let _ = agent
            .synchronise_consumer(generation, next.clone(), ingress.clone(), vec![])
            .await;
    }
    let retained = agent.consumer_owner().unwrap().publications.len();
    assert!(
        retained > 1,
        "views were forgotten while a request held one"
    );
    // ...until the request releases, and then one pass compacts them all.
    agent.drains.decrement_connections(&backend).await;
    agent
        .synchronise_consumer(41, empty, ingress, vec![])
        .await
        .unwrap();
    assert_eq!(agent.consumer_owner().unwrap().publications.len(), 1);
}

#[tokio::test]
async fn durable_consumer_local_change_keeps_the_published_view() {
    let (mut agent, _root, _catalog) = clustered_allocation_fixture().await;
    let service = crate::onion::service_id::ServiceId::new("default", "remote");
    let published = agent.service_map_tx.borrow().resolve_all().len();
    assert_eq!(published, 1);
    // Health probes, restarts and replacements all publish through here.
    agent
        .publish_backend_snapshot(&service, &agent.service_map.clone())
        .await
        .unwrap();
    assert_eq!(
        agent.service_map_tx.borrow().resolve_all().len(),
        published,
        "a local change blanked DNS and ingress"
    );
    assert!(!agent.routing_table.read().await.list_routes().is_empty());
}

#[tokio::test]
async fn unchanged_health_probe_does_not_rewrite_discovery_ownership() {
    use std::os::unix::fs::MetadataExt;
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent.set_records_dir(root.path().join("records"));
    agent.set_volumes_dir(root.path().join("volumes"));
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    grill.set_pid(std::process::id());
    grill.set_container_ip("10.0.2.5".parse().unwrap());
    grill
        .set_network_reference(original_test_network_reference())
        .await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let id = agent.supervisor.list_instances()[0].id.clone();
    let journal = root.path().join("discovery").join("discovery.json");
    agent.publish_instance_health(&id).await.unwrap();
    let before = std::fs::metadata(&journal).unwrap().ino();
    agent.publish_instance_health(&id).await.unwrap();
    assert_eq!(
        std::fs::metadata(&journal).unwrap().ino(),
        before,
        "an unchanged probe result rewrote the discovery journal"
    );
}

#[tokio::test]
async fn durable_consumer_refuses_changed_enrolment_before_recovery() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("discovery");
    let identity = crate::bun::consumer_owners::ConsumerIdentity {
        node_id: crate::meat::NodeId::new("test"),
        cluster_identity: [42; 32],
    };
    let (mut agent, _, _) = test_cluster_fault_agent().await;
    agent.set_records_dir(root.path().to_owned());
    agent.supervisor.grill().set_launch_inventory(vec![]).await;
    agent
        .recover_consumer_ownership(&path, identity.clone())
        .await
        .unwrap();
    let (catalog, ingress) = cluster_publication_fixture();
    agent
        .synchronise_consumer(3, catalog, ingress, vec![])
        .await
        .unwrap();
    drop(agent);
    let original = std::fs::read(path.join("discovery.json")).unwrap();
    let (mut replacement, _, _) = test_cluster_fault_agent().await;
    replacement.set_records_dir(root.path().to_owned());
    replacement
        .supervisor
        .grill()
        .set_launch_inventory(vec![])
        .await;
    let mut changed = identity;
    changed.cluster_identity = [43; 32];
    assert!(
        replacement
            .recover_consumer_ownership(&path, changed)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(path.join("discovery.json")).unwrap(),
        original
    );
    assert!(replacement.service_map_tx.borrow().resolve_all().is_empty());
}
async fn clustered_allocation_fixture() -> (
    BunAgent<MockGrill>,
    tempfile::TempDir,
    crate::onion::catalog::EndpointCatalog,
) {
    let (mut agent, _, _) = test_cluster_fault_agent().await;
    let root = tempfile::tempdir().unwrap();
    agent.set_records_dir(root.path().join("records"));
    agent.supervisor.grill().set_launch_inventory(vec![]).await;
    agent
        .recover_consumer_ownership(
            &root.path().join("discovery"),
            crate::bun::consumer_owners::ConsumerIdentity {
                node_id: crate::meat::NodeId::new("test"),
                cluster_identity: [42; 32],
            },
        )
        .await
        .unwrap();
    let (mut catalog, ingress) = cluster_publication_fixture();
    catalog.services.get_mut("default__remote").unwrap().vip =
        crate::onion::vip::VirtualIP("127.128.43.42".parse().unwrap());
    agent
        .synchronise_consumer(1, catalog.clone(), ingress, vec![])
        .await
        .unwrap();
    // As if the leader had just answered: a lapsed lease would shrink
    // the view to local backends on the next refresh.
    agent
        .renew_view_lease(crate::onion::lease::boot_clock_ns())
        .await;
    (agent, root, catalog)
}

#[tokio::test]
async fn clustered_local_registration_uses_the_committed_vip() {
    let (mut agent, _root, catalog) = clustered_allocation_fixture().await;
    let (reply, result) = oneshot::channel();
    agent
        .handle_deploy_op(DeployOp::RegisterServiceApp {
            app_name: "remote".into(),
            namespace: "default".into(),
            port: 8080,
            firewall: None,
            reply,
        })
        .await;
    result.await.unwrap().unwrap();
    let service = crate::onion::service_id::ServiceId::new("default", "remote");
    assert_eq!(
        agent.service_map.resolve(&service).unwrap().vip,
        catalog.resolve(&service).unwrap().vip
    );
    let (reply, result) = oneshot::channel();
    agent
        .handle_deploy_op(DeployOp::RegisterServiceApp {
            app_name: "uncommitted".into(),
            namespace: "default".into(),
            port: 8080,
            firewall: None,
            reply,
        })
        .await;
    assert!(
        result.await.unwrap().is_err(),
        "invented an uncommitted cluster allocation"
    );
}

/// A `relish stop` can land mid-rollout: the council withdraws the app's
/// allocation, the next consumer poll drops it from the committed
/// catalogue, and only then does the rollout try to finalise. The failed
/// finalisation must leave the local reservation in place, because the
/// retained replacement's retirement proves withdrawal against it. Losing
/// it made every retry fail with "original service withdrawal is unproven".
#[tokio::test]
async fn failed_rollout_finalisation_keeps_the_reservation_retirement_needs() {
    let (mut agent, _root, _catalog) = clustered_allocation_fixture().await;
    let service = crate::onion::service_id::ServiceId::new("default", "remote");
    let (reply, result) = oneshot::channel();
    agent
        .handle_deploy_op(DeployOp::RegisterServiceApp {
            app_name: "remote".into(),
            namespace: "default".into(),
            port: 8080,
            firewall: None,
            reply,
        })
        .await;
    result.await.unwrap().unwrap();
    // The rollout published its replacement before retiring the old one.
    let replacement = InstanceId("default__remote-g1-0".into());
    let backend = agent.local_backend(&replacement, &service, None, 30002, true);
    agent.service_map.add_backend(&service, backend).unwrap();
    agent
        .persist_discovery_publication(&service, &agent.service_map.clone())
        .await
        .unwrap();
    let reserved = agent.service_map.resolve(&service).unwrap().clone();
    agent
        .synchronise_consumer(2, Default::default(), vec![], vec![])
        .await
        .unwrap();

    let spec = Config::parse("[app.remote]\nimage = 'test:v1'\nport = 8080\n")
        .unwrap()
        .app
        .remove("remote")
        .unwrap();
    let finalised = agent
        .finalise_rolling_deploy(
            "remote",
            "default",
            &spec,
            &[],
            std::slice::from_ref(&replacement),
            &[(replacement.clone(), Some(30002))].into_iter().collect(),
            &[(replacement.clone(), None)].into_iter().collect(),
            Default::default(),
            Instant::now(),
        )
        .await;
    assert!(
        finalised.is_err(),
        "finalised against a withdrawn allocation"
    );

    let retained = agent.service_map.resolve(&service).cloned();
    assert_eq!(retained.as_ref().map(|entry| entry.vip), Some(reserved.vip));
    // Stop withdraws the replacement's backend, then retirement proves it.
    agent
        .service_map
        .remove_backend(&service, &replacement.0)
        .unwrap();
    agent.retire_discovery_service(&service).await.unwrap();
}

#[tokio::test]
async fn clustered_local_allocation_retires_without_cancelling_remote_replica_requests() {
    let (mut agent, root, catalog) = clustered_allocation_fixture().await;
    // Restore the exact local reservation; the public view also has a remote replica.
    let mut entry = agent.service_map_tx.borrow().resolve_all()[0].clone();
    let backend = entry.backends[0].instance_id.clone();
    entry.backends.clear();
    agent.service_map = crate::onion::service_map::ServiceMap::from_snapshot(&[entry]).unwrap();
    let service = crate::onion::service_id::ServiceId::new("default", "remote");
    agent
        .persist_discovery_publication(&service, &agent.service_map.clone())
        .await
        .unwrap();
    let guards = agent
        .drains
        .capture_requests(std::slice::from_ref(&backend), false)
        .await
        .unwrap();
    // Only this node's reservation retires. The remote replica keeps
    // serving, so a request it captured must not notice.
    agent.retire_discovery_service(&service).await.unwrap();
    assert!(
        !guards[0].is_cancelled(),
        "retiring a local allocation cancelled a remote replica's request"
    );
    agent.drains.decrement_connections(&backend).await;
    agent.service_map.unregister(&service).unwrap();
    assert_eq!(
        agent
            .service_map_tx
            .borrow()
            .resolve(&service)
            .unwrap()
            .backends[0]
            .instance_id,
        backend
    );
    assert!(
        agent
            .synchronise_consumer(1, catalog.clone(), vec![], vec![])
            .await
            .unwrap()
            .published
    );
    assert_eq!(
        agent.service_map_tx.borrow().resolve(&service).unwrap().vip,
        catalog.resolve(&service).unwrap().vip
    );
    drop(agent);
    let journal =
        crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    assert!(journal.inventory().services.is_empty());
    assert!(journal.inventory().consumer.is_some());
}
/// V02 soak: a restarted node received a deploy before the council's
/// allocation reached its view. The failed attempt left a Pending
/// instance behind, so every retry became a rollout of a service this
/// node had never published, and the deploy wedged for good.
#[tokio::test]
async fn deploy_before_its_committed_allocation_leaves_nothing_for_the_retry() {
    let (mut agent, _root, catalog) = clustered_allocation_fixture().await;
    agent.supervisor.grill().set_pid(std::process::id());
    let events = drain_deploy(&mut agent, basic_config()).await;
    match events.last() {
        Some(ApplyEvent::Error { message }) => assert!(
            message.contains("committed cluster allocation"),
            "unexpected failure: {message}"
        ),
        other => panic!("deploy ran without its allocation: {other:?}"),
    }
    assert!(
        agent
            .supervisor
            .list_instances()
            .iter()
            .all(|instance| instance.app_name != "web"),
        "the failed attempt left instances for its retry to replace"
    );
    assert!(
        !agent
            .supervisor
            .grill()
            .calls()
            .iter()
            .any(|(_, id)| id.0.starts_with("default__web")),
        "the failed attempt touched the runtime"
    );

    // The allocation arrives; the retry is an ordinary fresh deploy.
    let (_, ingress) = cluster_publication_fixture();
    let remote = catalog.services["default__remote"].clone();
    let committed = catalog
        .reconcile([
            (
                crate::onion::service_id::ServiceId::new("default", "remote"),
                remote.port,
                remote.backends,
            ),
            (
                crate::onion::service_id::ServiceId::new("default", "web"),
                8080,
                vec![],
            ),
        ])
        .unwrap();
    agent
        .synchronise_consumer(2, committed, ingress, vec![])
        .await
        .unwrap();
    agent
        .renew_view_lease(crate::onion::lease::boot_clock_ns())
        .await;
    let events = drain_deploy(&mut agent, basic_config()).await;
    let (_, instances) = expect_complete(&events);
    assert_eq!(instances, ["default__web-0".to_string()]);
}

/// A consumer node running `web-0`, whose published view of `web` also
/// names a replica on another node. Returns the agent, its records and
/// `web`'s VIP.
async fn consumer_web_fixture() -> (
    BunAgent<MockGrill>,
    tempfile::TempDir,
    crate::onion::vip::VirtualIP,
) {
    use crate::onion::catalog::CatalogBackend;
    use crate::onion::service_id::ServiceId;
    let (mut agent, root, catalog) = clustered_allocation_fixture().await;
    agent.supervisor.grill().set_pid(std::process::id());
    let (_, ingress) = cluster_publication_fixture();
    let web = ServiceId::new("default", "web");
    let remote = catalog.services["default__remote"].clone();
    let elsewhere = CatalogBackend {
        execution: None,
        node_id: "other-node".into(),
        node_ip: "192.168.1.2".parse().unwrap(),
        host_port: 30003,
        healthy: true,
    };
    let with_web = |backends: Vec<CatalogBackend>| {
        catalog
            .reconcile([
                (
                    ServiceId::new("default", "remote"),
                    remote.port,
                    remote.backends.clone(),
                ),
                (web.clone(), 8080, backends),
            ])
            .unwrap()
    };
    agent
        .synchronise_consumer(
            2,
            with_web(vec![elsewhere.clone()]),
            ingress.clone(),
            vec![],
        )
        .await
        .unwrap();
    agent
        .renew_view_lease(crate::onion::lease::boot_clock_ns())
        .await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);

    // The leader learns of the local replica and publishes both.
    let local = InstanceId("default__web-0".into());
    let execution = crate::grill::RuntimeExecution {
        instance_id: local.clone(),
        generation: crate::grill::RuntimeGeneration::process("original"),
    };
    let spec: crate::grill::OciSpec = serde_json::from_value(serde_json::json!({
        "root": {"path": "/fixture", "readonly": true},
        "process": {"args": ["/app"], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
        "mounts": [], "linux": {"namespaces": []},
    }))
    .unwrap();
    agent
        .supervisor
        .grill()
        .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
            instance_id: local.clone(),
            generation: execution.generation.clone(),
            spec,
            network_reference: None,
        }])
        .await;
    let here = CatalogBackend {
        execution: Some(execution),
        node_id: "test".into(),
        node_ip: "192.168.1.1".parse().unwrap(),
        host_port: 30002,
        healthy: true,
    };
    let committed = with_web(vec![here, elsewhere]);
    let vip = committed.resolve(&web).unwrap().vip;
    agent
        .synchronise_consumer(3, committed, ingress, vec![])
        .await
        .unwrap();
    let view = consumer_web_view(&agent);
    assert_eq!(view.len(), 2, "the view should name both replicas");
    assert!(view.iter().any(|backend| backend.local));
    (agent, root, vip)
}

/// The backends the installed consumer view names for `web`.
fn consumer_web_view(agent: &BunAgent<MockGrill>) -> Vec<crate::onion::types::BackendInstance> {
    agent
        .service_map_tx
        .borrow()
        .resolve(&crate::onion::service_id::ServiceId::new("default", "web"))
        .map(|entry| entry.backends.clone())
        .unwrap_or_default()
}

/// Whether an agent on this thread withdrew `vip`'s whole kernel entry
/// since the last call. Unit tests have no eBPF data path, so the agent
/// records the withdrawal instead.
fn withdrew_whole_entry(vip: crate::onion::vip::VirtualIP) -> bool {
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    let withdrew = super::routing::take_whole_entry_withdrawals().contains(&vip);
    // With the data path compiled in, an agent without a loaded handle
    // withdraws nothing, whole or not.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    let withdrew = {
        let _ = vip;
        false
    };
    withdrew
}

/// #481: stopping this node's last replica of a service that still runs
/// on other nodes withdrew the whole VIP from the kernel, so local clients
/// got `EPERM` until the next catalogue arrived. The stop must take only
/// this node's backends out of the installed view; the kernel entry is
/// written from that same view, so it keeps the remote backends too.
#[tokio::test]
async fn stopping_the_last_local_replica_keeps_routing_to_other_nodes() {
    let (mut agent, _root, vip) = consumer_web_fixture().await;
    let before = consumer_web_view(&agent);
    let _ = withdrew_whole_entry(vip);

    agent.begin_app_stop("web", "default").await.unwrap();
    assert!(
        !withdrew_whole_entry(vip),
        "the stop deleted the kernel entry, other nodes' backends included"
    );
    assert_eq!(
        consumer_web_view(&agent),
        before
            .into_iter()
            .filter(|backend| !backend.local)
            .collect::<Vec<_>>(),
        "the stop must withdraw only this node's backend, not the whole VIP"
    );
    assert!(agent.consumer_view_stale, "the next tick rebuilds the view");
}

/// #481, the rollout half: finishing a local rolling deploy withdrew the
/// whole kernel entry the same way, the other node's backend and the new
/// local replica included, until the next view refresh put it back.
#[tokio::test]
async fn a_local_rollout_keeps_routing_to_other_nodes() {
    let (mut agent, _root, vip) = consumer_web_fixture().await;
    let _ = withdrew_whole_entry(vip);

    let mut changed = basic_config();
    changed.app.get_mut("web").unwrap().image = Some("myapp:v2".into());
    expect_complete(&drain_deploy(&mut agent, changed).await);
    assert!(
        !withdrew_whole_entry(vip),
        "the rollout deleted the kernel entry, other nodes' backends included"
    );
    let view = consumer_web_view(&agent);
    assert!(
        view.iter().any(|backend| !backend.local),
        "the other node's backend left the view: {view:?}"
    );
    assert!(
        !view
            .iter()
            .any(|backend| backend.local && backend.instance_id == "default__web-0"),
        "the replaced replica is still routed: {view:?}"
    );
}

/// A discovery-owning agent with a Pending `web` instance whose runtime
/// holds an address, for a service this node never published.
async fn unpublished_hold_fixture() -> (
    TestAgent,
    MockGrill,
    tempfile::TempDir,
    crate::grill::runc_intent::NetworkReference,
) {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    let spec = basic_config().app.remove("web").unwrap();
    let (reply, result) = oneshot::channel();
    agent
        .handle_deploy_op(DeployOp::SupervisorDeployApp {
            app_name: "web".into(),
            namespace: "default".into(),
            spec: Box::new(spec),
            reply,
        })
        .await;
    assert_eq!(
        result.await.unwrap().unwrap(),
        std::slice::from_ref(&reference.instance_id)
    );
    (agent, grill, root, reference)
}

#[tokio::test]
async fn refused_reference_record_hands_the_runtime_hold_back() {
    let (mut agent, grill, root, reference) = unpublished_hold_fixture().await;
    let spec = basic_config().app.remove("web").unwrap();
    // The deploy worker retains the reference off the loop.
    let retained = grill
        .retain_network_reference(&reference.instance_id)
        .await
        .map_err(BunError::from);
    let (reply, result) = oneshot::channel();
    agent
        .handle_deploy_op(DeployOp::ApplyNetworkPreStart {
            instance_id: reference.instance_id.clone(),
            app_name: "web".into(),
            spec: Some(Box::new(spec)),
            cgroup_path: root.path().join("cgroup"),
            retained,
            egress: Box::default(),
            reply,
        })
        .await;
    assert!(result.await.unwrap().is_err());
    assert!(matches!(
        agent.discovery_ownership,
        DiscoveryOwnership::Ready(_)
    ));
    // The hand-back runs in a task; nothing on the loop waits for it.
    let handed_back = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        handed_back.is_ok(),
        "a hold nothing records outlived its refused launch"
    );
}

#[tokio::test]
async fn retirement_releases_a_hold_the_journal_never_recorded() {
    let (mut agent, grill, _root, reference) = unpublished_hold_fixture().await;
    // The runtime kept a hold the agent lost track of before recording it.
    agent
        .release_network_reference(&reference.instance_id, None)
        .await
        .unwrap();
    assert_eq!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn retirement_keeps_an_untracked_hold_without_an_authoritative_journal() {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    assert!(
        agent
            .release_network_reference(&reference.instance_id, None)
            .await
            .is_err()
    );
    assert_eq!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap(),
        Some(reference)
    );
}

#[tokio::test]
async fn clustered_startup_retains_orphan_ports_until_api_driven_cleanup_can_finish() {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    crate::grill::records::remove_record(&root.path().join("records"), &reference.instance_id.0)
        .unwrap();
    let journal =
        crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    let mut inventory = journal.inventory().clone();
    let identity = crate::bun::consumer_owners::ConsumerIdentity {
        node_id: crate::meat::NodeId::new("test"),
        cluster_identity: [42; 32],
    };
    inventory.consumer = Some(crate::bun::consumer_owners::ConsumerOwnership {
        identity: identity.clone(),
        publications: vec![],
        phase: crate::bun::consumer_owners::ConsumerPhase::Withdrawn,
        receipts: Default::default(),
    });
    drop(journal.persist(inventory).await.unwrap());
    let (mut clustered, _, _) = test_cluster_fault_agent().await;
    agent.cluster = clustered.cluster.take();
    agent
        .recover_consumer_ownership(&root.path().join("discovery"), identity)
        .await
        .unwrap();
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
    let launch = grill.launch_inventory().await.unwrap().unwrap().remove(0);
    assert!(
        agent
            .supervisor
            .port_allocator
            .is_allocated(launch.spec.port_mapping.unwrap().host_port)
            .await
    );
    assert!(
        grill
            .network_reference(&reference.instance_id)
            .await
            .unwrap()
            .is_some()
    );
    let (events, mut received) = mpsc::channel(8);
    agent.begin_deploy(basic_config(), events).await;
    assert!(matches!(
        received.recv().await,
        Some(ApplyEvent::Error { .. })
    ));
    agent.drive_startup_retirements().await;
    assert!(agent.startup_cleanup_pending);
    assert!(
        agent
            .supervisor
            .port_allocator
            .is_allocated(launch.spec.port_mapping.unwrap().host_port)
            .await
    );
    let confirmation = serde_json::json!({"node_id": "test", "execution": {"instance_id": reference.instance_id, "generation": launch.generation}}).to_string();
    let (client, server) =
        crate::cluster::producer::test_fixture(reqwest::StatusCode::OK, confirmation).await;
    agent.set_producer_release_client(client);
    agent.drive_startup_retirements().await;
    // The retirement is done; the discovery recovery after it journals on
    // the next tick (#422).
    assert!(agent.startup_cleanup_pending);
    agent.drive_startup_retirements().await;
    assert!(!agent.startup_cleanup_pending);
    assert!(
        !agent
            .supervisor
            .port_allocator
            .is_allocated(launch.spec.port_mapping.unwrap().host_port)
            .await
    );
    assert!(agent.network_references.is_empty());
    assert!(agent.service_map.resolve_all().is_empty());
    server.abort();
}

#[tokio::test]
async fn rootless_discovery_adoption_restores_owned_host_forward_without_container_ip() {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    let mut launches = grill.launch_inventory().await.unwrap().unwrap();
    launches[0].network_reference = None;
    grill.release_network_reference(&reference).await.unwrap();
    grill.set_launch_inventory(launches.clone()).await;
    grill.set_adopt_result(&reference.instance_id, true);
    grill.clear_container_ip();
    grill.set_rootless_network(crate::grill::records::RootlessNetworkRecord {
        api_socket: root.path().join("slirp.sock"),
        owner_pid: std::process::id(),
        owner_pid_started_at: 1,
        container_pid: std::process::id(),
        port_mapping: launches[0].spec.port_mapping,
    });
    let journal =
        crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    let mut inventory = journal.inventory().clone();
    inventory.references.clear();
    let backend = &mut inventory.services[0].entry.backends[0];
    backend.node_ip = std::net::Ipv4Addr::LOCALHOST;
    backend.host_port = launches[0].spec.port_mapping.unwrap().host_port;
    inventory.services[0].executions.insert(
        reference.instance_id.0.clone(),
        launches[0].generation.clone(),
    );
    drop(journal);
    std::fs::write(
        root.path().join("discovery/discovery.json"),
        serde_json::to_vec(&serde_json::json!({"schema": 4, "inventory": inventory})).unwrap(),
    )
    .unwrap();
    agent
        .recover_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let entry = agent
        .service_map_tx
        .borrow()
        .resolve(&service)
        .unwrap()
        .clone();
    assert_eq!(entry.backends[0].node_ip, std::net::Ipv4Addr::LOCALHOST);
    assert_eq!(
        entry.backends[0].host_port,
        launches[0].spec.port_mapping.unwrap().host_port
    );
}

// --- workload identity signing off the command loop ---

/// A council that is not the leader, so signing goes to the leader transport.
async fn follower_council() -> Arc<CouncilNode> {
    use crate::council::log_store::MemLogStore;
    use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use crate::council::state_machine::CouncilStateMachine;
    use crate::council::types::CouncilConfig;

    let network = InMemoryRaftNetworkFactory::new(2, InMemoryRaftRouter::new());
    let node = CouncilNode::new(
        2,
        CouncilConfig::default(),
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    assert!(!node.is_leader().await);
    Arc::new(node)
}

/// A leader transport whose "leader" accepts connections and then either
/// says nothing (`silent`) or hangs up at once.
async fn leader_transport(silent: bool) -> crate::cluster::workload_identity::WorkloadCsrClient {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            if silent {
                held.push(stream);
            }
        }
    });
    // A receiver keeps its last value after the sender goes.
    let (_, metrics) = watch::channel(openraft::RaftMetrics::new_initial(2));
    let directory = crate::mustard::directory::NodeDirectory {
        leader: Some(crate::mustard::message::LeaderHint {
            node_id: crate::meat::NodeId::new("leader"),
            term: 0,
            recovery_epoch: 0,
            api_address: address,
            reporting_address: address,
        }),
        ..Default::default()
    };
    let (_, directory) = watch::channel(directory);
    crate::cluster::workload_identity::WorkloadCsrClient::new(
        crate::cluster::ClusterHttp::secure(reqwest::Client::new()),
        metrics,
        directory,
        0,
    )
}

async fn follower_agent(silent_leader: bool) -> BunAgent<MockGrill> {
    let mut agent = agent_with_council(follower_council().await);
    agent.set_workload_csr_client(leader_transport(silent_leader).await);
    agent
}

fn provision_op(instance: &str) -> (DeployOp, oneshot::Receiver<()>) {
    let (reply, answered) = oneshot::channel();
    let op = DeployOp::ProvisionIdentity {
        app_name: "web".into(),
        namespace: "default".into(),
        instance_id: InstanceId(instance.into()),
        reply,
    };
    (op, answered)
}

/// PR #270's investigation: a follower's CSR goes to the leader with a
/// 10 s limit, and it ran inline from the deploy op, so a slow leader
/// held every queued command for up to 10 s. The loop now only starts
/// the signing; the deploy worker's reply comes when it has finished.
#[tokio::test]
async fn follower_csr_to_a_silent_leader_does_not_hold_the_command_loop() {
    let mut agent = follower_agent(true).await;
    let (op, mut answered) = provision_op("default__web-0");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        agent.handle_deploy_op(op),
    )
    .await
    .expect("the identity CSR held the agent loop");
    assert_eq!(agent.identity_signings.len(), 1);
    assert!(
        matches!(
            answered.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "the deploy worker was answered before its identity was signed"
    );
}

/// The rotation tick provisions a missing identity the same way: it
/// starts the signing and moves on.
#[tokio::test]
async fn identity_rotation_does_not_wait_for_the_leader() {
    // Deploy with no leader transport, so the deploy's own CSR fails
    // fast; then the leader goes silent.
    let mut agent = agent_with_council(follower_council().await);
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    agent.set_workload_csr_client(leader_transport(true).await);
    let id = agent.supervisor.list_instances()[0].id.clone();
    agent.supervisor.get_instance_mut(&id).unwrap().identity = None;
    agent.identity_retry_ticks = IDENTITY_RETRY_TICKS - 1;
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        agent.check_identity_rotation();
    })
    .await
    .expect("the rotation tick waited for the leader");
    assert_eq!(agent.identity_signings.len(), 1);
}

/// A deploy and the rotation tick asking for the same instance share one
/// CSR, and both are answered when it finishes, even when it fails.
#[tokio::test]
async fn concurrent_requests_for_one_identity_share_one_signing() {
    let mut agent = follower_agent(false).await;
    let (first, first_answered) = provision_op("default__web-0");
    let (second, second_answered) = provision_op("default__web-0");
    agent.handle_deploy_op(first).await;
    agent.handle_deploy_op(second).await;
    assert_eq!(agent.identity_signing_tasks.len(), 1);
    let signing = agent.identity_signings.values().next().unwrap();
    assert_eq!(signing.waiter_count(), 2);

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        agent.identity_signing_tasks.join_next_with_id(),
    )
    .await
    .unwrap()
    .unwrap();
    agent.finish_identity_provision(outcome);
    for answered in [first_answered, second_answered] {
        answered.await.expect("a waiter was dropped unanswered");
    }
    assert!(agent.identity_signings.is_empty());
}

/// A signed identity is written to the instance's mount and recorded;
/// one for an instance retired while its CSR was out is dropped, so it
/// can't recreate the directory retirement removed.
#[tokio::test]
async fn signed_identity_is_stored_only_for_a_live_instance() {
    let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
    let volumes = tempfile::tempdir().unwrap();
    agent.set_volumes_dir(volumes.path().to_path_buf());
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let live = agent.supervisor.list_instances()[0].id.clone();
    agent.supervisor.get_instance_mut(&live).unwrap().identity = None;
    let retired = InstanceId("default__web-9".into());

    let issued = write_test_identity(volumes.path(), "scratch");
    let signed = || {
        super::identity_signing::SignedIdentity::for_test(
            issued.spiffe_uri.clone(),
            issued.private_key_der.clone(),
            Ok(crate::cluster::workload_identity::SignedWorkload {
                cert_der: issued.certificate_der.clone(),
                workload_ca_cert_der: vec![1],
                root_ca_cert_der: vec![2],
                ca_bundle_der: vec![vec![1], vec![2]],
                jwt_token: Some("jwt".into()),
            }),
        )
    };
    for id in [&live, &retired] {
        let result = signed();
        let task = agent
            .identity_signing_tasks
            .spawn(async move { result })
            .id();
        agent.identity_signings.insert(
            task,
            super::identity_signing::IdentitySigning::for_test(id.clone()),
        );
        let outcome = agent
            .identity_signing_tasks
            .join_next_with_id()
            .await
            .unwrap();
        agent.finish_identity_provision(outcome);
    }

    let instance = agent.supervisor.get_instance(&live).unwrap();
    assert_eq!(instance.identity.as_ref().unwrap().jwt_token, "jwt");
    assert!(agent.instance_identity_dir(&live).exists());
    assert!(
        !agent.instance_identity_dir(&retired).exists(),
        "a late signature recreated a retired instance's identity directory"
    );
}

/// PR #270's investigation: runc answers `pid` and `exit_code` under the
/// instance's lifecycle lock, which a slow create or stop can hold for
/// seconds, and `get_status` waited on it with no deadline. Status now
/// answers anyway: what it knows, with the slow instance marked.
#[tokio::test]
async fn status_answers_promptly_when_one_instance_holds_its_runtime_lock() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 3\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    grill.set_pid(4242);
    let slow = InstanceId("default__web-1".into());
    grill.set_instance_pid_delay(&slow, std::time::Duration::from_secs(30));

    let statuses = tokio::time::timeout(
        STATUS_RUNTIME_READ_TIMEOUT + std::time::Duration::from_secs(2),
        agent.get_status(),
    )
    .await
    .expect("status waited for the busy instance");

    assert_eq!(statuses.len(), 3);
    for status in &statuses {
        if status.id == slow.0 {
            assert!(status.runtime_unknown, "{status:?}");
            assert_eq!(status.pid, None);
            assert_eq!(status.state, "running", "known state was dropped");
        } else {
            assert!(!status.runtime_unknown, "{status:?}");
            assert_eq!(status.pid, Some(4242));
        }
    }
}

/// The deadline covers the whole answer, not each instance in turn:
/// several busy instances cost one deadline, not one each.
#[tokio::test]
async fn busy_instances_share_one_status_deadline() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 6\n").unwrap();
    expect_complete(&drain_deploy(&mut agent, config).await);
    grill.set_pid_delay(Some(std::time::Duration::from_secs(30)));

    let started = std::time::Instant::now();
    let statuses = agent.get_status().await;
    let took = started.elapsed();

    assert!(
        took < 2 * STATUS_RUNTIME_READ_TIMEOUT,
        "six busy instances took {took:?}"
    );
    assert!(statuses.iter().all(|status| status.runtime_unknown));
}

/// A marked status still round-trips, and an unmarked one keeps its old
/// wire form.
#[test]
fn runtime_unknown_is_serialised_only_when_set() {
    let mut status = InstanceStatus {
        id: "default__web-0".into(),
        app_name: "web".into(),
        namespace: "default".into(),
        state: "running".into(),
        restart_count: 0,
        host_port: None,
        exit_code: None,
        pid: Some(7),
        runtime_unknown: false,
        status_age_ms: None,
        reason: None,
    };
    let plain = serde_json::to_value(&status).unwrap();
    assert!(plain.get("runtime_unknown").is_none(), "{plain}");
    status.runtime_unknown = true;
    let marked: InstanceStatus =
        serde_json::from_value(serde_json::to_value(&status).unwrap()).unwrap();
    assert!(marked.runtime_unknown);
}

// --- adopted instances that already run their placement ---

/// PR #267's timeline: a Bun upgraded before its reconciler recorded the
/// writer's deploy as applied adopted the running writer, then found the
/// placement still pending and rolled it (surge-first, two writers on one
/// volume). The adopting agent must be able to say that its adopted
/// instances already run exactly that placement.
#[tokio::test]
async fn adopted_instances_that_run_their_placement_are_recognised() {
    let directory = tempfile::tempdir().unwrap();
    let records = directory.path().join("instances");
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.clone());
    agent.set_volumes_dir(directory.path().join("volumes"));
    grill.set_pid(std::process::id());
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 2\n").unwrap();
    let placed = config.app["web"].clone();
    expect_complete(&drain_deploy(&mut agent, config).await);
    let ids: Vec<InstanceId> = agent
        .supervisor
        .list_instances()
        .iter()
        .map(|instance| instance.id.clone())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(
        !agent.adopted_instances_match("web", "default", &placed),
        "instances this agent deployed itself are not adoption evidence"
    );

    let (mut replacement, _tx, _shutdown, runtime) = test_agent_with_grill();
    replacement.set_records_dir(records);
    replacement.set_volumes_dir(directory.path().join("volumes"));
    runtime.set_pid(std::process::id());
    for id in &ids {
        runtime.set_adopt_result(id, true);
    }
    assert_eq!(replacement.adopt_recorded_instances().await.unwrap(), 2);
    assert!(replacement.adopted_instances_match("web", "default", &placed));

    let mut newer = placed.clone();
    newer.image = Some("web:v2".into());
    assert!(!replacement.adopted_instances_match("web", "default", &newer));
    let mut bigger = placed.clone();
    bigger.replicas = crate::config::Replicas::Fixed(3);
    assert!(!replacement.adopted_instances_match("web", "default", &bigger));
    assert!(!replacement.adopted_instances_match("api", "default", &placed));

    // An instance that isn't running any more needs the deploy.
    replacement
        .supervisor
        .get_instance_mut(&ids[0])
        .unwrap()
        .state = ContainerState::Unhealthy;
    assert!(!replacement.adopted_instances_match("web", "default", &placed));
    replacement
        .supervisor
        .get_instance_mut(&ids[0])
        .unwrap()
        .state = ContainerState::Running;
    assert!(replacement.adopted_instances_match("web", "default", &placed));

    // Once this agent deploys the app itself, adoption says nothing more.
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 2\n").unwrap();
    expect_complete(&drain_deploy(&mut replacement, config).await);
    assert!(!replacement.adopted_instances_match("web", "default", &placed));
}

/// Adopted instances whose records disagree about their spec prove
/// nothing, so the placement is deployed as before.
#[tokio::test]
async fn adopted_instances_with_disagreeing_records_are_not_converged() {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    let dir = tempfile::tempdir().unwrap();
    let first = adoption_record("default__web-0", "web", false);
    let mut second = adoption_record("default__web-1", "web", false);
    second.replica_index = 1;
    second.host_port = Some(30124);
    second.app_spec.as_mut().unwrap().port = Some(9999);
    crate::grill::records::write_record(dir.path(), &first).unwrap();
    crate::grill::records::write_record(dir.path(), &second).unwrap();
    agent.set_records_dir(dir.path().to_path_buf());
    for id in ["default__web-0", "default__web-1"] {
        grill.set_adopt_result(&InstanceId(id.into()), true);
    }
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 2);
    let mut placed = first.app_spec.clone().unwrap();
    placed.replicas = crate::config::Replicas::Fixed(2);
    assert!(!agent.adopted_instances_match("web", "default", &placed));
}

/// The reconciler asks over the command channel.
#[tokio::test]
async fn adopted_placement_query_is_answered_on_the_command_channel() {
    let (agent, tx, shutdown) = test_agent();
    let task = tokio::spawn(async move {
        let mut agent = agent;
        agent.run().await
    });
    let (response, answer) = oneshot::channel();
    tx.send(AgentCommand::AdoptedPlacementMatches {
        app_name: "web".into(),
        namespace: "default".into(),
        spec: Box::new(toml::from_str("image = 'web:v1'").unwrap()),
        response,
    })
    .await
    .unwrap();
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(5), answer)
            .await
            .unwrap()
            .unwrap()
    );
    shutdown.cancel();
    task.await.unwrap();
}
