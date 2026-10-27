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

use super::commands::AgentCommand;
use super::*;
use crate::bun::loop_meter::{LoopBranch, LoopTurnMeter, TURN_BUDGET, TurnPart};
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
        self.stalls.reset_most_in_a_turn();
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

/// An egress-protected instance's pre-start resolved its allowlist inside
/// the turn, under a fixed 5 s timeout (#419). The deploy worker resolves
/// it now, before it asks the loop to program it.
#[tokio::test]
async fn status_answers_while_a_deploy_resolves_an_egress_allowlist() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    // Admit the allowlist. The mock has no kernel hooks, so the loop then
    // refuses to program it, but only after the worker's lookup.
    agent
        .supervisor
        .set_egress_capability(crate::sesame::egress::EgressEnforcementCapability {
            connect_ipv4: true,
            connect_ipv6: true,
            udp_ipv4: true,
            udp_ipv6: true,
            pre_start: true,
        });
    let running = RunningAgent::start(agent, tx.clone(), shutdown);
    running.stalls.set(LoopStall::EgressDns, STALL);
    running.measure_from_here();
    let (events, _progress) = mpsc::channel(64);
    let config = Config::parse(
        "[app.web]\nimage = 'web:v1'\nport = 8080\n[app.web.egress]\nallow = ['203.0.113.9:443']\n",
    )
    .unwrap();
    tx.send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    wait_for_calls(&grill, "retain_network_reference", 0, 1).await;
    // The lookup comes straight after the retain; queue the status while
    // it is under way.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    running
        .assert_responsive("a deploy resolved its egress allowlist")
        .await;
}

/// Journalling a restarted backend's publication read the runtime's whole
/// launch inventory inside the turn, under a fixed 5 s timeout (#419).
/// The restart step reads the instance's execution now, with the rest of
/// what it started, and the publication takes it from there.
#[tokio::test]
async fn status_answers_while_a_restart_reads_a_slow_launch_inventory() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let running = RunningAgent::start(agent, tx, shutdown);
    grill.set_inventory_delay(Some(STALL));
    let before = calls_of(&grill, "start");
    running.measure_from_here();
    crash(&grill);
    wait_for_calls(&grill, "start", before, 1).await;
    // The step reads the inventory right after the start, then the loop
    // journals the publication. Judge the turns once both had time to meet
    // the slow inventory.
    tokio::time::sleep(STALL * 2 + std::time::Duration::from_millis(500)).await;
    let latency = running.status_latency().await;
    let worst = running.meter.worst_turn();
    stop(running).await;

    assert!(
        latency.is_some(),
        "status went unanswered while a restart read a slow launch inventory"
    );
    // Judge the inventory read, not the host disk. The publication's turn
    // journals a real fsync'd write, which a loaded runner stretched past the
    // 1 s budget on its own (1047 ms, the #508 class). An inventory read on
    // the loop would cost the whole 2.5 s stall, so the worst turn, less its
    // disk writes, stays under that stall and never reads the inventory.
    if let Some(worst) = worst {
        assert_eq!(
            worst.part(TurnPart::RuntimeInventory),
            None,
            "a loop turn read the slow launch inventory: {worst}"
        );
        let disk: std::time::Duration = [
            TurnPart::JournalWrite,
            TurnPart::InFlightWrite,
            TurnPart::EgressWrite,
        ]
        .into_iter()
        .filter_map(|part| worst.part(part))
        .sum();
        assert!(
            worst.took.saturating_sub(disk) < STALL,
            "the worst loop turn waited {:?} beside its disk writes ({worst}); a \
             launch inventory read on the loop would cost the {STALL:?} stall",
            worst.took.saturating_sub(disk)
        );
    }
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
        answer_delivered: None,
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    running
        .assert_responsive("an upgrade fetched its binary from a registry that never answers")
        .await;
}

// Egress DNS re-resolution (`reresolve_egress`) has no scenario: it runs only
// with a loaded eBPF program, which a unit test can't construct. The lookups
// run in a task now; `egress_resolution`'s tests cover what it resolves and
// which bindings an answer may still change. The execution fence that follows
// a failed egress stop needs no eBPF program, and has a scenario below the
// retirement ones.

// ---- disk, kernel and subprocesses -------------------------------------------

/// Retiring an instance removes its identity directory and record inline.
#[tokio::test]
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

/// Past the turn's runtime budget, inside the stop's patience.
const SLOW_RUNTIME: std::time::Duration = std::time::Duration::from_millis(1500);

/// Stop `web` through the loop, check the loop stays responsive while the
/// stop retires the instance, and return the stop's answer. `slow_calls`
/// counts the slow runtime call, which the stop must reach.
async fn stop_through_a_slow_retirement(
    running: &RunningAgent,
    grill: &MockGrill,
    slow_calls: impl Fn(&MockGrill) -> usize,
) -> Result<(), BunError> {
    let before = slow_calls(grill);
    let (response, stopped) = oneshot::channel();
    running
        .tx
        .send(AgentCommand::Stop {
            app_name: "web".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while slow_calls(grill) == before {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stop never reached the slow runtime call");
    let latency = running.status_latency().await;
    assert!(
        latency.is_some_and(|latency| latency < TURN_BUDGET),
        "status took {latency:?} while retirement waited on the runtime"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), stopped)
        .await
        .expect("the stop was never answered")
        .expect("the stop's waiter was dropped")
}

/// Retiring a stopped instance asks the runtime for its network reference.
/// On runc that waits for the instance's lifecycle lock, behind the health
/// sweep's and the status reader's state reads, and the turn's budget used
/// to run out first: the stop answered "the runtime did not answer for the
/// network reference within the turn" (#387). The read now finishes off the
/// loop and the stop answers once it has, having asked the runtime once.
#[tokio::test]
async fn a_stop_waits_out_a_network_reference_read_slower_than_the_turn() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let running = RunningAgent::start(agent, tx, shutdown);
    grill.set_call_delay(MockCall::NetworkReference, Some(SLOW_RUNTIME));
    running.measure_from_here();
    let stopped =
        stop_through_a_slow_retirement(&running, &grill, MockGrill::network_reference_reads).await;
    assert!(stopped.is_ok(), "{stopped:?}");
    assert_eq!(grill.network_reference_reads(), 1);
    running
        .assert_responsive("a stop read the network reference")
        .await;
}

/// The same for the release itself, of a reference the agent holds.
#[tokio::test]
async fn a_stop_waits_out_a_network_reference_release_slower_than_the_turn() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    assert_eq!(
        agent.network_references.get(&reference.instance_id),
        Some(&reference)
    );
    let running = RunningAgent::start(agent, tx, shutdown);
    grill.set_call_delay(MockCall::ReleaseNetworkReference, Some(SLOW_RUNTIME));
    running.measure_from_here();
    let stopped = stop_through_a_slow_retirement(&running, &grill, |grill| {
        calls_of(grill, "release_network_reference")
    })
    .await;
    assert!(stopped.is_ok(), "{stopped:?}");
    assert_eq!(calls_of(&grill, "release_network_reference"), 1);
    running
        .assert_responsive("a stop released the network reference")
        .await;
}

/// When a stop the egress fence relies on fails, the fence force-kills the
/// app, but only once the runtime confirms each instance still holds its
/// original address (#357). On runc that read waits for the lifecycle lock,
/// as retirement's does (#387), and it used to fail on the turn's budget,
/// leaving the app running until a later egress check fenced it again
/// (#393). The read now finishes off the loop: the stop keeps its waiters
/// until the fence holds, and the runtime is asked once.
#[tokio::test]
async fn a_failed_stop_fences_through_a_network_reference_read_slower_than_the_turn() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    grill.set_container_ip(std::net::Ipv4Addr::new(10, 0, 0, 7));
    grill
        .set_network_reference(original_test_network_reference())
        .await;
    expect_complete(&drain_deploy(&mut agent, replicated("web", 1)).await);
    let id = InstanceId("default__web-0".into());
    grill.set_fail_stop(true);
    // An operator stop is pending when the egress fence comes to rely on it.
    let (response, stopped) = oneshot::channel();
    agent
        .request_app_stop("web".into(), "default".into(), StopPurpose::Stop, response)
        .await;
    agent.stop_app_unattended("web", "default").await.unwrap();
    let reads_before = grill.network_reference_reads();
    grill.set_call_delay(MockCall::NetworkReference, Some(SLOW_RUNTIME));
    let running = RunningAgent::start(agent, tx, shutdown);
    running.measure_from_here();

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while grill.network_reference_reads() == reads_before {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the fence never read the network reference");
    let latency = running.status_latency().await;
    assert!(
        latency.is_some_and(|latency| latency < TURN_BUDGET),
        "status took {latency:?} while the fence waited on the runtime"
    );
    let answer = tokio::time::timeout(std::time::Duration::from_secs(10), stopped)
        .await
        .expect("the stop was never answered")
        .expect("the stop's waiter was dropped");
    assert!(answer.is_err(), "the stop must report its failure");
    let kills = grill
        .calls()
        .iter()
        .filter(|(op, i)| op == "kill" && i == &id)
        .count();
    assert_eq!(kills, 1, "the stop was answered before the fence held");
    assert_eq!(grill.network_reference_reads(), reads_before + 1);
    running
        .assert_responsive("the execution fence read the network reference")
        .await;
}

/// The tick applies the perimeter ruleset with an `nft` subprocess when
/// membership changes (and on the first tick).
#[tokio::test]
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
            workload: Default::default(),
            task_index: None,
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

// ---- consumer synchronisation (#505) ------------------------------------------

/// How long one discovery-journal write takes in the consumer scenarios.
/// The 0.1.4 candidate's compressed soak logged consumer syncs of 1.0 to
/// 1.2 s that journalled three writes each, so about 400 ms a write.
const SOAK_JOURNAL_WRITE: std::time::Duration = std::time::Duration::from_millis(400);

/// An agent enrolled as a cluster consumer that has just recovered its
/// ownership, so its view is withdrawn and nothing routes yet. Returns the
/// agent, its state directory, and a leader answer to publish.
async fn recovered_consumer() -> (
    BunAgent<MockGrill>,
    tempfile::TempDir,
    crate::onion::catalog::EndpointCatalog,
    Vec<crate::cluster::orchestrate::IngressAssignment>,
) {
    let root = tempfile::tempdir().unwrap();
    let (mut agent, _, _) = test_cluster_fault_agent().await;
    agent.set_records_dir(root.path().to_owned());
    agent
        .supervisor
        .grill()
        .set_launch_inventory(Vec::new())
        .await;
    let identity = crate::bun::consumer_owners::ConsumerIdentity {
        node_id: crate::meat::NodeId::new("test"),
        cluster_identity: [42; 32],
    };
    agent
        .recover_consumer_ownership(&root.path().join("discovery"), identity)
        .await
        .unwrap();
    let (catalog, ingress) = cluster_publication_fixture();
    (agent, root, catalog, ingress)
}

/// The fixture's catalogue with its one remote backend moved to another
/// port: a new view, replaced in place.
fn moved_remote_backend() -> crate::onion::catalog::EndpointCatalog {
    crate::onion::catalog::EndpointCatalog::rebuild([(
        crate::onion::service_id::ServiceId::new("default", "remote"),
        8080,
        vec![crate::onion::catalog::CatalogBackend {
            execution: None,
            node_id: "other-node".into(),
            node_ip: "192.168.1.2".parse().unwrap(),
            host_port: 30011,
            healthy: true,
        }],
    )])
    .unwrap()
}

/// Send one leader answer, as the placement poll does.
async fn sync_consumer(
    tx: &mpsc::Sender<AgentCommand>,
    generation: u64,
    catalog: crate::onion::catalog::EndpointCatalog,
    ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
) -> oneshot::Receiver<Result<ConsumerUpdate, BunError>> {
    let (response, reply) = oneshot::channel();
    tx.send(AgentCommand::SyncClusterConsumer {
        generation,
        catalog: Box::new(catalog),
        ingress,
        withdrawals: vec![],
        requested_at_ns: crate::onion::lease::boot_clock_ns(),
        response,
    })
    .await
    .unwrap();
    reply
}

/// The 0.1.4 candidate's compressed soak (#505): every slow command turn
/// was `sync_cluster_consumer`, 300 to 700 ms through setup and 1.0 to
/// 1.2 s after a leader kill. A sync that changes the view journals it
/// before installing it, then marks it active, then forgets the views it
/// replaced: three fsync'd writes in one turn, and five when the first
/// answer after a recovery withdraws everything first. At the soak's
/// 400 ms a write, either sync must journal at most one write in any turn,
/// and a status queued behind it must still be answered.
///
/// The verdict counts writes per turn instead of timing the status. A
/// status may wait two turns of one slowed write each, which leaves about
/// 200 ms of the budget for the host's real fsyncs, and on a loaded hosted
/// macOS runner `F_FULLFSYNC` took about 570 ms (#585), the #508 class.
#[tokio::test]
async fn status_answers_while_a_consumer_sync_journals_to_a_slow_disk() {
    let (agent, _root, catalog, ingress) = recovered_consumer().await;
    let lease = agent.view_lease_handle();
    let running = run_restarted(agent);
    running.stalls.set(LoopStall::Persist, SOAK_JOURNAL_WRITE);
    running.measure_from_here();
    for (generation, catalog, view) in [
        (1, catalog, "the first answer after a recovery"),
        (2, moved_remote_backend(), "a changed view"),
    ] {
        let reply = sync_consumer(&running.tx, generation, catalog, ingress.clone()).await;
        let mid_sync = running.status_latency().await;
        assert!(
            mid_sync.is_some(),
            "status went unanswered while {view} journalled to a slow disk"
        );
        let update = tokio::time::timeout(std::time::Duration::from_secs(10), reply)
            .await
            .expect("the sync was never answered")
            .unwrap()
            .unwrap();
        assert!(update.published, "{view} was not published");
        assert!(lease.is_valid(), "{view} did not renew the view lease");
    }
    let after_syncs = running.status_latency().await;
    let most_in_a_turn = running.stalls.most_in_a_turn(LoopStall::Persist);
    let worst = running.meter.worst_turn();
    stop(running).await;

    assert!(
        after_syncs.is_some(),
        "status went unanswered after the consumer syncs"
    );
    assert!(
        most_in_a_turn <= 1,
        "one turn journalled {most_in_a_turn} writes to a slow disk (the worst turn \
         was {worst:?}); a consumer sync step may journal one"
    );
}

