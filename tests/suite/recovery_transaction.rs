//! Offline council recovery replaces the Raft stores without losing the old
//! ones (#430).
use reliaburger::council::recovery::recover_data_dir;
use reliaburger::council::types::DesiredState;

/// The `.raft-recovery-*/previous` directories recovery left beside `raft/`.
fn retained_directories(data_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(data_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".raft-recovery-"))
        })
        .map(|path| path.join("previous"))
        .collect()
}

#[test]
fn recovery_retains_the_previous_log_even_when_its_stores_are_damaged() {
    let dir = tempfile::tempdir().unwrap();
    reliaburger::compatibility::ensure_state_compatible(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("raft")).unwrap();
    std::fs::write(dir.path().join("raft/log.redb"), b"original log").unwrap();
    // A snapshot "store" redb can't open: damage is what recovery is for.
    std::fs::create_dir(dir.path().join("raft/snapshot.redb")).unwrap();

    let mut state = DesiredState::default();
    state.config.insert("restored".into(), "yes".into());
    recover_data_dir(dir.path(), state).unwrap();

    let retained = retained_directories(dir.path());
    assert_eq!(retained.len(), 1, "exactly one retained directory");
    assert_eq!(
        std::fs::read(retained[0].join("log.redb")).unwrap(),
        b"original log",
        "recovery must keep the only durable log, not delete it"
    );
    assert!(
        !dir.path().join("raft/log.redb").exists(),
        "the dead council's log must not stay in the live directory"
    );
    assert!(dir.path().join("raft/snapshot.redb").is_file());
}

#[test]
fn recovery_must_not_replace_a_store_held_open_by_a_live_node() {
    let dir = tempfile::tempdir().unwrap();
    recover_data_dir(dir.path(), DesiredState::default()).unwrap();
    let before = retained_directories(dir.path()).len();
    let path = dir.path().join("raft/snapshot.redb");
    let _live_handle = redb::Database::create(&path).unwrap();
    let result = recover_data_dir(dir.path(), DesiredState::default());
    assert!(
        result.is_err(),
        "recovery unlinks the locked inode and replaces live state successfully"
    );
    assert_eq!(
        retained_directories(dir.path()).len(),
        before,
        "a refused recovery must leave no transaction behind"
    );
    assert!(
        !dir.path().join("raft.recovery").exists(),
        "no recovery intent"
    );
}
