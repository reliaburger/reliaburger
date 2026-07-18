//! Black-box API listener safety tests for the real Bun binary.
//!
//! An empty token store is the local bootstrap window. It must never make the
//! administrative API reachable beyond an IP-literal loopback listener.

use std::io::Read as _;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn write_config(root: &Path) -> std::path::PathBuf {
    let config = root.join("node.toml");
    let [gossip_port, raft_port, reporting_port] = reserve_ports();
    std::fs::write(
        &config,
        format!(
            r#"
[node]
name = "auth-startup-{gossip_port}"

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
"#,
            root = root.display(),
        ),
    )
    .unwrap();
    config
}

fn reserve_ports() -> [u16; 3] {
    let listeners = [
        TcpListener::bind("127.0.0.1:0").unwrap(),
        TcpListener::bind("127.0.0.1:0").unwrap(),
        TcpListener::bind("127.0.0.1:0").unwrap(),
    ];
    listeners.map(|listener| listener.local_addr().unwrap().port())
}

fn bun_command(config: &Path, listen: &str, clustered: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bun"));
    command
        .arg("--config")
        .arg(config)
        .arg("--listen")
        .arg(listen)
        .arg("--runtime")
        .arg("process")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if clustered {
        command.arg("--cluster");
    }
    command
}

fn wait_for_exit(command: &mut Command) -> Output {
    let mut child = command.spawn().unwrap();
    // The full nextest suite runs thousands of tests concurrently enough to
    // starve a freshly spawned binary for several seconds on smaller hosts.
    // Match the repository's ordinary real-process integration deadline.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "bun kept serving an unsafe listener; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_empty_token_listener_refused(listen: &str, clustered: bool) {
    let root = tempfile::tempdir().unwrap();
    let config = write_config(root.path());
    let output = wait_for_exit(&mut bun_command(&config, listen, clustered));
    assert!(
        !output.status.success(),
        "bun unexpectedly exited successfully"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("empty token store") && stderr.contains("IP-literal loopback"),
        "unexpected startup error: {stderr}"
    );
}

#[test]
fn standalone_refuses_wildcard_listener_during_bootstrap() {
    assert_empty_token_listener_refused("0.0.0.0:0", false);
}

#[test]
fn standalone_refuses_routable_listener_during_bootstrap() {
    assert_empty_token_listener_refused("192.0.2.10:9117", false);
}

#[test]
fn standalone_refuses_hostname_listener_during_bootstrap() {
    assert_empty_token_listener_refused("localhost:0", false);
}

#[test]
fn clustered_refuses_wildcard_listener_during_bootstrap() {
    assert_empty_token_listener_refused("0.0.0.0:0", true);
}

#[test]
fn standalone_allows_ip_literal_loopback_during_bootstrap() {
    let root = tempfile::tempdir().unwrap();
    let config = write_config(root.path());
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let address: SocketAddr = reserved.local_addr().unwrap();
    drop(reserved);

    let mut child = bun_command(&config, &address.to_string(), false)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut response = Vec::new();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            panic!(
                "bun exited before serving loopback ({status}); stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            use std::io::Write as _;
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            response.clear();
            let sent = stream.write_all(
                b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            );
            let received = sent.and_then(|()| stream.read_to_end(&mut response).map(|_| ()));
            if received.is_ok() && response.starts_with(b"HTTP/1.1 200") {
                break;
            }
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "bun did not serve the loopback bootstrap listener; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "unexpected health response: {}",
        String::from_utf8_lossy(&response)
    );
}