/// Counting the writes makes the split independent of the disk: whatever a
/// write costs, no consumer step pays for two. A recovery's first answer
/// journals five (withdrawing, withdrawn, receipts proven, publishing,
/// active) and a changed view three (publishing, active, compacted), in
/// that order, as they did in one turn before (#505).
#[tokio::test]
async fn a_consumer_sync_journals_at_most_one_write_a_turn() {
    let (mut agent, _root, catalog, ingress) = recovered_consumer().await;
    let lease = agent.view_lease_handle();
    for (generation, catalog, total) in [(1, catalog, 5), (2, moved_remote_backend(), 3)] {
        let (response, mut reply) = oneshot::channel();
        let mut writes_per_turn = Vec::new();
        let before = agent.loop_stalls.reached(LoopStall::Persist);
        agent
            .handle_command(AgentCommand::SyncClusterConsumer {
                generation,
                catalog: Box::new(catalog),
                ingress: ingress.clone(),
                withdrawals: vec![],
                requested_at_ns: crate::onion::lease::boot_clock_ns(),
                response,
            })
            .await;
        writes_per_turn.push(agent.loop_stalls.reached(LoopStall::Persist) - before);
        while !agent.consumer_syncs.is_idle() {
            assert!(
                reply.try_recv().is_err(),
                "generation {generation} was answered before its last step"
            );
            let before = agent.loop_stalls.reached(LoopStall::Persist);
            agent.continue_consumer_sync().await;
            writes_per_turn.push(agent.loop_stalls.reached(LoopStall::Persist) - before);
        }
        let update = reply.await.unwrap().unwrap();
        assert!(update.published);
        assert!(lease.is_valid());
        assert_eq!(
            agent.consumer_owner().unwrap().phase,
            crate::bun::consumer_owners::ConsumerPhase::Active
        );
        assert_eq!(
            writes_per_turn.iter().sum::<usize>(),
            total,
            "generation {generation}'s writes per turn: {writes_per_turn:?}"
        );
        assert!(
            writes_per_turn.iter().all(|writes| *writes <= 1),
            "a turn of generation {generation} journalled more than one write: \
             {writes_per_turn:?}"
        );
    }
}

/// A consumer that published a view, then restarted: the new process has
/// recovered its ownership from the journal the old one left, so the view
/// it published is withdrawn and nothing routes until the leader answers.
async fn restarted_consumer() -> (
    BunAgent<MockGrill>,
    tempfile::TempDir,
    crate::onion::catalog::EndpointCatalog,
    Vec<crate::cluster::orchestrate::IngressAssignment>,
) {
    let (mut agent, root, catalog, ingress) = recovered_consumer().await;
    let published = agent
        .synchronise_consumer(1, catalog.clone(), ingress.clone(), vec![])
        .await
        .unwrap();
    assert!(published.published);
    // The old process lets go of the journal's claim as it dies.
    drop(agent);
    let (mut agent, _, _) = test_cluster_fault_agent().await;
    agent.set_records_dir(root.path().to_owned());
    agent
        .supervisor
        .grill()
        .set_launch_inventory(Vec::new())
        .await;
    let identity = crate::bun::consumer_owners::ConsumerIdentity {
        node_id: crate::meat::NodeId::new("test"),
        cluster_identity: [42; 32],
    };
    agent
        .recover_consumer_ownership(&root.path().join("discovery"), identity)
        .await
        .unwrap();
    (agent, root, catalog, ingress)
}

/// The 0.1.5 candidate's compressed soak (#603): a node's first consumer
/// sync after its bun restarted took 1092 ms in its command turn, where
/// that node's earlier syncs took 290 to 800 ms. The command turn built the
/// candidate view, which reads the runtime's whole launch inventory from
/// disk, and then journalled the view's first write: two slow awaits in one
/// turn, on a disk a restart had just left cold.
///
/// Counting both per turn keeps the verdict independent of the disk. The
/// first answer after a restart reads the inventory once and journals five
/// writes (withdrawing, withdrawn, receipts proven, publishing, active),
/// and no turn may take more than one of those six.
#[tokio::test]
async fn the_first_consumer_sync_after_a_restart_takes_one_slow_await_a_turn() {
    let (mut agent, _root, catalog, ingress) = restarted_consumer().await;
    let lease = agent.view_lease_handle();
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::from_millis(50));
    agent
        .supervisor
        .grill()
        .set_inventory_delay(Some(std::time::Duration::from_millis(50)));
    let reached = |agent: &BunAgent<MockGrill>| {
        (
            agent.loop_stalls.reached(LoopStall::Persist),
            agent.loop_stalls.reached(LoopStall::RuntimeInventory),
        )
    };
    let mut per_turn = Vec::new();
    let (response, mut reply) = oneshot::channel();
    let before = reached(&agent);
    agent
        .handle_command(AgentCommand::SyncClusterConsumer {
            generation: 1,
            catalog: Box::new(catalog),
            ingress,
            withdrawals: vec![],
            requested_at_ns: crate::onion::lease::boot_clock_ns(),
            response,
        })
        .await;
    let after = reached(&agent);
    per_turn.push((after.0 - before.0, after.1 - before.1));
    while !agent.consumer_syncs.is_idle() {
        assert!(
            reply.try_recv().is_err(),
            "the sync was answered before its last step"
        );
        let before = reached(&agent);
        agent.continue_consumer_sync().await;
        let after = reached(&agent);
        per_turn.push((after.0 - before.0, after.1 - before.1));
    }
    let update = reply.await.unwrap().unwrap();
    assert!(update.published);
    assert!(lease.is_valid());
    assert_eq!(
        agent.consumer_owner().unwrap().phase,
        crate::bun::consumer_owners::ConsumerPhase::Active
    );
    let writes: usize = per_turn.iter().map(|(writes, _)| writes).sum();
    let reads: usize = per_turn.iter().map(|(_, reads)| reads).sum();
    assert_eq!(
        (writes, reads),
        (5, 1),
        "(writes, inventory reads) per turn: {per_turn:?}"
    );
    assert!(
        per_turn.iter().all(|(writes, reads)| writes + reads <= 1),
        "a turn took more than one slow await; (writes, inventory reads) per turn: \
         {per_turn:?}"
    );
}

/// The CPU time this process has used so far, user and system, on every
/// thread. Unlike the wall clock, it doesn't grow while a busy machine
/// leaves the test waiting for a core, so it measures work.
fn process_cpu_time() -> std::time::Duration {
    // SAFETY: `rusage` is plain old data, for which all-zero bytes are a
    // valid value, and `getrusage` only writes into the struct we pass.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `RUSAGE_SELF` is a valid target and `usage` a valid,
    // exclusively borrowed out-pointer for the duration of the call.
    let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    assert_eq!(status, 0, "getrusage failed");
    let duration = |time: libc::timeval| {
        std::time::Duration::from_secs(time.tv_sec as u64)
            + std::time::Duration::from_micros(time.tv_usec as u64)
    };
    duration(usage.ru_utime) + duration(usage.ru_stime)
}

/// Take a leader answer through the loop's consumer-sync turns, one step a
/// turn as `run_loop` does, and return each turn as the meter saw it.
async fn consumer_sync_turns(
    agent: &mut BunAgent<MockGrill>,
    generation: u64,
    catalog: crate::onion::catalog::EndpointCatalog,
    ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
) -> Vec<crate::bun::loop_meter::TurnRecord> {
    consumer_sync_turns_and_work(agent, generation, catalog, ingress)
        .await
        .into_iter()
        .map(|(turn, _)| turn)
        .collect()
}

/// [`consumer_sync_turns`], with the CPU time the process spent in each
/// turn: the loop's own work and any blocking-pool work it waited on.
async fn consumer_sync_turns_and_work(
    agent: &mut BunAgent<MockGrill>,
    generation: u64,
    catalog: crate::onion::catalog::EndpointCatalog,
    ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
) -> Vec<(crate::bun::loop_meter::TurnRecord, std::time::Duration)> {
    let (response, reply) = oneshot::channel();
    agent
        .handle_command(AgentCommand::SyncClusterConsumer {
            generation,
            catalog: Box::new(catalog),
            ingress,
            withdrawals: vec![],
            requested_at_ns: crate::onion::lease::boot_clock_ns(),
            response,
        })
        .await;
    let mut turns = Vec::new();
    while !agent.consumer_syncs.is_idle() {
        let cpu = process_cpu_time();
        let turn = agent.begin_consumer_sync_turn();
        agent.continue_consumer_sync().await;
        agent.consumer_syncs.end_turn();
        agent.turn_deadline = None;
        agent.loop_meter.finish(turn);
        let work = process_cpu_time().saturating_sub(cpu);
        turns.push((agent.loop_meter.last_turn().unwrap(), work));
    }
    let update = reply.await.unwrap().unwrap();
    assert!(
        update.published,
        "generation {generation} was not published"
    );
    turns
}

/// The 0.2.0 candidate's fast soak (#890): two nodes each took a
/// `consumer_sync` turn of about 1.9 s in the same second, and the log said
/// only `follow_up (consumer_sync)`. Seven steps share that name, and the
/// one that installs a view takes a journal write, the kernel's lock for
/// every service, and a firewall sync with runtime reads. Each turn now
/// names its step, and the meter keeps what its slow awaits took, so the
/// next slow turn's log line says where the time went.
#[tokio::test]
async fn each_consumer_sync_turn_names_its_step_and_times_its_slow_awaits() {
    let (mut agent, _root, catalog, ingress) = recovered_consumer().await;
    let write = std::time::Duration::from_millis(50);
    agent.loop_stalls.set(LoopStall::Persist, write);
    agent
        .supervisor
        .grill()
        .set_inventory_delay(Some(std::time::Duration::from_millis(50)));
    let turns = consumer_sync_turns(&mut agent, 1, catalog, ingress).await;
    let steps: Vec<_> = turns.iter().map(|turn| turn.detail).collect();
    assert_eq!(
        steps,
        [
            "consumer_sync/candidate",
            "consumer_sync/journal",
            "consumer_sync/withdraw",
            "consumer_sync/prove_receipts",
            "consumer_sync/publish",
            "consumer_sync/activate",
            "consumer_sync/compact",
        ]
        .map(Some),
        "turns: {turns:?}"
    );
    for turn in &turns {
        assert_eq!(turn.branch, LoopBranch::FollowUp);
        let writes = turn.detail != Some("consumer_sync/candidate")
            && turn.detail != Some("consumer_sync/compact");
        assert_eq!(
            turn.part(TurnPart::JournalWrite)
                .is_some_and(|took| took >= write),
            writes,
            "{turn}'s journal write: {:?}",
            turn.part(TurnPart::JournalWrite)
        );
    }
    let candidate = &turns[0];
    assert!(
        candidate
            .part(TurnPart::RuntimeInventory)
            .is_some_and(|took| took >= std::time::Duration::from_millis(50)),
        "{candidate}'s inventory read: {:?}",
        candidate.part(TurnPart::RuntimeInventory)
    );
    let publish = &turns[4];
    assert!(publish.part(TurnPart::ViewInstall).is_some(), "{publish}");
    assert!(publish.part(TurnPart::FirewallSync).is_some(), "{publish}");
}

/// A catalogue of `services` services, three remote backends each, on
/// host ports from `port_base`.
fn large_catalogue(services: usize, port_base: u16) -> crate::onion::catalog::EndpointCatalog {
    crate::onion::catalog::EndpointCatalog::rebuild((0..services).map(|index| {
        (
            crate::onion::service_id::ServiceId::new("default", format!("svc{index}")),
            8080,
            (0..3u8)
                .map(|replica| crate::onion::catalog::CatalogBackend {
                    execution: None,
                    node_id: format!("node-{replica}"),
                    node_ip: std::net::Ipv4Addr::new(192, 168, 1, 2 + replica),
                    host_port: port_base + (index % 1000) as u16,
                    healthy: true,
                })
                .collect::<Vec<_>>(),
        )
    }))
    .unwrap()
}

/// One lead for #890: a node's services rejoining change the view every
/// node consumes at once. Building, journalling and installing a view does
/// scale with its size, so a thousand services (the soak runs a handful),
/// first published after a recovery and then with every backend moved,
/// must leave each turn well inside the budget. The disk's share is the
/// slow-disk scenarios' business; this one measures what the view's size
/// costs the loop.
///
/// It counts work, not wall clock (#1140): the CPU time the process spent
/// in each turn, which a machine too busy to give the test a core doesn't
/// inflate. Timing the turn beside its journal write would have failed 33
/// of 120 runs at load average 285 to 310, with turns of up to 673 ms,
/// while no turn used more than 116 ms of CPU.
#[tokio::test]
async fn a_large_consumer_view_change_takes_each_turn_well_within_the_budget() {
    let (mut agent, _root, _catalog, _ingress) = recovered_consumer().await;
    for (generation, catalog) in [
        (1, large_catalogue(1000, 30000)),
        (2, large_catalogue(1000, 31000)),
    ] {
        for (turn, work) in
            consumer_sync_turns_and_work(&mut agent, generation, catalog, vec![]).await
        {
            assert!(
                work < TURN_BUDGET / 2,
                "generation {generation}: {turn} used {work:?} of CPU"
            );
        }
    }
}

/// Between a recovery's withdrawal and its publication the journal says
/// Withdrawn, and other turns run in between. The view about to be
/// published was built before, so the local addresses it names stay held.
#[tokio::test]
async fn a_local_address_stays_held_between_a_withdrawal_and_its_publication() {
    let (mut agent, _root, _catalog, _ingress) = recovered_consumer().await;
    let local = InstanceId("default__web-0".into());
    let backend = agent.local_backend(
        &local,
        &crate::onion::service_id::ServiceId::new("default", "web"),
        Some("10.0.2.2".parse().unwrap()),
        30002,
        true,
    );
    let vip = crate::onion::vip::VirtualIP(std::net::Ipv4Addr::new(127, 128, 0, 1));
    let publication = crate::bun::consumer_owners::ConsumerPublication {
        generation: 1,
        catalog: Default::default(),
        effective_services: vec![crate::onion::types::ServiceEntry {
            app_name: "web".into(),
            namespace: "default".into(),
            namespace_id: crate::onion::vip::name_to_id("default"),
            app_id: u32::from(vip.0),
            vip,
            port: 8080,
            backends: vec![backend],
            firewall_allow_from: None,
        }],
        ingress: vec![],
    };
    assert!(
        !agent.own_view_names(&local),
        "a withdrawn view names nothing"
    );
    agent.consumer_syncs.hold_for_test(publication);
    assert!(agent.own_view_names(&local));
}

