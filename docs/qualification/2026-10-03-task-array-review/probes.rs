//! PR #266 review probes, 3 October 2026.
//! These assert observed draft behaviour, not desired behaviour. A pass confirms
//! a defect or limitation; convert them to regressions asserting desired behaviour
//! when fixing the draft. They stay outside the automatically run test suite.
//!
//! Reproduce on this PR head:
//! cp docs/qualification/2026-10-03-task-array-review/probes.rs tests/pr266_review_probes.rs
//! cargo test --test pr266_review_probes -- --nocapture
//! rm tests/pr266_review_probes.rs
use reliaburger::bun::task_array_node::{
    ArrayAssignment, HeldChunk, NodeRunner, NodeSyncRequest, TaskArrayNode, TaskArrayNodeConfig,
};
use reliaburger::bun::task_executor::{AttemptOutcome, FakeRunner, TaskFinal, TaskRecord};
use reliaburger::bun::task_ledger::{self, GroupCommit, Ledger};
use reliaburger::config::process_workloads::ProcessWorkloadsConfig;
use reliaburger::meat::task_array::{ChunkId, TaskArraySpec};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn record(index: u32) -> TaskRecord {
    TaskRecord {
        index,
        attempts: 1,
        outcome: TaskFinal::Succeeded,
        exit_code: Some(0),
        run_ms: 1,
        output: None,
    }
}
fn node(root: &Path, delay: Duration, concurrency: u32) -> TaskArrayNode {
    TaskArrayNode::new(
        TaskArrayNodeConfig {
            root: root.into(),
            policy: ProcessWorkloadsConfig {
                allowed_binaries: vec!["/review/task".into()],
                mount_isolation: false,
                ..Default::default()
            },
            default_concurrency: concurrency,
            backoff: (Duration::from_millis(1), Duration::from_millis(2)),
            group_commit: GroupCommit {
                interval: Duration::from_millis(10),
                max_records: 4096,
            },
        },
        NodeRunner::Fake(FakeRunner::new(delay, |_| AttemptOutcome::Exited {
            code: 0,
        })),
    )
}
fn assignment(id: u64, count: u32, attempt: u8) -> ArrayAssignment {
    ArrayAssignment {
        batch_id: id,
        spec: TaskArraySpec {
            chunk_size: count,
            ..TaskArraySpec::with_count(count)
        },
        program: PathBuf::from("/review/task"),
        args: vec![],
        env: vec![],
        held: vec![HeldChunk {
            chunk: ChunkId(0),
            attempt,
        }],
        stopping: false,
    }
}
fn request(arrays: Vec<ArrayAssignment>) -> NodeSyncRequest {
    NodeSyncRequest {
        known: arrays.iter().map(|a| a.batch_id).collect(),
        arrays,
    }
}
async fn until_finished(
    node: &TaskArrayNode,
    req: &NodeSyncRequest,
) -> reliaburger::bun::task_array_node::ArrayProgress {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let progress = node.sync(req).await.arrays.remove(0);
        if !progress.finished.is_empty() {
            return progress;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
#[test]
fn appending_after_a_torn_tail_poisoned_the_ledger() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("ledger");
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.append(&[record(0)]);
        ledger.flush().unwrap();
    }
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&1u32.to_le_bytes()).unwrap();
    file.write_all(&0u32.to_le_bytes()).unwrap();
    file.write_all(&[0, 0, 0, 0]).unwrap();
    file.sync_all().unwrap();
    assert!(task_ledger::replay(&path).unwrap().torn_tail);
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger.append(&[record(1)]);
        ledger.flush().unwrap();
    }
    let replay = task_ledger::replay(&path);
    eprintln!("after append following torn tail: {replay:?}");
    assert!(replay.is_err(), "draft unexpectedly repaired its torn tail");
}
#[tokio::test]
async fn two_arrays_each_get_the_full_node_capacity() {
    let root = tempfile::tempdir().unwrap();
    let node = node(root.path(), Duration::from_secs(1), 2);
    let req = request(vec![assignment(1, 10, 1), assignment(2, 10, 1)]);
    node.sync(&req).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let progress = node.sync(&req).await;
    let running: u64 = progress.arrays.iter().map(|a| a.counters.running).sum();
    eprintln!("node configured for 2 slots, two arrays have {running} running attempts");
    assert_eq!(running, 4);
    let stopped = request(
        req.arrays
            .into_iter()
            .map(|mut a| {
                a.stopping = true;
                a
            })
            .collect(),
    );
    node.sync(&stopped).await;
}
#[tokio::test]
async fn completed_tasks_have_no_ledger_records_until_the_chunk_finishes() {
    let root = tempfile::tempdir().unwrap();
    let node = node(root.path(), Duration::from_millis(10), 1);
    let req = request(vec![assignment(1, 100, 1)]);
    node.sync(&req).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let progress = node.sync(&req).await.arrays.remove(0);
    let replay = task_ledger::replay(&node.array_dir(1).join("ledger")).unwrap();
    eprintln!(
        "completed {}, durable {} (150ms after start, 10ms commit interval)",
        progress.counters.succeeded,
        replay.records.len()
    );
    assert!(progress.counters.succeeded > 0 && progress.counters.succeeded < 100);
    assert!(replay.records.is_empty());
    let mut stopped = req;
    stopped.arrays[0].stopping = true;
    until_finished(&node, &stopped).await;
}
#[tokio::test]
async fn a_delayed_assignment_can_roll_a_worker_back_to_an_older_attempt() {
    let root = tempfile::tempdir().unwrap();
    let node = Arc::new(node(root.path(), Duration::ZERO, 1));
    until_finished(&node, &request(vec![assignment(1, 10, 2)])).await;
    let stale = until_finished(&node, &request(vec![assignment(1, 10, 1)])).await;
    eprintln!(
        "after stale sync: grant attempt {}, task attempts started {}",
        stale.finished[0].attempt, stale.counters.attempts_started
    );
    assert_eq!(stale.finished[0].attempt, 1);
    assert_eq!(stale.counters.attempts_started, 20);
}

