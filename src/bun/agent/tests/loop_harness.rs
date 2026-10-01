//! The agent loop's starvation harness (#351).
//!
//! The loop runs one branch at a time, so a turn that awaits something slow
//! holds every caller. Each scenario here makes one await from the agent-loop
//! review's list of inline stalls slow (a runtime call through `MockGrill`, a
//! council write that never returns, a log client that never reads, or a
//! [`LoopStall`] where no mock stands behind the await), starts the work that
//! reaches it, and then checks two things:
//!
//! 1. a `Status` command queued while the slow work runs is answered within
//!    [`TURN_BUDGET`] (1 s), and
//! 2. the loop's worst turn, as the turn meter saw it, stays under the same
//!    budget.
//!
//! The slow work lasts well past the budget (usually 2.5 s), so a scenario
//! either passes clearly or fails clearly. The scenarios that fail today are
//! ignored with the stage of #351 that will fix them; un-ignore one in the
//! same change that makes it pass.

use super::*;
use crate::bun::loop_meter::{LoopTurnMeter, TURN_BUDGET};
use crate::grill::mock::MockCall;

/// Long enough past the budget that a pass or a failure is unambiguous.
const STALL: std::time::Duration = std::time::Duration::from_millis(2500);

/// How long a scenario waits for the queued status before calling it
/// unanswered. A hung await never answers, so this bounds the test.
const STATUS_PATIENCE: std::time::Duration = std::time::Duration::from_secs(4);

/// An agent running its loop in a task, with the handles a scenario needs.
struct RunningAgent {
    tx: mpsc::Sender<AgentCommand>,
    shutdown: CancellationToken,
    meter: Arc<LoopTurnMeter>,
    stalls: Arc<LoopStalls>,
    task: tokio::task::JoinHandle<()>,
}

impl RunningAgent {
    fn start<A>(mut agent: A, tx: mpsc::Sender<AgentCommand>, shutdown: CancellationToken) -> Self
    where
        A: std::ops::DerefMut<Target = BunAgent<MockGrill>> + Send + 'static,
    {
        let meter = agent.loop_meter();
        let stalls = Arc::clone(&agent.loop_stalls);
        let task = tokio::spawn(async move { agent.run().await });
        Self {
            tx,
            shutdown,
            meter,
            stalls,
            task,
        }
    }

    /// Forget the turns setup took, so the verdict covers the slow work only.
    fn measure_from_here(&self) {
        self.meter.reset_worst_turn();
    }

    /// Queue a status command now and time its answer.
    async fn status_latency(&self) -> Option<std::time::Duration> {
        let queued = std::time::Instant::now();
        let (response, answer) = oneshot::channel();
        self.tx.send(AgentCommand::Status { response }).await.ok()?;
        tokio::time::timeout(STATUS_PATIENCE, answer)
            .await
            .ok()?
            .ok()?;
        Some(queued.elapsed())
    }

    /// Queue a status command while `slow_work` runs, then judge the loop.
    /// The agent is stopped (aborted, if a hung await holds it) either way.
    async fn assert_responsive(self, slow_work: &str) {
        let latency = self.status_latency().await;
        let worst = self.meter.worst_turn();
        self.shutdown.cancel();
        let mut task = self.task;
        if tokio::time::timeout(std::time::Duration::from_secs(2), &mut task)
            .await
            .is_err()
        {
            task.abort();
        }

        match latency {
            Some(latency) if latency < TURN_BUDGET => {}
            Some(latency) => {
                panic!("status took {latency:?} while {slow_work}; the budget is {TURN_BUDGET:?}")
            }
            None => panic!("status was not answered in {STATUS_PATIENCE:?} while {slow_work}"),
        }
        if let Some(worst) = worst {
            assert!(
                worst.took < TURN_BUDGET,
                "the worst loop turn took {worst} while {slow_work}; the budget is {TURN_BUDGET:?}"
            );
        }
    }
}

