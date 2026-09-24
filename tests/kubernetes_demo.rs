//! Z1.5: the tutorial's Kubernetes demo runs on a real Runc node.
//!
//! One real Bun, with the runc runtime, eBPF service discovery, the `.internal`
//! DNS responder and ingress, applies `examples/kubernetes/podinfo.yaml` with
//! `relish apply -f`. The frontend, reached through the ingress by its host
//! name, must reach the backend as `backend` and redis as `redis`: the short
//! Kubernetes names (Z1.2), the images' own entrypoints, users and working
//! directories (Z1.1), and the import (Z1.3, Z1.4) all have to work for that.
//!
//! Needs root, runc, nftables, bpffs and a cgroup v2 host, so it runs in the
//! provisioned Linux VM only. The pinned images come from the local test
//! mirror (`make test-linux` starts one) or, without it, the internet.
#![cfg(all(target_os = "linux", feature = "ebpf", feature = "kubernetes"))]

#[path = "support/bun_process.rs"]
mod bun_process;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bun_process::{BunProcess, BunStart, reserve_address, wait_for_bind};
use reliaburger::onion::trace::TraceVerdict;
use reliaburger::relish::client::BunClient;

const APPS: [&str; 4] = ["frontend", "backend", "redis", "loadgen"];

/// Stops the demo and removes what it leaves on the host, including while a
/// failed assertion unwinds. Best-effort: nothing here may panic.
struct Cleanup {
    root: PathBuf,
    api: Option<std::net::SocketAddr>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(address) = self.api {
            let _ = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()?;
                        runtime.block_on(async {
                            let client = BunClient::new(&format!("http://{address}"));
                            for app in APPS {
                                let _ = tokio::time::timeout(
                                    Duration::from_secs(30),
                                    client.stop(app, "default"),
                                )
                                .await;
                            }
                        });
                        Ok::<_, std::io::Error>(())
                    })
                    .join()
            });
        }
        kill_root_processes(&self.root);
        delete_runc_containers(&self.root.join("data/instances/runc/state"));
        retire_kernel(&self.root);
        for app in APPS {
            remove_network(&format!("default__{app}"));
            remove_cgroup(&Path::new("/sys/fs/cgroup/reliaburger/default").join(app));
        }
    }
}

/// A podinfo demo applied to one real Runc node, cleaned up when dropped.
struct Demo {
    root: PathBuf,
    api: std::net::SocketAddr,
    /// Talks to the ingress by the demo's host name.
    http: reqwest::Client,
    /// `http://podinfo.localhost:<ingress port>`.
    base: String,
    // Field order is drop order: stop Bun, then clean up after it.
    _bun: BunProcess,
    _cleanup: Cleanup,
}

impl Demo {
    fn log(&self) -> String {
        std::fs::read_to_string(self.root.join("bun.log")).unwrap_or_default()
    }

    fn client(&self) -> BunClient {
        BunClient::new(&format!("http://{}", self.api))
    }

    /// Everything a failed demo run needs to explain itself.
    async fn diagnostics(&self) -> String {
        diagnostics(&self.root, self.api, "").await
    }

    /// `relish path frontend --to redis --count 5`, printed for the record.
    async fn path_frontend_to_redis(&self) -> reliaburger::onion::trace::TraceResult {
        let result = reliaburger::relish::path_cmd::probe_path(
            &reliaburger::onion::trace::TraceRequest {
                source: "frontend".to_string(),
                source_namespace: "default".to_string(),
                destination: "redis".to_string(),
                destination_namespace: "default".to_string(),
                port: None,
                count: Some(5),
            },
            &self.client(),
        )
        .await
        .unwrap_or_else(|error| panic!("path probe failed: {error}\n{}", self.log()));
        eprintln!(
            "path: {}\n{}",
            serde_json::to_string(&result.overall_result).unwrap(),
            result
                .steps
                .iter()
                .flat_map(|step| {
                    std::iter::once(format!(
                        "  {}. {} {:?}",
                        step.step_number, step.name, step.verdict
                    ))
                    .chain(step.details.iter().map(|detail| format!("     {detail}")))
                })
                .collect::<Vec<_>>()
                .join("\n")
        );
        result
    }

