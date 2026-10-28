//! A real council in front of a fleet of hollow nodes (#1401).
//!
//! Our biggest real cluster is three VMs. This binary puts a three-member
//! council (gossip, Raft, reporting, the HTTP API and the leader scheduler,
//! wired as `bun --cluster` wires them) in one process, and starts tens to
//! hundreds of hollow nodes in another: real agents whose runtime runs
//! nothing (`support/hollow.rs`). Keeping the council in a process of its
//! own is what lets the harness read the council's CPU and memory apart
//! from the fleet's.
//!
//! [`run_fleet`] measures, in order:
//!
//! - gossip convergence: every hollow node sees every other member, and
//!   the council lists them all as alive;
//! - scheduling: one apply of an app with a replica per hollow node, timed
//!   until every replica runs;
//! - API writes: twenty small applies one after another;
//! - Raft commits: the council process writes a no-op config key through
//!   Raft every second and reports how long each commit took;
//! - churn: a share of the hollow nodes stops, the council notices, their
//!   replicas run elsewhere, and the nodes come back.
//!
//! Two tests run it. `make test-hollow` runs twenty hollow nodes and checks
//! the harness still works. `scripts/release/qualify-hollow-nodes.sh` runs
//! the measured sizes one at a time and keeps each JSON report.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc as std_mpsc};
use std::time::{Duration, Instant};

use reliaburger::config::Config;
use reliaburger::council::types::RaftRequest;
use reliaburger::relish::client::BunClient;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

#[path = "support/bun_process.rs"]
mod bun_process;
#[path = "support/cluster.rs"]
mod cluster_support;
#[path = "support/hollow.rs"]
mod hollow;
use cluster_support::task_harness;

use cluster_support::{MembershipSource, WiredNodeOptions, local, start_wired_node};
use hollow::{FleetCounters, HollowNode, HollowSpec, PORTS_PER_NODE, start_hollow_node};

/// The council process reads its plan from these.
const COUNCIL_BASE_ENV: &str = "RELIABURGER_HOLLOW_COUNCIL_BASE";
const COUNCIL_SIZE_ENV: &str = "RELIABURGER_HOLLOW_COUNCIL_SIZE";
const COUNCIL_DIR_ENV: &str = "RELIABURGER_HOLLOW_COUNCIL_DIR";
/// The council process's first stdout line once it has a full council.
const READY: &str = "hollow-council-ready";
/// Each Raft probe: `hollow-probe <microseconds>` or `hollow-probe-error`.
const PROBE: &str = "hollow-probe";
/// Hollow nodes take their ports from here, outside the shared test range.
const HOLLOW_PORTS: (u16, u16) = (31_000, 47_000);
/// Above this one-minute load average the harness stops rather than measure
/// a host that's busy with something else. `RELIABURGER_HOLLOW_MAX_LOAD`
/// overrides it.
const MAX_LOAD: f64 = 40.0;

fn max_load() -> f64 {
    std::env::var("RELIABURGER_HOLLOW_MAX_LOAD")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(MAX_LOAD)
}

/// One fleet run.
#[derive(Debug, Clone, Serialize)]
pub struct FleetPlan {
    pub hollow_nodes: usize,
    pub council_members: usize,
    /// How many hollow nodes start at once.
    pub start_parallelism: usize,
    /// How often each hollow node reports (Bun's default is 5 s).
    pub report_interval_secs: u64,
    /// How long to watch the fleet with everything running.
    pub steady_secs: u64,
    /// Share of the hollow nodes to stop and restart, 0 for none.
    pub churn_fraction: f64,
}

/// Everything one run measured. Durations are in milliseconds.
#[derive(Debug, Default, Serialize)]
pub struct FleetReport {
    pub plan: Option<FleetPlan>,
    pub host: String,
    pub started_at_unix: u64,
    pub start_all_ms: u64,
    /// Last hollow node started → every hollow node sees every member.
    pub gossip_converged_ms: Option<u64>,
    /// Last hollow node started → the council lists every node alive.
    pub council_sees_all_ms: Option<u64>,
    /// Hollow nodes that saw every member alive when the harness stopped
    /// waiting for gossip.
    pub nodes_seeing_everyone: usize,
    pub council_voters: usize,
    pub hollow_voters: usize,
    pub replicas: usize,
    pub apply_ms: Option<u64>,
    /// Apply returned → first replica running.
    pub first_replica_ms: Option<u64>,
    /// Apply returned → every replica running.
    pub all_replicas_ms: Option<u64>,
    pub small_applies: Summary,
    pub raft_commit: Vec<PhaseLatency>,
    pub resources: Vec<PhaseResources>,
    pub churn: Option<ChurnReport>,
    /// Why the run stopped early, if it did.
    pub stopped: Option<String>,
}