/// How long one discovery-journal write took after a deploy-kill restarted
/// a node's Bun in the 0.2.0 candidate 7 soak (#890): 1,574 ms of a
/// 1,575 ms `consumer_sync/journal` turn. A throttled disk took up to 6 s.
const STALLED_JOURNAL_WRITE: std::time::Duration = std::time::Duration::from_millis(1600);

/// The host port the installed view sends `default/remote` to, if any.
fn installed_remote_port(agent: &BunAgent<MockGrill>) -> Option<u16> {
    agent
        .service_map_tx
        .borrow()
        .resolve(&crate::onion::service_id::ServiceId::new(
            "default", "remote",
        ))
        .and_then(|entry| entry.backends.first().map(|backend| backend.host_port))
}

/// A consumer that published the fixture's view, and the leader's next
/// answer for it: the same service with its backend moved.
async fn published_consumer() -> (
    BunAgent<MockGrill>,
    tempfile::TempDir,
    Vec<crate::cluster::orchestrate::IngressAssignment>,
) {
    let (mut agent, root, catalog, ingress) = recovered_consumer().await;
    let published = agent
        .synchronise_consumer(1, catalog, ingress.clone(), vec![])
        .await
        .unwrap();
    assert!(published.published);
    assert_eq!(installed_remote_port(&agent), Some(30001));
    (agent, root, ingress)
}

/// Take one consumer-sync turn as `run_loop` does, and return it as the
/// meter saw it.
async fn one_consumer_sync_turn(
    agent: &mut BunAgent<MockGrill>,
) -> crate::bun::loop_meter::TurnRecord {
    let turn = agent.begin_consumer_sync_turn();
    agent.continue_consumer_sync().await;
    agent.consumer_syncs.end_turn();
    agent.turn_deadline = None;
    agent.loop_meter.finish(turn);
    agent.loop_meter.last_turn().unwrap()
}

/// The 0.2.0 soaks (#890): after a restart, one discovery-journal write
/// took 1.6 s, and the consumer step that waited for it held the loop for
/// all of it. A step now waits for its write only until the turn's runtime
/// budget runs out. The write goes on off the loop, the turn ends, and a
/// status queued meanwhile is answered; once the write lands, the step's
/// next part runs. Each of the changed view's three writes is this slow.
#[tokio::test]
async fn status_answers_while_a_consumer_journal_write_outlasts_the_turn() {
    let (agent, _root, ingress) = published_consumer().await;
    let lease = agent.view_lease_handle();
    let running = run_restarted(agent);
    running
        .stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    running.measure_from_here();
    let reply = sync_consumer(&running.tx, 2, moved_remote_backend(), ingress).await;
    // Let the journal step start its write before the status queues.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mid_write = running.status_latency().await;
    let update = tokio::time::timeout(std::time::Duration::from_secs(20), reply)
        .await
        .expect("the sync was never answered")
        .unwrap()
        .unwrap();
    assert!(update.published, "the changed view was not published");
    assert!(lease.is_valid(), "the changed view did not renew the lease");
    running
        .stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    assert!(
        mid_write.is_some_and(|latency| latency < TURN_BUDGET),
        "status took {mid_write:?} while a journal write took {STALLED_JOURNAL_WRITE:?}"
    );
    running
        .assert_responsive("each of a consumer sync's journal writes took 1.6 s")
        .await;
}

/// Journal before kernel: a view reaches the kernel, DNS and Wrapper only
/// after the write that records it has landed, however long that write
/// takes and however many turns run meanwhile. The turn that starts the
/// write ends within the budget with the old view still installed; the
/// turn after the write lands installs the new one.
#[tokio::test]
async fn a_consumer_view_is_installed_only_after_its_journal_write_lands() {
    let (mut agent, _root, ingress) = published_consumer().await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let (response, mut reply) = oneshot::channel();
    agent
        .request_consumer_sync(crate::bun::agent::consumer::ConsumerRequest {
            generation: 2,
            catalog: moved_remote_backend(),
            ingress,
            withdrawals: vec![],
            answer: Some(crate::bun::agent::consumer::ConsumerAnswer {
                requested_at_ns: crate::onion::lease::boot_clock_ns(),
                response,
            }),
        })
        .await;
    let candidate = one_consumer_sync_turn(&mut agent).await;
    assert_eq!(candidate.detail, Some("consumer_sync/candidate"));
    let journal = one_consumer_sync_turn(&mut agent).await;
    assert_eq!(journal.detail, Some("consumer_sync/journal"));
    assert!(journal.took < TURN_BUDGET, "{journal}");
    assert!(
        agent.discovery_write_in_flight(),
        "the write was waited for: {journal}"
    );
    assert_eq!(
        installed_remote_port(&agent),
        Some(30001),
        "the view was installed before its journal write landed"
    );
    assert!(reply.try_recv().is_err(), "answered before its last step");

    agent.settle_discovery_write().await;
    assert!(!agent.discovery_write_in_flight());
    assert_eq!(
        installed_remote_port(&agent),
        Some(30001),
        "landing the write installed the view by itself"
    );
    let install = one_consumer_sync_turn(&mut agent).await;
    assert_eq!(install.detail, Some("consumer_sync/install"));
    assert_eq!(installed_remote_port(&agent), Some(30011));
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    while !agent.consumer_syncs.is_idle() {
        one_consumer_sync_turn(&mut agent).await;
    }
    assert!(reply.await.unwrap().unwrap().published);
    assert_eq!(
        agent.consumer_owner().unwrap().phase,
        crate::bun::consumer_owners::ConsumerPhase::Active
    );
}

/// A journal write that fails after the turn moved on fences discovery as
/// a failed inline write does: ownership becomes uncertain, readiness says
/// so, the view it would have recorded never reaches the kernel, and the
/// leader hears the failure.
#[tokio::test]
async fn a_journal_write_that_fails_off_the_turn_fences_discovery_and_installs_nothing() {
    let (mut agent, _root, ingress) = published_consumer().await;
    let readiness = crate::bun::readiness::ReadinessTracker::new();
    agent.set_readiness_tracker(readiness.clone());
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let (response, reply) = oneshot::channel();
    agent
        .request_consumer_sync(crate::bun::agent::consumer::ConsumerRequest {
            generation: 2,
            catalog: moved_remote_backend(),
            ingress,
            withdrawals: vec![],
            answer: Some(crate::bun::agent::consumer::ConsumerAnswer {
                requested_at_ns: crate::onion::lease::boot_clock_ns(),
                response,
            }),
        })
        .await;
    one_consumer_sync_turn(&mut agent).await;
    let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
        &mut agent.discovery_ownership
    else {
        panic!("discovery ownership is not ready before the journal step");
    };
    journal.fail_next_write();
    one_consumer_sync_turn(&mut agent).await;
    assert!(agent.discovery_write_in_flight());

    agent.settle_discovery_write().await;
    assert!(matches!(
        agent.discovery_ownership,
        DiscoveryOwnership::Uncertain
    ));
    let degraded = readiness
        .snapshot()
        .await
        .subsystems
        .into_iter()
        .find(|subsystem| subsystem.name == "discovery:journal")
        .map(|subsystem| subsystem.state);
    assert_eq!(
        degraded,
        Some(crate::bun::readiness::SubsystemState::Degraded)
    );
    while !agent.consumer_syncs.is_idle() {
        one_consumer_sync_turn(&mut agent).await;
    }
    assert!(
        reply.await.unwrap().is_err(),
        "a failed write was published"
    );
    assert_eq!(installed_remote_port(&agent), Some(30001));
}

// ---- receipt confirmations (#1340) --------------------------------------------

/// The leader's withdrawal of the fixture's first view, sent with a later
/// answer.
fn withdrawal_of_the_first_view() -> crate::onion::withdrawal::EndpointWithdrawalInstruction {
    let (catalog, _) = cluster_publication_fixture();
    crate::onion::withdrawal::EndpointWithdrawalInstruction {
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
    }
}

/// A consumer whose second answer withdrew its first view: receipt 1 is
/// proven locally and waits for the leader's acknowledgement.
async fn consumer_with_a_ready_receipt() -> (BunAgent<MockGrill>, tempfile::TempDir) {
    let (mut agent, root, _ingress) = published_consumer().await;
    let update = agent
        .synchronise_consumer(
            2,
            Default::default(),
            vec![],
            vec![withdrawal_of_the_first_view()],
        )
        .await
        .unwrap();
    assert_eq!(update.receipts, vec![1]);
    (agent, root)
}

/// The receipts the discovery journal itself records: not the inventory
/// before a write in flight.
fn journalled_receipts(agent: &BunAgent<MockGrill>) -> Vec<u64> {
    let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
        &agent.discovery_ownership
    else {
        panic!("discovery ownership is not settled");
    };
    receipts_of(journal)
}

/// The receipts a discovery journal records.
fn receipts_of(journal: &crate::bun::discovery_owners::DiscoveryJournal) -> Vec<u64> {
    journal
        .inventory()
        .consumer
        .as_ref()
        .unwrap()
        .receipts
        .keys()
        .copied()
        .collect()
}

/// The receipts on disk, once the agent that owned the journal is gone.
fn receipts_on_disk(root: &tempfile::TempDir) -> Vec<u64> {
    let journal =
        crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
            .unwrap();
    receipts_of(&journal)
}

/// Deliver the leader's acknowledgement of receipt `generation` in a
/// command turn of its own, as the loop does. Returns the turn the meter
/// saw and the reply still to come.
async fn receipt_turn(
    agent: &mut BunAgent<MockGrill>,
    generation: u64,
) -> (
    crate::bun::loop_meter::TurnRecord,
    oneshot::Receiver<Result<(), BunError>>,
) {
    let (response, reply) = oneshot::channel();
    let turn = begin_scenario_turn(agent, LoopBranch::Command, Some("confirm_consumer_receipt"));
    agent
        .handle_command(AgentCommand::ConfirmConsumerReceipt {
            generation,
            response,
        })
        .await;
    (end_scenario_turn(agent, turn), reply)
}

/// Take the loop's turn for confirmations that waited for the journal.
async fn waiting_receipts_turn(
    agent: &mut BunAgent<MockGrill>,
) -> crate::bun::loop_meter::TurnRecord {
    assert!(
        agent.receipt_confirmations.is_waiting(),
        "no confirmation waits for the journal"
    );
    let turn = agent.begin_receipt_turn();
    agent.confirm_waiting_receipts().await;
    end_scenario_turn(agent, turn)
}

/// 0.2.2's gate met this four times during host disk stalls (#1340): the
/// leader acknowledged a receipt, and forgetting it journalled inline, a
/// 1.4 to 3.6 s `command (confirm_consumer_receipt)` turn whose only part
/// was its journal write. The write now waits only until the turn's
/// deadline. A status queued meanwhile is answered, and the confirmation
/// is answered once the write has landed.
#[tokio::test]
async fn status_answers_while_a_receipt_confirmation_journals_to_a_stalled_disk() {
    let (agent, root) = consumer_with_a_ready_receipt().await;
    let running = run_restarted(agent);
    running
        .stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    running.measure_from_here();
    let reached = running.stalls.reached(LoopStall::Persist);
    let (response, mut reply) = oneshot::channel();
    running
        .tx
        .send(AgentCommand::ConfirmConsumerReceipt {
            generation: 1,
            response,
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while running.stalls.reached(LoopStall::Persist) == reached {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the confirmation never journalled");
    let mid_write = running.status_latency().await;
    let early = reply.try_recv();
    let answered_early = early.is_ok();
    let confirmed = match early {
        Ok(confirmed) => confirmed,
        Err(_) => tokio::time::timeout(std::time::Duration::from_secs(10), reply)
            .await
            .expect("the confirmation was never answered")
            .unwrap(),
    };
    running
        .stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let worst = running.meter.worst_turn();
    stop(running).await;

    assert!(
        mid_write.is_some_and(|latency| latency < TURN_BUDGET),
        "status took {mid_write:?} while a receipt confirmation's journal write took \
         {STALLED_JOURNAL_WRITE:?}"
    );
    assert!(
        !answered_early,
        "the confirmation was answered before its write landed"
    );
    assert!(confirmed.is_ok(), "{confirmed:?}");
    if let Some(worst) = worst {
        assert!(worst.took < TURN_BUDGET, "the worst loop turn took {worst}");
    }
    assert!(receipts_on_disk(&root).is_empty());
}

/// The confirmation's own write waits only until the turn's deadline, and
/// the confirmation is answered from the turn that lands it. Until then the
/// receipt is still on disk, so the leader keeps being told about it; a
/// second acknowledgement meanwhile writes nothing and waits for nothing.
#[tokio::test]
async fn a_receipt_confirmation_leaves_its_slow_journal_write_in_flight() {
    let (mut agent, _root) = consumer_with_a_ready_receipt().await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, mut reply) = receipt_turn(&mut agent, 1).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "the confirmation didn't journal: {turn}"
    );
    assert!(turn.took < TURN_BUDGET, "{turn}");
    assert!(
        agent.discovery_write_in_flight(),
        "the confirmation's turn waited for its journal write: {turn}"
    );
    assert!(
        reply.try_recv().is_err(),
        "answered before the write landed"
    );
    assert_eq!(
        agent.consumer_update(true).receipts,
        vec![1],
        "a receipt still on disk stopped being reported"
    );

    let (again_turn, mut again) = receipt_turn(&mut agent, 1).await;
    assert!(again_turn.took < TURN_BUDGET, "{again_turn}");
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached + 1);
    assert!(again.try_recv().is_err());

    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.detail, Some("consumer_receipt/landed"), "{landed}");
    assert!(reply.await.unwrap().is_ok());
    assert!(journalled_receipts(&agent).is_empty());
    assert!(agent.consumer_update(true).receipts.is_empty());

    // The second acknowledgement finds the receipt already forgotten.
    waiting_receipts_turn(&mut agent).await;
    assert!(again.await.unwrap().is_ok());
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached + 1);
}

