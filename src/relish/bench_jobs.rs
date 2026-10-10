//! Fixed-window job benchmarks through the public job path and a local Linux baseline.

use super::{CommandOutcome, RelishError, client::BunClient, output::OutputFormat};
use crate::{
    bun::task_array_api::TaskManifestRequest,
    config::{
        job::{JobRuntime, JobSpec},
        types::ResourceRange,
    },
    meat::{task_array::TaskArraySpec, task_array_store::ManifestCohort},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Digest-pinned multi-platform BusyBox used by the job scenarios.
pub const DEFAULT_IMAGE: &str = "public.ecr.aws/docker/library/busybox@sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028";

/// Built-in timed job scenarios, selected independently of the ordinary suite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
pub enum JobScenario {
    /// A fresh isolated runc container per task.
    #[value(name = "jobs-containers")]
    #[serde(rename = "jobs-containers")]
    Containers,
    /// Fresh command processes in compatible reusable runc containers.
    #[value(name = "jobs-shared-containers")]
    #[serde(rename = "jobs-shared-containers")]
    SharedContainers,
    /// Trusted allowlisted host processes without container isolation.
    #[value(name = "jobs-host-processes")]
    #[serde(rename = "jobs-host-processes")]
    HostProcesses,
    /// Raw child exits on the Linux machine running Relish, without scheduler guarantees.
    #[value(name = "jobs-vm-baseline")]
    #[serde(rename = "jobs-vm-baseline")]
    VmBaseline,
}
impl JobScenario {
    fn runtime(self) -> Option<JobRuntime> {
        match self {
            Self::Containers => Some(JobRuntime::Runc),
            Self::SharedContainers => Some(JobRuntime::SharedRunc),
            Self::HostProcesses => Some(JobRuntime::Process),
            Self::VmBaseline => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Containers => "jobs-containers",
            Self::SharedContainers => "jobs-shared-containers",
            Self::HostProcesses => "jobs-host-processes",
            Self::VmBaseline => "jobs-vm-baseline",
        }
    }
}

/// Parsed options for a single fixed-window job scenario.
pub struct JobBenchArgs {
    /// Which isolation and measurement boundary to exercise.
    pub scenario: JobScenario,
    /// Measurement duration, excluding positive drain.
    pub seconds: u32,
    /// Per-node cap for public jobs; total local children for the raw baseline.
    pub concurrency: u32,
    /// Public per-command CPU request in millicores, with a one-core limit.
    pub cpu_request: u32,
    /// Pinned BusyBox image for container scenarios.
    pub image: String,
    /// BusyBox override; host defaults to /bin/busybox, raw defaults to the pinned image.
    pub exec: Option<PathBuf>,
    /// Namespace for the benchmark's own submission.
    pub namespace: String,
    /// Final report rendering. Progress is always written to stderr.
    pub output: OutputFormat,
    /// Optional JSON evidence file, created without replacing an existing file.
    pub report: Option<PathBuf>,
}
impl JobBenchArgs {
    fn validate(&self) -> Result<(), RelishError> {
        for (flag, valid, reason) in [
            (
                "seconds",
                (1..=86400).contains(&self.seconds),
                "must be 1–86400",
            ),
            (
                "concurrency",
                (1..=256).contains(&self.concurrency),
                "must be 1–256",
            ),
            (
                "cpu-request",
                (1..=1000).contains(&self.cpu_request),
                "must be 1–1000m",
            ),
            (
                "exec",
                self.exec.as_ref().is_none_or(|path| path.is_absolute()),
                "must be an absolute BusyBox path",
            ),
            (
                "image",
                self.image.contains("@sha256:"),
                "must be digest-pinned",
            ),
            ("namespace", !self.namespace.is_empty(), "must not be empty"),
        ] {
            if !valid {
                return Err(RelishError::InvalidFlag {
                    flag: flag.into(),
                    reason: reason.into(),
                });
            }
        }
        Ok(())
    }
    fn manifest(&self, name: &str) -> Result<TaskManifestRequest, RelishError> {
        self.validate()?;
        let runtime = self
            .scenario
            .runtime()
            .ok_or_else(|| failure("raw baseline has no job submission"))?;
        let mut spec = TaskArraySpec::with_count(if runtime == JobRuntime::Runc {
            60000
        } else {
            16000000
        });
        spec.chunk_size = if runtime == JobRuntime::Runc { 1 } else { 1000 };
        spec.per_node_concurrency = Some(self.concurrency);
        let template = JobSpec {
            runtime,
            image: (runtime != JobRuntime::Process).then(|| self.image.clone()),
            exec: (runtime == JobRuntime::Process)
                .then(|| self.exec.clone().unwrap_or_else(|| "/bin/busybox".into())),
            command: Some(if runtime == JobRuntime::Process {
                vec!["true".into()]
            } else {
                vec!["/bin/busybox".into(), "true".into()]
            }),
            cpu: Some(ResourceRange {
                request: u64::from(self.cpu_request),
                limit: 1000,
            }),
            memory: Some(ResourceRange {
                request: 32 << 20,
                limit: 32 << 20,
            }),
            namespace: Some(self.namespace.clone()),
            schedule: None,
            run_before: vec![],
            env: Default::default(),
            script: None,
            max_attempts: None,
            task_timeout_secs: None,
            overlap: None,
            replay_unknown: false,
        };
        Ok(TaskManifestRequest {
            name: name.into(),
            namespace: self.namespace.clone(),
            cohort: vec![ManifestCohort {
                name: "commands".into(),
                spec,
                template,
            }],
        })
    }
}

/// Positive observed settlement of one benchmark-owned submission.
#[derive(Debug, Serialize, Deserialize)]
pub struct DrainProof {
    /// Identity returned by the benchmark's own submission.
    pub batch_id: u64,
    /// Terminal status observed from the public API.
    pub done: bool,
    /// Observed outstanding held grants.
    pub held: u64,
    /// Observed verified active commands.
    pub active_commands: u64,
}
impl DrainProof {
    fn observe(summary: &Value, id: u64) -> Option<Self> {
        (summary["batch_id"].as_u64() == Some(id)
            && summary["done"] == true
            && summary["held"].as_u64() == Some(0)
            && summary["active_commands"].as_u64() == Some(0))
        .then_some(Self {
            batch_id: id,
            done: true,
            held: 0,
            active_commands: 0,
        })
    }
}

/// Auditable result of one timed scenario; projections are never daily qualification.
#[derive(Debug, Serialize, Deserialize)]
pub struct JobBenchReport {
    /// Stable job benchmark report format, separate from the ordinary suite.
    pub schema_version: u32,
    /// Selected scenario.
    pub scenario: JobScenario,
    /// UTC time before the measured submission or raw spawn loop.
    pub started_at: String,
    /// Requested measurement window.
    pub requested_seconds: u32,
    /// Configured concurrency, not a claim of continuously active commands.
    pub concurrency: u32,
    /// CPU request enforced on public jobs, absent on raw children.
    pub cpu_request_millicores: Option<u32>,
    /// CPU limit enforced on public jobs, absent on raw children.
    pub cpu_limit_millicores: Option<u32>,
    /// Memory request/limit enforced on public jobs, absent on raw children.
    pub memory_bytes: Option<u64>,
    /// Receipt chunk size; independent of per-command resource admission.
    pub receipt_chunk_size: Option<u32>,
    /// Requested pinned image, absent for host jobs and the raw baseline.
    pub image: Option<String>,
    /// Host/local executable path, absent for container scenarios.
    pub executable: Option<PathBuf>,
    /// Local executable hash; unavailable for a remote host job.
    pub executable_sha256: Option<String>,
    /// Explains where and what is counted, including limits of a comparison.
    pub measurement_boundary: String,
    /// Unique successes accepted by the cluster before the deadline.
    pub unique_accepted_successes: u64,
    /// Successful local raw child exits before the deadline.
    pub verified_successes: u64,
    /// Accepted terminal failures or failed raw children within the window.
    pub terminal_failures: u64,
    /// Retries accepted by the cluster, never counted as extra successes.
    pub accepted_retries: u64,
    /// Raw children drained after the cutoff without earning credit.
    pub post_cutoff_drained: u64,
    /// Stable identities belonging only to this benchmark.
    pub batch_ids: Vec<u64>,
    /// Submissions still active when observation stopped, before cancellation.
    pub active_submissions: Vec<u64>,
    /// Positive API settlement observations, including post-window cancellation.
    pub drain_proofs: Vec<DrainProof>,
    /// Time of the last complete status response credited inside the window.
    pub last_accepted_sample_seconds: Option<f64>,
    /// Unique names aid recovery if a submission response is lost.
    pub submission_names: Vec<String>,
    /// The entire requested window completed without interruption or error.
    pub measurement_complete: bool,
    /// Caller requested graceful interruption.
    pub interrupted: bool,
    /// All known owned work positively drained; unknown submission ownership fails this.
    pub cleanup_verified: bool,
    /// Owned submissions whose cancellation or positive drain failed.
    pub cleanup_failed_submissions: Vec<u64>,
    /// Error diagnostic, if the measurement could not finish.
    pub error: Option<String>,
    /// Measured wall time including submission and positive drain.
    pub elapsed_including_drain_seconds: f64,
    /// Successful outcomes divided by the requested window, never by preparation or drain.
    pub successes_per_second: f64,
    /// Window rate multiplied by 86400, never an observed daily total.
    pub extrapolated_runs_per_day: f64,
    /// Explicit interpretation of the projection.
    pub daily_projection: String,
    /// Always false: a fixed window is not a daily reliability qualification.
    pub qualified_100m_per_day: bool,
}
impl JobBenchReport {
    fn new(args: &JobBenchArgs) -> Self {
        let runtime = args.scenario.runtime();
        let public = runtime.is_some();
        Self {
            schema_version: 1,
            scenario: args.scenario,
            started_at: crate::testkit::runner::now_rfc3339(),
            requested_seconds: args.seconds,
            concurrency: args.concurrency,
            cpu_request_millicores: public.then_some(args.cpu_request),
            cpu_limit_millicores: public.then_some(1000),
            memory_bytes: public.then_some(32 << 20),
            receipt_chunk_size: runtime.map(|r| if r == JobRuntime::Runc { 1 } else { 1000 }),
            image: (matches!(runtime, Some(JobRuntime::Runc | JobRuntime::SharedRunc))
                || runtime.is_none() && args.exec.is_none())
            .then(|| args.image.clone()),
            executable: if runtime == Some(JobRuntime::Process) {
                Some(args.exec.clone().unwrap_or_else(|| "/bin/busybox".into()))
            } else if runtime.is_none() {
                args.exec.clone()
            } else {
                None
            },
            executable_sha256: None,
            measurement_boundary: if public {
                "configured cluster; concurrency is per node; unique accepted outcomes received before the monotonic cutoff; submission and cold executor/image startup are included; 10m/8Mi helper reservation for shared/host; remote host executable hash is unknown; compare on the same node and byte-identical BusyBox".into()
            } else {
                "local Linux machine running Relish; raw child exits, no resource enforcement, ownership journal or accepted task ledger; compare only with jobs on this same machine and byte-identical BusyBox".into()
            },
            unique_accepted_successes: 0,
            verified_successes: 0,
            terminal_failures: 0,
            accepted_retries: 0,
            post_cutoff_drained: 0,
            batch_ids: vec![],
            active_submissions: vec![],
            drain_proofs: vec![],
            last_accepted_sample_seconds: None,
            submission_names: vec![],
            measurement_complete: false,
            interrupted: false,
            cleanup_verified: false,
            cleanup_failed_submissions: vec![],
            error: None,
            elapsed_including_drain_seconds: 0.0,
            successes_per_second: 0.0,
            extrapolated_runs_per_day: 0.0,
            daily_projection: "extrapolation from this window; not an observed daily total".into(),
            qualified_100m_per_day: false,
        }
    }
    fn count(&self) -> u64 {
        self.unique_accepted_successes + self.verified_successes
    }
    fn finalise(&mut self, elapsed: f64) {
        if self.measurement_complete && self.count() == 0 && self.error.is_none() {
            self.error = Some("no successful outcomes were observed inside the measurement window; inspect the submitted job for runtime or admission refusals".into());
        }
        self.elapsed_including_drain_seconds = elapsed;
        self.successes_per_second = self.count() as f64 / f64::from(self.requested_seconds);
        self.extrapolated_runs_per_day = self.successes_per_second * 86400.0;
    }
    fn healthy(&self) -> bool {
        self.measurement_complete
            && self.count() > 0
            && self.terminal_failures == 0
            && self.cleanup_verified
            && self.error.is_none()
    }
}
fn failure(message: impl Into<String>) -> RelishError {
    RelishError::ApiError {
        status: 0,
        body: message.into(),
    }
}
fn within_window(observed: Instant, deadline: Instant) -> bool {
    observed <= deadline
}

struct AcceptedCounts {
    id: u64,
    total: Option<u64>,
    previous: [u64; 3],
}
impl AcceptedCounts {
    fn new(id: u64) -> Self {
        Self {
            id,
            total: None,
            previous: [0; 3],
        }
    }
    fn observe(&mut self, summary: &Value) -> Result<[u64; 3], RelishError> {
        let number = |key: &str| {
            summary[key].as_u64().ok_or_else(|| {
                failure(format!(
                    "invalid accepted counter {key} for batch {}",
                    self.id
                ))
            })
        };
        if number("batch_id")? != self.id {
            return Err(failure("summary belongs to another submission"));
        }
        let total = number("total")?;
        let counts = [number("succeeded")?, number("failed")?, number("retried")?];
        let not_run = number("not_run")?;
        if self.total.is_some_and(|old| old != total)
            || counts[0]
                .checked_add(counts[1])
                .and_then(|sum| sum.checked_add(not_run))
                .is_none_or(|sum| sum > total)
            || counts
                .iter()
                .zip(self.previous)
                .any(|(now, before)| *now < before)
        {
            return Err(failure(
                "accepted counters changed identity, exceeded total or went backwards",
            ));
        }
        let delta = std::array::from_fn(|i| counts[i] - self.previous[i]);
        self.total = Some(total);
        self.previous = counts;
        Ok(delta)
    }
}

async fn positive_drain(client: &BunClient, id: u64) -> Result<DrainProof, RelishError> {
    client.cancel_batch(id).await?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let summary=tokio::time::timeout_at(deadline,client.batch_status(id)).await.map_err(|_|failure(format!("batch {id} did not positively drain within 120s; inspect `relish batch-status {id}`")))??;
        if let Some(proof) = DrainProof::observe(&summary, id) {
            return Ok(proof);
        }
        tokio::time::sleep_until((Instant::now() + Duration::from_millis(250)).min(deadline)).await;
    }
}
async fn measure_public(
    args: &JobBenchArgs,
    client: &BunClient,
    cancel: &CancellationToken,
) -> JobBenchReport {
    let mut report = JobBenchReport::new(args);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(u64::from(args.seconds));
    let mut active: Option<AcceptedCounts> = None;
    let mut next_progress = started;
    let mut unknown_submission = false;
    let measured:Result<(),RelishError>=async {
        while Instant::now()<deadline && !cancel.is_cancelled() {
            if active.is_none() {
                let name=format!("bench-jobs-{:032x}",rand::random::<u128>());
                let request=args.manifest(&name)?;report.submission_names.push(name.clone());
                // Don't abort a POST on Ctrl-C: its response carries the identity we must drain.
                unknown_submission=true;
                let answer=client.submit_task_manifest(&request,Some(&name)).await.map_err(|error|failure(format!("submission {name} may have been accepted; inspect `relish jobs`; {error}")))?;
                let id=answer["batch_id"].as_u64().filter(|id|*id>0).ok_or_else(||failure(format!("submission {name} returned no positive identity")))?;
                if report.batch_ids.contains(&id) {return Err(failure("benchmark submission reused a previous task identity"));}
                report.batch_ids.push(id);active=Some(AcceptedCounts::new(id));unknown_submission=false;
            }
            let Some(tracker)=active.as_mut() else {continue};
            let answer=tokio::select! {
                biased;
                ()=cancel.cancelled()=>break,
                result=tokio::time::timeout_at(deadline,client.batch_status(tracker.id))=>match result {Ok(value)=>value?,Err(_)=>break},
            };
            if !within_window(Instant::now(),deadline) {break;}
            report.last_accepted_sample_seconds=Some(started.elapsed().as_secs_f64());
            let delta=tracker.observe(&answer)?;
            report.unique_accepted_successes+=delta[0];report.terminal_failures+=delta[1];report.accepted_retries+=delta[2];
            if answer["done"].as_bool().is_none() {return Err(failure("summary omitted terminal status"));}
            if answer["done"]==true {
                if answer["not_run"].as_u64()!=Some(0) {return Err(failure("benchmark stopped before completing its queued work"));}
                let proof=DrainProof::observe(&answer,tracker.id).ok_or_else(||failure("terminal summary has no positive drain proof"))?;
                report.drain_proofs.push(proof);active=None;
            }
            if Instant::now()>=next_progress {
                eprintln!("{:.1}s: {} unique accepted successes, {} failures, {} retries; {:.1}/s",started.elapsed().as_secs_f64(),report.unique_accepted_successes,report.terminal_failures,report.accepted_retries,report.unique_accepted_successes as f64/started.elapsed().as_secs_f64().max(0.001));
                next_progress=Instant::now()+Duration::from_secs(5);
            }
            if active.is_some() {
                tokio::select! {()=cancel.cancelled()=>break,()=tokio::time::sleep_until((Instant::now()+Duration::from_millis(250)).min(deadline))=>{}}
            }
        }
        Ok(())
    }.await;
    report.interrupted = cancel.is_cancelled();
    report.measurement_complete =
        measured.is_ok() && !report.interrupted && Instant::now() >= deadline;
    if let Err(error) = measured {
        report.error = Some(error.to_string());
    }
    report.cleanup_verified = !unknown_submission;
    if let Some(tracker) = active {
        report.active_submissions.push(tracker.id);
        match positive_drain(client, tracker.id).await {
            Ok(proof) => report.drain_proofs.push(proof),
            Err(error) => {
                report.cleanup_verified = false;
                report.cleanup_failed_submissions.push(tracker.id);
                let diagnostic = format!("cleanup of batch {} failed: {error}", tracker.id);
                report.error = Some(
                    report
                        .error
                        .map_or_else(|| diagnostic.clone(), |old| format!("{old}; {diagnostic}")),
                );
            }
        }
    }
    report.finalise(started.elapsed().as_secs_f64());
    report
}

