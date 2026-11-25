//! Long-lived runtime roles must be bound before activation and drained together.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reliaburger::grill::command::{CommandId, CommandState, OwnedCommands};
use reliaburger::grill::runc_intent::{
    IntentCommands, IntentConfiguration, IntentJournal, IntentPhase, RuntimeRole,
};
use reliaburger::grill::{InstanceId, OciSpec};

#[path = "support/first_launch.rs"]
mod first_launch;

/// The owner executable, its first launch paid: every role's owner is a
/// `bun` that `start` gives 15 s to answer (#1149).
fn bun() -> PathBuf {
    first_launch::first_launch(&[(Path::new(env!("CARGO_BIN_EXE_bun")), &["--version"])]);
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

/// Wait until a role exec's command has written `ready`, while the exec is
/// still running.
///
/// The exec crosses the role owner's socket, its gate and a fresh shell, and
/// at 12 nextest threads with the disk busy that took over 15 s once (#2305).
/// A fixed deadline couldn't tell slow from refused, so this one is generous
/// and an exec that ends first fails at once with its own answer.
async fn wait_for_exec(
    ready: &Path,
    caller: &mut tokio::task::JoinHandle<std::io::Result<reliaburger::grill::ExecOutput>>,
) {
    tokio::time::timeout(Duration::from_secs(50), async {
        while !ready.exists() {
            if caller.is_finished() {
                let answer = (&mut *caller).await;
                panic!("role exec ended before its command started: {answer:?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("role exec's command never started");
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
    // The fixture child starts `bun` as its owner inside the wait below, so
    // pay that first launch here (#1149).
    let _ = bun();
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
    assert_eq!(output.output, "auxiliary");
    let ready = root.path().join("exec-ready");
    let running = executor.clone();
    let arguments = vec![
        "/bin/sh".into(),
        "-c".into(),
        "touch \"$1\"; exec sleep 60".into(),
        "exec-fixture".into(),
        ready.display().to_string(),
    ];
    let mut caller =
        tokio::spawn(async move { running.exec_role(RuntimeRole::Launcher, &arguments).await });
    wait_for_exec(&ready, &mut caller).await;
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

/// The `pkill -f` pattern for the process owners under `root` whose
/// command directory sits in `scope`: a role's directory (`launcher/`,
/// `rootless-network/`), or `""` for every owner.
fn owner_pattern(root: &Path, scope: &str) -> String {
    // The tempdir's own name, since macOS resolves `/var` to `/private/var`.
    let name = root.file_name().unwrap_or_default().to_string_lossy();
    format!("__process-owner.* --directory .*/{name}/.*{scope}")
}

/// Send `signal` to every process owner under `root`.
fn signal_owners(root: &Path, signal: &str) {
    signal_scoped_owners(root, "", signal);
}

/// Send `signal` to the process owners under `root` in `scope`.
fn signal_scoped_owners(root: &Path, scope: &str, signal: &str) {
    let pattern = owner_pattern(root, scope);
    let status = std::process::Command::new("pkill")
        .args([signal, "-f", &pattern])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "no process owner under {}",
        root.display()
    );
}

/// Stops every process owner under a root until dropped, so a failing
/// test never leaves an owner stopped behind it.
struct StoppedOwners<'a>(&'a Path, &'static str);

impl<'a> StoppedOwners<'a> {
    fn stop(root: &'a Path) -> Self {
        Self::stop_scope(root, "")
    }

    /// Stop only the owners of one role, named by its directory.
    fn stop_scope(root: &'a Path, scope: &'static str) -> Self {
        signal_scoped_owners(root, scope, "-STOP");
        Self(root, scope)
    }
}

impl Drop for StoppedOwners<'_> {
    fn drop(&mut self) {
        // Not `signal_owners`: a panic inside `drop` while unwinding aborts.
        let _ = std::process::Command::new("pkill")
            .args(["-CONT", "-f", &owner_pattern(self.0, self.1)])
            .status();
    }
}

/// How long the stall tests keep an owner stopped: more than two of its
/// 2 s request deadlines, so admission really asks again.
const OWNER_STALL: Duration = Duration::from_secs(5);

/// A role exec first asks the role's owner whether the role runs, and each
/// of those requests gets 2 s. On a host whose disk was busy, one answer
/// came late and the exec failed with "process owner request timed out"
/// although the role was running (#2305). A late answer from a live owner
/// is asked again, as the waits for a command's end already do, so a
/// stalled owner only delays the exec.
#[tokio::test]
async fn role_exec_waits_out_an_owner_that_misses_a_request_deadline() {
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
    let stopped = StoppedOwners::stop(root.path());
    let running = executor.clone();
    let caller = tokio::spawn(async move {
        running
            .exec_role(
                RuntimeRole::Launcher,
                &["/bin/sh".into(), "-c".into(), "printf late".into()],
            )
            .await
    });
    tokio::time::sleep(OWNER_STALL).await;
    drop(stopped);
    let output = caller.await.unwrap().unwrap();
    assert_eq!(output.output, "late");
    executor
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

/// Asking a stalled owner again mustn't hold the adapter mutex, and a seal
/// waits out the same stall rather than failing on it (#2320). With both an
/// exec and a seal asking a stopped owner, each attempt takes the mutex
/// afresh, so neither queues behind the other's asking. Once the owner
/// resumes the seal succeeds, and the exec never gives up asking. Which of
/// them gets in first is a race: the exec may run, refuse as sealed, or be
/// admitted and then retired by the seal, and each is correct.
#[tokio::test]
async fn a_seal_during_an_owner_stall_is_not_held_up_by_exec_admission() {
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
    let stopped = StoppedOwners::stop(root.path());
    let running = executor.clone();
    let caller = tokio::spawn(async move {
        running
            .exec_role(
                RuntimeRole::Launcher,
                &["/bin/sh".into(), "-c".into(), "printf late".into()],
            )
            .await
    });
    // Let the exec's first ask start and miss its deadline.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let sealing = executor.clone();
    let seal = tokio::spawn(async move { sealing.seal(Duration::from_secs(30)).await });
    tokio::time::sleep(OWNER_STALL - Duration::from_secs(1)).await;
    assert!(
        !seal.is_finished(),
        "the seal ended while the owner was stopped"
    );
    drop(stopped);
    // One ask in flight (2 s) each for the exec and the seal, then the
    // drain, with room for a loaded host; well under exec admission's 30 s.
    let cleanup = tokio::time::timeout(Duration::from_secs(20), seal)
        .await
        .expect("the seal didn't finish after the owner resumed")
        .unwrap()
        .expect("the seal failed after waiting out the owner stall");
    if let Err(error) = caller.await.unwrap() {
        assert!(
            !error.to_string().contains("didn't answer"),
            "the exec gave up asking: {error}"
        );
    }
    cleanup.finish(None).await.unwrap();
}

/// A seal's first look at the roles asks each role's owner for its state,
/// and each ask has a 2 s deadline. A stop that seals while the owner is
/// stalled (paused, or behind a busy disk) failed on that one late answer
/// and left the claim for recovery (#2320). The seal now asks again, within
/// its own deadline, so a stall only delays it.
#[tokio::test]
async fn a_seal_waits_out_an_owner_that_misses_a_request_deadline() {
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
    let stopped = StoppedOwners::stop(root.path());
    let sealing = executor.clone();
    let seal = tokio::spawn(async move { sealing.seal(Duration::from_secs(30)).await });
    tokio::time::sleep(OWNER_STALL).await;
    assert!(
        !seal.is_finished(),
        "the seal ended while the owner was stopped"
    );
    drop(stopped);
    let cleanup = tokio::time::timeout(Duration::from_secs(20), seal)
        .await
        .expect("the seal didn't finish after the owner resumed")
        .unwrap()
        .expect("the seal failed after waiting out the owner stall");
    assert!(matches!(
        cleanup.role_state(RuntimeRole::Launcher).await.unwrap(),
        Some(CommandState::Retired { .. })
    ));
    cleanup.finish(None).await.unwrap();
}

/// A seal keeps part of its timeout back for the drain. An owner that
/// answers only after the asking's share has run out, too late to retire
/// anything in what's left, must not cost the claim: the seal returns
/// `TimedOut` by its own deadline with the claim intact, and sealing again
/// once the owner answers succeeds.
#[tokio::test]
async fn a_seal_whose_owner_answers_too_late_to_drain_keeps_the_claim() {
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
    let stopped = StoppedOwners::stop(root.path());
    // 6 s leaves 3 s for asking and 3 s for the drain. The owner resumes
    // at 4.5 s, after the asking's share and before the seal's deadline.
    let timeout = Duration::from_secs(6);
    let started = std::time::Instant::now();
    let sealing = executor.clone();
    let seal = tokio::spawn(async move {
        let sealed = sealing.seal(timeout).await;
        (sealed, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(4500)).await;
    drop(stopped);
    let (sealed, took) = seal.await.unwrap();
    let error = sealed.expect_err("a seal drained in less than its reserve");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    assert!(took < timeout, "the seal took {took:?} of its {timeout:?}");
    assert!(matches!(
        executor.role_state(RuntimeRole::Launcher).await.unwrap(),
        Some(CommandState::Running { .. })
    ));
    executor
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

/// Asking again is only for late answers. An owner that has died while
/// its workload still runs is a real refusal: the seal reports it at once
/// rather than asking for its whole deadline, and leaves the claim for
/// recovery as before.
#[tokio::test]
async fn a_seal_against_a_dead_owner_refuses_at_once() {
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
    let Some(CommandState::Running { pid }) =
        executor.role_state(RuntimeRole::Launcher).await.unwrap()
    else {
        panic!("the launcher isn't running");
    };
    let workload = RunningWorkload(pid);
    signal_owners(root.path(), "-KILL");
    wait_for_no_owners(root.path()).await;
    let started = std::time::Instant::now();
    let error = executor
        .seal(Duration::from_secs(30))
        .await
        .expect_err("a seal succeeded with its owner dead and its workload running");
    assert!(
        error.to_string().contains("owner died"),
        "unexpected refusal: {error}"
    );
    // Asking again would take the seal's whole 27 s of asking; the bound
    // leaves room for a loaded host.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "refusal took {:?}",
        started.elapsed()
    );
    // The failed seal left the claim for recovery.
    assert!(
        executor
            .role_state(RuntimeRole::Launcher)
            .await
            .unwrap_err()
            .to_string()
            .contains("recover")
    );
    drop(workload);
}

/// Whether this generation's seal has fenced admission and started its drain.
async fn draining(root: &Path) -> bool {
    journal(root).inventory().await.unwrap()[0].phase == IntentPhase::Retiring
}

/// The drain inside a seal checks every role with its owner again before
/// it retires anything, and each ask has a 2 s deadline. A late answer
/// there failed the drain, which had already taken the claim, so the
/// instance needed recovery (#2328). The drain asks again within its own
/// timeout, so a stall only delays it.
#[tokio::test]
async fn a_seals_drain_waits_out_an_owner_that_misses_a_request_deadline() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::Launcher,
        "exec sleep 60",
    )
    .await;
    let stopped = StoppedOwners::stop(root.path());
    let seal = tokio::spawn(async move { commands.seal(Duration::from_secs(30)).await });
    tokio::time::sleep(OWNER_STALL).await;
    assert!(
        !seal.is_finished(),
        "the drain ended while the owner was stopped"
    );
    drop(stopped);
    let cleanup = tokio::time::timeout(Duration::from_secs(20), seal)
        .await
        .expect("the drain didn't finish after the owner resumed")
        .unwrap()
        .expect("the drain failed after waiting out the owner stall");
    assert!(matches!(
        cleanup.role_state(RuntimeRole::Launcher).await.unwrap(),
        Some(CommandState::Retired { .. })
    ));
    cleanup.finish(None).await.unwrap();
}

/// A host whose owners keep stalling can stall a seal twice: once in its
/// first check of the roles and again in the drain's (#2328). Both are
/// late answers, so the seal waits out each and still seals.
///
/// The first check asks the launcher's owner, then the helper's. With the
/// helper's owner stopped, an ask is nearly always waiting on the helper,
/// its launcher answer already in hand. Stopping the launcher's owner and
/// then letting the helper's go lets that ask answer, so the seal starts
/// its drain, and the drain's own check meets the stopped launcher.
#[tokio::test]
async fn a_seal_waits_out_a_second_owner_stall_in_its_drain() {
    use reliaburger::grill::command::ClaimedCommandExecutor;
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::Launcher,
        "exec sleep 60",
    )
    .await;
    let executor = ClaimedCommandExecutor::new(
        shell(commands, RuntimeRole::RootlessNetwork, "exec sleep 60").await,
    );
    let helper_stopped = StoppedOwners::stop_scope(root.path(), "rootless-network/");
    let sealing = executor.clone();
    let seal = tokio::spawn(async move { sealing.seal(Duration::from_secs(30)).await });
    tokio::time::sleep(OWNER_STALL).await;
    assert!(!seal.is_finished(), "the seal ended during the first stall");
    assert!(
        !draining(root.path()).await,
        "the seal drained with no answer"
    );
    let launcher_stopped = StoppedOwners::stop_scope(root.path(), "launcher/");
    drop(helper_stopped);
    tokio::time::sleep(OWNER_STALL).await;
    assert!(
        draining(root.path()).await,
        "the second stall didn't reach the drain"
    );
    assert!(
        !seal.is_finished(),
        "the seal ended during the second stall"
    );
    drop(launcher_stopped);
    let cleanup = tokio::time::timeout(Duration::from_secs(20), seal)
        .await
        .expect("the seal didn't finish after the owners resumed")
        .unwrap()
        .expect("the seal failed after waiting out two owner stalls");
    for role in [RuntimeRole::Launcher, RuntimeRole::RootlessNetwork] {
        assert!(matches!(
            cleanup.role_state(role).await.unwrap(),
            Some(CommandState::Retired { .. })
        ));
    }
    cleanup.finish(None).await.unwrap();
}

/// The drain asks again only on late answers. An owner that dies after
/// the first check, while its workload still runs, is a real refusal: the
/// drain reports it at once rather than asking until its deadline, and
/// leaves the claim for recovery as before.
#[tokio::test]
async fn a_seal_whose_owner_dies_before_the_drain_refuses_at_once() {
    use reliaburger::grill::command::ClaimedCommandExecutor;
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::Launcher,
        "exec sleep 60",
    )
    .await;
    let commands = shell(commands, RuntimeRole::RootlessNetwork, "exec sleep 60").await;
    let mut workloads = Vec::new();
    for role in [RuntimeRole::Launcher, RuntimeRole::RootlessNetwork] {
        let Some(CommandState::Running { pid }) = commands.role_state(role).await.unwrap() else {
            panic!("the {role:?} isn't running");
        };
        workloads.push(RunningWorkload(pid));
    }
    let executor = ClaimedCommandExecutor::new(commands);
    // As in the test above: the first check waits on the helper, with the
    // launcher's answer in hand, when the launcher's owner dies.
    let helper_stopped = StoppedOwners::stop_scope(root.path(), "rootless-network/");
    let sealing = executor.clone();
    let seal = tokio::spawn(async move { sealing.seal(Duration::from_secs(30)).await });
    tokio::time::sleep(Duration::from_secs(3)).await;
    signal_scoped_owners(root.path(), "launcher/", "-KILL");
    wait_for_no_scoped_owners(root.path(), "launcher/").await;
    drop(helper_stopped);
    let resumed = std::time::Instant::now();
    // Asking again would take the drain's whole remaining 20 s or more;
    // the bound leaves room for a loaded host.
    let error = tokio::time::timeout(Duration::from_secs(10), seal)
        .await
        .expect("the drain asked a dead owner again")
        .unwrap()
        .expect_err("a seal succeeded with its owner dead and its workload running");
    assert!(
        error.to_string().contains("owner died"),
        "unexpected refusal: {error}"
    );
    assert!(
        draining(root.path()).await,
        "the refusal came before the drain"
    );
    assert!(
        resumed.elapsed() < Duration::from_secs(10),
        "refusal took {:?}",
        resumed.elapsed()
    );
    assert!(
        executor
            .role_state(RuntimeRole::Launcher)
            .await
            .unwrap_err()
            .to_string()
            .contains("recover")
    );
    drop(workloads);
}

/// A stop reads the launcher's state before it seals, and that read is one
/// owner request with a 2 s deadline. A patient read asks again while the
/// owner answers late (#2328), so a stall only delays the stop.
#[tokio::test]
async fn a_patient_role_read_waits_out_an_owner_that_misses_a_request_deadline() {
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
    let stopped = StoppedOwners::stop(root.path());
    let reading = executor.clone();
    let read = tokio::spawn(async move {
        reading
            .role_state_patiently(RuntimeRole::Launcher, Duration::from_secs(20))
            .await
    });
    tokio::time::sleep(OWNER_STALL).await;
    assert!(
        !read.is_finished(),
        "the read ended while the owner was stopped"
    );
    drop(stopped);
    let state = tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .expect("the read didn't finish after the owner resumed")
        .unwrap()
        .expect("the read failed after waiting out the owner stall");
    assert!(matches!(state, Some(CommandState::Running { .. })));
    executor
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

/// A patient read keeps to its bound: an owner that stays stopped fails it
/// with `TimedOut` soon after the bound, with the claim intact.
#[tokio::test]
async fn a_patient_role_read_gives_up_at_its_bound() {
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
    let stopped = StoppedOwners::stop(root.path());
    let started = std::time::Instant::now();
    let error = executor
        .role_state_patiently(RuntimeRole::Launcher, Duration::from_secs(3))
        .await
        .expect_err("a read answered with its owner stopped");
    let took = started.elapsed();
    drop(stopped);
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut, "{error}");
    // The bound, plus at most one 2 s ask that started just before it.
    assert!(took < Duration::from_secs(6), "the read took {took:?}");
    executor
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

/// Kills a role's workload when dropped, for a test that killed its owner.
struct RunningWorkload(u32);

impl Drop for RunningWorkload {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &self.0.to_string()])
            .status();
    }
}

/// Wait until no process owner under `root` is left.
async fn wait_for_no_owners(root: &Path) {
    wait_for_no_scoped_owners(root, "").await;
}

/// Wait until no process owner under `root` in `scope` is left.
async fn wait_for_no_scoped_owners(root: &Path, scope: &str) {
    let pattern = owner_pattern(root, scope);
    tokio::time::timeout(Duration::from_secs(10), async {
        while std::process::Command::new("pgrep")
            .args(["-f", &pattern])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a killed process owner is still there");
}

/// Asking again is only for late answers. An exec into a launcher that has
/// exited refuses straight away; asking again would take 30 s.
#[tokio::test]
async fn role_exec_into_an_exited_launcher_refuses_at_once() {
    use reliaburger::grill::command::ClaimedCommandExecutor;
    let root = tempfile::tempdir().unwrap();
    let commands = shell(fresh(root.path()).await, RuntimeRole::Launcher, "exit 0").await;
    terminal(&commands, RuntimeRole::Launcher).await;
    let executor = ClaimedCommandExecutor::new(commands);
    let started = std::time::Instant::now();
    let error = executor
        .exec_role(RuntimeRole::Launcher, &["/bin/true".into()])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("not running"),
        "unexpected refusal: {error}"
    );
    // Well under one 2 s owner request: nothing was asked again.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "refusal took {:?}",
        started.elapsed()
    );
    executor
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
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
    let mut caller =
        tokio::spawn(async move { running.exec_role(RuntimeRole::Launcher, &arguments).await });
    wait_for_exec(&ready, &mut caller).await;
    caller.abort();
    let _ = caller.await;
    // Sealing retires the launcher and the cancelled exec. On a loaded host
    // that overran 15 s (#2305); the bound is how long cleanup may take,
    // not what the test checks, and it stays inside nextest's 60 s kill.
    let cleanup = executor.seal(Duration::from_secs(40)).await.unwrap();
    std::fs::write(root.path().join("release"), "continue").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!root.path().join("late-exec").exists());
    cleanup.finish(None).await.unwrap();
}

#[tokio::test]
async fn a_retired_network_helper_can_be_replaced_without_restarting_the_launcher() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::Launcher,
        "exec sleep 60",
    )
    .await;
    let launcher = commands.role_state(RuntimeRole::Launcher).await.unwrap();
    let commands = shell(commands, RuntimeRole::RootlessNetwork, "exit 13").await;
    assert_eq!(
        terminal(&commands, RuntimeRole::RootlessNetwork).await,
        CommandState::Retired {
            exit_code: Some(13)
        }
    );
    drop(commands);
    let commands = shell(
        recover(root.path()).await,
        RuntimeRole::RootlessNetwork,
        "exec sleep 60",
    )
    .await;
    assert_eq!(
        commands.role_state(RuntimeRole::Launcher).await.unwrap(),
        launcher
    );
    assert_eq!(
        role_collection(root.path(), "rootless-network")
            .inventory()
            .await
            .unwrap()
            .len(),
        2
    );
    commands
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

