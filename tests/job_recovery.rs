//! Real Bun process death must preserve a job's budget and unknown outcome.

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
        let (client, endpoint) = tokio::time::timeout(Duration::from_secs(20), async {
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

async fn wait_job(client: &BunClient, expected_state: &str, restarts: u32) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let jobs = client.jobs().await.unwrap();
            if jobs.iter().any(|job| {
                job.name == "work" && job.state == expected_state && job.restart_count == restarts
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("job must become {expected_state} with {restarts} retries"));
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
    let _release = ReleaseJob(release.clone());
    let mut config = Config::parse("[job.work]\nimage = 'proc-grill:image-ignored'\n").unwrap();
    let job = config.job.get_mut("work").unwrap();
    job.command = Some(vec!["/bin/sh".into(), "-c".into(),
        "printf 'run\\n' >> \"$RUN_FILE\"; if [ \"$(wc -l < \"$RUN_FILE\")\" -eq 1 ]; then exit 1; fi; i=0; while [ $i -lt 600 ]; do [ -f \"$RELEASE_FILE\" ] && exit 0; i=$((i+1)); sleep 0.05; done; exit 1".into()]);
    job.env.insert(
        "RUN_FILE".into(),
        EnvValue::Plain(count.display().to_string()),
    );
    job.env.insert(
        "RELEASE_FILE".into(),
        EnvValue::Plain(release.display().to_string()),
    );
    let manifest = root.path().join("job.toml");
    std::fs::write(&manifest, toml::to_string(&config).unwrap()).unwrap();

    let mut node = Node::start(&config_path, &log).await;
    let client = &node.client;
    client.apply(&config).await.unwrap();
    wait_job(client, "running", 1).await;
    let pid = client.status().await.unwrap()[0].pid.unwrap();
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
