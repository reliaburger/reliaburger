//! Recovery guarantees for delegated task execution.
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
        grant_attempt: 1,
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
fn assignment(id: u64, count: u32, attempt: u64) -> ArrayAssignment {
    ArrayAssignment {
        template: None,
        batch_id: id,
        resources: reliaburger::meat::Resources::new(1000, 64 << 20, 0),
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
        replay_unknown: true,
    }
}
fn request(arrays: Vec<ArrayAssignment>) -> NodeSyncRequest {
    NodeSyncRequest {
        version: Default::default(),
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
fn appending_after_a_torn_tail_repairs_the_ledger() {
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
    let replay = task_ledger::replay(&path).unwrap();
    eprintln!("after append following torn tail: {replay:?}");
    assert!(!replay.torn_tail);
    assert_eq!(replay.records, vec![record(0), record(1)]);
}
#[tokio::test]
async fn arrays_share_the_node_capacity() {
    let root = tempfile::tempdir().unwrap();
    let node = node(root.path(), Duration::from_secs(1), 2);
    let req = request(vec![assignment(1, 10, 1), assignment(2, 10, 1)]);
    node.sync(&req).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let progress = node.sync(&req).await;
    let running: u64 = progress.arrays.iter().map(|a| a.counters.running).sum();
    eprintln!("node configured for 2 slots, two arrays have {running} running attempts");
    assert_eq!(running, 2);
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
async fn completed_tasks_are_durable_before_the_chunk_finishes() {
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
    assert!(!replay.records.is_empty());
    let mut stopped = req;
    stopped.arrays[0].stopping = true;
    until_finished(&node, &stopped).await;
}
#[tokio::test]
async fn a_delayed_assignment_cannot_roll_back_a_worker() {
    let root = tempfile::tempdir().unwrap();
    let node = Arc::new(node(root.path(), Duration::ZERO, 1));
    until_finished(&node, &request(vec![assignment(1, 10, 2)])).await;
    let stale = until_finished(&node, &request(vec![assignment(1, 10, 1)])).await;
    eprintln!(
        "after stale sync: grant attempt {}, task attempts started {}",
        stale.finished[0].attempt, stale.counters.attempts_started
    );
    assert_eq!(stale.finished[0].attempt, 2);
    assert_eq!(stale.counters.attempts_started, 10);
}

#[test]
fn failed_only_results_discard_a_superseded_failure() {
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
                duration_counts: [0; 16],
                chunk: ChunkId(0),
                attempt: 1,
                succeeded: 1,
                failed_count: 0,
                failed_indices: IndexRangeSet::new(),
                not_run: 0,
                retried: 0,
            },
        )
        .unwrap();
    let template = serde_json::from_value(serde_json::json!({ "exec": "/review/task" })).unwrap();
    let record = TaskArrayRecord {
        terminal_at_epoch_secs: None,
        name: "review".into(),
        namespace: "default".into(),
        template,
        state,
    };
    // `results?failed=true` has already filtered the new holder's success out.
    let stale = TaskResultRow {
        grant_attempt: 1,
        index: 0,
        attempts: 1,
        succeeded: false,
        not_run: false,
        exit_code: Some(1),
        run_ms: 1,
    };
    let (rows, _) = merge_rows(&record, vec![(NodeId::new("old-holder"), vec![stale])], 100);
    eprintln!(
        "leader succeeded={}, failed={}; failed-only result rows={rows:?}",
        record.state.summary().succeeded,
        record.state.summary().failed
    );
    assert!(rows.is_empty());
}

#[tokio::test]
async fn stale_control_cannot_delete_ledgers_after_a_restart() {
    use reliaburger::bun::task_array_node::ControlVersion;
    let root = tempfile::tempdir().unwrap();
    let first = node(root.path(), Duration::ZERO, 1);
    let mut req = request(vec![assignment(1, 10, 2)]);
    req.version = ControlVersion {
        epoch: 3,
        term: 8,
        index: 20,
    };
    until_finished(&first, &req).await;
    drop(first);
    let restarted = node(root.path(), Duration::ZERO, 1);
    let stale = NodeSyncRequest {
        version: ControlVersion {
            epoch: 2,
            term: 99,
            index: 999,
        },
        ..Default::default()
    };
    restarted.sync(&stale).await;
    assert_eq!(
        task_ledger::replay(&restarted.array_dir(1).join("ledger"))
            .unwrap()
            .records
            .len(),
        10
    );
    let result = until_finished(&restarted, &req).await;
    assert_eq!(result.counters.attempts_started, 0);
    assert_eq!(result.finished[0].attempt, 2);
}

#[tokio::test]
async fn mixed_profiles_share_capacity_with_an_application() {
    use reliaburger::bun::execution_budget::ExecutionBudget;
    use reliaburger::meat::Resources;
    let root = tempfile::tempdir().unwrap();
    let budget = ExecutionBudget::new(Resources::new(4000, 512 << 20, 0));
    let app = budget
        .try_acquire(Resources::new(2000, 256 << 20, 0))
        .unwrap();
    let node = node(root.path(), Duration::from_millis(100), 32).with_budget(budget.clone());
    let mut large = assignment(1, 5, 1);
    large.resources = Resources::new(2000, 256 << 20, 0);
    let mut small = assignment(2, 20, 1);
    small.resources = Resources::new(250, 32 << 20, 0);
    let mut req = request(vec![large, small]);
    node.sync(&req).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let progress = node.sync(&req).await;
        let large = progress.arrays[0].counters.running;
        let small = progress.arrays[1].counters.running;
        assert!(large * 2000 + small * 250 <= 2000);
        assert!(large * (256 << 20) + small * (32 << 20) <= 256 << 20);
        if progress.arrays.iter().all(|a| !a.finished.is_empty()) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(app);
    for a in &mut req.arrays {
        a.stopping = true;
    }
    node.sync(&req).await;
    assert_eq!(budget.available(), Resources::new(4000, 512 << 20, 0));
}

#[tokio::test]
async fn stale_grant_cannot_overwrite_a_newer_index_even_when_it_finishes_last() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ledger");
    let mut ledger = Ledger::open(&path).unwrap();
    let mut newer = record(42);
    newer.grant_attempt = 2;
    ledger.append(&[newer.clone()]);
    ledger.flush().unwrap();
    let mut older = record(42);
    older.outcome = TaskFinal::Failed;
    ledger.append(&[older]);
    ledger.flush().unwrap();
    let index = ledger.index();
    let rows = index.page(42, 43, false, 1).unwrap();
    assert_eq!(rows[0].grant_attempt, 2);
    assert_eq!(rows[0].outcome, TaskFinal::Succeeded);
    assert!(index.page(42, 43, true, 1).unwrap().is_empty());
    drop(index);
    drop(ledger);
    let recovered = Ledger::open(&path).unwrap();
    assert_eq!(
        recovered.index().page(42, 43, false, 1).unwrap()[0].grant_attempt,
        2
    );
}

