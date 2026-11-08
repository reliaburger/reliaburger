//! One `relish gym run`: every run of every scenario, each cleaned up.
//!
//! A run is: apply the scenario's apps, inject its faults, create the
//! solver's token, run the solver, read its answer, its calls and the audit
//! log, score them, and clean up. The cleanup isn't the last step of that
//! list; it's what [`one_run`] does *after* the list, whatever happened in
//! it:
//!
//! - the list finishes, or returns an error: cleanup runs next;
//! - Ctrl-C: `tokio::select!` drops the list's future at its next `.await`
//!   (dropping the solver kills its process group), then cleanup runs;
//! - a panic: `catch_unwind` on the future catches it, cleanup runs, then
//!   the panic carries on.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::catalogue::{FaultStep, Scenario, Step, Workloads};
use super::cleanup::{CleanupPace, CleanupReport, Ledger, clean_up};
use super::cluster::{GymCluster, PlacedFault, instance_nodes};
use super::record;
use super::report::{CallSummary, Injected, Redactor, RunOutcome, RunRecord};
use super::score::{self, Draft, Score};
use super::solver::{self, Attempt, SolverError};
use crate::smoker::types::{FaultRequest, FaultType};

/// How long the gym waits for things, so tests can run in milliseconds.
#[derive(Debug, Clone, Copy)]
pub struct Pace {
    /// How often to look at the cluster while waiting.
    pub poll: Duration,
    /// How long an app may take to run (an image pull included).
    pub wait_running: Duration,
    /// Multiplies each scenario's settle time; 0 in tests.
    pub settle_scale: f64,
    /// The pause between two kills of a kill step.
    pub kill_gap: Duration,
    /// How cleanup waits.
    pub cleanup: CleanupPace,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(2),
            wait_running: Duration::from_secs(300),
            settle_scale: 1.0,
            kill_gap: Duration::from_secs(3),
            cleanup: CleanupPace::default(),
        }
    }
}

/// Everything a run needs to know.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// The staging namespace.
    pub namespace: String,
    /// The scenarios, in order.
    pub scenarios: Vec<Scenario>,
    /// Runs per scenario.
    pub runs: u32,
    /// The solver's argv.
    pub solver: Vec<String>,
    /// The solver's budget per run.
    pub timeout: Duration,
    /// How the gym's apps run.
    pub workloads: Workloads,
    /// This `relish`, which the recording `relish` runs.
    pub relish_exe: PathBuf,
    /// Where solvers work: removed afterwards, since it holds their drafts.
    pub scratch: PathBuf,
    /// Where each run's redacted evidence goes.
    pub out: PathBuf,
    /// A short id naming this invocation's tokens.
    pub run_id: String,
    /// The environment the solver inherits, without any `RELIABURGER_*`.
    pub base_env: BTreeMap<String, String>,
    /// How long things take.
    pub pace: Pace,
}

/// Why a run stopped before it was scored.
#[derive(Debug)]
enum Stop {
    /// The cluster refused a step the scenario needs.
    Skipped(String),
    /// Something failed.
    Error(String),
    /// Ctrl-C.
    Cancelled,
}

/// What a run has gathered so far, kept outside the run's future so a
/// cancelled or panicked run still reports what it got to.
#[derive(Debug, Default)]
struct Progress {
    injected: Vec<Injected>,
    truth: BTreeMap<String, String>,
    token: Option<String>,
    token_secret: Option<String>,
    attempt: Option<Attempt>,
    calls: Vec<score::Call>,
    answer: Option<Value>,
    answer_error: Option<String>,
    score: Option<Score>,
    run_dir: Option<PathBuf>,
}

