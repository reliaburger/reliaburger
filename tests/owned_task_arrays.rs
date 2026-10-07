//! Image-backed delegated jobs, run by the warmed-image Linux gate.
//! The OCI interruption driver deliberately owns a separate, offline fixture set.
#![cfg(target_os = "linux")]

use reliaburger::grill::runc::RuncGrill;
use reliaburger::grill::{ContainerState, Grill, ImageStore};
#[cfg(feature = "ebpf")]
use std::path::Path;
use std::time::Duration;

/// Real isolated attempts reuse a bounded owner pool, without losing app capacity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, pinned test image, ip and nft; run with make test-linux"]
async fn runc_owned_task_arrays_pack_profiles_reuse_slots_and_retire_cancelled_process_trees() {
    use reliaburger::bun::execution_budget::ExecutionBudget;
    use reliaburger::bun::task_executor::{
        AttemptOutcome, ChunkWork, PoolConfig, TaskInvocation, TaskPool, TaskRunner,
    };
    use reliaburger::bun::task_runtime::OwnedRunner;
    use reliaburger::meat::Resources;
    use reliaburger::meat::task_array::{ChunkId, TaskArraySpec};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.path().join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = RuncGrill::new(
        root.path().join("bundles"),
        images,
        false,
        root.path().join("state"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap();
    eprintln!("batch fixture root: {}", root.path().display());
    let runner = Arc::new(OwnedRunner::with_slot_count(runtime.clone(), 2));
    let budget = ExecutionBudget::new(Resources::new(1000, 128 << 20, 0));
    let app = budget
        .try_acquire(Resources::new(500, 64 << 20, 0))
        .unwrap();
    let slots = Arc::new(tokio::sync::Semaphore::new(4));
    let template = |cpu: &str, memory: &str| -> reliaburger::config::job::JobSpec {
        toml::from_str(&format!(
            "image='{image}'\nnamespace='rbtest-batch'\ncpu='{cpu}'\nmemory='{memory}'"
        ))
        .unwrap()
    };
    let work = |id, template| {
        ChunkWork{replay_unknown:true,batch_id:id,template:Some(Box::new(template)),spec:TaskArraySpec{chunk_size:6,max_attempts:1,..TaskArraySpec::with_count(6)},chunk:ChunkId(0),grant_attempt:1,program:"/unused".into(),args:vec!["/bin/sh".into(),"-c".into(),"test ! -e /tmp/previous; echo task > /tmp/previous; test -r /proc/self/cgroup; sleep 0.05".into()],env:vec![]}
    };
    let config = PoolConfig::with_concurrency(4);
    let small = TaskPool::with_node_slots(runner.clone(), config, slots.clone())
        .with_budget(budget.clone(), Resources::new(200, 32 << 20, 0));
    let large = TaskPool::with_node_slots(runner.clone(), config, slots)
        .with_budget(budget.clone(), Resources::new(500, 64 << 20, 0));
    let cancelled = CancellationToken::new();
    let started = std::time::Instant::now();
    let small_work = work(1, template("200m", "32Mi"));
    let large_work = work(2, template("500m", "64Mi"));
    let (small, large) = tokio::join!(
        small.run_chunk(&small_work, &cancelled),
        large.run_chunk(&large_work, &cancelled)
    );
    assert_eq!(small.result.succeeded, 6, "{:?}", small.records);
    assert_eq!(large.result.succeeded, 6, "{:?}", large.records);
    eprintln!(
        "12 real image tasks beside reserved app capacity took {:.3}s ({:.2}/s), warm image, 2 owner slots",
        started.elapsed().as_secs_f64(),
        12.0 / started.elapsed().as_secs_f64()
    );
    assert_eq!(budget.available(), Resources::new(500, 64 << 20, 0));
    let retained = std::fs::read_dir(root.path().join("bundles/.intents/records"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("rbtest-batch__executor-")
        })
        .count();
    assert_eq!(retained, 2, "retired runtime metadata grew with task count");
    let inventory = runtime.launch_inventory().await.unwrap().unwrap();
    assert!(
        inventory.len() <= 2,
        "runtime identity count grew with task count"
    );
    assert!(inventory.iter().all(|entry| {
        entry
            .spec
            .process
            .rlimits
            .iter()
            .any(|limit| limit.kind == "RLIMIT_FSIZE")
    }));
    let cancel = CancellationToken::new();
    let attempt = TaskInvocation {
        template: Some(Box::new(template("500m", "64Mi"))),
        index: 0,
        attempt: 1,
        program: "/unused".into(),
        args: vec!["/bin/sh".into(), "-c".into(), "sleep 60 & wait".into()],
        env: vec![],
    };
    let running = {
        let runner = runner.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { runner.run(&attempt, Duration::from_secs(30), &cancel).await })
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(30), running)
            .await
            .unwrap()
            .unwrap()
            .outcome,
        AttemptOutcome::Cancelled
    );
    for entry in runtime.launch_inventory().await.unwrap().unwrap() {
        assert_eq!(
            runtime.state(&entry.instance_id).await.unwrap(),
            ContainerState::Stopped
        );
    }
    drop(app);
    assert_eq!(budget.available(), Resources::new(1000, 128 << 20, 0));
}