/// Failure detection and rescheduling when part of the fleet stops.
#[derive(Debug, Default, Serialize)]
pub struct ChurnReport {
    pub stopped_nodes: usize,
    pub replicas_lost: usize,
    /// Nodes stopped → the council lists none of them alive.
    pub detected_ms: Option<u64>,
    /// Nodes stopped → every replica running again.
    pub rescheduled_ms: Option<u64>,
    /// Nodes restarted → the council lists them alive again.
    pub rejoined_ms: Option<u64>,
}

/// Percentiles of a set of samples, in milliseconds.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub count: usize,
    pub errors: usize,
    pub p50: Option<f64>,
    pub p90: Option<f64>,
    pub p99: Option<f64>,
    pub max: Option<f64>,
}

impl Summary {
    fn of(mut samples: Vec<f64>, errors: usize) -> Self {
        samples.sort_by(f64::total_cmp);
        let at = |q: f64| -> Option<f64> {
            if samples.is_empty() {
                return None;
            }
            let index = ((samples.len() as f64 * q).ceil() as usize).clamp(1, samples.len()) - 1;
            Some(samples[index])
        };
        Self {
            count: samples.len(),
            errors,
            p50: at(0.50),
            p90: at(0.90),
            p99: at(0.99),
            max: samples.last().copied(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PhaseLatency {
    pub phase: &'static str,
    pub latency: Summary,
}

/// CPU and memory of both processes over one phase.
#[derive(Debug, Serialize)]
pub struct PhaseResources {
    pub phase: &'static str,
    pub seconds: f64,
    /// CPU time over wall time, as a percentage of one core.
    pub council_cpu_percent: f64,
    pub council_rss_mb_max: f64,
    pub fleet_cpu_percent: f64,
    pub fleet_rss_mb_max: f64,
    pub load_average_max: f64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Twenty hollow nodes join a three-member council, take a replica each,
/// and come back after a quarter of them stops. A check that the harness
/// works, not a measurement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a council process and twenty hollow nodes; run with make test-hollow"]
async fn twenty_hollow_nodes_join_a_three_member_council_and_run_a_replica_each() {
    let report = run_fleet(FleetPlan {
        hollow_nodes: 20,
        council_members: 3,
        start_parallelism: 10,
        report_interval_secs: 1,
        steady_secs: 3,
        churn_fraction: 0.25,
    })
    .await;
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert_eq!(report.stopped, None, "the run stopped early");
    assert!(
        report.gossip_converged_ms.is_some(),
        "gossip never converged"
    );
    assert!(
        report.council_sees_all_ms.is_some(),
        "council never saw all"
    );
    assert_eq!(report.council_voters, 3, "the council isn't three voters");
    assert_eq!(report.hollow_voters, 0, "a hollow node became a voter");
    assert!(report.all_replicas_ms.is_some(), "replicas never all ran");
    assert_eq!(report.small_applies.errors, 0, "an apply failed");
    assert!(
        report
            .raft_commit
            .iter()
            .any(|phase| phase.latency.count > 0),
        "the council reported no Raft commits"
    );
    let churn = report.churn.expect("churn ran");
    assert_eq!(churn.stopped_nodes, 5);
    assert!(
        churn.detected_ms.is_some(),
        "council never noticed the stop"
    );
    assert!(
        churn.rescheduled_ms.is_some(),
        "lost replicas never came back"
    );
    assert!(churn.rejoined_ms.is_some(), "stopped nodes never rejoined");
}

/// The measured run: `RELIABURGER_HOLLOW_NODES` hollow nodes (default 100),
/// with the JSON report written to `RELIABURGER_HOLLOW_REPORT`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement, not a check; run through scripts/release/qualify-hollow-nodes.sh"]
async fn hollow_fleet_measurement() {
    let read =
        |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
    let plan = FleetPlan {
        hollow_nodes: read("RELIABURGER_HOLLOW_NODES", "100").parse().unwrap(),
        council_members: read("RELIABURGER_HOLLOW_COUNCIL", "3").parse().unwrap(),
        start_parallelism: read("RELIABURGER_HOLLOW_PARALLELISM", "25")
            .parse()
            .unwrap(),
        report_interval_secs: read("RELIABURGER_HOLLOW_REPORT_INTERVAL", "5")
            .parse()
            .unwrap(),
        steady_secs: read("RELIABURGER_HOLLOW_STEADY_SECS", "60")
            .parse()
            .unwrap(),
        churn_fraction: read("RELIABURGER_HOLLOW_CHURN", "0.1").parse().unwrap(),
    };
    let report = run_fleet(plan).await;
    let json = serde_json::to_string_pretty(&report).unwrap();
    println!("{json}");
    if let Ok(path) = std::env::var("RELIABURGER_HOLLOW_REPORT") {
        std::fs::write(&path, &json).unwrap();
    }
}

/// The council half of a fleet run, in a process of its own. `run_fleet`
/// starts `hollow_council_member` with the plan in its environment, and
/// stops it by closing its stdin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "subprocess fixture for run_fleet's council"]
async fn hollow_council_member() {
    let base: u16 = std::env::var(COUNCIL_BASE_ENV)
        .expect("run_fleet sets the council's base port")
        .parse()
        .unwrap();
    let size: usize = std::env::var(COUNCIL_SIZE_ENV).unwrap().parse().unwrap();
    let root = PathBuf::from(std::env::var(COUNCIL_DIR_ENV).unwrap());
    let shutdown = CancellationToken::new();

    let mut members = Vec::new();
    for index in 0..size {
        let gossip_port = base + PORTS_PER_NODE * index as u16;
        members.push(
            start_wired_node(WiredNodeOptions {
                name: format!("council-{index}"),
                gossip_port,
                seeds: if index == 0 {
                    vec![]
                } else {
                    vec![local(base)]
                },
                shutdown: shutdown.clone(),
                data_dir_prefix: "rb-hollow-council",
                data_root: Some(root.clone()),
                stale_report_timeout_secs: 30,
                stale_node_grace: None,
                metrics_rollup: None,
                scheduler: Some(reliaburger::config::node::ReconstructionSection {
                    report_threshold_percent: 95,
                    learning_period_timeout_secs: 2,
                    large_cluster_timeout_secs: 4,
                    large_cluster_node_count: 5000,
                }),
                lease_reaper: true,
                membership: MembershipSource::Gossip,
                service_identity: None,
                operator_token: None,
                fault_injection: false,
                labels: [("role".to_string(), "council".to_string())].into(),
                keep_data_dir: false,
                task_arrays: None,
                task_arrays_on_agent_budget: None,
                process_only: false,
                council_size_cap: Some(size),
            })
            .await,
        );
    }

    let seated =
        cluster_support::wait_until(Duration::from_secs(60), Duration::from_millis(100), || {
            members.iter().any(|member| {
                let metrics = member.metrics_rx.borrow();
                metrics.current_leader == Some(metrics.id)
                    && metrics.membership_config.membership().voter_ids().count() == size
            })
        })
        .await;
    assert!(seated, "the council never seated {size} voters");
    println!("{READY}");
    std::io::stdout().flush().unwrap();

    // Closing stdin is the parent's only way to say stop, and it also
    // happens when the parent dies, so no council outlives its harness.
    let stdin_closed = CancellationToken::new();
    let closed = stdin_closed.clone();
    std::thread::spawn(move || {
        let mut sink = String::new();
        while std::io::stdin()
            .read_line(&mut sink)
            .is_ok_and(|read| read > 0)
        {}
        closed.cancel();
    });

    let mut probe = 0u64;
    loop {
        tokio::select! {
            _ = stdin_closed.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        let Some(leader) = members.iter().find(|member| *member.thinks_leader.borrow()) else {
            println!("{PROBE}-error no leader");
            continue;
        };
        probe += 1;
        let started = Instant::now();
        let write = leader.council.write(RaftRequest::ConfigSet {
            key: "hollow.probe".to_string(),
            value: probe.to_string(),
        });
        match tokio::time::timeout(Duration::from_secs(10), write).await {
            Ok(Ok(_)) => println!("{PROBE} {}", started.elapsed().as_micros()),
            Ok(Err(error)) => println!("{PROBE}-error {error}"),
            Err(_) => println!("{PROBE}-error timed out"),
        }
    }
    shutdown.cancel();
    drop(members);
}

// ---------------------------------------------------------------------------
// The council process, seen from the harness
// ---------------------------------------------------------------------------

/// What the harness reads from the council process's stdout.
enum CouncilLine {
    Probe(Instant, Option<f64>),
}

struct CouncilProcess {
    child: Child,
    /// Dropping this closes the council's stdin, which stops it.
    stdin: Option<ChildStdin>,
    lines: std_mpsc::Receiver<CouncilLine>,
    api: Vec<BunClient>,
    _data: tempfile::TempDir,
    _ports: bun_process::PortBlockReservation,
}

/// Where the council process's own log goes: the file
/// `RELIABURGER_HOLLOW_COUNCIL_LOG` names, or nowhere.
fn council_log() -> Stdio {
    std::env::var_os("RELIABURGER_HOLLOW_COUNCIL_LOG")
        .and_then(|path| std::fs::File::create(path).ok())
        .map_or_else(Stdio::null, Stdio::from)
}

impl CouncilProcess {
    fn start(size: usize) -> Self {
        let ports = bun_process::reserve_port_block_lease(PORTS_PER_NODE * size as u16);
        let data = tempfile::tempdir().unwrap();
        let base = ports.base;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "hollow_council_member",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(COUNCIL_BASE_ENV, base.to_string())
            .env(COUNCIL_SIZE_ENV, size.to_string())
            .env(COUNCIL_DIR_ENV, data.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(council_log())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let (lines_tx, lines) = std_mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let now = Instant::now();
                // libtest prints "test hollow_council_member ... " without a
                // newline before the test's own first line.
                if line.ends_with(READY) {
                    let _ = ready_tx.send(());
                } else if let Some(rest) = line.find(PROBE).map(|at| &line[at + PROBE.len()..]) {
                    let micros = rest.strip_prefix(' ').and_then(|v| v.parse::<f64>().ok());
                    let _ = lines_tx.send(CouncilLine::Probe(now, micros.map(|us| us / 1000.0)));
                }
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(120))
            .expect("the council process never seated its council");
        let api = (0..size)
            .map(|index| {
                BunClient::new(&format!(
                    "http://127.0.0.1:{}",
                    base + PORTS_PER_NODE * index as u16 + 3
                ))
            })
            .collect();
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            api,
            _data: data,
            _ports: ports,
        }
    }

    fn seeds(&self) -> Vec<std::net::SocketAddr> {
        vec![local(self._ports.base)]
    }

    /// The council's view of the cluster, from whichever member answers.
    async fn nodes(&self) -> Option<Vec<reliaburger::bun::agent::NodeStatus>> {
        for client in &self.api {
            if let Ok(Ok(nodes)) =
                tokio::time::timeout(Duration::from_secs(10), client.nodes()).await
            {
                return Some(nodes);
            }
        }
        None
    }

    fn stop(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Resource sampling
// ---------------------------------------------------------------------------

/// One `ps` reading of a process: resident memory and total CPU time.
#[derive(Debug, Clone, Copy)]
struct ProcessSample {
    rss_mb: f64,
    cpu_secs: f64,
}

/// `ps -o rss=,time=` for `pid`.
fn sample_process(pid: u32) -> Option<ProcessSample> {
    let output = Command::new("ps")
        .args(["-o", "rss=,time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let rss_kb: f64 = fields.next()?.parse().ok()?;
    Some(ProcessSample {
        rss_mb: rss_kb / 1024.0,
        cpu_secs: parse_cpu_time(fields.next()?)?,
    })
}

/// `ps` CPU time: `[[dd-]hh:]mm:ss[.ss]`.
fn parse_cpu_time(text: &str) -> Option<f64> {
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<f64>().ok()?, clock),
        None => (0.0, text),
    };
    let mut seconds = 0.0;
    for part in clock.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + seconds)
}

/// The host's one-minute load average.
fn load_average() -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
        return text.split_whitespace().next()?.parse().ok();
    }
    let output = Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()?;
    // "{ 2.68 2.89 2.74 }"
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .find_map(|field| field.parse().ok())
}

#[derive(Debug, Clone, Copy)]
struct Reading {
    at: Instant,
    phase: usize,
    council: Option<ProcessSample>,
    fleet: Option<ProcessSample>,
    load: Option<f64>,
}

const PHASES: [&str; 6] = ["council-alone", "join", "deploy", "steady", "churn", "done"];

/// Samples both processes every second on a thread of its own, because
/// `ps` blocks.
struct Sampler {
    phase: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    overloaded: Arc<AtomicBool>,
    readings: std::thread::JoinHandle<Vec<Reading>>,
}

impl Sampler {
    fn start(council_pid: u32) -> Self {
        let phase = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let overloaded = Arc::new(AtomicBool::new(false));
        let (thread_phase, thread_stop, thread_overloaded) = (
            Arc::clone(&phase),
            Arc::clone(&stop),
            Arc::clone(&overloaded),
        );
        let limit = max_load();
        let readings = std::thread::spawn(move || {
            let mut readings = Vec::new();
            while !thread_stop.load(Ordering::Relaxed) {
                let load = load_average();
                if load.is_some_and(|load| load > limit) {
                    thread_overloaded.store(true, Ordering::Relaxed);
                }
                readings.push(Reading {
                    at: Instant::now(),
                    phase: thread_phase.load(Ordering::Relaxed),
                    council: sample_process(council_pid),
                    fleet: sample_process(std::process::id()),
                    load,
                });
                std::thread::sleep(Duration::from_secs(1));
            }
            readings
        });
        Self {
            phase,
            stop,
            overloaded,
            readings,
        }
    }

    fn enter(&self, phase: &str) {
        let index = PHASES.iter().position(|name| *name == phase).unwrap();
        self.phase.store(index, Ordering::Relaxed);
    }

    fn overloaded(&self) -> bool {
        self.overloaded.load(Ordering::Relaxed)
    }

    fn finish(self) -> Vec<Reading> {
        self.stop.store(true, Ordering::Relaxed);
        self.readings.join().unwrap()
    }
}

fn summarise_resources(readings: &[Reading]) -> Vec<PhaseResources> {
    let mut phases = Vec::new();
    for (index, name) in PHASES.iter().enumerate() {
        let in_phase: Vec<&Reading> = readings.iter().filter(|r| r.phase == index).collect();
        let (Some(first), Some(last)) = (in_phase.first(), in_phase.last()) else {
            continue;
        };
        let seconds = last.at.duration_since(first.at).as_secs_f64();
        let cpu = |pick: fn(&Reading) -> Option<ProcessSample>| -> f64 {
            match (pick(first), pick(last)) {
                (Some(a), Some(b)) if seconds > 0.0 => 100.0 * (b.cpu_secs - a.cpu_secs) / seconds,
                _ => 0.0,
            }
        };
        let rss = |pick: fn(&Reading) -> Option<ProcessSample>| -> f64 {
            in_phase
                .iter()
                .filter_map(|r| pick(r))
                .map(|s| s.rss_mb)
                .fold(0.0, f64::max)
        };
        phases.push(PhaseResources {
            phase: name,
            seconds,
            council_cpu_percent: cpu(|r| r.council),
            council_rss_mb_max: rss(|r| r.council),
            fleet_cpu_percent: cpu(|r| r.fleet),
            fleet_rss_mb_max: rss(|r| r.fleet),
            load_average_max: in_phase.iter().filter_map(|r| r.load).fold(0.0, f64::max),
        });
    }
    phases
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

fn host_name() -> String {
    let model = Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    format!(
        "{model}, {} logical CPUs",
        std::thread::available_parallelism().map_or(0, |n| n.get())
    )
}

/// Poll `check` every `every` until it holds or `timeout` passes, and say
/// how long it took.
async fn time_until<F, Fut>(timeout: Duration, every: Duration, mut check: F) -> Option<Duration>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = Instant::now();
    loop {
        if check().await {
            return Some(started.elapsed());
        }
        if started.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(every).await;
    }
}

struct Fleet {
    nodes: Vec<Option<HollowNode>>,
    blocks: Vec<u16>,
    counters: Arc<FleetCounters>,
    data: tempfile::TempDir,
    seeds: Vec<std::net::SocketAddr>,
    report_interval_secs: u64,
}

fn hollow_name(index: usize) -> String {
    format!("hollow-{index:04}")
}

impl Fleet {
    fn spec(&self, index: usize) -> HollowSpec {
        HollowSpec {
            name: hollow_name(index),
            gossip_port: self.blocks[index],
            seeds: self.seeds.clone(),
            data_root: self.data.path().to_path_buf(),
            cpu_millicores: 8_000,
            memory_mb: 16_384,
            labels: [("role".to_string(), "hollow".to_string())].into(),
            counters: Arc::clone(&self.counters),
            report_interval_secs: self.report_interval_secs,
        }
    }

    /// Start the nodes at `indices`, `parallelism` at a time.
    async fn start(&mut self, indices: &[usize], parallelism: usize) -> Result<(), String> {
        for batch in indices.chunks(parallelism.max(1)) {
            let started = futures_util::future::join_all(
                batch
                    .iter()
                    .map(|index| start_hollow_node(self.spec(*index))),
            )
            .await;
            for (index, node) in batch.iter().zip(started) {
                let node =
                    node.map_err(|error| format!("{} didn't start: {error}", hollow_name(*index)))?;
                self.nodes[*index] = Some(node);
            }
        }
        Ok(())
    }

    /// Stop the node at `index` as if its host died, and say how many
    /// replicas it took with it. Its traffic stops first, so the council
    /// learns of it the way it learns of a crash: gossip suspects the node
    /// and then declares it dead, rather than hearing it leave.
    async fn stop(&mut self, index: usize) -> usize {
        let Some(node) = self.nodes[index].take() else {
            return 0;
        };
        node.gate.quiesce();
        let lost = node.grill.running().await;
        node.shutdown.cancel();
        drop(node);
        lost
    }

    fn converged(&self, members: usize) -> bool {
        self.nodes
            .iter()
            .flatten()
            .all(|node| node.alive_peers() + 1 >= members)
    }

    /// How many running nodes see every member alive.
    fn seeing_everyone(&self, members: usize) -> usize {
        self.nodes
            .iter()
            .flatten()
            .filter(|node| node.alive_peers() + 1 >= members)
            .count()
    }

    /// Replicas running on the nodes that are up. A stopped node's
    /// runtime is gone with it, whatever it last ran.
    async fn running(&self) -> usize {
        let mut running = 0;
        for node in self.nodes.iter().flatten() {
            running += node.grill.running().await;
        }
        running
    }
}

fn alive_names(
    nodes: &[reliaburger::bun::agent::NodeStatus],
) -> std::collections::BTreeSet<String> {
    nodes
        .iter()
        .filter(|node| node.state == "alive")
        .map(|node| node.node_id.clone())
        .collect()
}

/// Run one fleet and measure it. Never panics on a slow cluster: whatever
/// didn't happen in time is `None` in the report, and the caller decides.
async fn run_fleet(plan: FleetPlan) -> FleetReport {
    let mut report = FleetReport {
        plan: Some(plan.clone()),
        host: host_name(),
        started_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        ..FleetReport::default()
    };
    // Generous bounds that grow with the fleet: these say "never", not
    // "slow".
    let wait = Duration::from_secs(60) + Duration::from_millis(300 * plan.hollow_nodes as u64);

    let blocks = hollow::free_blocks(plan.hollow_nodes, HOLLOW_PORTS.0, HOLLOW_PORTS.1);
    if blocks.len() < plan.hollow_nodes {
        report.stopped = Some(format!(
            "only {} free port blocks for {} hollow nodes",
            blocks.len(),
            plan.hollow_nodes
        ));
        return report;
    }

    let council = CouncilProcess::start(plan.council_members);
    let sampler = Sampler::start(council.child.id());
    let mut fleet = Fleet {
        nodes: (0..plan.hollow_nodes).map(|_| None).collect(),
        blocks,
        counters: Arc::new(FleetCounters::default()),
        data: tempfile::tempdir().unwrap(),
        seeds: council.seeds(),
        report_interval_secs: plan.report_interval_secs,
    };

    let outcome = measure(&plan, &council, &sampler, &mut fleet, &mut report, wait).await;
    if let Err(reason) = outcome {
        report.stopped = Some(reason);
    }

    sampler.enter("done");
    let readings = sampler.finish();
    report.resources = summarise_resources(&readings);
    let probes: Vec<CouncilLine> = council.lines.try_iter().collect();
    report.raft_commit = summarise_probes(&probes, &readings);
    for index in 0..fleet.nodes.len() {
        fleet.stop(index).await;
    }
    council.stop();
    report
}

fn summarise_probes(probes: &[CouncilLine], readings: &[Reading]) -> Vec<PhaseLatency> {
    // A probe belongs to the phase of the last reading before it.
    let phase_at = |at: Instant| -> usize {
        readings
            .iter()
            .take_while(|reading| reading.at <= at)
            .last()
            .map_or(0, |reading| reading.phase)
    };
    PHASES
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let mut samples = Vec::new();
            let mut errors = 0;
            for CouncilLine::Probe(at, latency) in probes {
                if phase_at(*at) != index {
                    continue;
                }
                match latency {
                    Some(ms) => samples.push(*ms),
                    None => errors += 1,
                }
            }
            (samples.len() + errors > 0).then(|| PhaseLatency {
                phase: name,
                latency: Summary::of(samples, errors),
            })
        })
        .collect()
}

fn check_load(sampler: &Sampler) -> Result<(), String> {
    if sampler.overloaded() {
        return Err(format!("host load average went over {}", max_load()));
    }
    Ok(())
}

async fn measure(
    plan: &FleetPlan,
    council: &CouncilProcess,
    sampler: &Sampler,
    fleet: &mut Fleet,
    report: &mut FleetReport,
    wait: Duration,
) -> Result<(), String> {
    let total_members = plan.hollow_nodes + plan.council_members;
    // A baseline of the council with nobody to serve.
    tokio::time::sleep(Duration::from_secs(10)).await;
    check_load(sampler)?;

    sampler.enter("join");
    let started = Instant::now();
    let all: Vec<usize> = (0..plan.hollow_nodes).collect();
    fleet.start(&all, plan.start_parallelism).await?;
    report.start_all_ms = millis(started.elapsed());
    check_load(sampler)?;

    // Both views at once: the council can list everyone well before the
    // last hollow node has heard of the last one, or the other way round.
    let joined = Instant::now();
    while joined.elapsed() < wait
        && (report.gossip_converged_ms.is_none() || report.council_sees_all_ms.is_none())
    {
        if report.gossip_converged_ms.is_none() && fleet.converged(total_members) {
            report.gossip_converged_ms = Some(millis(joined.elapsed()));
        }
        if report.council_sees_all_ms.is_none()
            && council
                .nodes()
                .await
                .is_some_and(|nodes| alive_names(&nodes).len() >= total_members)
        {
            report.council_sees_all_ms = Some(millis(joined.elapsed()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    report.nodes_seeing_everyone = fleet.seeing_everyone(total_members);
    if let Some(nodes) = council.nodes().await {
        report.council_voters = nodes.iter().filter(|node| node.is_council).count();
        report.hollow_voters = nodes
            .iter()
            .filter(|node| node.is_council && node.node_id.starts_with("hollow-"))
            .count();
    }
    if report.council_sees_all_ms.is_none() {
        return Err("the council never listed every hollow node alive".to_string());
    }
    check_load(sampler)?;

    sampler.enter("deploy");
    report.replicas = plan.hollow_nodes;
    let web = Config::parse(&format!(
        r#"
        [app.hollow-web]
        image = "hollow/web:1"
        replicas = {}

        [app.hollow-web.placement]
        required = ["role=hollow"]
        "#,
        report.replicas
    ))
    .unwrap();
    let applied = Instant::now();
    tokio::time::timeout(Duration::from_secs(60), council.api[0].apply(&web))
        .await
        .map_err(|_| "the big apply timed out".to_string())?
        .map_err(|error| format!("the big apply failed: {error}"))?;
    report.apply_ms = Some(millis(applied.elapsed()));
    let applied = Instant::now();
    let view = &*fleet;
    report.first_replica_ms = time_until(wait, Duration::from_millis(50), move || async move {
        view.running().await > 0
    })
    .await
    .map(millis);
    let wanted = report.replicas;
    report.all_replicas_ms = time_until(wait, Duration::from_millis(100), move || async move {
        view.running().await >= wanted
    })
    .await
    .map(|_| millis(applied.elapsed()));
    if report.all_replicas_ms.is_none() {
        return Err(format!(
            "only {} of {wanted} replicas ran",
            fleet.running().await
        ));
    }

    // Twenty small applies, one after another, for the write path's
    // latency with the fleet attached.
    let mut samples = Vec::new();
    let mut errors = 0;
    for app in 0..20 {
        let small = Config::parse(&format!(
            r#"
            [app.hollow-small-{app}]
            image = "hollow/small:1"
            replicas = 2

            [app.hollow-small-{app}.placement]
            required = ["role=hollow"]
            "#
        ))
        .unwrap();
        let client = &council.api[app % council.api.len()];
        let started = Instant::now();
        match tokio::time::timeout(Duration::from_secs(30), client.apply(&small)).await {
            Ok(Ok(_)) => samples.push(started.elapsed().as_secs_f64() * 1000.0),
            _ => errors += 1,
        }
    }
    report.small_applies = Summary::of(samples, errors);
    let wanted = report.replicas + 2 * (20 - errors);
    let view = &*fleet;
    if time_until(wait, Duration::from_millis(100), move || async move {
        view.running().await >= wanted
    })
    .await
    .is_none()
    {
        return Err(format!(
            "only {} of {wanted} replicas ran",
            fleet.running().await
        ));
    }
    check_load(sampler)?;

    sampler.enter("steady");
    tokio::time::sleep(Duration::from_secs(plan.steady_secs)).await;
    check_load(sampler)?;

    if plan.churn_fraction > 0.0 {
        sampler.enter("churn");
        report.churn = Some(churn(plan, council, fleet, wanted, wait).await);
    }
    Ok(())
}

async fn churn(
    plan: &FleetPlan,
    council: &CouncilProcess,
    fleet: &mut Fleet,
    wanted: usize,
    wait: Duration,
) -> ChurnReport {
    let count = ((plan.hollow_nodes as f64 * plan.churn_fraction).round() as usize).max(1);
    // Every n-th node, so the stopped ones aren't all from one start batch.
    let step = plan.hollow_nodes / count;
    let victims: Vec<usize> = (0..count).map(|n| n * step).collect();
    let names: Vec<String> = victims.iter().map(|index| hollow_name(*index)).collect();
    let mut churn = ChurnReport {
        stopped_nodes: count,
        ..ChurnReport::default()
    };
    let stopped = Instant::now();
    for index in &victims {
        churn.replicas_lost += fleet.stop(*index).await;
    }
    let names = &names;
    churn.detected_ms = time_until(wait, Duration::from_millis(500), move || async move {
        council.nodes().await.is_some_and(|nodes| {
            let alive = alive_names(&nodes);
            names.iter().all(|name| !alive.contains(name))
        })
    })
    .await
    .map(|_| millis(stopped.elapsed()));
    let view = &*fleet;
    churn.rescheduled_ms = time_until(wait, Duration::from_millis(200), move || async move {
        view.running().await >= wanted
    })
    .await
    .map(|_| millis(stopped.elapsed()));

    let restarted = Instant::now();
    if fleet.start(&victims, plan.start_parallelism).await.is_err() {
        return churn;
    }
    churn.rejoined_ms = time_until(wait, Duration::from_millis(500), move || async move {
        council.nodes().await.is_some_and(|nodes| {
            let alive = alive_names(&nodes);
            names.iter().all(|name| alive.contains(name))
        })
    })
    .await
    .map(|_| millis(restarted.elapsed()));
    churn
}

#[test]
fn cpu_time_parses_every_ps_format() {
    assert_eq!(parse_cpu_time("0:01.50"), Some(1.5));
    assert_eq!(parse_cpu_time("12:03.00"), Some(723.0));
    assert_eq!(parse_cpu_time("01:02:03"), Some(3723.0));
    assert_eq!(parse_cpu_time("1-00:00:01"), Some(86_401.0));
    assert_eq!(parse_cpu_time("soon"), None);
}

#[test]
fn summary_percentiles_take_the_nearest_rank() {
    let summary = Summary::of((1..=100).map(f64::from).collect(), 2);
    assert_eq!(summary.count, 100);
    assert_eq!(summary.errors, 2);
    assert_eq!(summary.p50, Some(50.0));
    assert_eq!(summary.p99, Some(99.0));
    assert_eq!(summary.max, Some(100.0));
    assert_eq!(Summary::of(Vec::new(), 0).p50, None);
}