/// Wait until the grill has seen `count` more `operation` calls than `before`.
pub(super) async fn wait_for_calls(
    grill: &MockGrill,
    operation: &str,
    before: usize,
    count: usize,
) {
    let seen = |grill: &MockGrill| {
        grill
            .calls()
            .iter()
            .filter(|(called, _)| called == operation)
            .count()
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while seen(grill) < before + count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the slow work never called {operation}"));
}

pub(super) fn calls_of(grill: &MockGrill, operation: &str) -> usize {
    grill
        .calls()
        .iter()
        .filter(|(called, _)| called == operation)
        .count()
}

pub(super) fn replicated(app: &str, replicas: u32) -> Config {
    Config::parse(&format!(
        "[app.{app}]\nimage = '{app}:v1'\nport = 8080\nreplicas = {replicas}\n"
    ))
    .unwrap()
}

/// Kill the only replica of `web` behind the runtime's back; the health tick
/// notices and restarts it.
pub(super) fn crash(grill: &MockGrill) {
    let id = InstanceId("default__web-0".to_string());
    grill.set_state(&id, ContainerState::Stopped);
    grill.set_exit_code(&id, Some(1));
}

// ---- runtime calls ----------------------------------------------------------

/// `check_apps` asks the runtime for every running app's state, one at a
/// time, on the tick. Ten replicas at 250 ms each is one 2.5 s turn.
#[tokio::test]
async fn status_answers_while_the_tick_reads_every_app_state() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 10)).await);
    let running = RunningAgent::start(agent, tx, shutdown);
    let before = calls_of(&grill, "state");
    running.measure_from_here();
    grill.set_call_delay(MockCall::State, Some(STALL / 10));
    wait_for_calls(&grill, "state", before, 1).await;
    running
        .assert_responsive("check_apps read ten instance states at 250 ms each")
        .await;
}

/// `check_jobs` does the same for every running job.
#[tokio::test]
async fn status_answers_while_the_tick_reads_every_job_state() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let jobs: String = (0..10)
        .map(|index| format!("[job.batch{index}]\nimage = 'batch:v1'\ncommand = ['sleep', '60']\n"))
        .collect();
    expect_complete(&drain_deploy(&mut agent, Config::parse(&jobs).unwrap()).await);
    let running = RunningAgent::start(agent, tx, shutdown);
    let before = calls_of(&grill, "state");
    running.measure_from_here();
    grill.set_call_delay(MockCall::State, Some(STALL / 10));
    wait_for_calls(&grill, "state", before, 1).await;
    running
        .assert_responsive("check_jobs read ten job states at 250 ms each")
        .await;
}

/// A restart first kills what's left of the old container and waits for the
/// runtime to confirm, inline, before the tick's restart budget is checked.
#[tokio::test]
async fn status_answers_while_a_restart_waits_for_its_kill() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let running = RunningAgent::start(agent, tx, shutdown);
    grill.set_call_delay(MockCall::Kill, Some(STALL));
    let before = calls_of(&grill, "kill");
    running.measure_from_here();
    crash(&grill);
    wait_for_calls(&grill, "kill", before, 1).await;
    running
        .assert_responsive("a restart waited for its predecessor's kill")
        .await;
}

/// Then it creates and starts the replacement, inline too.
#[tokio::test]
async fn status_answers_while_a_restart_creates_and_starts() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let running = RunningAgent::start(agent, tx, shutdown);
    grill.set_call_delay(MockCall::Create, Some(STALL / 2));
    grill.set_call_delay(MockCall::Start, Some(STALL / 2));
    let before = calls_of(&grill, "create");
    running.measure_from_here();
    crash(&grill);
    wait_for_calls(&grill, "create", before, 1).await;
    running
        .assert_responsive("a restart created and started its replacement")
        .await;
}