#[test]
fn failed_only_results_keep_a_failure_from_a_superseded_run() {
    use reliaburger::bun::task_array_api::merge_rows;
    use reliaburger::bun::task_array_node::TaskResultRow;
    use reliaburger::meat::NodeId;
    use reliaburger::meat::index_set::IndexRangeSet;
    use reliaburger::meat::task_array_state::{ChunkResult, TaskArrayState};
    use reliaburger::meat::task_array_store::TaskArrayRecord;
    let mut state = TaskArrayState::new(TaskArraySpec::with_count(1), 1).unwrap();
    let holder = NodeId::new("new-holder");
    state
        .grant(&holder, &IndexRangeSet::from_range(0..=0))
        .unwrap();
    state
        .complete(
            &holder,
            &ChunkResult {
                chunk: ChunkId(0),
                attempt: 1,
                succeeded: 1,
                failed_indices: IndexRangeSet::new(),
                not_run: 0,
                retried: 0,
            },
        )
        .unwrap();
    let template = serde_json::from_value(serde_json::json!({ "exec": "/review/task" })).unwrap();
    let record = TaskArrayRecord {
        name: "review".into(),
        namespace: "default".into(),
        template,
        state,
    };
    // `results?failed=true` has already filtered the new holder's success out.
    let stale = TaskResultRow {
        index: 0,
        attempts: 1,
        succeeded: false,
        exit_code: Some(1),
        run_ms: 1,
    };
    let (rows, _) = merge_rows(&record, vec![vec![stale]], 100);
    eprintln!(
        "leader succeeded={}, failed={}; failed-only result rows={rows:?}",
        record.state.summary().succeeded,
        record.state.summary().failed
    );
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].succeeded);
}