#[cfg(target_os = "linux")]
async fn measure_raw(args: &JobBenchArgs, cancel: &CancellationToken) -> JobBenchReport {
    use sha2::{Digest, Sha256};
    use std::os::unix::process::CommandExt;
    let mut report = JobBenchReport::new(args);
    let started = Instant::now();
    let executable = match &args.exec {
        Some(path) => path.clone(),
        None => {
            let Some(cache) = dirs::cache_dir() else {
                report.error = Some(
                    "cannot find user image cache; supply an absolute --exec BusyBox path".into(),
                );
                report.cleanup_verified = true;
                return report;
            };
            eprintln!(
                "preparing the pinned BusyBox image in the local cache; excluded from the raw window"
            );
            let images = crate::grill::ImageStore::new(cache.join("reliaburger/bench-jobs/images"));
            match images.pull_and_unpack(&args.image).await {
                Ok(image) => image.rootfs.join("bin/busybox"),
                Err(error) => {
                    report.error = Some(format!("cannot prepare baseline image: {error}"));
                    report.cleanup_verified = true;
                    return report;
                }
            }
        }
    };
    report.executable = Some(executable.clone());
    let bytes = match tokio::fs::read(&executable).await {
        Ok(bytes) => bytes,
        Err(error) => {
            report.error = Some(format!(
                "cannot read local BusyBox {}: {error}",
                executable.display()
            ));
            report.cleanup_verified = true;
            return report;
        }
    };
    report.executable_sha256 = Some(format!("{:x}", Sha256::digest(&bytes)));
    drop(bytes);
    report.started_at = crate::testkit::runner::now_rfc3339();
    let measured_start = Instant::now();
    let deadline = measured_start + Duration::from_secs(u64::from(args.seconds));
    let mut pending = tokio::task::JoinSet::new();
    let mut next = 0u32;
    let mut next_progress = measured_start;
    while !pending.is_empty()
        || (Instant::now() < deadline && !cancel.is_cancelled() && report.error.is_none())
    {
        while pending.len() < args.concurrency as usize
            && Instant::now() < deadline
            && !cancel.is_cancelled()
            && report.error.is_none()
        {
            let executable = executable.clone();
            let index = next;
            next = next.wrapping_add(1);
            pending.spawn(async move {
                let mut command = tokio::process::Command::new(executable);
                command.as_std_mut().arg0("/bin/busybox");
                command
                    .arg("true")
                    .current_dir("/")
                    .env_clear()
                    .envs(crate::meat::task_array::task_env(0, u32::MAX, index, 1))
                    .kill_on_drop(true);
                let mut child = command.spawn()?;
                let result = match tokio::time::timeout_at(
                    deadline + Duration::from_secs(120),
                    child.wait(),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        child.kill().await?;
                        Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "raw child exceeded the drain deadline and was killed/reaped",
                        ))
                    }
                };
                Ok::<_, std::io::Error>((result, Instant::now()))
            });
        }
        if let Some(joined) = pending.join_next().await {
            match joined {
                Ok(Ok((result, completed))) => {
                    if within_window(completed, deadline) {
                        match result {
                            Ok(status) if status.success() => report.verified_successes += 1,
                            Ok(_) => report.terminal_failures += 1,
                            Err(error) => {
                                report.error =
                                    Some(format!("raw child failed to launch or wait: {error}"))
                            }
                        }
                    } else {
                        report.post_cutoff_drained += 1;
                        if let Err(error) = result {
                            report.error = Some(format!("raw child drain failed: {error}"));
                        }
                    }
                }
                Ok(Err(error)) => {
                    report.error = Some(format!(
                        "raw child could not be spawned or positively reaped: {error}"
                    ))
                }
                Err(error) => report.error = Some(format!("raw child waiter failed: {error}")),
            }
        }
        if Instant::now() >= next_progress {
            eprintln!(
                "{:.1}s: {} raw successful exits within the window; {} failures",
                measured_start.elapsed().as_secs_f64(),
                report.verified_successes,
                report.terminal_failures
            );
            next_progress = Instant::now() + Duration::from_secs(5);
        }
    }
    report.interrupted = cancel.is_cancelled();
    report.measurement_complete =
        !report.interrupted && report.error.is_none() && Instant::now() >= deadline;
    report.cleanup_verified = pending.is_empty() && report.error.is_none();
    report.finalise(started.elapsed().as_secs_f64());
    report
}
#[cfg(not(target_os = "linux"))]
async fn measure_raw(args: &JobBenchArgs, _cancel: &CancellationToken) -> JobBenchReport {
    let mut report = JobBenchReport::new(args);
    report.error=Some("jobs-vm-baseline runs on local Linux; run Relish inside the same VM/node as the public jobs".into());
    report.cleanup_verified = true;
    report
}

