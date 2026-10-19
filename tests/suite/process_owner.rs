//! Actual-binary contracts for the foreground process owner, before agent wiring.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use reliaburger::grill::process_owner;

/// A fresh 32-hex-digit generation, unique per test process and owner.
fn generation() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{:016x}{:016x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// An owner record in the current format, running `command`.
fn owner_record(
    nonce: &str,
    command: serde_json::Value,
    phase: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "schema": 3,
        "boot_id": process_owner::current_boot_id().unwrap().unwrap(),
        "nonce": nonce,
        "command": command,
        "environment": {},
        "phase": phase,
        "launch": {
            "instance_id": "default__owner-0",
            "spec": {
                "root": {"path": "/", "readonly": false},
                "process": {
                    "args": command,
                    "env": [],
                    "cwd": "/",
                    "user": {"uid": 0, "gid": 0},
                },
                "mounts": [],
                "linux": {"namespaces": []},
            },
        },
    })
}

struct Owner {
    directory: tempfile::TempDir,
    nonce: String,
    socket: PathBuf,
    child: Child,
}

impl Owner {
    fn start(script: &str) -> Self {
        Self::start_with_oom_score(script, None)
    }

    /// Start an owner whose launch spec asks for `oom_score_adj`, as Bun's
    /// generated workload specs do.
    fn start_with_oom_score(script: &str, oom_score_adj: Option<i32>) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let nonce = generation();
        let mut spec = owner_record(
            &nonce,
            serde_json::json!(["/bin/sh", "-c", script]),
            serde_json::json!({ "state": "prepared" }),
        );
        if let Some(score) = oom_score_adj {
            spec["launch"]["spec"]["process"]["oomScoreAdj"] = serde_json::json!(score);
        }
        std::fs::write(
            directory.path().join("owner.json"),
            serde_json::to_vec(&spec).unwrap(),
        )
        .unwrap();
        let socket = process_owner::socket_path(&serde_json::from_value(spec).unwrap());
        // On macOS the first `exec` of Bun in each test process waits for the
        // host's executable assessment, 4 to 13 s for the debug build at load
        // average 300 to 440, and longer when other worktrees' new binaries
        // are queued ahead. Pay it here, under its own bound, so the phase
        // deadlines below time the owner, not the queue (#1134).
        crate::first_launch::first_launch(&[(
            std::path::Path::new(env!("CARGO_BIN_EXE_bun")),
            &["--version"],
        )]);
        let child = Command::new(env!("CARGO_BIN_EXE_bun"))
            .args(["__process-owner", "--directory"])
            .arg(directory.path())
            .arg("--generation")
            .arg(&nonce)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(directory.path().join("owner.log")).unwrap())
            .spawn()
            .unwrap();
        Self {
            directory,
            nonce,
            socket,
            child,
        }
    }

    /// Run a second owner helper against this owner's directory.
    fn launch_duplicate(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_bun"))
            .args(["__process-owner", "--directory"])
            .arg(self.directory.path())
            .arg("--generation")
            .arg(&self.nonce)
            .output()
            .unwrap()
    }

    fn wait_phase(&mut self, expected: &str) -> serde_json::Value {
        // `start` has already paid Bun's first launch, so this times only the
        // owner and its gate. Retirement latency is tested separately.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let record: serde_json::Value = serde_json::from_slice(
                &std::fs::read(self.directory.path().join("owner.json")).unwrap(),
            )
            .unwrap();
            // A running owner writes its record once more, when the gate
            // reports on its `exec` (#905); only then is the record settled.
            let settled = expected != "running" || !record["gate"].is_null();
            if record["phase"]["state"] == expected && settled {
                return record;
            }
            if self.child.try_wait().unwrap().is_some() {
                let final_record: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(self.directory.path().join("owner.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    final_record["phase"]["state"],
                    expected,
                    "owner exited: {}",
                    self.diagnostics()
                );
                return final_record;
            }
            assert!(
                Instant::now() < deadline,
                "owner did not reach {expected}: {record}; {}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn diagnostics(&self) -> String {
        std::fs::read_to_string(self.directory.path().join("owner.log")).unwrap()
    }

    /// Send one request and parse the owner's one-line reply.
    ///
    /// The owner gives each client 100 ms from accepting it to receiving the
    /// whole line, then hangs up without acting (#521). The request goes out
    /// in a single write, as the agent's client sends it: `writeln!` with a
    /// JSON value issued one write per token, 18 for a kill, and a test
    /// thread descheduled between two of them missed the deadline. Even one
    /// write can miss it on a loaded machine if the thread stalls between
    /// connecting and writing, so a hang-up before any reply is sent again.
    /// That is safe: the owner always answers a request it has read.
    fn request(&self, nonce: &str, action: &str) -> serde_json::Value {
        let request = format!(
            "{}\n",
            serde_json::json!({"nonce": nonce, "action": action})
        );
        let mut dropped = Vec::new();
        for _ in 0..5 {
            let mut socket = UnixStream::connect(&self.socket).unwrap_or_else(|error| {
                panic!(
                    "connect for {action}: {error}; earlier attempts {dropped:?}; owner log: {}",
                    self.diagnostics()
                )
            });
            let mut line = String::new();
            let sent = socket
                .write_all(request.as_bytes())
                .and_then(|()| socket.set_read_timeout(Some(Duration::from_secs(2))))
                .and_then(|()| BufReader::new(&socket).read_line(&mut line));
            match sent {
                // A hang-up before the owner read the request: try again.
                Ok(0) | Err(_) if line.is_empty() => dropped.push(format!("{sent:?}")),
                _ => {
                    return serde_json::from_str(&line).unwrap_or_else(|error| {
                        panic!(
                            "owner reply to {action} was unreadable ({error}, raw {line:?}); \
                             owner log: {}",
                            self.diagnostics()
                        )
                    });
                }
            }
        }
        panic!(
            "owner hung up on {action} without a reply every time: {dropped:?}; owner log: {}",
            self.diagnostics()
        )
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        if let Ok(mut socket) = UnixStream::connect(&self.socket) {
            let _ = writeln!(
                socket,
                "{}",
                serde_json::json!({"nonce": self.nonce, "action": "kill"})
            );
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn owner_persists_observed_exit_without_an_agent_adoption_record() {
    let mut owner = Owner::start("exit 7");
    let record = owner.wait_phase("retired");
    assert_eq!(record["phase"]["exit_code"], 7);
    assert!(!owner.directory.path().join("instance.json").exists());
}

#[test]
fn owner_retires_children_after_the_foreground_parent_exits() {
    let marker = tempfile::tempdir_in("/tmp").unwrap();
    let path = marker.path().join("child.pid");
    let release = marker.path().join("exit-parent");
    let mut owner = Owner::start(&format!(
        "sleep 30 & echo $! > '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; exit 9",
        path.display(),
        release.display()
    ));
    owner.wait_phase("running");
    let started = Instant::now();
    std::fs::write(release, "exit").unwrap();
    let record = owner.wait_phase("retired");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "cleanup waited for natural child exit"
    );
    assert_eq!(record["phase"]["exit_code"], 9);
    let pid: u32 = std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        reliaburger::grill::records::process_start_time(pid).is_none(),
        "owner reported retirement with a surviving child"
    );
}

/// The workload's own OOM score, as the kernel reports it once user code runs.
#[cfg(target_os = "linux")]
fn workload_oom_score(oom_score_adj: Option<i32>) -> i32 {
    let marker = tempfile::tempdir_in("/tmp").unwrap();
    let path = marker.path().join("score");
    let mut owner = Owner::start_with_oom_score(
        &format!("cat /proc/self/oom_score_adj > '{}'", path.display()),
        oom_score_adj,
    );
    owner.wait_phase("retired");
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The test process's own score, which a child inherits by default.
#[cfg(target_os = "linux")]
fn own_oom_score() -> i32 {
    std::fs::read_to_string("/proc/self/oom_score_adj")
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// #984: Bun's unit runs it at -900 and children inherit that, so without
/// this a workload was as safe from the OOM killer as Bun itself.
#[cfg(target_os = "linux")]
#[test]
fn owned_workload_starts_with_the_score_its_spec_asks_for() {
    // Ask for more than the test runs at, not for the product's +500:
    // GitHub's runner already starts job processes at 500, so asking for
    // that wouldn't show the gate raising anything. The specs' own value is
    // pinned by the unit tests in `grill::oci`.
    let own = own_oom_score();
    assert!(own < 1000, "the test can't run at the maximum score");
    let wanted = own + (1001 - own) / 2;
    assert_eq!(workload_oom_score(Some(wanted)), wanted);
}

#[cfg(target_os = "linux")]
#[test]
fn owned_workload_without_a_score_keeps_the_one_it_inherits() {
    assert_eq!(workload_oom_score(None), own_oom_score());
}

/// The gate only ever raises a score: lowering one needs privilege, and a
/// workload already more killable than asked stays that way.
#[cfg(target_os = "linux")]
#[test]
fn owned_workload_never_has_its_score_lowered() {
    assert_eq!(workload_oom_score(Some(-1000)), own_oom_score());
}

#[test]
fn wrong_generation_cannot_signal_a_live_owner() {
    let mut owner = Owner::start("sleep 30");
    owner.wait_phase("running");
    assert_eq!(
        owner.request("another-generation", "kill")["error"],
        "owner generation mismatch"
    );
    assert_eq!(
        owner.request(&owner.nonce, "status")["phase"]["state"],
        "running"
    );
    assert_eq!(owner.request(&owner.nonce, "kill")["accepted"], true);
    let record = owner.wait_phase("retired");
    assert!(record["phase"]["exit_code"].is_null());
}

#[test]
fn execution_gate_refuses_eof_before_activation() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let marker = directory.path().join("ran");
    let spec = owner_record(
        &generation(),
        serde_json::json!(["/bin/sh", "-c", format!("touch '{}'", marker.display())]),
        serde_json::json!({"state": "prepared"}),
    );
    std::fs::write(
        directory.path().join("owner.json"),
        serde_json::to_vec(&spec).unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_bun"))
        .args(["__process-exec-gate", "--directory"])
        .arg(directory.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!marker.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("activation"));
}

#[test]
fn duplicate_owner_cannot_launch_or_replace_a_live_generation() {
    let mut owner = Owner::start("sleep 30");
    let running = owner.wait_phase("running");
    let output = owner.launch_duplicate();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
    assert_eq!(
        owner.request(&owner.nonce, "status")["phase"],
        running["phase"]
    );
    owner.request(&owner.nonce, "kill");
    owner.wait_phase("retired");
}

#[test]
fn retired_generation_cannot_execute_again() {
    let mut owner = Owner::start("exit 0");
    owner.wait_phase("retired");
    assert!(owner.child.wait().unwrap().success());
    let output = owner.launch_duplicate();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already started"));
    owner.wait_phase("retired");
}

#[test]
fn malformed_and_abandoned_clients_do_not_end_the_owner() {
    let mut owner = Owner::start("sleep 30");
    owner.wait_phase("running");
    let path = owner.socket.clone();
    // This connection never sends a newline. Its timeout must let the next
    // client through without preventing exit observation or future cleanup.
    let stalled = UnixStream::connect(&path).unwrap();
    let mut oversized = serde_json::to_vec(&serde_json::json!({
        "nonce": owner.nonce, "action": "kill"
    }))
    .unwrap();
    oversized.extend(vec![b' '; 64 * 1024]);
    oversized.push(b'\n');
    for request in [b"not-json\n".to_vec(), oversized] {
        let mut socket = UnixStream::connect(&path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        // An over-limit sender may receive a broken pipe when the bounded
        // reader refuses before it has finished sending. Closure is refusal;
        // any reply must be an error, and the valid-but-oversized Kill must
        // never execute. The live status below proves that last condition.
        let write = socket.write_all(&request);
        let mut response = String::new();
        if write.is_ok() {
            let _ = BufReader::new(socket).read_line(&mut response);
        }
        if !response.is_empty() {
            assert!(
                serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"].is_string()
            );
        }
    }
    drop(stalled);
    assert_eq!(
        owner.request(&owner.nonce, "status")["phase"]["state"],
        "running"
    );
    owner.request(&owner.nonce, "kill");
    owner.wait_phase("retired");
}

#[test]
fn execution_gate_requires_its_exact_durable_identity() {
    for phase in [
        serde_json::json!({"state": "prepared"}),
        serde_json::json!({"state": "running", "pid": 0}),
    ] {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let marker = directory.path().join("ran");
        std::fs::write(
            directory.path().join("owner.json"),
            serde_json::to_vec(&owner_record(
                &generation(),
                serde_json::json!(["/bin/sh", "-c", format!("touch '{}'", marker.display())]),
                phase,
            ))
            .unwrap(),
        )
        .unwrap();
        let mut gate = Command::new(env!("CARGO_BIN_EXE_bun"))
            .args(["__process-exec-gate", "--directory"])
            .arg(directory.path())
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        gate.stdin.take().unwrap().write_all(b"activate\n").unwrap();
        let output = gate.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("matching durable owner"));
        assert!(!marker.exists());
    }
}

#[test]
fn failed_terminal_persistence_does_not_publish_retirement() {
    let mut owner = Owner::start("sleep 30");
    owner.wait_phase("running");
    let path = owner.directory.path().join("owner.json");
    std::fs::rename(&path, owner.directory.path().join("last-confirmed.json")).unwrap();
    std::fs::create_dir(&path).unwrap();
    owner.request(&owner.nonce, "kill");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = owner.child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "owner did not report failed persistence"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(path.is_dir());
    let last: serde_json::Value = serde_json::from_slice(
        &std::fs::read(owner.directory.path().join("last-confirmed.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(last["phase"]["state"], "running");
}

/// Zombie children of `parent`, read from `ps` so the check works on Linux and
/// macOS alike.
fn zombie_children(parent: u32) -> usize {
    let output = Command::new("ps")
        .args(["-A", "-o", "ppid=,stat="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse::<u32>().ok()?, fields.next()?))
        })
        .filter(|(ppid, stat)| *ppid == parent && stat.starts_with('Z'))
        .count()
}

#[test]
fn owner_reaps_orphaned_descendants_while_the_workload_runs() {
    let marker = tempfile::tempdir_in("/tmp").unwrap();
    let release = marker.path().join("exit-parent");
    // Each subshell backgrounds a short sleep and exits at once, so the sleep
    // is orphaned to the subreaper owner, the way a double-forking daemon is.
    let mut owner = Owner::start(&format!(
        "while [ ! -f '{}' ]; do (sleep 0.01 &); sleep 0.02; done; exit 5",
        release.display()
    ));
    owner.wait_phase("running");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut worst = 0;
    while Instant::now() < deadline {
        worst = worst.max(zombie_children(owner.child.id()));
        std::thread::sleep(Duration::from_millis(100));
    }
    // A zombie can exist for one owner tick before it is reaped. Dozens of
    // orphans exit in two seconds, so an owner that never reaps fails here.
    assert!(worst <= 3, "{worst} zombies piled up under the owner");
    std::fs::write(release, "exit").unwrap();
    let record = owner.wait_phase("retired");
    assert_eq!(
        record["phase"]["exit_code"], 5,
        "reaping orphans lost the workload's own exit status"
    );
}