#[tokio::test]
async fn abandoned_helper_replacement_is_cancelled_before_another_helper_can_start() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::RootlessNetwork,
        "exit 0",
    )
    .await;
    terminal(&commands, RuntimeRole::RootlessNetwork).await;
    drop(commands);
    let collection = role_collection(root.path(), "rootless-network");
    let abandoned = collection
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    let commands = shell(
        recover(root.path()).await,
        RuntimeRole::RootlessNetwork,
        "exec sleep 60",
    )
    .await;
    assert_eq!(
        collection.state(&abandoned).await.unwrap(),
        CommandState::Cancelled
    );
    assert!(collection.start(&abandoned).await.is_err());
    commands
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_extra_executed_helper_refuses_replacement_and_retirement() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::RootlessNetwork,
        "exit 0",
    )
    .await;
    terminal(&commands, RuntimeRole::RootlessNetwork).await;
    drop(commands);
    let collection = role_collection(root.path(), "rootless-network");
    let extra = collection
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    collection.start(&extra).await.unwrap();
    collection
        .wait(&extra, Duration::from_secs(15))
        .await
        .unwrap();
    assert!(
        recover(root.path())
            .await
            .start_role(
                RuntimeRole::RootlessNetwork,
                Path::new("/bin/sh"),
                &["-c".into(), "exit 0".into()],
                &BTreeMap::new()
            )
            .await
            .is_err()
    );
    assert!(
        recover(root.path())
            .await
            .seal(Duration::from_secs(15))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn helper_replacement_never_authorises_a_second_workload_launcher() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(fresh(root.path()).await, RuntimeRole::Launcher, "exit 0").await;
    terminal(&commands, RuntimeRole::Launcher).await;
    let marker = root.path().join("repeated-workload");
    assert!(
        commands
            .start_role(
                RuntimeRole::Launcher,
                Path::new("/usr/bin/touch"),
                &[marker.display().to_string()],
                &BTreeMap::new()
            )
            .await
            .is_err()
    );
    assert!(!marker.exists());
    recover(root.path())
        .await
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(Some(0))
        .await
        .unwrap();
}