/// Every run of every scenario, and whether Ctrl-C cut it short.
pub async fn run_all(
    cluster: &impl GymCluster,
    config: &RunConfig,
    cancel: &CancellationToken,
    redactor: &mut Redactor,
) -> (Vec<RunRecord>, bool) {
    let mut records = Vec::new();
    let mut sequence = 0u32;
    for scenario in &config.scenarios {
        for index in 1..=config.runs {
            if cancel.is_cancelled() {
                return (records, true);
            }
            sequence += 1;
            eprintln!("{} run {index}/{}", scenario.id, config.runs);
            let record =
                one_run(cluster, config, scenario, index, sequence, cancel, redactor).await;
            eprintln!(
                "  {}",
                match (&record.score, record.outcome) {
                    (Some(score), _) => score.verdict.as_str().to_string(),
                    (None, outcome) => format!(
                        "{}: {}",
                        outcome.as_str(),
                        record.reason.as_deref().unwrap_or("")
                    ),
                }
            );
            let cancelled = record.outcome == RunOutcome::Cancelled;
            // A run that left something behind stops the gym: the next run
            // would start on a namespace it can't vouch for.
            let dirty = !record.cleanup.verified;
            records.push(record);
            if cancelled {
                return (records, true);
            }
            if dirty {
                eprintln!("relish gym: stopping, because the last run's cleanup wasn't verified");
                return (records, false);
            }
        }
    }
    (records, false)
}

/// One run, always cleaned up.
async fn one_run(
    cluster: &impl GymCluster,
    config: &RunConfig,
    scenario: &Scenario,
    index: u32,
    sequence: u32,
    cancel: &CancellationToken,
    redactor: &mut Redactor,
) -> RunRecord {
    let mut ledger = Ledger::new(&config.namespace);
    let mut progress = Progress::default();
    let outcome = AssertUnwindSafe(async {
        tokio::select! {
            result = run_body(cluster, config, scenario, index, sequence, &mut ledger, &mut progress, cancel) => result,
            () = cancel.cancelled() => Err(Stop::Cancelled),
        }
    })
    .catch_unwind()
    .await;
    // Whatever happened above, undo it.
    let cleanup = clean_up(cluster, &mut ledger, config.pace.cleanup).await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    if let Some(secret) = &progress.token_secret {
        redactor.mask(secret);
    }
    finish(
        config, scenario, index, outcome, progress, cleanup, redactor,
    )
}

fn finish(
    config: &RunConfig,
    scenario: &Scenario,
    index: u32,
    outcome: Result<(), Stop>,
    progress: Progress,
    cleanup: CleanupReport,
    redactor: &mut Redactor,
) -> RunRecord {
    let (outcome, reason) = match outcome {
        Ok(()) => (RunOutcome::Scored, None),
        Err(Stop::Skipped(reason)) => (RunOutcome::Skipped, Some(reason)),
        Err(Stop::Error(reason)) => (RunOutcome::Error, Some(reason)),
        Err(Stop::Cancelled) => (
            RunOutcome::Cancelled,
            Some("cancelled (Ctrl-C); the solver was stopped and the run cleaned up".into()),
        ),
    };
    let calls: Vec<CallSummary> = progress
        .calls
        .iter()
        .map(|call| CallSummary {
            argv: call.argv.iter().map(|word| redactor.text(word)).collect(),
            exit: call.exit,
            seconds: call.seconds,
        })
        .collect();
    let mut score = progress.score;
    if let Some(score) = &mut score {
        score.safety.audit_records = score
            .safety
            .audit_records
            .iter()
            .map(|record| redactor.value(record))
            .collect();
        for check in score.checks.values_mut() {
            check.got = check.got.as_ref().map(|got| redactor.value(got));
            check.detail = check.detail.as_ref().map(|detail| redactor.value(detail));
        }
        for problem in &mut score.answer_problems {
            *problem = redactor.text(problem);
        }
    }
    let answer = progress
        .answer
        .as_ref()
        .map(|answer| redactor.value(answer));
    write_evidence(
        config,
        scenario,
        index,
        &progress.calls,
        answer.as_ref(),
        progress.attempt.as_ref(),
        progress.answer_error.as_deref(),
        redactor,
    );
    RunRecord {
        scenario: scenario.id.to_string(),
        run: index,
        outcome,
        reason: reason.map(|reason| redactor.text(&reason)),
        injected: progress.injected,
        truth: progress.truth,
        answer,
        solver: progress.attempt,
        calls,
        score,
        token: progress.token,
        cleanup,
    }
}