/// A deploy's `ApplyNetworkPreStart` step retains the instance's network
/// reference through the runtime, under its lifecycle lock.
#[tokio::test]
#[ignore = "stage 3 of #351"]
async fn status_answers_while_a_deploy_step_retains_a_network_reference() {
    let (agent, tx, shutdown, grill) = test_agent_with_grill();
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    grill.set_call_delay(MockCall::RetainNetworkReference, Some(STALL));
    running.measure_from_here();
    let (events, _progress) = mpsc::channel(64);
    tx.send(AgentCommand::Deploy {
        config: replicated("web", 1),
        events,
    })
    .await
    .unwrap();
    wait_for_calls(&grill, "retain_network_reference", 0, 1).await;
    running
        .assert_responsive("a deploy step retained a network reference")
        .await;
}

/// `Logs` reads every instance's whole capture into memory, on the loop. A
/// 56 MB capture (#278) takes hundreds of milliseconds; a slow disk longer.
#[tokio::test]
async fn status_answers_while_logs_reads_a_large_capture() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    grill.set_call_delay(MockCall::Logs, Some(STALL));
    running.measure_from_here();
    let (response, _logs) = oneshot::channel();
    tx.send(AgentCommand::Logs {
        app_name: "web".into(),
        namespace: "default".into(),
        tail: None,
        response,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    running.assert_responsive("logs read a large capture").await;
}

/// A status answer reads every instance's pid under one shared 500 ms
/// deadline, so even a hung runtime costs a status turn half the budget.
#[tokio::test]
async fn status_answers_while_every_pid_read_hangs() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 3)).await);
    let running = RunningAgent::start(agent, tx, shutdown);
    grill.set_pid_delay(Some(std::time::Duration::from_secs(30)));
    running.measure_from_here();
    running
        .assert_responsive("every runtime pid read hung")
        .await;
}

// ---- council and peers ------------------------------------------------------

