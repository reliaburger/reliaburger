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
    assert!(matches!(
        output,
        Err(reliaburger::grill::command::CommandError::OutputTooLarge)
    ));
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