/// A confirmation changes the ownership as it is on disk, so it waits for
/// any write already in flight, but off the loop: the command's turn ends
/// at once, and the loop takes the confirmation in a turn of its own once
/// the journal is free.
#[tokio::test]
async fn a_receipt_confirmation_waits_off_the_loop_for_a_write_already_in_flight() {
    let (mut agent, _root) = consumer_with_a_ready_receipt().await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    // Another writer's write, left in flight at its turn's deadline.
    let landing = agent
        .write_discovery_inventory(
            &crate::onion::service_id::ServiceId::new("system", "discovery"),
            |_| {},
            Some(tokio::time::Instant::now()),
            super::discovery_ownership::WriteOwner::AskAgain {
                branch: LoopBranch::FollowUp,
                detail: "another_writer/landed",
            },
        )
        .await
        .unwrap();
    assert_eq!(landing, super::discovery_ownership::Landing::InFlight);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, mut reply) = receipt_turn(&mut agent, 1).await;
    assert!(turn.took < TURN_BUDGET, "{turn}");
    assert!(
        agent.discovery_write_in_flight(),
        "the confirmation's turn waited for another writer's write: {turn}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert!(reply.try_recv().is_err());

    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.detail, Some("another_writer/landed"), "{landed}");
    assert!(
        reply.try_recv().is_err(),
        "answered before forgetting the receipt was journalled"
    );
    let confirmed = waiting_receipts_turn(&mut agent).await;
    assert_eq!(
        confirmed.detail,
        Some("consumer_receipt/confirm"),
        "{confirmed}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached + 1);
    assert!(reply.await.unwrap().is_ok());
    assert!(journalled_receipts(&agent).is_empty());
}

/// Only a locally proven receipt is forgotten. One whose withdrawal the
/// published view still contradicts is refused at once, and nothing is
/// journalled.
#[tokio::test]
async fn a_receipt_without_local_withdrawal_proof_is_refused() {
    let (mut agent, _root, ingress) = published_consumer().await;
    let (catalog, _) = cluster_publication_fixture();
    let update = agent
        .synchronise_consumer(2, catalog, ingress, vec![withdrawal_of_the_first_view()])
        .await
        .unwrap();
    assert!(update.receipts.is_empty(), "{update:?}");
    assert_eq!(journalled_receipts(&agent), vec![1]);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, mut reply) = receipt_turn(&mut agent, 1).await;
    assert!(
        matches!(reply.try_recv(), Ok(Err(_))),
        "an unproven receipt was not refused"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(journalled_receipts(&agent), vec![1]);
}

