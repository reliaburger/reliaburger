//! Snapshot metadata must describe the bytes sent to a catching-up follower.
use openraft::storage::RaftStateMachine;
use openraft::{CommittedLeaderId, Entry, EntryPayload, LogId, Membership, RaftSnapshotBuilder};
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilNodeInfo, DesiredState, RaftRequest, TypeConfig};
use std::collections::{BTreeMap, BTreeSet};

#[tokio::test]
async fn a_current_snapshot_keeps_its_captured_log_position_and_membership() {
    let log = |index| LogId::new(CommittedLeaderId::new(1, 1), index);
    let mut leader = CouncilStateMachine::new();
    leader
        .apply([Entry::<TypeConfig> {
            log_id: log(1),
            payload: EntryPayload::Normal(RaftRequest::ConfigSet {
                key: "before".into(),
                value: "snapshot".into(),
            }),
        }])
        .await
        .unwrap();
    let captured = leader
        .get_snapshot_builder()
        .await
        .build_snapshot()
        .await
        .unwrap();
    let members = BTreeMap::from([(
        1,
        CouncilNodeInfo::new("127.0.0.1:9000".parse().unwrap(), "leader"),
    )]);
    leader
        .apply([
            Entry {
                log_id: log(2),
                payload: EntryPayload::Normal(RaftRequest::ConfigSet {
                    key: "after".into(),
                    value: "snapshot".into(),
                }),
            },
            Entry {
                log_id: log(3),
                payload: EntryPayload::Membership(Membership::new(
                    vec![BTreeSet::from([1])],
                    members,
                )),
            },
        ])
        .await
        .unwrap();
    let snapshot = leader.get_current_snapshot().await.unwrap().unwrap();
    let payload: DesiredState = serde_json::from_slice(snapshot.snapshot.get_ref()).unwrap();
    assert_eq!(snapshot.meta, captured.meta);
    assert_eq!(snapshot.meta.last_log_id, payload.last_applied_log);
    assert_eq!(snapshot.meta.last_membership, payload.last_membership);
    let mut follower = CouncilStateMachine::new();
    follower
        .install_snapshot(&snapshot.meta, snapshot.snapshot)
        .await
        .unwrap();
    // Entries after the snapshot must still be eligible for replay.
    assert_eq!(follower.applied_state().await.unwrap().0, Some(log(1)));
    assert!(!follower.desired_state().await.config.contains_key("after"));
}