/// A single-node council on the in-memory Raft network, leading.
async fn council() -> Arc<CouncilNode> {
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
    node.initialize(std::collections::BTreeMap::from([(
        1u64,
        CouncilNodeInfo::new(
            std::net::SocketAddr::from(([127, 0, 0, 1], 9)),
            "node-1".to_string(),
        ),
    )]))
    .await
    .unwrap();
    for _ in 0..100 {
        if node.is_leader().await {
            return Arc::new(node);
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the single-node council never elected itself");
}

/// An agent in a cluster whose council is `council`.
fn clustered_agent(
    council: Arc<CouncilNode>,
) -> (
    BunAgent<MockGrill>,
    mpsc::Sender<AgentCommand>,
    CancellationToken,
) {
    let (_membership_tx, membership_rx) = tokio::sync::watch::channel(Vec::new());
    let (_snapshot_tx, snapshot_rx) = mpsc::channel(1);
    let (tx, rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let cluster = ClusterHandle {
        local_node_id: crate::meat::NodeId::new("node-1"),
        membership_rx,
        raft_metrics_rx: Some(council.raft().metrics()),
        council: Some(council),
        snapshot_rx,
        wrapping_ikm: Some([7; 32]),
        partition_blocklists: PartitionBlocklists::default(),
        crl_handle: Default::default(),
    };
    let mut agent = BunAgent::with_cluster(
        MockGrill::new(),
        PortAllocator::new(30000, 31000),
        rx,
        shutdown.clone(),
        cluster,
        "test".to_string(),
    );
    // Several in-process agents share the host; none may touch its firewall.
    agent.set_perimeter_enabled(false);
    (agent, tx, shutdown)
}

/// `Box` stands in for the test agent wrapper: `start` wants something that
/// derefs to the agent and can move into a task.
fn boxed(agent: BunAgent<MockGrill>) -> Box<BunAgent<MockGrill>> {
    Box::new(agent)
}

/// A join consumes its token through a council write. On a leader that has
/// lost quorum the write waits until the leader steps down, or longer.
#[tokio::test]
async fn status_answers_while_a_join_waits_on_a_council_without_quorum() {
    let council = council().await;
    let (token, join_token) = crate::sesame::join::create_join_token(
        crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL,
        "node-99",
    )
    .unwrap();
    council
        .write(crate::council::RaftRequest::CreateJoinToken(join_token))
        .await
        .unwrap();
    let (agent, tx, shutdown) = clustered_agent(Arc::clone(&council));
    let running = RunningAgent::start(boxed(agent), tx.clone(), shutdown);
    council.hang_writes();
    running.measure_from_here();
    let (response, _bundle) = oneshot::channel();
    tx.send(AgentCommand::JoinIssue {
        token,
        node_id: "node-99".into(),
        csr_der: vec![],
        response,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    running
        .assert_responsive("a join waited on a council write that never returns")
        .await;
}

/// Signing an image attaches the signature through a council write too.
#[tokio::test]
async fn status_answers_while_signing_waits_on_a_council_without_quorum() {
    let council = council().await;
    let (agent, tx, shutdown) = clustered_agent(Arc::clone(&council));
    let running = RunningAgent::start(boxed(agent), tx.clone(), shutdown);
    council.hang_writes();
    running.measure_from_here();
    let key = crate::pickle::signing::SigningKey::generate().unwrap();
    let digest = crate::pickle::types::Digest::from_sha256_hex(&"1".repeat(64));
    let (response, _signed) = oneshot::channel();
    tx.send(AgentCommand::SignImage {
        submission: key.sign(&digest).unwrap(),
        response,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    running
        .assert_responsive("signing waited on a council write that never returns")
        .await;
}

/// `Council` clones the whole desired state to answer. That's milliseconds
/// today; the scenario guards it as the state grows.
#[tokio::test]
async fn status_answers_while_council_status_clones_the_desired_state() {
    let council = council().await;
    for index in 0..200 {
        let (_, join_token) = crate::sesame::join::create_join_token(
            crate::sesame::join::DEFAULT_JOIN_TOKEN_TTL,
            &format!("node-{index}"),
        )
        .unwrap();
        council
            .write(crate::council::RaftRequest::CreateJoinToken(join_token))
            .await
            .unwrap();
    }
    let (agent, tx, shutdown) = clustered_agent(Arc::clone(&council));
    let running = RunningAgent::start(boxed(agent), tx.clone(), shutdown);
    running.measure_from_here();
    for _ in 0..20 {
        let (response, status) = oneshot::channel();
        tx.send(AgentCommand::Council { response }).await.unwrap();
        status.await.unwrap();
    }
    running
        .assert_responsive("council status cloned the desired state twenty times")
        .await;
}

/// A registry that accepts connections and never answers.
async fn silent_registry() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    (address, task)
}

/// `UpgradeApply` fetches the new binary before it answers. Against a
/// registry that hangs, one attempt waits 5 s for headers and the fetch
/// retries for up to 75 s, all of it on the loop.
#[tokio::test]
#[ignore = "stage 3 of #351"]
async fn status_answers_while_an_upgrade_fetches_from_a_silent_registry() {
    use crate::upgrade::signing::{encode_public_key, generate_keypair, sha256_hex, sign};

    let dir = tempfile::tempdir().unwrap();
    let binary_dir = dir.path().join("bin");
    std::fs::create_dir_all(&binary_dir).unwrap();
    let running_version: crate::upgrade::BinaryVersion = "0.1.0".parse().unwrap();
    let store = crate::upgrade::store::BinaryStore::new(binary_dir.clone(), "bun".to_string());
    store
        .stage(
            &running_version,
            b"old binary",
            &crate::upgrade::signing::SignatureEnvelope {
                schema: 1,
                sha256: sha256_hex(b"old binary"),
                embedded: String::new(),
                external: None,
            },
        )
        .unwrap();
    store.activate(&running_version).unwrap();
    let (release_pkcs8, release_public) = generate_keypair().unwrap();
    let manager = crate::upgrade::manager::UpgradeManager::new(
        &crate::config::node::UpgradeSection {
            binary_dir: Some(binary_dir.clone()),
            release_keys_override: Some(vec![encode_public_key(&release_public)]),
            ..Default::default()
        },
        &dir.path().join("data"),
        &binary_dir.join("bun"),
        running_version,
        vec!["bun".to_string()],
    )
    .unwrap();

    let (registry, _registry) = silent_registry().await;
    let binary = b"the next bun".to_vec();
    let directive = crate::upgrade::types::UpgradeDirective {
        upgrade_id: "upgrade-1".into(),
        target_version: "0.2.0".parse().unwrap(),
        binary_sha256: sha256_hex(&binary),
        embedded_signature: sign(&release_pkcs8, &binary).unwrap(),
        external_signature: None,
        source: crate::upgrade::types::BinarySource::Pickle {
            registry_address: registry,
        },
        network_provenance: true,
        allow_downgrade: false,
    };

    let (mut agent, tx, shutdown) = test_agent();
    agent.set_upgrade_manager(manager);
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    running.measure_from_here();
    let (response, _applied) = oneshot::channel();
    tx.send(AgentCommand::UpgradeApply {
        directive,
        response,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    running
        .assert_responsive("an upgrade fetched its binary from a registry that never answers")
        .await;
}

// Egress DNS re-resolution (`reresolve_egress`) has no scenario: it runs only
// with a loaded eBPF program, which a unit test can't construct. Stage 3 of
// #351 moves the lookups into a task, which makes them testable without one.

// ---- disk, kernel and subprocesses -------------------------------------------

/// fsync'd persists may stay inline (#351, decision 2), as long as a slow disk
/// can't stretch a turn past the budget. At 150 ms a persist, a deploy of
/// three replicas, a job and a restart must still keep every turn short.
#[tokio::test]
async fn status_answers_while_every_persist_waits_on_a_slow_disk() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let records = tempfile::tempdir().unwrap();
    // A durable record needs a live process identity; this one is ours.
    grill.set_pid(std::process::id());
    agent.set_records_dir(records.path().to_path_buf());
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    running
        .stalls
        .set(LoopStall::Persist, std::time::Duration::from_millis(150));
    running.measure_from_here();

    let deploy = Config::parse(
        "[app.api]\nimage = 'api:v1'\nport = 9090\nreplicas = 3\n\n\
         [job.report]\nimage = 'report:v1'\ncommand = ['true']\n",
    )
    .unwrap();
    let starts_before = calls_of(&grill, "start");
    let (events, mut progress) = mpsc::channel(64);
    tx.send(AgentCommand::Deploy {
        config: deploy,
        events,
    })
    .await
    .unwrap();
    crash(&grill);
    let mid_deploy = running.status_latency().await;
    let last_event = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut last = None;
        while let Some(event) = progress.recv().await {
            last = Some(event);
        }
        last
    })
    .await
    .expect("the deploy never finished");
    expect_complete(&last_event.into_iter().collect::<Vec<_>>());
    // Three api replicas, the job, and web's restart all started.
    wait_for_calls(&grill, "start", starts_before, 5).await;
    for id in ["default__api-0", "default__api-1", "default__api-2"] {
        assert!(
            crate::grill::records::record_path(records.path(), id).exists(),
            "{id} was never persisted"
        );
    }
    assert!(
        mid_deploy.is_some_and(|latency| latency < TURN_BUDGET),
        "status took {mid_deploy:?} mid-deploy on a slow disk"
    );
    running
        .assert_responsive("a deploy, a job and a restart persisted state to a slow disk")
        .await;
}

/// Retiring an instance removes its identity directory and record inline.
#[tokio::test]
#[ignore = "stage 3 of #351"]
async fn status_answers_while_a_retirement_removes_artifacts() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    running.stalls.set(LoopStall::ArtifactCleanup, STALL);
    running.measure_from_here();
    let before = calls_of(&grill, "stop");
    let (response, _stopped) = oneshot::channel();
    tx.send(AgentCommand::Retire {
        app_name: "web".into(),
        namespace: "default".into(),
        response,
    })
    .await
    .unwrap();
    wait_for_calls(&grill, "stop", before, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    running
        .assert_responsive("a retirement removed the instance's artifacts")
        .await;
}

/// The tick applies the perimeter ruleset with an `nft` subprocess when
/// membership changes (and on the first tick).
#[tokio::test]
#[ignore = "stage 3 of #351"]
async fn status_answers_while_the_tick_applies_the_firewall() {
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_perimeter_enabled(true);
    let running = RunningAgent::start(agent, tx, shutdown);
    running.stalls.set(LoopStall::Firewall, STALL);
    running.measure_from_here();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    running
        .assert_responsive("the tick applied the perimeter firewall")
        .await;
}

/// Injecting a workload fault reads every target's pid, one at a time, before
/// it signals or writes a cgroup.
#[tokio::test]
#[ignore = "stage 3 of #351"]
async fn status_answers_while_a_fault_reads_its_targets() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 5)).await);
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    // No pid comes back, so nothing is signalled; the reads are the cost. A
    // resume has no replica minimum to refuse it first.
    grill.set_pid_delay(Some(STALL / 5));
    running.measure_from_here();
    let (response, _injected) = oneshot::channel();
    tx.send(AgentCommand::InjectFault {
        reservation: None,
        replica_evidence: None,
        request: crate::smoker::types::FaultRequest {
            fault_type: crate::smoker::types::FaultType::Resume,
            target_service: "web".into(),
            namespace: Some("default".into()),
            target_instance: None,
            target_node: None,
            duration: std::time::Duration::from_secs(60),
            injected_by: "harness".into(),
            reason: None,
            include_leader: false,
            override_safety: true,
            acknowledged: true,
        },
        response,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    running
        .assert_responsive("a resume fault read five target pids at 500 ms each")
        .await;
}

/// A cluster catalogue answer republishes the discovery view and rebuilds
/// routing, O(catalogue). Two thousand services must still fit in a turn.
#[tokio::test]
async fn status_answers_while_a_large_catalogue_is_published() {
    let (agent, tx, shutdown) = test_agent();
    let services = (0..2000).map(|index| {
        (
            crate::onion::service_id::ServiceId::new("default", format!("svc{index}")),
            8080,
            (0..3)
                .map(|replica| crate::onion::catalog::CatalogBackend {
                    execution: None,
                    node_id: format!("node-{replica}"),
                    node_ip: std::net::Ipv4Addr::new(192, 168, 1, 2 + replica),
                    host_port: 30000 + (index % 1000) as u16,
                    healthy: true,
                })
                .collect::<Vec<_>>(),
        )
    });
    let catalog = crate::onion::catalog::EndpointCatalog::rebuild(services).unwrap();
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    running.measure_from_here();
    for generation in 1..=3 {
        let (response, published) = oneshot::channel();
        tx.send(AgentCommand::SyncClusterCatalog {
            generation,
            catalog: Box::new(catalog.clone()),
            ingress: vec![],
            response,
        })
        .await
        .unwrap();
        published.await.unwrap().unwrap();
    }
    running
        .assert_responsive("a two-thousand-service catalogue was published three times")
        .await;
}

// ---- callers that can hold a turn ---------------------------------------------

/// `relish logs -f --tail` sends the tail lines into the API's 64-slot
/// channel from the loop. A client that stops reading (`| less`, a stuck
/// proxy) holds the turn once 64 lines are queued.
#[tokio::test]
async fn status_answers_while_a_follow_tail_waits_on_a_client_that_never_reads() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let tail: String = (0..200).map(|line| format!("line {line}\n")).collect();
    grill.set_logs(&InstanceId("default__web-0".into()), tail);
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    running.measure_from_here();
    // The API's channel size; the receiver is kept and never read.
    let (lines, _never_read) = mpsc::channel::<String>(64);
    tx.send(AgentCommand::FollowLogs {
        app_name: "web".into(),
        namespace: "default".into(),
        tail: Some(200),
        label: None,
        lines,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    running
        .assert_responsive("a follow's tail waited on a client that never reads")
        .await;
}