/// Run one timed scenario and render a report even when measurement or cleanup fails.
pub async fn run(args: JobBenchArgs) -> Result<CommandOutcome, RelishError> {
    args.validate()?;
    // Reserve the report before any work so a typo cannot overwrite older evidence.
    let mut evidence = match &args.report {
        Some(path) => Some(
            tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .await
                .map_err(|error| {
                    failure(format!("cannot create report {}: {error}", path.display()))
                })?,
        ),
        None => None,
    };
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    eprintln!(
        "{}: {} seconds, concurrency {}{}; {}",
        args.scenario.name(),
        args.seconds,
        args.concurrency,
        if args.scenario == JobScenario::VmBaseline {
            " locally"
        } else {
            " per node"
        },
        if args.scenario == JobScenario::VmBaseline {
            "image/executable preparation precedes the window; positive drain follows it"
        } else {
            "submission and cold startup are inside the window; positive drain follows it"
        }
    );
    let report = if args.scenario == JobScenario::VmBaseline {
        measure_raw(&args, &cancel).await
    } else {
        measure_public(&args, &BunClient::default_local(), &cancel).await
    };
    signal.abort();
    let json = serde_json::to_string_pretty(&report).map_err(RelishError::SerialiseJson)?;
    if let Some(file) = evidence.as_mut() {
        use tokio::io::AsyncWriteExt;
        file.write_all(format!("{json}\n").as_bytes())
            .await
            .map_err(|error| failure(format!("cannot write benchmark evidence: {error}")))?;
        file.sync_all()
            .await
            .map_err(|error| failure(format!("cannot sync benchmark evidence: {error}")))?;
    }
    let rendered = match args.output {
        OutputFormat::Json => json,
        OutputFormat::Yaml => serde_yaml::to_string(&report).map_err(RelishError::SerialiseYaml)?,
        OutputFormat::Human => format!(
            "{}: {} {} in {} seconds ({:.1}/s)\n{} failures, {} retries; cleanup {}\n{} runs/day extrapolated — not an observed daily total{}",
            args.scenario.name(),
            report.count(),
            if args.scenario == JobScenario::VmBaseline {
                "raw successful exits"
            } else {
                "unique accepted successes"
            },
            args.seconds,
            report.successes_per_second,
            report.terminal_failures,
            report.accepted_retries,
            if report.cleanup_verified {
                "verified"
            } else {
                "UNVERIFIED"
            },
            human_count(report.extrapolated_runs_per_day),
            report
                .error
                .as_ref()
                .map_or(String::new(), |error| format!("\nfailed: {error}"))
        ),
    };
    println!("{rendered}");
    if args.output == OutputFormat::Human && !report.measurement_complete {
        eprintln!(
            "measurement incomplete{}; the rate still uses the requested window, not a shortened denominator",
            if report.interrupted {
                " (interrupted)"
            } else {
                ""
            }
        );
    }
    Ok(if report.healthy() {
        CommandOutcome::Clean
    } else {
        CommandOutcome::Problems
    })
}
fn human_count(value: f64) -> String {
    for (scale, suffix) in [(1e9, "B"), (1e6, "M"), (1e3, "k")] {
        if value >= scale {
            let rounded = (value / scale * 10.0).round() / 10.0;
            let number = format!("{rounded:.1}");
            return format!("{}{suffix}", number.trim_end_matches(".0"));
        }
    }
    format!("{value:.0}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    fn args() -> JobBenchArgs {
        JobBenchArgs {
            scenario: JobScenario::SharedContainers,
            seconds: 1,
            concurrency: 27,
            cpu_request: 25,
            image: DEFAULT_IMAGE.into(),
            exec: Some("/bin/busybox".into()),
            namespace: "default".into(),
            output: OutputFormat::Json,
            report: None,
        }
    }

    #[test]
    fn all_public_scenarios_use_the_same_resources_and_explicit_runtime() {
        for (scenario, runtime, chunk) in [
            (JobScenario::Containers, JobRuntime::Runc, 1),
            (JobScenario::SharedContainers, JobRuntime::SharedRunc, 1000),
            (JobScenario::HostProcesses, JobRuntime::Process, 1000),
        ] {
            let options = JobBenchArgs { scenario, ..args() };
            let request = options.manifest("bench-test").unwrap();
            let cohort = &request.cohort[0];
            assert_eq!(cohort.template.runtime, runtime);
            assert_eq!(cohort.spec.chunk_size, chunk);
            assert_eq!(cohort.spec.per_node_concurrency, Some(27));
            assert_eq!(cohort.spec.max_attempts, 3);
            assert_eq!(
                cohort.template.cpu,
                Some(ResourceRange {
                    request: 25,
                    limit: 1000
                })
            );
            assert_eq!(
                cohort.template.memory,
                Some(ResourceRange {
                    request: 32 << 20,
                    limit: 32 << 20
                })
            );
            cohort.spec.validate().unwrap();
            cohort.template.validate_runtime().unwrap();
            assert_eq!(
                cohort.template.image.is_some(),
                runtime != JobRuntime::Process
            );
            assert_eq!(
                cohort.template.exec.is_some(),
                runtime == JobRuntime::Process
            );
        }
    }

    #[test]
    fn accepted_counters_reject_identity_changes_regressions_and_impossible_totals() {
        let mut counts = AcceptedCounts::new(7);
        let summary =
            json!({"batch_id":7,"total":10000,"succeeded":1000,"failed":0,"not_run":0,"retried":1});
        assert_eq!(counts.observe(&summary).unwrap(), [1000, 0, 1]);
        assert_eq!(counts.observe(&summary).unwrap(), [0, 0, 0]);
        for (field, value) in [
            ("batch_id", 8),
            ("succeeded", 999),
            ("failed", 10000),
            ("total", 9000),
        ] {
            let mut bad = summary.clone();
            bad[field] = json!(value);
            assert!(counts.observe(&bad).is_err(), "{bad}");
        }
        let mut bad = summary;
        bad["retried"] = serde_json::Value::Null;
        assert!(counts.observe(&bad).is_err());
    }

    #[test]
    fn daily_projection_is_labelled_and_cutoff_is_inclusive() {
        let cutoff = Instant::now();
        assert!(within_window(cutoff, cutoff));
        assert!(!within_window(cutoff + Duration::from_nanos(1), cutoff));
        let mut report = JobBenchReport::new(&args());
        report.unique_accepted_successes = 625;
        report.finalise(1.1);
        assert_eq!(report.extrapolated_runs_per_day, 54_000_000.0);
        assert_eq!(human_count(report.extrapolated_runs_per_day), "54M");
        assert!(!report.qualified_100m_per_day);
        assert!(report.daily_projection.contains("not an observed"));
    }

    async fn fixture(
        late: bool,
        status_error: bool,
        cancel_error: bool,
    ) -> (BunClient, Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
        let polls = Arc::new(AtomicU64::new(0));
        let cancels = Arc::new(AtomicU64::new(0));
        let p = polls.clone();
        let c = cancels.clone();
        let post_c = cancels.clone();
        let app=Router::new()
            .route("/v1/batch/manifest",post(|Json(request):Json<TaskManifestRequest>| async move {
                assert!(request.name.starts_with("bench-jobs-"));
                assert_eq!(request.cohort[0].template.runtime,JobRuntime::SharedRunc);
                Json(json!({"batch_id":7}))
            }))
            .route("/v1/batch/{id}",get(move || {
                let p=p.clone();let c=c.clone();async move {
                    let cancelled=c.load(Ordering::SeqCst)>0;
                    if cancelled { return Ok(Json(json!({"batch_id":7,"done":true,"held":0,"active_commands":0,"succeeded":9000}))); }
                    if status_error { return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE); }
                    let first=p.fetch_add(1,Ordering::SeqCst)==0;
                    if late && !first {tokio::time::sleep(Duration::from_millis(1500)).await;}
                    Ok(Json(json!({"batch_id":7,"total":16000000,"succeeded":if first {1000}else{5000},"failed":0,"not_run":0,"retried":0,"done":false})))
                }
            }))
            .route("/v1/batch/{id}/cancel",post(move || {let c=post_c.clone();async move {
                c.fetch_add(1,Ordering::SeqCst);
                if cancel_error {axum::http::StatusCode::SERVICE_UNAVAILABLE}else{axum::http::StatusCode::OK}
            }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            BunClient::new_with_token(&format!("http://{}", listener.local_addr().unwrap()), None);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (client, cancels, server)
    }

    #[tokio::test]
    async fn late_status_and_post_cutoff_drain_are_never_credited() {
        let (client, cancels, server) = fixture(true, false, false).await;
        let report = measure_public(&args(), &client, &CancellationToken::new()).await;
        assert_eq!(report.unique_accepted_successes, 1000);
        assert_eq!(cancels.load(Ordering::SeqCst), 1);
        assert!(report.measurement_complete);
        assert!(report.cleanup_verified);
        assert!(report.error.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn api_errors_still_cancel_owned_work_and_cleanup_errors_fail_the_report() {
        for (status_error, cancel_error) in [(true, false), (false, true)] {
            let (client, cancels, server) = fixture(false, status_error, cancel_error).await;
            let report = measure_public(&args(), &client, &CancellationToken::new()).await;
            assert!(report.error.is_some() || !report.cleanup_verified);
            assert_eq!(cancels.load(Ordering::SeqCst), 1);
            assert!(!report.healthy());
            server.abort();
        }
    }

    #[tokio::test]
    async fn interrupt_cancels_only_the_owned_submission_and_waits_for_positive_drain() {
        let (client, cancels, server) = fixture(false, false, false).await;
        let token = CancellationToken::new();
        let interrupt = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            interrupt.cancel();
        });
        let report = measure_public(&args(), &client, &token).await;
        assert!(report.interrupted);
        assert!(!report.measurement_complete);
        assert!(report.cleanup_verified);
        assert_eq!(report.batch_ids, vec![7]);
        assert_eq!(cancels.load(Ordering::SeqCst), 1);
        assert!(!report.healthy());
        server.abort();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn local_baseline_verifies_children_and_reports_its_executable() {
        let options = JobBenchArgs {
            scenario: JobScenario::VmBaseline,
            exec: Some("/bin/true".into()),
            ..args()
        };
        let report = measure_raw(&options, &CancellationToken::new()).await;
        assert!(report.measurement_complete);
        assert!(report.cleanup_verified);
        assert!(report.verified_successes > 0);
        assert!(report.executable_sha256.is_some());
        assert_eq!(report.unique_accepted_successes, 0);
        assert!(report.healthy(), "{report:?}");
    }
}