/// Write a run's redacted evidence next to the report.
#[allow(clippy::too_many_arguments)]
fn write_evidence(
    config: &RunConfig,
    scenario: &Scenario,
    index: u32,
    calls: &[score::Call],
    answer: Option<&Value>,
    attempt: Option<&Attempt>,
    answer_error: Option<&str>,
    redactor: &mut Redactor,
) {
    let dir = config.out.join(scenario.id).join(format!("run-{index}"));
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut lines = String::new();
    for call in calls {
        let mut call = call.clone();
        call.argv = call.argv.iter().map(|word| redactor.text(word)).collect();
        call.stdout = redactor.text(&call.stdout);
        call.stderr = redactor.text(&call.stderr);
        if let Ok(line) = serde_json::to_string(&call) {
            lines.push_str(&line);
            lines.push('\n');
        }
    }
    let _ = std::fs::write(dir.join("calls.jsonl"), lines);
    let answer_text = match (answer, answer_error) {
        (Some(answer), _) => serde_json::to_string_pretty(answer).unwrap_or_default() + "\n",
        (None, Some(error)) => format!("{}\n", redactor.text(error)),
        (None, None) => String::new(),
    };
    let _ = std::fs::write(dir.join("answer.json"), answer_text);
    if let Some(attempt) = attempt {
        let _ = std::fs::write(dir.join("solver.stdout"), redactor.text(&attempt.stdout));
        let _ = std::fs::write(dir.join("solver.stderr"), redactor.text(&attempt.stderr));
    }
    let _ = std::fs::write(
        dir.join("prompt.md"),
        scenario.full_prompt(&config.namespace),
    );
}

fn error(context: &str) -> impl FnOnce(super::cluster::ClusterError) -> Stop + '_ {
    move |error| Stop::Error(format!("{context}: {error}"))
}

#[allow(clippy::too_many_arguments)]
async fn run_body(
    cluster: &impl GymCluster,
    config: &RunConfig,
    scenario: &Scenario,
    index: u32,
    sequence: u32,
    ledger: &mut Ledger,
    progress: &mut Progress,
    cancel: &CancellationToken,
) -> Result<(), Stop> {
    let namespace = config.namespace.as_str();
    for step in &scenario.steps {
        match step {
            Step::Apply { app, wait } => {
                // On the ledger before the request: an apply that times out
                // may still land.
                ledger.app(app.name);
                cluster
                    .apply(&app.manifest(namespace, &config.workloads))
                    .await
                    .map_err(error(&format!("apply {}", app.name)))?;
                progress.injected.push(Injected::Applied {
                    app: app.name.to_string(),
                    note: app_note(app.name, app.replicas, app.required_label),
                });
                if *wait {
                    wait_running(cluster, namespace, app.name, app.replicas, config.pace).await?;
                }
            }
            Step::Fault(fault) => inject(cluster, config, fault, ledger, progress).await?,
            Step::Settle(duration) => {
                tokio::time::sleep(duration.mul_f64(config.pace.settle_scale)).await;
            }
        }
    }

    let facts = instance_nodes(cluster, namespace, &scenario.facts_needed())
        .await
        .map_err(error("read where the apps run"))?;
    progress.truth = scenario
        .resolve_truth(namespace, &facts)
        .map_err(Stop::Error)?;

    let token_name = format!("gym-solver-{}-{sequence}", config.run_id);
    let secret = cluster
        .create_token(&token_name, namespace)
        .await
        .map_err(error(
            "create the solver's token (an unscoped admin token can)",
        ))?;
    ledger.token(&token_name);
    progress.token = Some(token_name.clone());
    progress.token_secret = Some(secret.clone());

    let run_dir = config.scratch.join(format!("{}-{index}", scenario.id));
    progress.run_dir = Some(run_dir.clone());
    let prepared = prepare(config, scenario, &run_dir, &secret, cluster.connection())
        .map_err(|error| Stop::Error(format!("prepare the solver's directory: {error}")))?;

    let prompt = scenario.full_prompt(namespace);
    let attempt = solver::run(
        &config.solver,
        &prompt,
        &prepared.env,
        &prepared.work_dir,
        config.timeout,
        cancel,
    )
    .await
    .map_err(|error| match error {
        SolverError::Cancelled => Stop::Cancelled,
        other => Stop::Error(other.to_string()),
    })?;
    progress.attempt = Some(attempt);

    let (answer, answer_error) = read_answer(&prepared.answer);
    progress.answer = answer;
    progress.answer_error = answer_error;
    progress.calls = record::read_log(&prepared.call_log);
    let audit = cluster
        .audit(&token_name)
        .await
        .map_err(error("read the audit log, which proves what the token did"))?;
    let drafts = find_drafts(&prepared.work_dir);
    let mut scored = score::score(
        &scenario.spec,
        &progress.truth,
        progress.answer.as_ref(),
        &progress.calls,
        &audit,
        &drafts,
    );
    if let Some(problem) = &progress.answer_error {
        scored.answer_problems.insert(0, problem.clone());
    }
    progress.score = Some(scored);
    Ok(())
}

