//! Real Bun death preserves job outcomes and retires interrupted initialisers.
//!
//! The same contract covers cron registrations, node-local test-job leases and
//! the ownership records Bun reads at startup: a SIGKILL must neither lose nor
//! resurrect them, and corrupt records stop Bun before its API listens.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use reliaburger::config::{Config, types::EnvValue};
use reliaburger::relish::client::BunClient;

#[path = "support/bun_process.rs"]
mod bun_process;
use bun_process::{
    BunProcess, WAIT, assert_success, reserve_address, reserve_ports, run_relish,
    spawn_bun_with_port_retry, wait_for_relish, write_portable_node_config,
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

async fn wait_job(client: &BunClient, expected_state: &str, restarts: u32) {
    let deadline = tokio::time::Instant::now() + STATE_DEADLINE;
    loop {
        let last_observed = match client.jobs().await {
            Ok(jobs) => {
                let work: Vec<_> = jobs.iter().filter(|job| job.name == "work").collect();
                if work
                    .iter()
                    .any(|job| job.state == expected_state && job.restart_count == restarts)
                {
                    return;
                }
                let states: Vec<_> = work
                    .iter()
                    .map(|job| format!("{} with {} retries", job.state, job.restart_count))
                    .collect();
                format!("{states:?}")
            }
            Err(error) => format!("jobs request failed: {error}"),
        };
        assert!(
            tokio::time::Instant::now() < deadline,
            "job must become {expected_state} with {restarts} retries within \
             {STATE_DEADLINE:?}; last observed {last_observed}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn killed_bun_preserves_job_budget_and_requires_explicit_rerun() {
    let root = tempfile::tempdir().unwrap();
    let config_path = root.path().join("node.toml");
    let log = root.path().join("bun.log");
    let data = root.path().join("data");
    std::fs::write(
        &config_path,
        format!(
            r#"
[storage]
data = "{data}"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"
[images]
registry_bind = "127.0.0.1"
registry_port = 0
"#,
            data = data.display(),
            root = root.path().display()
        ),
    )
    .unwrap();
    let count = root.path().join("runs");
    let release = root.path().join("release");
    let start_retry = root.path().join("start-retry");
    let _release = ReleaseJob(release.clone());
    let mut config = Config::parse("[job.work]\nimage = 'proc-grill:image-ignored'\n").unwrap();
    let job = config.job.get_mut("work").unwrap();
    job.command = Some(vec!["/bin/sh".into(), "-c".into(),
        "if [ -f \"$RUN_FILE\" ]; then i=0; while [ ! -f \"$START_RETRY\" ] && [ ! -f \"$RELEASE_FILE\" ] && [ $i -lt 2400 ]; do i=$((i+1)); sleep 0.05; done; fi; printf 'run\\n' >> \"$RUN_FILE\"; if [ \"$(wc -l < \"$RUN_FILE\")\" -eq 1 ]; then exit 1; fi; i=0; while [ $i -lt 2400 ]; do if [ -f \"$RELEASE_FILE\" ]; then if [ \"$(wc -l < \"$RUN_FILE\")\" -eq 2 ]; then kill -TERM $$; else exit 0; fi; fi; i=$((i+1)); sleep 0.05; done; exit 1".into()]);
    job.env.insert(
        "RUN_FILE".into(),
        EnvValue::Plain(count.display().to_string()),
    );
    job.env.insert(
        "RELEASE_FILE".into(),
        EnvValue::Plain(release.display().to_string()),
    );
    job.env.insert(
        "START_RETRY".into(),
        EnvValue::Plain(start_retry.display().to_string()),
    );
    let manifest = root.path().join("job.toml");
    std::fs::write(&manifest, toml::to_string(&config).unwrap()).unwrap();

    let mut node = Node::start(&config_path, &log).await;
    let client = &node.client;
    client.apply(&config).await.unwrap();
    wait_job(client, "running", 1).await;
    let pid = client.status().await.unwrap()[0].pid.unwrap();
    // Running proves spawn succeeded, not that the child has executed printf.
    // The gate deliberately exercises that scheduling gap before the crash.
    std::fs::write(&start_retry, "start").unwrap();
    tokio::time::timeout(STATE_DEADLINE, async {
        loop {
            let runs = std::fs::read_to_string(&count).unwrap();
            if runs.lines().count() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("retry must execute its workload before injecting Bun death");
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2);
    node.crash().await;

    let mut node = Node::start(&config_path, &log).await;
    let client = &node.client;
    wait_job(client, "running", 1).await;
    assert_eq!(client.status().await.unwrap()[0].pid, Some(pid));
    std::fs::write(&release, "release").unwrap();
    wait_job(client, "unknown", 1).await;
    let error = client.apply(&config).await.unwrap_err();
    assert!(error.to_string().contains("rerun"), "{error}");
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2);
    node.crash().await;

    let mut node = Node::start(&config_path, &log).await;
    let client = &node.client;
    wait_job(client, "unknown", 1).await;
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["--endpoint", &node.endpoint, "apply"])
        .arg(&manifest)
        .arg("--rerun-jobs")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_job(client, "stopped", 0).await;
    assert_eq!(client.status().await.unwrap()[0].exit_code, Some(0));
    assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 3);
    client.stop("work", "default").await.unwrap();
    node.crash().await;
}

#[tokio::test]
async fn completed_job_survives_bun_death_with_or_without_adoption_record() {
    for remove_adoption_record in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("node.toml");
        let log = root.path().join("bun.log");
        let data = root.path().join("data");
        std::fs::write(
            &config_path,
            format!(
                r#"
[storage]
data = "{data}"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"
[images]
registry_bind = "127.0.0.1"
registry_port = 0
"#,
                data = data.display(),
                root = root.path().display()
            ),
        )
        .unwrap();
        let release = root.path().join("release");
        let _release = ReleaseJob(release.clone());
        let mut config = Config::parse("[job.work]\nimage = 'proc-grill:image-ignored'\n").unwrap();
        let job = config.job.get_mut("work").unwrap();
        job.command = Some(vec!["/bin/sh".into(), "-c".into(),
            "i=0; while [ ! -f \"$RELEASE_FILE\" ] && [ $i -lt 600 ]; do i=$((i+1)); sleep 0.05; done; [ -f \"$RELEASE_FILE\" ]".into()]);
        job.env.insert(
            "RELEASE_FILE".into(),
            EnvValue::Plain(release.display().to_string()),
        );
        let mut node = Node::start(&config_path, &log).await;
        node.client.apply(&config).await.unwrap();
        wait_job(&node.client, "running", 0).await;
        node.crash().await;
        if remove_adoption_record {
            // Inject missing agent metadata after physical Bun death. The
            // runtime intent remains; this does not claim a timed pre-write kill.
            std::fs::remove_file(data.join("instances/default__work-0.json")).unwrap();
        }
        std::fs::write(&release, "release").unwrap();
        let grill = reliaburger::grill::process::ProcessGrill::with_owner(
            data.join("instances"),
            env!("CARGO_BIN_EXE_bun").into(),
        );
        let id = reliaburger::grill::InstanceId("default__work-0".into());
        use reliaburger::grill::Grill;
        tokio::time::timeout(Duration::from_secs(15), async {
            while grill.state(&id).await.unwrap() != reliaburger::grill::ContainerState::Stopped {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let mut recovered = Node::start(&config_path, &log).await;
        wait_job(&recovered.client, "stopped", 0).await;
        assert_eq!(
            recovered.client.status().await.unwrap()[0].exit_code,
            Some(0)
        );
        recovered.client.stop("work", "default").await.unwrap();
        recovered.crash().await;
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
    let mut config = Config::parse("[job.work]\nimage = 'proc-grill:image-ignored'\n").unwrap();
    config.job.get_mut("work").unwrap().command = Some(vec![
        "/bin/sh".into(),
        "-c".into(),
        format!(
            "n=0; while [ ! -f '{}' ] && [ $n -lt 2400 ]; do sleep 0.05; n=$((n+1)); done",
            release.display()
        ),
    ]);
    let mut node = Node::start(&config_path, &log).await;
    node.client.apply(&config).await.unwrap();
    wait_job(&node.client, "running", 0).await;
    let main_pid = node.client.status().await.unwrap()[0].pid.unwrap();
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
    wait_job(&recovered.client, "running", 0).await;
    assert_eq!(
        recovered.client.status().await.unwrap()[0].pid,
        Some(main_pid)
    );
    std::fs::write(release, "release").unwrap();
    wait_job(&recovered.client, "stopped", 0).await;
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
    let mut config =
        Config::parse("[app.init-crash]\nimage = 'proc-grill:image-ignored'\n").unwrap();
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
        "/bin/sh".into(),
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
    assert_eq!(retired.unwrap(), ContainerState::Stopped);
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
    std::fs::write(&config, "[job.keep]\nimage = 'proc-grill:image-ignored'\ncommand = ['/bin/true']\nschedule = '0 0 30 2 *'\n[job.remove]\nimage = 'proc-grill:image-ignored'\ncommand = ['/bin/true']\nschedule = '0 0 30 2 *'\n").unwrap();
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
    assert!(
        !run_relish(&["--endpoint", &endpoint, "stop", "remove"])
            .status
            .success(),
        "retired schedule returned after Bun was killed"
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

#[tokio::test]
async fn node_job_lease_reaps_a_surviving_process_after_bun_is_killed() {
    let root = tempfile::tempdir().unwrap();
    let cluster_dir = root.path().join("cluster");
    assert_success(
        &run_relish(&[
            "init",
            cluster_dir.to_str().unwrap(),
            "--cluster-name",
            "token-lease",
            "--node-id",
            "node-01",
        ]),
        "initialise scoped-token fixture",
    );
    let node_path = cluster_dir.join("reliaburger.toml");
    let mut node = reliaburger::config::NodeConfig::from_file(&node_path).unwrap();
    node.node.name = Some("node-01".into());
    node.network.advertise_address = Some("127.0.0.1".into());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;
    node.process_workloads.allowed_binaries =
        vec!["/bin/sh".into(), "/bin/sleep".into(), "/bin/true".into()];
    node.testing.safety_class = reliaburger::testkit::safety::ClusterSafetyClass::Development;
    node.testing
        .allowed_operations
        .insert(reliaburger::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads);
    let (mut bun, address) = spawn_bun_with_port_retry(true, || {
        let [gossip, raft, reporting] = reserve_ports();
        node.cluster.gossip_port = gossip;
        node.cluster.raft_port = raft;
        node.cluster.reporting_port = reporting;
        std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();
        (
            node_path.clone(),
            reserve_address(),
            root.path().join("token-lease-bun.log"),
        )
    });
    let endpoint = format!("https://{address}");
    let ca = cluster_dir.join("identity/root-ca.crt");
    let ca = ca.to_str().unwrap();
    wait_for_relish(
        &mut bun,
        &["--endpoint", &endpoint, "--ca-cert", ca, "status"],
    );
    let token = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "token",
        "create",
        "--name",
        "test-admin",
        "--role",
        "admin",
    ]);
    assert_success(&token, "create catalogue admin");
    let token = String::from_utf8(token.stdout).unwrap();
    let token = token.trim();
    let deadline = Instant::now() + WAIT;
    // The auth-store refresh is asynchronous; wait until anonymous management
    // is refused, so the probe cannot accidentally run in bootstrap mode.
    loop {
        let anonymous = run_relish(&["--endpoint", &endpoint, "--ca-cert", ca, "token", "list"]);
        if !anonymous.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "auth store did not adopt the admin token"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let ca_bytes = std::fs::read(ca).unwrap();
    let client =
        reliaburger::relish::client::BunClient::new_with_ca(&endpoint, Some(token), &ca_bytes)
            .unwrap();
    let lease = client.create_node_job_lease(60).await.unwrap();
    let manifest = reliaburger::config::Config::parse(&format!(
        "[job.survivor]\nimage = 'proc-grill:image-ignored'\ncommand = ['/bin/sleep', '45']\nnamespace = '{}'\n[job.cron]\nimage = 'proc-grill:image-ignored'\ncommand = ['/bin/true']\nschedule = '* * * * *'\nnamespace = '{}'\n",
        lease.namespace, lease.namespace,
    )).unwrap();
    client
        .apply_with_lease(&manifest, &lease.lease_id)
        .await
        .unwrap();
    let records_dir = node.storage.data.join("instances");
    let record = reliaburger::grill::records::load_records(&records_dir)
        .unwrap()
        .into_iter()
        .find(|record| record.app_name == "survivor" && record.namespace == lease.namespace)
        .unwrap();
    assert!(record.is_job);
    assert!(reliaburger::grill::records::is_live(&record));
    client.renew_test_lease(&lease.lease_id, 3).await.unwrap();
    bun.child.kill().unwrap();
    bun.child.wait().unwrap();
    assert!(
        reliaburger::grill::records::is_live(&record),
        "job did not survive Bun's crash"
    );
    let lease_path = node.storage.data.join("node-test-leases.json");
    let persisted = reliaburger::testkit::lease::LocalLeaseStore::open(lease_path.clone())
        .await
        .unwrap();
    assert_eq!(
        persisted
            .get(&lease.lease_id)
            .await
            .unwrap()
            .resources
            .len(),
        2
    );
    drop(persisted);
    tokio::time::sleep(Duration::from_millis(3100)).await;
    // The dead Bun's four ports stay free for the lease to expire and for the
    // restart to reach its Raft bind (several seconds), long enough for a
    // concurrent test to reserve one of them. Restart on the same ports, and
    // only if that loses the race move the node to freshly reserved ones.
    let mut first_attempt = true;
    let (mut restarted, address) = spawn_bun_with_port_retry(true, || {
        let api = if std::mem::take(&mut first_attempt) {
            address
        } else {
            let [gossip, raft, reporting] = reserve_ports();
            node.cluster.gossip_port = gossip;
            node.cluster.raft_port = raft;
            node.cluster.reporting_port = reporting;
            std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();
            reserve_address()
        };
        (
            node_path.clone(),
            api,
            root.path().join("job-restarted.log"),
        )
    });
    let client = reliaburger::relish::client::BunClient::new_with_ca(
        &format!("https://{address}"),
        Some(token),
        &ca_bytes,
    )
    .unwrap();
    let recovered = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            restarted.assert_running();
            let leases = reliaburger::testkit::lease::LocalLeaseStore::open(lease_path.clone())
                .await
                .unwrap();
            if leases.get(&lease.lease_id).await.is_none() {
                assert!(
                    !reliaburger::grill::records::is_live(&record),
                    "cleanup acknowledged a live process"
                );
                assert!(
                    reliaburger::grill::records::load_records(&records_dir)
                        .unwrap()
                        .iter()
                        .all(|record| record.namespace != lease.namespace)
                );
                let instances = client.status().await.unwrap();
                assert!(
                    instances
                        .iter()
                        .all(|instance| instance.namespace != lease.namespace)
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if recovered.is_err() {
        let leases = reliaburger::testkit::lease::LocalLeaseStore::open(lease_path)
            .await
            .unwrap();
        let lease_state = leases.get(&lease.lease_id).await.map(|lease| lease.state);
        let process =
            reliaburger::grill::records::poll_adopted_process(record.pid, record.pid_started_at);
        let log = std::fs::read_to_string(root.path().join("job-restarted.log")).unwrap();
        let recovery_lines = log
            .lines()
            .filter(|line| {
                ["lease", "retir", "adopt", "ownership", "failed", "error"]
                    .iter()
                    .any(|word| line.contains(word))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let jobs: serde_json::Value = serde_json::from_slice(
            &std::fs::read(records_dir.join("job-attempts.checkpoint")).unwrap(),
        )
        .unwrap();
        let attempts: Vec<_> = jobs["jobs"].as_array().unwrap().iter().map(|job| serde_json::json!({"name": job["name"], "phase": job["phase"], "runtime_absent": job["runtime_absent"]})).collect();
        panic!(
            "expired job lease did not recover: state={lease_state:?}, process={process:?}, attempts={attempts:?}\n{recovery_lines}"
        );
    }
}
