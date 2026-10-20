//! Real Bun death retires interrupted initialisers and API execs.
//!
//! The same contract covers cron registrations and the ownership records Bun
//! reads at startup: a SIGKILL must neither lose nor resurrect them, and
//! corrupt records stop Bun before its API listens.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reliaburger::config::Config;
use reliaburger::relish::client::BunClient;

#[path = "support/bun_process.rs"]
mod bun_process;
use bun_process::{
    BunProcess, WAIT, assert_success, reserve_address, run_relish, spawn_bun_with_port_retry,
    wait_for_relish, write_portable_node_config,
};

struct Node {
    child: tokio::process::Child,
    client: BunClient,
    endpoint: String,
}

impl Node {
    async fn start(config: &Path, log: &Path) -> Self {
        let offset = std::fs::metadata(log).map_or(0, |metadata| metadata.len()) as usize;
        let output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        // Pay `bun`'s first launch before the readiness deadline (#1149).
        bun_process::first_launch_binaries();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bun"))
            .arg("--config")
            .arg(config)
            .args(["--listen", "127.0.0.1:0", "--runtime", "process"])
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Discover the bound port; releasing a reserved ephemeral port before
        // Bun binds it would race other parallel integration tests.
        let (client, endpoint) = tokio::time::timeout(STATE_DEADLINE, async {
            loop {
                let contents = std::fs::read_to_string(log).unwrap();
                if let Some(address) = contents[offset..]
                    .lines()
                    .find_map(|line| line.strip_prefix("bun: API server listening on "))
                {
                    let endpoint = format!("http://{address}");
                    let client = BunClient::new(&endpoint);
                    if client.health().await.is_ok() {
                        break (client, endpoint);
                    }
                }
                if let Some(status) = child.try_wait().unwrap() {
                    panic!(
                        "Bun exited {status}: {}",
                        std::fs::read_to_string(log).unwrap()
                    );
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Bun must become reachable after recovery");
        Self {
            child,
            client,
            endpoint,
        }
    }

    async fn crash(&mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}

/// Release the bounded workload even if an assertion unwinds before cleanup.
struct ReleaseJob(PathBuf);
impl Drop for ReleaseJob {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "release");
    }
}

/// Overall ceiling for a state change. Each wait returns as soon as the
/// state appears, so a generous ceiling costs nothing on a quiet machine and
/// keeps a loaded runner from failing a correct recovery.
const STATE_DEADLINE: Duration = Duration::from_secs(60);

async fn wait_app_state(client: &BunClient, expected: &str) {
    tokio::time::timeout(STATE_DEADLINE, async {
        loop {
            if client
                .status()
                .await
                .unwrap()
                .iter()
                .any(|row| row.app_name == "work" && row.state == expected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
}

/// The job's pid, from the first status whose runtime evidence is complete.
///
/// Status reads each pid from the process owner and marks an answer it
/// couldn't complete in time `runtime_unknown` rather than hold the reply,
/// so that answer (or a failed request) is worth asking again. A pending
/// retry has no runtime binding yet, even if an earlier job-summary read
/// reported its run as active. A complete running answer with no pid is a
/// running job reported without a process,
/// the product bug #358 fixed, and fails at once.
async fn job_pid(client: &BunClient) -> u32 {
    let deadline = tokio::time::Instant::now() + STATE_DEADLINE;
    loop {
        let last_observed = match client.status().await {
            Ok(statuses) => match statuses.first() {
                Some(status) if status.state == "running" && !status.runtime_unknown => {
                    return status
                        .pid
                        .unwrap_or_else(|| panic!("a running job had no pid: {status:?}"));
                }
                Some(status) => format!("{status:?}"),
                None => "no instances".to_string(),
            },
            Err(error) => format!("status request failed: {error}"),
        };
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runtime never answered for the job within {STATE_DEADLINE:?}; last observed {last_observed}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn killed_bun_retires_api_exec_and_adopts_the_original_workload() {
    let root = tempfile::tempdir().unwrap();
    let config_path = root.path().join("node.toml");
    let log = root.path().join("bun.log");
    let data = root.path().join("data");
    std::fs::write(
        &config_path,
        format!(
            r#"
[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"
[process_workloads]
allowed_binaries = ["/bin/sh"]
mount_isolation = false
[images]
registry_bind = "127.0.0.1"
registry_port = 0
"#,
            root = root.path().display()
        ),
    )
    .unwrap();
    let release = root.path().join("release");
    let _release = ReleaseJob(release.clone());
    let mut config = Config::parse("[app.work]\nexec = '/bin/sh'\n").unwrap();
    config.app.get_mut("work").unwrap().command = vec![
        "-c".into(),
        format!(
            "n=0; while [ ! -f '{}' ] && [ $n -lt 2400 ]; do sleep 0.05; n=$((n+1)); done",
            release.display()
        ),
    ];
    let mut node = Node::start(&config_path, &log).await;
    node.client.apply(&config).await.unwrap();
    wait_app_state(&node.client, "running").await;
    let main_pid = job_pid(&node.client).await;
    let marker = root.path().join("exec-pid");
    let client = BunClient::new(&node.endpoint);
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("echo $$ > '{}'; exec sleep 60", marker.display()),
    ];
    let mut request = tokio::spawn(async move { client.exec("work", "default", &command).await });
    let exec_pid = wait_for_exec_pid(&marker, &mut request).await;
    node.crash().await;
    tokio::time::timeout(STATE_DEADLINE, async {
        loop {
            let owners = data.join("instances/process-owners/default__work-0");
            let active_exec = std::fs::read_dir(&owners).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("exec-")
            });
            if !active_exec && reliaburger::grill::records::process_start_time(exec_pid).is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual Bun death must retire exec and its helper");
    request.abort();
    let _ = request.await;
    let mut recovered = Node::start(&config_path, &log).await;
    wait_app_state(&recovered.client, "running").await;
    assert_eq!(job_pid(&recovered.client).await, main_pid);
    std::fs::write(release, "release").unwrap();
    recovered.client.stop("work", "default").await.unwrap();
    wait_app_state(&recovered.client, "stopped").await;
    recovered.client.stop("work", "default").await.unwrap();
    recovered.crash().await;
}

/// Wait for the exec'd shell to record its pid. An exec request that has
/// already failed can never write it, so report that error at once rather
/// than waiting out the deadline.
async fn wait_for_exec_pid<T: std::fmt::Debug>(
    marker: &Path,
    request: &mut tokio::task::JoinHandle<T>,
) -> u32 {
    let deadline = tokio::time::Instant::now() + STATE_DEADLINE;
    loop {
        if let Ok(value) = std::fs::read_to_string(marker)
            && let Ok(pid) = value.trim().parse()
        {
            return pid;
        }
        if request.is_finished() {
            let outcome = request.await;
            panic!("exec finished before its shell recorded a pid: {outcome:?}");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "exec shell did not record its pid within {STATE_DEADLINE:?}; \
             marker exists: {}",
            marker.exists()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Recovery must retire the interrupted init chain before a new apply can retry it.
#[tokio::test]
async fn killed_bun_during_initialisation_retires_the_chain_before_explicit_retry() {
    use reliaburger::config::app::InitContainerSpec;
    use reliaburger::grill::process::ProcessGrill;
    use reliaburger::grill::state::ContainerState;
    use reliaburger::grill::{Grill, InstanceId};

    let root = tempfile::tempdir().unwrap();
    let config_path = root.path().join("node.toml");
    let log = root.path().join("bun.log");
    std::fs::write(
        &config_path,
        format!(
            r#"
[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"
[process_workloads]
allowed_binaries = ["/bin/sh"]
mount_isolation = false
[images]
registry_bind = "127.0.0.1"
registry_port = 0
"#,
            root = root.path().display()
        ),
    )
    .unwrap();
    let runs = root.path().join("init-runs");
    let pid_file = root.path().join("init-pid");
    let release = root.path().join("release-init");
    let successor = root.path().join("second-init-runs");
    let main = root.path().join("main-runs");
    let _release = ReleaseJob(release.clone());
    let mut config = Config::parse("[app.init-crash]\nexec = '/bin/sh'\n").unwrap();
    let app = config.app.get_mut("init-crash").unwrap();
    app.init = vec![
        InitContainerSpec {
            image: None,
            command: vec!["/bin/sh".into(), "-c".into(),
                "printf 'init\\n' >> \"$1\"; printf '%s\\n' \"$$\" > \"$2\"; n=0; while [ ! -f \"$3\" ] && [ $n -lt 600 ]; do sleep 0.05; n=$((n+1)); done; [ -f \"$3\" ]".into(),
                "init".into(), runs.display().to_string(), pid_file.display().to_string(), release.display().to_string()],
        },
        InitContainerSpec {
            image: None,
            command: vec!["/bin/sh".into(), "-c".into(), "printf 'next\\n' >> \"$1\"".into(), "next".into(), successor.display().to_string()],
        },
    ];
    app.command = vec![
        "-c".into(),
        "printf 'main\\n' >> \"$1\"; exec sleep 60".into(),
        "main".into(),
        main.display().to_string(),
    ];

    let mut node = Node::start(&config_path, &log).await;
    let applying = config.clone();
    let client = BunClient::new(&node.endpoint);
    let request = tokio::spawn(async move { client.apply(&applying).await });
    let initialiser_pid: u32 = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = text.trim().parse()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first initialiser must execute before the crash");
    let original_process = reliaburger::grill::records::process_start_time(initialiser_pid);
    node.crash().await;
    request.abort();
    let _ = request.await;

    let mut recovered = Node::start(&config_path, &log).await;
    let runtime = ProcessGrill::with_owner(
        root.path().join("data/instances"),
        env!("CARGO_BIN_EXE_bun").into(),
    );
    let retired = runtime
        .state(&InstanceId("default__init-crash-0__init-0".into()))
        .await;
    let original_gone =
        reliaburger::grill::records::process_start_time(initialiser_pid) != original_process;
    let did_not_advance = !successor.exists() && !main.exists();
    let recovered_status = recovered.client.status().await.unwrap();
    let runs_before_retry = std::fs::read_to_string(&runs).unwrap();

    std::fs::write(&release, "retry may proceed").unwrap();
    let retry = recovered.client.apply(&config).await;
    let main_started = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if main.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let stopped = recovered.client.stop("init-crash", "default").await;
    recovered.crash().await;
    assert!(original_process.is_some());
    // Recovery retires the unadopted initialiser and then forgets its
    // journal (#738), which the runtime does only for a retired generation.
    assert!(
        matches!(
            retired,
            Ok(ContainerState::Stopped) | Err(reliaburger::grill::GrillError::NotFound { .. })
        ),
        "{retired:?}"
    );
    assert!(
        original_gone,
        "the interrupted initialiser survived recovery"
    );
    assert!(
        did_not_advance,
        "the interrupted chain launched later payloads"
    );
    assert!(
        recovered_status.is_empty(),
        "an unacknowledged application was adopted"
    );
    assert_eq!(runs_before_retry.lines().count(), 1);
    assert_eq!(retry.unwrap().created, 1);
    main_started.expect("explicit retry must reach the main workload");
    stopped.unwrap();
    assert_eq!(std::fs::read_to_string(runs).unwrap().lines().count(), 2);
    assert_eq!(
        std::fs::read_to_string(successor).unwrap().lines().count(),
        1
    );
    assert_eq!(std::fs::read_to_string(main).unwrap().lines().count(), 1);
}

#[test]
fn cron_registration_and_retirement_survive_bun_sigkill() {
    let root = tempfile::tempdir().unwrap();
    let node = write_portable_node_config(root.path());
    let (mut bun, address) = spawn_bun_with_port_retry(false, || {
        (
            node.clone(),
            reserve_address(),
            root.path().join("cron-before.log"),
        )
    });
    let endpoint = format!("http://{address}");
    wait_for_relish(&mut bun, &["--endpoint", &endpoint, "status"]);
    let config = root.path().join("jobs.toml");
    std::fs::write(&config, "[job.keep]\nimage = 'app:v1'\ncommand = ['/bin/true']\nschedule = '0 0 30 2 *'\n[job.remove]\nimage = 'app:v1'\ncommand = ['/bin/true']\nschedule = '0 0 30 2 *'\n").unwrap();
    assert_success(
        &run_relish(&["--endpoint", &endpoint, "apply", config.to_str().unwrap()]),
        "register cron jobs",
    );
    assert_success(
        &run_relish(&["--endpoint", &endpoint, "stop", "remove"]),
        "retire cron before its first firing",
    );
    bun.child.kill().unwrap();
    bun.child.wait().unwrap();
    let (mut replacement, address) = spawn_bun_with_port_retry(false, || {
        (
            node.clone(),
            reserve_address(),
            root.path().join("cron-after.log"),
        )
    });
    let endpoint = format!("http://{address}");
    wait_for_relish(&mut replacement, &["--endpoint", &endpoint, "status"]);
    assert_success(
        &run_relish(&["--endpoint", &endpoint, "stop", "keep"]),
        "retire recovered cron registration",
    );
    let jobs = run_relish(&["--endpoint", &endpoint, "jobs"]);
    assert_success(&jobs, "inspect retained schedule retirement");
    assert!(
        !String::from_utf8_lossy(&jobs.stdout).contains("scheduled"),
        "a retired schedule returned after Bun was killed"
    );
}

#[test]
fn corrupt_workload_ownership_refuses_startup_before_the_api_listens() {
    let root = tempfile::tempdir().unwrap();
    let config = write_portable_node_config(root.path());
    let data = root.path().join("data");
    reliaburger::compatibility::ensure_state_compatible(&data).unwrap();
    let records = data.join("instances");
    std::fs::create_dir_all(&records).unwrap();
    let record = records.join("default__web-0.json");
    std::fs::write(&record, b"{incomplete").unwrap();
    let log = root.path().join("corrupt-ownership.log");
    let mut bun = BunProcess::spawn(&config, "127.0.0.1:0".parse().unwrap(), false, log.clone());
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = bun.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Bun did not refuse corrupt ownership"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    assert_eq!(std::fs::read(&record).unwrap(), b"{incomplete");
    let output = std::fs::read_to_string(log).unwrap();
    assert!(
        output.contains("cannot restore workload ownership"),
        "{output}"
    );
    assert!(!output.contains("API server listening"), "{output}");
}

/// Serve successive status snapshots without depending on child scheduling.
struct PidStatusFixture {
    client: BunClient,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for PidStatusFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl PidStatusFixture {
    async fn start(states: Vec<serde_json::Value>) -> Self {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let cursor = Arc::new(AtomicUsize::new(0));
        let states = Arc::new(states);
        let router = axum::Router::new().route(
            "/v1/status",
            axum::routing::get(move || {
                let states = states.clone();
                let cursor = cursor.clone();
                async move {
                    let index = cursor.fetch_add(1, Ordering::SeqCst).min(states.len() - 1);
                    axum::Json(vec![states[index].clone()])
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            client: BunClient::new(&format!("http://{address}")),
            server,
        }
    }
}

fn pid_status(state: &str, pid: Option<u32>, unknown: bool) -> serde_json::Value {
    serde_json::json!({"id":"run-1", "app_name":"work", "namespace":"default",
        "state":state, "restart_count":1, "host_port":null, "pid":pid,
        "runtime_unknown":unknown})
}

#[tokio::test]
async fn pid_wait_accepts_a_pending_retry_before_its_running_process() {
    let fixture = PidStatusFixture::start(vec![
        pid_status("pending", None, false),
        pid_status("running", Some(123), false),
    ])
    .await;
    assert_eq!(job_pid(&fixture.client).await, 123);
}

#[tokio::test]
async fn pid_wait_keeps_unknown_runtime_evidence_pending() {
    let fixture = PidStatusFixture::start(vec![
        pid_status("running", None, true),
        pid_status("running", Some(123), false),
    ])
    .await;
    assert_eq!(job_pid(&fixture.client).await, 123);
}

#[tokio::test]
#[should_panic(expected = "a running job had no pid")]
async fn pid_wait_still_refuses_a_known_running_process_without_a_pid() {
    let fixture = PidStatusFixture::start(vec![pid_status("running", None, false)]).await;
    job_pid(&fixture.client).await;
}