#[tokio::test]
async fn helper_history_refuses_duplicate_missing_or_unretired_bindings() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::RootlessNetwork,
        "exit 0",
    )
    .await;
    terminal(&commands, RuntimeRole::RootlessNetwork).await;
    drop(commands);
    let path = record_path(root.path());
    let original = std::fs::read(&path).unwrap();
    let collection = role_collection(root.path(), "rootless-network");
    let prepared = collection
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&original).unwrap();
    for invalid in [
        record["roles"]["rootless_network"].clone(),
        serde_json::json!("command-missing"),
        serde_json::to_value(prepared).unwrap(),
    ] {
        let mut damaged = record.clone();
        damaged["roles"]["retired_rootless_network"] = serde_json::json!([invalid]);
        std::fs::write(&path, serde_json::to_vec(&damaged).unwrap()).unwrap();
        assert!(
            recover(root.path())
                .await
                .seal(Duration::from_secs(15))
                .await
                .is_err()
        );
    }
    std::fs::write(path, original).unwrap();
    recover(root.path())
        .await
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

/// Bind a prepared helper as the current one, moving the old binding into
/// the history, as `start_role` does just before it starts the helper.
fn bind_prepared_helper(root: &Path, prepared: &CommandId) {
    let path = record_path(root);
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let previous = record["roles"]["rootless_network"].clone();
    record["roles"]["retired_rootless_network"]
        .as_array_mut()
        .unwrap()
        .push(previous);
    record["roles"]["rootless_network"] = serde_json::to_value(prepared).unwrap();
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
}

