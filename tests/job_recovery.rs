//! Real Bun death preserves job outcomes and retires interrupted initialisers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use reliaburger::config::{Config, types::EnvValue};
use reliaburger::relish::client::BunClient;

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
            "n=0; while [ ! -f '{}' ] && [ $n -lt 600 ]; do sleep 0.05; n=$((n+1)); done",
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
    let request = tokio::spawn(async move { client.exec("work", "default", &command).await });
    let exec_pid: u32 = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(value) = std::fs::read_to_string(&marker)
                && let Ok(pid) = value.trim().parse()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    node.crash().await;
    tokio::time::timeout(Duration::from_secs(5), async {
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
