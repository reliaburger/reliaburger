//! Durable OCI intent must precede preparation and survive adapter replacement.

use std::path::Path;

use reliaburger::grill::runc_intent::{IntentConfiguration, IntentJournal, IntentPhase};
use reliaburger::grill::{InstanceId, OciSpec};

fn configuration(root: &Path) -> IntentConfiguration {
    IntentConfiguration {
        bundle_directory: root.join("bundles"),
        state_directory: root.join("state"),
        image_directory: root.join("images"),
        runc_program: "runc".into(),
        rootless: true,
        dns_nameserver: None,
        node_index: 1,
    }
}

fn spec(command: &str) -> OciSpec {
    serde_json::from_value(serde_json::json!({
        "root": {"path": "registry.example/test:latest", "readonly": true},
        "process": {"args": [command], "env": ["SECRET=value"], "cwd": "/", "user": {"uid": 1000, "gid": 1000}},
        "mounts": [], "linux": {"namespaces": []}
    }))
    .unwrap()
}

#[tokio::test]
async fn published_intent_preserves_original_input_across_adapter_replacement() {
    let root = tempfile::tempdir().unwrap();
    let journal = IntentJournal::new(root.path().join("intents"), configuration(root.path()));
    let id = InstanceId("default__worker-0".into());
    let mut original = spec("original");
    let guard = journal.claim(&id, None).await.unwrap();
    let guard = guard.publish(&original).await.unwrap();
    let generation = guard.record().unwrap().generation.clone();
    original.process.args[0] = "prepared-command".into();
    drop(guard);
    let recovered = IntentJournal::new(root.path().join("intents"), configuration(root.path()));
    let records = recovered.inventory().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].instance_id, id);
    assert_eq!(records[0].spec, spec("original"));
    assert_eq!(records[0].generation, generation);
    assert_eq!(records[0].phase, IntentPhase::Owned);
    assert_eq!(
        recovered.observe(&id).await.unwrap(),
        Some(generation.clone())
    );
    let guard = recovered.claim(&id, Some(generation)).await.unwrap();
    assert!(
        guard.publish(&original).await.is_err(),
        "an unretired owner must fence replacement"
    );
    assert_eq!(
        recovered.inventory().await.unwrap()[0].spec,
        spec("original")
    );
}

#[tokio::test]
async fn independent_adapters_share_locks_and_stale_generations_cannot_retire_successors() {
    let root = tempfile::tempdir().unwrap();
    let first = IntentJournal::new(root.path().join("intents"), configuration(root.path()));
    let second = IntentJournal::new(root.path().join("intents"), configuration(root.path()));
    let id = InstanceId("default__worker-0".into());
    let guard = first
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("first"))
        .await
        .unwrap();
    let generation = guard.record().unwrap().generation.clone();
    assert!(
        second.claim(&id, Some(generation.clone())).await.is_err(),
        "separate adapters must share lifecycle ownership"
    );
    let guard = guard.retire(Some(17)).await.unwrap();
    assert_eq!(
        guard.record().unwrap().phase,
        IntentPhase::Retired {
            exit_code: Some(17)
        }
    );
    let guard = guard.publish(&spec("second")).await.unwrap();
    let replacement = guard.record().unwrap().generation.clone();
    assert_ne!(replacement, generation);
    drop(guard);
    assert!(
        second.claim(&id, Some(generation)).await.is_err(),
        "stale authority must refuse a successor"
    );
    assert!(
        second.claim(&id, None).await.is_err(),
        "stale absence must refuse a published generation"
    );
    let guard = second.claim(&id, Some(replacement)).await.unwrap();
    assert_eq!(guard.record().unwrap().spec, spec("second"));
    assert_eq!(guard.record().unwrap().phase, IntentPhase::Owned);
}

#[tokio::test]
async fn incompatible_runtime_configuration_refuses_inventory_and_mutation() {
    let root = tempfile::tempdir().unwrap();
    let original = configuration(root.path());
    let journal = IntentJournal::new(root.path().join("intents"), original.clone());
    let id = InstanceId("default__worker-0".into());
    let guard = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("original"))
        .await
        .unwrap();
    let generation = guard.record().unwrap().generation.clone();
    drop(guard);
    let mut alternatives = Vec::new();
    let mut changed = original.clone();
    changed.rootless = false;
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.node_index = 2;
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.bundle_directory = root.path().join("other-bundles");
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.state_directory = root.path().join("other-state");
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.image_directory = root.path().join("other-images");
    alternatives.push(changed);
    let mut changed = original;
    changed.runc_program = "/different/runc".into();
    alternatives.push(changed);
    for configuration in alternatives {
        let changed = IntentJournal::new(root.path().join("intents"), configuration);
        assert!(changed.inventory().await.is_err());
        assert!(changed.claim(&id, Some(generation.clone())).await.is_err());
    }
    assert_eq!(journal.inventory().await.unwrap()[0].spec, spec("original"));
}