fn app_note(app: &str, replicas: u32, label: Option<&str>) -> String {
    let mut note = format!("{replicas} replica(s)");
    if let Some(label) = label {
        note.push_str(&format!(", requires node label `{label}`"));
    }
    if app == "gym-web" {
        note.push_str(", a version of gym-web");
    }
    note
}

async fn wait_running(
    cluster: &impl GymCluster,
    namespace: &str,
    app: &str,
    replicas: u32,
    pace: Pace,
) -> Result<(), Stop> {
    let deadline = tokio::time::Instant::now() + pace.wait_running;
    loop {
        let instances = cluster
            .instances(namespace, app)
            .await
            .map_err(error(&format!("read {app}'s instances")))?;
        let running = instances
            .iter()
            .filter(|instance| instance.state == "running")
            .count();
        if running >= replicas as usize && running == instances.len() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Stop::Error(format!(
                "{app} didn't run within {} s ({running} of {replicas} running)",
                pace.wait_running.as_secs()
            )));
        }
        tokio::time::sleep(pace.poll).await;
    }
}

/// A fault's lifetime: the solver's budget and five minutes for the rest,
/// so a fault the gym never cleared still ends by itself.
fn fault_ttl(config: &RunConfig) -> Duration {
    config.timeout + Duration::from_secs(300)
}

fn fault_request(
    config: &RunConfig,
    fault_type: FaultType,
    app: &str,
    instance: Option<String>,
    duration: Duration,
) -> FaultRequest {
    FaultRequest {
        fault_type,
        target_service: app.to_string(),
        namespace: Some(config.namespace.clone()),
        target_instance: instance,
        workload: Default::default(),
        task_index: None,
        target_node: None,
        duration,
        // Bun records the authenticated token's name instead.
        injected_by: String::new(),
        reason: Some("relish gym run".to_string()),
        include_leader: false,
        override_safety: false,
        acknowledged: true,
    }
}

async fn inject(
    cluster: &impl GymCluster,
    config: &RunConfig,
    fault: &FaultStep,
    ledger: &mut Ledger,
    progress: &mut Progress,
) -> Result<(), Stop> {
    let namespace = config.namespace.as_str();
    let refused = |error: super::cluster::ClusterError| {
        Stop::Skipped(format!("the cluster refused the Smoker fault: {error}"))
    };
    let mut record = |placed: &[PlacedFault], instance: Option<String>, progress: &mut Progress| {
        ledger.faults(placed);
        for fault in placed {
            progress.injected.push(Injected::Fault {
                app: fault.app.clone(),
                fault: fault.kind.clone(),
                id: fault.id,
                node: fault.node.clone(),
                instance: instance.clone(),
            });
        }
    };
    match fault {
        FaultStep::Pause { app, ordinal } => {
            let mut instances = cluster
                .instances(namespace, app)
                .await
                .map_err(error(&format!("read {app}'s instances")))?;
            instances.sort_by(|left, right| left.id.cmp(&right.id));
            let suffix = format!("-{ordinal}");
            let instance = instances
                .iter()
                .find(|instance| instance.id.ends_with(&suffix))
                .or_else(|| instances.first())
                .map(|instance| instance.id.clone());
            let request = fault_request(
                config,
                FaultType::Pause,
                app,
                instance.clone(),
                fault_ttl(config),
            );
            let placed = cluster.inject(&request).await.map_err(refused)?;
            record(&placed, instance, progress);
        }
        FaultStep::Kill { app, times } => {
            let replicas = super::catalogue::API.replicas;
            for kill in 0..*times {
                if kill > 0 {
                    tokio::time::sleep(config.pace.kill_gap).await;
                    wait_running(cluster, namespace, app, replicas, config.pace).await?;
                }
                let request = fault_request(
                    config,
                    FaultType::Kill { count: 1 },
                    app,
                    None,
                    Duration::ZERO,
                );
                let placed = cluster.inject(&request).await.map_err(refused)?;
                record(&placed, None, progress);
            }
        }
    }
    Ok(())
}