/// A confirmation whose write fails after its turn moved on fences
/// discovery, answers the failure, and leaves the receipt on disk, as a
/// crash before the write lands does: the leader is asked to acknowledge
/// it again, and the next confirmation forgets it.
#[tokio::test]
async fn a_receipt_confirmation_whose_write_fails_off_the_turn_keeps_the_receipt() {
    let (mut agent, root) = consumer_with_a_ready_receipt().await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
        &mut agent.discovery_ownership
    else {
        panic!("discovery ownership is not ready");
    };
    journal.fail_next_write();
    let (turn, reply) = receipt_turn(&mut agent, 1).await;
    assert!(agent.discovery_write_in_flight(), "{turn}");
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    landing_turn(&mut agent).await;
    assert!(
        reply.await.unwrap().is_err(),
        "a failed write was confirmed"
    );
    assert!(matches!(
        agent.discovery_ownership,
        DiscoveryOwnership::Uncertain
    ));
    drop(agent);
    assert_eq!(receipts_on_disk(&root), vec![1]);
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
        instance: None,
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

// ---- startup recovery -------------------------------------------------------

/// An agent that has just restarted after a whole-cluster stop. No runtime
/// survived, so adoption deferred the one record's cleanup as a startup
/// retirement, which the first health tick drives: it reads the runtime
/// inventory and asks the leader to release the instance's address.
/// Returns the running agent, the grill, the retired host port, and the
/// leader's confirmation for [`crate::cluster::producer::test_delayed_fixture`].
async fn restarted_with_a_startup_retirement() -> (
    BunAgent<MockGrill>,
    MockGrill,
    tempfile::TempDir,
    u16,
    String,
) {
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
    assert!(agent.startup_cleanup_pending);
    let launch = grill.launch_inventory().await.unwrap().unwrap().remove(0);
    let host_port = launch.spec.port_mapping.unwrap().host_port;
    let confirmation = serde_json::json!({
        "node_id": "test",
        "execution": {"instance_id": reference.instance_id, "generation": launch.generation},
    })
    .to_string();
    (agent, grill, root, host_port, confirmation)
}

/// Start `agent`'s loop with a command channel a scenario can use.
fn run_restarted(mut agent: BunAgent<MockGrill>) -> RunningAgent {
    let (tx, rx) = mpsc::channel(32);
    agent.command_rx = rx;
    let shutdown = agent.shutdown.clone();
    RunningAgent::start(boxed(agent), tx, shutdown)
}

/// Wait until the startup retirement has released its host port.
async fn wait_for_release(allocator: &PortAllocator, host_port: u16, within: std::time::Duration) {
    tokio::time::timeout(within, async {
        while allocator.is_allocated(host_port).await {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the startup retirement never released its port");
}

/// The readiness subsystem a startup retirement reports through.
const STARTUP_CLEANUP: &str = "discovery:startup-cleanup";

/// Wait until every startup retirement has finished, the discovery
/// recovery after them included.
async fn wait_for_startup_cleanup(
    readiness: &crate::bun::readiness::ReadinessTracker,
    within: std::time::Duration,
) {
    tokio::time::timeout(within, async {
        loop {
            let ready = readiness
                .snapshot()
                .await
                .subsystems
                .iter()
                .any(|subsystem| {
                    subsystem.name == STARTUP_CLEANUP
                        && subsystem.state == crate::bun::readiness::SubsystemState::Ready
                });
            if ready {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the startup retirement never finished");
}

/// How far past the turn's runtime budget a turn that gave up at that
/// deadline may end: the bookkeeping after the wait, with room for a loaded
/// coverage runner. Well inside the 1 s [`TURN_BUDGET`], so a turn that
/// waited out the slow work, or a fixed one-second timeout, still fails.
const DEADLINE_MARGIN: std::time::Duration = std::time::Duration::from_millis(250);

/// Long enough for two health ticks to meet the slow work.
const TWO_TICKS: std::time::Duration = std::time::Duration::from_millis(1600);

/// While a startup retirement waits on something slow, every turn ends at
/// the turn's deadline (within [`DEADLINE_MARGIN`]), a queued status is
/// answered within the budget, and the retirement still holds its port.
///
/// The turn that later completes the retirement isn't judged here. It
/// journals the release with four fsync'd persists, which on a real disk
/// under a loaded coverage build took a second between them (0.1.3 release
/// candidate 2). The slow-disk scenario below bounds that turn instead.
async fn assert_turns_end_at_the_deadline(
    running: &RunningAgent,
    allocator: &PortAllocator,
    host_port: u16,
    slow_work: &str,
) {
    let worst = running.meter.worst_turn().expect("no turn ran");
    let bound = TURN_RUNTIME_BUDGET + DEADLINE_MARGIN;
    assert!(
        worst.took < bound,
        "the worst loop turn took {worst} while {slow_work}; a turn that gives up at \
         the turn's deadline ends within {bound:?}"
    );
    let latency = running.status_latency().await;
    assert!(
        latency.is_some_and(|latency| latency < TURN_BUDGET),
        "status took {latency:?} while {slow_work}"
    );
    assert!(
        allocator.is_allocated(host_port).await,
        "the port was released before the slow work finished"
    );
}

/// Stop a running agent without judging its turns.
async fn stop(running: RunningAgent) {
    running.shutdown.cancel();
    let mut task = running.task;
    if tokio::time::timeout(std::time::Duration::from_secs(2), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
}

/// The 0.1.3 final tier (#418): after a graceful whole-cluster stop, the
/// first node back ran its first health tick for 1008 ms. Its startup
/// retirement asked the remembered leader, still booting, to release an
/// address, and waited the producer release's whole second for an answer.
/// The tick now waits only until the turn's deadline; the request carries
/// on in its task, and a later tick collects the leader's answer.
#[tokio::test]
async fn the_tick_waits_out_a_startup_retirement_whose_leader_answers_late() {
    let (mut agent, _grill, _root, host_port, confirmation) =
        restarted_with_a_startup_retirement().await;
    let allocator = agent.supervisor.port_allocator.clone();
    let (client, server) = crate::cluster::producer::test_delayed_fixture(
        axum::http::StatusCode::OK,
        confirmation,
        STALL,
    )
    .await;
    agent.set_producer_release_client(client);
    let running = run_restarted(agent);
    running.measure_from_here();
    tokio::time::sleep(TWO_TICKS).await;
    assert_turns_end_at_the_deadline(
        &running,
        &allocator,
        host_port,
        "a startup retirement waited for a leader that answers late",
    )
    .await;
    // Retirement isn't abandoned: once the leader answers, a tick finishes it.
    wait_for_release(&allocator, host_port, STALL * 3).await;
    stop(running).await;
    server.abort();
}

/// The same retirement reads the runtime inventory first, which runc
/// answers from disk: on a cold boot, slowly. The tick waits only until the
/// turn's deadline, and retries on a later tick.
#[tokio::test]
async fn the_tick_waits_out_a_startup_retirement_whose_inventory_is_slow() {
    let (mut agent, grill, _root, host_port, confirmation) =
        restarted_with_a_startup_retirement().await;
    let allocator = agent.supervisor.port_allocator.clone();
    let (client, server) =
        crate::cluster::producer::test_fixture(axum::http::StatusCode::OK, confirmation).await;
    agent.set_producer_release_client(client);
    // Far past any budget: only a turn that gives up at its deadline passes.
    grill.set_inventory_delay(Some(std::time::Duration::from_secs(30)));
    let running = run_restarted(agent);
    running.measure_from_here();
    tokio::time::sleep(TWO_TICKS).await;
    assert_turns_end_at_the_deadline(
        &running,
        &allocator,
        host_port,
        "a startup retirement read a slow runtime inventory",
    )
    .await;
    grill.set_inventory_delay(None);
    wait_for_release(&allocator, host_port, STALL * 2).await;
    stop(running).await;
    server.abort();
}

/// The four journal writes that complete a startup retirement never share a
/// tick (#422). Whether they did used to depend on the disk: writes that
/// left time before the turn's deadline let the artifact cleanup finish in
/// the same turn, and then the discovery recovery's two writes followed it,
/// so a write just under 250 ms put the turn near two runtime budgets.
/// Counting the writes per tick makes that independent of the disk.
#[tokio::test]
async fn a_startup_retirement_journals_at_most_two_writes_a_tick() {
    let writes_per_tick = startup_retirement_writes_per_tick(std::time::Duration::ZERO).await;
    assert_at_most_two_writes_a_tick(&writes_per_tick);
}

/// The same promise when the leader answers the producer release only after
/// the first turn's deadline, as a loaded machine makes even a local leader
/// do (#761). The ticks that follow find the request still in flight and
/// return at once; the retirement finishes once a later tick collects the
/// answer, and still journals at most two writes a tick.
#[tokio::test]
async fn a_startup_retirement_whose_leader_answers_after_the_turn_journals_at_most_two_writes_a_tick()
 {
    let writes_per_tick =
        startup_retirement_writes_per_tick(TURN_RUNTIME_BUDGET + TURN_RUNTIME_BUDGET / 2).await;
    assert_at_most_two_writes_a_tick(&writes_per_tick);
}

/// The gap a scenario leaves between the ticks it drives by hand. The loop
/// spaces its health ticks [`HEALTH_TICK_INTERVAL`] apart, and in-flight
/// work (a leader's answer, a disk cleanup) runs in the gap. Back-to-back
/// ticks would leave it none: ten of them could pass while the leader's
/// answer was still on its way (#761).
const BETWEEN_TICKS: std::time::Duration = std::time::Duration::from_millis(100);

/// How long a scenario that drives ticks by hand waits for the work to
/// finish. Far past any delay a scenario sets, so only work that never
/// finishes runs out of it.
const TICKS_PATIENCE: std::time::Duration = std::time::Duration::from_secs(20);

/// Drive a startup retirement tick by tick, with the leader answering its
/// producer release after `leader_delay`, and count each tick's journal
/// writes. Ticks are spaced like the loop's, and counted by tick rather than
/// by time, so a loaded machine changes how many ticks it takes but not
/// what each tick journals.
async fn startup_retirement_writes_per_tick(leader_delay: std::time::Duration) -> Vec<usize> {
    let (mut agent, _grill, _root, _host_port, confirmation) =
        restarted_with_a_startup_retirement().await;
    let (client, server) = crate::cluster::producer::test_delayed_fixture(
        axum::http::StatusCode::OK,
        confirmation,
        leader_delay,
    )
    .await;
    agent.set_producer_release_client(client);
    let mut writes_per_tick = Vec::new();
    let patience = tokio::time::Instant::now() + TICKS_PATIENCE;
    while agent.startup_cleanup_pending && tokio::time::Instant::now() < patience {
        if !writes_per_tick.is_empty() {
            tokio::time::sleep(BETWEEN_TICKS).await;
        }
        let before = agent.loop_stalls.reached(LoopStall::Persist);
        agent.turn_deadline = Some(tokio::time::Instant::now() + TURN_RUNTIME_BUDGET);
        agent.drive_startup_retirements().await;
        agent.turn_deadline = None;
        writes_per_tick.push(agent.loop_stalls.reached(LoopStall::Persist) - before);
    }
    server.abort();
    assert!(
        !agent.startup_cleanup_pending,
        "the startup retirement never finished: {writes_per_tick:?}"
    );
    writes_per_tick
}

/// A finished startup retirement journalled its four writes, at most two in
/// any one tick.
fn assert_at_most_two_writes_a_tick(writes_per_tick: &[usize]) {
    assert_eq!(
        writes_per_tick.iter().sum::<usize>(),
        4,
        "journal writes per tick: {writes_per_tick:?}"
    );
    assert!(
        writes_per_tick.iter().all(|writes| *writes <= 2),
        "a tick journalled more than two writes: {writes_per_tick:?}"
    );
}

/// Completing a startup retirement journals it: the network reference's
/// release, then the discovery service's retirement, four fsync'd persists.
/// Persists may stay inline (#351, decision 2) only while a slow disk keeps
/// every turn in budget. In one turn the four came to about 663 ms at
/// 150 ms a write, and would pass the budget at the 250 ms a write a loaded
/// coverage runner measured (#421), so the release and the service's
/// retirement now journal on different ticks (#422).
///
/// The verdict counts writes per turn instead of timing the turn. Two
/// slowed writes take 500 ms, which leaves the host's real fsyncs the other
/// half of the budget, and on a loaded hosted macOS runner they took more:
/// a turn of exactly two writes ran 1032 ms (#939), the #508 class.
#[tokio::test]
async fn a_startup_retirement_completes_within_the_budget_on_a_slow_disk() {
    let (mut agent, _grill, _root, host_port, confirmation) =
        restarted_with_a_startup_retirement().await;
    let allocator = agent.supervisor.port_allocator.clone();
    let readiness = crate::bun::readiness::ReadinessTracker::new();
    readiness.register(STARTUP_CLEANUP, true).await;
    agent.set_readiness_tracker(readiness.clone());
    let (client, server) =
        crate::cluster::producer::test_fixture(axum::http::StatusCode::OK, confirmation).await;
    agent.set_producer_release_client(client);
    let running = run_restarted(agent);
    running
        .stalls
        .set(LoopStall::Persist, std::time::Duration::from_millis(250));
    running.measure_from_here();
    wait_for_release(&allocator, host_port, std::time::Duration::from_secs(10)).await;
    // The port goes back before the retirement's last journal writes, so
    // judge only once the whole retirement has finished.
    wait_for_startup_cleanup(&readiness, std::time::Duration::from_secs(10)).await;
    let status = running.status_latency().await;
    let most_in_a_turn = running.stalls.most_in_a_turn(LoopStall::Persist);
    let worst = running
        .meter
        .worst_turn()
        .map_or_else(|| "none".to_string(), |turn| turn.to_string());
    stop(running).await;
    server.abort();

    assert!(
        status.is_some(),
        "status went unanswered after a startup retirement journalled to a slow disk"
    );
    assert!(
        most_in_a_turn <= 2,
        "one turn journalled {most_in_a_turn} writes to a slow disk (the worst turn \
         took {worst}); a startup retirement may journal two"
    );
}

/// A deployed `default/web` that journals discovery and turns unhealthy on
/// one failed probe.
async fn web_journalling_discovery() -> (
    TestAgent,
    tempfile::TempDir,
    InstanceId,
    crate::onion::service_id::ServiceId,
) {
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
    let id = InstanceId("default__web-0".into());
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let mut health = crate::bun::health::HealthCheckConfig::from_spec(
        config_with_health().app["web"].health.as_ref().unwrap(),
        8080,
    );
    health.threshold_unhealthy = 1;
    health.threshold_healthy = 1;
    agent
        .supervisor
        .get_instance_mut(&id)
        .unwrap()
        .health_config = Some(health);
    assert!(agent.service_map.resolve(&service).unwrap().backends[0].healthy);
    (agent, root, id, service)
}

/// [`web_journalling_discovery`] with a no-op journal write left in flight
/// off the loop, as a consumer step leaves one when its write outlasts the
/// turn (#890).
async fn web_with_a_discovery_write_in_flight() -> (
    TestAgent,
    tempfile::TempDir,
    InstanceId,
    crate::onion::service_id::ServiceId,
) {
    let (mut agent, root, id, service) = web_journalling_discovery().await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let landing = agent
        .write_discovery_inventory(
            &service,
            |_| {},
            Some(tokio::time::Instant::now()),
            crate::bun::agent::discovery_ownership::WriteOwner::ConsumerStep,
        )
        .await
        .unwrap();
    assert!(matches!(
        landing,
        crate::bun::agent::discovery_ownership::Landing::InFlight
    ));
    // Only the write in flight is slow: wait until it has read its stall.
    while agent.loop_stalls.reached(LoopStall::Persist) == reached {
        tokio::task::yield_now().await;
    }
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    (agent, root, id, service)
}

/// The 0.2.1 candidate 5 soak: a node restarted on a busy disk, a consumer
/// sync's journal write went on off the loop for 5.9 s, and the next health
/// probe that changed an instance's health published it, which waited for
/// that write inline: a 4,987 ms `deploy_op (health_probe_result)` turn
/// (#1100). The probe now leaves publication to the next one, and a health
/// change it couldn't publish doesn't restart the instance.
#[tokio::test]
async fn a_health_change_waits_for_no_discovery_write_in_flight() {
    let (mut agent, _root, id, service) = web_with_a_discovery_write_in_flight().await;
    let created_at = agent.supervisor.get_instance(&id).unwrap().created_at;
    let probe = std::time::Instant::now();
    agent
        .handle_deploy_op(crate::bun::agent::deploy_ops::DeployOp::HealthProbeResult {
            instance_id: id.clone(),
            created_at,
            status: Ok(crate::bun::health::HealthStatus::Unhealthy),
        })
        .await;
    let took = probe.elapsed();
    assert!(
        took < TURN_BUDGET,
        "the probe's turn took {took:?} while a journal write took {STALLED_JOURNAL_WRITE:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "the probe waited for the write"
    );
    assert!(
        agent.service_map.resolve(&service).unwrap().backends[0].healthy,
        "published without journalling"
    );
    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Unhealthy);
    assert_eq!(
        instance.restart_count, 0,
        "restarted before its withdrawal was published"
    );

    agent.settle_discovery_write().await;
    let created_at = agent.supervisor.get_instance(&id).unwrap().created_at;
    agent
        .complete_health_probe(
            id.clone(),
            created_at,
            Ok(crate::bun::health::HealthStatus::Unhealthy),
        )
        .await;
    assert!(
        !agent.service_map.resolve(&service).unwrap().backends[0].healthy,
        "the next probe didn't publish the withdrawal"
    );
    assert!(
        !agent
            .service_map_tx
            .borrow()
            .resolve(&service)
            .unwrap()
            .backends[0]
            .healthy,
        "readers still see a healthy backend"
    );
    agent.retire_workload("web", "default").await.unwrap();
}

/// A turn that does wait for a write in flight says so in its slow-turn
/// line, rather than reporting only its own (fast) journal write (#1100).
#[tokio::test]
async fn a_turn_that_waits_for_a_write_in_flight_names_the_wait() {
    let (mut agent, _root, _id, _service) = web_with_a_discovery_write_in_flight().await;
    let turn = agent
        .loop_meter
        .begin(LoopBranch::DeployOp, Some("health_probe_result"));
    agent.settle_discovery_write().await;
    agent.loop_meter.finish(turn);
    let waited = agent
        .loop_meter
        .last_turn()
        .unwrap()
        .part(TurnPart::InFlightWrite)
        .expect("the wait for the write in flight wasn't timed");
    assert!(
        waited >= STALLED_JOURNAL_WRITE / 2,
        "timed {waited:?} of a {STALLED_JOURNAL_WRITE:?} write"
    );
    assert!(!agent.discovery_write_in_flight());
}

/// Deliver one health probe's result in a `deploy_op` turn of its own, as
/// the loop does, and return the turn the meter saw.
async fn probe_turn(
    agent: &mut TestAgent,
    id: &InstanceId,
    status: crate::bun::health::HealthStatus,
) -> crate::bun::loop_meter::TurnRecord {
    let created_at = agent.supervisor.get_instance(id).unwrap().created_at;
    let turn = agent.begin_turn(LoopBranch::DeployOp, Some("health_probe_result"));
    agent
        .handle_deploy_op(crate::bun::agent::deploy_ops::DeployOp::HealthProbeResult {
            instance_id: id.clone(),
            created_at,
            status: Ok(status),
        })
        .await;
    agent.turn_deadline = None;
    agent.loop_meter.finish(turn);
    agent.loop_meter.last_turn().unwrap()
}

/// Wait for the write in flight to land, then take the loop's turn for it.
async fn landing_turn(agent: &mut BunAgent<MockGrill>) -> crate::bun::loop_meter::TurnRecord {
    let outcome = BunAgent::<MockGrill>::discovery_write_done(&mut agent.discovery_ownership).await;
    let turn = agent.begin_landing_turn();
    agent.land_write_in_flight(outcome).await;
    agent.consumer_syncs.end_turn();
    agent.turn_deadline = None;
    agent.loop_meter.finish(turn);
    agent.loop_meter.last_turn().unwrap()
}

/// The 0.2.1 candidate 9 jobs final: all three VMs met the same host disk
/// stall, and a health probe that turned an instance unhealthy journalled
/// its withdrawal inline, under the #351 decision-2 allowance for fsync'd
/// writes: a 3,950 ms `deploy_op (health_probe_result)` turn whose only
/// part was its own journal write (#1255). The probe now waits for its
/// write only until the turn's runtime budget runs out, as a consumer step
/// does. The write goes on off the loop; the withdrawal, and the restart
/// it allows, follow once the write has landed.
#[tokio::test]
async fn a_health_probe_publication_leaves_its_own_slow_journal_write_in_flight() {
    let (mut agent, _root, id, service) = web_journalling_discovery().await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let probe = probe_turn(&mut agent, &id, crate::bun::health::HealthStatus::Unhealthy).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "the probe didn't journal its withdrawal: {probe}"
    );
    // The write sleeps 1.6 s and the turn's deadline is 500 ms, so a turn
    // that waited for it would find it landed.
    assert!(
        agent.discovery_write_in_flight(),
        "the probe's turn waited for its own journal write: {probe}"
    );
    assert!(probe.took < TURN_BUDGET, "{probe}");
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    assert!(
        agent.service_map.resolve(&service).unwrap().backends[0].healthy,
        "the withdrawal was installed before its journal write landed"
    );
    let instance = agent.supervisor.get_instance(&id).unwrap();
    assert_eq!(instance.state, ContainerState::Unhealthy);
    assert_eq!(
        instance.restart_count, 0,
        "restarted before its withdrawal was on disk"
    );

    // A probe meanwhile journals nothing more and restarts nothing.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    probe_turn(&mut agent, &id, crate::bun::health::HealthStatus::Unhealthy).await;
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 0);

    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.branch, LoopBranch::DeployOp, "{landed}");
    assert_eq!(landed.detail, Some("health_probe_result/landed"));
    assert!(!agent.discovery_write_in_flight());
    assert!(
        !agent.service_map.resolve(&service).unwrap().backends[0].healthy,
        "the landed write's withdrawal wasn't installed"
    );
    assert!(
        !agent
            .service_map_tx
            .borrow()
            .resolve(&service)
            .unwrap()
            .backends[0]
            .healthy,
        "readers still see a healthy backend"
    );
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().restart_count,
        1,
        "the published withdrawal didn't restart the instance"
    );
    agent.retire_workload("web", "default").await.unwrap();
}

/// A health probe's write that fails after its turn moved on fences
/// discovery like any failed write, and the withdrawal it would have
/// recorded is neither installed nor allowed to restart the instance.
#[tokio::test]
async fn a_health_probe_write_that_fails_off_the_turn_installs_nothing_and_restarts_nothing() {
    let (mut agent, _root, id, service) = web_journalling_discovery().await;
    let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
        &mut agent.discovery_ownership
    else {
        panic!("discovery ownership is not ready before the probe");
    };
    journal.fail_next_write();
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let probe = probe_turn(&mut agent, &id, crate::bun::health::HealthStatus::Unhealthy).await;
    assert!(agent.discovery_write_in_flight(), "{probe}");
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);

    landing_turn(&mut agent).await;
    assert!(matches!(
        agent.discovery_ownership,
        DiscoveryOwnership::Uncertain
    ));
    assert!(
        agent.service_map.resolve(&service).unwrap().backends[0].healthy,
        "a withdrawal whose write failed was installed"
    );
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().restart_count,
        0,
        "restarted on a withdrawal that never reached disk"
    );
}

/// A discovery-owning agent with a Pending `web` instance whose runtime
/// holds an address, for a service the journal already owns, so the start
/// may record the hold.
async fn published_hold_fixture() -> (
    TestAgent,
    MockGrill,
    tempfile::TempDir,
    crate::grill::runc_intent::NetworkReference,
) {
    let (mut agent, grill, root, reference) = unpublished_hold_fixture().await;
    let (reply, registered) = oneshot::channel();
    agent
        .handle_deploy_op(
            crate::bun::agent::deploy_ops::DeployOp::RegisterServiceApp {
                app_name: "web".into(),
                namespace: "default".into(),
                port: 8080,
                firewall: None,
                reply,
            },
        )
        .await;
    registered.await.unwrap().unwrap();
    (agent, grill, root, reference)
}

/// Hand the loop a workload's `ApplyNetworkPreStart` in a `deploy_op` turn
/// of its own, as a deploy worker does, with the hold the runtime retained
/// for it. Returns the turn the meter saw and the loop's answer.
async fn pre_start_turn(
    agent: &mut TestAgent,
    grill: &MockGrill,
    root: &tempfile::TempDir,
    reference: &crate::grill::runc_intent::NetworkReference,
) -> (crate::bun::loop_meter::TurnRecord, Result<(), BunError>) {
    let spec = basic_config().app.remove("web").unwrap();
    let retained = grill
        .retain_network_reference(&reference.instance_id)
        .await
        .map_err(BunError::from);
    let (reply, result) = oneshot::channel();
    let turn = agent.begin_turn(LoopBranch::DeployOp, Some("apply_network_pre_start"));
    agent
        .handle_deploy_op(
            crate::bun::agent::deploy_ops::DeployOp::ApplyNetworkPreStart {
                instance_id: reference.instance_id.clone(),
                app_name: "web".into(),
                spec: Some(Box::new(spec)),
                cgroup_path: root.path().join("cgroup"),
                retained,
                egress: Box::default(),
                reply,
            },
        )
        .await;
    agent.turn_deadline = None;
    agent.loop_meter.finish(turn);
    (agent.loop_meter.last_turn().unwrap(), result.await.unwrap())
}

/// The 0.2.1 candidate 9 jobs final, run 2: a slow host disk held one
/// `deploy_op (apply_network_pre_start)` turn for 2,493 ms (#1266). A
/// workload's start journals its network reference before it may run, and
/// that write had no deadline. It now waits for its write only until the
/// turn's runtime budget runs out, as a health probe's publication does
/// (#1255). The start stays pending meanwhile, and the deploy worker's next
/// ask finds the reference recorded and goes on. The workload never starts
/// before its reference is on disk.
#[tokio::test]
async fn a_workload_start_leaves_its_slow_network_reference_write_in_flight() {
    let (mut agent, grill, root, reference) = published_hold_fixture().await;
    let id = reference.instance_id.clone();
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (start, result) = pre_start_turn(&mut agent, &grill, &root, &reference).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "the start didn't journal its network reference: {start}, {result:?}"
    );
    // The write sleeps 1.6 s and the turn's deadline is 500 ms, so a turn
    // that waited for it would find it landed.
    assert!(
        agent.discovery_write_in_flight(),
        "the start's turn waited for its own journal write: {start}"
    );
    assert!(start.took < TURN_BUDGET, "{start}");
    assert!(
        start.part(TurnPart::JournalWrite).is_some(),
        "the turn's line doesn't name the journal write: {start}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "a start whose reference isn't on disk yet must be asked again: {result:?}"
    );
    assert_ne!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Failed,
        "a pending start was failed"
    );
    assert!(!agent.network_references.contains_key(&id));
    assert!(
        grill.network_reference(&id).await.unwrap().is_some(),
        "a hold the journal may be recording was handed back"
    );
    // A retirement's turn meanwhile can't read the journal as not recording
    // the hold: the write in flight may be recording it. (Outside a turn,
    // where nothing else waits, it would wait the write out, #1263.)
    let turn = agent.begin_turn(LoopBranch::StopWait, None);
    let released = agent.release_network_reference(&id, None).await;
    agent.turn_deadline = None;
    agent.loop_meter.finish(turn);
    assert!(
        matches!(released, Err(BunError::StillRunning { .. })),
        "{released:?}"
    );

    // The worker asks again before the write lands: still pending, with no
    // second write and no wait.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (again, result) = pre_start_turn(&mut agent, &grill, &root, &reference).await;
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert!(again.took < TURN_BUDGET, "{again}");

    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.branch, LoopBranch::DeployOp, "{landed}");
    assert_eq!(landed.detail, Some("apply_network_pre_start/landed"));
    assert!(!agent.discovery_write_in_flight());
    assert_eq!(
        agent.journal_reference(&reference),
        crate::bun::agent::discovery_ownership::JournalReference::Recorded
    );

    // The next ask finds the reference on disk: the start goes on without
    // writing it again.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = pre_start_turn(&mut agent, &grill, &root, &reference).await;
    result.unwrap();
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(agent.network_references.get(&id), Some(&reference));
}

