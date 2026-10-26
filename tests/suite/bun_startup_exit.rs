//! A start-up error in the real Bun binary exits promptly (#816).
//!
//! Bun hashes its own binary in the background at boot (#765). A start-up
//! error raised after that point must not wait for the hash: a debug Bun is
//! hundreds of megabytes, and the error exit waited up to 17 s for it.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::bun_process::reserve_ports;

fn write_config(root: &Path, binary_dir: &Path) -> std::path::PathBuf {
    let config = root.join("node.toml");
    let [gossip_port, raft_port, reporting_port] = reserve_ports();
    std::fs::write(
        &config,
        format!(
            r#"
[node]
name = "startup-exit-{gossip_port}"

[cluster]
join = []
gossip_port = {gossip_port}
raft_port = {raft_port}
reporting_port = {reporting_port}

[network]
advertise_address = "127.0.0.1"

[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"

[images]
registry_bind = "127.0.0.1"
registry_port = 0

[upgrades]
binary_dir = "{binary_dir}"
"#,
            root = root.display(),
            binary_dir = binary_dir.display(),
        ),
    )
    .unwrap();
    config
}

#[test]
fn a_late_start_up_error_exits_without_waiting_for_the_binary_hash() {
    let root = tempfile::tempdir().unwrap();

    // The boot-time hash reads the store's file for the running version. A
    // FIFO nobody writes to makes that hash never finish, so the old exit,
    // which waited for it, hangs until the deadline below.
    let binary_dir = root.path().join("bin");
    std::fs::create_dir_all(&binary_dir).unwrap();
    let running = binary_dir.join(format!("bun-v{}", env!("CARGO_PKG_VERSION")));
    nix::unistd::mkfifo(&running, nix::sys::stat::Mode::S_IRWXU).unwrap();

    // Durable discovery state on a process-runtime node is refused after
    // runtime selection, well after the hash has started. Stamp the data
    // directory first, as a previous boot would have, or Bun refuses it as
    // unversioned before it ever reaches the hash.
    let data = root.path().join("data");
    reliaburger::compatibility::ensure_state_compatible(&data).unwrap();
    std::fs::create_dir_all(data.join("discovery")).unwrap();

    let config = write_config(root.path(), &binary_dir);
    // Pay `bun`'s first launch before the deadline below starts (#1149).
    crate::bun_process::first_launch_binaries();
    let mut child = Command::new(env!("CARGO_BIN_EXE_bun"))
        .arg("--config")
        .arg(&config)
        .arg("--listen")
        .arg("127.0.0.1:0")
        .arg("--runtime")
        .arg("process")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // The hash never finishes, so any bound works; this one matches the
    // suite's real-process deadline for a starved host.
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "bun waited for the binary hash after a start-up error; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success(), "bun unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing a mode change"),
        "unexpected start-up error: {stderr}"
    );
}
