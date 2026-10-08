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
    for isolation in ["fresh-container", "reusable-container"] {
        delegated_namespace_isolation_and_policy_loss(isolation).await;
    }
}

#[cfg(feature = "ebpf")]
async fn delegated_namespace_isolation_and_policy_loss(isolation: &str) {
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
    let root_path = tempfile::Builder::new()
        .prefix("rb-639-ns-")
        .tempdir()
        .unwrap()
        .keep();
    let root = root_path.as_path();
    eprintln!("namespace fixture ({isolation}): {}", root.display());
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = RuncGrill::new(
        root.join("bundles"),
        images,
        false,
        root.join("state"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap();
    let kernel = Arc::new(tokio::sync::Mutex::new(
        OnionEbpf::load_embedded(Path::new("/sys/fs/cgroup")).unwrap(),
    ));
    let policy = TaskNamespacePolicy::recover(kernel.clone(), root)
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
            toml::from_str(&format!(
                "image='{image}'\nnamespace='{namespace}'\nisolation='{isolation}'"
            ))
            .unwrap(),
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
        serde_json::from_slice(&std::fs::read(root.join("batch-namespaces.json")).unwrap())
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
    let reason = match &retired.outcome {
        AttemptOutcome::Unknown { reason } => reason.as_str(),
        _ => "",
    };
    assert!(
        reason.contains("namespace enforcement was lost")
            || String::from_utf8_lossy(&retired.output.head)
                .contains("namespace enforcement was lost"),
        "{retired:?}"
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
    TaskNamespacePolicy::recover(kernel.clone(), root)
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
    std::fs::remove_dir_all(root).unwrap();
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
    let root = tempfile::Builder::new()
        .prefix("rb-639-api-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("common mixed/reuse API fixture: {}", root.display());
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.as_path().join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = RuncGrill::new(
        root.as_path().join("bundles"),
        images,
        false,
        root.as_path().join("runtime"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap();
    let runtime = AnyGrill::with_host_processes(
        runtime,
        &root.join("instances"),
        env!("CARGO_BIN_EXE_bun").into(),
    );
    let host_policy = reliaburger::config::process_workloads::ProcessWorkloadsConfig {
        allowed_binaries: vec!["/usr/bin/printf".into()],
        mount_isolation: false,
        ..Default::default()
    };
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
        runtime.clone(),
        PortAllocator::new(43000, 44000),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_node_capacity(2000, 128);
    agent.set_process_config(host_policy.clone());
    let runner = OwnedRunner::for_data_dir(runtime.clone(), root.as_path())
        .unwrap()
        .with_secrets(council.clone(), ikm);
    let executor = TaskArrayNode::new(
        TaskArrayNodeConfig::for_data_dir(root.as_path(), host_policy),
        NodeRunner::Owned(Box::new(runner)),
    )
    .with_budget(agent.execution_budget());
    let service = Arc::new(
        TaskArrayService::with_timings(
            Some(Arc::new(executor)),
            Duration::from_millis(50),
            Duration::from_secs(3),
        )
        .with_storage(root.as_path())
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
    for (name, template, count) in [
        (
            "host-job",
            json!({"exec":"/usr/bin/printf","command":["host:%s", "{index}"]}),
            1,
        ),
        (
            "reused-jobs",
            json!({"image":image,"isolation":"reusable-container","cpu":"100m","memory":"32Mi","command":["/bin/sh","-c","test ! -e /tmp/previous && echo state >/tmp/previous && test \"$1\" -ge 0","task","{index}"]}),
            1000,
        ),
    ] {
        let started = std::time::Instant::now();
        let response = http.post(format!("{base}/v1/jobs/runs")).json(&json!({"name":name,"request_id":name,"definition":{"template":template,"tasks":{"count":count,"chunk_size":128,"max_attempts":1}}})).send().await.unwrap();
        assert_eq!(response.status(), 202, "{}", response.text().await.unwrap());
        let run = response.json::<Value>().await.unwrap()["batch_id"]
            .as_u64()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let summary = http
                    .get(format!("{base}/v1/batch/{run}"))
                    .send()
                    .await
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap();
                if summary["done"] == true {
                    assert_eq!(summary["succeeded"], count, "{summary}");
                    assert_eq!(summary["failed"], 0, "{summary}");
                    assert_eq!(summary["retried"], 0, "{summary}");
                    assert_eq!(summary["active_commands"], 0, "{summary}");
                    if count > 1 {
                        assert_eq!(summary["isolation"], "reusable-container");
                    }
                    eprintln!(
                        "real common API {name}: {count} unique accepted successes in {:.3}s",
                        started.elapsed().as_secs_f64()
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        for index in [0, count - 1] {
            let details = http
                .get(format!(
                    "{base}/v1/batch/{run}/results?index={index}&limit=1"
                ))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            assert_eq!(details["rows"][0]["index"], index, "{details}");
            assert_eq!(details["rows"][0]["succeeded"], true, "{details}");
        }
    }
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
    std::fs::remove_dir_all(root).unwrap();
}

/// Reuse keeps one live container while every command gets independent scratch,
/// credentials, cgroup limits and whole-tree retirement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc and pinned images; run with make test-linux"]
async fn runc_reusable_commands_keep_the_container_but_retire_task_state_and_idle_resources() {
    use reliaburger::bun::task_executor::{AttemptOutcome, TaskInvocation, TaskRunner};
    use reliaburger::bun::{execution_budget::ExecutionBudget, task_runtime::OwnedRunner};
    use reliaburger::grill::InstanceIdentity;
    use reliaburger::meat::Resources;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::Builder::new()
        .prefix("rb-639-executor-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("reusable fixture: {}", root.display());
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.as_path().join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = RuncGrill::new(
        root.as_path().join("bundles"),
        images,
        false,
        root.as_path().join("state"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap();
    let budget = ExecutionBudget::new(Resources::new(1000, 128 << 20, 0));
    let app = budget
        .try_acquire(Resources::new(500, 64 << 20, 0))
        .unwrap();
    let runner = Arc::new(
        OwnedRunner::for_data_dir(runtime.clone(), root.as_path())
            .unwrap()
            .with_budget(budget.clone()),
    );
    let prefix = std::fs::read_to_string(root.as_path().join("batch-executor-id")).unwrap();
    let id =
        InstanceIdentity::new("rbtest-reuse", format!("executor-{prefix}-reuse"), 0).instance_id();
    let template: reliaburger::config::job::JobSpec = toml::from_str(&format!("image='{image}'\nnamespace='rbtest-reuse'\nisolation='reusable-container'\ncpu='100m'\nmemory='32Mi'")).unwrap();
    let command = "test ! -e /tmp/previous && test ! -e /dev/shm/previous && echo state >/tmp/previous && echo state >/dev/shm/previous && printf complete";
    let task = TaskInvocation {
        template: Some(Box::new(template.clone())),
        index: 0,
        attempt: 1,
        program: "/unused".into(),
        args: vec!["/bin/sh".into(), "-c".into(), command.into()],
        env: vec![
            ("RELIABURGER_TASK_COUNT".into(), "2".into()),
            ("RELIABURGER_BATCH_ID".into(), "42".into()),
        ],
    };
    let cancel = CancellationToken::new();
    let first = runner.run(&task, Duration::from_secs(20), &cancel).await;
    assert_eq!(
        first.outcome,
        AttemptOutcome::Exited { code: 0 },
        "{:?}",
        first
    );
    assert!(String::from_utf8_lossy(&first.output.head).contains("complete"));
    let base = reliaburger::grill::cgroup::instance_cgroup_path(
        "rbtest-reuse",
        &format!("executor-{prefix}-reuse"),
        &id,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(base.join("task/cpu.max"))
            .unwrap()
            .trim(),
        "10000 100000"
    );
    assert_eq!(
        std::fs::read_to_string(base.join("task/memory.max"))
            .unwrap()
            .trim(),
        (32u64 << 20).to_string()
    );
    assert!(
        std::fs::read_to_string(base.join("task/cgroup.procs"))
            .unwrap()
            .is_empty()
    );
    let launcher = runtime.pid(&id).await.unwrap().unwrap();
    assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Running);
    assert_eq!(
        budget.available(),
        Resources::new(390, 24 << 20, 0),
        "idle profile plus helper still owns capacity"
    );
    let next = runner.run(&task, Duration::from_secs(20), &cancel).await;
    assert_eq!(
        next.outcome,
        AttemptOutcome::Exited { code: 0 },
        "{:?}",
        next
    );
    assert_eq!(
        runtime.pid(&id).await.unwrap(),
        Some(launcher),
        "a second command must retain the same owned container"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while budget.available() != Resources::new(500, 64 << 20, 0) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Stopped);
    // Explicit environment values and escaped process groups cannot survive
    // a command boundary inside the retained PID namespace.
    let mut transient = task.clone();
    transient.env.push(("ONE_COMMAND".into(), "private".into()));
    transient.args = vec!["/bin/sh".into(), "-c".into(), "test \"$ONE_COMMAND\" = private || exit 1; setsid /bin/sh -c 'trap \"\" TERM; while :; do sleep 1; done' >/dev/null 2>&1 & printf '%s' \"$!\"".into()];
    let escaped = runner
        .run(&transient, Duration::from_secs(20), &cancel)
        .await;
    assert_eq!(
        escaped.outcome,
        AttemptOutcome::Exited { code: 0 },
        "{escaped:?}"
    );
    let descendant: u32 = String::from_utf8(escaped.output.head)
        .unwrap()
        .parse()
        .unwrap();
    let mut clean = task.clone();
    clean.args = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "test -z \"${{ONE_COMMAND+x}}\" && test ! -r /run/rb-executor/helper && test ! -e /proc/{descendant} && printf clean"
        ),
    ];
    let cleaned = runner.run(&clean, Duration::from_secs(20), &cancel).await;
    assert_eq!(
        cleaned.outcome,
        AttemptOutcome::Exited { code: 0 },
        "{cleaned:?}"
    );
    assert!(String::from_utf8_lossy(&cleaned.output.head).contains("clean"));
    let mut memory_hog = task.clone();
    memory_hog.args = vec![
        "/bin/awk".into(),
        "BEGIN { value=\"x\"; for (i=0;i<27;i++) value=value value; print length(value) }".into(),
    ];
    let limited = runner
        .run(&memory_hog, Duration::from_secs(20), &cancel)
        .await;
    assert_eq!(
        limited.outcome,
        AttemptOutcome::Signalled { signal: 9 },
        "{limited:?}"
    );
    let events = std::fs::read_to_string(base.join("task/memory.events")).unwrap();
    assert!(
        events.lines().any(|line| line
            .strip_prefix("oom_kill ")
            .is_some_and(|count| count.parse::<u64>().unwrap() > 0)),
        "memory limit did not kill the command: {events}"
    );
    assert_eq!(
        runner
            .run(&clean, Duration::from_secs(20), &cancel)
            .await
            .outcome,
        AttemptOutcome::Exited { code: 0 }
    );
    let mut long = task.clone();
    long.args = vec![
        "/bin/sh".into(),
        "-c".into(),
        "setsid /bin/sh -c 'trap \"\" TERM; sleep 60' >/dev/null 2>&1 & sleep 60".into(),
    ];
    assert_eq!(
        runner
            .run(&long, Duration::from_millis(300), &cancel)
            .await
            .outcome,
        AttemptOutcome::TimedOut
    );
    assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Stopped);
    assert!(!base.exists(), "timeout must remove the killed task cgroup");
    assert_eq!(budget.available(), Resources::new(500, 64 << 20, 0));

    let stopped = CancellationToken::new();
    let active = tokio::spawn({
        let runner = runner.clone();
        let stopped = stopped.clone();
        let long = long.clone();
        async move { runner.run(&long, Duration::from_secs(20), &stopped).await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if std::fs::read_to_string(base.join("task/cgroup.procs")).is_ok_and(|p| !p.is_empty())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancel regression never started its task");
    tokio::time::timeout(Duration::from_secs(5), async {
        while runner.active_commands(42, Some(&template)).await != Some(1) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("active metric never observed its verified command start");
    stopped.cancel();
    let cancelled = active.await.unwrap();
    assert_eq!(runner.active_commands(42, Some(&template)).await, Some(0));
    assert_eq!(cancelled.outcome, AttemptOutcome::Cancelled);
    assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Stopped);
    assert!(
        !base.exists(),
        "cancellation must remove the killed task cgroup"
    );
    assert_eq!(budget.available(), Resources::new(500, 64 << 20, 0));
    // Eight callers fit only one retained 40 MiB profile in the app's remaining
    // 64 MiB. Compatible waiters must reuse it instead of churning containers.
    use reliaburger::bun::task_executor::{ChunkWork, PoolConfig, TaskPool};
    use reliaburger::meat::task_array::{ChunkId, TaskArraySpec};
    let before = runtime.launch_inventory().await.unwrap().unwrap().len();
    let concurrent =
        Arc::new(OwnedRunner::with_slot_count(runtime.clone(), 8).with_budget(budget.clone()));
    let pool = TaskPool::with_node_slots(
        concurrent,
        PoolConfig::with_concurrency(8),
        Arc::new(tokio::sync::Semaphore::new(8)),
    )
    .with_budget(budget.clone(), Resources::new(100, 32 << 20, 0));
    let work = ChunkWork {
        replay_unknown: true,
        batch_id: 99,
        template: Some(Box::new(template)),
        spec: TaskArraySpec::with_count(16),
        chunk: ChunkId(0),
        grant_attempt: 1,
        program: "/unused".into(),
        args: task.args.clone(),
        env: vec![],
    };
    let completed = pool.run_chunk(&work, &cancel).await;
    assert_eq!(completed.result.succeeded, 16, "{:?}", completed.records);
    let after = runtime.launch_inventory().await.unwrap().unwrap().len();
    assert_eq!(
        after - before,
        1,
        "compatible resource waiters churned owned containers instead of reusing the one profile that fits"
    );
    // Queueing for an already-admitted compatible slot is not command run
    // time. Eight 300 ms commands each fit their one-second attempt timeout,
    // even though their serial execution takes more than two seconds.
    let mut timed = work.clone();
    timed.batch_id = 100;
    timed.spec = TaskArraySpec::with_count(8);
    timed.spec.task_timeout_secs = 1;
    timed.spec.max_attempts = 1;
    timed.args = vec!["/bin/sh".into(), "-c".into(), "sleep 0.3".into()];
    let waited = pool.run_chunk(&timed, &cancel).await;
    assert_eq!(
        waited.result.succeeded, 8,
        "compatible queue wait consumed command timeout: {:?}",
        waited.records
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while budget.available() != Resources::new(500, 64 << 20, 0) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    drop(app);
    std::fs::remove_dir_all(root).unwrap();
}

/// One Bun routes host commands and images independently and retains that route
/// through a cold runtime restart and a positively retired backend change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, pinned test image, ip and nft; run with make test-linux"]
async fn runc_mixed_runtimes_run_host_and_container_jobs_and_recover_both_original_owners() {
    use reliaburger::grill::{AnyGrill, InstanceId};
    async fn retire(runtime: &AnyGrill, id: &InstanceId) {
        runtime.kill(id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while runtime.state(id).await.unwrap() != ContainerState::Stopped {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("original runtime did not positively retire");
    }
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::Builder::new()
        .prefix("rb-mixed-jobs-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("mixed runtime fixture: {}", root.display());
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = || {
        AnyGrill::with_host_processes(
            RuncGrill::new(
                root.join("bundles"),
                images.clone(),
                false,
                root.join("state"),
                env!("CARGO_BIN_EXE_bun").into(),
            )
            .unwrap(),
            &root.join("instances"),
            env!("CARGO_BIN_EXE_bun").into(),
        )
    };
    let live = runtime();
    let host = InstanceId("rbtest-mixed__host-0".into());
    let container = InstanceId("rbtest-mixed__image-0".into());
    let host_job: reliaburger::config::job::JobSpec =
        toml::from_str("exec='/usr/bin/sleep'\ncommand=['60']").unwrap();
    let image_job: reliaburger::config::job::JobSpec =
        toml::from_str(&format!("image='{image}'\ncommand=['/bin/sleep','60']")).unwrap();
    let host_spec = reliaburger::grill::generate_job_oci_spec(
        "host",
        "rbtest-mixed",
        &host_job,
        "/sys/fs/cgroup/reliaburger/rbtest-mixed/host/0",
        None,
    );
    let image_spec = reliaburger::grill::generate_job_oci_spec(
        "image",
        "rbtest-mixed",
        &image_job,
        "/sys/fs/cgroup/reliaburger/rbtest-mixed/image/0",
        None,
    );
    let (a, b) = tokio::join!(
        live.create(&host, &host_spec),
        live.create(&container, &image_spec)
    );
    a.unwrap();
    b.unwrap();
    let (a, b) = tokio::join!(live.start(&host), live.start(&container));
    a.unwrap();
    b.unwrap();
    assert_eq!(live.state(&host).await.unwrap(), ContainerState::Running);
    assert_eq!(
        live.state(&container).await.unwrap(),
        ContainerState::Running
    );
    assert_eq!(live.container_ip(&host).await, None);
    assert!(live.container_ip(&container).await.is_some());
    assert!(live.workload_cgroup(&host).await.unwrap().is_none());
    assert!(live.workload_cgroup(&container).await.unwrap().is_some());
    let original = live.launch_inventory().await.unwrap().unwrap();
    assert_eq!(original.len(), 2);
    assert!(
        original
            .iter()
            .any(|entry| entry.instance_id == host && entry.spec.host_process)
    );
    assert!(
        original
            .iter()
            .any(|entry| entry.instance_id == container && !entry.spec.host_process)
    );
    drop(live);
    let recovered = runtime();
    assert_eq!(
        recovered.state(&host).await.unwrap(),
        ContainerState::Running
    );
    assert_eq!(
        recovered.state(&container).await.unwrap(),
        ContainerState::Running
    );
    assert!(recovered.create(&container, &host_spec).await.is_err());
    retire(&recovered, &container).await;
    assert_eq!(
        recovered.state(&container).await.unwrap(),
        ContainerState::Stopped
    );
    assert_eq!(
        recovered.state(&host).await.unwrap(),
        ContainerState::Running
    );
    recovered.create(&container, &host_spec).await.unwrap();
    recovered.start(&container).await.unwrap();
    assert_eq!(recovered.container_ip(&container).await, None);
    assert_eq!(
        recovered.state(&container).await.unwrap(),
        ContainerState::Running
    );
    retire(&recovered, &container).await;
    retire(&recovered, &host).await;
    for id in [&host, &container] {
        assert_eq!(recovered.state(id).await.unwrap(), ContainerState::Stopped);
    }
    recovered.create(&host, &image_spec).await.unwrap();
    recovered.start(&host).await.unwrap();
    assert!(recovered.container_ip(&host).await.is_some());
    retire(&recovered, &host).await;
    assert_eq!(
        recovered.state(&host).await.unwrap(),
        ContainerState::Stopped
    );
    assert_eq!(
        recovered.launch_inventory().await.unwrap().unwrap().len(),
        2
    );
    std::fs::remove_dir_all(root).unwrap();
}
