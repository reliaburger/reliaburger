//! Shared executor handles must preserve generation fences across cancellation.
use std::path::Path;
use std::time::Duration;

use reliaburger::grill::command::{ClaimedCommandExecutor, RuntimeCommandExecutor};
use reliaburger::grill::runc_intent::{IntentConfiguration, IntentJournal, IntentPhase};
use reliaburger::grill::{InstanceId, OciSpec};

async fn executor(root: &Path) -> (IntentJournal, ClaimedCommandExecutor) {
    let journal = IntentJournal::new(
        root.join("intents"),
        IntentConfiguration {
            bundle_directory: root.join("bundles"),
            state_directory: root.join("state"),
            image_directory: root.join("images"),
            runc_program: "runc".into(),
            rootless: true,
            node_index: 1,
        },
    );
    let spec: OciSpec = serde_json::from_value(serde_json::json!({
        "root": {"path": "/", "readonly": true},
        "process": {"args": ["original"], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
        "mounts": [], "linux": {"namespaces": []}
    }))
    .unwrap();
    let claim = journal
        .claim(&InstanceId("default__executor-0".into()), None)
        .await
        .unwrap()
        .publish(&spec)
        .await
        .unwrap();
    (
        journal,
        ClaimedCommandExecutor::new(
            claim
                .supervise_commands(env!("CARGO_BIN_EXE_bun").into())
                .unwrap(),
        ),
    )
}

#[tokio::test]
async fn stale_executor_clones_refuse_after_cleanup_seals_admission() {
    let root = tempfile::tempdir().unwrap();
    let (journal, executor) = executor(root.path()).await;
    let stale = executor.clone();
    let output = executor
        .output("/bin/sh", &["-c", "printf output; exit 7"])
        .await
        .unwrap();
    assert_eq!(output.exit_code, Some(7));
    assert_eq!(output.stdout, b"output");
    let cleanup = executor.seal(Duration::from_secs(15)).await.unwrap();
    let marker = root.path().join("must-not-exist");
    assert!(
        stale
            .output("/usr/bin/touch", &[marker.to_str().unwrap()])
            .await
            .is_err()
    );
    assert!(!marker.exists());
    assert_eq!(
        cleanup
            .output("/bin/echo", &["cleanup"])
            .await
            .unwrap()
            .exit_code,
        Some(0)
    );
    cleanup.finish(None).await.unwrap();
    assert!(
        stale
            .output("/usr/bin/touch", &[marker.to_str().unwrap()])
            .await
            .is_err()
    );
    assert!(!marker.exists());
    assert_eq!(
        journal.inventory().await.unwrap()[0].phase,
        IntentPhase::Retired { exit_code: None }
    );
}

#[tokio::test]
async fn cancelled_executor_caller_does_not_discard_the_workers_retained_claim() {
    let root = tempfile::tempdir().unwrap();
    let (journal, executor) = executor(root.path()).await;
    let ready = root.path().join("ready");
    let release = root.path().join("release");
    let runner = executor.clone();
    let ready_arg = ready.clone();
    let release_arg = release.clone();
    let caller = tokio::spawn(async move {
        runner
            .output(
                "/bin/sh",
                &[
                    "-c",
                    "touch \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.02; done",
                    "fixture",
                    ready_arg.to_str().unwrap(),
                    release_arg.to_str().unwrap(),
                ],
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    let _ = caller.await;
    let id = InstanceId("default__executor-0".into());
    assert!(
        journal
            .claim(&id, journal.observe(&id).await.unwrap())
            .await
            .is_err()
    );
    std::fs::write(release, "continue").unwrap();
    let output = executor.output("/bin/echo", &["next"]).await.unwrap();
    assert_eq!(output.stdout, b"next\n");
    executor
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}