/// A start's reference write that fails after its turn moved on fences
/// discovery like any failed write. The hold isn't handed back, since the
/// write may have reached disk, and the start's next ask records the
/// reference again before the workload may run.
#[tokio::test]
async fn a_workload_start_whose_reference_write_fails_off_the_turn_records_it_again() {
    let (mut agent, grill, root, reference) = published_hold_fixture().await;
    let id = reference.instance_id.clone();
    let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
        &mut agent.discovery_ownership
    else {
        panic!("discovery ownership is not ready before the start");
    };
    journal.fail_next_write();
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let (start, result) = pre_start_turn(&mut agent, &grill, &root, &reference).await;
    assert!(agent.discovery_write_in_flight(), "{start}");
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);

    landing_turn(&mut agent).await;
    assert!(matches!(
        agent.discovery_ownership,
        DiscoveryOwnership::Uncertain
    ));
    assert!(!agent.network_references.contains_key(&id));
    assert!(
        grill.network_reference(&id).await.unwrap().is_some(),
        "a hold whose write may have reached disk was handed back"
    );

    let (_, result) = pre_start_turn(&mut agent, &grill, &root, &reference).await;
    result.unwrap();
    assert_eq!(
        agent.journal_reference(&reference),
        crate::bun::agent::discovery_ownership::JournalReference::Recorded
    );
    assert_eq!(agent.network_references.get(&id), Some(&reference));
}

/// End to end: a deploy whose start meets a slow discovery journal still
/// completes. Its worker asks again while the reference write is in flight,
/// rather than failing the start (#1266).
#[tokio::test]
async fn a_deploy_completes_when_its_network_reference_write_outlasts_the_turn() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let task = tokio::spawn(async move {
        agent.run().await;
        agent
    });
    expect_complete(&send_deploy(&tx, replicated("web", 1)).await);
    shutdown.cancel();
    let agent = task.await.unwrap();
    assert_eq!(
        agent.network_references.get(&reference.instance_id),
        Some(&reference)
    );
    assert_eq!(
        agent.journal_reference(&reference),
        crate::bun::agent::discovery_ownership::JournalReference::Recorded
    );
}

/// A restart records its recreated instance's network reference in the
/// loop's turn between Create and Start, so it meets the same slow journal.
/// A write that outlasts that turn holds the restart until it has landed;
/// it doesn't refuse the restart (#1266).
#[tokio::test]
async fn a_restart_waits_for_its_slow_network_reference_write_instead_of_refusing() {
    let (mut agent, _tx, shutdown, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let original = original_test_network_reference();
    grill.set_network_reference(original.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let successor: crate::grill::runc_intent::NetworkReference =
        serde_json::from_value(serde_json::json!({
            "instance_id": "default__web-0",
            "generation": "fedcba0987654321fedcba0987654321",
            "container_index": 8
        }))
        .unwrap();
    let stalls = Arc::clone(&agent.loop_stalls);
    let task = tokio::spawn(async move {
        agent.run().await;
        agent
    });
    // Hold the create, so the successor's hold is in place before the
    // restart retains it.
    grill.set_call_delay(
        MockCall::Create,
        Some(std::time::Duration::from_millis(500)),
    );
    let released = calls_of(&grill, "release_network_reference");
    let started = calls_of(&grill, "start");
    crash(&grill);
    wait_for_calls(&grill, "release_network_reference", released, 1).await;
    grill.set_network_reference(successor.clone()).await;
    stalls.set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    wait_for_calls(&grill, "start", started, 1).await;
    stalls.set(LoopStall::Persist, std::time::Duration::ZERO);
    shutdown.cancel();
    let mut agent = task.await.unwrap();
    assert_eq!(
        agent.network_references.get(&successor.instance_id),
        Some(&successor)
    );
    // The replacement's publication may still be in flight (#1263).
    agent.settle_discovery_write().await;
    assert!(agent.journal_holds(&successor));
}

// ---- every writer's own slow write (#1263) ---------------------------------
//
// Consumer steps, health probes and workload starts already left a slow
// discovery-journal write in flight. These scenarios stall the persist under
// each of the other writers in turn. Their verdict counts work rather than
// timing the turn against the host's disk (#508, #939, #1140): a stalled
// write that is still in flight when its turn has ended is one the turn
// didn't wait for, and a write that was waited for has always landed.

/// Longer than [`STATUS_PATIENCE`], so a turn that waited for a write this
/// slow couldn't answer a status in time, whatever the host's disk does.
const STALLED_DISK: std::time::Duration = std::time::Duration::from_secs(5);

/// Begin a turn of `branch` the way the loop does, for a scenario that hands
/// the agent its work directly.
fn begin_scenario_turn(
    agent: &mut BunAgent<MockGrill>,
    branch: LoopBranch,
    detail: Option<&'static str>,
) -> crate::bun::loop_meter::Turn {
    agent.begin_turn(branch, detail)
}

/// End a turn begun with [`begin_scenario_turn`] and return what the meter saw.
fn end_scenario_turn(
    agent: &mut BunAgent<MockGrill>,
    turn: crate::bun::loop_meter::Turn,
) -> crate::bun::loop_meter::TurnRecord {
    agent.turn_deadline = None;
    agent.loop_meter.finish(turn);
    agent.loop_meter.last_turn().unwrap()
}

/// The backends the discovery journal itself records for `service`: not the
/// inventory before a write in flight.
fn journalled_backends(
    agent: &BunAgent<MockGrill>,
    service: &crate::onion::service_id::ServiceId,
) -> Option<Vec<String>> {
    let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
        &agent.discovery_ownership
    else {
        return None;
    };
    journal
        .inventory()
        .services
        .iter()
        .find(|owner| {
            owner.entry.namespace == service.namespace && owner.entry.app_name == service.name
        })
        .map(|owner| {
            owner
                .entry
                .backends
                .iter()
                .map(|backend| backend.instance_id.clone())
                .collect()
        })
}

/// The backends `service_map` routes `service` to.
fn routed_backends(
    agent: &TestAgent,
    service: &crate::onion::service_id::ServiceId,
) -> Option<Vec<String>> {
    agent.service_map.resolve(service).map(|entry| {
        entry
            .backends
            .iter()
            .map(|backend| backend.instance_id.clone())
            .collect()
    })
}

/// An agent that journals discovery, with `basic_config`'s `web` deployed.
async fn journalling_web() -> (
    TestAgent,
    mpsc::Sender<AgentCommand>,
    CancellationToken,
    MockGrill,
    tempfile::TempDir,
) {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    (agent, tx, shutdown, grill, root)
}

/// Queue a status once the running loop has reached a stalled persist, and
/// say whether it was answered within [`STATUS_PATIENCE`].
async fn status_answered_during_a_stalled_write(
    tx: &mpsc::Sender<AgentCommand>,
    stalls: &LoopStalls,
    reached_before: usize,
) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while stalls.reached(LoopStall::Persist) == reached_before {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the work never reached a persist");
    let (response, answer) = oneshot::channel();
    tx.send(AgentCommand::Status { response }).await.unwrap();
    tokio::time::timeout(STATUS_PATIENCE, answer)
        .await
        .is_ok_and(|answer| answer.is_ok())
}

/// Hand the loop a deploy op in a turn of its own and return the turn and
/// the op's answer.
async fn deploy_op_turn<T>(
    agent: &mut TestAgent,
    detail: &'static str,
    op: impl FnOnce(oneshot::Sender<Result<T, BunError>>) -> crate::bun::agent::deploy_ops::DeployOp,
) -> (crate::bun::loop_meter::TurnRecord, Result<T, BunError>) {
    let (reply, result) = oneshot::channel();
    let turn = begin_scenario_turn(agent, LoopBranch::DeployOp, Some(detail));
    agent.handle_deploy_op(op(reply)).await;
    let record = end_scenario_turn(agent, turn);
    (record, result.await.unwrap())
}

/// Crash `web`'s only replica and hand the loop its restart's steps, one a
/// turn, until the Start step is in flight.
async fn restart_until_its_start(agent: &mut TestAgent, grill: &MockGrill, id: &InstanceId) {
    crash(grill);
    agent.check_apps().await;
    agent.begin_restart_launches().await;
    while agent.restart_step_of(id) != Some(super::restarts::RestartStep::Start) {
        let outcome = agent
            .restart_steps
            .join_next_with_id()
            .await
            .expect("the restart stopped before its start");
        agent.finish_restart_step(outcome).await;
    }
}

/// Take the next restart step's result in a turn of its own.
async fn restart_step_turn(agent: &mut TestAgent) -> crate::bun::loop_meter::TurnRecord {
    let outcome = agent
        .restart_steps
        .join_next_with_id()
        .await
        .expect("no restart step is in flight");
    let turn = begin_scenario_turn(agent, LoopBranch::RestartStep, None);
    agent.finish_restart_step(outcome).await;
    end_scenario_turn(agent, turn)
}

/// The 0.2.2 candidate 4 V02 fast tier: a 5 s host disk stall made one
/// `restart_step` turn take 5,555 ms (#1263). A restart publishes its
/// replacement's backend in the turn after Start, and that journal write
/// had no deadline. It now waits only until the turn's deadline. The
/// replacement stays `Starting` and out of `service_map` meanwhile, and a
/// step that comes back after 100 ms looks again: it publishes once the
/// journal records the backend, without writing it a second time.
#[tokio::test]
async fn a_restart_publication_leaves_its_slow_journal_write_in_flight() {
    let (mut agent, _tx, _shutdown, grill, _root) = journalling_web().await;
    let id = InstanceId("default__web-0".into());
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    // A new address, so the journal doesn't already record the backend the
    // restart publishes.
    grill.set_container_ip("10.0.2.9".parse().unwrap());
    restart_until_its_start(&mut agent, &grill, &id).await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let start = restart_step_turn(&mut agent).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "the restart didn't journal its replacement: {start}"
    );
    // The write sleeps 1.6 s and the turn's deadline is 500 ms, so a turn
    // that waited for it would find it landed.
    assert!(
        agent.discovery_write_in_flight(),
        "the restart's turn waited for its own journal write: {start}"
    );
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Starting
    );
    assert_eq!(
        routed_backends(&agent, &service),
        Some(Vec::new()),
        "routed to the replacement before its journal write landed"
    );
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);

    // Its next look, before the write lands, writes nothing and waits for
    // nothing.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let again = restart_step_turn(&mut agent).await;
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert!(agent.discovery_write_in_flight(), "{again}");

    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.branch, LoopBranch::RestartStep, "{landed}");
    assert_eq!(landed.detail, Some("restart_step/landed"));
    assert_eq!(
        journalled_backends(&agent, &service),
        Some(vec![id.0.clone()])
    );

    // The look after it finds the backend recorded and publishes it.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    restart_step_turn(&mut agent).await;
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running
    );
    assert_eq!(routed_backends(&agent, &service), Some(vec![id.0.clone()]));
}

