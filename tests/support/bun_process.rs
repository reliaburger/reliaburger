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

/// Keep a readable Bun log unchanged, or expose only the failed read's kind.
/// Error text and paths can contain sensitive fixture data and are omitted.
fn bun_log_diagnostic(log_path: &Path) -> String {
    match std::fs::read_to_string(log_path) {
        Ok(log) => log,
        Err(error) => format!("bun log-read-error kind={:?}", error.kind()),
    }
}

thread_local! {
    static TRACE_FIRST_RUN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Enable phase-only diagnostics on this first-run test thread. No argv, token
/// value, subprocess output or security-file contents are logged by the tracer.
pub fn enable_phase_diagnostics() {
    TRACE_FIRST_RUN.with(|enabled| enabled.set(true));
}

pub struct FixturePhase {
    name: &'static str,
    started: Instant,
    enabled: bool,
}

impl Drop for FixturePhase {
    fn drop(&mut self) {
        if self.enabled {
            eprintln!(
                "first-run phase={} end elapsed={:?} panicking={}",
                self.name,
                self.started.elapsed(),
                std::thread::panicking()
            );
        }
    }
}

pub fn fixture_phase(name: &'static str) -> FixturePhase {
    let enabled = TRACE_FIRST_RUN.with(std::cell::Cell::get);
    if enabled {
        eprintln!("first-run phase={name} begin");
    }
    FixturePhase {
        name,
        started: Instant::now(),
        enabled,
    }
}

fn safe_relish_phase(args: &[&str]) -> &'static str {
    // Only fixed semantic labels leave this function. Never interpolate args.
    if args.first() == Some(&"init") {
        return "cluster-init-command";
    }
    if args.first() == Some(&"join") {
        return "join-command";
    }
    if args.windows(2).any(|pair| pair == ["join-token", "create"]) {
        return "join-token-mint-command";
    }
    if args.windows(2).any(|pair| pair == ["token", "create"]) {
        return "bearer-mint-command";
    }
    if args.windows(2).any(|pair| pair == ["token", "list"]) {
        return "token-replication-probe-command";
    }
    if args.contains(&"nodes") {
        return "membership-nodes-command";
    }
    if args.contains(&"council") {
        return "membership-council-command";
    }
    if args.contains(&"status") {
        return "status-command";
    }
    if args.contains(&"apply") {
        return "apply-command";
    }
    "relish-command"
}

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
        let _phase = fixture_phase("bun-child-spawn");
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
            let log = bun_log_diagnostic(&self.log_path);
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

/// A block held by a crash fixture while its Bun is offline.
#[derive(Debug)]
pub struct PortBlockReservation {
    pub base: u16,
    _leases: Vec<reliaburger::file_lock::FileLock>,
}

impl PortBlockReservation {
    /// Claim an exact block if both protocols and the fixture leases are free.
    pub fn try_reserve(base: u16, count: u16) -> Option<Self> {
        let end = base.checked_add(count)?;
        if count == 0 || base < TEST_PORTS.start || end > TEST_PORTS.end {
            return None;
        }
        use reliaburger::file_lock::{FileLock, FileLockError};
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let directory =
            std::env::temp_dir().join(format!("reliaburger-test-ports-{}", nix::unistd::geteuid()));
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("cannot create port lease directory: {error}"),
        }
        let mut leases = Vec::with_capacity(usize::from(count));
        for port in base..end {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(directory.join(port.to_string()))
                .unwrap();
            match FileLock::try_lock(file) {
                Ok(lease) => leases.push(lease),
                Err(FileLockError::Busy) => return None,
                Err(error) => panic!("cannot take port lease: {error}"),
            }
        }
        let held: Option<Vec<_>> = (base..end)
            .map(|port| {
                let tcp = TcpListener::bind(("127.0.0.1", port)).ok()?;
                let udp = std::net::UdpSocket::bind(("127.0.0.1", port)).ok()?;
                Some((tcp, udp))
            })
            .collect();
        let _held = held?;
        Some(Self {
            base,
            _leases: leases,
        })
    }
}

/// Reserve a block until its returned guard is dropped, including downtime.
pub fn reserve_port_block_lease(count: u16) -> PortBlockReservation {
    use rand::Rng;
    assert!(count > 0 && count < TEST_PORTS.end - TEST_PORTS.start);
    let mut random = rand::thread_rng();
    for _ in 0..1_000 {
        let base = random.gen_range(TEST_PORTS.start..TEST_PORTS.end - count);
        if let Some(reservation) = PortBlockReservation::try_reserve(base, count) {
            return reservation;
        }
    }
    panic!("no free block of {count} test ports in {TEST_PORTS:?}");
}

/// Find currently TCP/UDP-free ports, respecting other fixtures' leases.
pub fn reserve_port_block(count: u16) -> u16 {
    reserve_port_block_lease(count).base
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
            let log = bun_log_diagnostic(&bun.log_path);
            if log.contains("Address already in use") {
                return BunStart::PortRace;
            }
            panic!("bun exited before binding its listeners ({status}):\n{log}");
        }
        // A connection to a reserved address may reach the process that stole
        // it. Only this child's own announcement proves its binds succeeded.
        let bound_address = bun_log_diagnostic(&bun.log_path)
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
            let log = bun_log_diagnostic(&bun.log_path);
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
    let _phase = fixture_phase("bun-listener-readiness");
    const ATTEMPTS: usize = 3;
    for attempt in 1..=ATTEMPTS {
        let (config, address, log_path) = build();
        let mut bun = BunProcess::spawn_runtime(&config, address, clustered, log_path, runtime);
        match wait_for_bind(&mut bun, address) {
            BunStart::Ready(address) => return (bun, address),
            BunStart::PortRace => {
                let log = bun_log_diagnostic(&bun.log_path);
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
    let _phase = fixture_phase(safe_relish_phase(args));
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
    let _phase = fixture_phase("relish-endpoint-readiness");
    let deadline = Instant::now() + WAIT;
    loop {
        bun.assert_running();
        let output = run_relish(args);
        if output.status.success() {
            return output;
        }
        if Instant::now() >= deadline {
            let log = bun_log_diagnostic(&bun.log_path);
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
            let log = bun_log_diagnostic(&bun.log_path);
            panic!(
                "relish output never contained {expected:?}\nstdout={}\nstderr={}\nbun log={log}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
