//! Image-backed delegated jobs, run by the warmed-image Linux gate.
//! The OCI interruption driver deliberately owns a separate, offline fixture set.
#![cfg(target_os = "linux")]

use reliaburger::grill::runc::RuncGrill;
use reliaburger::grill::{ContainerState, Grill, ImageStore};
#[cfg(feature = "ebpf")]
use std::path::Path;
use std::time::Duration;

#[path = "support/bun_process.rs"]
mod bun_process;
#[path = "support/cluster.rs"]
mod cluster_support;

/// Retired, and then forgotten once that retirement is proven (#678).
async fn assert_retired(runtime: &impl Grill, id: &reliaburger::grill::InstanceId) {
    match runtime.state(id).await {
        Ok(ContainerState::Stopped) | Err(reliaburger::grill::GrillError::NotFound { .. }) => {}
        other => panic!("{} has not retired: {other:?}", id.0),
    }
}

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
    // Retired executors are forgotten (#678): nothing remains, at any task count.
    assert_eq!(retained, 0, "retired runtime metadata was kept");
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
        run: None,
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

/// Runc whose next executor starts report the failure #759 saw in CI: a
/// runtime command past its five-second bound. The real start runs first,
/// so the pool holds a live executor it was told failed to start.
#[derive(Clone)]
struct StartMissesItsBound {
    runtime: RuncGrill,
    failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    starts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl StartMissesItsBound {
    fn starts(&self) -> usize {
        self.starts.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Grill for StartMissesItsBound {
    async fn create(
        &self,
        id: &reliaburger::grill::InstanceId,
        spec: &reliaburger::grill::OciSpec,
    ) -> Result<(), reliaburger::grill::GrillError> {
        self.runtime.create(id, spec).await
    }
    async fn start(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<(), reliaburger::grill::GrillError> {
        use std::sync::atomic::Ordering;
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.runtime.start(id).await?;
        // Only this test's one slot starts executors, so the check and the
        // decrement never race.
        if self.failures.load(Ordering::SeqCst) > 0 {
            self.failures.fetch_sub(1, Ordering::SeqCst);
            return Err(reliaburger::grill::GrillError::StateUnavailable {
                instance: id.clone(),
                reason: "runtime command CommandId(injected) did not finish within 5s".into(),
            });
        }
        Ok(())
    }
    async fn stop(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<(), reliaburger::grill::GrillError> {
        self.runtime.stop(id).await
    }
    async fn kill(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<(), reliaburger::grill::GrillError> {
        self.runtime.kill(id).await
    }
    async fn state(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<ContainerState, reliaburger::grill::GrillError> {
        self.runtime.state(id).await
    }
    async fn forget_retired(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<(), reliaburger::grill::GrillError> {
        self.runtime.forget_retired(id).await
    }
    async fn launch_inventory(
        &self,
    ) -> Result<Option<Vec<reliaburger::grill::RuntimeLaunch>>, reliaburger::grill::GrillError>
    {
        self.runtime.launch_inventory().await
    }
    async fn container_ip(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Option<std::net::Ipv4Addr> {
        self.runtime.container_ip(id).await
    }
    fn runtime_kind(&self) -> reliaburger::grill::records::RuntimeKind {
        self.runtime.runtime_kind()
    }
    fn honours_cgroup_path(&self) -> bool {
        self.runtime.honours_cgroup_path()
    }
    fn reusable_runtime(&self) -> Option<RuncGrill> {
        self.runtime.reusable_runtime()
    }
}

/// An executor start that misses a runtime command's bound costs the
/// command nothing: the pool retires that executor and runs the command
/// once on a fresh one. A start that keeps failing still fails the command,
/// after one retry, with nothing held (#759).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, pinned test image, ip and nft; run with make test-linux"]
async fn runc_executor_start_that_misses_its_bound_retries_on_a_fresh_executor() {
    use reliaburger::bun::execution_budget::ExecutionBudget;
    use reliaburger::bun::task_executor::{AttemptOutcome, TaskInvocation, TaskRunner};
    use reliaburger::bun::task_runtime::OwnedRunner;
    use reliaburger::meat::Resources;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::Builder::new()
        .prefix("rb-759-start-")
        .tempdir()
        .unwrap();
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.path().join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = StartMissesItsBound {
        runtime: RuncGrill::new(
            root.path().join("bundles"),
            images,
            false,
            root.path().join("state"),
            env!("CARGO_BIN_EXE_bun").into(),
        )
        .unwrap(),
        failures: Arc::new(AtomicUsize::new(1)),
        starts: Arc::new(AtomicUsize::new(0)),
    };
    let budget = ExecutionBudget::new(Resources::new(1000, 256 << 20, 0));
    let runner = OwnedRunner::with_slot_count(runtime.clone(), 1).with_budget(budget.clone());
    let cancel = CancellationToken::new();
    let task = |cpu: &str, script: &str| {
        TaskInvocation {
        template: Some(Box::new(
            toml::from_str(&format!(
                "image='{image}'\nnamespace='rbtest-start'\nruntime='shared-runc'\ncpu='{cpu}'\nmemory='32Mi'"
            ))
            .unwrap(),
        )),
        index: 0,
        attempt: 1,
        program: "/unused".into(),
        args: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: vec![],
        run: None,
    }
    };
    // The command never reached the first executor; the second runs it.
    let ran = runner
        .run(
            &task("100m", "printf ran"),
            Duration::from_secs(60),
            &cancel,
        )
        .await;
    assert_eq!(ran.outcome, AttemptOutcome::Exited { code: 0 }, "{ran:?}");
    assert_eq!(ran.output.head, b"ran", "{ran:?}");
    assert_eq!(runtime.starts(), 2, "the failed start was not retried");
    // Every later start fails: a different profile replaces the warm
    // executor, tries twice and gives up with the runtime's reason, as a
    // launch failure the task pool may retry on a later attempt (#889).
    runtime.failures.store(usize::MAX, Ordering::SeqCst);
    let refused = runner
        .run(
            &task("200m", "printf ran"),
            Duration::from_secs(60),
            &cancel,
        )
        .await;
    match &refused.outcome {
        AttemptOutcome::LaunchFailed { reason } => {
            assert!(reason.contains("did not finish within 5s"), "{reason}")
        }
        other => panic!("a start that keeps failing was not a launch failure: {other:?}"),
    }
    assert_eq!(runtime.starts(), 4, "the retry was not bounded");
    // Nothing is held once the refused command returns.
    assert_eq!(budget.available(), budget.capacity());
    let executors = runtime
        .launch_inventory()
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .filter(|launch| launch.instance_id.0.contains("__executor-"))
        .map(|launch| launch.instance_id)
        .collect::<Vec<_>>();
    for id in &executors {
        assert_retired(&runtime, id).await;
    }
}

/// A loopback stand-in for a registry that counts every connection. It
/// forwards to `upstream` until `throttled` is set, then answers each one
/// `429 Too Many Requests`, as an anonymous registry does under a burst.
async fn counting_registry_proxy(
    upstream: String,
    connections: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    throttled: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> String {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            connections.fetch_add(1, Ordering::SeqCst);
            let upstream = upstream.clone();
            let throttled = throttled.load(Ordering::SeqCst);
            tokio::spawn(async move {
                if throttled {
                    let mut request = [0u8; 4096];
                    let _ = client.read(&mut request).await;
                    let body =
                        r#"{"errors":[{"code":"TOOMANYREQUESTS","message":"rate limited"}]}"#;
                    let response = format!(
                        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = client.write_all(response.as_bytes()).await;
                    return;
                }
                if let Ok(mut server) = tokio::net::TcpStream::connect(&upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                }
            });
        }
    });
    address
}

/// Cold shared-runc executors for a digest-pinned image that is already
/// unpacked read nothing from the registry: not in the pool's pull, not in
/// runc's create. So a burst of them can't trip a rate limit (#790).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires root, runc, pinned test image, ip and nft; run with make test-linux"]
async fn runc_warm_pinned_executor_cold_starts_make_no_registry_requests() {
    use reliaburger::bun::execution_budget::ExecutionBudget;
    use reliaburger::bun::task_executor::{AttemptOutcome, TaskInvocation, TaskRunner};
    use reliaburger::bun::task_runtime::OwnedRunner;
    use reliaburger::grill::ImageMirrors;
    use reliaburger::grill::image::ImageReference;
    use reliaburger::meat::Resources;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;
    const BURST: usize = 4;
    assert!(nix::unistd::geteuid().is_root());
    let mirror_env = reliaburger::testkit::pinned_images::TEST_IMAGE_MIRROR_ENV;
    let mirror = std::env::var(mirror_env).unwrap_or_else(|_| {
        panic!("{mirror_env} is unset: run with make test-linux, which serves the pinned images")
    });
    let connections = Arc::new(AtomicUsize::new(0));
    let throttled = Arc::new(AtomicBool::new(false));
    let proxy =
        counting_registry_proxy(mirror, Arc::clone(&connections), Arc::clone(&throttled)).await;
    let root = tempfile::Builder::new()
        .prefix("rb-790-warm-")
        .tempdir()
        .unwrap()
        .keep();
    // Declared before the runtime, so it drops after it: a failed assertion
    // still releases the executors' namespaces for the next runc test.
    let _residue = ExecutorResidue { root: root.clone() };
    let node = root.join("node-0");
    std::fs::create_dir_all(&node).unwrap();
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let registry = ImageReference::parse(image).unwrap().registry;
    let images = ImageStore::new(root.join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(ImageMirrors::new([(registry, proxy)].into()).unwrap());
    images.pull_and_unpack(image).await.unwrap();
    assert!(
        connections.load(Ordering::SeqCst) > 0,
        "the warm-up pull bypassed the counting registry"
    );
    // From here the registry refuses everything, and every request counts.
    throttled.store(true, Ordering::SeqCst);
    connections.store(0, Ordering::SeqCst);
    let runtime = RuncGrill::new(
        root.join("bundles"),
        images,
        false,
        root.join("state"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap();
    let budget = ExecutionBudget::new(Resources::new(1000, 512 << 20, 0));
    // A data directory, so the residue guard can name this runner's executors.
    let runner = OwnedRunner::for_data_dir(runtime.clone(), &node)
        .unwrap()
        .with_budget(budget.clone());
    let template: reliaburger::config::job::JobSpec = toml::from_str(&format!(
        "image='{image}'\nnamespace='rbtest-warm-pin'\nruntime='shared-runc'\ncpu='100m'\nmemory='32Mi'"
    ))
    .unwrap();
    let tasks: Vec<TaskInvocation> = (0..BURST as u32)
        .map(|index| TaskInvocation {
            template: Some(Box::new(template.clone())),
            index,
            attempt: 1,
            program: "/unused".into(),
            // Overlapping commands keep every slot busy, so each one
            // cold-starts its own executor.
            args: vec!["/bin/sh".into(), "-c".into(), "sleep 1; printf ran".into()],
            env: vec![],
            run: None,
        })
        .collect();
    let cancel = CancellationToken::new();
    let attempts = futures_util::future::join_all(
        tasks
            .iter()
            .map(|task| runner.run(task, Duration::from_secs(60), &cancel)),
    )
    .await;
    for attempt in &attempts {
        assert_eq!(
            attempt.outcome,
            AttemptOutcome::Exited { code: 0 },
            "{attempt:?}"
        );
        assert_eq!(attempt.output.head, b"ran", "{attempt:?}");
    }
    let executors = runtime
        .launch_inventory()
        .await
        .unwrap()
        .unwrap()
        .into_iter()
        .filter(|launch| launch.instance_id.0.contains("__executor-"))
        .map(|launch| launch.instance_id)
        .collect::<Vec<_>>();
    assert_eq!(executors.len(), BURST, "every slot cold-started");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "warm pinned cold starts asked the registry"
    );
    // Idle executors retire and hand back everything they held.
    tokio::time::timeout(Duration::from_secs(15), async {
        while budget.available() != budget.capacity() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("idle executors never retired");
    for id in &executors {
        assert_retired(&runtime, id).await;
    }
}

/// Delegated sources inherit their tenant before their first connect; losing
/// that live binding retires the process tree instead of running unguarded.
#[cfg(feature = "ebpf")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, eBPF, pinned test image, ip and nft; run with make test-linux"]
async fn runc_delegated_namespace_isolation_and_policy_loss_retire_the_original_owner() {
    for isolation in ["runc", "shared-runc"] {
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
    let policy = TaskNamespacePolicy::recover(kernel.clone(), root, Default::default(), None)
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
                "image='{image}'\nnamespace='{namespace}'\nruntime='{isolation}'"
            ))
            .unwrap(),
        )),
        index: 0,
        attempt: 1,
        program: "/unused".into(),
        args: vec!["/bin/sh".into(), "-c".into(), command],
        env: vec![],
        run: None,
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
    // A fresh container's owner reports the loss in its output; a reusable
    // executor's pool reports it as the attempt's reason.
    let reported = if isolation == "shared-runc" {
        reason.to_string()
    } else {
        String::from_utf8_lossy(&retired.output.head).into_owned()
    };
    assert!(
        reported.contains("namespace enforcement was lost"),
        "{isolation}: {retired:?}"
    );
    for owner in runtime.launch_inventory().await.unwrap().unwrap() {
        assert_eq!(
            runtime.state(&owner.instance_id).await.unwrap(),
            ContainerState::Stopped
        );
    }
    // These two host backends were published directly by this fixture, without
    // application discovery owners. Retire only those known keys before testing
    // application adoption; unknown kernel backends must still be refused.
    {
        let mut kernel = kernel.lock().await;
        for service in services.resolve_all() {
            BpfServiceMap::new()
                .remove_backends_bpf(&mut kernel, service.vip, service.port)
                .unwrap();
        }
    }
    // A delegated source has its own journal, not an application policy owner.
    // Republish the recorded identity after positive retirement: application
    // preflight must recognise it without deleting it or claiming its firewall.
    reliaburger::sesame::firewall::write_cgroup_namespace_entry(
        &mut kernel.lock().await.bpf,
        cgroup,
        reliaburger::onion::vip::name_to_id(&namespace),
    )
    .unwrap();
    let (_commands, commands) = tokio::sync::mpsc::channel(8);
    let mut adopter = reliaburger::bun::agent::BunAgent::new(
        runtime.clone(),
        reliaburger::grill::PortAllocator::new(43000, 44000),
        commands,
        CancellationToken::new(),
    );
    adopter.set_records_dir(root.join("instances"));
    adopter.set_onion_ebpf(kernel.clone()).await;
    adopter
        .recover_discovery_ownership(&root.join("discovery"))
        .await
        .unwrap();
    adopter.adopt_recorded_instances().await.unwrap();
    assert_eq!(
        reliaburger::sesame::firewall::read_firewall_state(&mut kernel.lock().await.bpf, cgroup, 0)
            .unwrap()
            .source_namespace_id,
        Some(reliaburger::onion::vip::name_to_id(&namespace)),
        "application preflight removed the delegated owner's source",
    );
    // The startup pass clears only recorded, same-boot ancestry after owners retire.
    drop(runner);
    drop(policy);
    TaskNamespacePolicy::recover(kernel.clone(), root, Default::default(), None)
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
            json!({"runtime":"process","exec":"/usr/bin/printf","command":["host:%s", "{index}"]}),
            1,
        ),
        (
            "reused-jobs",
            json!({"image":image,"runtime":"shared-runc","cpu":"100m","memory":"32Mi","command":["/bin/sh","-c","test ! -e /tmp/previous && echo state >/tmp/previous && test \"$1\" -ge 0","task","{index}"]}),
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
                        assert_eq!(summary["runtime"], "shared-runc");
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
    let template: reliaburger::config::job::JobSpec = toml::from_str(&format!("image='{image}'\nnamespace='rbtest-reuse'\nruntime='shared-runc'\ncpu='100m'\nmemory='32Mi'")).unwrap();
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
        run: Some(reliaburger::bun::task_executor::RunIdentity {
            batch_id: 42,
            task_count: 2,
            job_name: None,
        }),
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
    assert_retired(&runtime, &id).await;
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
    assert_retired(&runtime, &id).await;
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
    assert_retired(&runtime, &id).await;
    assert!(
        !base.exists(),
        "cancellation must remove the killed task cgroup"
    );
    assert_eq!(budget.available(), Resources::new(500, 64 << 20, 0));
    // Eight callers fit only one retained 40 MiB profile in the app's remaining
    // 64 MiB. Compatible waiters must reuse it instead of churning containers.
    use reliaburger::bun::task_executor::{ChunkWork, PoolConfig, TaskPool};
    use reliaburger::meat::task_array::{ChunkId, TaskArraySpec};
    // Retired executors are forgotten (#678), so the inventory cannot count
    // churn after the fact. Sample the generations that ever existed instead.
    let sampling = CancellationToken::new();
    let generations = tokio::spawn({
        let runtime = runtime.clone();
        let sampling = sampling.clone();
        async move {
            let mut seen = std::collections::HashSet::new();
            while !sampling.is_cancelled() {
                for launch in runtime.launch_inventory().await.unwrap().unwrap() {
                    if launch.instance_id.0.starts_with("rbtest-reuse__") {
                        seen.insert(launch.generation);
                    }
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            seen.len()
        }
    });
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
    sampling.cancel();
    assert_eq!(
        generations.await.unwrap(),
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
        toml::from_str("runtime='process'\nexec='/usr/bin/sleep'\ncommand=['60']").unwrap();
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

/// A node that ran `--runtime runc` and restarts with `--runtime mixed`
/// keeps its running containers. The mixed runtime routes each one to the
/// runc owner that launched it, so startup recovery can read them (#823).
/// The runc node ran with a workload resolver, which every launch records:
/// routing succeeds only once the mixed runtime has the same resolver (#839).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, pinned test image, ip and nft; run with make test-linux"]
async fn runc_containers_from_a_runc_agent_are_recovered_by_a_mixed_agent() {
    use reliaburger::grill::{AnyGrill, InstanceId};
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::Builder::new()
        .prefix("rb-runc-to-mixed-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("runtime switch fixture: {}", root.display());
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runc = || {
        RuncGrill::new(
            root.join("bundles"),
            images.clone(),
            false,
            root.join("state"),
            env!("CARGO_BIN_EXE_bun").into(),
        )
        .unwrap()
    };
    let container = InstanceId("rbtest-switch__frontend-2".into());
    let job: reliaburger::config::job::JobSpec =
        toml::from_str(&format!("image='{image}'\ncommand=['/bin/sleep','60']")).unwrap();
    let spec = reliaburger::grill::generate_job_oci_spec(
        "frontend",
        "rbtest-switch",
        &job,
        "/sys/fs/cgroup/reliaburger/rbtest-switch/frontend/2",
        None,
    );
    let resolver = runc().dns_gateway_address().unwrap();
    let single = AnyGrill::Runc(runc().with_dns_nameserver(resolver));
    single.create(&container, &spec).await.unwrap();
    single.start(&container).await.unwrap();
    assert_eq!(
        single.state(&container).await.unwrap(),
        ContainerState::Running
    );
    drop(single);

    // Bun builds the mixed runtime without a resolver and sets it later.
    let AnyGrill::Mixed(router) = AnyGrill::with_host_processes(
        runc(),
        &root.join("instances"),
        env!("CARGO_BIN_EXE_bun").into(),
    ) else {
        panic!("with_host_processes did not build the mixed runtime");
    };
    assert!(
        router.route_unrouted_launches().await.is_err(),
        "a launch recorded under another resolver was routed"
    );
    let router = router.with_container(runc().with_dns_nameserver(resolver));
    let mixed = AnyGrill::Mixed(router.clone());
    assert!(
        mixed.launch_inventory().await.is_err(),
        "an unrouted runc launch was readable before routing"
    );
    assert_eq!(
        router.route_unrouted_launches().await.unwrap(),
        vec![container.clone()]
    );
    let launches = mixed.launch_inventory().await.unwrap().unwrap();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].instance_id, container);
    assert_eq!(
        mixed.state(&container).await.unwrap(),
        ContainerState::Running
    );
    mixed.kill(&container).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while mixed.state(&container).await.unwrap() != ContainerState::Stopped {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the routed runc container did not retire");
    std::fs::remove_dir_all(root).unwrap();
}

/// #704: a task-array command in a reusable runc executor is listed for
/// Smoker while it runs, by its executor's task cgroup, and that cgroup holds
/// the command's processes and nothing else: stopping every process in it
/// freezes the task, and continuing them lets it finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc and pinned images; run with make test-linux"]
async fn runc_array_attempt_can_be_paused_and_resumed() {
    use reliaburger::bun::live_attempts::AttemptHandle;
    use reliaburger::bun::task_executor::{AttemptOutcome, TaskInvocation, TaskRunner};
    use reliaburger::bun::{execution_budget::ExecutionBudget, task_runtime::OwnedRunner};
    use reliaburger::meat::Resources;
    use reliaburger::smoker::types::WorkloadKind;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::Builder::new()
        .prefix("rb-704-pause-")
        .tempdir()
        .unwrap();
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
    let budget = ExecutionBudget::new(Resources::new(1000, 128 << 20, 0));
    let runner = Arc::new(
        OwnedRunner::for_data_dir(runtime.clone(), root.path())
            .unwrap()
            .with_budget(budget.clone()),
    );
    let attempts = runner.live_attempts();
    let template: reliaburger::config::job::JobSpec = toml::from_str(&format!(
        "image='{image}'\nnamespace='rbtest-pause'\nruntime='shared-runc'\ncpu='100m'\nmemory='32Mi'"
    ))
    .unwrap();
    // Two seconds of work, then a marker: a frozen task can't print it.
    let task = TaskInvocation {
        template: Some(Box::new(template)),
        index: 2,
        attempt: 1,
        program: "/unused".into(),
        args: vec!["/bin/sh".into(), "-c".into(), "sleep 2; printf done".into()],
        env: vec![],
        run: Some(reliaburger::bun::task_executor::RunIdentity {
            batch_id: 704,
            task_count: 3,
            job_name: Some("render".into()),
        }),
    };
    let running = {
        let runner = runner.clone();
        tokio::spawn(async move {
            runner
                .run(&task, Duration::from_secs(60), &CancellationToken::new())
                .await
        })
    };
    wait_until("the command is listed", || !attempts.snapshot().is_empty()).await;
    let listed = attempts.snapshot().remove(0);
    assert_eq!(listed.job, "render");
    assert_eq!(listed.index, 2);
    assert_eq!(listed.kind(), WorkloadKind::TaskArray);
    assert!(listed.shared_executor.is_some(), "{listed:?}");
    let AttemptHandle::TaskGroup(group) = &listed.handle else {
        panic!("a shared command is reached by its task cgroup: {listed:?}");
    };
    let pids: Vec<i32> = std::fs::read_to_string(group.join("cgroup.procs"))
        .unwrap()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();
    assert!(!pids.is_empty(), "the task cgroup holds the command");
    let signal = |signal| {
        for pid in &pids {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(*pid), signal);
        }
    };

    signal(nix::sys::signal::Signal::SIGSTOP);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(!running.is_finished(), "the paused task finished anyway");
    signal(nix::sys::signal::Signal::SIGCONT);

    let finished = tokio::time::timeout(Duration::from_secs(30), running)
        .await
        .expect("the resumed task never finished")
        .unwrap();
    assert_eq!(
        finished.outcome,
        AttemptOutcome::Exited { code: 0 },
        "{finished:?}"
    );
    assert!(String::from_utf8_lossy(&finished.output.head).contains("done"));
    assert!(
        attempts.snapshot().is_empty(),
        "a finished task stayed listed"
    );
    wait_until("the executor retires", || {
        budget.available() == budget.capacity()
    })
    .await;
}

/// Warm capacity remains usable while its reservation is retained; queued
/// commands must resolve secrets again after that wait, before execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, pinned test image, ip and nft; run with make test-linux"]
async fn runc_reusable_admission_preserves_warm_capacity_and_rechecks_queued_secrets() {
    use reliaburger::bun::{
        execution_budget::ExecutionBudget,
        task_array_node::{
            ArrayAssignment, ControlVersion, HeldChunk, NodeRunner, NodeSyncRequest, TaskArrayNode,
            TaskArrayNodeConfig,
        },
        task_executor::{AttemptOutcome, TaskInvocation, TaskRunner},
        task_runtime::OwnedRunner,
    };
    use reliaburger::council::{
        log_store::MemLogStore,
        network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter},
        node::CouncilNode,
        state_machine::CouncilStateMachine,
        types::{CouncilConfig, CouncilNodeInfo, RaftRequest},
    };
    use reliaburger::{
        config::process_workloads::ProcessWorkloadsConfig,
        grill::AnyGrill,
        meat::{
            Resources,
            task_array::{ChunkId, TaskArraySpec},
        },
        sesame::{
            secret,
            types::{AgeKeyScope, SecurityState},
        },
    };
    use std::{
        collections::BTreeMap,
        future::{Future, poll_fn},
        sync::Arc,
        task::Poll,
    };
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::Builder::new()
        .prefix("rb-639-executor-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("admission and revocation fixture: {}", root.display());
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
    let capacity = Resources::new(110, 40 << 20, 0);
    let budget = ExecutionBudget::new(capacity);
    let mut template: reliaburger::config::job::JobSpec = toml::from_str(&format!(
        "image='{image}'\nnamespace='rbtest-reuse'\nruntime='shared-runc'\ncpu='100m'\nmemory='32Mi'"
    )).unwrap();
    let node = TaskArrayNode::new(
        TaskArrayNodeConfig::for_data_dir(&root, ProcessWorkloadsConfig::default()),
        NodeRunner::Owned(Box::new(
            OwnedRunner::for_data_dir(AnyGrill::Runc(runtime.clone()), &root).unwrap(),
        )),
    )
    .with_budget(budget.clone());
    let mut spec = TaskArraySpec::with_count(2);
    spec.chunk_size = 1;
    spec.max_attempts = 1;
    let request = NodeSyncRequest {
        version: ControlVersion {
            index: 1,
            ..Default::default()
        },
        known: vec![1],
        arrays: vec![ArrayAssignment {
            template: Some(Box::new(template.clone())),
            resources: Resources::new(100, 32 << 20, 0),
            batch_id: 1,
            spec,
            program: "/unused".into(),
            args: vec!["/bin/true".into()],
            env: vec![],
            held: vec![HeldChunk {
                chunk: ChunkId(0),
                attempt: 1,
            }],
            stopping: false,
            replay_unknown: true,
            labels: Default::default(),
        }],
    };
    let progress = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let progress = node.sync(&request).await.unwrap().arrays.remove(0);
            if !progress.finished.is_empty() {
                break progress;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(progress.finished[0].succeeded, 1);
    assert_eq!(
        budget.available(),
        Resources::default(),
        "the warm helper owns the whole profile"
    );
    assert_eq!(
        progress.slots, 1,
        "zero unreserved resources must not strand compatible warm capacity"
    );
    let cancel_owner = CancellationToken::new();
    let owner = budget.acquire(capacity, &cancel_owner);
    tokio::pin!(owner);
    poll_fn(|cx| {
        assert!(owner.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(
        node.sync(&request).await.unwrap().arrays[0].slots,
        0,
        "warm capacity must yield to a queued owner"
    );
    let lease = tokio::time::timeout(Duration::from_secs(10), owner)
        .await
        .unwrap()
        .unwrap();
    for launch in runtime.launch_inventory().await.unwrap().unwrap() {
        assert_eq!(
            runtime.state(&launch.instance_id).await.unwrap(),
            ContainerState::Stopped
        );
    }
    drop(lease);
    drop(node);
    assert_eq!(budget.available(), capacity);

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
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    let ikm = [42u8; 32];
    let scope = AgeKeyScope::Namespace("rbtest-reuse".into());
    let (keypair, _) = secret::generate_age_keypair(scope.clone(), &ikm, 0).unwrap();
    let sealed = secret::encrypt_secret("proof-value", &keypair.public_key).unwrap();
    council
        .write(RaftRequest::SecurityStateInit(Box::new(SecurityState {
            age_keypairs: vec![keypair],
            ..Default::default()
        })))
        .await
        .unwrap();
    template.env.insert(
        "TOKEN".into(),
        reliaburger::config::types::EnvValue::Encrypted(sealed.clone()),
    );
    let runner = Arc::new(
        OwnedRunner::for_data_dir(runtime.clone(), &root)
            .unwrap()
            .with_budget(budget.clone())
            .with_secrets(council.clone(), ikm),
    );
    let mut task = TaskInvocation {
        template: Some(Box::new(template.clone())),
        index: 0,
        attempt: 1,
        program: "/unused".into(),
        args: vec![
            "/bin/sh".into(),
            "-c".into(),
            "test \"$TOKEN\" = proof-value && sleep 60".into(),
        ],
        env: vec![
            ("RELIABURGER_TASK_COUNT".into(), "2".into()),
            ("RELIABURGER_BATCH_ID".into(), "42".into()),
        ],
        run: Some(reliaburger::bun::task_executor::RunIdentity {
            batch_id: 42,
            task_count: 2,
            job_name: None,
        }),
    };
    let active_cancel = CancellationToken::new();
    let active = tokio::spawn({
        let runner = runner.clone();
        let cancel = active_cancel.clone();
        let task = task.clone();
        async move { runner.run(&task, Duration::from_secs(30), &cancel).await }
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while runner.active_commands(42, Some(&template)).await != Some(1) {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    task.index = 1;
    task.args = vec![
        "/bin/sh".into(),
        "-c".into(),
        "test \"$TOKEN\" = proof-value && printf revoked-secret-launched".into(),
    ];
    let queued = tokio::spawn({
        let runner = runner.clone();
        async move {
            runner
                .run(&task, Duration::from_secs(20), &CancellationToken::new())
                .await
        }
    });
    // Leave the original, verified command running while the second caller
    // decrypts and waits for the only profile that fits this node.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!queued.is_finished());
    let (replacement, _) = secret::generate_age_keypair(scope.clone(), &ikm, 1).unwrap();
    council
        .write(RaftRequest::RotateSecretKey {
            scope: scope.clone(),
            new_keypair: replacement,
            resealed: vec![],
        })
        .await
        .unwrap();
    council
        .write(RaftRequest::FinalizeSecretRotation { scope })
        .await
        .unwrap();
    let identities =
        secret::namespace_identities(&council.security_state().await, "rbtest-reuse", &ikm);
    assert!(
        identities
            .iter()
            .all(|identity| secret::decrypt_secret(&sealed, identity).is_err()),
        "old ciphertext must really be revoked"
    );
    active_cancel.cancel();
    assert_eq!(active.await.unwrap().outcome, AttemptOutcome::Cancelled);
    let result = tokio::time::timeout(Duration::from_secs(20), queued)
        .await
        .unwrap()
        .unwrap();
    let AttemptOutcome::Refused { reason } = &result.outcome else {
        panic!("queued command used a revoked decryption snapshot: {result:?}");
    };
    // The command never ran: its only output is the refusal (#786), never
    // anything the revoked secret's command printed.
    let output = String::from_utf8_lossy(&result.output.head);
    assert_eq!(output, format!("{reason}\n"));
    assert!(!output.contains("revoked-secret-launched"), "{output}");
    tokio::time::timeout(Duration::from_secs(10), async {
        while budget.available() != capacity {
            tokio::time::sleep(Duration::from_millis(20)).await
        }
    })
    .await
    .unwrap();
    council.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

/// Native jobs reuse their supervisor and enforce limits before user code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_jobs_reuse_owned_helpers_with_fresh_processes_and_enforced_profiles() {
    use reliaburger::bun::{
        execution_budget::ExecutionBudget,
        task_executor::{TaskInvocation, TaskRunner},
        task_runtime::OwnedRunner,
    };
    use reliaburger::config::job::{JobRuntime, JobSpec};
    use reliaburger::grill::{AnyGrill, ProcessGrill};
    use reliaburger::meat::Resources;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;
    let root_path = tempfile::Builder::new()
        .prefix("rb-native-executor-")
        .tempdir()
        .unwrap()
        .keep();
    let root = root_path.as_path();
    eprintln!("native fixture: {}", root.display());
    let runtime = ProcessGrill::with_owner(root.join("owners"), env!("CARGO_BIN_EXE_bun").into());
    let budget = ExecutionBudget::new(Resources::new(1000, 128 << 20, 0));
    let runner = OwnedRunner::with_slot_count(AnyGrill::Process(runtime.clone()), 1)
        .with_budget(budget.clone());
    let template: JobSpec = toml::from_str("runtime='process'\nexec='/bin/sh'\nnamespace='rbtest-native'\ncpu='100m-1000m'\nmemory='32Mi'").unwrap();
    let cancel = CancellationToken::new();
    for index in 0..128 {
        if index % 32 == 0 {
            eprintln!("native command {index}");
        }
        let task = TaskInvocation {
            template: Some(Box::new(template.clone())),
            index,
            attempt: 1,
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"cat /proc/self/cgroup; printf '%s' "${PREVIOUS-unset}"; export PREVIOUS=old"#
                    .into(),
            ],
            env: vec![],
            run: None,
        };
        let outcome = runner.run(&task, Duration::from_secs(10), &cancel).await;
        assert!(outcome.outcome.succeeded(), "{outcome:?}");
        let text = String::from_utf8_lossy(&outcome.output.head);
        assert!(
            text.contains("/task"),
            "command escaped resource cgroup: {text}"
        );
        assert!(text.ends_with("unset"), "environment leaked: {text}");
        if index == 0 {
            let path = text.lines().next().unwrap().strip_prefix("0::").unwrap();
            let group = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
            for (file, expected) in [
                ("cpu.max", "100000 100000"),
                ("memory.max", "33554432"),
                ("memory.swap.max", "0"),
                ("pids.max", "256"),
            ] {
                assert_eq!(
                    std::fs::read_to_string(group.join(file)).unwrap().trim(),
                    expected,
                    "{file}"
                );
            }
            let helper = group.parent().unwrap().join("helper");
            assert_eq!(
                std::fs::read_to_string(helper.join("cpu.weight"))
                    .unwrap()
                    .trim(),
                reliaburger::grill::cgroup::cpu_weight_from_millicores(10).to_string()
            );
            assert_eq!(
                std::fs::read_to_string(helper.join("memory.swap.max"))
                    .unwrap()
                    .trim(),
                "0"
            );
        }
    }
    eprintln!("native repeated commands complete");
    // Same environment contract as the other host backends: Bun's allowlisted
    // variables plus the job's own, and nothing else of Bun's.
    let private = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .find(|key| {
            // The shell itself sets these.
            !["PWD", "OLDPWD", "SHLVL", "_"].contains(&key.as_str())
                && !reliaburger::grill::process::inherited_by_host_commands(key)
        })
        .expect("the test environment has a variable outside the allowlist");
    let environment = TaskInvocation {
        template: Some(Box::new(template.clone())),
        index: 128,
        attempt: 1,
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exec /usr/bin/env".into()],
        env: vec![("JOB".into(), "1".into())],
        run: None,
    };
    let outcome = runner
        .run(&environment, Duration::from_secs(10), &cancel)
        .await;
    assert!(outcome.outcome.succeeded(), "{outcome:?}");
    let text = String::from_utf8_lossy(&outcome.output.head);
    let path = format!("PATH={}", std::env::var("PATH").unwrap());
    assert!(text.lines().any(|line| line == path), "{text}");
    assert!(text.lines().any(|line| line == "JOB=1"), "{text}");
    assert!(
        !text
            .lines()
            .any(|line| line.starts_with(&format!("{private}="))),
        "{private} leaked: {text}"
    );
    let timings = runner.executor_timings(JobRuntime::Process).unwrap();
    assert_eq!(
        timings.startup.samples, 1,
        "one owner per command regressed"
    );
    assert_eq!(timings.command.samples, 129);
    assert_eq!(timings.cleanup.samples, 129);
    let memory_hog = TaskInvocation {
        template: Some(Box::new(template.clone())),
        index: 128,
        attempt: 1,
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "exec /usr/bin/python3 -c 'value=bytearray(67108864)'".into(),
        ],
        env: vec![],
        run: None,
    };
    let outcome = runner
        .run(&memory_hog, Duration::from_secs(10), &cancel)
        .await;
    assert!(
        matches!(
            outcome.outcome,
            reliaburger::bun::task_executor::AttemptOutcome::Signalled { signal: 9 }
        ),
        "memory limit did not OOM-kill allocation: {outcome:?}"
    );
    let next = TaskInvocation {
        args: vec!["-c".into(), "printf after-oom".into()],
        ..memory_hog
    };
    let outcome = runner.run(&next, Duration::from_secs(10), &cancel).await;
    assert!(
        outcome.outcome.succeeded(),
        "task OOM killed its helper: {outcome:?}"
    );
    assert_eq!(outcome.output.head, b"after-oom");
    assert_eq!(
        runner
            .executor_timings(JobRuntime::Process)
            .unwrap()
            .startup
            .samples,
        1
    );
    let inventory = runtime.launch_inventory().await.unwrap().unwrap();
    assert_eq!(
        inventory.len(),
        1,
        "owner inventory grew with completed jobs"
    );
    let id = inventory[0].instance_id.clone();
    let alive = runtime.pid(&id).await.unwrap().unwrap();
    assert!(
        std::fs::read_to_string(format!("/proc/{alive}/cgroup"))
            .unwrap()
            .contains("/helper")
    );
    // Host helpers keep only CAP_KILL (5), CAP_SETGID (6) and CAP_SETUID (7).
    let status = std::fs::read_to_string(format!("/proc/{alive}/status")).unwrap();
    for set in ["CapEff", "CapPrm"] {
        assert!(
            status
                .lines()
                .any(|line| line == format!("{set}:\t00000000000000e0")),
            "{status}"
        );
    }
    let cancel_command = CancellationToken::new();
    let task = TaskInvocation {
        template: Some(Box::new(template)),
        index: 129,
        attempt: 1,
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "setsid /bin/sh -c 'sleep 30' & wait".into()],
        env: vec![("RELIABURGER_BATCH_ID".into(), "77".into())],
        run: Some(reliaburger::bun::task_executor::RunIdentity {
            batch_id: 77,
            task_count: 2,
            job_name: None,
        }),
    };
    let execution = runner.run(&task, Duration::from_secs(10), &cancel_command);
    let trigger = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while runner.active_commands(77, task.template.as_deref()).await != Some(1) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        cancel_command.cancel();
    };
    eprintln!("native cancellation test starts");
    let (outcome, ()) = tokio::join!(execution, trigger);
    assert!(matches!(
        outcome.outcome,
        reliaburger::bun::task_executor::AttemptOutcome::Cancelled
    ));
    eprintln!("native cancellation returned {outcome:?}");
    assert_retired(&runtime, &id).await;
    assert_eq!(budget.available(), budget.capacity());
    std::fs::remove_dir_all(root).unwrap();
}

/// Dropping a caller hands its executor to the eviction loop, which retires
/// it and returns the reservation without a restart. A restart afterwards
/// reuses only the originally retired slot identity, never a recovered PID.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_executor_retires_a_dropped_callers_slot_without_a_restart() {
    use reliaburger::bun::{
        execution_budget::ExecutionBudget,
        task_executor::{TaskInvocation, TaskRunner},
        task_runtime::OwnedRunner,
    };
    use reliaburger::{
        grill::{AnyGrill, ProcessGrill},
        meat::Resources,
    };
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    let root = tempfile::Builder::new()
        .prefix("rb-native-recovery-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("native recovery fixture: {}", root.display());
    let runtime = ProcessGrill::with_owner(root.join("owners"), env!("CARGO_BIN_EXE_bun").into());
    let budget = ExecutionBudget::new(Resources::new(1000, 128 << 20, 0));
    let runner = Arc::new(
        OwnedRunner::for_data_dir(AnyGrill::Process(runtime.clone()), &root)
            .unwrap()
            .with_budget(budget.clone()),
    );
    let template = toml::from_str("runtime='process'\nexec='/bin/sh'\nnamespace='rbtest-native-recovery'\ncpu='100m-1000m'\nmemory='32Mi'").unwrap();
    let task = TaskInvocation {
        template: Some(Box::new(template)),
        index: 0,
        attempt: 1,
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "setsid /bin/sh -c 'sleep 30' & wait".into()],
        env: vec![("RELIABURGER_BATCH_ID".into(), "88".into())],
        run: Some(reliaburger::bun::task_executor::RunIdentity {
            batch_id: 88,
            task_count: 2,
            job_name: None,
        }),
    };
    let execution = {
        let runner = runner.clone();
        let task = task.clone();
        tokio::spawn(async move {
            runner
                .run(&task, Duration::from_secs(60), &CancellationToken::new())
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while runner.active_commands(88, task.template.as_deref()).await != Some(1) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let id = runtime.launch_inventory().await.unwrap().unwrap()[0]
        .instance_id
        .clone();
    execution.abort();
    assert!(execution.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(10), async {
        while runtime.state(&id).await.unwrap() != ContainerState::Stopped {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dropped connection did not retire the original subtree");
    tokio::time::timeout(Duration::from_secs(10), async {
        while budget.available() != budget.capacity() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the dropped caller's reservation waited for a restart");
    drop(runner);
    let recovered_budget = ExecutionBudget::new(budget.capacity());
    let recovered = OwnedRunner::for_data_dir(AnyGrill::Process(runtime.clone()), &root)
        .unwrap()
        .with_budget(recovered_budget.clone());
    let next = TaskInvocation {
        args: vec!["-c".into(), "printf recovered".into()],
        ..task
    };
    let outcome = recovered
        .run(&next, Duration::from_secs(10), &CancellationToken::new())
        .await;
    assert!(outcome.outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.output.head, b"recovered");
    let inventory = runtime.launch_inventory().await.unwrap().unwrap();
    assert_eq!(inventory.len(), 1);
    assert_eq!(
        inventory[0].instance_id, id,
        "restart grew ownership identities"
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while recovered_budget.available() != recovered_budget.capacity() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_retired(&runtime, &id).await;
    std::fs::remove_dir_all(root).unwrap();
}

/// A native runner with one slot, so every command reuses the same executor
/// identity and cgroup path.
fn one_slot_native_runner(
    root: &std::path::Path,
) -> (
    reliaburger::bun::task_runtime::OwnedRunner<reliaburger::grill::AnyGrill>,
    std::sync::Arc<reliaburger::bun::execution_budget::ExecutionBudget>,
    reliaburger::config::job::JobSpec,
    reliaburger::grill::ProcessGrill,
) {
    use reliaburger::grill::{AnyGrill, ProcessGrill};
    let runtime = ProcessGrill::with_owner(root.join("owners"), env!("CARGO_BIN_EXE_bun").into());
    let budget = reliaburger::bun::execution_budget::ExecutionBudget::new(
        reliaburger::meat::Resources::new(1000, 128 << 20, 0),
    );
    let runner = reliaburger::bun::task_runtime::OwnedRunner::with_slot_count(
        AnyGrill::Process(runtime.clone()),
        1,
    )
    .with_budget(budget.clone());
    let template = toml::from_str(
        "runtime='process'\nexec='/bin/sh'\nnamespace='rbtest-native'\ncpu='100m-1000m'\nmemory='32Mi'",
    )
    .unwrap();
    (runner, budget, template, runtime)
}

fn shell_task(
    template: &reliaburger::config::job::JobSpec,
    index: u32,
    script: &str,
) -> reliaburger::bun::task_executor::TaskInvocation {
    reliaburger::bun::task_executor::TaskInvocation {
        template: Some(Box::new(template.clone())),
        index,
        attempt: 1,
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        run: None,
    }
}

/// The task cgroup a native command ran in, from its `/proc/self/cgroup`.
fn task_cgroup(output: &[u8]) -> std::path::PathBuf {
    let text = String::from_utf8_lossy(output);
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .unwrap_or_else(|| panic!("no cgroup line: {text}"));
    std::path::Path::new("/sys/fs/cgroup").join(path.trim().trim_start_matches('/'))
}

async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// Retirement writes `cgroup.kill`; on Linux 6.8 a killed group keeps its
/// kill sequence, so a reused one would SIGKILL the next command at birth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_executor_never_reuses_a_killed_task_cgroup() {
    use reliaburger::bun::task_executor::TaskRunner;
    use std::os::unix::fs::MetadataExt;
    let root = tempfile::Builder::new()
        .prefix("rb-native-fresh-")
        .tempdir()
        .unwrap();
    let (runner, budget, template, _) = one_slot_native_runner(root.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let cold = std::time::Instant::now();
    let first = runner
        .run(
            &shell_task(&template, 0, "cat /proc/self/cgroup"),
            Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert!(first.outcome.succeeded(), "{first:?}");
    // The pool reports the command alone, not the cold helper start.
    assert!(
        first.ran.is_some_and(|ran| ran < cold.elapsed()),
        "{first:?}"
    );
    let task = task_cgroup(&first.output.head);
    wait_until("idle eviction retires the executor", || {
        !task.exists() && budget.available() == budget.capacity()
    })
    .await;
    // Plant a killed, empty task cgroup where the next executor will look.
    std::fs::create_dir_all(&task).unwrap();
    std::fs::write(task.join("cgroup.kill"), "1").unwrap();
    let planted = std::fs::metadata(&task).unwrap().ino();
    let second = runner
        .run(
            &shell_task(&template, 1, "cat /proc/self/cgroup"),
            Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert!(second.outcome.succeeded(), "{second:?}");
    assert_eq!(task_cgroup(&second.output.head), task);
    assert_ne!(
        std::fs::metadata(&task).unwrap().ino(),
        planted,
        "the executor adopted a killed task cgroup"
    );
    wait_until("the executor retires", || {
        budget.available() == budget.capacity()
    })
    .await;
}

/// A task group that never empties (as with a task in uninterruptible sleep)
/// must not hang the caller; the slot and its lease stay quarantined until
/// the eviction loop finally retires it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_executor_retirement_is_bounded_and_quarantines_the_slot() {
    use reliaburger::bun::task_executor::{AttemptOutcome, TaskRunner};
    let root = tempfile::Builder::new()
        .prefix("rb-native-stuck-")
        .tempdir()
        .unwrap();
    let (runner, budget, template, _) = one_slot_native_runner(root.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let first = runner
        .run(
            &shell_task(&template, 0, "cat /proc/self/cgroup"),
            Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert!(first.outcome.succeeded(), "{first:?}");
    let task = task_cgroup(&first.output.head);
    // The warm executor runs the next command in this same task group. Make
    // the group report itself populated forever, as a task in uninterruptible
    // sleep would: bind a file saying "populated 1" over its cgroup.events.
    // Racing real processes against cgroup.kill can't do this reliably: a fast
    // kernel empties the group between respawns, and retirement rightly
    // succeeds.
    let fake = root.path().join("populated");
    std::fs::write(&fake, "populated 1\nfrozen 0\n").unwrap();
    let events = task.join("cgroup.events");
    nix::mount::mount(
        Some(&fake),
        &events,
        None::<&str>,
        nix::mount::MsFlags::MS_BIND,
        None::<&str>,
    )
    .unwrap();
    let started = std::time::Instant::now();
    let stuck = tokio::time::timeout(
        Duration::from_secs(30),
        runner.run(
            &shell_task(&template, 1, "sleep 30"),
            Duration::from_secs(1),
            &cancel,
        ),
    )
    .await
    .expect("retirement blocked the caller");
    assert!(
        matches!(stuck.outcome, AttemptOutcome::TimedOut),
        "{stuck:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_ne!(
        budget.available(),
        budget.capacity(),
        "a quarantined executor released its reservation"
    );
    nix::mount::umount(&events).unwrap();
    wait_until("the eviction loop retires the quarantined executor", || {
        budget.available() == budget.capacity()
    })
    .await;
    let after = runner
        .run(
            &shell_task(&template, 2, "printf ok"),
            Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert!(after.outcome.succeeded(), "{after:?}");
    assert_eq!(after.output.head, b"ok");
    wait_until("the executor retires", || {
        budget.available() == budget.capacity()
    })
    .await;
}

/// The pid of the one-slot runner's live executor helper, once it has one.
async fn running_helper(runtime: &reliaburger::grill::ProcessGrill) -> nix::unistd::Pid {
    use reliaburger::grill::Grill;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let inventory = runtime.launch_inventory().await.unwrap().unwrap();
            if let [launch] = inventory.as_slice()
                && let Some(pid) = runtime.pid(&launch.instance_id).await.unwrap()
            {
                return nix::unistd::Pid::from_raw(pid as i32);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timed out waiting for the executor helper")
}

/// Output still buffered in an enlarged pipe at exit must all arrive; output
/// that never stops after exit is cut off with a visible marker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_executor_drains_output_buffered_at_exit() {
    use nix::sys::signal::{Signal, kill};
    use reliaburger::bun::task_executor::TaskRunner;
    let root = tempfile::Builder::new()
        .prefix("rb-native-drain-")
        .tempdir()
        .unwrap();
    let (runner, budget, template, runtime) = one_slot_native_runner(root.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let warm = runner
        .run(
            &shell_task(&template, 0, "true"),
            Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert!(warm.outcome.succeeded(), "{warm:?}");
    // Stop the helper while the command fills a 1 MiB pipe (F_SETPIPE_SZ is
    // 1031) and exits, so all 500,000 bytes are still buffered when the
    // helper sees the exit. Running, it would drain them as they arrive.
    // Look the helper up only once the command holds its slot: the pool
    // retires an executor idle for a second, and on a loaded host the warm
    // one can go before the command starts, leaving a stale pid (#715).
    let python = shell_task(
        &template,
        1,
        "exec /usr/bin/python3 -c 'import fcntl, os, time; fcntl.fcntl(1, 1031, 1 << 20); time.sleep(0.5); os.write(1, b\"x\" * 500000)'",
    );
    let buffered = runner.run(&python, Duration::from_secs(10), &cancel);
    let pause = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let helper = running_helper(&runtime).await;
        kill(helper, Signal::SIGSTOP).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        kill(helper, Signal::SIGCONT).unwrap();
    };
    let (buffered, ()) = tokio::join!(buffered, pause);
    assert!(buffered.outcome.succeeded(), "{buffered:?}");
    assert_eq!(buffered.output.total_bytes, 500_000);
    let endless = runner
        .run(
            &shell_task(&template, 2, "/usr/bin/yes & sleep 0.2; exit 0"),
            Duration::from_secs(30),
            &cancel,
        )
        .await;
    assert!(endless.outcome.succeeded(), "{endless:?}");
    let tail = &endless.output.tail;
    assert!(
        tail.ends_with(b"\n[reliaburger: output written after exit truncated]\n"),
        "{} bytes, tail {:?}",
        endless.output.total_bytes,
        String::from_utf8_lossy(&tail[tail.len().saturating_sub(80)..])
    );
    wait_until("the executor retires", || {
        budget.available() == budget.capacity()
    })
    .await;
}

/// Another local user can create any name in `/tmp`. A helper's control
/// socket must live where only root can, or that user can block the slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_executor_sockets_cannot_be_blocked_by_other_users() {
    use reliaburger::bun::task_executor::TaskRunner;
    use reliaburger::grill::Grill;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let root = tempfile::Builder::new()
        .prefix("rb-native-socket-")
        .tempdir()
        .unwrap();
    let (runner, budget, template, runtime) = one_slot_native_runner(root.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let first = runner
        .run(
            &shell_task(&template, 0, "true"),
            Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert!(first.outcome.succeeded(), "{first:?}");
    let id = runtime.launch_inventory().await.unwrap().unwrap()[0]
        .instance_id
        .clone();
    wait_until("idle eviction retires the executor", || {
        budget.available() == budget.capacity()
    })
    .await;
    // What an unprivileged user could have done: claim the old, predictable name.
    let squatted = std::path::Path::new("/tmp").join(format!(
        "rbhx-{}",
        hex::encode(Sha256::digest(id.0.as_bytes()))
    ));
    std::fs::write(&squatted, b"").unwrap();
    std::os::unix::fs::lchown(&squatted, Some(65534), Some(65534)).unwrap();
    let second = runner
        .run(
            &shell_task(&template, 1, "true"),
            Duration::from_secs(10),
            &cancel,
        )
        .await;
    std::fs::remove_file(&squatted).unwrap();
    assert!(second.outcome.succeeded(), "{second:?}");
    let directory = std::fs::symlink_metadata("/run/reliaburger/host-executors").unwrap();
    assert!(directory.is_dir());
    assert_eq!(directory.uid(), 0);
    assert_eq!(directory.permissions().mode() & 0o7777, 0o711);
    wait_until("the executor retires", || {
        budget.available() == budget.capacity()
    })
    .await;
}

/// Callers waiting inside a reusable pool for a slot or admission aren't
/// running anything; advertising them as slots inflates the leader's grants.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_arrays_advertise_running_commands_not_queued_callers() {
    use reliaburger::bun::{
        execution_budget::ExecutionBudget,
        task_array_node::{
            ArrayAssignment, ControlVersion, HeldChunk, NodeRunner, NodeSyncRequest, TaskArrayNode,
            TaskArrayNodeConfig,
        },
        task_runtime::OwnedRunner,
    };
    use reliaburger::{
        config::process_workloads::ProcessWorkloadsConfig,
        grill::{AnyGrill, ProcessGrill},
        meat::{
            Resources,
            task_array::{ChunkId, TaskArraySpec},
        },
    };
    let root = tempfile::Builder::new()
        .prefix("rb-native-slots-")
        .tempdir()
        .unwrap();
    let runtime =
        ProcessGrill::with_owner(root.path().join("owners"), env!("CARGO_BIN_EXE_bun").into());
    // Exactly two executors: 100m/32 MiB each plus 10m/8 MiB for the helper.
    let capacity = Resources::new(220, 80 << 20, 0);
    let budget = ExecutionBudget::new(capacity);
    let template: reliaburger::config::job::JobSpec = toml::from_str(
        "runtime='process'\nexec='/bin/sh'\nnamespace='rbtest-slots'\ncpu='100m'\nmemory='32Mi'",
    )
    .unwrap();
    let mut config = TaskArrayNodeConfig::for_data_dir(
        root.path(),
        ProcessWorkloadsConfig {
            allowed_binaries: vec!["/bin/sh".into()],
            mount_isolation: false,
            script_dir: root.path().join("scripts"),
        },
    );
    config.default_concurrency = 32;
    let node = TaskArrayNode::new(
        config,
        NodeRunner::Owned(Box::new(
            OwnedRunner::for_data_dir(AnyGrill::Process(runtime), root.path()).unwrap(),
        )),
    )
    .with_budget(budget.clone());
    let mut spec = TaskArraySpec::with_count(64);
    spec.chunk_size = 64;
    spec.max_attempts = 1;
    let assignment = |stopping| NodeSyncRequest {
        version: ControlVersion {
            index: 1,
            ..Default::default()
        },
        known: vec![1],
        arrays: vec![ArrayAssignment {
            template: Some(Box::new(template.clone())),
            resources: Resources::new(100, 32 << 20, 0),
            batch_id: 1,
            spec: spec.clone(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 2".into()],
            env: vec![],
            held: vec![HeldChunk {
                chunk: ChunkId(0),
                attempt: 1,
            }],
            stopping,
            replay_unknown: true,
            labels: Default::default(),
        }],
    };
    // Wait until both executors are busy and the other callers are queued.
    let progress = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let progress = node
                .sync(&assignment(false))
                .await
                .unwrap()
                .arrays
                .remove(0);
            if progress.counters.active_commands == Some(2) && progress.counters.running > 2 {
                break progress;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("two native commands never started");
    assert!(
        progress.slots <= 2,
        "advertised {} slots with {} callers but room for two executors",
        progress.slots,
        progress.counters.running
    );
    tokio::time::timeout(Duration::from_secs(60), async {
        while node.sync(&assignment(true)).await.unwrap().arrays[0]
            .finished
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the stopped array never finished");
    drop(node);
    let mut retired = false;
    for _ in 0..300 {
        if budget.available() == capacity {
            retired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(retired, "executors never returned their reservations");
}

/// A slot counts as busy from checkout, before the helper confirms the start,
/// so millisecond commands that start and exit between samples still count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_busy_slots_count_checked_out_slots_before_the_command_starts() {
    use nix::sys::signal::{Signal, kill};
    use reliaburger::bun::task_executor::TaskRunner;
    use reliaburger::grill::Grill;
    let root = tempfile::Builder::new()
        .prefix("rb-native-busy-")
        .tempdir()
        .unwrap();
    let (runner, budget, template, runtime) = one_slot_native_runner(root.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut warm = shell_task(&template, 0, "true");
    warm.run = Some(reliaburger::bun::task_executor::RunIdentity {
        batch_id: 9,
        task_count: 2,
        job_name: None,
    });
    let first = runner.run(&warm, Duration::from_secs(10), &cancel).await;
    assert!(first.outcome.succeeded(), "{first:?}");
    let id = runtime.launch_inventory().await.unwrap().unwrap()[0]
        .instance_id
        .clone();
    let helper = nix::unistd::Pid::from_raw(runtime.pid(&id).await.unwrap().unwrap() as i32);
    // A stopped helper never confirms the start, but the slot is checked out.
    kill(helper, Signal::SIGSTOP).unwrap();
    let second = reliaburger::bun::task_executor::TaskInvocation {
        index: 1,
        ..warm.clone()
    };
    let held = runner.run(&second, Duration::from_secs(10), &cancel);
    let sample = async {
        wait_until_async(|| async { runner.busy_slots(9, Some(&template)).await == Some(1) }).await;
        let active = runner.active_commands(9, Some(&template)).await;
        kill(helper, Signal::SIGCONT).unwrap();
        active
    };
    let (outcome, active) = tokio::join!(held, sample);
    assert_eq!(active, Some(0), "the command hadn't started yet");
    assert!(outcome.outcome.succeeded(), "{outcome:?}");
    assert_eq!(runner.busy_slots(9, Some(&template)).await, Some(0));
    wait_until("the executor retires", || {
        budget.available() == budget.capacity()
    })
    .await;
}

async fn wait_until_async<F: std::future::Future<Output = bool>>(mut ready: impl FnMut() -> F) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition never held");
}

/// A command whose output streams are both at EOF when its exit is seen must
/// not wait out the post-exit drain poll; that 10 ms per command cut native
/// throughput to a third.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root and cgroup v2; run with make test-linux"]
async fn cgroup_host_executor_adds_no_drain_wait_to_quiet_commands() {
    use reliaburger::bun::task_executor::TaskRunner;
    let root = tempfile::Builder::new()
        .prefix("rb-native-latency-")
        .tempdir()
        .unwrap();
    let (runner, budget, template, _) = one_slot_native_runner(root.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let warm = runner
        .run(
            &shell_task(&template, 0, "true"),
            Duration::from_secs(10),
            &cancel,
        )
        .await;
    assert!(warm.outcome.succeeded(), "{warm:?}");
    let started = std::time::Instant::now();
    for index in 1..=200 {
        let outcome = runner
            .run(
                &shell_task(&template, index, "true"),
                Duration::from_secs(10),
                &cancel,
            )
            .await;
        assert!(outcome.outcome.succeeded(), "{outcome:?}");
    }
    let elapsed = started.elapsed();
    eprintln!("200 sequential quiet commands took {elapsed:?}");
    // A 10 ms wait per command alone would take 2 seconds.
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    wait_until("the executor retires", || {
        budget.available() == budget.capacity()
    })
    .await;
}

/// Every namespace that ever ran a shared-runc array used to leave executor
/// identities, journals and route files behind for good (#678). Once the
/// executors retire, nothing of them remains, and a restart sweep finds
/// nothing more to collect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, pinned test image, ip and nft; run with make test-linux"]
async fn runc_shared_arrays_across_namespaces_leave_no_executor_metadata_after_retirement() {
    use reliaburger::bun::task_executor::{AttemptOutcome, TaskInvocation, TaskRunner};
    use reliaburger::bun::{execution_budget::ExecutionBudget, task_runtime::OwnedRunner};
    use reliaburger::grill::AnyGrill;
    use reliaburger::meat::Resources;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::Builder::new()
        .prefix("rb-678-metadata-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("executor metadata fixture: {}", root.display());
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = AnyGrill::with_host_processes(
        RuncGrill::new(
            root.join("bundles"),
            images,
            false,
            root.join("state"),
            env!("CARGO_BIN_EXE_bun").into(),
        )
        .unwrap(),
        &root.join("instances"),
        env!("CARGO_BIN_EXE_bun").into(),
    );
    let budget = ExecutionBudget::new(Resources::new(2000, 512 << 20, 0));
    let runner = Arc::new(
        OwnedRunner::for_data_dir(runtime.clone(), &root)
            .unwrap()
            .with_budget(budget.clone()),
    );
    let cancel = CancellationToken::new();
    let namespaces = ["rbtest-tenant-a", "rbtest-tenant-b", "rbtest-tenant-c"];
    for namespace in namespaces {
        let template: reliaburger::config::job::JobSpec = toml::from_str(&format!(
            "image='{image}'\nnamespace='{namespace}'\nruntime='shared-runc'\ncpu='100m'\nmemory='32Mi'"
        ))
        .unwrap();
        let task = TaskInvocation {
            template: Some(Box::new(template)),
            index: 0,
            attempt: 1,
            program: "/unused".into(),
            args: vec!["/bin/sh".into(), "-c".into(), "printf ran".into()],
            env: vec![
                ("RELIABURGER_TASK_COUNT".into(), "1".into()),
                ("RELIABURGER_BATCH_ID".into(), "7".into()),
            ],
            run: Some(reliaburger::bun::task_executor::RunIdentity {
                batch_id: 7,
                task_count: 1,
                job_name: None,
            }),
        };
        let outcome = runner.run(&task, Duration::from_secs(30), &cancel).await;
        assert_eq!(
            outcome.outcome,
            AttemptOutcome::Exited { code: 0 },
            "{outcome:?}"
        );
    }
    let executors = |inventory: Vec<reliaburger::grill::RuntimeLaunch>| {
        inventory
            .into_iter()
            .filter(|launch| launch.instance_id.0.contains("__executor-"))
            .count()
    };
    // Idle executors retire about a second after their last command.
    tokio::time::timeout(Duration::from_secs(20), async {
        while budget.available() != budget.capacity()
            || executors(runtime.launch_inventory().await.unwrap().unwrap()) != 0
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("idle executors were not retired and forgotten");
    let leftovers = |directory: &std::path::Path| -> Vec<String> {
        match std::fs::read_dir(directory) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .filter(|name| name.contains("__executor-"))
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("{}: {error}", directory.display()),
        }
    };
    for directory in [
        root.join("instances/runtime-routes"),
        root.join("bundles/.intents/records"),
        root.join("bundles/.intents/locks"),
        root.join("bundles"),
        root.join("state"),
    ] {
        let kept = leftovers(&directory);
        assert!(kept.is_empty(), "{} kept {kept:?}", directory.display());
    }
    assert_eq!(runner.forget_retired_executors().await, 0);
    drop(runner);
    std::fs::remove_dir_all(root).unwrap();
}

/// A running shared-runc profile is measured from its executor's cgroup:
/// `relish top` and Mayo get CPU and memory for array tasks, which run
/// outside the app supervisor and its per-process samples.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc and pinned images; run with make test-linux"]
async fn runc_shared_profile_reports_cgroup_cpu_and_memory() {
    use reliaburger::bun::{
        execution_budget::ExecutionBudget,
        task_array_node::{
            ArrayAssignment, ControlVersion, HeldChunk, NodeRunner, NodeSyncRequest, TaskArrayNode,
            TaskArrayNodeConfig,
        },
        task_runtime::OwnedRunner,
    };
    use reliaburger::config::process_workloads::ProcessWorkloadsConfig;
    use reliaburger::grill::AnyGrill;
    use reliaburger::mayo::collector::SystemCollector;
    use reliaburger::mayo::job_metrics::{CgroupCpu, JobLabels, UsageTarget};
    use reliaburger::meat::{
        Resources,
        task_array::{ChunkId, TaskArraySpec},
    };
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::Builder::new()
        .prefix("rb-684-usage-")
        .tempdir()
        .unwrap();
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
    let budget = ExecutionBudget::new(Resources::new(1000, 256 << 20, 0));
    let template: reliaburger::config::job::JobSpec = toml::from_str(&format!(
        "image='{image}'\nnamespace='rbtest-usage'\nruntime='shared-runc'\ncpu='200m'\nmemory='32Mi'"
    ))
    .unwrap();
    let node = TaskArrayNode::new(
        TaskArrayNodeConfig::for_data_dir(
            root.path(),
            ProcessWorkloadsConfig {
                allowed_binaries: Vec::new(),
                mount_isolation: false,
                script_dir: root.path().join("scripts"),
            },
        ),
        NodeRunner::Owned(Box::new(
            OwnedRunner::for_data_dir(AnyGrill::Runc(runtime), root.path()).unwrap(),
        )),
    )
    .with_budget(budget.clone());
    let labels = JobLabels {
        namespace: "rbtest-usage".into(),
        definition: "burn".into(),
        profile: "default".into(),
    };
    let mut spec = TaskArraySpec::with_count(2);
    spec.chunk_size = 2;
    spec.max_attempts = 1;
    let request = |stopping| NodeSyncRequest {
        version: ControlVersion {
            index: 1,
            ..Default::default()
        },
        known: vec![1],
        arrays: vec![ArrayAssignment {
            template: Some(Box::new(template.clone())),
            resources: Resources::new(200, 32 << 20, 0),
            batch_id: 1,
            spec: spec.clone(),
            program: "/unused".into(),
            // Busy for a while, so there is CPU to see.
            args: vec![
                "/bin/sh".into(),
                "-c".into(),
                "end=$(($(date +%s)+8)); while [ $(date +%s) -lt $end ]; do :; done".into(),
            ],
            env: vec![],
            held: vec![HeldChunk {
                chunk: ChunkId(0),
                attempt: 1,
            }],
            stopping,
            replay_unknown: true,
            labels: labels.clone(),
        }],
    };
    let telemetry = node.telemetry().clone();
    let mut last = None;
    let found = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            last = Some(node.sync(&request(false)).await.unwrap());
            let targets = telemetry.usage_targets();
            if targets.get(&labels).is_some_and(|(_, targets)| {
                targets
                    .iter()
                    .any(|target| matches!(target, UsageTarget::Cgroup(_)))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        found.is_ok(),
        "the shared-runc executor never reported a cgroup; last sync {last:?}, targets {:?}",
        telemetry.usage_targets()
    );

    let mut collector = SystemCollector::new();
    let mut cgroups = CgroupCpu::default();
    collector.refresh();
    telemetry.sample_usage(&collector, &mut cgroups, "node-a");
    tokio::time::sleep(Duration::from_secs(1)).await;
    node.sync(&request(false)).await.unwrap();
    collector.refresh();
    let samples = telemetry.sample_usage(&collector, &mut cgroups, "node-a");
    let value = |name: &str| {
        samples
            .iter()
            .find(|sample| sample.key.name.as_str() == name)
            .map(|sample| sample.value)
    };
    let memory = value("job_memory_bytes").expect("no memory sample");
    let cpu = value("job_cpu_percent").expect("no cpu sample");
    assert!(memory > 0.0, "memory {memory}");
    assert!(cpu > 1.0, "cpu {cpu}");
    let usage = telemetry.usage()[&labels];
    assert!(usage.running >= 1, "{usage:?}");

    tokio::time::timeout(Duration::from_secs(60), async {
        while node.sync(&request(true)).await.unwrap().arrays[0]
            .finished
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the stopped array never finished");
}

/// Each task fails its first attempt when its index ends in 07, so the
/// accepted retry count is exact and every retry has a known cause.
/// Ports each leader-loss node takes: gossip, then Raft, reporting and API.
const LEADER_LOSS_PORTS_PER_NODE: u16 = 4;
const LEADER_LOSS_SCRIPT: &str = "sleep 0.05; [ \"$RELIABURGER_TASK_ATTEMPT\" -gt 1 ] || [ $((RELIABURGER_TASK_INDEX % 100)) -ne 7 ]";
const LEADER_LOSS_TASKS: u64 = 3000;

/// The kernel resources a fixture's executors may hold: runc containers,
/// network namespaces with their host veths, and executor cgroups. A run
/// that fails before its executors retire must still release them, or the
/// next runc test on the host meets `RTNETLINK answers: File exists` (#786).
struct ExecutorResidue {
    root: std::path::PathBuf,
}

impl ExecutorResidue {
    /// The instance-name stem of every node's executors under `root`.
    fn stems(&self) -> Vec<String> {
        let nodes = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten();
        nodes
            .filter_map(|node| std::fs::read_to_string(node.path().join("batch-executor-id")).ok())
            .map(|prefix| format!("executor-{}-", prefix.trim()))
            .collect()
    }

    /// The network namespaces those executors still hold.
    fn namespaces(&self) -> Vec<String> {
        let stems = self.stems();
        std::fs::read_dir("/var/run/netns")
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| stems.iter().any(|stem| name.contains(stem.as_str())))
            .collect()
    }

    /// The executor slot cgroups those executors still hold, as
    /// `<namespace>/<app>/<slot>`.
    fn slot_cgroups(&self) -> Vec<String> {
        let stems = self.stems();
        let ours = |app: &str| {
            stems
                .iter()
                .any(|stem| app.starts_with(stem.as_str()) || app == stem.trim_end_matches('-'))
        };
        let directories = |path: std::path::PathBuf| {
            std::fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        };
        let mut slots = Vec::new();
        for namespace in directories("/sys/fs/cgroup/reliaburger".into()) {
            for app in directories(namespace.path()) {
                let name = app.file_name().to_string_lossy().into_owned();
                if !ours(&name) {
                    continue;
                }
                for slot in directories(app.path()) {
                    slots.push(format!(
                        "{}/{name}/{}",
                        namespace.file_name().to_string_lossy(),
                        slot.file_name().to_string_lossy()
                    ));
                }
            }
        }
        slots
    }

    /// Kill the containers, then delete the namespaces, veths and cgroups.
    fn release(&self) {
        let state = self.root.join("state");
        if let Ok(listed) = std::process::Command::new("runc")
            .arg("--root")
            .arg(&state)
            .args(["list", "--quiet"])
            .output()
        {
            for id in String::from_utf8_lossy(&listed.stdout).lines() {
                let _ = std::process::Command::new("runc")
                    .arg("--root")
                    .arg(&state)
                    .args(["delete", "--force", id])
                    .output();
            }
        }
        for namespace in self.namespaces() {
            if let Some(instance) = namespace.strip_prefix("rb-") {
                let veth = reliaburger::grill::netns::host_veth_name(
                    &reliaburger::grill::InstanceId(instance.to_owned()),
                );
                let _ = std::process::Command::new("ip")
                    .args(["link", "del", &veth])
                    .output();
            }
            let _ = std::process::Command::new("ip")
                .args(["netns", "del", &namespace])
                .output();
        }
        let stems = self.stems();
        let cgroups = std::fs::read_dir("/sys/fs/cgroup/reliaburger")
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|namespace| std::fs::read_dir(namespace.path()).into_iter().flatten())
            .flatten()
            .map(|app| app.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| stems.iter().any(|stem| name.starts_with(stem.as_str())))
            });
        for cgroup in cgroups {
            remove_cgroup_tree(&cgroup);
        }
    }
}

impl Drop for ExecutorResidue {
    fn drop(&mut self) {
        self.release();
    }
}

/// Kill every process in a cgroup subtree, then remove it bottom-up.
fn remove_cgroup_tree(path: &std::path::Path) {
    if !path.join("cgroup.procs").exists() {
        return;
    }
    let _ = std::fs::write(path.join("cgroup.kill"), "1");
    for child in std::fs::read_dir(path).into_iter().flatten().flatten() {
        if child.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_cgroup_tree(&child.path());
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::fs::remove_dir(path).is_err_and(|error| {
        error.kind() != std::io::ErrorKind::NotFound && std::time::Instant::now() < deadline
    }) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Three in-process nodes with the same gossip, Raft, HTTP API and leader
/// loop as `bun --cluster`, each with its own real executor (distinct
/// executor identities over one host runtime). The leader is a worker too;
/// once it is running its own share, its control plane stops without a
/// handover while its executor keeps running. Those commands may finish,
/// but no report of theirs can ever be accepted: the new leader must take
/// back exactly the chunks it held and count every task once.
async fn leader_loss_mid_array(template: serde_json::Value) {
    use reliaburger::bun::task_array_leader::TaskArrayService;
    use reliaburger::bun::task_array_node::{NodeRunner, TaskArrayNode, TaskArrayNodeConfig};
    use reliaburger::bun::task_ledger::GroupCommit;
    use reliaburger::bun::task_runtime::OwnedRunner;
    use reliaburger::config::process_workloads::ProcessWorkloadsConfig;
    use reliaburger::grill::AnyGrill;
    use reliaburger::meat::task_array::ChunkId;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    assert!(nix::unistd::geteuid().is_root());
    if template["runtime"] != "process" {
        // Cold executors of the warmed pin read nothing from the registry
        // (#790), but the warm-up pull does, and the public registry's
        // anonymous rate limit must not decide whether this test runs (#786).
        let mirror = reliaburger::testkit::pinned_images::TEST_IMAGE_MIRROR_ENV;
        assert!(
            std::env::var_os(mirror).is_some(),
            "{mirror} is unset: run with make test-linux, which serves the pinned images locally"
        );
    }
    let root = tempfile::Builder::new()
        .prefix("rb-leader-loss-")
        .tempdir()
        .unwrap()
        .keep();
    eprintln!("leader-loss fixture: {}", root.display());
    // Declared before the runtime and nodes, so it drops after them: on a
    // failed assertion as on a pass.
    let residue = ExecutorResidue { root: root.clone() };
    let image = reliaburger::testkit::pinned_images::PINNED_TEST_WORKLOAD_IMAGE;
    let images = ImageStore::new(root.join("images"))
        .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
        .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors().unwrap());
    images.pull_and_unpack(image).await.unwrap();
    let runtime = AnyGrill::with_host_processes(
        RuncGrill::new(
            root.join("bundles"),
            images,
            false,
            root.join("state"),
            env!("CARGO_BIN_EXE_bun").into(),
        )
        .unwrap(),
        &root.join("instances"),
        env!("CARGO_BIN_EXE_bun").into(),
    );
    let mut template = template;
    if template["runtime"] != "process" {
        template["image"] = json!(image);
    }

    // Leased for the whole test, so neither another test on the host nor
    // another worktree's run binds them (#852). Node `n` takes four ports
    // from `base + 4n`: gossip, Raft, reporting and API.
    let ports = bun_process::reserve_port_block_lease(3 * LEADER_LOSS_PORTS_PER_NODE);
    let gossip_port = |index: usize| ports.base + index as u16 * LEADER_LOSS_PORTS_PER_NODE;
    let shutdown = CancellationToken::new();
    let mut nodes = Vec::new();
    let mut services = Vec::new();
    for index in 0..3usize {
        let data = root.join(format!("node-{index}"));
        std::fs::create_dir_all(&data).unwrap();
        let runner = OwnedRunner::for_data_dir(runtime.clone(), &data).unwrap();
        let executor = TaskArrayNode::new(
            TaskArrayNodeConfig {
                audit_dir: None,
                root: data.join("task-arrays"),
                policy: ProcessWorkloadsConfig {
                    allowed_binaries: vec!["/bin/sh".into()],
                    mount_isolation: false,
                    ..Default::default()
                },
                default_concurrency: 4,
                backoff: (Duration::from_millis(1), Duration::from_millis(5)),
                group_commit: GroupCommit::default(),
            },
            NodeRunner::Owned(Box::new(runner)),
        );
        let service = Arc::new(TaskArrayService::with_timings(
            Some(Arc::new(executor)),
            Duration::from_millis(200),
            Duration::from_secs(3),
        ));
        services.push(Arc::downgrade(&service));
        nodes.push(
            cluster_support::start_wired_node(cluster_support::WiredNodeOptions {
                name: format!("loss{index}"),
                gossip_port: gossip_port(index),
                seeds: if index == 0 {
                    vec![]
                } else {
                    vec![cluster_support::local(gossip_port(0))]
                },
                shutdown: shutdown.child_token(),
                data_dir_prefix: "rb-leader-loss",
                data_root: Some(root.clone()),
                stale_report_timeout_secs: 10,
                stale_node_grace: None,
                metrics_rollup: None,
                scheduler: None,
                lease_reaper: false,
                membership: cluster_support::MembershipSource::Gossip,
                service_identity: Some("leader-loss-service".into()),
                operator_token: None,
                fault_injection: false,
                labels: Default::default(),
                keep_data_dir: false,
                task_arrays: Some(service),
                task_arrays_on_agent_budget: None,
                process_only: false,
            })
            .await,
        );
    }
    let deadline = |seconds| tokio::time::Instant::now() + Duration::from_secs(seconds);
    let until = deadline(60);
    loop {
        let mut ready = true;
        for node in &nodes {
            ready &= node.membership_table.read().await.len() == 3
                && node
                    .metrics_rx
                    .borrow()
                    .membership_config
                    .membership()
                    .voter_ids()
                    .count()
                    == 3;
        }
        if ready {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "three voters never formed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut leader = None;
    for (index, node) in nodes.iter().enumerate() {
        if node.council.is_leader().await {
            leader = Some(index);
        }
    }
    let leader = leader.expect("a leader");
    let survivors = [(leader + 1) % 3, (leader + 2) % 3];
    let http = reqwest::Client::new();
    let status = async |node: &cluster_support::WiredNode, id: u64| -> Option<Value> {
        http.get(format!("http://127.0.0.1:{}/v1/batch/{id}", node.api_port))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()
    };
    let answer: Value = http
        .post(format!(
            "http://127.0.0.1:{}/v1/batch/array",
            nodes[survivors[0]].api_port
        ))
        .json(&json!({
            "name": "leader-loss",
            "template": template,
            "spec": { "count": LEADER_LOSS_TASKS, "chunk_size": 20, "per_node_concurrency": 4 },
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let batch_id = answer["batch_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{answer}"));

    let leader_name = format!("loss{leader}");
    let until = deadline(180);
    loop {
        let running = status(&nodes[leader], batch_id)
            .await
            .is_some_and(|summary| {
                summary["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|row| {
                        row["node"] == leader_name.as_str()
                            && row["counters"]["succeeded"].as_u64().unwrap_or(0) > 0
                    })
            });
        if running {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "the leader never ran its share"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A chunk the leader finished and had accepted before the kill is a
    // legitimate acceptance; only one accepted after the takeover isn't.
    let lost = reliaburger::meat::NodeId::new(&leader_name);
    let accepted_before_kill =
        cluster_support::chunks_accepted_from(&nodes[leader], batch_id, &lost).await;
    nodes[leader].shutdown.cancel();

    let until = deadline(30);
    let new_leader = loop {
        let mut found = None;
        for index in survivors {
            if nodes[index].council.is_leader().await {
                found = Some(index);
            }
        }
        if let Some(index) = found {
            break index;
        }
        assert!(tokio::time::Instant::now() < until, "no new leader");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // Once the new leader applies an entry of its own term, everything the
    // old leader wrote while it led is applied here too.
    let until = deadline(30);
    while !cluster_support::applied_own_term(&nodes[new_leader]) {
        assert!(
            tokio::time::Instant::now() < until,
            "the new leader never applied its own term"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let accepted_under_old_leader =
        cluster_support::chunks_accepted_from(&nodes[new_leader], batch_id, &lost).await;
    assert!(
        accepted_before_kill.is_subset(&accepted_under_old_leader),
        "chunks {accepted_before_kill:?} accepted from {lost} at the kill are missing at takeover: {accepted_under_old_leader:?}"
    );
    let held_at_loss = nodes[new_leader]
        .council
        .desired_state()
        .await
        .task_arrays
        .get(batch_id)
        .and_then(|record| record.state.held_by(&lost).cloned())
        .unwrap_or_default();

    let until = deadline(300);
    let last = loop {
        if let Some(summary) = status(&nodes[new_leader], batch_id).await
            && summary["done"] == true
        {
            break summary;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "the array never finished under the new leader"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let flaky = LEADER_LOSS_TASKS / 100;
    if last["failed"] != 0 {
        // Say why each failed task failed before the assertions below stop.
        let base = format!("http://127.0.0.1:{}", nodes[new_leader].api_port);
        let failed: Value = match http
            .get(format!("{base}/v1/batch/{batch_id}/results?failed=true"))
            .send()
            .await
        {
            Ok(response) => response.json().await.unwrap_or_default(),
            Err(_) => Value::Null,
        };
        eprintln!("failed tasks: {failed}");
        for row in failed["rows"].as_array().into_iter().flatten() {
            let logs = match http
                .get(format!(
                    "{base}/v1/batch/{batch_id}/tasks/{}/logs",
                    row["index"]
                ))
                .send()
                .await
            {
                Ok(response) => response.text().await.unwrap_or_default(),
                Err(error) => error.to_string(),
            };
            eprintln!("task {} output: {logs}", row["index"]);
        }
    }
    assert_eq!(last["status"], "Succeeded", "{last}");
    assert_eq!(last["succeeded"], LEADER_LOSS_TASKS, "{last}");
    assert_eq!(last["failed"], 0, "{last}");
    assert_eq!(last["not_run"], 0, "{last}");
    assert_eq!(last["retried"], flaky, "{last}");
    assert_eq!(last["retry_causes"]["exit_code"], flaky, "{last}");

    let record = nodes[new_leader]
        .council
        .desired_state()
        .await
        .task_arrays
        .get(batch_id)
        .unwrap()
        .clone();
    let chunks = record.state.spec.chunk_count();
    let mut replayed = 0u64;
    for chunk in 0..chunks {
        let grant = record
            .state
            .accepted_grant(ChunkId(chunk))
            .expect("every chunk has one accepted grant");
        let attempt = grant.attempt;
        // Only a record folded past MAX_ACCEPTED_RANGES drops the owner.
        // The lost leader's own chunks count only if they were accepted in
        // its term: its executor keeps running, and no report it finishes
        // after the takeover may count.
        if grant.owner.as_ref() == Some(&lost) {
            assert!(
                accepted_under_old_leader.contains(&chunk),
                "chunk {chunk} was accepted from the lost leader after the takeover \
                 (accepted at kill {accepted_before_kill:?}, at takeover {accepted_under_old_leader:?})"
            );
            assert_eq!(
                attempt, 1,
                "chunk {chunk} was accepted from the lost leader at a later attempt"
            );
        }
        if attempt > 1 {
            replayed += 1;
            assert!(
                held_at_loss.contains(chunk),
                "chunk {chunk} replayed at attempt {attempt} but the lost leader didn't hold it"
            );
        }
    }
    assert!(replayed <= held_at_loss.len());
    eprintln!(
        "leader loss with {}: {} chunks accepted from it at the kill, {} by the takeover, {} held at loss, {replayed} replayed",
        template["runtime"],
        accepted_before_kill.len(),
        accepted_under_old_leader.len(),
        held_at_loss.len()
    );
    // Every executor must retire and be forgotten while its node still
    // runs. Cancelling the nodes first left their pools retiring executors
    // the runtime already reported stopped: one pool's forget recreated
    // its route claim under `root` while the teardown removed it (#924).
    // The lost leader's pool went with its control plane, so its idle
    // executors stop without being forgotten. A restarted Bun's startup
    // sweep forgets them (#678); run that sweep here.
    let lost_runner =
        OwnedRunner::for_data_dir(runtime.clone(), &root.join(format!("node-{leader}"))).unwrap();
    let routes = root.join("instances/runtime-routes");
    let until = deadline(120);
    loop {
        lost_runner.forget_retired_executors().await;
        let launches: Vec<_> = runtime
            .launch_inventory()
            .await
            .unwrap()
            .unwrap_or_default()
            .into_iter()
            .map(|launch| launch.instance_id.0)
            .collect();
        let claims: Vec<_> = std::fs::read_dir(&routes)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        if launches.is_empty() && claims.is_empty() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "executors not retired and forgotten: {launches:?}, route files {claims:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The lost node's pool, which owned its executors' slot cgroups, is
    // gone. A restarted Bun's sweep removes the ones it left (#952).
    lost_runner.forget_retired_executors().await;
    drop(lost_runner);
    // Retirement tears each executor's network down with it (#786).
    assert_eq!(residue.namespaces(), Vec::<String>::new());
    assert_eq!(residue.slot_cgroups(), Vec::<String>::new());
    shutdown.cancel();
    // Dropping the nodes joins their agent, API and Raft tasks. Each node's
    // leader loop stops by itself once its agent has, after finishing any
    // tick in progress, and that tick may still sync a node's ledger. The
    // last reference to a node's task-array service goes with the loop.
    drop(nodes);
    let until = deadline(30);
    while services.iter().any(|service| service.strong_count() > 0) {
        assert!(
            tokio::time::Instant::now() < until,
            "a node's task-array service outlived its node"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(residue);
    // The nodes' data lives here too, volumes and all.
    cluster_support::clear_fixture_dir(&root).unwrap();
}

/// The leader loses its control plane while shared-runc chunks run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires root, runc, cgroup v2 and the pinned test image; run with make test-linux"]
async fn leader_loss_mid_array_loses_and_duplicates_nothing_with_shared_runc_executors() {
    leader_loss_mid_array(serde_json::json!({
        "runtime": "shared-runc",
        "command": ["/bin/sh", "-c", LEADER_LOSS_SCRIPT],
        "cpu": "100m",
        "memory": "32Mi",
    }))
    .await;
}

/// The leader loses its control plane while native host chunks run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires root, runc, cgroup v2 and the pinned test image; run with make test-linux"]
async fn leader_loss_mid_array_loses_and_duplicates_nothing_with_cgroup_host_executors() {
    leader_loss_mid_array(serde_json::json!({
        "runtime": "process",
        "exec": "/bin/sh",
        "command": ["-c", LEADER_LOSS_SCRIPT],
        "cpu": "100m",
        "memory": "32Mi",
    }))
    .await;
}
