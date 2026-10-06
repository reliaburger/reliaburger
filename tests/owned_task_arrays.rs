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
        ChunkWork{batch_id:id,template:Some(Box::new(template)),spec:TaskArraySpec{chunk_size:6,max_attempts:1,..TaskArraySpec::with_count(6)},chunk:ChunkId(0),grant_attempt:1,program:"/unused".into(),args:vec!["/bin/sh".into(),"-c".into(),"test ! -e /tmp/previous; echo task > /tmp/previous; test -r /proc/self/cgroup; sleep 0.05".into()],env:vec![]}
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
    tokio::time::sleep(Duration::from_secs(2)).await;
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
        matches!(retired.outcome, AttemptOutcome::SpawnFailed { .. }),
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
