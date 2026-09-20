//! Long-lived runtime roles must be bound before activation and drained together.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reliaburger::grill::command::{CommandState, OwnedCommands};
use reliaburger::grill::runc_intent::{
    IntentCommands, IntentConfiguration, IntentJournal, IntentPhase, RuntimeRole,
};
use reliaburger::grill::{InstanceId, OciSpec};

fn bun() -> PathBuf {
    env!("CARGO_BIN_EXE_bun").into()
}

fn instance() -> InstanceId {
    InstanceId("default__roles-0".into())
}

fn journal(root: &Path) -> IntentJournal {
    IntentJournal::new(
        root.join("intents"),
        IntentConfiguration {
            bundle_directory: root.join("bundles"),
            state_directory: root.join("state"),
            image_directory: root.join("images"),
            runc_program: "runc".into(),
            rootless: true,
            dns_nameserver: None,
            node_index: 1,
        },
    )
}

async fn fresh(root: &Path) -> IntentCommands {
    let spec: OciSpec = serde_json::from_value(serde_json::json!({
        "root": {"path": "/", "readonly": true},
        "process": {"args": ["original"], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
        "mounts": [], "linux": {"namespaces": []}
    }))
    .unwrap();
    journal(root)
        .claim(&instance(), None)
        .await
        .unwrap()
        .publish(&spec)
        .await
        .unwrap()
        .supervise_commands(bun())
        .unwrap()
}

async fn recover(root: &Path) -> IntentCommands {
    let journal = journal(root);
    let generation = journal.observe(&instance()).await.unwrap();
    journal
        .claim(&instance(), generation)
        .await
        .unwrap()
        .supervise_commands(bun())
        .unwrap()
}

async fn shell(commands: IntentCommands, role: RuntimeRole, script: &str) -> IntentCommands {
    commands
        .start_role(
            role,
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap()
}

async fn terminal(commands: &IntentCommands, role: RuntimeRole) -> CommandState {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = commands.role_state(role).await.unwrap().unwrap();
            if matches!(
                state,
                CommandState::Retired { .. } | CommandState::Cancelled
            ) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

fn record_path(root: &Path) -> PathBuf {
    root.join("intents/records")
        .join(instance().0)
        .join("intent.json")
}

fn role_collection(root: &Path, role: &str) -> OwnedCommands {
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(record_path(root)).unwrap()).unwrap();
    OwnedCommands::new(
        root.join("intents/records")
            .join(instance().0)
            .join("generations")
            .join(record["generation"].as_str().unwrap())
            .join(role),
        bun(),
    )
}

#[tokio::test]
async fn launcher_exit_and_logs_survive_adapter_reconstruction() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::Launcher,
        "printf launched; exit 7",
    )
    .await;
    assert_eq!(
        terminal(&commands, RuntimeRole::Launcher).await,
        CommandState::Retired { exit_code: Some(7) }
    );
    drop(commands);
    let commands = recover(root.path()).await;
    assert_eq!(
        commands.role_state(RuntimeRole::Launcher).await.unwrap(),
        Some(CommandState::Retired { exit_code: Some(7) })
    );
    let stem = commands
        .role_log_stem(RuntimeRole::Launcher)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read(stem.with_extension("stdout")).unwrap(),
        b"launched"
    );
    commands
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(Some(7))
        .await
        .unwrap();
    assert_eq!(
        journal(root.path()).inventory().await.unwrap()[0].phase,
        IntentPhase::Retired { exit_code: Some(7) }
    );
}