/// End to end: a restart whose publication meets a stalled disk keeps the
/// loop answering, and finishes once the write lands.
#[tokio::test]
async fn status_answers_while_a_restart_journals_to_a_stalled_disk() {
    let (agent, tx, shutdown, grill, _root) = journalling_web().await;
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let stalls = Arc::clone(&agent.loop_stalls);
    let mut agent = agent;
    let task = tokio::spawn(async move {
        agent.run().await;
        agent
    });
    stalls.set(LoopStall::Persist, STALLED_DISK);
    let reached = stalls.reached(LoopStall::Persist);
    grill.set_container_ip("10.0.2.9".parse().unwrap());
    crash(&grill);
    let answered = status_answered_during_a_stalled_write(&tx, &stalls, reached).await;
    stalls.set(LoopStall::Persist, std::time::Duration::ZERO);
    assert!(
        answered,
        "status went unanswered while a restart journalled to a stalled disk"
    );
    // The replacement has started, and stays `Starting` until its write
    // lands, so no state sweep reads it before the runtime says it runs.
    let id = InstanceId("default__web-0".into());
    grill.set_state(&id, ContainerState::Running);
    grill.set_exit_code(&id, None);
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let (response, answer) = oneshot::channel();
            tx.send(AgentCommand::Status { response }).await.unwrap();
            if answer.await.unwrap().iter().any(|instance| {
                instance.restart_count > 0 && instance.state.eq_ignore_ascii_case("running")
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the restart never finished");
    shutdown.cancel();
    let agent = task.await.unwrap();
    assert_eq!(
        journalled_backends(&agent, &service),
        Some(vec!["default__web-0".to_string()])
    );
    assert_eq!(
        routed_backends(&agent, &service),
        Some(vec!["default__web-0".to_string()])
    );
}

/// A discovery-journalling agent with `web` admitted and its service
/// registered, its only replica `Starting`, and what the runtime says about
/// the replica's launch.
async fn starting_web() -> (
    TestAgent,
    tempfile::TempDir,
    InstanceId,
    super::launch_evidence::LaunchEvidence,
) {
    let (mut agent, _, _, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let spec = basic_config().app.remove("web").unwrap();
    let (_, admitted) = deploy_op_turn(&mut agent, "supervisor_deploy_app", |reply| {
        crate::bun::agent::deploy_ops::DeployOp::SupervisorDeployApp {
            app_name: "web".into(),
            namespace: "default".into(),
            spec: Box::new(spec),
            reply,
        }
    })
    .await;
    let id = admitted.unwrap()[0].clone();
    let (_, registered) = deploy_op_turn(&mut agent, "register_service_app", |reply| {
        crate::bun::agent::deploy_ops::DeployOp::RegisterServiceApp {
            app_name: "web".into(),
            namespace: "default".into(),
            port: 8080,
            firewall: None,
            reply,
        }
    })
    .await;
    registered.unwrap();
    agent.supervisor.get_instance_mut(&id).unwrap().state = ContainerState::Starting;
    let evidence = super::launch_evidence::LaunchEvidence::read(&grill, &id).await;
    (agent, root, id, evidence)
}

/// Hand the loop a fresh replica's `FinishFreshInstance` in a turn.
async fn finish_fresh_turn(
    agent: &mut TestAgent,
    id: &InstanceId,
    evidence: &super::launch_evidence::LaunchEvidence,
) -> (crate::bun::loop_meter::TurnRecord, Result<(), BunError>) {
    deploy_op_turn(agent, "finish_fresh_instance", |reply| {
        crate::bun::agent::deploy_ops::DeployOp::FinishFreshInstance {
            instance_id: id.clone(),
            app_name: "web".into(),
            namespace: "default".into(),
            evidence: Box::new(evidence.clone()),
            reply,
        }
    })
    .await
}

/// A deploy publishes each fresh replica's backend in the turn after its
/// start, and that journal write had no deadline (#1263). It now waits only
/// until the turn's deadline, as a workload start's reference write does
/// (#1266). Nothing about the start is recorded meanwhile: the replica
/// stays `Starting` and out of `service_map`, and the op answers
/// `StillRunning`. The deploy worker's next ask after the landing finds the
/// backend recorded and finishes the start without writing it again.
#[tokio::test]
async fn a_fresh_start_publication_leaves_its_slow_journal_write_in_flight() {
    let (mut agent, _root, id, evidence) = starting_web().await;
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (start, result) = finish_fresh_turn(&mut agent, &id, &evidence).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "the start didn't journal its backend: {start}, {result:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "the start's turn waited for its own journal write: {start}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "a start whose backend isn't on disk yet must be asked again: {result:?}"
    );
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Starting
    );
    assert_eq!(routed_backends(&agent, &service), Some(Vec::new()));

    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (again, result) = finish_fresh_turn(&mut agent, &id, &evidence).await;
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert!(agent.discovery_write_in_flight(), "{again}");

    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.branch, LoopBranch::DeployOp, "{landed}");
    assert_eq!(landed.detail, Some("finish_fresh_instance/landed"));

    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = finish_fresh_turn(&mut agent, &id, &evidence).await;
    result.unwrap();
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().state,
        ContainerState::Running
    );
    assert_eq!(routed_backends(&agent, &service), Some(vec![id.0.clone()]));
    assert_eq!(
        journalled_backends(&agent, &service),
        Some(vec![id.0.clone()])
    );
}

/// A deploy registers its app's service, and publishes it, before any
/// replica starts (#1263). A slow write leaves the service unregistered in
/// `service_map` until the worker's next ask finds it journalled.
#[tokio::test]
async fn a_service_registration_leaves_its_slow_journal_write_in_flight() {
    let (mut agent, _, _, _) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let register = |reply| crate::bun::agent::deploy_ops::DeployOp::RegisterServiceApp {
        app_name: "web".into(),
        namespace: "default".into(),
        port: 8080,
        firewall: None,
        reply,
    };
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = deploy_op_turn(&mut agent, "register_service_app", register).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "{turn}, {result:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "the registration's turn waited for its own journal write: {turn}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(routed_backends(&agent, &service), None);

    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(
        landed.detail,
        Some("register_service_app/landed"),
        "{landed}"
    );
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = deploy_op_turn(&mut agent, "register_service_app", register).await;
    result.unwrap();
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(routed_backends(&agent, &service), Some(Vec::new()));
    assert_eq!(journalled_backends(&agent, &service), Some(Vec::new()));
}

/// A rolling deploy publishes each healthy replacement's backend before
/// anything old retires (#1263). A slow write leaves it unpublished until
/// the worker's next ask finds it journalled.
#[tokio::test]
async fn a_rolling_replacement_publication_leaves_its_slow_journal_write_in_flight() {
    let (mut agent, _tx, _shutdown, _grill, _root) = journalling_web().await;
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let replacement = InstanceId("default__web-1".into());
    let publish = |reply| crate::bun::agent::deploy_ops::DeployOp::PublishNewBackend {
        app_name: "web".into(),
        namespace: "default".into(),
        new_id: InstanceId("default__web-1".into()),
        host_port: Some(30_999),
        container_ip: None,
        has_port: true,
        reply,
    };
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = deploy_op_turn(&mut agent, "publish_new_backend", publish).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "{turn}, {result:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "the publication's turn waited for its own journal write: {turn}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(
        routed_backends(&agent, &service),
        Some(vec!["default__web-0".to_string()])
    );

    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = deploy_op_turn(&mut agent, "publish_new_backend", publish).await;
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);

    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(
        landed.detail,
        Some("publish_new_backend/landed"),
        "{landed}"
    );
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = deploy_op_turn(&mut agent, "publish_new_backend", publish).await;
    result.unwrap();
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(
        routed_backends(&agent, &service),
        Some(vec!["default__web-0".to_string(), replacement.0.clone()])
    );
}

/// End to end: a deploy whose publications meet a stalled disk keeps the
/// loop answering, and completes once the writes land.
#[tokio::test]
async fn status_answers_while_a_deploy_journals_to_a_stalled_disk() {
    let (mut agent, tx, shutdown, _grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let stalls = Arc::clone(&agent.loop_stalls);
    let task = tokio::spawn(async move {
        agent.run().await;
        agent
    });
    stalls.set(LoopStall::Persist, STALLED_DISK);
    let reached = stalls.reached(LoopStall::Persist);
    let deploying = {
        let tx = tx.clone();
        tokio::spawn(async move { send_deploy(&tx, basic_config()).await })
    };
    let answered = status_answered_during_a_stalled_write(&tx, &stalls, reached).await;
    stalls.set(LoopStall::Persist, std::time::Duration::ZERO);
    assert!(
        answered,
        "status went unanswered while a deploy journalled to a stalled disk"
    );
    expect_complete(&deploying.await.unwrap());
    shutdown.cancel();
    let agent = task.await.unwrap();
    assert_eq!(
        journalled_backends(&agent, &service),
        Some(vec!["default__web-0".to_string()])
    );
}

/// `web` deployed with a network reference the journal records, its backend
/// withdrawn, ready for retirement to release the reference.
async fn withdrawn_web_holding_its_reference() -> (
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
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    assert!(agent.journal_holds(&reference));
    agent
        .withdraw_instance_backend(&reference.instance_id)
        .await
        .unwrap();
    (agent, grill, root, reference)
}

/// Release `id`'s network reference in a turn of its own.
async fn release_turn(
    agent: &mut TestAgent,
    id: &InstanceId,
) -> (crate::bun::loop_meter::TurnRecord, Result<(), BunError>) {
    let turn = begin_scenario_turn(agent, LoopBranch::StopWait, None);
    let result = agent.release_network_reference(id, None).await;
    (end_scenario_turn(agent, turn), result)
}

/// Retiring an instance releases its network reference with two journal
/// writes, the release permission and then forgetting it, and both had no
/// deadline (#1263). Each now waits only until the turn's deadline, and the
/// retirement answers `StillRunning` until the journal says what it wrote.
#[tokio::test]
async fn a_network_reference_release_leaves_its_slow_journal_writes_in_flight() {
    let (mut agent, grill, _root, reference) = withdrawn_web_holding_its_reference().await;
    let id = reference.instance_id.clone();
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = release_turn(&mut agent, &id).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "the release didn't journal its permission: {turn}, {result:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "the release's turn waited for its permission's journal write: {turn}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(calls_of(&grill, "release_network_reference"), 0);

    // Asked again before the write lands: nothing written, nothing waited for.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = release_turn(&mut agent, &id).await;
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);

    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.detail, Some("discovery_release/landed"), "{landed}");

    // With the permission on disk the runtime releases the address, and
    // forgetting the permission meets the stalled disk too.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = release_turn(&mut agent, &id).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "{turn}, {result:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "the release's turn waited for forgetting the permission: {turn}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert!(grill.network_reference(&id).await.unwrap().is_none());
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    landing_turn(&mut agent).await;

    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = release_turn(&mut agent, &id).await;
    result.unwrap();
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert!(!agent.network_references.contains_key(&id));
    assert_eq!(
        agent.journal_reference(&reference),
        crate::bun::agent::discovery_ownership::JournalReference::Unrecorded
    );
}

/// Retire `service`'s allocation in a turn of its own.
async fn retire_service_turn(
    agent: &mut BunAgent<MockGrill>,
    service: &crate::onion::service_id::ServiceId,
) -> (crate::bun::loop_meter::TurnRecord, Result<(), BunError>) {
    let turn = begin_scenario_turn(agent, LoopBranch::StopWait, None);
    let result = agent.retire_discovery_service(service).await;
    (end_scenario_turn(agent, turn), result)
}

/// A completed stop retires its service's allocation with two journal
/// writes, `Withdrawn` and then removing it, and both had no deadline
/// (#1263). Each now waits only until the turn's deadline, and the stop
/// asks again until the journal no longer holds the allocation.
#[tokio::test]
async fn a_service_retirement_leaves_its_slow_journal_writes_in_flight() {
    let (mut agent, _tx, _shutdown, _grill, _root) = journalling_web().await;
    let id = InstanceId("default__web-0".into());
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    agent.withdraw_instance_backend(&id).await.unwrap();
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = retire_service_turn(&mut agent, &service).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "{turn}, {result:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "the retirement's turn waited for its own journal write: {turn}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );

    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = retire_service_turn(&mut agent, &service).await;
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.detail, Some("service_retirement/landed"), "{landed}");

    // `Withdrawn` is on disk: the next ask writes only the removal.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = retire_service_turn(&mut agent, &service).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "{turn}, {result:?}"
    );
    assert!(agent.discovery_write_in_flight(), "{turn}");
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    landing_turn(&mut agent).await;

    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = retire_service_turn(&mut agent, &service).await;
    result.unwrap();
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(journalled_backends(&agent, &service), None);
}

/// End to end: a stop whose retirement journals to a stalled disk keeps the
/// loop answering, and is answered once its writes land.
#[tokio::test]
async fn status_answers_while_a_stop_journals_to_a_stalled_disk() {
    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    agent
        .enable_fresh_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    let reference = original_test_network_reference();
    grill.set_network_reference(reference.clone()).await;
    expect_complete(&drain_deploy(&mut agent, basic_config()).await);
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let stalls = Arc::clone(&agent.loop_stalls);
    let task = tokio::spawn(async move {
        agent.run().await;
        agent
    });
    stalls.set(LoopStall::Persist, STALLED_DISK);
    let reached = stalls.reached(LoopStall::Persist);
    let (response, stopped) = oneshot::channel();
    tx.send(AgentCommand::Stop {
        app_name: "web".into(),
        namespace: "default".into(),
        response,
    })
    .await
    .unwrap();
    let answered = status_answered_during_a_stalled_write(&tx, &stalls, reached).await;
    stalls.set(LoopStall::Persist, std::time::Duration::ZERO);
    assert!(
        answered,
        "status went unanswered while a stop journalled to a stalled disk"
    );
    tokio::time::timeout(std::time::Duration::from_secs(20), stopped)
        .await
        .expect("the stop was never answered")
        .expect("the stop's waiter was dropped")
        .unwrap();
    shutdown.cancel();
    let agent = task.await.unwrap();
    assert_eq!(journalled_backends(&agent, &service), None);
    assert_eq!(
        agent.journal_reference(&reference),
        crate::bun::agent::discovery_ownership::JournalReference::Unrecorded
    );
}

/// The 0.2.2 candidate 4 soak line again: of that 5,555 ms `restart_step`
/// turn, 5,513 ms was the egress owners checkpoint (#1263). A restart, a
/// deploy's start and a retirement each write it on a Linux node with eBPF,
/// and each waited for it inline. It now waits only until the turn's
/// deadline, like a discovery journal write. The binding it records goes
/// into `egress_bindings` only once it has landed, so no co-tenant's map
/// rebuild sees it first; another writer meanwhile neither writes nor
/// waits; and outside a turn, where nothing else waits, the write is waited
/// out. The Linux callers turn `InFlight` into `StillRunning`.
#[tokio::test]
async fn an_egress_owners_checkpoint_leaves_its_slow_write_in_flight() {
    let (mut agent, _, _, _) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    let records = root.path().join("records");
    agent.set_records_dir(records.clone());
    let id = InstanceId("default__web-0".into());
    let other = InstanceId("default__api-0".into());
    let binding = crate::bun::egress_owners::owned_test_binding(7, vec!["example.com:443".into()]);
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let turn = begin_scenario_turn(&mut agent, LoopBranch::RestartStep, None);
    let deadline = agent.turn_deadline();
    let landing = agent
        .checkpoint_egress_owner(&id, Some(binding.clone()), Some(deadline))
        .await;
    let restart = end_scenario_turn(&mut agent, turn);
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "the turn didn't write the checkpoint: {restart}, {landing:?}"
    );
    // The write sleeps 1.6 s and the turn's deadline is 500 ms, so a turn
    // that waited for it would find it landed.
    assert!(
        agent.egress_write_in_flight(),
        "the turn waited for its own egress owners write: {restart}"
    );
    assert!(
        matches!(
            landing,
            Ok(crate::bun::agent::discovery_ownership::Landing::InFlight)
        ),
        "{landing:?}"
    );
    assert!(
        restart.part(TurnPart::EgressWrite).is_some(),
        "the turn's line doesn't name the egress owners write: {restart}"
    );
    assert!(
        !agent.egress_bindings.contains_key(&id),
        "the binding was in memory before it was durable"
    );

    // Another writer meanwhile neither writes nor waits.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let turn = begin_scenario_turn(&mut agent, LoopBranch::DeployOp, None);
    let deadline = agent.turn_deadline();
    let landing = agent
        .checkpoint_egress_owner(&other, Some(binding.clone()), Some(deadline))
        .await;
    let again = end_scenario_turn(&mut agent, turn);
    assert!(
        matches!(
            landing,
            Ok(crate::bun::agent::discovery_ownership::Landing::InFlight)
        ),
        "{landing:?}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert!(agent.egress_write_in_flight(), "{again}");
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);

    // Outside a turn the next writer waits the write out, then writes.
    let landing = agent
        .checkpoint_egress_owner(&other, Some(binding.clone()), None)
        .await;
    assert!(
        matches!(
            landing,
            Ok(crate::bun::agent::discovery_ownership::Landing::Landed)
        ),
        "{landing:?}"
    );
    assert!(!agent.egress_write_in_flight());
    assert_eq!(agent.egress_bindings.get(&id), Some(&binding));
    assert_eq!(agent.egress_bindings.get(&other), Some(&binding));
    let durable = crate::bun::egress_owners::load(&records).unwrap();
    assert_eq!(durable.get(&id), Some(&binding));
    assert_eq!(durable.get(&other), Some(&binding));
}

