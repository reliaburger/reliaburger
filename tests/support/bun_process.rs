//! Shared fixtures for black-box tests that launch the real `bun` binary.
//!
//! Each test gets a Bun child process with its own config and ports, drives
//! it through the compiled `relish` CLI (or `BunClient`), and tears it down on
//! `Drop`, including while unwinding from a panic. Included with
//! `#[path = "support/bun_process.rs"] mod bun_process;` (or
//! `"../support/bun_process.rs"` from `tests/suite/main.rs`).
//!
//! Not every consumer uses every helper, so the module allows dead code
//! rather than making each test binary import everything.
#![allow(dead_code)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long a helper waits for Bun or Relish before failing the test.
pub const WAIT: Duration = Duration::from_secs(30);

/// A running `bun` child, terminated (SIGTERM, then SIGKILL) on drop.
pub struct BunProcess {
    pub child: Child,
    pub log_path: PathBuf,
}

impl BunProcess {
    /// Start Bun with the process runtime.
    pub fn spawn(config: &Path, address: SocketAddr, clustered: bool, log_path: PathBuf) -> Self {
        Self::spawn_runtime(config, address, clustered, log_path, "process")
    }

    /// Start Bun with the named runtime, logging stdout and stderr to `log_path`.
    pub fn spawn_runtime(
        config: &Path,
        address: SocketAddr,
        clustered: bool,
        log_path: PathBuf,
        runtime: &str,
    ) -> Self {
        let log = std::fs::File::create(&log_path).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bun"));
        command
            .arg("--config")
            .arg(config)
            .arg("--listen")
            .arg(address.to_string())
            .arg("--runtime")
            .arg(runtime)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        if clustered {
            command.arg("--cluster");
        }
        Self {
            child: command.spawn().unwrap(),
            log_path,
        }
    }

    /// Panic with Bun's log if the process has already exited.
    pub fn assert_running(&mut self) {
        if let Some(status) = self.child.try_wait().unwrap() {
            let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
            panic!("bun exited before the first-run command ({status}):\n{log}");
        }
    }
}

impl Drop for BunProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        #[cfg(unix)]
        {
            let pid = nix::unistd::Pid::from_raw(self.child.id() as i32);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where test ports come from: below every ephemeral range the runners use
/// (Linux 32768–60999, macOS 49152–65535) and clear of the fixed ports the
/// in-process cluster suites hard-code (15000–26999, 30000 and up).
///
/// Ports the OS hands out with `bind(0)` are the wrong source. macOS assigns
/// ephemeral ports sequentially to `bind(0)` and `connect()` alike, so the
/// released port, and the next few after it, are exactly what the next
/// outgoing connections anywhere on the host receive. On a busy runner one of
/// them usually has before Bun binds, and Bun exits with "Address already in
/// use" however often the harness retries.
const TEST_PORTS: std::ops::Range<u16> = 27000..30000;

/// `count` consecutive loopback ports, each free for both TCP and UDP right now.
///
/// They come from [`TEST_PORTS`], which no outgoing connection can be given,
/// so only another test's concurrent random pick can take one before its
/// owner binds it.
pub fn reserve_port_block(count: u16) -> u16 {
    use rand::Rng;
    let mut random = rand::thread_rng();
    for _ in 0..1_000 {
        let base = random.gen_range(TEST_PORTS.start..TEST_PORTS.end - count);
        // Holding every socket until the whole block checks out keeps a port
        // from being counted twice.
        let held: Option<Vec<_>> = (base..base + count)
            .map(|port| {
                let tcp = TcpListener::bind(("127.0.0.1", port)).ok()?;
                let udp = std::net::UdpSocket::bind(("127.0.0.1", port)).ok()?;
                Some((tcp, udp))
            })
            .collect();
        if held.is_some() {
            return base;
        }
    }
    panic!("no free block of {count} test ports in {TEST_PORTS:?}");
}

/// A free loopback address for a Bun listener.
pub fn reserve_address() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], reserve_port_block(1)))
}

/// Three free loopback ports for gossip, Raft and reporting.
pub fn reserve_ports() -> [u16; 3] {
    let base = reserve_port_block(3);
    [base, base + 1, base + 2]
}

/// How a freshly spawned bun came up.
pub enum BunStart {
    /// The API answered on its address; every earlier bind succeeded.
    Ready(SocketAddr),
    /// Bun exited with "Address already in use": between reserving a port
    /// and bun binding it, another process on the runner grabbed it.
    PortRace,
}