/// Bun execs while it replaces a dead network helper: the new helper is bound,
/// but the process that would have started it is gone. The next process must
/// cancel that helper and start another, not refuse adoption (#778).
#[tokio::test]
async fn the_next_process_replaces_a_helper_its_predecessor_bound_but_never_started() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::RootlessNetwork,
        "exit 0",
    )
    .await;
    terminal(&commands, RuntimeRole::RootlessNetwork).await;
    drop(commands);
    let collection = role_collection(root.path(), "rootless-network");
    let abandoned = collection
        .prepare(
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap();
    bind_prepared_helper(root.path(), &abandoned);
    let commands = recover(root.path()).await;
    assert_eq!(
        commands
            .role_state(RuntimeRole::RootlessNetwork)
            .await
            .unwrap(),
        Some(CommandState::Prepared)
    );
    assert_eq!(
        commands.settle_network_helper().await.unwrap(),
        Some(CommandState::Cancelled)
    );
    assert!(collection.start(&abandoned).await.is_err());
    let commands = shell(commands, RuntimeRole::RootlessNetwork, "exec sleep 60").await;
    assert!(matches!(
        commands.settle_network_helper().await.unwrap(),
        Some(CommandState::Running { .. })
    ));
    assert_eq!(collection.inventory().await.unwrap().len(), 3);
    commands
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

/// A helper that is still exiting when the next process replaces it gets a
/// bounded wait, like an earlier short command does (#778).
#[tokio::test]
async fn helper_replacement_waits_for_a_predecessor_helper_that_is_still_exiting() {
    let root = tempfile::tempdir().unwrap();
    let release = root.path().join("release");
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::RootlessNetwork,
        &format!(
            "while [ ! -f '{}' ]; do sleep 0.02; done",
            release.display()
        ),
    )
    .await;
    drop(commands);
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(release, "exit").unwrap();
    });
    let commands = recover(root.path())
        .await
        .start_role(
            RuntimeRole::RootlessNetwork,
            Path::new("/bin/sh"),
            &["-c".into(), "exec sleep 60".into()],
            &BTreeMap::new(),
        )
        .await
        .expect("a helper that was still exiting refused its replacement");
    releaser.await.unwrap();
    commands
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}

