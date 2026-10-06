//! A million tasks, in process (million-jobs plan, M4.1).
//!
//! The portable suite runs 100,000 tasks, which takes a few seconds in a
//! debug build. Set `RELIABURGER_TASK_ARRAY_TASKS=1000000` to run the full
//! million (about 30 s in a debug build on an M-series laptop).
//!
//! Three simulated nodes, each a real `TaskPool` with a `FakeRunner` and
//! a real on-disk ledger, driven by the leader's real `TaskArrayState`
//! and grant policy. One task in a hundred fails its first attempt and
//! succeeds on retry, and one node is lost a third of the way through.
//! Nothing is wired into Raft or HTTP yet; the leader loop here stands in
//! for the once-a-second sync, and each tick that changes the state
//! counts as the one Raft entry it will become.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reliaburger::bun::task_executor::{
    AttemptOutcome, ChunkWork, FakeRunner, PoolConfig, TaskPool,
};
use reliaburger::bun::task_ledger::{self, GroupCommit, Ledger, LedgerHandle};
use reliaburger::meat::NodeId;
use reliaburger::meat::index_set::IndexRangeSet;
use reliaburger::meat::task_array::{ChunkId, TaskArraySpec};
use reliaburger::meat::task_array_state::{
    ChunkResult, NodeSlots, TaskArrayState, TaskArrayStatus, plan_grants,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const DEFAULT_TASKS: u32 = 100_000;
const SLOTS: u32 = 32;
const TICK: Duration = Duration::from_millis(20);

struct SimulatedNode {
    id: NodeId,
    pool: Arc<TaskPool<FakeRunner>>,
    ledger: LedgerHandle,
    ledger_path: PathBuf,
    cancel: CancellationToken,
    alive: bool,
}

fn task_count() -> u32 {
    std::env::var("RELIABURGER_TASK_ARRAY_TASKS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_TASKS)
}

fn fails_first_attempt(index: u32) -> bool {
    index % 100 == 42
}

fn start_node(name: &str, directory: &std::path::Path) -> SimulatedNode {
    let runner = Arc::new(FakeRunner::new(Duration::ZERO, |task| {
        let fails = fails_first_attempt(task.index) && task.attempt == 1;
        AttemptOutcome::Exited {
            code: i32::from(fails),
        }
    }));
    let config = PoolConfig {
        concurrency: SLOTS,
        backoff_base: Duration::from_millis(1),
        backoff_cap: Duration::from_millis(2),
    };
    let ledger_path = directory.join(format!("{name}.ledger"));
    let ledger = Ledger::open(&ledger_path).unwrap();
    let (handle, _writer) = task_ledger::spawn_writer(ledger, GroupCommit::default());
    SimulatedNode {
        id: NodeId::new(name),
        pool: Arc::new(TaskPool::new(runner, config)),
        ledger: handle,
        ledger_path,
        cancel: CancellationToken::new(),
        alive: true,
    }
}

/// Start every chunk in `chunks` on `node`; each one reports back through
/// `reports` once its records are durable in the node's ledger.
fn dispatch(
    node: &SimulatedNode,
    state: &TaskArrayState,
    chunks: &IndexRangeSet,
    reports: &mpsc::UnboundedSender<(NodeId, ChunkResult)>,
) {
    for chunk in chunks.iter() {
        let work = ChunkWork {
            template: None,
            batch_id: 1,
            spec: state.spec.clone(),
            chunk: ChunkId(chunk),
            grant_attempt: state.attempt_of(ChunkId(chunk)),
            program: PathBuf::from("/unused"),
            args: vec!["{index}".to_string()],
            env: Vec::new(),
        };
        let pool = Arc::clone(&node.pool);
        let ledger = node.ledger.clone();
        let cancel = node.cancel.clone();
        let reports = reports.clone();
        let id = node.id.clone();
        tokio::spawn(async move {
            let outcome = pool.run_chunk(&work, &cancel).await;
            if ledger.append(outcome.records).await.is_ok() {
                let _ = reports.send((id, outcome.result));
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_array_finishes_with_retries_and_a_lost_node() {
    let tasks = task_count();
    let directory = tempfile::tempdir().unwrap();
    let mut nodes: BTreeMap<NodeId, SimulatedNode> = ["n1", "n2", "n3"]
        .iter()
        .map(|name| {
            let node = start_node(name, directory.path());
            (node.id.clone(), node)
        })
        .collect();
    let mut state = TaskArrayState::new(TaskArraySpec::with_count(tasks), 1).unwrap();
    let (reports, mut received) = mpsc::unbounded_channel();
    let lose_after = u64::from(state.spec.chunk_count() / 3);

    let started = Instant::now();
    let deadline = started + Duration::from_secs(240);
    let mut ticks = 0u64;
    let mut entries = 0u64;
    let mut refused = 0u64;
    let mut changed = false;
    let mut lost = false;
    let mut interval = tokio::time::interval(TICK);
    while !state.status().is_terminal() {
        assert!(Instant::now() < deadline, "{:?}", state.summary());
        tokio::select! {
            Some((node, result)) = received.recv() => {
                match state.complete(&node, &result) {
                    Ok(_) => changed = true,
                    Err(_) => refused += 1,
                }
            }
            _ = interval.tick() => {
                ticks += 1;
                // Lose n3 a third of the way through, while it holds chunks.
                let lose = NodeId::new("n3");
                if !lost && state.summary().chunks_done >= lose_after && state.held_by(&lose).is_some() {
                    lost = true;
                    let node = nodes.get_mut(&lose).unwrap();
                    node.alive = false;
                    node.cancel.cancel();
                    state.requeue_node(&lose);
                    changed = true;
                }
                let slots: Vec<NodeSlots> = nodes
                    .values()
                    .filter(|n| n.alive)
                    .map(|n| NodeSlots { node: n.id.clone(), slots: SLOTS })
                    .collect();
                for (holder, chunks) in plan_grants(&state, &slots) {
                    state.grant(&holder, &chunks).unwrap();
                    dispatch(&nodes[&holder], &state, &chunks, &reports);
                    changed = true;
                }
                if changed {
                    entries += 1;
                    changed = false;
                }
            }
        }
    }
    let elapsed = started.elapsed();

    let summary = state.summary();
    assert_eq!(summary.status, TaskArrayStatus::Succeeded, "{summary:?}");
    assert_eq!(summary.succeeded, u64::from(tasks));
    assert_eq!(summary.failed, 0);
    // Every retry that counted came from an accepted chunk: exactly one
    // per index ending in 42, however often a re-granted chunk re-ran.
    assert_eq!(summary.retried, u64::from(tasks / 100));
    assert!(lost, "the node loss never happened");
    assert!(refused >= 1, "the lost node's late reports must be refused");

    // The control plane wrote at most one entry per tick, a few hundred
    // for a million tasks, and holds a few kilobytes.
    assert!(entries <= ticks, "{entries} entries in {ticks} ticks");
    assert!(entries < 2_000, "{entries} entries");
    let bytes = serde_json::to_vec(&state).unwrap().len();
    assert!(bytes <= 256 * 1024, "state is {bytes} bytes");

    // Between them, the ledgers hold a terminal record for every index.
    let mut finished = IndexRangeSet::new();
    for node in nodes.values() {
        let replayed = task_ledger::replay(&node.ledger_path).unwrap();
        finished.extend_from(&replayed.finished);
    }
    assert_eq!(finished, IndexRangeSet::from_range(0..=tasks - 1));

    eprintln!(
        "task_array_million: {tasks} tasks in {elapsed:.2?} ({:.0}/s), {entries} leader entries, \
         {refused} stale reports refused, state {bytes} bytes",
        f64::from(tasks) / elapsed.as_secs_f64()
    );
}