/// Wait until `bun` accepts TCP on its API address, distinguishing the
/// reserved-port race from a genuine startup failure (which still panics).
pub fn wait_for_bind(bun: &mut BunProcess, address: SocketAddr) -> BunStart {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = bun.child.try_wait().unwrap() {
            let log = std::fs::read_to_string(&bun.log_path).unwrap_or_default();
            if log.contains("Address already in use") {
                return BunStart::PortRace;
            }
            panic!("bun exited before binding its listeners ({status}):\n{log}");
        }
        // A connection to a reserved address may reach the process that stole
        // it. Only this child's own announcement proves its binds succeeded.
        let bound_address = std::fs::read_to_string(&bun.log_path)
            .unwrap_or_default()
            .lines()
            .find_map(|line| line.strip_prefix("bun: API server listening on "))
            .and_then(|bound| bound.parse::<SocketAddr>().ok())
            .filter(|bound| bound.port() != 0 && (address.port() == 0 || *bound == address));
        if let Some(address) = bound_address
            && TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok()
        {
            return BunStart::Ready(address);
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(&bun.log_path).unwrap_or_default();
            panic!("bun never listened on {address}:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Spawn a bun and wait for its API listener, re-reserving every port and
/// respawning when it loses the reserve-then-bind race.
///
/// `reserve_ports` releases its listeners before bun rebinds the ports, so a
/// concurrently running test can steal one in between; with harness retries
/// deliberately at zero, that host-level race must be healed here, scoped to
/// the exact "Address already in use" exit. `build` reserves fresh ports,
/// writes the config and returns `(config, api_address, log_path)`; on the
/// race it runs again, so nothing from the lost attempt is reused.
pub fn spawn_bun_with_port_retry<F>(clustered: bool, build: F) -> (BunProcess, SocketAddr)
where
    F: FnMut() -> (PathBuf, SocketAddr, PathBuf),
{
    spawn_bun_with_runtime_port_retry(clustered, "process", build)
}

/// [`spawn_bun_with_port_retry`] for a named runtime.
pub fn spawn_bun_with_runtime_port_retry<F>(
    clustered: bool,
    runtime: &str,
    mut build: F,
) -> (BunProcess, SocketAddr)
where
    F: FnMut() -> (PathBuf, SocketAddr, PathBuf),
{
    const ATTEMPTS: usize = 3;
    for attempt in 1..=ATTEMPTS {
        let (config, address, log_path) = build();
        let mut bun = BunProcess::spawn_runtime(&config, address, clustered, log_path, runtime);
        match wait_for_bind(&mut bun, address) {
            BunStart::Ready(address) => return (bun, address),
            BunStart::PortRace => {
                let log = std::fs::read_to_string(&bun.log_path).unwrap_or_default();
                assert!(
                    attempt < ATTEMPTS,
                    "bun lost the reserved-port race {ATTEMPTS} times in a row:\n{log}"
                );
                eprintln!(
                    "bun lost the reserved-port race (attempt {attempt}); \
                     retrying with freshly reserved ports"
                );
            }
        }
    }
    unreachable!("the retry loop returns on success and panics on exhaustion");
}

/// Write a single-node config under `root` with freshly reserved ports.
pub fn write_portable_node_config(root: &Path) -> PathBuf {
    write_portable_node_config_with_ports(root, reserve_ports())
}

/// Write a single-node config under `root` using the given
/// gossip, Raft and reporting ports.
pub fn write_portable_node_config_with_ports(root: &Path, ports: [u16; 3]) -> PathBuf {
    let config = root.join("node.toml");
    let [gossip_port, raft_port, reporting_port] = ports;
    std::fs::write(
        &config,
        format!(
            r#"
[node]
name = "first-run-{gossip_port}"

[cluster]
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

/// Run the compiled `relish` with no endpoint, token or CA inherited from
/// the environment.
pub fn run_relish(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(args)
        .env_remove("RELIABURGER_ENDPOINT")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap()
}

/// Panic with Relish's stdout and stderr unless it succeeded.
pub fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Retry a Relish command until it succeeds, failing if Bun exits first.
pub fn wait_for_relish(bun: &mut BunProcess, args: &[&str]) -> Output {
    let deadline = Instant::now() + WAIT;
    loop {
        bun.assert_running();
        let output = run_relish(args);
        if output.status.success() {
            return output;
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(&bun.log_path).unwrap_or_default();
            panic!(
                "relish never reached bun\nstdout={}\nstderr={}\nbun log={log}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Retry a Relish command until it succeeds and its stdout contains
/// `expected`, failing if Bun exits first.
pub fn wait_for_relish_output(bun: &mut BunProcess, args: &[&str], expected: &str) -> Output {
    let deadline = Instant::now() + WAIT;
    loop {
        bun.assert_running();
        let output = run_relish(args);
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(expected) {
            return output;
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(&bun.log_path).unwrap_or_default();
            panic!(
                "relish output never contained {expected:?}\nstdout={}\nstderr={}\nbun log={log}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
