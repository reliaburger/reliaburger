use reliaburger::council::types::DesiredState;
#[test]
fn failed_recovery_must_preserve_the_previous_log() {
    use reliaburger::council::recovery::recover_data_dir;
    let dir = tempfile::tempdir().unwrap();
    reliaburger::compatibility::ensure_state_compatible(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("raft")).unwrap();
    std::fs::write(dir.path().join("raft/log.redb"), b"original log").unwrap();
    // A replacement snapshot cannot be installed at this unexpected path.
    std::fs::create_dir(dir.path().join("raft/snapshot.redb")).unwrap();
    assert!(recover_data_dir(dir.path(), DesiredState::default()).is_err());
    assert!(
        dir.path().join("raft/log.redb").exists(),
        "failed recovery deleted the only durable log before verifying its output"
    );
}

#[test]
fn recovery_must_not_replace_a_store_held_open_by_a_live_node() {
    use reliaburger::council::recovery::recover_data_dir;
    let dir = tempfile::tempdir().unwrap();
    recover_data_dir(dir.path(), DesiredState::default()).unwrap();
    let path = dir.path().join("raft/snapshot.redb");
    let _live_handle = redb::Database::create(&path).unwrap();
    let result = recover_data_dir(dir.path(), DesiredState::default());
    assert!(
        result.is_err(),
        "recovery unlinks the locked inode and replaces live state successfully"
    );
}
