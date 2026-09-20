//! Runtime commands remain discoverable when their caller disappears.
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use reliaburger::grill::command::{CommandState, OwnedCommands};

fn commands(path: &Path) -> OwnedCommands {
    OwnedCommands::new(path.to_path_buf(), env!("CARGO_BIN_EXE_bun").into())
}

#[tokio::test]
async fn command_intent_precedes_activation_and_preserves_actual_output() {
    let root = tempfile::tempdir().unwrap();
    let first = commands(root.path());
    let marker = root.path().join("activated");
    let id = first
        .prepare(
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                format!(
                    "touch '{}'; printf '%s' \"$COMMAND_VALUE\"; printf error >&2; exit 23",
                    marker.display()
                ),
            ],
            &BTreeMap::from([("COMMAND_VALUE".into(), "space ' quote".into())]),
        )
        .await
        .unwrap();
    assert!(!marker.exists());
    drop(first);
    let recovered = commands(root.path());
    assert_eq!(recovered.inventory().await.unwrap(), vec![id.clone()]);
    assert_eq!(recovered.state(&id).await.unwrap(), CommandState::Prepared);
    recovered.start(&id).await.unwrap();
    let output = recovered.wait(&id, Duration::from_secs(15)).await.unwrap();
    assert_eq!(output.exit_code, Some(23));
    assert_eq!(output.stdout, b"space ' quote");
    assert_eq!(output.stderr, b"error");
    assert!(marker.exists());
}

#[tokio::test]
async fn timed_out_command_retains_authority_until_explicit_retirement() {
    let root = tempfile::tempdir().unwrap();
    let first = commands(root.path());
    let id = first
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "sleep 60 & wait".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    first.start(&id).await.unwrap();
    let output = first.wait(&id, Duration::from_millis(20)).await;
    assert!(output.is_err());
    drop(first);
    let recovered = commands(root.path());
    assert_eq!(recovered.inventory().await.unwrap(), vec![id.clone()]);
    recovered
        .retire(&id, Duration::from_secs(15))
        .await
        .unwrap();
    assert_eq!(
        recovered.state(&id).await.unwrap(),
        CommandState::Retired { exit_code: None }
    );
}

#[tokio::test]
async fn prepared_command_can_be_retired_without_ever_executing() {
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let marker = root.path().join("must-not-run");
    let id = owner
        .prepare(
            Path::new("/usr/bin/touch"),
            &[marker.display().to_string()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    owner.retire(&id, Duration::from_secs(5)).await.unwrap();
    assert_eq!(owner.state(&id).await.unwrap(), CommandState::Cancelled);
    assert!(owner.start(&id).await.is_err());
    assert!(!marker.exists());
}

#[tokio::test]
async fn invalid_arguments_and_environment_never_publish_a_command() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let non_utf8 = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));
    assert!(
        owner
            .prepare(&non_utf8, &[], &BTreeMap::new())
            .await
            .is_err()
    );
    assert!(
        owner
            .prepare(Path::new("/bin/echo"), &["a\0b".into()], &BTreeMap::new())
            .await
            .is_err()
    );
    for (key, value) in [("", "value"), ("a=b", "value"), ("key", "a\0b")] {
        assert!(
            owner
                .prepare(
                    Path::new("/bin/echo"),
                    &[],
                    &BTreeMap::from([(key.into(), value.into())])
                )
                .await
                .is_err()
        );
    }
    assert!(owner.inventory().await.unwrap().is_empty());
}