    /// The frontends' recent log lines about the cache, for the record.
    async fn frontend_cache_logs(&self) -> String {
        let options = reliaburger::relish::client::LogOptions {
            tail: Some(12),
            follow: false,
            grep: Some("cache".to_string()),
            start: None,
            json_field: None,
        };
        self.client()
            .logs("frontend", "default", &options)
            .await
            .unwrap_or_else(|error| format!("(logs unavailable: {error})"))
    }

    /// The median time of five `GET /cache/demo` calls through the ingress.
    async fn median_cache_read(&self) -> Duration {
        let mut samples = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            let response = self
                .http
                .get(format!("{}/cache/demo", self.base))
                .send()
                .await
                .expect("cache read answered");
            let _ = response.bytes().await;
            samples.push(started.elapsed());
        }
        samples.sort();
        samples[samples.len() / 2]
    }

    /// Write a value through the frontend into redis and read it back.
    async fn cache_round_trip(&self, value: &str) -> Result<String, String> {
        let stored = self
            .http
            .post(format!("{}/cache/demo", self.base))
            .body(value.to_string())
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !stored.status().is_success() {
            let status = stored.status();
            let body = stored.text().await.unwrap_or_default();
            return Err(format!("store: {status} {body}"));
        }
        let read = self
            .http
            .get(format!("{}/cache/demo", self.base))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = read.status();
        let body = read.text().await.unwrap_or_default();
        if status.is_success() && body.contains(value) {
            Ok(body)
        } else {
            Err(format!("read: {status} {body}"))
        }
    }
}

/// The local test mirrors as a TOML inline table, `{}` without a mirror.
fn inline_mirrors() -> String {
    let mirrors = reliaburger::testkit::pinned_images::local_test_mirrors().unwrap();
    let pairs: Vec<String> = mirrors
        .as_map()
        .iter()
        .map(|(upstream, mirror)| format!("{upstream:?} = {mirror:?}"))
        .collect();
    format!("{{ {} }}", pairs.join(", "))
}

