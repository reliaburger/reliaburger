//! Matched executable, concurrency and resource measurements beside a live service.
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
        config::{
            job::{JobRuntime, JobSpec},
            process_workloads::ProcessWorkloadsConfig,
        },
        grill::{AnyGrill, ContainerState, Grill, ImageStore, ProcessGrill, runc::RuncGrill},
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
        BareLimited,
        Fresh,
        Reused,
        Host,
        DurableFresh,
        DurableReused,
        DurableHost,
    }
    #[derive(Clone, Copy, Debug, ValueEnum)]
    enum Workload {
        True,
        Sleep,
        Cpu,
        Output,
    }
    #[derive(Parser)]
    struct Options {
        #[arg(long, value_enum)]
        path: ExecutionPath,
        /// Fresh fixture directory with traversable ancestors.
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        bun: PathBuf,
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
        /// Unmeasured commands through the SAME runner, before the measured phase.
        #[arg(long, default_value_t = 0)]
        warmup_count: u32,
        #[arg(long, value_enum, default_value_t = Workload::True)]
        workload: Workload,
        #[arg(long, default_value_t = 100)]
        cpu_request_millicores: u64,
        #[arg(long, default_value_t = 1000)]
        cpu_limit_millicores: u64,
        #[arg(long, default_value_t = 33554432)]
        memory_bytes: u64,
    }
    impl Options {
        fn command(&self) -> Vec<String> {
            match self.workload {
                Workload::True => vec!["/bin/busybox".into(), "true".into()],
                Workload::Sleep => vec!["/bin/busybox".into(), "sleep".into(), "0.01".into()],
                Workload::Cpu => vec!["/bin/busybox".into(), "sh".into(), "-c".into(), "i=0; while [ $i -lt 10000 ]; do i=$((i+1)); done".into()],
                Workload::Output => vec!["/bin/busybox".into(), "sh".into(), "-c".into(), "printf 'out:%s\\n' \"$RELIABURGER_TASK_INDEX\"; printf 'err:%s\\n' \"$RELIABURGER_TASK_INDEX\" >&2".into()],
            }
        }
    }
    struct Measurement<'a> {
        options: &'a Options,
        template: &'a JobSpec,
        executable: &'a PathBuf,
        runner: &'a Arc<OwnedRunner<AnyGrill>>,
        node: Option<&'a TaskArrayNode>,
        limited: &'a [PathBuf],
        cancel: &'a CancellationToken,
    }
    impl Measurement<'_> {
        async fn run(&self, count: u32, batch: u64) -> anyhow::Result<(u64, u64)> {
            if let Some(node) = self.node {
                let mut spec = TaskArraySpec::with_count(count);
                spec.chunk_size = 1000;
                spec.per_node_concurrency = Some(self.options.concurrency);
                spec.max_attempts = 1;
                let host = self.template.runtime == JobRuntime::Process;
                let args = self.options.command();
                let request = NodeSyncRequest {
                    version: ControlVersion {
                        index: batch,
                        ..Default::default()
                    },
                    known: vec![batch],
                    arrays: vec![ArrayAssignment {
                        template: Some(Box::new(self.template.clone())),
                        resources: Resources::new(
                            self.options.cpu_request_millicores,
                            self.options.memory_bytes,
                            0,
                        ),
                        batch_id: batch,
                        spec: spec.clone(),
                        program: if host {
                            self.executable.clone()
                        } else {
                            "/unused".into()
                        },
                        args: if host { args[1..].to_vec() } else { args },
                        env: vec![],
                        held: (0..spec.chunk_count())
                            .map(|index| HeldChunk {
                                chunk: ChunkId(index),
                                attempt: 1,
                            })
                            .collect(),
                        stopping: false,
                        replay_unknown: false,
                    }],
                };
                loop {
                    let progress = node.sync(&request).await.arrays.remove(0);
                    if let Some(reason) = progress.refused {
                        anyhow::bail!("worker refused: {reason}");
                    }
                    if progress.finished.len() == spec.chunk_count() as usize {
                        return Ok((
                            progress
                                .finished
                                .iter()
                                .map(|row| u64::from(row.succeeded))
                                .sum(),
                            progress
                                .finished
                                .iter()
                                .map(|row| u64::from(row.failed_count))
                                .sum(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            let mut pending = tokio::task::JoinSet::new();
            let mut available: Vec<u32> = (0..self.options.concurrency).collect();
            let (mut next, mut successes, mut failures) = (0, 0, 0);
            while next < count || !pending.is_empty() {
                while next < count && !available.is_empty() {
                    let Some(slot) = available.pop() else { break };
                    let index = next;
                    next += 1;
                    let runner = self.runner.clone();
                    let template = self.template.clone();
                    let executable = self.executable.clone();
                    let cancel = self.cancel.clone();
                    let args = self.options.command();
                    let bare = matches!(
                        self.options.path,
                        ExecutionPath::Bare | ExecutionPath::BareLimited
                    );
                    let placement = self.limited.get(slot as usize).cloned();
                    pending.spawn(async move {
                        let env: Vec<(String, String)> =
                            reliaburger::meat::task_array::task_env(batch, count, index, 1)
                                .into_iter()
                                .map(|(name, value)| (name.into(), value))
                                .collect();
                        let succeeded = if bare {
                            use std::os::unix::process::CommandExt;
                            let mut command = tokio::process::Command::new(executable);
                            command.as_std_mut().arg0("/bin/busybox");
                            command
                                .args(&args[1..])
                                .current_dir("/")
                                .env_clear()
                                .envs(env.iter().map(|(name, value)| (name, value)))
                                .kill_on_drop(true);
                            if let Some(path) = placement {
                                use std::os::fd::AsRawFd;
                                let file = std::fs::OpenOptions::new()
                                    .write(true)
                                    .open(path.join("cgroup.procs"))?;
                                // SAFETY: pre_exec performs only an async-signal-safe write
                                // through a pre-opened descriptor before executing user code.
                                unsafe {
                                    command.pre_exec(move || {
                                        if nix::libc::write(
                                            file.as_raw_fd(),
                                            b"0\n".as_ptr().cast(),
                                            2,
                                        ) != 2
                                        {
                                            return Err(std::io::Error::last_os_error());
                                        }
                                        Ok(())
                                    });
                                }
                            }
                            command.output().await?.status.success()
                        } else {
                            let host = template.runtime == JobRuntime::Process;
                            let task = TaskInvocation {
                                template: Some(Box::new(template)),
                                index,
                                attempt: 1,
                                program: executable,
                                args: if host { args[1..].to_vec() } else { args },
                                env,
                            };
                            runner
                                .run(&task, Duration::from_secs(30), &cancel)
                                .await
                                .outcome
                                .succeeded()
                        };
                        Ok::<_, anyhow::Error>((slot, succeeded))
                    });
                }
                if let Some(result) = pending.join_next().await {
                    let (slot, succeeded) = result??;
                    available.push(slot);
                    if succeeded {
                        successes += 1;
                    } else {
                        failures += 1;
                    }
                }
            }
            Ok((successes, failures))
        }
    }
    fn limited_groups(options: &Options) -> anyhow::Result<Vec<PathBuf>> {
        if !matches!(options.path, ExecutionPath::BareLimited) {
            return Ok(vec![]);
        }
        let name = format!("raw-{:032x}", rand::random::<u128>());
        let base = PathBuf::from("/sys/fs/cgroup/reliaburger/default").join(name);
        for directory in [
            PathBuf::from("/sys/fs/cgroup/reliaburger"),
            base.parent().unwrap().into(),
            base.clone(),
        ] {
            std::fs::create_dir_all(&directory)?;
            std::fs::write(
                directory.join("cgroup.subtree_control"),
                "+cpu +memory +pids",
            )?;
        }
        let mut groups = vec![];
        for slot in 0..options.concurrency {
            let slot_directory = base.join(slot.to_string());
            std::fs::create_dir(&slot_directory)?;
            std::fs::write(
                slot_directory.join("cgroup.subtree_control"),
                "+cpu +memory +pids",
            )?;
            let path = slot_directory.join("task");
            std::fs::create_dir(&path)?;
            for (file, value) in [
                (
                    "cpu.max",
                    reliaburger::grill::cpu_max_from_millicores(options.cpu_limit_millicores),
                ),
                (
                    "cpu.weight",
                    reliaburger::grill::cgroup::cpu_weight_from_millicores(
                        options.cpu_request_millicores,
                    )
                    .to_string(),
                ),
                ("memory.max", options.memory_bytes.to_string()),
                ("memory.high", options.memory_bytes.to_string()),
                ("memory.swap.max", "0".into()),
                ("memory.oom.group", "1".into()),
                ("pids.max", "256".into()),
            ] {
                std::fs::write(path.join(file), value)?;
            }
            groups.push(path);
        }
        Ok(groups)
    }
    pub async fn main() -> anyhow::Result<()> {
        let options = Options::parse();
        anyhow::ensure!(nix::unistd::geteuid().is_root(), "requires rootful Linux");
        anyhow::ensure!(
            options.count > 0 && (1..=27).contains(&options.concurrency),
            "positive count; concurrency 1–27"
        );
        anyhow::ensure!(
            options.cpu_request_millicores > 0
                && options.cpu_limit_millicores >= options.cpu_request_millicores
                && options.cpu_limit_millicores >= 10
                && options.cpu_limit_millicores <= 256000
                && options.memory_bytes > 0
                && options.memory_bytes <= 4 << 30,
            "invalid resource profile"
        );
        anyhow::ensure!(
            (options.cpu_request_millicores + 10) * u64::from(options.concurrency) <= 3000,
            "requested concurrency cannot fit the declared CPU budget, including helpers"
        );
        anyhow::ensure!(
            (options.memory_bytes + (8 << 20)) * u64::from(options.concurrency) <= 4 << 30,
            "requested concurrency cannot fit memory budget"
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
        let executable = pulled.rootfs.join("bin/busybox");
        let executable_sha256 = hex::encode(Sha256::digest(std::fs::read(&executable)?));
        let preparation_seconds = preparation.elapsed().as_secs_f64();
        let host = matches!(
            options.path,
            ExecutionPath::Host | ExecutionPath::DurableHost
        );
        let reused = matches!(
            options.path,
            ExecutionPath::Reused | ExecutionPath::DurableReused
        );
        let durable = matches!(
            options.path,
            ExecutionPath::DurableFresh | ExecutionPath::DurableReused | ExecutionPath::DurableHost
        );
        let mode = if host {
            JobRuntime::Process
        } else if reused {
            JobRuntime::SharedRunc
        } else {
            JobRuntime::Runc
        };
        let runtime = if host {
            AnyGrill::Process(ProcessGrill::with_owner(
                options.root.join("state"),
                options.bun.clone(),
            ))
        } else {
            AnyGrill::Runc(RuncGrill::new(
                options.root.join("bundles"),
                images,
                false,
                options.root.join("state"),
                options.bun.clone(),
            )?)
        };
        let template: JobSpec = toml::from_str(&format!(
            "{}\nnamespace='default'\nruntime='{}'\ncpu='{}m-{}m'\nmemory='{}'",
            if host {
                format!("exec='{}'", executable.display())
            } else {
                format!("image='{}'", options.image)
            },
            if host {
                "process"
            } else if reused {
                "shared-runc"
            } else {
                "runc"
            },
            options.cpu_request_millicores,
            options.cpu_limit_millicores,
            options.memory_bytes
        ))?;
        let capacity = Resources::new(3000, 4 << 30, 0);
        let budget = ExecutionBudget::new(capacity);
        let runner = Arc::new(
            OwnedRunner::for_data_dir(runtime.clone(), &options.root)?.with_budget(budget.clone()),
        );
        let node = if durable {
            let policy = ProcessWorkloadsConfig {
                mount_isolation: false,
                allowed_binaries: vec![executable.clone()],
                ..Default::default()
            };
            let mut config = TaskArrayNodeConfig::for_data_dir(&options.root, policy);
            config.default_concurrency = options.concurrency;
            Some(
                TaskArrayNode::new(
                    config,
                    NodeRunner::Owned(Box::new(OwnedRunner::for_data_dir(
                        runtime.clone(),
                        &options.root,
                    )?)),
                )
                .with_budget(budget.clone()),
            )
        } else {
            None
        };
        let limited = limited_groups(&options)?;
        let cancel = CancellationToken::new();
        let measurement = Measurement {
            options: &options,
            template: &template,
            executable: &executable,
            runner: &runner,
            node: node.as_ref(),
            limited: &limited,
            cancel: &cancel,
        };
        let before_warmup = Instant::now();
        if options.warmup_count > 0 {
            let (successes, failures) = tokio::time::timeout(
                Duration::from_secs(options.timeout),
                measurement.run(options.warmup_count, 1),
            )
            .await??;
            anyhow::ensure!(
                successes == u64::from(options.warmup_count) && failures == 0,
                "warmup failed"
            );
        }
        let warmup_seconds = before_warmup.elapsed().as_secs_f64();
        let timing = || match &node {
            Some(node) => node.executor_timings(mode),
            None => runner.executor_timings(mode),
        };
        let timings_before = timing();
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
                tokio::select! { () = probe_cancel.cancelled() => break, () = tokio::time::sleep(Duration::from_secs(1)) => {} }
                anyhow::ensure!(rows.len() <= 3600, "probe storage bound exceeded");
            }
            Ok::<_, anyhow::Error>(rows)
        });
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(options.timeout),
            measurement.run(options.count, 2),
        )
        .await;
        let elapsed_seconds = started.elapsed().as_secs_f64();
        let timings_after = timing();
        cancel.cancel();
        let probe_result = probes.await;
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
        for path in &limited {
            anyhow::ensure!(
                std::fs::read_to_string(path.join("cgroup.procs"))?
                    .trim()
                    .is_empty(),
                "raw cgroup still owns a process"
            );
            std::fs::remove_dir(path)?;
            std::fs::remove_dir(path.parent().unwrap())?;
        }
        if let Some(path) = limited
            .first()
            .and_then(|path| path.parent())
            .and_then(|path| path.parent())
        {
            std::fs::remove_dir(path)?;
        }
        let probe_rows = probe_result??;
        let (successes, failures) = outcome??;
        let report = json!({
            "path": format!("{:?}", options.path), "image": options.image, "executable_sha256": executable_sha256,
            "command": options.command(), "count": options.count, "concurrency": options.concurrency,
            "image_preparation_seconds_excluded": preparation_seconds, "warmth": if options.warmup_count == 0 { "warm-images-cold-executors" } else { "warm-images-after-disclosed-warmup" },
            "warmup_count": options.warmup_count, "warmup_seconds_excluded": warmup_seconds,
            "executor_timings_before": timings_before, "executor_timings_after": timings_after,
            "elapsed_seconds": elapsed_seconds, "verified_successes": successes, "failures": failures, "retries": 0,
            "verified_successes_per_second": successes as f64 / elapsed_seconds,
            "container_cpu_request_millicores": options.cpu_request_millicores, "container_cpu_limit_millicores": options.cpu_limit_millicores, "container_memory_bytes": options.memory_bytes,
            "resource_enforcement": match options.path {
                ExecutionPath::Bare => "none; exit-status-only raw process floor",
                ExecutionPath::Fresh | ExecutionPath::DurableFresh => "per-command CPU and memory limits before user code; zero swap; existing OCI PID policy",
                _ => "per-command CPU, memory, swap and PID limits before user code",
            },
            "bare_process_resource_limits": if matches!(options.path, ExecutionPath::BareLimited) { "matched per-slot cgroups before user exec" } else { "none; profile fields are comparison inputs only" },
            "node_job_budget_cpu_millicores": 3000, "service_probe_latency_ms": probe_rows,
            "durability": if durable { "worker grant fence, per-task ledger/index, verified chunk receipts; no Raft admission" } else if matches!(options.path, ExecutionPath::Bare | ExecutionPath::BareLimited) { "exit statuses only; no durable ownership or ledger" } else { "durable executor/runtime ownership, exit and positive cleanup; no task ledger or Raft admission" },
            "optimised_build": !cfg!(debug_assertions), "qualified_100m_per_day": false,
            "namespace_policy_hook": "not injected in direct runner; full Bun dispatch includes live namespace supervision",
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