#[tokio::test]
async fn oversized_output_preserves_confirmed_retirement() {
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let id = owner
        .prepare(
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                "head -c 700000 /dev/zero; head -c 400000 /dev/zero >&2".into(),
            ],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    owner.start(&id).await.unwrap();
    let output = owner.wait(&id, Duration::from_secs(15)).await;
    match output {
        Err(reliaburger::grill::command::CommandError::OutputTooLarge) => {}
        Err(error) => panic!("unexpected output error: {error:?}"),
        Ok(output) => panic!(
            "output unexpectedly accepted: exit {:?}, stdout {} bytes, stderr {} bytes ({})",
            output.exit_code,
            output.stdout.len(),
            output.stderr.len(),
            String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(512)])
        ),
    }
    assert_eq!(
        owner.state(&id).await.unwrap(),
        CommandState::Retired { exit_code: Some(0) }
    );
    owner.retire(&id, Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn pruning_removes_only_confirmed_terminal_commands_and_fences_old_starts() {
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let prepared = owner
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    let running = owner
        .prepare(Path::new("/bin/sleep"), &["60".into()], &BTreeMap::new())
        .await
        .unwrap();
    owner.start(&running).await.unwrap();
    let finished = owner
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    owner.start(&finished).await.unwrap();
    owner
        .wait(&finished, Duration::from_secs(15))
        .await
        .unwrap();
    assert_eq!(owner.prune_retired().await.unwrap(), 1);
    let inventory = owner.inventory().await.unwrap();
    assert_eq!(inventory.len(), 2);
    assert!(inventory.contains(&prepared));
    assert!(inventory.contains(&running));
    assert!(
        !owner
            .log_stem(&finished)
            .unwrap()
            .with_extension("stdout")
            .exists()
    );
    assert!(owner.start(&finished).await.is_err());
    for id in [&prepared, &running] {
        owner.retire(id, Duration::from_secs(15)).await.unwrap();
    }
    assert_eq!(owner.prune_retired().await.unwrap(), 2);
    assert!(owner.inventory().await.unwrap().is_empty());
    assert!(owner.start(&prepared).await.is_err());
    assert_eq!(owner.prune_retired().await.unwrap(), 0);
}

#[tokio::test]
async fn pruning_preserves_unknown_owner_evidence() {
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let id = owner
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    let directory = owner.log_stem(&id).unwrap().parent().unwrap().to_path_buf();
    let path = directory.join("owner.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    // A recorded Running command with no reachable owner is uncertain, not absent.
    record["phase"] = serde_json::json!({"state":"running", "pid": u32::MAX});
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(owner.prune_retired().await.unwrap(), 0);
    assert_eq!(owner.inventory().await.unwrap(), vec![id]);
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[tokio::test]
async fn pruning_resumes_after_interrupted_deletion_outside_the_active_inventory() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let id = owner
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    owner.retire(&id, Duration::from_secs(5)).await.unwrap();
    let directory = owner.log_stem(&id).unwrap().parent().unwrap().to_path_buf();
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("owner.json")).unwrap()).unwrap();
    let garbage = root.path().join("retired-commands");
    std::fs::create_dir(&garbage).unwrap();
    std::fs::set_permissions(&garbage, std::fs::Permissions::from_mode(0o700)).unwrap();
    let tombstone = garbage.join(format!(
        "{}-{}",
        directory.file_name().unwrap().to_str().unwrap(),
        record["nonce"].as_str().unwrap()
    ));
    // Crash after atomic removal from the active collection, halfway through deletion.
    std::fs::rename(&directory, &tombstone).unwrap();
    std::fs::remove_file(tombstone.join("owner.json")).unwrap();
    assert!(owner.inventory().await.unwrap().is_empty());
    assert_eq!(owner.prune_retired().await.unwrap(), 0);
    assert!(std::fs::read_dir(garbage).unwrap().next().is_none());
    assert!(owner.start(&id).await.is_err());
}

#[tokio::test]
async fn pruning_refuses_redirected_garbage_storage_and_preserves_active_records() {
    let root = tempfile::tempdir().unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    let marker = unrelated.path().join("keep");
    std::fs::write(&marker, "unrelated").unwrap();
    let owner = commands(root.path());
    let id = owner
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    owner.retire(&id, Duration::from_secs(5)).await.unwrap();
    std::os::unix::fs::symlink(unrelated.path(), root.path().join("retired-commands")).unwrap();
    assert!(owner.prune_retired().await.is_err());
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "unrelated");
    assert_eq!(owner.inventory().await.unwrap(), vec![id]);
}

struct InterruptedControl {
    socket: std::path::PathBuf,
    parked: std::path::PathBuf,
    listener: Option<tokio::net::UnixListener>,
}

impl InterruptedControl {
    fn new(owner: &OwnedCommands, id: &reliaburger::grill::command::CommandId) -> Self {
        let directory = owner.log_stem(id).unwrap().parent().unwrap().to_path_buf();
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("owner.json")).unwrap()).unwrap();
        let parent = std::path::PathBuf::from(format!(
            "/tmp/rbp-{}-{}",
            nix::unistd::geteuid(),
            record["nonce"].as_str().unwrap()
        ));
        let socket = parent.join("control.sock");
        let parked = parent.join("interrupted.sock");
        std::fs::rename(&socket, &parked).unwrap();
        let mut interruption = Self {
            socket,
            parked,
            listener: None,
        };
        interruption.listener = Some(tokio::net::UnixListener::bind(&interruption.socket).unwrap());
        interruption
    }
}

impl Drop for InterruptedControl {
    fn drop(&mut self) {
        self.listener.take();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::rename(&self.parked, &self.socket);
    }
}

