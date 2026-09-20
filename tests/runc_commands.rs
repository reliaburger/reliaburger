//! Generation claims must cover command registration and positive retirement.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use reliaburger::grill::runc_intent::{IntentConfiguration, IntentJournal, IntentPhase};
use reliaburger::grill::{InstanceId, OciSpec};

fn journal(root: &Path) -> IntentJournal {
    IntentJournal::new(
        root.join("intents"),
        IntentConfiguration {
            bundle_directory: root.join("bundles"),
            state_directory: root.join("state"),
            image_directory: root.join("images"),
            runc_program: "runc".into(),
            rootless: true,
            node_index: 1,
        },
    )
}

fn instance() -> InstanceId {
    InstanceId("default__command-0".into())
}

fn spec() -> OciSpec {
    serde_json::from_value(serde_json::json!({
        "root": {"path": "/", "readonly": true},
        "process": {"args": ["original"], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
        "mounts": [], "linux": {"namespaces": []}
    }))
    .unwrap()
}

async fn wait_for(path: &Path) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

fn bun() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_bun").into()
}

#[tokio::test]
async fn cancelled_command_caller_retains_generation_claim_until_the_worker_finishes() {
    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path());
    let id = instance();
    let claim = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec())
        .await
        .unwrap();
    let generation = claim.record().unwrap().generation.clone();
    let commands = claim.supervise_commands(bun()).unwrap();
    let ready = root.path().join("ready");
    let release = root.path().join("release");
    let changed = root.path().join("changed");
    let arguments = vec![
        "-c".into(),
        "touch \"$READY\"; while [ ! -f \"$RELEASE\" ]; do sleep 0.02; done; touch \"$CHANGED\""
            .into(),
    ];
    let environment = BTreeMap::from([
        ("READY".into(), ready.display().to_string()),
        ("RELEASE".into(), release.display().to_string()),
        ("CHANGED".into(), changed.display().to_string()),
    ]);
    let caller = tokio::spawn(async move {
        commands
            .run(
                Path::new("/bin/sh"),
                &arguments,
                &environment,
                Duration::from_secs(20),
            )
            .await
    });
    wait_for(&ready).await;
    caller.abort();
    let _ = caller.await;
    assert!(
        journal.claim(&id, Some(generation.clone())).await.is_err(),
        "caller cancellation released a live mutation's claim"
    );
    std::fs::write(&release, "continue").unwrap();
    wait_for(&changed).await;
    let claim = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(claim) = journal.claim(&id, Some(generation.clone())).await {
                break claim;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let commands = claim
        .supervise_commands(bun())
        .unwrap()
        .seal(Duration::from_secs(15))
        .await
        .unwrap();
    // The runtime separately checks/removes its resources after all prior mutators retire.
    std::fs::remove_file(changed).unwrap();
    commands.finish(None).await.unwrap();
    assert_eq!(
        journal.inventory().await.unwrap()[0].phase,
        IntentPhase::Retired { exit_code: None }
    );
}

#[tokio::test]
async fn timed_out_mutation_is_retired_before_cleanup_and_sealing_fences_new_work() {
    let root = tempfile::tempdir().unwrap();
    let journal = journal(root.path());
    let id = instance();
    let claim = journal
        .claim(&id, None)
        .await
        .unwrap()
        .publish(&spec())
        .await
        .unwrap();
    let generation = claim.record().unwrap().generation.clone();
    let commands = claim.supervise_commands(bun()).unwrap();
    let result = commands
        .run(
            Path::new("/bin/sh"),
            &["-c".into(), "sleep 60".into()],
            &BTreeMap::new(),
            Duration::from_millis(20),
        )
        .await;
    assert!(result.is_err());
    let claim = journal.claim(&id, Some(generation.clone())).await.unwrap();
    assert_eq!(claim.record().unwrap().phase, IntentPhase::Owned);
    let early_marker = root.path().join("cannot-overtake-timeout");
    assert!(
        claim
            .supervise_commands(bun())
            .unwrap()
            .run(
                Path::new("/usr/bin/touch"),
                &[early_marker.display().to_string()],
                &BTreeMap::new(),
                Duration::from_secs(5),
            )
            .await
            .is_err()
    );
    assert!(!early_marker.exists());
    let claim = journal.claim(&id, Some(generation.clone())).await.unwrap();
    let commands = claim
        .supervise_commands(bun())
        .unwrap()
        .seal(Duration::from_secs(15))
        .await
        .unwrap();
    let marker = root.path().join("must-not-run");
    assert!(
        commands
            .run(
                Path::new("/usr/bin/touch"),
                &[marker.display().to_string()],
                &BTreeMap::new(),
                Duration::from_secs(5)
            )
            .await
            .is_err()
    );
    assert!(!marker.exists());
    let claim = journal.claim(&id, Some(generation.clone())).await.unwrap();
    assert_eq!(claim.record().unwrap().phase, IntentPhase::Retiring);
    assert!(
        claim
            .supervise_commands(bun())
            .unwrap()
            .run_cleanup(
                Path::new("/usr/bin/touch"),
                &[marker.display().to_string()],
                &BTreeMap::new(),
                Duration::from_secs(5),
            )
            .await
            .is_err(),
        "recovery must drain again before authorising cleanup commands"
    );
    assert!(!marker.exists());
    let claim = journal.claim(&id, Some(generation)).await.unwrap();
    let commands = claim
        .supervise_commands(bun())
        .unwrap()
        .seal(Duration::from_secs(15))
        .await
        .unwrap();
    let (commands, output) = commands
        .run_cleanup(
            Path::new("/bin/sh"),
            &["-c".into(), "printf cleaned; exit 0".into()],
            &BTreeMap::new(),
            Duration::from_secs(15),
        )
        .await
        .unwrap();
    assert_eq!(output.stdout, b"cleaned");
    assert_eq!(output.exit_code, Some(0));
    commands.finish(Some(23)).await.unwrap();
    assert_eq!(
        journal.inventory().await.unwrap()[0].phase,
        IntentPhase::Retired {
            exit_code: Some(23)
        }
    );
}

#[tokio::test]
async fn killed_caller_cannot_leave_a_late_mutation_after_recovered_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime_command_fixture", "--nocapture"])
        .env("RELIABURGER_COMMAND_FIXTURE", root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_for(&root.path().join("ready")).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let journal = journal(root.path());
    let id = instance();
    let generation = journal.observe(&id).await.unwrap();
    let claim = journal.claim(&id, generation).await.unwrap();
    let commands = claim
        .supervise_commands(bun())
        .unwrap()
        .seal(Duration::from_secs(15))
        .await
        .unwrap();
    std::fs::write(root.path().join("release"), "continue").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!root.path().join("late-mutation").exists());
    commands.finish(None).await.unwrap();
    assert_eq!(
        journal.inventory().await.unwrap()[0].phase,
        IntentPhase::Retired { exit_code: None }
    );
}

#[tokio::test]
async fn runtime_command_fixture() {
    let Some(root) = std::env::var_os("RELIABURGER_COMMAND_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let claim = journal(&root)
        .claim(&instance(), None)
        .await
        .unwrap()
        .publish(&spec())
        .await
        .unwrap();
    let commands = claim.supervise_commands(bun()).unwrap();
    let environment = BTreeMap::from([("ROOT".into(), root.display().to_string())]);
    commands.run(Path::new("/bin/sh"), &["-c".into(), "touch \"$ROOT/ready\"; while [ ! -f \"$ROOT/release\" ]; do sleep 0.02; done; touch \"$ROOT/late-mutation\"".into()], &environment, Duration::from_secs(30)).await.unwrap();
}