#[tokio::test]
async fn output_storage_failure_refuses_work_and_never_acknowledges_the_chunk() {
    let directory = tempfile::tempdir().unwrap();
    let failing = TaskArrayNode::new(
        TaskArrayNodeConfig {
            root: directory.path().into(),
            policy: ProcessWorkloadsConfig {
                allowed_binaries: vec!["/review/task".into()],
                mount_isolation: false,
                ..Default::default()
            },
            default_concurrency: 1,
            backoff: (Duration::ZERO, Duration::ZERO),
            group_commit: GroupCommit {
                interval: Duration::from_millis(10),
                max_records: 4096,
            },
        },
        NodeRunner::Fake(FakeRunner::new(Duration::from_millis(100), |_| {
            AttemptOutcome::Exited { code: 1 }
        })),
    );
    let mut a = assignment(1, 100, 1);
    a.spec.max_attempts = 1;
    let req = request(vec![a]);
    failing.sync(&req).await;
    let output = directory.path().join("1/output");
    std::fs::remove_dir(&output).unwrap();
    std::fs::write(&output, b"unavailable output directory").unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let response = failing.sync(&req).await;
            let p = &response.arrays[0];
            assert!(p.finished.is_empty());
            if let Some(reason) = &p.refused {
                assert!(reason.contains("durability"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        task_ledger::replay(&directory.path().join("1/ledger"))
            .unwrap()
            .records
            .is_empty()
    );
}

#[tokio::test]
async fn a_new_term_cannot_delete_data_using_a_snapshot_behind_the_committed_revision() {
    let directory = tempfile::tempdir().unwrap();
    let executor = node(directory.path(), Duration::ZERO, 1);
    let mut current = request(vec![assignment(1, 1, 1)]);
    current.version = reliaburger::bun::task_array_node::ControlVersion {
        epoch: 1,
        term: 4,
        index: 100,
    };
    until_finished(&executor, &current).await;
    let delayed = NodeSyncRequest {
        version: reliaburger::bun::task_array_node::ControlVersion {
            epoch: 1,
            term: 5,
            index: 99,
        },
        known: vec![],
        arrays: vec![],
    };
    executor.sync(&delayed).await;
    assert!(directory.path().join("1/ledger").exists());
    assert_eq!(executor.results(1, false, 1).await.unwrap().len(), 1);
}
