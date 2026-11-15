/// Task-array benchmarks (million-jobs plan, M4.2).
///
/// Everything here runs in one process with no cluster, so the numbers
/// predict the headline before any VM exists:
/// - range-set operations the leader does per chunk;
/// - the leader's whole control-plane cost for a 1M-task array (no
///   processes), with 3 and 200 nodes, and the JSON snapshot of it,
///   encoded 64 times an iteration;
/// - one leader tick (plan every array's writes, batch them, apply the
///   entry) with 64 concurrent arrays on 3 nodes, and with one array of
///   one-task chunks on 10 nodes, the shape that splits state the most;
/// - the node pool's overhead per task with a zero-cost fake runner (the
///   ceiling a real process can't beat);
/// - the fork/exec floor of this machine: `/usr/bin/true` through the
///   real process runner at concurrency 1, 4 and 16;
/// - a worker's disk cost per task: 1M outcomes through the real ledger and
///   result index, broken down per file, failing above
///   `ACTIVE_STORAGE_BYTES_PER_TASK` while the run is active, then the
///   compacted results of the finished run, failing above
///   `STORAGE_BYTES_PER_TASK`;
/// - the same 64k-task writes on tmpfs (Linux), where fsync costs nothing,
///   so the ledger and index code is measured without the runner's disk.
///
/// Every benchmark reports tasks (or elements) per second.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use reliaburger::bun::task_array_leader::{TickInputs, plan_tick, tick_entries};
use reliaburger::bun::task_array_node::{
    ACTIVE_STORAGE_BYTES_PER_TASK, ArrayProgress, NodeArrayCounters, NodeSyncResponse,
    STORAGE_BYTES_PER_TASK,
};
use reliaburger::bun::task_executor::{
    AttemptOutcome, ChunkWork, FakeRunner, PoolConfig, ProcessRunner, TaskFinal, TaskPool,
    TaskRecord, TaskRunner,
};
use reliaburger::bun::task_ledger::Ledger;
use reliaburger::bun::task_result_index;
use reliaburger::meat::NodeId;
use reliaburger::meat::index_set::IndexRangeSet;
use reliaburger::meat::task_array::{ChunkId, TaskArraySpec};
use reliaburger::meat::task_array_state::{ChunkResult, NodeSlots, TaskArrayState, plan_grants};
use reliaburger::meat::task_array_store::{TaskArrayWrite, TaskArrays};
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
                    retry_causes: Default::default(),
                };
                let _ = state.complete(&holder, &result);
            }
        }
        for (holder, chunks) in plan_grants(&state, &slots, true) {
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
    // One encode of the finished state takes about 0.1 ms, close enough to
    // timer and scheduling noise that it was the noisiest benchmark in the
    // gate (#872). So each iteration encodes it `JSON_ENCODES` times, and the
    // throughput still reports encodes per second. The name carries the
    // count: a different workload must not be compared with main's baseline
    // for the old one.
    const JSON_ENCODES: usize = 64;
    let finished = simulate_leader(3);
    group.throughput(Throughput::Elements(JSON_ENCODES as u64));
    group.bench_function("json_encode_1m_x64", |b| {
        b.iter(|| {
            (0..JSON_ENCODES)
                .map(|_| serde_json::to_vec(&finished).map_or(0, |bytes| bytes.len()))
                .sum::<usize>()
        })
    });
    group.finish();
}

/// `count` registered arrays of `tasks` tasks in `chunk_size`-task chunks.
fn running_arrays(count: u64, tasks: u32, chunk_size: u32) -> TaskArrays {
    let mut arrays = TaskArrays::default();
    for id in 1..=count {
        let register = TaskArrayWrite::Register {
            name: format!("bench-{id}"),
            namespace: format!("team-{}", id % 4),
            template: Box::new(
                toml::from_str("runtime = 'process'\nexec = '/usr/bin/true'").unwrap(),
            ),
            spec: TaskArraySpec {
                chunk_size,
                ..TaskArraySpec::with_count(tasks)
            },
            submitted_at_epoch_secs: 1,
        };
        arrays.apply(&register, || id).unwrap();
    }
    arrays
}

