//! Actual-binary contracts for the foreground process owner, before agent wiring.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Owner {
    directory: tempfile::TempDir,
    child: Child,
}

impl Owner {
    fn start(script: &str) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let spec = serde_json::json!({
            "schema": 1,
            "nonce": "test-owner-generation",
            "command": ["/bin/sh", "-c", script],
            "environment": {},
            "phase": { "state": "prepared" },
        });
        std::fs::write(
            directory.path().join("owner.json"),
            serde_json::to_vec(&spec).unwrap(),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_bun"))
            .args(["__process-owner", "--directory"])
            .arg(directory.path())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(directory.path().join("owner.log")).unwrap())
            .spawn()
            .unwrap();
        Self { directory, child }
    }

    fn wait_phase(&mut self, expected: &str) -> serde_json::Value {
        // A fresh large debug binary can spend six seconds in macOS dyld
        // before main. Retirement latency is tested separately after launch.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let record: serde_json::Value = serde_json::from_slice(
                &std::fs::read(self.directory.path().join("owner.json")).unwrap(),
            )
            .unwrap();
            if record["phase"]["state"] == expected {
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

    fn request(&self, nonce: &str, action: &str) -> serde_json::Value {
        let mut socket = UnixStream::connect(self.directory.path().join("control.sock")).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        writeln!(
            socket,
            "{}",
            serde_json::json!({"nonce": nonce, "action": action})
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(socket).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        if let Ok(mut socket) = UnixStream::connect(self.directory.path().join("control.sock")) {
            let _ = writeln!(
                socket,
                "{}",
                serde_json::json!({"nonce": "test-owner-generation", "action": "kill"})
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

#[test]
fn wrong_generation_cannot_signal_a_live_owner() {
    let mut owner = Owner::start("sleep 30");
    owner.wait_phase("running");
    assert_eq!(
        owner.request("another-generation", "kill")["error"],
        "owner generation mismatch"
    );
    assert_eq!(
        owner.request("test-owner-generation", "status")["phase"]["state"],
        "running"
    );
    assert_eq!(
        owner.request("test-owner-generation", "kill")["accepted"],
        true
    );
    let record = owner.wait_phase("retired");
    assert!(record["phase"]["exit_code"].is_null());
}

#[test]
fn execution_gate_refuses_eof_before_activation() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let marker = directory.path().join("ran");
    let spec = serde_json::json!({
        "schema": 1, "nonce": "test-owner-generation",
        "command": ["/bin/sh", "-c", format!("touch '{}'", marker.display())],
        "environment": {}, "phase": {"state": "prepared"},
    });
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
    let output = Command::new(env!("CARGO_BIN_EXE_bun"))
        .args(["__process-owner", "--directory"])
        .arg(owner.directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
    assert_eq!(
        owner.request("test-owner-generation", "status")["phase"],
        running["phase"]
    );
    owner.request("test-owner-generation", "kill");
    owner.wait_phase("retired");
}

#[test]
fn retired_generation_cannot_execute_again() {
    let mut owner = Owner::start("exit 0");
    owner.wait_phase("retired");
    assert!(owner.child.wait().unwrap().success());
    let output = Command::new(env!("CARGO_BIN_EXE_bun"))
        .args(["__process-owner", "--directory"])
        .arg(owner.directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already started"));
    owner.wait_phase("retired");
}

#[test]
fn malformed_and_abandoned_clients_do_not_end_the_owner() {
    let mut owner = Owner::start("sleep 30");
    owner.wait_phase("running");
    let path = owner.directory.path().join("control.sock");
    // This connection never sends a newline. Its timeout must let the next
    // client through without preventing exit observation or future cleanup.
    let stalled = UnixStream::connect(&path).unwrap();
    let mut oversized = serde_json::to_vec(&serde_json::json!({
        "nonce": "test-owner-generation", "action": "kill"
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
        owner.request("test-owner-generation", "status")["phase"]["state"],
        "running"
    );
    owner.request("test-owner-generation", "kill");
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
            serde_json::to_vec(&serde_json::json!({
                "schema": 1, "nonce": "test-owner-generation",
                "command": ["/bin/sh", "-c", format!("touch '{}'", marker.display())],
                "environment": {}, "phase": phase,
            }))
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
    owner.request("test-owner-generation", "kill");
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