#[tokio::test]
async fn command_wait_recovers_from_a_reset_without_inventing_retirement() {
    use tokio::io::AsyncReadExt;
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let release = root.path().join("release");
    let id = owner
        .prepare(
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                format!(
                    "while [ ! -f '{}' ]; do sleep 0.01; done; printf confirmed",
                    release.display()
                ),
            ],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    owner.start(&id).await.unwrap();
    let interruption = InterruptedControl::new(&owner, &id);
    let waiting_owner = owner.clone();
    let waiting_id = id.clone();
    let mut waiting = tokio::spawn(async move {
        waiting_owner
            .wait(&waiting_id, Duration::from_secs(15))
            .await
    });
    let (mut connection, _) = tokio::time::timeout(
        Duration::from_secs(5),
        interruption.listener.as_ref().unwrap().accept(),
    )
    .await
    .unwrap()
    .unwrap();
    // Close with unread request bytes, forcing a reset while the actual owner
    // and workload remain live. Their durable record still says Running.
    connection.read_exact(&mut [0u8; 1]).await.unwrap();
    drop(connection);
    let early = tokio::time::timeout(Duration::from_millis(100), &mut waiting).await;
    drop(interruption);
    std::fs::write(release, b"release").unwrap();
    let returned_early = early.is_ok();
    let output = if returned_early {
        owner.wait(&id, Duration::from_secs(15)).await.unwrap()
    } else {
        waiting.await.unwrap().unwrap()
    };
    owner.retire(&id, Duration::from_secs(5)).await.unwrap();
    assert!(
        !returned_early,
        "transport reset ended the bounded wait: {early:?}"
    );
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, b"confirmed");
}

#[tokio::test]
async fn unreachable_command_owner_expires_the_wait_and_retains_original_evidence() {
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let id = owner
        .prepare(Path::new("/bin/sleep"), &["60".into()], &BTreeMap::new())
        .await
        .unwrap();
    owner.start(&id).await.unwrap();
    let record = owner
        .log_stem(&id)
        .unwrap()
        .parent()
        .unwrap()
        .join("owner.json");
    let original = std::fs::read(&record).unwrap();
    let mut interruption = InterruptedControl::new(&owner, &id);
    interruption.listener.take();
    let result = owner.wait(&id, Duration::from_millis(100)).await;
    let retained = std::fs::read(record).unwrap() == original;
    drop(interruption);
    owner.retire(&id, Duration::from_secs(5)).await.unwrap();
    assert!(
        matches!(
            result,
            Err(reliaburger::grill::command::CommandError::TimedOut { .. })
        ),
        "{result:?}"
    );
    assert!(
        retained,
        "unreachable owner lost its original Running evidence"
    );
}

#[tokio::test]
async fn command_retirement_recovers_from_a_reset_before_confirming_exit() {
    use tokio::io::AsyncReadExt;
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let id = owner
        .prepare(Path::new("/bin/sleep"), &["60".into()], &BTreeMap::new())
        .await
        .unwrap();
    owner.start(&id).await.unwrap();
    let mut interruption = InterruptedControl::new(&owner, &id);
    let retiring_owner = owner.clone();
    let retiring_id = id.clone();
    let mut retiring = tokio::spawn(async move {
        retiring_owner
            .retire(&retiring_id, Duration::from_secs(5))
            .await
    });
    let (mut connection, _) = tokio::time::timeout(
        Duration::from_secs(5),
        interruption.listener.as_ref().unwrap().accept(),
    )
    .await
    .unwrap()
    .unwrap();
    connection.read_exact(&mut [0u8; 1]).await.unwrap();
    drop(connection);
    interruption.listener.take();
    let early = tokio::time::timeout(Duration::from_millis(100), &mut retiring).await;
    drop(interruption);
    let returned_early = early.is_ok();
    if returned_early {
        owner.retire(&id, Duration::from_secs(5)).await.unwrap();
    } else {
        retiring.await.unwrap().unwrap();
    }
    assert!(
        !returned_early,
        "transport reset ended the bounded retirement: {early:?}"
    );
    assert!(matches!(
        owner.state(&id).await.unwrap(),
        CommandState::Retired { .. }
    ));
}

#[tokio::test]
async fn unreachable_command_owner_expires_retirement_and_retains_original_evidence() {
    let root = tempfile::tempdir().unwrap();
    let owner = commands(root.path());
    let id = owner
        .prepare(Path::new("/bin/sleep"), &["60".into()], &BTreeMap::new())
        .await
        .unwrap();
    owner.start(&id).await.unwrap();
    let record = owner
        .log_stem(&id)
        .unwrap()
        .parent()
        .unwrap()
        .join("owner.json");
    let original = std::fs::read(&record).unwrap();
    let mut interruption = InterruptedControl::new(&owner, &id);
    interruption.listener.take();
    let result = owner.retire(&id, Duration::from_millis(100)).await;
    let retained = std::fs::read(record).unwrap() == original;
    drop(interruption);
    owner.retire(&id, Duration::from_secs(5)).await.unwrap();
    assert!(
        matches!(
            result,
            Err(reliaburger::grill::command::CommandError::TimedOut { .. })
        ),
        "{result:?}"
    );
    assert!(
        retained,
        "unreachable owner lost its original Running evidence"
    );
}