/// An egress owners write that fails after its turn moved on fences every
/// later egress mutation, as an inline failure does, and its binding never
/// reaches memory.
#[tokio::test]
async fn an_egress_owners_write_that_fails_off_the_turn_fences_egress() {
    let (mut agent, _, _, _) = test_agent_with_grill();
    let root = tempfile::tempdir().unwrap();
    // A records directory under a regular file can't be created.
    let blocker = root.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    agent.set_records_dir(blocker.join("records"));
    let id = InstanceId("default__web-0".into());
    let binding = crate::bun::egress_owners::owned_test_binding(7, vec!["example.com:443".into()]);
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let turn = begin_scenario_turn(&mut agent, LoopBranch::RestartStep, None);
    let deadline = agent.turn_deadline();
    let landing = agent
        .checkpoint_egress_owner(&id, Some(binding.clone()), Some(deadline))
        .await;
    let restart = end_scenario_turn(&mut agent, turn);
    assert!(
        matches!(
            landing,
            Ok(crate::bun::agent::discovery_ownership::Landing::InFlight)
        ),
        "{landing:?}, {restart}"
    );
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);

    let next = agent
        .checkpoint_egress_owner(&id, Some(binding.clone()), None)
        .await;
    assert!(next.is_err(), "a write after a failed one went ahead");
    assert!(agent.egress_store_uncertain);
    assert!(!agent.egress_bindings.contains_key(&id));
}

// ---- a rollout's registration and an inline republication (#1336) ----------

/// `web` rolled out to one replacement, `web-g1-0`, on port 30999: what the
/// deploy worker hands the loop once every old instance has retired.
fn finalise_web(
    reply: oneshot::Sender<Result<(), BunError>>,
) -> crate::bun::agent::deploy_ops::DeployOp {
    let replacement = InstanceId("default__web-g1-0".into());
    crate::bun::agent::deploy_ops::DeployOp::FinaliseRollingDeploy {
        app_name: "web".into(),
        namespace: "default".into(),
        spec: Box::new(basic_config().app.remove("web").unwrap()),
        existing: vec![],
        new_ids: vec![replacement.clone()],
        new_ports: [(replacement.clone(), Some(30_999))].into_iter().collect(),
        new_ips: [(replacement, None)].into_iter().collect(),
        new_specs: Default::default(),
        now: Instant::now(),
        reply,
    }
}

/// A rollout registers its replacements by registering the service afresh
/// with only their backends, and it journalled that inline, after it had
/// already put them in the supervisor and `service_map` (#1336). It now
/// journals the rolled-out service first, waiting only until the turn's
/// deadline, and installs nothing until an ask finds it recorded.
#[tokio::test]
async fn a_rolling_registration_leaves_its_slow_journal_write_in_flight() {
    let (mut agent, _tx, _shutdown, _grill, _root) = journalling_web().await;
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let replacement = InstanceId("default__web-g1-0".into());
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = deploy_op_turn(&mut agent, "finalise_rolling_deploy", finalise_web).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "{turn}, {result:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "the registration's turn waited for its own journal write: {turn}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(
        routed_backends(&agent, &service),
        Some(vec!["default__web-0".to_string()])
    );
    assert!(agent.supervisor.get_instance(&replacement).is_none());

    // Before the landing, an ask writes nothing and installs nothing.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = deploy_op_turn(&mut agent, "finalise_rolling_deploy", finalise_web).await;
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert!(agent.supervisor.get_instance(&replacement).is_none());

    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(
        landed.detail,
        Some("finalise_rolling_deploy/landed"),
        "{landed}"
    );
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = deploy_op_turn(&mut agent, "finalise_rolling_deploy", finalise_web).await;
    result.unwrap();
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(
        routed_backends(&agent, &service),
        Some(vec![replacement.0.clone()])
    );
    assert_eq!(
        journalled_backends(&agent, &service),
        Some(vec![replacement.0.clone()])
    );
    assert!(agent.supervisor.get_instance(&replacement).is_some());
}

/// A rolling registration whose write fails after its turn moved on fences
/// discovery and installs nothing; the next ask journals it again.
#[tokio::test]
async fn a_rolling_registration_whose_write_fails_off_the_turn_installs_nothing() {
    let (mut agent, _tx, _shutdown, _grill, _root) = journalling_web().await;
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    let replacement = InstanceId("default__web-g1-0".into());
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let (DiscoveryOwnership::Ready(journal) | DiscoveryOwnership::Recovered(journal)) =
        &mut agent.discovery_ownership
    else {
        panic!("discovery ownership is not ready");
    };
    journal.fail_next_write();
    let (turn, result) = deploy_op_turn(&mut agent, "finalise_rolling_deploy", finalise_web).await;
    assert!(agent.discovery_write_in_flight(), "{turn}");
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    landing_turn(&mut agent).await;
    assert!(matches!(
        agent.discovery_ownership,
        DiscoveryOwnership::Uncertain
    ));
    assert_eq!(
        routed_backends(&agent, &service),
        Some(vec!["default__web-0".to_string()])
    );
    assert!(agent.supervisor.get_instance(&replacement).is_none());

    let (_, result) = deploy_op_turn(&mut agent, "finalise_rolling_deploy", finalise_web).await;
    result.unwrap();
    assert_eq!(
        journalled_backends(&agent, &service),
        Some(vec![replacement.0.clone()])
    );
    assert_eq!(routed_backends(&agent, &service), Some(vec![replacement.0]));
}

/// End to end: a rolling deploy whose every journal write outlasts its turn
/// still completes, and routes to exactly what the journal records.
#[tokio::test]
async fn a_rolling_deploy_completes_when_its_journal_writes_outlast_the_turn() {
    let (mut agent, tx, shutdown, _grill, _root) = journalling_web().await;
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let task = tokio::spawn(async move {
        agent.run().await;
        agent
    });
    let v2 = Config::parse("[app.web]\nimage = \"myapp:v2\"\nport = 8080\n").unwrap();
    expect_complete(&send_deploy(&tx, v2).await);
    shutdown.cancel();
    let agent = task.await.unwrap();
    let routed = routed_backends(&agent, &service).unwrap();
    assert_eq!(routed.len(), 1, "{routed:?}");
    assert_ne!(routed, vec!["default__web-0".to_string()]);
    assert_eq!(journalled_backends(&agent, &service), Some(routed));
}

/// A clustered agent holding the committed local reservation of
/// `default/remote`, journalled and withdrawn, ready to retire; and a local
/// ingress route the published view doesn't carry yet, so the retirement's
/// republication changes the view.
async fn clustered_reservation_to_retire() -> (
    BunAgent<MockGrill>,
    tempfile::TempDir,
    crate::onion::service_id::ServiceId,
) {
    let (mut agent, root, _catalog) = clustered_allocation_fixture().await;
    let mut entry = agent.service_map_tx.borrow().resolve_all()[0].clone();
    entry.backends.clear();
    agent.service_map = crate::onion::service_map::ServiceMap::from_snapshot(&[entry]).unwrap();
    let service = crate::onion::service_id::ServiceId::new("default", "remote");
    agent
        .persist_discovery_publication(&service, &agent.service_map.clone())
        .await
        .unwrap();
    let route: crate::config::app::IngressSpec =
        toml::from_str("host = \"local.test\"\ntls = \"disabled\"").unwrap();
    agent
        .ingress_configs
        .insert(("default".into(), "local".into()), route);
    (agent, root, service)
}

/// A retirement on a cluster node republishes the consumer view inline,
/// before it retires the allocation. Each of that synchronisation's steps
/// waited at its start for the previous step's write, inline in the
/// retirement's turn (#1336): one turn took every write the view needed.
/// Now the synchronisation stops at a write in flight, carries on in the
/// loop's own turns, and the retirement asks again.
#[tokio::test]
async fn a_service_retirement_leaves_its_republication_write_in_flight() {
    let (mut agent, _root, service) = clustered_reservation_to_retire().await;
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = retire_service_turn(&mut agent, &service).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "the retirement's turn waited for its republication's journal writes: {turn}, {result:?}"
    );
    assert!(agent.discovery_write_in_flight(), "{turn}");
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert!(!agent.consumer_syncs.is_idle());

    // Before the landing, an ask writes nothing.
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = retire_service_turn(&mut agent, &service).await;
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);

    // The loop lands the write and takes the rest of the synchronisation.
    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    landing_turn(&mut agent).await;
    while !agent.consumer_syncs.is_idle() {
        one_consumer_sync_turn(&mut agent).await;
    }
    let (_, result) = retire_service_turn(&mut agent, &service).await;
    result.unwrap();
    assert_eq!(journalled_backends(&agent, &service), None);
    assert!(agent.consumer_owner().is_some_and(|owner| {
        owner.phase == crate::bun::consumer_owners::ConsumerPhase::Active
            && owner.publications.last().is_some_and(|publication| {
                publication
                    .ingress
                    .iter()
                    .any(|route| route.name == "local")
            })
    }));
}

/// Take startup discovery recovery's work in a health tick's turn.
async fn recovery_turn(
    agent: &mut BunAgent<MockGrill>,
) -> (crate::bun::loop_meter::TurnRecord, Result<(), BunError>) {
    let turn = begin_scenario_turn(agent, LoopBranch::HealthTick, None);
    let result = agent.finish_discovery_recovery().await;
    (end_scenario_turn(agent, turn), result)
}

/// Startup discovery recovery publishes adopted backends once the last
/// startup retirement has gone, on a health tick, and it waited inline for
/// any write in flight (#1336). In a turn it now answers `StillRunning`,
/// and the next tick tries again.
#[tokio::test]
async fn startup_discovery_recovery_waits_for_no_write_in_flight() {
    let (mut agent, _root, _id, _service) = web_with_a_discovery_write_in_flight().await;
    let (turn, result) = recovery_turn(&mut agent).await;
    assert!(
        agent.discovery_write_in_flight(),
        "recovery's turn waited for a write in flight: {turn}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
}

/// A recovered agent whose adopted `web` replica has since moved to a new
/// address, with its backend not yet published, so recovery has a backend
/// to journal.
async fn recovered_web_to_publish() -> (
    BunAgent<MockGrill>,
    tempfile::TempDir,
    InstanceId,
    crate::onion::service_id::ServiceId,
) {
    let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
    grill.set_adopt_result(&reference.instance_id, true);
    agent
        .recover_discovery_ownership(&root.path().join("discovery"))
        .await
        .unwrap();
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
    let service = crate::onion::service_id::ServiceId::new("default", "web");
    grill.set_container_ip("10.0.2.6".parse().unwrap());
    agent
        .service_map
        .remove_backend(&service, &reference.instance_id.0)
        .unwrap();
    (agent, root, reference.instance_id, service)
}

/// The address `service_map` routes `id` to, if it routes to `id` at all.
fn routed_address(
    agent: &BunAgent<MockGrill>,
    service: &crate::onion::service_id::ServiceId,
    id: &InstanceId,
) -> Option<std::net::Ipv4Addr> {
    agent
        .service_map
        .resolve(service)?
        .backends
        .iter()
        .find(|backend| backend.instance_id == id.0)
        .map(|backend| backend.node_ip)
}

/// Recovery's own journal write waited inline too. It now waits only until
/// the turn's deadline; the instance and `service_map` stay as they were
/// until a tick finds the backend recorded.
#[tokio::test]
async fn startup_discovery_recovery_leaves_its_slow_journal_write_in_flight() {
    let (mut agent, _root, id, service) = recovered_web_to_publish().await;
    let new_address: std::net::Ipv4Addr = "10.0.2.6".parse().unwrap();
    agent
        .loop_stalls
        .set(LoopStall::Persist, STALLED_JOURNAL_WRITE);
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (turn, result) = recovery_turn(&mut agent).await;
    assert_eq!(
        agent.loop_stalls.reached(LoopStall::Persist),
        reached + 1,
        "{turn}, {result:?}"
    );
    assert!(
        agent.discovery_write_in_flight(),
        "recovery's turn waited for its own journal write: {turn}"
    );
    assert!(
        matches!(result, Err(BunError::StillRunning { .. })),
        "{result:?}"
    );
    assert_eq!(routed_address(&agent, &service, &id), None);
    assert_ne!(
        agent.supervisor.get_instance(&id).unwrap().container_ip,
        Some(new_address)
    );

    agent
        .loop_stalls
        .set(LoopStall::Persist, std::time::Duration::ZERO);
    let landed = landing_turn(&mut agent).await;
    assert_eq!(landed.detail, Some("discovery_recovery/landed"), "{landed}");
    let reached = agent.loop_stalls.reached(LoopStall::Persist);
    let (_, result) = recovery_turn(&mut agent).await;
    result.unwrap();
    assert_eq!(agent.loop_stalls.reached(LoopStall::Persist), reached);
    assert_eq!(routed_address(&agent, &service, &id), Some(new_address));
    assert_eq!(
        agent.supervisor.get_instance(&id).unwrap().container_ip,
        Some(new_address)
    );
}