/// Start one Runc Bun with eBPF, DNS and ingress, apply the podinfo manifest
/// and wait until the frontend answers and reaches the backend and redis by
/// name. `extra_config` is appended to the node config.
async fn start_demo(extra_config: &str) -> Demo {
    assert!(nix::unistd::geteuid().is_root(), "run as root");
    let root = tempfile::tempdir().unwrap().keep();
    let mut cleanup = Cleanup {
        root: root.clone(),
        api: None,
    };
    let ingress = reserve_address().port();
    let config = root.join("node.toml");
    std::fs::write(
        &config,
        format!(
            r#"
[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"
[images]
registry_bind = "127.0.0.1"
registry_port = 0
mirrors = {mirrors}
[ebpf]
enabled = true
[dns]
enabled = true
listen = "0.0.0.0:53"
[ingress]
enabled = true
http_port = {ingress}
https_port = {https}
{extra_config}
"#,
            root = root.display(),
            https = reserve_address().port(),
            // The pinned podinfo, redis and busybox images come from the
            // local test mirror when the harness runs one.
            mirrors = inline_mirrors(),
        ),
    )
    .unwrap();

    let mut bun = BunProcess::spawn_runtime(
        &config,
        "127.0.0.1:0".parse().unwrap(),
        false,
        root.join("bun.log"),
        "runc",
    );
    let BunStart::Ready(api) = wait_for_bind(&mut bun, "127.0.0.1:0".parse().unwrap()) else {
        panic!("bun lost a port race on an ephemeral API port");
    };
    cleanup.api = Some(api);

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/kubernetes/podinfo.yaml");
    let applied = tokio::time::timeout(
        Duration::from_secs(600),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(["apply", "-f", manifest.to_str().unwrap()])
            .env("RELIABURGER_ENDPOINT", format!("http://{api}"))
            .env_remove("RELIABURGER_TOKEN")
            .env_remove("RELIABURGER_CA_CERT")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("apply finished")
    .unwrap();
    let log = || std::fs::read_to_string(root.join("bun.log")).unwrap_or_default();
    assert!(
        applied.status.success(),
        "apply failed:\n{}\n{}\nbun log:\n{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr),
        log()
    );

    // Through the ingress, by host name, the way the tutorial's browser does.
    let http = reqwest::Client::builder()
        .resolve(
            "podinfo.localhost",
            std::net::SocketAddr::from(([127, 0, 0, 1], ingress)),
        )
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("http://podinfo.localhost:{ingress}");
    // The last answer each wait saw, so a timeout says what went wrong.
    let last = RefCell::new(String::new());
    let home = eventually(Duration::from_secs(180), || async {
        let outcome = get_text(http.get(&base)).await;
        let body = remember(&last, outcome)?;
        body.contains("podinfo").then_some(body)
    })
    .await;
    let Some(home) = home else {
        let last = last.borrow().clone();
        panic!(
            "the frontend never answered through ingress\n{}",
            diagnostics(&root, api, &last).await
        );
    };
    assert!(home.contains("\"hostname\""), "{home}");

    // The frontend forwards /api/echo to --backend-url=http://backend:9898,
    // and answers with the list of backend responses.
    let echoed = eventually(Duration::from_secs(60), || async {
        let outcome = get_text(
            http.post(format!("{base}/api/echo"))
                .body("reliaburger-demo"),
        )
        .await;
        let body = remember(&last, outcome)?;
        let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
        parsed.is_array().then_some(body)
    })
    .await;
    let Some(echoed) = echoed else {
        let last = last.borrow().clone();
        panic!(
            "frontend never reached the backend by name\n{}",
            diagnostics(&root, api, &last).await
        );
    };
    assert!(echoed.contains("reliaburger-demo"), "{echoed}");

    let demo = Demo {
        root,
        api,
        http,
        base,
        _bun: bun,
        _cleanup: cleanup,
    };
    // The frontend's /cache API stores in --cache-server=tcp://redis:6379.
    let cached = eventually(Duration::from_secs(60), || async {
        let outcome = demo.cache_round_trip("kept-in-redis").await;
        remember(&last, outcome)
    })
    .await;
    if cached.is_none() {
        let last = last.borrow().clone();
        panic!(
            "frontend never reached redis by name\n{}",
            diagnostics(&demo.root, demo.api, &last).await
        );
    }
    demo
}

#[tokio::test]
#[ignore = "requires root, runc, nftables, bpffs and the pinned images (local test mirror or internet access)"]
async fn podinfo_demo_frontend_reaches_backend_and_redis_by_name() {
    let demo = start_demo("").await;

    // Three frontends, as the manifest asks.
    let statuses = demo.client().cluster_status().await.unwrap();
    let frontends = statuses
        .iter()
        .filter(|row| row.instance.app_name == "frontend")
        .count();
    assert_eq!(frontends, 3, "{statuses:?}");

    // The load generator calls the frontend as `frontend` and writes the
    // time into /cache/loadgen; seeing a value there through the ingress
    // proves its traffic goes all the way to redis (Z6.7).
    let written = eventually(Duration::from_secs(90), || async {
        let response = demo
            .http
            .get(format!("{}/cache/loadgen", demo.base))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.text().await.ok()?;
        (!body.trim().is_empty()).then_some(body)
    })
    .await;
    assert!(
        written.is_some(),
        "the load generator never wrote to the cache:\n{}",
        demo.log()
    );
}

/// The tour's fault beats on the real demo: network faults act on the
/// frontend's own connections to redis, including the ones its pool already
/// holds open.
#[tokio::test]
#[ignore = "requires root, runc, nftables, bpffs and the pinned images (local test mirror or internet access)"]
async fn podinfo_demo_feels_network_faults_between_frontend_and_redis() {
    use reliaburger::smoker::types::{FaultRequest, FaultType};

    let demo = start_demo(
        r#"
[testing]
safety_class = "development"
allowed_operations = ["inject_workload_faults"]
"#,
    )
    .await;
    let client = demo.client();
    let fault = |fault_type| FaultRequest {
        fault_type,
        target_service: "redis".to_string(),
        namespace: Some("default".to_string()),
        target_instance: None,
        target_node: None,
        duration: Duration::from_secs(120),
        injected_by: String::new(),
        reason: Some("tour".to_string()),
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    };

    // Z6.4: with nothing injected, the path passes end to end.
    let clean = demo.path_frontend_to_redis().await;
    assert_eq!(
        clean.overall_result,
        TraceVerdict::Pass,
        "{}",
        serde_json::to_string_pretty(&clean).unwrap()
    );

    // Z6.1 + Z6.2: a partition from the frontend cuts the connections its
    // redis pool already holds, so the very next cache call fails instead of
    // riding an old connection.
    demo.cache_round_trip("before-the-partition")
        .await
        .expect("the pool is warm before the fault");
    client
        .inject_fault(&fault(FaultType::Partition {
            source_app: Some("frontend".to_string()),
        }))
        .await
        .unwrap_or_else(|error| panic!("partition refused: {error}\n{}", demo.log()));
    // The ingress spreads calls over the three frontends, so six calls
    // reach each one's pool twice. Every one must fail straight away.
    let mut outcomes = Vec::new();
    for attempt in 0..6 {
        outcomes.push(
            demo.cache_round_trip(&format!("during-the-partition-{attempt}"))
                .await,
        );
    }
    eprintln!("under partition, podinfo says: {outcomes:?}");
    eprintln!(
        "frontend logs under partition:\n{}",
        demo.frontend_cache_logs().await
    );
    let partitioned = demo.path_frontend_to_redis().await;
    assert!(
        matches!(&partitioned.overall_result, TraceVerdict::Fail { reason } if reason.contains("partition from frontend")),
        "{}",
        serde_json::to_string_pretty(&partitioned).unwrap()
    );
    assert!(
        outcomes.iter().all(Result::is_err),
        "a frontend kept reaching redis through a partition: {outcomes:?}\n{}",
        demo.log()
    );

    client
        .clear_faults_by_service("redis", Some("default"))
        .await
        .unwrap();
    let healed = eventually(Duration::from_secs(20), || async {
        demo.cache_round_trip("after-the-partition").await.ok()
    })
    .await;
    if healed.is_none() {
        panic!("redis never came back\n{}", demo.diagnostics().await);
    }

    // Z6.3: a 300ms delay from the frontend to redis. Reading a key is two
    // redis commands (EXISTS, then GET), each held back 300ms on the way
    // out, over connections the pool already has open.
    let before = demo.median_cache_read().await;
    let mut delay = fault(FaultType::Delay {
        delay_ns: 300_000_000,
        jitter_ns: 0,
        source_app: Some("frontend".to_string()),
    });
    delay.duration = Duration::from_secs(60);
    client
        .inject_fault(&delay)
        .await
        .unwrap_or_else(|error| panic!("delay refused: {error}\n{}", demo.log()));
    let during = demo.median_cache_read().await;
    eprintln!("cache read median: {before:?} before the delay, {during:?} during it");
    let delayed = demo.path_frontend_to_redis().await;
    assert!(
        matches!(&delayed.overall_result, TraceVerdict::Degraded { reason } if reason.contains("delay 300ms from frontend")),
        "{}",
        serde_json::to_string_pretty(&delayed).unwrap()
    );
    let median = delayed
        .latency_ms
        .expect("connects timed inside the container");
    assert!(
        median >= 290.0,
        "median connect {median} ms under a 300ms delay"
    );
    assert!(
        during >= before + Duration::from_millis(500),
        "a 300ms delay only moved the cache read from {before:?} to {during:?}"
    );

    // Every frontend carries the delay, and one that restarts mid-fault
    // gets it back: the agent reconciles delays against the instances
    // running now, not the ones that ran at injection.
    for replica in 0..3 {
        let qdisc = frontend_qdisc(replica);
        assert!(qdisc.contains("netem"), "frontend-{replica}: {qdisc}");
    }
    let restarts_before = frontend_restarts(&client, "default__frontend-1").await;
    let mut kill = fault(FaultType::Kill { count: 1 });
    kill.target_service = "frontend".to_string();
    kill.target_instance = Some("default__frontend-1".to_string());
    kill.duration = Duration::ZERO;
    client
        .inject_fault(&kill)
        .await
        .unwrap_or_else(|error| panic!("kill refused: {error}\n{}", demo.log()));
    let restarted = eventually(Duration::from_secs(60), || async {
        let restarts = frontend_restarts(&client, "default__frontend-1").await;
        (restarts > restarts_before).then_some(restarts)
    })
    .await;
    assert!(
        restarted.is_some(),
        "frontend-1 never restarted:\n{}",
        demo.log()
    );
    let reshaped = eventually(Duration::from_secs(10), || async {
        let qdisc = frontend_qdisc(1);
        qdisc.contains("netem").then_some(qdisc)
    })
    .await;
    assert!(
        reshaped.is_some(),
        "the restarted frontend lost its delay: {}",
        frontend_qdisc(1)
    );
    eprintln!(
        "frontend-1 restarted ({restarts_before} -> {:?}) and carries: {}",
        restarted,
        reshaped.unwrap().trim()
    );

    client
        .clear_faults_by_service("redis", Some("default"))
        .await
        .unwrap();
    for replica in 0..3 {
        let qdisc = frontend_qdisc(replica);
        assert!(
            !qdisc.contains("fa01:"),
            "frontend-{replica} kept its delay: {qdisc}"
        );
    }
    let after = demo.median_cache_read().await;
    eprintln!("cache read median after clearing the delay: {after:?}");
    let healed = demo.path_frontend_to_redis().await;
    assert_eq!(healed.overall_result, TraceVerdict::Pass);
    assert!(
        healed.latency_ms.is_some_and(|median| median < 100.0),
        "{:?}",
        healed.latency_ms
    );
    assert!(
        after < before + Duration::from_millis(200),
        "the delay outlived its clear: {before:?} before, {after:?} after"
    );
}

/// The root qdiscs on a frontend replica's container interface.
fn frontend_qdisc(replica: u32) -> String {
    let output = std::process::Command::new("ip")
        .args([
            "netns",
            "exec",
            &format!("rb-default__frontend-{replica}"),
            "tc",
            "qdisc",
            "show",
            "dev",
            "eth0",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// How many times Bun has restarted a frontend instance.
async fn frontend_restarts(client: &BunClient, instance: &str) -> u32 {
    client
        .status()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|status| status.id == instance && status.state == "running")
        .map(|status| status.restart_count)
        .unwrap_or_default()
}

/// Send a request and read its body. A non-2xx answer is an error that
/// keeps its status and body.
async fn get_text(request: reqwest::RequestBuilder) -> Result<String, String> {
    let response = request.send().await.map_err(|error| format!("{error:?}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("{status}, body unreadable: {error}"))?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(format!("{status} {body}"))
    }
}

/// Record the latest outcome of a polled request in `last`, and pass the
/// body on when there is one.
fn remember(last: &RefCell<String>, outcome: Result<String, String>) -> Option<String> {
    let text = match &outcome {
        Ok(body) => format!("ok: {body}"),
        Err(error) => format!("error: {error}"),
    };
    *last.borrow_mut() = bounded(&text, 2000);
    outcome.ok()
}

/// At most `limit` bytes of `text`, cut on a character boundary.
fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... ({} bytes more)", &text[..end], text.len() - end)
}

/// The last `lines` lines of `text`.
fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// A command's stdout and stderr, cut to its last `lines` lines, or why it
/// didn't run.
fn command_output(program: &str, args: &[&str], lines: usize) -> String {
    match std::process::Command::new(program).args(args).output() {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            tail(text.trim_end(), lines)
        }
        Err(error) => format!("(cannot run {program}: {error})"),
    }
}

/// Every file under `directory` ending in `.stdout` or `.stderr`: the
/// containers' captured output.
fn captured_output_files(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            captured_output_files(&path, found);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "stdout" || extension == "stderr")
        {
            found.push(path);
        }
    }
}

/// What a failed demo prints: the last answer the test saw, the host
/// (kernel, runc, user-namespace and forwarding settings), every instance's
/// state and output, runc's own view and the tail of Bun's log. Bounded, so
/// a CI log stays readable.
async fn diagnostics(root: &Path, api: std::net::SocketAddr, last: &str) -> String {
    let mut report = String::new();
    let mut section = |title: &str, body: &str| {
        report.push_str(&format!("\n=== {title} ===\n{}\n", body.trim_end()));
    };
    section("last answer", last);

    let sysctls = [
        "kernel.apparmor_restrict_unprivileged_userns",
        "kernel.unprivileged_userns_clone",
        "user.max_user_namespaces",
        "net.ipv4.ip_forward",
    ]
    .iter()
    .map(|name| {
        let path = Path::new("/proc/sys").join(name.replace('.', "/"));
        let value = std::fs::read_to_string(path).unwrap_or_else(|_| "(absent)".to_string());
        format!("{name} = {}", value.trim())
    })
    .collect::<Vec<_>>()
    .join("\n");
    let subuid = std::fs::read_to_string("/etc/subuid").unwrap_or_default();
    section(
        "host",
        &format!(
            "kernel: {}\nrunc: {}\n{sysctls}\n/etc/subuid:\n{}\ndemo root filesystem: {}",
            command_output("uname", &["-r"], 1),
            command_output("sh", &["-c", "runc --version | head -1"], 1),
            tail(&subuid, 10),
            command_output("stat", &["-f", "-c", "%T", &root.display().to_string()], 1),
        ),
    );
    // Container-to-container traffic crosses the host's forward hook, where
    // a DROP policy (Docker's, ufw's) cuts the frontend off from its peers.
    section(
        "forwarding",
        &format!(
            "iptables -S FORWARD:\n{}\nnft forward chains:\n{}",
            command_output("iptables", &["-S", "FORWARD"], 30),
            command_output(
                "sh",
                &["-c", "nft list chains | grep -B2 'hook forward'"],
                30
            ),
        ),
    );

    let client = BunClient::new(&format!("http://{api}"));
    let statuses = match tokio::time::timeout(Duration::from_secs(10), client.status()).await {
        Ok(Ok(statuses)) => statuses
            .iter()
            .map(|status| {
                format!(
                    "{} {} restarts={} exit={:?} pid={:?}",
                    status.id, status.state, status.restart_count, status.exit_code, status.pid
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Ok(Err(error)) => format!("(status failed: {error})"),
        Err(_) => "(status timed out)".to_string(),
    };
    section("instances", &statuses);
    for app in APPS {
        let options = reliaburger::relish::client::LogOptions {
            tail: Some(15),
            follow: false,
            grep: None,
            start: None,
            json_field: None,
        };
        let logs = match tokio::time::timeout(
            Duration::from_secs(10),
            client.logs(app, "default", &options),
        )
        .await
        {
            Ok(Ok(logs)) => logs,
            Ok(Err(error)) => format!("(logs failed: {error})"),
            Err(_) => "(logs timed out)".to_string(),
        };
        section(&format!("{app} logs"), &bounded(&logs, 4000));
    }

    // The containers' own output (in case the log API is what broke) and
    // every runtime command's stderr, where runc explains a failed create.
    // Runtime commands' stdout is runc's state JSON, which says nothing new.
    let mut files = Vec::new();
    captured_output_files(&root.join("data"), &mut files);
    files.retain(|file| {
        file.extension()
            .is_some_and(|extension| extension == "stderr")
            || file.components().any(|part| part.as_os_str() == "launcher")
    });
    files.sort();
    for file in files.iter().take(24) {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        if !text.trim().is_empty() {
            let name = file.strip_prefix(root).unwrap_or(file);
            section(
                &name.display().to_string(),
                &bounded(&tail(&text, 10), 2000),
            );
        }
    }

    let state = root.join("data/instances/runc/state");
    section(
        "runc list",
        &command_output(
            "runc",
            &["--root", &state.display().to_string(), "list"],
            20,
        ),
    );
    let log = std::fs::read_to_string(root.join("bun.log")).unwrap_or_default();
    section("bun log (tail)", &tail(&log, 80));
    report
}

/// Poll `check` until it returns `Some`, for at most `limit`.
async fn eventually<T, F, Fut>(limit: Duration, mut check: F) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + limit;
    loop {
        if let Some(value) = check().await {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// SIGKILL every process whose command line names a path under the root:
/// Bun and the detached owners it launched.
fn kill_root_processes(root: &Path) {
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::{Pid, getpgid, getpgrp};
    use std::os::unix::ffi::OsStrExt;
    let mut needle = root.as_os_str().as_bytes().to_vec();
    needle.push(b'/');
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(command) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if !command.windows(needle.len()).any(|part| part == needle) {
            continue;
        }
        let pid = Pid::from_raw(pid);
        match getpgid(Some(pid)) {
            Ok(group) if group != getpgrp() => {
                let _ = killpg(group, Signal::SIGKILL);
            }
            _ => {
                let _ = kill(pid, Signal::SIGKILL);
            }
        }
    }
    std::thread::sleep(Duration::from_millis(500));
}

fn delete_runc_containers(state: &Path) {
    let Ok(listed) = std::process::Command::new("runc")
        .arg("--root")
        .arg(state)
        .args(["list", "--quiet"])
        .output()
    else {
        return;
    };
    for id in String::from_utf8_lossy(&listed.stdout).lines() {
        let _ = std::process::Command::new("runc")
            .arg("--root")
            .arg(state)
            .args(["delete", "--force", id])
            .output();
    }
}

/// Unpin and detach the eBPF programs Bun attached: left behind, they'd
/// intercept every connect() on the host.
fn retire_kernel(root: &Path) {
    let policy = root.join("data/kernel-policy");
    let Ok(bytes) = std::fs::read(policy.join("owner.json")) else {
        return;
    };
    let Ok(owner) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    let (Some(cgroup), Some(pins)) = (
        owner["cgroup_path"].as_str(),
        owner["pin_directory"].as_str(),
    ) else {
        return;
    };
    if !Path::new(pins).exists() {
        return;
    }
    if let Err(error) = reliaburger::onion::ebpf::loader::OnionEbpf::retire_owned_state(
        Path::new(cgroup),
        &policy,
        Path::new(pins),
    ) {
        eprintln!("test cleanup: cannot retire the kernel programs: {error}");
    }
    let _ = std::fs::remove_dir(pins);
}

/// Delete the namespaces and veths of every instance whose id starts with
/// `prefix`.
fn remove_network(prefix: &str) {
    use reliaburger::grill::{InstanceId, netns};
    for entry in std::fs::read_dir("/run/netns")
        .into_iter()
        .flatten()
        .flatten()
    {
        let namespace = entry.file_name().to_string_lossy().into_owned();
        let Some(instance) = namespace.strip_prefix("rb-") else {
            continue;
        };
        if !instance.starts_with(prefix) {
            continue;
        }
        let veth = netns::host_veth_name(&InstanceId(instance.to_owned()));
        let _ = std::process::Command::new("ip")
            .args(["link", "del", &veth])
            .output();
        let _ = std::process::Command::new("ip")
            .args(["netns", "del", &namespace])
            .output();
    }
}

/// Kill a cgroup subtree's processes and remove it bottom-up.
fn remove_cgroup(path: &Path) {
    for entry in std::fs::read_dir(path.parent().unwrap_or(path))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let wanted = path
            .file_name()
            .is_some_and(|app| name.starts_with(&*app.to_string_lossy()));
        if wanted {
            remove_cgroup_tree(&entry.path());
        }
    }
}

fn remove_cgroup_tree(path: &Path) {
    let _ = std::fs::write(path.join("cgroup.kill"), "1");
    for child in std::fs::read_dir(path).into_iter().flatten().flatten() {
        if child.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_cgroup_tree(&child.path());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::remove_dir(path).is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}