#[tokio::test]
async fn recovered_sealing_drains_both_roles_without_blocking_short_observations() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::Launcher,
        "exec sleep 60",
    )
    .await;
    let commands = shell(commands, RuntimeRole::RootlessNetwork, "exec sleep 60").await;
    let (commands, output) = commands
        .run(
            Path::new("/bin/sh"),
            &["-c".into(), "printf observed".into()],
            &BTreeMap::new(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(output.stdout, b"observed");
    drop(commands);
    let commands = recover(root.path())
        .await
        .seal(Duration::from_secs(15))
        .await
        .unwrap();
    for role in [RuntimeRole::Launcher, RuntimeRole::RootlessNetwork] {
        assert!(matches!(
            commands.role_state(role).await.unwrap(),
            Some(CommandState::Retired { .. })
        ));
    }
    assert!(
        commands
            .start_role(
                RuntimeRole::Launcher,
                Path::new("/bin/true"),
                &[],
                &BTreeMap::new()
            )
            .await
            .is_err()
    );
    recover(root.path())
        .await
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

#[tokio::test]
async fn unpublished_prepared_role_is_cancelled_during_recovery() {
    let root = tempfile::tempdir().unwrap();
    drop(fresh(root.path()).await);
    // Crash window after durable command preparation but before binding it to intent.
    let collection = role_collection(root.path(), "launcher");
    let id = collection
        .prepare(Path::new("/bin/true"), &[], &BTreeMap::new())
        .await
        .unwrap();
    let commands = recover(root.path())
        .await
        .seal(Duration::from_secs(15))
        .await
        .unwrap();
    assert_eq!(
        collection.state(&id).await.unwrap(),
        CommandState::Cancelled
    );
    assert!(collection.start(&id).await.is_err());
    commands.finish(None).await.unwrap();
}

#[tokio::test]
async fn corrupted_role_binding_refuses_before_retiring_any_live_role() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::Launcher,
        "exec sleep 60",
    )
    .await;
    let commands = shell(commands, RuntimeRole::RootlessNetwork, "exec sleep 60").await;
    drop(commands);
    let path = record_path(root.path());
    let original = std::fs::read(&path).unwrap();
    let mut record: serde_json::Value = serde_json::from_slice(&original).unwrap();
    record["roles"]["rootless_network"] = serde_json::json!("command-missing");
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    assert!(
        recover(root.path())
            .await
            .seal(Duration::from_secs(15))
            .await
            .is_err()
    );
    let collection = role_collection(root.path(), "launcher");
    let id = collection.inventory().await.unwrap().pop().unwrap();
    assert!(matches!(
        collection.state(&id).await.unwrap(),
        CommandState::Running { .. }
    ));
    std::fs::write(&path, original).unwrap();
    recover(root.path())
        .await
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

#[tokio::test]
async fn unbound_executed_role_cannot_be_mistaken_for_an_unlaunched_attempt() {
    let root = tempfile::tempdir().unwrap();
    drop(fresh(root.path()).await);
    let collection = role_collection(root.path(), "launcher");
    let id = collection
        .prepare(Path::new("/bin/true"), &[], &BTreeMap::new())
        .await
        .unwrap();
    collection.start(&id).await.unwrap();
    collection.wait(&id, Duration::from_secs(15)).await.unwrap();
    let commands = recover(root.path()).await;
    assert!(commands.role_state(RuntimeRole::Launcher).await.is_err());
    assert!(commands.seal(Duration::from_secs(15)).await.is_err());
    assert_eq!(
        journal(root.path()).inventory().await.unwrap()[0].phase,
        IntentPhase::Retiring
    );
}

#[tokio::test]
async fn stale_shared_handle_cannot_start_roles_after_sealing() {
    use reliaburger::grill::command::ClaimedCommandExecutor;
    let root = tempfile::tempdir().unwrap();
    let executor = ClaimedCommandExecutor::new(fresh(root.path()).await);
    executor
        .start_role(
            RuntimeRole::Launcher,
            Path::new("/bin/sh"),
            &["-c".into(), "exec sleep 60".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        executor.role_state(RuntimeRole::Launcher).await.unwrap(),
        Some(CommandState::Running { .. })
    ));
    assert!(
        executor
            .role_log_stem(RuntimeRole::Launcher)
            .await
            .unwrap()
            .is_some()
    );
    let cleanup = executor.seal(Duration::from_secs(15)).await.unwrap();
    assert!(
        executor
            .start_role(
                RuntimeRole::RootlessNetwork,
                Path::new("/bin/true"),
                &[],
                &BTreeMap::new()
            )
            .await
            .is_err()
    );
    cleanup.finish(None).await.unwrap();
}

#[tokio::test]
async fn launcher_and_helper_survive_caller_sigkill_until_recovered_retirement() {
    let root = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "runtime_roles_fixture", "--nocapture"])
        .env("RELIABURGER_ROLES_FIXTURE", root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !root.path().join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "role fixture exited before readiness"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let commands = recover(root.path()).await;
    for role in [RuntimeRole::Launcher, RuntimeRole::RootlessNetwork] {
        assert!(matches!(
            commands.role_state(role).await.unwrap(),
            Some(CommandState::Running { .. })
        ));
    }
    let commands = commands.seal(Duration::from_secs(15)).await.unwrap();
    std::fs::write(root.path().join("release"), "continue").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!root.path().join("late-launcher").exists());
    assert!(!root.path().join("late-network").exists());
    commands.finish(None).await.unwrap();
}

