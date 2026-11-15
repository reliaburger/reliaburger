//! What requests wrote to a council, apart from the leader's cron clock.
//!
//! Building the API router spawns the task-array leader loop, and that loop
//! commits `TaskArrayWrite::CronObserve` on its first tick and again on each
//! new wall-clock minute. Nothing orders that entry against a test's own
//! requests, so a test that counts Raft entries, or compares the whole
//! desired state, around a request sees it at random (#849, #1329, #1331).
//!
//! These helpers judge what was written rather than how many entries there
//! are: [`writes_after`] lists every entry past an index except the cron
//! clock's, and [`changed_desired_fields`] compares every desired-state
//! field except the two that entry moves. [`observe_a_later_minute`] commits
//! one such entry on purpose, so a test meets the race on every run instead
//! of once in a few hundred.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use openraft::storage::RaftLogReader;
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo, RaftRequest, TypeConfig};
use reliaburger::council::{CouncilNode, CouncilResponse, DesiredState};
use reliaburger::meat::task_array_store::TaskArrayWrite;

/// Whether a log entry is the task-array leader loop's cron clock.
pub(crate) fn is_cron_observation(entry: &openraft::Entry<TypeConfig>) -> bool {
    matches!(
        &entry.payload,
        openraft::EntryPayload::Normal(RaftRequest::TaskArray(write))
            if matches!(**write, TaskArrayWrite::CronObserve { .. })
    )
}

/// The index of the newest entry in `log`, or 0 when it holds none.
pub(crate) async fn last_index(log: &MemLogStore) -> u64 {
    entries(log)
        .await
        .last()
        .map_or(0, |entry| entry.log_id.index)
}

/// Every entry in `log` past `index`, except the cron clock's, each named
/// by its log id and payload so a failing assertion says what was written.
pub(crate) async fn writes_after(log: &MemLogStore, index: u64) -> Vec<String> {
    entries(log)
        .await
        .into_iter()
        .filter(|entry| entry.log_id.index > index && !is_cron_observation(entry))
        .map(|entry| match entry.payload {
            openraft::EntryPayload::Normal(request) => format!("{}: {request:?}", entry.log_id),
            other => format!("{}: {other:?}", entry.log_id),
        })
        .collect()
}

/// The top-level desired-state fields that differ between two reads,
/// ignoring the log position and `cron_observed_minute`: the only two
/// fields the cron clock's entry moves, so neither can show whether a
/// request wrote.
pub(crate) fn changed_desired_fields(before: &DesiredState, after: &DesiredState) -> Vec<String> {
    let fields = |state: &DesiredState| {
        let serde_json::Value::Object(mut fields) = serde_json::to_value(state).unwrap() else {
            panic!("desired state serialises as an object");
        };
        fields.remove("last_applied_log");
        if let Some(serde_json::Value::Object(arrays)) = fields.get_mut("task_arrays") {
            arrays.remove("cron_observed_minute");
        }
        fields
    };
    let (before, after) = (fields(before), fields(after));
    before
        .keys()
        .chain(after.keys())
        .filter(|key| before.get(*key) != after.get(*key))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Commit the cron clock's entry for the minute after the wall clock's, as
/// the leader loop would on reaching it. A test calls this inside the
/// window it judges, so the stray entry the loop can write there is always
/// present. The loop then fires no schedule until the wall clock catches
/// up, which a test with no schedules never notices.
pub(crate) async fn observe_a_later_minute(council: &CouncilNode) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let minute = i64::try_from(now / 60).unwrap() + 1;
    let response = council
        .write(RaftRequest::TaskArray(Box::new(
            TaskArrayWrite::CronObserve { minute },
        )))
        .await
        .unwrap();
    assert!(
        !matches!(response, CouncilResponse::Refused { .. }),
        "the cron observation was refused: {response:?}"
    );
}

async fn entries(log: &MemLogStore) -> Vec<openraft::Entry<TypeConfig>> {
    log.clone().try_get_log_entries(..).await.unwrap()
}

/// A one-voter council that leads, with a handle on its Raft log.
async fn leader_with_log() -> (Arc<CouncilNode>, MemLogStore) {
    let log = MemLogStore::new();
    let network = InMemoryRaftRouter::new();
    let node = CouncilNode::new(
        1,
        CouncilConfig {
            heartbeat_interval_ms: 50,
            election_timeout_min_ms: 150,
            election_timeout_max_ms: 300,
            ..Default::default()
        },
        InMemoryRaftNetworkFactory::new(1, network.clone()),
        log.clone(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    network.register(1, node.raft().clone()).await;
    node.initialize(BTreeMap::from([(
        1,
        CouncilNodeInfo::new("127.0.0.1:9000".parse().unwrap(), "node"),
    )]))
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !node.is_leader().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (Arc::new(node), log)
}

/// The race in #1331, forced: the cron clock's entry lands between a
/// test's two reads. Counting entries or comparing whole states calls
/// that a write; these helpers don't, yet still name a request's write.
#[tokio::test]
async fn the_cron_clock_is_not_a_write_but_a_request_still_is() {
    let (council, log) = leader_with_log().await;
    let before_index = last_index(&log).await;
    let before = council.desired_state().await;

    observe_a_later_minute(&council).await;
    let after = council.desired_state().await;
    assert!(last_index(&log).await > before_index, "nothing was written");
    assert_ne!(before.last_applied_log, after.last_applied_log);
    assert_eq!(writes_after(&log, before_index).await, Vec::<String>::new());
    assert_eq!(
        changed_desired_fields(&before, &after),
        Vec::<String>::new()
    );

    council
        .write(RaftRequest::RegisterEndpointConsumer {
            node_id: "team-a".into(),
        })
        .await
        .unwrap();
    let written = writes_after(&log, before_index).await;
    assert_eq!(written.len(), 1, "{written:?}");
    assert!(written[0].contains("team-a"), "{written:?}");
    assert_eq!(
        changed_desired_fields(&before, &council.desired_state().await),
        ["endpoint_consumers"]
    );
    council.shutdown().await.unwrap();
}
