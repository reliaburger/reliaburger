//! Matched warm-image lower-bound measurements. No fake runner or rate extrapolation.
//! Rootful Linux only; run beside the same public-cluster service as measure-jobs.py.
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("job-throughput requires provisioned rootful Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use clap::{Parser, ValueEnum};
    use reliaburger::{
        bun::{
            execution_budget::ExecutionBudget,
            task_array_node::{
                ArrayAssignment, ControlVersion, HeldChunk, NodeRunner, NodeSyncRequest,
                TaskArrayNode, TaskArrayNodeConfig,
            },
            task_executor::{TaskInvocation, TaskRunner},
            task_runtime::OwnedRunner,
        },
        config::{job::JobSpec, process_workloads::ProcessWorkloadsConfig},
        grill::{AnyGrill, ContainerState, Grill, ImageStore, runc::RuncGrill},
        meat::{
            Resources,
            task_array::{ChunkId, TaskArraySpec},
        },
    };
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    use tokio_util::sync::CancellationToken;

    #[derive(Clone, Copy, Debug, ValueEnum)]
    enum ExecutionPath {
        Bare,
        Fresh,
        Reused,
        DurableFresh,
        DurableReused,
    }
    #[derive(Parser)]
    struct Options {
        #[arg(long, value_enum)]
        path: ExecutionPath,
        /// Fresh task-owned directory, with traversable ancestors for the user namespace.
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        bun: PathBuf,
        /// Digest-pinned image, also used by the full-dispatch manifest.
        #[arg(long)]
        image: String,
        #[arg(long, default_value_t = 10000)]
        count: u32,
        #[arg(long, default_value_t = 27)]
        concurrency: u32,
        #[arg(long)]
        service_url: String,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    }

    pub async fn main() -> anyhow::Result<()> {
        let options = Options::parse();
        anyhow::ensure!(nix::unistd::geteuid().is_root(), "requires rootful Linux");
        anyhow::ensure!(
            options.count > 0 && (1..=27).contains(&options.concurrency),
            "positive count; concurrency 1–27 for the disclosed 3000m budget"
        );
        anyhow::ensure!(
            options.image.contains("@sha256:"),
            "image must be digest pinned"
        );
        std::fs::create_dir(&options.root)?;
        let images = ImageStore::new(options.root.join("images"))
            .with_owner_shift(reliaburger::grill::userns::HOST_ID_BASE)
            .with_mirrors(reliaburger::testkit::pinned_images::local_test_mirrors()?);
        let preparation = Instant::now();
        let pulled = images.pull_and_unpack(&options.image).await?;
        let preparation_seconds = preparation.elapsed().as_secs_f64();
        let executable = pulled.rootfs.join("bin/busybox");
        let executable_sha256 = hex::encode(Sha256::digest(std::fs::read(&executable)?));
        let runtime = RuncGrill::new(
            options.root.join("bundles"),
            images,
            false,
            options.root.join("state"),
            options.bun.clone(),
        )?;
        let capacity = Resources::new(3000, 4 << 30, 0);
        let budget = ExecutionBudget::new(capacity);
        let reused = matches!(
            options.path,
            ExecutionPath::Reused | ExecutionPath::DurableReused
        );
        let durable = matches!(
            options.path,
            ExecutionPath::DurableFresh | ExecutionPath::DurableReused
        );
        let template: JobSpec = toml::from_str(&format!(
            "image='{}'\nnamespace='default'\nruntime='{}'\ncpu='100m-1000m'\nmemory='32Mi'",
            options.image,
            if reused { "shared-runc" } else { "runc" }
        ))?;
        let runner = Arc::new(
            OwnedRunner::for_data_dir(AnyGrill::Runc(runtime.clone()), &options.root)?
                .with_budget(budget.clone()),
        );
        let cancel = CancellationToken::new();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?;
        let service_url = options.service_url.clone();
        let probe_cancel = cancel.clone();
        let probes = tokio::spawn(async move {
            let mut rows = Vec::new();
            loop {
                let before = Instant::now();
                let response = http.get(&service_url).send().await?;
                anyhow::ensure!(response.status() == 200, "concurrent service unavailable");
                response.bytes().await?;
                rows.push(before.elapsed().as_secs_f64() * 1000.0);
                tokio::select! { () = probe_cancel.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(1)) => {} }
                anyhow::ensure!(rows.len() <= 3600, "probe storage bound exceeded");
            }
            Ok::<_, anyhow::Error>(rows)
        });
        let started = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(options.timeout), async {
            if durable {
                let mut config = TaskArrayNodeConfig::for_data_dir(
                    &options.root,
                    ProcessWorkloadsConfig::default(),
                );
                config.default_concurrency = options.concurrency;
                let node = TaskArrayNode::new(
                    config,
                    NodeRunner::Owned(Box::new(OwnedRunner::for_data_dir(
                        AnyGrill::Runc(runtime.clone()),
                        &options.root,
                    )?)),
                )
                .with_budget(budget.clone());
                let mut spec = TaskArraySpec::with_count(options.count);
                spec.chunk_size = 1000;
                spec.per_node_concurrency = Some(options.concurrency);
                spec.max_attempts = 1;
                let request = NodeSyncRequest {
                    version: ControlVersion {
                        index: 1,
                        ..Default::default()
                    },
                    known: vec![1],
                    arrays: vec![ArrayAssignment {
                        template: Some(Box::new(template.clone())),
                        resources: Resources::new(100, 32 << 20, 0),
                        batch_id: 1,
                        spec: spec.clone(),
                        program: "/unused".into(),
                        args: vec!["/bin/busybox".into(), "true".into()],
                        env: vec![],
                        held: (0..spec.chunk_count())
                            .map(|index| HeldChunk {
                                chunk: ChunkId(index),
                                attempt: 1,
                            })
                            .collect(),
                        stopping: false,
                        replay_unknown: true,
                    }],
                };
                loop {
                    let progress = node.sync(&request).await.arrays.remove(0);
                    if let Some(reason) = progress.refused {
                        anyhow::bail!("worker refused: {reason}");
                    }
                    if progress.finished.len() == spec.chunk_count() as usize {
                        let successes: u64 = progress
                            .finished
                            .iter()
                            .map(|row| u64::from(row.succeeded))
                            .sum();
                        let failures: u64 = progress
                            .finished
                            .iter()
                            .map(|row| u64::from(row.failed_count))
                            .sum();
                        return Ok((successes, failures));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            let mut pending = tokio::task::JoinSet::new();
            let mut next = 0;
            let mut successes = 0;
            let mut failures = 0;
            while next < options.count || !pending.is_empty() {
                while next < options.count && pending.len() < options.concurrency as usize {
                    let index = next;
                    next += 1;
                    let runner = runner.clone();
                    let template = template.clone();
                    let executable = executable.clone();
                    let cancel = cancel.clone();
                    let bare = matches!(options.path, ExecutionPath::Bare);
                    pending.spawn(async move {
                        if bare {
                            use std::os::unix::process::CommandExt;
                            let mut command = tokio::process::Command::new(executable);
                            command.as_std_mut().arg0("/bin/busybox");
                            let output = command
                                .arg("true")
                                .env_clear()
                                .kill_on_drop(true)
                                .output()
                                .await?;
                            Ok::<_, anyhow::Error>(output.status.success())
                        } else {
                            let task = TaskInvocation {
                                template: Some(Box::new(template)),
                                index,
                                attempt: 1,
                                program: "/unused".into(),
                                args: vec!["/bin/busybox".into(), "true".into()],
                                env: vec![
                                    ("RELIABURGER_BATCH_ID".into(), "1".into()),
                                    ("RELIABURGER_TASK_COUNT".into(), options.count.to_string()),
                                ],
                            };
                            let result = runner.run(&task, Duration::from_secs(30), &cancel).await;
                            Ok(result.outcome.succeeded())
                        }
                    });
                }
                if pending
                    .join_next()
                    .await
                    .transpose()?
                    .transpose()?
                    .unwrap_or(false)
                {
                    successes += 1;
                } else {
                    failures += 1;
                }
            }
            Ok::<_, anyhow::Error>((successes, failures))
        })
        .await;
        let elapsed_seconds = started.elapsed().as_secs_f64();
        cancel.cancel();
        let probe_result = probes.await;
        // Retire only this fresh fixture's original owners, including on failure.
        for entry in runtime.launch_inventory().await?.unwrap_or_default() {
            runtime.kill(&entry.instance_id).await?;
            tokio::time::timeout(Duration::from_secs(30), async {
                while runtime.state(&entry.instance_id).await? != ContainerState::Stopped {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while budget.available() != capacity {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        let probe_rows = probe_result??;
        let (successes, failures) = outcome??;
        let report = json!({
            "path": format!("{:?}", options.path), "image": options.image, "executable_sha256": executable_sha256,
            "command": ["/bin/busybox", "true"], "count": options.count, "concurrency": options.concurrency,
            "image_preparation_seconds_excluded": preparation_seconds, "warmth": "warm-images-cold-executors",
            "elapsed_seconds": elapsed_seconds, "verified_successes": successes, "failures": failures,
            "retries": 0, "verified_successes_per_second": successes as f64 / elapsed_seconds,
            "container_cpu_request_millicores": 100, "container_cpu_limit_millicores": 1000, "container_memory_bytes": 32 << 20,
            "bare_process_resource_limits": "none; container profile fields are comparison inputs only",
            "node_job_budget_cpu_millicores": 3000, "service_probe_latency_ms": probe_rows,
            "durability": if durable { "worker grant fence, per-task ledger/index, verified chunk receipts; no Raft admission" } else if matches!(options.path, ExecutionPath::Bare) { "exit statuses only; no isolation, limits, ownership journal or task ledger" } else { "owned runtime journal, command exit and positive cleanup; no task ledger or Raft admission" },
            "optimised_build": !cfg!(debug_assertions),
            "namespace_policy_hook": "not injected in direct runner; full Bun dispatch includes live namespace supervision",
            "qualified_100m_per_day": false,
        });
        std::fs::write(
            options.root.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        anyhow::ensure!(
            successes == u64::from(options.count) && failures == 0,
            "not all commands succeeded"
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    linux::main().await
}