#[tokio::test]
async fn runtime_roles_fixture() {
    let Some(root) = std::env::var_os("RELIABURGER_ROLES_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let mut commands = fresh(&root).await;
    for (role, marker) in [
        (RuntimeRole::Launcher, "late-launcher"),
        (RuntimeRole::RootlessNetwork, "late-network"),
    ] {
        let environment = BTreeMap::from([
            ("ROOT".into(), root.display().to_string()),
            ("MARKER".into(), marker.into()),
        ]);
        commands = commands.start_role(role, Path::new("/bin/sh"), &["-c".into(), "for i in $(seq 1 1000); do if [ -f \"$ROOT/release\" ]; then touch \"$ROOT/$MARKER\"; exit; fi; sleep 0.02; done".into()], &environment).await.unwrap();
    }
    std::fs::write(root.join("ready"), "ready").unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    commands
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

#[tokio::test]
async fn role_exec_does_not_hold_the_adapter_lock_while_cleanup_retires_it() {
    use reliaburger::grill::command::ClaimedCommandExecutor;
    let root = tempfile::tempdir().unwrap();
    let executor = ClaimedCommandExecutor::new(
        shell(
            fresh(root.path()).await,
            RuntimeRole::Launcher,
            "exec sleep 60",
        )
        .await,
    );
    let output = executor
        .exec_role(
            RuntimeRole::Launcher,
            &["/bin/sh".into(), "-c".into(), "printf auxiliary".into()],
        )
        .await
        .unwrap();
    assert_eq!(output, "auxiliary");
    let ready = root.path().join("exec-ready");
    let running = executor.clone();
    let arguments = vec![
        "/bin/sh".into(),
        "-c".into(),
        "touch \"$1\"; exec sleep 60".into(),
        "exec-fixture".into(),
        ready.display().to_string(),
    ];
    let caller =
        tokio::spawn(async move { running.exec_role(RuntimeRole::Launcher, &arguments).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let cleanup = tokio::time::timeout(
        Duration::from_secs(10),
        executor.seal(Duration::from_secs(5)),
    )
    .await
    .expect("exec held the adapter mutex during cleanup")
    .unwrap();
    assert!(caller.await.unwrap().is_err());
    assert!(
        executor
            .exec_role(RuntimeRole::Launcher, &["/bin/true".into()])
            .await
            .is_err()
    );
    cleanup.finish(None).await.unwrap();
}

#[tokio::test]
async fn cancelled_role_exec_cannot_complete_a_late_mutation() {
    use reliaburger::grill::command::ClaimedCommandExecutor;
    let root = tempfile::tempdir().unwrap();
    let executor = ClaimedCommandExecutor::new(
        shell(
            fresh(root.path()).await,
            RuntimeRole::Launcher,
            "exec sleep 60",
        )
        .await,
    );
    let ready = root.path().join("exec-ready");
    let running = executor.clone();
    let arguments = vec!["/bin/sh".into(), "-c".into(), "touch \"$1/exec-ready\"; while [ ! -f \"$1/release\" ]; do sleep 0.02; done; touch \"$1/late-exec\"".into(), "exec-fixture".into(), root.path().display().to_string()];
    let caller =
        tokio::spawn(async move { running.exec_role(RuntimeRole::Launcher, &arguments).await });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    let _ = caller.await;
    let cleanup = executor.seal(Duration::from_secs(15)).await.unwrap();
    std::fs::write(root.path().join("release"), "continue").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!root.path().join("late-exec").exists());
    cleanup.finish(None).await.unwrap();
}
