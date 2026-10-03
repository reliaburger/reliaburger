//! The exec that ends an upgrade waits for its answer to reach the caller.
//!
//! Exec closes every socket the process holds. The agent used to pause a
//! fixed 200 ms after answering and hope the HTTP layer had written the
//! answer by then; on a loaded host it hadn't, and the caller saw a dropped
//! connection for an upgrade that went ahead (#526).

use super::commands::AgentCommand;
use super::*;
use crate::upgrade::marker::{MarkerPhase, UpgradeMarker};
use crate::upgrade::signing::{encode_public_key, generate_keypair, sha256_hex, sign};

/// The marker's phase: `Staged` until the exec starts, absent once a failed
/// exec has been rolled back.
fn marker_phase(data: &std::path::Path) -> Option<MarkerPhase> {
    UpgradeMarker::load(&UpgradeMarker::path_in(data))
        .unwrap()
        .map(|marker| marker.phase)
}

/// A node on 0.1.0 with an upgrade manager, and a signed directive to 0.2.0.
struct Fixture {
    _dir: tempfile::TempDir,
    data: std::path::PathBuf,
    store: crate::upgrade::store::BinaryStore,
    manager: crate::upgrade::manager::UpgradeManager,
    directive: crate::upgrade::types::UpgradeDirective,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let binary_dir = dir.path().join("bin");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&binary_dir).unwrap();
    let running_version: crate::upgrade::BinaryVersion = "0.1.0".parse().unwrap();
    let store = crate::upgrade::store::BinaryStore::new(binary_dir.clone(), "bun".to_string());
    store
        .stage(
            &running_version,
            b"old binary",
            &crate::upgrade::signing::SignatureEnvelope {
                schema: 1,
                sha256: sha256_hex(b"old binary"),
                embedded: String::new(),
                external: None,
            },
        )
        .unwrap();
    store.activate(&running_version).unwrap();
    let (release_pkcs8, release_public) = generate_keypair().unwrap();
    let manager = crate::upgrade::manager::UpgradeManager::new(
        &crate::config::node::UpgradeSection {
            binary_dir: Some(binary_dir.clone()),
            release_keys_override: Some(vec![encode_public_key(&release_public)]),
            ..Default::default()
        },
        &data,
        &binary_dir.join("bun"),
        running_version,
        vec!["bun".to_string()],
    )
    .unwrap();
    // It answers the compatibility query and fails anything else. Once it is
    // staged, the test overwrites it so the exec itself fails and this test
    // process carries on; that failure is what shows the exec was tried.
    let formats = serde_json::to_string(&crate::compatibility::CURRENT).unwrap();
    let binary =
        format!("#!/bin/sh\n[ \"$1\" = --compatibility ] || exit 1\nprintf '%s' '{formats}'\n")
            .into_bytes();
    let source = dir.path().join("next-bun");
    std::fs::write(&source, &binary).unwrap();
    let directive = crate::upgrade::types::UpgradeDirective {
        upgrade_id: "upgrade-1".into(),
        target_version: "0.2.0".parse().unwrap(),
        binary_sha256: sha256_hex(&binary),
        embedded_signature: sign(&release_pkcs8, &binary).unwrap(),
        external_signature: None,
        source: crate::upgrade::types::BinarySource::LocalFile { path: source },
        network_provenance: false,
        allow_downgrade: false,
    };

    Fixture {
        _dir: dir,
        data,
        store,
        manager,
        directive,
    }
}

#[tokio::test]
async fn an_upgrade_execs_only_after_its_answer_is_delivered() {
    let Fixture {
        _dir,
        data,
        store,
        manager,
        directive,
    } = fixture();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_upgrade_manager(manager);
    let running = tokio::spawn(async move { agent.run().await });
    let delivered = CancellationToken::new();
    let (response, answer) = oneshot::channel();
    tx.send(AgentCommand::UpgradeApply {
        directive,
        response,
        answer_delivered: Some(crate::sesame::connection::ConnectionClosed::from_token(
            delivered.clone(),
        )),
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), answer)
        .await
        .expect("the upgrade was never answered")
        .unwrap()
        .unwrap();
    std::fs::write(
        store.binary_path(&"0.2.0".parse().unwrap()),
        b"not an executable",
    )
    .unwrap();

    // Well past the old fixed pause, the answer is still undelivered, so
    // the exec must not have started.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(marker_phase(&data), Some(MarkerPhase::Staged));

    delivered.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while marker_phase(&data).is_some() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the exec never followed the delivered answer");
    shutdown.cancel();
    let _ = running.await;
}

/// A directive re-sent to a node that has already exec'd (its marker is
/// still verifying) is answered without starting anything, so it must not
/// leave the node refusing new work.
#[tokio::test]
async fn a_redelivered_directive_leaves_the_node_taking_work() {
    let fixture = fixture();
    UpgradeMarker {
        schema: 1,
        upgrade_id: fixture.directive.upgrade_id.clone(),
        previous_version: "0.0.9".parse().unwrap(),
        previous_binary: "bun-v0.0.9".into(),
        target_version: "0.1.0".parse().unwrap(),
        target_binary: "bun-v0.1.0".into(),
        phase: MarkerPhase::Executed,
        boot_attempts: 1,
        pre_upgrade_instances: vec![],
    }
    .store(&UpgradeMarker::path_in(&fixture.data))
    .unwrap();
    let (mut agent, tx, shutdown) = test_agent();
    agent.set_upgrade_manager(fixture.manager);
    let draining = Arc::clone(&agent.draining);
    let running = tokio::spawn(async move { agent.run().await });
    let (response, answer) = oneshot::channel();
    tx.send(AgentCommand::UpgradeApply {
        directive: fixture.directive,
        response,
        answer_delivered: None,
    })
    .await
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), answer)
        .await
        .expect("the re-delivery was never answered")
        .unwrap()
        .unwrap();
    assert!(!draining.load(std::sync::atomic::Ordering::Relaxed));
    shutdown.cancel();
    let _ = running.await;
}
