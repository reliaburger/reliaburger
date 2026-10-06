/// Task-array benchmarks (million-jobs plan, M4.2).
///
/// Everything here runs in one process with no cluster, so the numbers
/// predict the headline before any VM exists:
/// - range-set operations the leader does per chunk;
/// - the leader's whole control-plane cost for a 1M-task array (no
///   processes), with 3 and 200 nodes, and the JSON snapshot of it;
/// - the node pool's overhead per task with a zero-cost fake runner (the
///   ceiling a real process can't beat);
/// - the fork/exec floor of this machine: `/usr/bin/true` through the
///   real process runner at concurrency 1, 4 and 16.
///
/// Every benchmark reports tasks (or elements) per second.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reliaburger::bun::task_executor::{
    AttemptOutcome, ChunkWork, FakeRunner, PoolConfig, ProcessRunner, TaskPool, TaskRunner,
};
use reliaburger::meat::NodeId;
use reliaburger::meat::index_set::IndexRangeSet;
use reliaburger::meat::task_array::{ChunkId, TaskArraySpec};
use reliaburger::meat::task_array_state::{ChunkResult, NodeSlots, TaskArrayState, plan_grants};
use tokio_util::sync::CancellationToken;

const MILLION: u32 = 1_000_000;

fn bench_index_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_set");
    group.throughput(Throughput::Elements(u64::from(MILLION)));
    group.bench_function("insert_sequential_1m", |b| {
        b.iter(|| {
            let mut set = IndexRangeSet::new();
            for index in 0..MILLION {
                set.insert(index);
            }
            set
        });
    });
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("insert_sparse_10k", |b| {
        b.iter(|| {
            let mut set = IndexRangeSet::new();
            for index in (0..MILLION).step_by(100) {
                set.insert(index);
            }
            set
        });
    });
    group.finish();
}

/// Run a whole 1M-task array through the leader's state machine: every
/// round, each node finishes what it holds (1% of indices failing) and is
/// topped up. Returns the final state.
fn simulate_leader(nodes: usize) -> TaskArrayState {
    let mut state = TaskArrayState::new(TaskArraySpec::with_count(MILLION), 1).unwrap();
    let slots: Vec<NodeSlots> = (0..nodes)
        .map(|i| NodeSlots {
            node: NodeId::new(format!("n{i}")),
            slots: 16,
        })
        .collect();
    while !state.status().is_terminal() {
        let holders: Vec<NodeId> = state.holders().cloned().collect();
        for holder in holders {
            let held: Vec<u32> = state
                .held_by(&holder)
                .map(|set| set.iter().collect())
                .unwrap_or_default();
            for chunk in held {
                let Some(range) = state.spec.chunk_range(ChunkId(chunk)) else {
                    continue;
                };
                let mut failed = IndexRangeSet::new();
                for index in range.clone().filter(|i| i % 100 == 42) {
                    failed.insert(index);
                }
                let tasks = range.end() - range.start() + 1;
                let result = ChunkResult {
                    duration_counts: [0; 16],
                    chunk: ChunkId(chunk),
                    attempt: state.attempt_of(ChunkId(chunk)),
                    succeeded: tasks - failed.len() as u32,
                    failed_count: failed.len() as u32,
                    failed_indices: failed,
                    not_run: 0,
                    retried: 0,
                };
                let _ = state.complete(&holder, &result);
            }
        }
        for (holder, chunks) in plan_grants(&state, &slots) {
            let _ = state.grant(&holder, &chunks);
        }
    }
    state
}

fn bench_leader(c: &mut Criterion) {
    let mut group = c.benchmark_group("state");
    group.sample_size(10);
    group.throughput(Throughput::Elements(u64::from(MILLION)));
    for nodes in [3usize, 200] {
        group.bench_with_input(
            BenchmarkId::new("plan_and_complete_1m", nodes),
            &nodes,
            |b, &nodes| b.iter(|| simulate_leader(nodes)),
        );
    }
    let finished = simulate_leader(3);
    group.throughput(Throughput::Elements(1));
    group.bench_function("json_encode_1m", |b| {
        b.iter(|| serde_json::to_vec(&finished).map(|bytes| bytes.len()))
    });
    group.finish();
}

fn chunk_work(count: u32, chunk: u32, program: &str, args: &[&str]) -> ChunkWork {
    ChunkWork {
        template: None,
        batch_id: 1,
        spec: TaskArraySpec {
            chunk_size: count,
            ..TaskArraySpec::with_count(count)
        },
        chunk: ChunkId(chunk),
        grant_attempt: 1,
        program: PathBuf::from(program),
        args: args.iter().map(|a| a.to_string()).collect(),
        env: Vec::new(),
    }
}

fn run_pool<R: TaskRunner>(
    runtime: &tokio::runtime::Runtime,
    runner: Arc<R>,
    concurrency: u32,
    work: &ChunkWork,
) -> u32 {
    runtime.block_on(async {
        let pool = TaskPool::new(runner, PoolConfig::with_concurrency(concurrency));
        let outcome = pool.run_chunk(work, &CancellationToken::new()).await;
        outcome.result.succeeded
    })
}

fn bench_executor(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("executor");
    group.sample_size(10);

    let tasks = 65_536;
    let fake = Arc::new(FakeRunner::new(Duration::ZERO, |_| {
        AttemptOutcome::Exited { code: 0 }
    }));
    let work = chunk_work(tasks, 0, "/unused", &["{index}"]);
    group.throughput(Throughput::Elements(u64::from(tasks)));
    group.bench_function("fake_64k", |b| {
        b.iter(|| run_pool(&runtime, Arc::clone(&fake), 64, &work))
    });

    let spawned = 256;
    let work = chunk_work(spawned, 0, "/usr/bin/true", &[]);
    group.throughput(Throughput::Elements(u64::from(spawned)));
    for concurrency in [1u32, 4, 16] {
        group.bench_with_input(
            BenchmarkId::new("process_true_256", concurrency),
            &concurrency,
            |b, &concurrency| {
                b.iter(|| {
                    run_pool(
                        &runtime,
                        Arc::new(ProcessRunner::default()),
                        concurrency,
                        &work,
                    )
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_index_set, bench_leader, bench_executor);
criterion_main!(benches);