#[tokio::test]
async fn incomplete_corrupt_or_redirected_intent_refuses_the_complete_inventory() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("intents");
    let journal = IntentJournal::new(directory.clone(), configuration(root.path()));
    assert!(journal.inventory().await.unwrap().is_empty());
    let id = InstanceId("default__worker-0".into());
    let guard = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("original"))
        .await
        .unwrap();
    drop(guard);
    let record = directory.join("records").join(&id.0).join("intent.json");
    let bytes = std::fs::read(&record).unwrap();
    for damaged in [b"{".to_vec(), vec![b' '; 1024 * 1024 + 1]] {
        std::fs::write(&record, &damaged).unwrap();
        assert!(journal.inventory().await.is_err());
        assert!(journal.observe(&id).await.is_err());
        assert_eq!(std::fs::read(&record).unwrap(), damaged);
    }
    std::fs::remove_file(&record).unwrap();
    assert!(
        journal.inventory().await.is_err(),
        "a published directory without intent is not empty inventory"
    );
    assert!(journal.observe(&id).await.is_err());
    let external = root.path().join("external.json");
    std::fs::write(&external, &bytes).unwrap();
    symlink(&external, &record).unwrap();
    assert!(journal.inventory().await.is_err());
    assert_eq!(std::fs::read(&external).unwrap(), bytes);
    std::fs::remove_file(&record).unwrap();
    std::fs::write(&record, bytes).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(journal.inventory().await.unwrap().len(), 1);
}

#[tokio::test]
async fn unpublished_claims_and_abandoned_staging_never_appear_as_launched_intent() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("intents");
    let journal = IntentJournal::new(directory.clone(), configuration(root.path()));
    let id = InstanceId("default__worker-0".into());
    drop(journal.claim(&id, None).await.unwrap());
    assert!(journal.inventory().await.unwrap().is_empty());
    assert_eq!(journal.observe(&id).await.unwrap(), None);
    let staging = directory.join("records/.preparing-interrupted");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(staging.join("intent.json"), "partial").unwrap();
    assert!(journal.inventory().await.unwrap().is_empty());
    for bad in ["", "..", "../escape", "a/b", "/absolute"] {
        assert!(journal.claim(&InstanceId(bad.into()), None).await.is_err());
    }
    let guard = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("original"))
        .await
        .unwrap();
    let record = directory.join("records").join(&id.0).join("intent.json");
    assert_eq!(
        std::fs::metadata(&record).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(record.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    drop(guard);
}

#[tokio::test]
async fn killed_lifecycle_owner_leaves_intent_and_releases_only_its_lock() {
    let root = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "journal_owner_fixture", "--nocapture"])
        .env("RELIABURGER_INTENT_FIXTURE", root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !root.path().join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "fixture exited before publication"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let journal = IntentJournal::new(root.path().join("intents"), configuration(root.path()));
    let id = InstanceId("default__worker-0".into());
    let generation = journal.observe(&id).await.unwrap();
    assert!(journal.claim(&id, generation.clone()).await.is_err());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let guard = journal.claim(&id, generation).await.unwrap();
    assert_eq!(guard.record().unwrap().phase, IntentPhase::Owned);
    assert_eq!(guard.record().unwrap().spec, spec("original"));
    assert!(guard.publish(&spec("replacement")).await.is_err());
}

#[tokio::test]
async fn journal_owner_fixture() {
    let Some(root) = std::env::var_os("RELIABURGER_INTENT_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let journal = IntentJournal::new(root.join("intents"), configuration(&root));
    let id = InstanceId("default__worker-0".into());
    let _guard = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("original"))
        .await
        .unwrap();
    std::fs::write(root.join("ready"), "published").unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn invalid_publication_and_conflicting_exit_evidence_preserve_the_previous_generation() {
    let root = tempfile::tempdir().unwrap();
    let journal = IntentJournal::new(root.path().join("intents"), configuration(root.path()));
    let id = InstanceId("default__worker-0".into());
    let mut oversized = spec("original");
    oversized.process.env = vec!["X".repeat(1024 * 1024)];
    assert!(
        journal
            .claim(&id, None)
            .await
            .unwrap()
            .publish(&oversized)
            .await
            .is_err()
    );
    assert!(journal.inventory().await.unwrap().is_empty());
    let guard = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("original"))
        .await
        .unwrap();
    let generation = guard.record().unwrap().generation.clone();
    let guard = guard
        .retire(Some(3))
        .await
        .unwrap()
        .retire(Some(3))
        .await
        .unwrap();
    assert!(guard.retire(Some(4)).await.is_err());
    let guard = journal.claim(&id, Some(generation.clone())).await.unwrap();
    assert!(guard.publish(&oversized).await.is_err());
    let records = journal.inventory().await.unwrap();
    assert_eq!(records[0].generation, generation);
    assert_eq!(
        records[0].phase,
        IntentPhase::Retired { exit_code: Some(3) }
    );
    assert_eq!(records[0].spec, spec("original"));
}