/// Every node's answer: 16 free slots per array, and every chunk it holds
/// finished except the lowest, so completions land out of order.
fn answers(arrays: &TaskArrays, nodes: usize) -> Vec<(NodeId, NodeSyncResponse)> {
    (0..nodes)
        .map(|i| {
            let node = NodeId::new(format!("n{i:02}"));
            let response = NodeSyncResponse {
                arrays: arrays
                    .active()
                    .map(|(batch_id, record)| ArrayProgress {
                        batch_id,
                        slots: 16,
                        refused: None,
                        unrunnable: false,
                        finished: record
                            .state
                            .held_by(&node)
                            .into_iter()
                            .flat_map(|held| held.iter().skip(1))
                            .map(|chunk| {
                                let range = record.state.spec.chunk_range(ChunkId(chunk)).unwrap();
                                let mut result = ChunkResult {
                                    duration_counts: [0; 16],
                                    chunk: ChunkId(chunk),
                                    attempt: record.state.attempt_of(ChunkId(chunk)),
                                    succeeded: range.end() - range.start() + 1,
                                    failed_count: 0,
                                    failed_indices: IndexRangeSet::new(),
                                    not_run: 0,
                                    retried: 0,
                                    retry_causes: Default::default(),
                                };
                                result.duration_counts[0] = u64::from(result.succeeded);
                                result
                            })
                            .collect(),
                        counters: NodeArrayCounters::default(),
                    })
                    .collect(),
            };
            (node, response)
        })
        .collect()
}

/// One leader tick against `arrays`: plan, batch, apply. Returns the
/// number of Raft entries it wrote.
fn tick(arrays: &mut TaskArrays, answers: &[(NodeId, NodeSyncResponse)]) -> usize {
    let now = Instant::now();
    let heard = HashMap::new();
    let inputs = TickInputs {
        answers,
        last_heard: &heard,
        since: now,
        now,
        silence: Duration::from_secs(30),
        now_epoch_secs: 2,
        eligible: answers.iter().map(|(node, _)| node.clone()).collect(),
    };
    let entries = tick_entries(plan_tick(arrays, None, &inputs));
    for entry in &entries {
        let _ = arrays.apply(entry, || 0);
    }
    entries.len()
}

/// Run `ticks` ticks so the arrays are mid-run, with nodes holding work.
fn warmed(mut arrays: TaskArrays, nodes: usize, ticks: usize) -> TaskArrays {
    for _ in 0..ticks {
        let answers = answers(&arrays, nodes);
        tick(&mut arrays, &answers);
    }
    arrays
}

fn bench_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("leader_tick");
    group.sample_size(20);
    let shapes = [
        (
            "64_arrays_3_nodes",
            warmed(running_arrays(64, 100_000, 1024), 3, 2),
            3,
        ),
        (
            "one_task_chunks_10_nodes",
            warmed(running_arrays(1, 1 << 16, 1), 10, 100),
            10,
        ),
    ];
    for (name, arrays, nodes) in shapes {
        let answers = answers(&arrays, nodes);
        let mut probe = arrays.clone();
        let entries = tick(&mut probe, &answers);
        assert_eq!(entries, 1, "{name}: the arrays must still be running");
        let bytes = serde_json::to_vec(&arrays).map_or(0, |bytes| bytes.len());
        eprintln!(
            "leader_tick/{name}: {entries} Raft entry per tick, {bytes} bytes of array state"
        );
        group.throughput(Throughput::Elements(1));
        group.bench_function(name, |b| {
            b.iter_batched(
                || arrays.clone(),
                |mut arrays| tick(&mut arrays, &answers),
                criterion::BatchSize::LargeInput,
            )
        });
    }
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
        replay_unknown: true,
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