/// A solver's directory, environment and files.
struct Prepared {
    work_dir: PathBuf,
    env: BTreeMap<String, String>,
    answer: PathBuf,
    call_log: PathBuf,
}

fn prepare(
    config: &RunConfig,
    scenario: &Scenario,
    run_dir: &Path,
    token: &str,
    connection: super::cluster::Connection,
) -> std::io::Result<Prepared> {
    let work_dir = run_dir.join("work");
    let home = run_dir.join("home");
    let bin = run_dir.join("bin");
    std::fs::create_dir_all(&work_dir)?;
    std::fs::create_dir_all(&home)?;
    let call_log = run_dir.join("calls.jsonl");
    record::install(&bin, &config.relish_exe, &call_log)?;
    let prompt_path = run_dir.join("prompt.md");
    std::fs::write(&prompt_path, scenario.full_prompt(&config.namespace))?;
    let answer = run_dir.join("answer.json");

    let mut env = config.base_env.clone();
    let path = env.get("PATH").cloned().unwrap_or_default();
    env.insert(
        "PATH".into(),
        if path.is_empty() {
            bin.display().to_string()
        } else {
            format!("{}:{path}", bin.display())
        },
    );
    // A fresh home, so `relish` doesn't find the user's saved context and
    // its admin credentials by itself.
    env.insert("RELIABURGER_HOME".into(), home.display().to_string());
    env.insert("RELIABURGER_ENDPOINT".into(), connection.endpoint);
    env.insert("RELIABURGER_TOKEN".into(), token.to_string());
    if let Some(pem) = connection.ca_pem {
        let ca = run_dir.join("ca.pem");
        std::fs::write(&ca, pem)?;
        env.insert("RELIABURGER_CA_CERT".into(), ca.display().to_string());
    }
    env.insert("GYM_ANSWER".into(), answer.display().to_string());
    env.insert("GYM_PROMPT".into(), prompt_path.display().to_string());
    env.insert("GYM_CALL_LOG".into(), call_log.display().to_string());
    env.insert("GYM_PLATFORM".into(), "reliaburger".into());
    env.insert("GYM_NAMESPACE".into(), config.namespace.clone());
    env.insert(
        "GYM_SOLVER_META".into(),
        run_dir.join("solver-meta.json").display().to_string(),
    );
    Ok(Prepared {
        work_dir,
        env,
        answer,
        call_log,
    })
}

/// The answer file as JSON, or why there isn't one.
fn read_answer(path: &Path) -> (Option<Value>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, Some("the solver wrote no answer file".to_string()));
    };
    match serde_json::from_str(&text) {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(format!("the answer isn't JSON: {error}"))),
    }
}

/// Every report draft under `dir`: each directory holding a MANIFEST.json,
/// with the text of every file in it.
fn find_drafts(dir: &Path) -> Vec<Draft> {
    let mut manifests = Vec::new();
    collect_manifests(dir, &mut manifests, 0);
    manifests.sort();
    manifests
        .into_iter()
        .filter_map(|manifest| {
            let draft = manifest.parent()?.to_path_buf();
            let mut files = BTreeMap::new();
            collect_files(&draft, &draft, &mut files, 0);
            let manifest = files
                .get("MANIFEST.json")
                .and_then(|text| serde_json::from_str(text).ok());
            Some(Draft {
                dir: draft
                    .strip_prefix(dir)
                    .unwrap_or(&draft)
                    .display()
                    .to_string(),
                files,
                manifest,
            })
        })
        .collect()
}

/// How deep the gym looks for drafts, so a solver's huge tree can't stall it.
const MAX_DEPTH: usize = 6;

fn collect_manifests(dir: &Path, found: &mut Vec<PathBuf>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && depth < MAX_DEPTH {
            collect_manifests(&path, found, depth + 1);
        } else if kind.is_file() && entry.file_name() == "MANIFEST.json" {
            found.push(path);
        }
    }
}

fn collect_files(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && depth < MAX_DEPTH {
            collect_files(root, &path, files, depth + 1);
        } else if kind.is_file()
            && let Ok(bytes) = std::fs::read(&path)
        {
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            files.insert(name, String::from_utf8_lossy(&bytes).into_owned());
        }
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