/// The wait is bounded: a helper that keeps running still refuses its
/// replacement, and keeps running.
#[tokio::test]
async fn helper_replacement_still_refuses_a_helper_that_keeps_running() {
    let root = tempfile::tempdir().unwrap();
    let commands = shell(
        fresh(root.path()).await,
        RuntimeRole::RootlessNetwork,
        "exec sleep 60",
    )
    .await;
    drop(commands);
    let started = std::time::Instant::now();
    let error = recover(root.path())
        .await
        .start_role(
            RuntimeRole::RootlessNetwork,
            Path::new("/bin/sh"),
            &["-c".into(), "exit 0".into()],
            &BTreeMap::new(),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("still running"),
        "unexpected refusal: {error}"
    );
    assert!(started.elapsed() < Duration::from_secs(15));
    let commands = recover(root.path()).await;
    assert!(matches!(
        commands
            .role_state(RuntimeRole::RootlessNetwork)
            .await
            .unwrap(),
        Some(CommandState::Running { .. })
    ));
    assert_eq!(
        role_collection(root.path(), "rootless-network")
            .inventory()
            .await
            .unwrap()
            .len(),
        1
    );
    commands
        .seal(Duration::from_secs(15))
        .await
        .unwrap()
        .finish(None)
        .await
        .unwrap();
}