/// Write `tasks` outcomes (1% failed) the way a worker does: blocks of 4,096
/// records, each fsynced to the ledger and then its result index. Returns
/// the allocated bytes of the ledger and the index.
fn write_outcomes(directory: &std::path::Path, tasks: u32) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let path = directory.join("ledger");
    let mut ledger = Ledger::open(&path).unwrap();
    for start in (0..tasks).step_by(4096) {
        let records: Vec<TaskRecord> = (start..(start + 4096).min(tasks))
            .map(|index| TaskRecord {
                grant_attempt: 1,
                index,
                attempts: 1,
                outcome: if index % 100 == 0 {
                    TaskFinal::Failed
                } else {
                    TaskFinal::Succeeded
                },
                exit_code: Some(i32::from(index % 100 == 0)),
                run_ms: 5,
                output: None,
                retry_causes: Default::default(),
            })
            .collect();
        ledger.append(&records);
        ledger.flush().unwrap();
    }
    let allocated = |path: &std::path::Path| std::fs::metadata(path).unwrap().blocks() * 512;
    (allocated(&path), allocated(&path.with_extension("index")))
}

fn bench_storage(c: &mut Criterion) {
    let directory = tempfile::tempdir().unwrap();
    let (ledger, index) = write_outcomes(directory.path(), MILLION);
    let per_task = (ledger + index) as f64 / f64::from(MILLION);
    eprintln!(
        "storage per 1M tasks while the run is active: ledger {:.1} MiB ({:.1} B/task), result index {:.1} MiB ({:.1} B/task), total {per_task:.1} B/task",
        ledger as f64 / 1048576.0,
        ledger as f64 / f64::from(MILLION),
        index as f64 / 1048576.0,
        index as f64 / f64::from(MILLION),
    );
    assert!(
        ledger + index <= u64::from(MILLION) * ACTIVE_STORAGE_BYTES_PER_TASK,
        "{per_task:.1} bytes per task is over the {ACTIVE_STORAGE_BYTES_PER_TASK}-byte bound"
    );
    // The run finishes: the node compacts both files into its results.
    let results = directory.path().join(task_result_index::RESULTS_FILE);
    task_result_index::compact(&directory.path().join("ledger"), &results)
        .unwrap()
        .unwrap();
    let kept = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&results).unwrap().blocks() * 512
    };
    let per_task = kept as f64 / f64::from(MILLION);
    eprintln!(
        "storage per 1M tasks once the run finishes: results {:.1} MiB ({per_task:.1} B/task)",
        kept as f64 / 1048576.0,
    );
    assert!(
        kept <= u64::from(MILLION) * STORAGE_BYTES_PER_TASK,
        "{per_task:.1} bytes per finished task is over the {STORAGE_BYTES_PER_TASK}-byte bound"
    );

    let mut group = c.benchmark_group("storage");
    group.sample_size(10);
    group.throughput(Throughput::Elements(65_536));
    group.bench_function("ledger_and_index_64k", |b| {
        b.iter(|| {
            let directory = tempfile::tempdir().unwrap();
            write_outcomes(directory.path(), 65_536)
        })
    });
    // The on-disk benchmark is mostly its 30-odd fsyncs, so it measures the
    // runner's disk, whose speed varies several-fold within one CI job
    // (#1038). On tmpfs an fsync is free, which leaves the cost of encoding,
    // writing and indexing the records: what a change to the ledger can make
    // slower. CI gates on this one and only reports the on-disk one.
    let tmpfs = std::path::Path::new("/dev/shm");
    if tmpfs.is_dir() {
        group.bench_function("ledger_and_index_64k_tmpfs", |b| {
            b.iter(|| {
                let directory = tempfile::tempdir_in(tmpfs).unwrap();
                write_outcomes(directory.path(), 65_536)
            })
        });
    } else {
        eprintln!("storage/ledger_and_index_64k_tmpfs: skipped, no tmpfs at /dev/shm");
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_index_set,
    bench_leader,
    bench_tick,
    bench_executor,
    bench_storage
);
criterion_main!(benches);