#[tokio::test]
async fn network_reference_survives_recovery_and_blocks_full_retirement() {
    use reliaburger::grill::runc_intent::NetworkReferenceState;
    let root = tempfile::tempdir().unwrap();
    let mut config = configuration(root.path());
    config.rootless = false;
    let journal = IntentJournal::new(root.path().join("intents"), config);
    let id = InstanceId("default__network-0".into());
    let claim = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("first"))
        .await
        .unwrap()
        .retain_network(7)
        .await
        .unwrap();
    let generation = claim.record().unwrap().generation.clone();
    let reference = match claim.record().unwrap().network_reference.clone().unwrap() {
        NetworkReferenceState::Held(reference) => reference,
        other => panic!("unexpected network state: {other:?}"),
    };
    assert!(
        claim.retire(Some(0)).await.is_err(),
        "execution exit freed a discovery reference"
    );
    let recovered = journal.inventory().await.unwrap().remove(0);
    assert_eq!(
        recovered.network_reference,
        Some(NetworkReferenceState::Held(reference.clone()))
    );
    let claim = journal.claim(&id, Some(generation)).await.unwrap();
    let claim = claim
        .release_network(reference.clone())
        .await
        .unwrap()
        .release_network(reference.clone())
        .await
        .unwrap();
    let claim = claim.retire(Some(0)).await.unwrap();
    assert_eq!(
        claim.record().unwrap().network_reference,
        Some(NetworkReferenceState::Released(reference))
    );
}

#[tokio::test]
async fn stale_network_release_cannot_discharge_a_successor_reference() {
    use reliaburger::grill::runc_intent::NetworkReferenceState;
    let root = tempfile::tempdir().unwrap();
    let mut config = configuration(root.path());
    config.rootless = false;
    let journal = IntentJournal::new(root.path().join("intents"), config);
    let id = InstanceId("default__network-0".into());
    let claim = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("first"))
        .await
        .unwrap()
        .retain_network(0)
        .await
        .unwrap();
    let NetworkReferenceState::Held(original) =
        claim.record().unwrap().network_reference.clone().unwrap()
    else {
        panic!("missing hold");
    };
    let claim = claim
        .release_network(original.clone())
        .await
        .unwrap()
        .retire(Some(0))
        .await
        .unwrap()
        .publish(&spec("second"))
        .await
        .unwrap()
        .retain_network(0)
        .await
        .unwrap();
    let successor = claim.record().unwrap().network_reference.clone();
    assert!(claim.release_network(original).await.is_err());
    assert_eq!(
        journal.inventory().await.unwrap()[0].network_reference,
        successor
    );
}

#[tokio::test]
async fn missing_or_conflicting_network_references_cannot_be_released() {
    use reliaburger::grill::runc_intent::NetworkReferenceState;
    let root = tempfile::tempdir().unwrap();
    let mut config = configuration(root.path());
    config.rootless = false;
    let journal = IntentJournal::new(root.path().join("intents"), config);
    let id = InstanceId("default__network-0".into());
    let claim = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec("first"))
        .await
        .unwrap()
        .retain_network(4)
        .await
        .unwrap();
    let generation = claim.record().unwrap().generation.clone();
    let NetworkReferenceState::Held(mut forged) =
        claim.record().unwrap().network_reference.clone().unwrap()
    else {
        panic!("missing hold");
    };
    forged.container_index = 5;
    assert!(claim.release_network(forged).await.is_err());
    let claim = journal.claim(&id, Some(generation)).await.unwrap();
    assert!(claim.retain_network(5).await.is_err());
    assert!(matches!(
        journal.inventory().await.unwrap()[0].network_reference,
        Some(NetworkReferenceState::Held(_))
    ));
}