/// Delegated sources inherit their tenant before their first connect; losing
/// that live binding retires the process tree instead of running unguarded.
#[cfg(feature = "ebpf")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, eBPF, pinned test image, ip and nft; run with make test-linux"]
async fn runc_delegated_namespace_isolation_and_policy_loss_retire_the_original_owner() {
    use reliaburger::bun::task_executor::{AttemptOutcome, TaskInvocation, TaskRunner};
    use reliaburger::bun::task_namespace::TaskNamespacePolicy;
    use reliaburger::bun::task_runtime::OwnedRunner;
    use reliaburger::onion::{
        ebpf::loader::OnionEbpf, ebpf::maps::BpfServiceMap, service_id::ServiceId,
        service_map::ServiceMap, types::BackendInstance,
    };
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.path().join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = RuncGrill::new(
        root.path().join("bundles"),
        images,
        false,
        root.path().join("state"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap();
    let kernel = Arc::new(tokio::sync::Mutex::new(
        OnionEbpf::load_embedded(Path::new("/sys/fs/cgroup")).unwrap(),
    ));
    let policy = TaskNamespacePolicy::recover(kernel.clone(), root.path())
        .await
        .unwrap();
    let runner = Arc::new(
        OwnedRunner::with_slot_count(runtime.clone(), 1).with_namespace_policy(policy.clone()),
    );
    // Runtime perimeter rules protect allocated host ports (10000–60000).
    // This fixture is a host service, so bind outside that protected range.
    let mut listener = None;
    for port in 8000..8100 {
        if let Ok(bound) = tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
            listener = Some(bound);
            break;
        }
    }
    let listener = listener.expect("no unallocated host fixture port is available");
    let port = listener.local_addr().unwrap().port();
    let server_cancel = CancellationToken::new();
    let server = {
        let cancel = server_cancel.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! { () = cancel.cancelled() => break, accepted = listener.accept() => accepted };
                let (mut connection, _) = accepted.unwrap();
                let mut request = [0u8; 1024];
                let _ = connection.read(&mut request).await;
                let _ = connection
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            }
        })
    };
    let namespace = format!("rbtest-job-{}", rand::random::<u32>());
    let other = format!("rbtest-other-{}", rand::random::<u32>());
    let mut services = ServiceMap::new();
    for ns in [&namespace, &other] {
        services.register_app("web", ns, port, None).unwrap();
        services
            .add_backend(
                &ServiceId::new(ns, "web"),
                BackendInstance {
                    instance_id: format!("{ns}-web"),
                    node_ip: runtime.dns_gateway_address().unwrap(),
                    host_port: port,
                    healthy: true,
                    local: true,
                },
            )
            .unwrap();
    }
    {
        let mut kernel = kernel.lock().await;
        BpfServiceMap::new()
            .sync_from_service_map(&services, &mut kernel)
            .unwrap();
    }
    let vip = |ns: &str| {
        services
            .resolve(&ServiceId::new(ns, "web"))
            .unwrap()
            .vip
            .to_string()
    };
    let invocation = |command| TaskInvocation {
        template: Some(Box::new(
            toml::from_str(&format!("image='{image}'\nnamespace='{namespace}'")).unwrap(),
        )),
        index: 0,
        attempt: 1,
        program: "/unused".into(),
        args: vec!["/bin/sh".into(), "-c".into(), command],
        env: vec![],
    };
    let outcome = runner.run(&invocation(format!("test \"$(wget -qO- -T 2 http://{}:{port}/)\" = ok && ! wget -qO- -T 2 http://{}:{port}/", vip(&namespace), vip(&other))), Duration::from_secs(30), &CancellationToken::new()).await;
    assert_eq!(
        outcome.outcome,
        AttemptOutcome::Exited { code: 0 },
        "{:?}",
        outcome.output
    );
    // Cached ancestry is reused; one journal entry covers the executor slots.
    let journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("batch-namespaces.json")).unwrap())
            .unwrap();
    assert_eq!(journal["namespaces"].as_object().unwrap().len(), 1);
    let cgroup = journal["namespaces"][&namespace].as_u64().unwrap();
    let sleeping = invocation("sleep 60 & wait".into());
    let attempt = {
        let runner = runner.clone();
        tokio::spawn(async move {
            runner
                .run(
                    &sleeping,
                    Duration::from_secs(30),
                    &CancellationToken::new(),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut running = false;
            for owner in runtime.launch_inventory().await.unwrap().unwrap() {
                if runtime.state(&owner.instance_id).await.ok() == Some(ContainerState::Running)
                    && runtime
                        .pid(&owner.instance_id)
                        .await
                        .ok()
                        .flatten()
                        .is_some()
                {
                    running = true;
                    break;
                }
            }
            if running {
                break;
            }
            assert!(
                !attempt.is_finished(),
                "task ended before namespace-loss injection"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("task never established a running owner");
    reliaburger::sesame::firewall::delete_cgroup_namespace_entry(
        &mut kernel.lock().await.bpf,
        cgroup,
    )
    .unwrap();
    let retired = tokio::time::timeout(Duration::from_secs(30), attempt)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(retired.outcome, AttemptOutcome::Unknown { .. }),
        "{retired:?}"
    );
    assert!(
        String::from_utf8_lossy(&retired.output.head).contains("namespace enforcement was lost")
    );
    for owner in runtime.launch_inventory().await.unwrap().unwrap() {
        assert_eq!(
            runtime.state(&owner.instance_id).await.unwrap(),
            ContainerState::Stopped
        );
    }
    // The startup pass clears only recorded, same-boot ancestry after owners retire.
    drop(runner);
    drop(policy);
    TaskNamespacePolicy::recover(kernel.clone(), root.path())
        .await
        .unwrap();
    assert!(
        reliaburger::sesame::firewall::read_firewall_state(&mut kernel.lock().await.bpf, cgroup, 0)
            .unwrap()
            .source_namespace_id
            .is_none()
    );
    server_cancel.cancel();
    server.await.unwrap();
    kernel.lock().await.detach().unwrap();
}

/// Common public admission retains encrypted templates, writable singleton roots
/// and accepted hook gates on the actual owned container runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, pinned test image, ip and nft; run with make test-linux"]
async fn runc_common_job_api_runs_encrypted_singletons_and_gates_hooks_on_accepted_success() {
    use reliaburger::{
        bun::{
            agent::BunAgent,
            api,
            task_array_leader::TaskArrayService,
            task_array_node::{NodeRunner, TaskArrayNode, TaskArrayNodeConfig},
            task_runtime::OwnedRunner,
        },
        council::{
            log_store::MemLogStore,
            network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter},
            node::CouncilNode,
            state_machine::CouncilStateMachine,
            types::{CouncilConfig, CouncilNodeInfo, RaftRequest},
        },
        grill::{AnyGrill, PortAllocator},
        sesame::{
            secret,
            types::{AgeKeyScope, SecurityState},
        },
    };
    use serde_json::{Value, json};
    use std::{collections::BTreeMap, sync::Arc};
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.path().join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = RuncGrill::new(
        root.path().join("bundles"),
        images,
        false,
        root.path().join("runtime"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap();
    let network = InMemoryRaftRouter::new();
    let council = Arc::new(
        CouncilNode::new(
            1,
            CouncilConfig {
                heartbeat_interval_ms: 50,
                election_timeout_min_ms: 150,
                election_timeout_max_ms: 400,
                snapshot_threshold: 1000,
                max_in_snapshot_log_to_keep: 500,
            },
            InMemoryRaftNetworkFactory::new(1, network.clone()),
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap(),
    );
    network.register(1, council.raft().clone()).await;
    council
        .initialize(BTreeMap::from([(
            1,
            CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "worker"),
        )]))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !council.is_leader().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let ikm = [42u8; 32];
    let (keypair, _) =
        secret::generate_age_keypair(AgeKeyScope::Namespace("default".into()), &ikm, 0).unwrap();
    let sealed = secret::encrypt_secret("proof-value", &keypair.public_key).unwrap();
    council
        .write(RaftRequest::SecurityStateInit(Box::new(SecurityState {
            age_keypairs: vec![keypair],
            ..Default::default()
        })))
        .await
        .unwrap();
    let shutdown = CancellationToken::new();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(64);
    let mut agent = BunAgent::new(
        AnyGrill::Runc(runtime.clone()),
        PortAllocator::new(43000, 44000),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_node_capacity(2000, 128);
    let runner = OwnedRunner::for_data_dir(AnyGrill::Runc(runtime.clone()), root.path())
        .unwrap()
        .with_secrets(council.clone(), ikm);
    let executor = TaskArrayNode::new(
        TaskArrayNodeConfig::for_data_dir(root.path(), Default::default()),
        NodeRunner::Owned(Box::new(runner)),
    )
    .with_budget(agent.execution_budget());
    let service = Arc::new(
        TaskArrayService::with_timings(
            Some(Arc::new(executor)),
            Duration::from_millis(50),
            Duration::from_secs(3),
        )
        .with_storage(root.path())
        .await
        .unwrap(),
    );
    let agent_task = tokio::spawn(async move {
        agent.run().await;
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = api::router_with_upgrade(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        Some(council.clone()),
        None,
        None,
        None,
        None,
        None,
        None,
        port,
        None,
        None,
        None,
        "default".into(),
        Some("worker".into()),
        reliaburger::bun::build_runner::BuildSettings::with_timeout(900),
        reliaburger::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        Default::default(),
        reliaburger::bun::readiness::ReadinessTracker::new(),
        None,
        None,
        None,
        Some(service),
    );
    let server_shutdown = shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(server_shutdown.cancelled_owned())
            .await
            .unwrap();
    });
    let http = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let submitted = http.post(format!("{base}/v1/jobs/runs")).json(&json!({"name":"secret-job","request_id":"secret-request","definition":{"template":{"image":image,"command":["/bin/sh","-c","printf %s \"$TOKEN\" | sha256sum | grep '^e63e947dfc6fd0b3c0caa104bfbebf2e74121d42c971e3a80596adfdfeca77cc ' && touch /tmp/writable && echo safe-output"],"env":{"TOKEN":sealed}},"tasks":{"max_attempts":1}}})).send().await.unwrap();
    assert_eq!(submitted.status(), 202);
    let id = submitted.json::<Value>().await.unwrap()["batch_id"]
        .as_u64()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let summary = http
                .get(format!("{base}/v1/batch/{id}"))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            if summary["done"] == true {
                assert_eq!(summary["succeeded"], 1, "{summary}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let output = http
        .get(format!("{base}/v1/batch/{id}/tasks/0/logs"))
        .send()
        .await
        .unwrap();
    assert_eq!(output.status(), 200);
    assert!(output.text().await.unwrap().contains("safe-output"));
    let state = council.desired_state().await;
    assert!(
        !serde_json::to_string(&state)
            .unwrap()
            .contains("proof-value")
    );
    assert!(state.task_arrays.get(id).unwrap().template.env["TOKEN"].is_encrypted());
    let operation = "0123456789abcdef0123456789abcdef";
    let hook = format!(
        "[app.after-hook]\nimage='{image}'\ncommand=['/bin/sh','-c','sleep 60']\n[job.prepare]\nimage='{image}'\ncommand=['/bin/sh','-c','sleep 1; touch /tmp/prepared; echo prepared']\nrun_before=['app.after-hook']\n"
    );
    let applied = http
        .post(format!("{base}/v1/apply"))
        .header("idempotency-key", operation)
        .body(hook)
        .send()
        .await
        .unwrap();
    assert_eq!(applied.status(), 200);
    assert!(
        council.desired_state().await.apps.is_empty(),
        "app publication preceded the accepted hook"
    );
    let events = tokio::time::timeout(Duration::from_secs(30), applied.text())
        .await
        .unwrap()
        .unwrap();
    assert!(events.contains("\"type\":\"Complete\""), "{events}");
    let state = council.desired_state().await;
    assert!(
        state
            .apps
            .contains_key(&reliaburger::meat::AppId::new("after-hook", "default"))
    );
    let hook_id = state
        .task_arrays
        .deployments()
        .find(|(id, _)| *id == operation)
        .unwrap()
        .1
        .hook_runs["prepare"];
    assert_eq!(
        state.task_arrays.get(hook_id).unwrap().state.status(),
        reliaburger::meat::task_array_state::TaskArrayStatus::Succeeded
    );
    shutdown.cancel();
    server.await.unwrap();
    agent_task.await.unwrap();
    council.shutdown().await.unwrap();
    for owner in runtime.launch_inventory().await.unwrap().unwrap() {
        assert!(matches!(
            runtime.state(&owner.instance_id).await,
            Ok(ContainerState::Stopped) | Err(reliaburger::grill::GrillError::NotFound { .. })
        ));
    }
}
