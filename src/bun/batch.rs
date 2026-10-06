//! Batch job submission, dispatch, and tracking (Phase 12, F1).
//!
//! The flow: `POST /v1/batch` (leader-forwarded) carries full job
//! specs; the leader maps the reporting pipeline's `AggregatedState`
//! into scheduler capacities, runs the library `schedule_batch`
//! bin-packer, registers the batch durably (Raft when a council
//! exists, the in-memory tracker standalone), and dispatches per-node
//! job groups — locally for itself, via `POST /v1/batch/run` for
//! peers. Running nodes watch their jobs to a terminal state and
//! report through `POST /v1/batch/{id}/report`; `GET /v1/batch/{id}`
//! serves the summary.
//!
//! Since 12b.2 (JOB3/JOB4) the push reports have a pull backstop: the
//! leader runs a per-batch watcher that polls the assigned nodes, so a
//! lost callback (or a leader restart — the watcher respawns from the
//! durable record) can no longer strand a batch. Dispatch and
//! callbacks retry with bounded backoff, reports are transition-
//! validated and idempotent, unschedulable jobs appear in the batch
//! as `Unschedulable` instead of vanishing, and the job namespace is
//! resolved once at submit and used everywhere.
//!
//! Batch deliberately does NOT ride the deploy placements reconciler:
//! that machinery *converges desired state* — a completed job looks
//! like drift to it, and moving an assignment would kill a running
//! job. Run-to-completion work wants dispatch + completion callbacks.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::config::job::JobSpec;
use crate::meat::batch::{BatchJob, schedule_batch};
use crate::meat::batch_tracker::{
    BatchJobRecord, BatchRecord, JobStatus, ReportError, ReportOutcome, epoch_now_secs,
};
use crate::meat::types::{NodeCapacity, NodeId, Resources};
use crate::reporting::aggregator::AggregatedState;

use super::agent::{AgentCommand, InstanceStatus};
use super::api::{ApiState, NodeMembershipInfo};

/// How long a dispatched job may run before the watcher gives up and
/// reports it failed.
const JOB_WATCH_TIMEOUT_SECS: u64 = 3600;

/// Extra slack the leader-side pull watcher grants past the job
/// timeout before it fails whatever is still pending.
const WATCH_GRACE_SECS: u64 = 60;

/// How often the leader-side pull watcher polls assigned nodes.
const PULL_INTERVAL_MS: u64 = 1000;

/// Attempts for dispatching a job group to its node.
const DISPATCH_ATTEMPTS: u32 = 3;

/// Attempts for delivering a completion callback.
const CALLBACK_ATTEMPTS: u32 = 4;

/// One job in a batch submission: the full spec travels with the
/// request, so target nodes need no prior deploy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchJobSubmission {
    pub name: String,
    /// Namespace the job deploys into. Optional on the wire; the submit
    /// handler resolves one authoritative value per job (JOB3) — this
    /// field must agree with `spec.namespace` when both are set.
    #[serde(default)]
    pub namespace: Option<String>,
    pub spec: JobSpec,
}

impl BatchJobSubmission {
    /// The resolved namespace (always set after submit-side resolution).
    pub fn namespace(&self) -> &str {
        self.namespace.as_deref().unwrap_or("default")
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchSubmitRequest {
    pub jobs: Vec<BatchJobSubmission>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchSubmitResponse {
    pub executions: BTreeMap<String, String>,
    pub batch_id: u64,
    pub assigned: usize,
    pub unschedulable: Vec<String>,
}

/// Trusted logical label for exactly one configured runtime execution.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchExecutionLabel {
    pub name: String,
    pub namespace: String,
}

/// Node-to-node dispatch: run these jobs, report to the callback.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchRunRequest {
    pub batch_id: u64,
    /// Base URL of the submitting (leader) node for completion
    /// reports; `None` when the leader runs its own share in-process.
    pub callback_base_url: Option<String>,
    pub jobs: Vec<BatchJobSubmission>,
    #[serde(default)]
    pub execution_labels: BTreeMap<String, BatchExecutionLabel>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchReportRequest {
    pub job_name: String,
    pub namespace: String,
    pub exit_code: Option<i32>,
    /// `"completed"`, `"failed"` or `"running"`.
    pub status: String,
}

// ---------------------------------------------------------------------------
// Namespace resolution (JOB3)
// ---------------------------------------------------------------------------

/// Resolve one authoritative namespace per job. The submission field
/// and `spec.namespace` used to be two independent sources: the agent
/// deployed into the spec's namespace while the watcher matched the
/// submission's, so a disagreement left the batch pending for an hour.
/// Now a conflict is rejected at submit, and the resolved value is
/// written into *both* places so dispatch, deploy and watching agree.
pub fn resolve_job_namespaces(
    mut jobs: Vec<BatchJobSubmission>,
) -> Result<Vec<BatchJobSubmission>, String> {
    let mut names = std::collections::HashSet::new();
    for job in &mut jobs {
        if !names.insert(job.name.clone()) {
            return Err(format!("duplicate batch job name {:?}", job.name));
        }
        let effective = match (&job.namespace, &job.spec.namespace) {
            (Some(a), Some(b)) if a != b => {
                return Err(format!(
                    "job {:?} names two namespaces: {a:?} in the submission, {b:?} in the spec",
                    job.name
                ));
            }
            (Some(a), _) => a.clone(),
            (None, Some(b)) => b.clone(),
            (None, None) => "default".to_string(),
        };
        job.namespace = Some(effective.clone());
        job.spec.namespace = Some(effective);
    }
    Ok(jobs)
}

/// Admit the complete group before either tracker registration or agent dispatch.
async fn validate_batch_jobs(
    state: &ApiState,
    jobs: &[BatchJobSubmission],
    headers: &axum::http::HeaderMap,
) -> Result<(), (StatusCode, String)> {
    if jobs.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "batch has no jobs".into()));
    }
    if headers.contains_key("x-reliaburger-test-lease") {
        return Err((
            StatusCode::BAD_REQUEST,
            "batch jobs do not support test leases".into(),
        ));
    }
    // Batch dispatch has no application-lease ownership registration. A
    // service credential cannot turn a reserved namespace/image into ordinary work.
    if jobs
        .iter()
        .any(|job| crate::testkit::lease::valid_test_namespace(job.namespace()))
    {
        return Err((
            StatusCode::CONFLICT,
            "batch jobs cannot use test lease namespaces without lease ownership".into(),
        ));
    }
    crate::testkit::lease::authorise_image_references(
        jobs.iter().filter_map(|job| job.spec.image.as_deref()),
        None,
    )
    .map_err(|error| (StatusCode::CONFLICT, error.to_string()))?;
    if jobs
        .iter()
        .any(|job| job.spec.schedule.is_some() || !job.spec.run_before.is_empty())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "batch jobs cannot declare schedule or run_before; use ordinary apply".into(),
        ));
    }
    let config = Config {
        job: jobs
            .iter()
            .map(|job| (job.name.clone(), job.spec.clone()))
            .collect(),
        ..Config::default()
    };
    let known_namespaces = match &state.council {
        Some(council) => council
            .desired_state()
            .await
            .namespaces
            .into_keys()
            .collect(),
        None => Vec::new(),
    };
    config
        .validate_against(&known_namespaces)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))
}

// ---------------------------------------------------------------------------
// Capacity
// ---------------------------------------------------------------------------

/// Map the leader's aggregated worker reports into scheduler
/// capacities — the same translation the deploy scheduler makes.
/// Reports carry *commitments* (per-instance requests summed against
/// the `[resources]` totals), which keeps this deterministic.
pub fn capacities_from_reports(
    members: &[NodeMembershipInfo],
    aggregated: &AggregatedState,
) -> Vec<NodeCapacity> {
    let mut capacities = Vec::new();
    for member in members {
        if !aggregated.report_is_fresh(&member.node_id) {
            continue;
        }
        let Some(report) = aggregated.reports.get(&member.node_id) else {
            continue;
        };
        let usage = &report.resource_usage;
        if usage.cpu_total_millicores == 0 {
            continue; // pre-capacity node
        }
        capacities.push(NodeCapacity {
            node_id: member.node_id.clone(),
            address: member.address,
            total: Resources::new(
                u64::from(usage.cpu_total_millicores),
                u64::from(usage.memory_total_mb) * 1024 * 1024,
                0,
            ),
            reserved: Resources::new(0, 0, 0), // baked into the totals
            allocated: Resources::new(
                u64::from(usage.cpu_used_millicores),
                u64::from(usage.memory_used_mb) * 1024 * 1024,
                0,
            ),
            labels: Default::default(),
        });
    }
    capacities
}

/// Standalone fallback: one self node with effectively unlimited
/// capacity, so single-node clusters (and tests) schedule locally.
pub fn local_only_capacity(node_name: &str) -> Vec<NodeCapacity> {
    vec![NodeCapacity {
        node_id: NodeId(node_name.to_string()),
        address: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        total: Resources::new(u64::MAX / 2, u64::MAX / 2, 0),
        reserved: Resources::new(0, 0, 0),
        allocated: Resources::new(0, 0, 0),
        labels: Default::default(),
    }]
}

// ---------------------------------------------------------------------------
// Durable batch state access
// ---------------------------------------------------------------------------

/// Why a batch report could not be applied, mapped to an HTTP status
/// by the report handler.
#[derive(Debug, Clone)]
pub enum BatchRejection {
    /// Unknown batch or job → 404.
    NotFound(String),
    /// Forged state, illegal or conflicting transition → 409.
    Conflict(String),
    /// The durable tracker could not be reached → 503.
    Unavailable(String),
}

fn map_report_error(error: ReportError) -> BatchRejection {
    match error {
        ReportError::UnknownBatch { .. } | ReportError::UnknownJob { .. } => {
            BatchRejection::NotFound(error.to_string())
        }
        ReportError::IllegalTransition { .. }
        | ReportError::NotReportable { .. }
        | ReportError::UnprovenExit => BatchRejection::Conflict(error.to_string()),
    }
}

/// Register a batch record durably: through Raft when a council
/// exists (the id comes from the replicated counter, JOB4), through
/// the in-memory tracker standalone.
pub(crate) async fn register_batch(
    state: &ApiState,
    record: BatchRecord,
    expected_log_id: Option<openraft::LogId<u64>>,
) -> Result<u64, String> {
    match &state.council {
        Some(council) => {
            let desired = council.desired_state().await;
            desired.batch_state.preflight_registration(&record)?;
            if record.jobs.iter().any(|job| {
                desired.apps.contains_key(&crate::meat::types::AppId::new(
                    &job.execution_name,
                    &job.namespace,
                ))
            }) {
                return Err("execution identity already belongs to an app".into());
            }
            match council
                .write(crate::council::types::RaftRequest::BatchRegister {
                    expected_log_id,
                    batch: record,
                })
                .await
            {
                Ok(crate::council::types::CouncilResponse::BatchRegistered { batch_id }) => {
                    Ok(batch_id)
                }
                Ok(crate::council::types::CouncilResponse::Refused { reason }) => Err(reason),
                Ok(other) => Err(format!("unexpected raft response: {other:?}")),
                Err(e) => Err(format!("raft register failed: {e}")),
            }
        }
        None => state
            .batch_tracker
            .lock()
            .await
            .register(record)
            .map(|id| id.0),
    }
}

/// Read a batch record from the durable tracker.
pub(crate) async fn get_batch(state: &ApiState, batch_id: u64) -> Option<BatchRecord> {
    match &state.council {
        Some(council) => council
            .desired_state()
            .await
            .batch_state
            .get(batch_id)
            .cloned(),
        None => state.batch_tracker.lock().await.get(batch_id),
    }
}

/// Apply a job report to the durable tracker, validating the
/// transition. On a node whose council handle is a follower the report
/// is forwarded to the leader's report endpoint over HTTP.
pub(crate) async fn report_batch_job(
    state: &ApiState,
    batch_id: u64,
    job_name: &str,
    namespace: &str,
    status: JobStatus,
    exit_code: Option<i32>,
) -> Result<ReportOutcome, BatchRejection> {
    let Some(council) = &state.council else {
        return state
            .batch_tracker
            .lock()
            .await
            .report(batch_id, job_name, namespace, status, exit_code)
            .map_err(map_report_error);
    };

    if !council.is_leader().await {
        return forward_report_to_leader(
            state, council, batch_id, job_name, namespace, status, exit_code,
        )
        .await;
    }

    // Pre-validate against the leader's (authoritative) replica so the
    // caller gets a precise verdict; duplicates never hit the log.
    let Some(record) = council
        .desired_state()
        .await
        .batch_state
        .get(batch_id)
        .cloned()
    else {
        return Err(BatchRejection::NotFound(
            ReportError::UnknownBatch { batch_id }.to_string(),
        ));
    };
    let mut probe = record;
    match probe.report(job_name, namespace, status, exit_code) {
        Err(e) => return Err(map_report_error(e)),
        Ok(ReportOutcome::Duplicate) => return Ok(ReportOutcome::Duplicate),
        Ok(ReportOutcome::Applied) => {}
    }
    match council
        .write(crate::council::types::RaftRequest::BatchJobUpdate {
            batch_id,
            job_name: job_name.to_string(),
            namespace: namespace.to_string(),
            status,
            exit_code,
        })
        .await
    {
        // A refusal here means we lost a race with another report; the
        // apply-side validation is the authority.
        Ok(crate::council::types::CouncilResponse::Refused { reason }) => {
            Err(BatchRejection::Conflict(reason))
        }
        Ok(_) => Ok(ReportOutcome::Applied),
        Err(e) => Err(BatchRejection::Unavailable(format!(
            "raft report failed: {e}"
        ))),
    }
}

/// POST the report to the leader's `/v1/batch/{id}/report`.
async fn forward_report_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    batch_id: u64,
    job_name: &str,
    namespace: &str,
    status: JobStatus,
    exit_code: Option<i32>,
) -> Result<ReportOutcome, BatchRejection> {
    let Some(leader_url) = super::api::leader_api_url(state, council).await else {
        return Err(BatchRejection::Unavailable(
            "no cluster leader known yet".to_string(),
        ));
    };
    let body = BatchReportRequest {
        job_name: job_name.to_string(),
        namespace: namespace.to_string(),
        exit_code,
        status: status_to_wire(status).to_string(),
    };
    let mut request = state
        .cluster_http
        .client()
        .post(format!("{leader_url}/v1/batch/{batch_id}/report"))
        .json(&body);
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => Ok(ReportOutcome::Applied),
        Ok(response) => {
            let code = response.status();
            let text = response.text().await.unwrap_or_default();
            match code.as_u16() {
                404 => Err(BatchRejection::NotFound(text)),
                409 => Err(BatchRejection::Conflict(text)),
                _ => Err(BatchRejection::Unavailable(text)),
            }
        }
        Err(e) => Err(BatchRejection::Unavailable(format!(
            "leader forward failed: {e}"
        ))),
    }
}

fn status_to_wire(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Pending => "pending",
        JobStatus::Unschedulable => "unschedulable",
    }
}

fn status_from_wire(status: &str) -> Option<JobStatus> {
    match status {
        "running" => Some(JobStatus::Running),
        "completed" => Some(JobStatus::Completed),
        "failed" => Some(JobStatus::Failed),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Running and watching
// ---------------------------------------------------------------------------

/// Where completion reports go.
pub enum Reporter {
    /// The submitting node itself — apply through the durable tracker.
    Leader(Box<ApiState>),
    /// A remote submitter — POST to its report endpoint, with bounded
    /// retries (JOB3). The leader-side pull watcher is the backstop if
    /// every attempt is lost.
    Callback {
        base_url: String,
        client: reqwest::Client,
        service_token: Option<String>,
    },
}

impl Reporter {
    async fn report(&self, batch_id: u64, job_name: &str, namespace: &str, exit_code: i32) {
        let status = if exit_code == 0 {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
        match self {
            Reporter::Leader(state) => {
                if let Err(e) = report_batch_job(
                    state,
                    batch_id,
                    job_name,
                    namespace,
                    status,
                    Some(exit_code),
                )
                .await
                {
                    eprintln!("bun: batch {batch_id}: local report for {job_name} rejected: {e:?}");
                }
            }
            Reporter::Callback {
                base_url,
                client,
                service_token,
            } => {
                let url = format!("{base_url}/v1/batch/{batch_id}/report");
                let body = BatchReportRequest {
                    job_name: job_name.to_string(),
                    namespace: namespace.to_string(),
                    exit_code: Some(exit_code),
                    status: status_to_wire(status).to_string(),
                };
                for attempt in 0..CALLBACK_ATTEMPTS {
                    let mut request = client.post(&url).json(&body);
                    if let Some(token) = service_token {
                        request = request.bearer_auth(token);
                    }
                    match request.send().await {
                        Ok(response) if response.status().is_success() => return,
                        // 4xx verdicts are final (duplicate/conflict);
                        // retrying cannot change them.
                        Ok(response) if response.status().is_client_error() => {
                            eprintln!("bun: batch report to {url} rejected: {}", response.status());
                            return;
                        }
                        Ok(response) => {
                            eprintln!("bun: batch report to {url}: {}", response.status());
                        }
                        Err(e) => eprintln!("bun: batch report to {url} failed: {e}"),
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * u64::from(attempt + 1),
                    ))
                    .await;
                }
                eprintln!(
                    "bun: batch {batch_id}: callback for {job_name} gave up after \
                     {CALLBACK_ATTEMPTS} attempts; the leader's pull watcher will catch it"
                );
            }
        }
    }
}

/// Map instance statuses to a job outcome: `Some(0)` completed,
/// `Some(1)` failed, `None` still running/unknown.
///
/// `stopped` alone is ambiguous: a failing job passes through it
/// between retries (any exit maps to Stopped; the code is tracked
/// separately). Success is stopped with exit 0; a non-zero stop is
/// backoff, not terminal — the agent marks the instance `failed` once
/// retries exhaust. Runtimes without exit codes (runc, review H13)
/// report `None`: preserve an unknown outcome until positive exit evidence exists.
fn job_outcome(statuses: &[InstanceStatus], name: &str, namespace: &str) -> Option<i32> {
    let expected = crate::grill::InstanceIdentity::new(namespace, name, 0).instance_id();
    statuses
        .iter()
        .find(|status| status.id == expected.0 && status.namespace == namespace)
        .and_then(|status| match (status.state.as_str(), status.exit_code) {
            ("stopped", Some(0)) => Some(0),
            ("failed", Some(code)) if code != 0 => Some(code),
            _ => None,
        })
}

/// Preflight and durably admit this entire group, then watch its exact executions.
// Admission errors already carry the HTTP status and body for this route.
#[allow(clippy::result_large_err)]
async fn admit_jobs_and_watch(
    state: &ApiState,
    batch_id: u64,
    jobs: Vec<BatchJobSubmission>,
    labels: BTreeMap<String, BatchExecutionLabel>,
    reporter: Reporter,
) -> Result<(), Response> {
    let mut config = Config::default();
    for job in &jobs {
        config.job.insert(job.name.clone(), job.spec.clone());
    }
    let (events, event_rx) = mpsc::channel(64);
    let terminal =
        super::api::ask_agent_bounded(&state.cmd_tx, |response| AgentCommand::RunJobsWithLabels {
            batch_id,
            config,
            execution_labels: labels,
            events,
            response,
        })
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "durable batch admission remains uncertain or unavailable",
            )
                .into_response()
        })?
        .map_err(|error| {
            let status = if matches!(error, crate::bun::BunError::BatchConflict(_)) {
                StatusCode::CONFLICT
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            (
                status,
                Json(serde_json::json!({"error": error.to_string()})),
            )
                .into_response()
        })?;
    tokio::spawn(run_jobs_and_watch(
        state.cmd_tx.clone(),
        batch_id,
        jobs,
        reporter,
        event_rx,
        terminal,
    ));
    Ok(())
}

/// Deploy this node's share of a batch and watch each job to a
/// terminal state, reporting as they finish. Spawned; never blocks a
/// handler.
async fn run_jobs_and_watch(
    cmd_tx: mpsc::Sender<AgentCommand>,
    batch_id: u64,
    jobs: Vec<BatchJobSubmission>,
    reporter: Reporter,
    mut event_rx: mpsc::Receiver<crate::bun::agent::ApplyEvent>,
    terminal: BTreeMap<String, i32>,
) {
    // Deployment errors do not prove an execution exited; retain the original
    // owned attempt and let positive runtime evidence decide its result.
    while event_rx.recv().await.is_some() {}
    let mut pending = Vec::new();
    for job in &jobs {
        if let Some(code) = terminal.get(&job.name) {
            reporter
                .report(batch_id, &job.name, job.namespace(), *code)
                .await;
        } else {
            pending.push(job);
        }
    }
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_secs(JOB_WATCH_TIMEOUT_SECS);
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
    while !pending.is_empty() && tokio::time::Instant::now() < deadline {
        ticker.tick().await;
        let (status_tx, status_rx) = oneshot::channel();
        if cmd_tx
            .send(AgentCommand::Status {
                response: status_tx,
            })
            .await
            .is_err()
        {
            return;
        }
        let Ok(statuses) = status_rx.await else {
            return;
        };
        let mut still_pending = Vec::new();
        for job in pending {
            if let Some(code) = job_outcome(&statuses, &job.name, job.namespace()) {
                reporter
                    .report(batch_id, &job.name, job.namespace(), code)
                    .await;
            } else {
                still_pending.push(job);
            }
        }
        pending = still_pending;
    }
    // Timeout/drop is an unknown boundary, not positive terminal evidence.
}

// ---------------------------------------------------------------------------
// The leader-side pull watcher (JOB3/JOB4)
// ---------------------------------------------------------------------------

/// Spawn a completion watcher for a batch, unless one is already live
/// in this process. Submit spawns one; a status read on a batch with
/// no watcher (a restarted leader) spawns one too — that is how a new
/// leader resumes in-flight batches from the durable record.
pub(crate) fn spawn_batch_watcher(state: &ApiState, batch_id: u64) {
    let state = state.clone();
    tokio::spawn(async move {
        {
            let mut watchers = state.batch_watchers.lock().await;
            if !watchers.insert(batch_id) {
                return; // someone is already watching
            }
        }
        watch_batch(&state, batch_id).await;
        state.batch_watchers.lock().await.remove(&batch_id);
    });
}

/// Poll the durable record and the assigned nodes until the batch is
/// terminal. Completion callbacks are the fast path; this pull loop is
/// the liveness backstop — a lost callback, a dead runner or a leader
/// restart all still converge to a terminal batch, bounded by the job
/// timeout plus grace.
async fn watch_batch(state: &ApiState, batch_id: u64) {
    loop {
        let Some(record) = get_batch(state, batch_id).await else {
            return; // pruned or never registered here
        };
        if record.is_terminal() {
            return;
        }

        let now = epoch_now_secs();
        let deadline = record.submitted_at_epoch_secs + JOB_WATCH_TIMEOUT_SECS + WATCH_GRACE_SECS;
        if now > deadline {
            return;
        }

        // Fetch this node's statuses once per tick if any job is local.
        let self_name = state
            .node_name
            .clone()
            .unwrap_or_else(|| "local".to_string());
        let needs_local = record
            .jobs
            .iter()
            .any(|j| !j.status.is_terminal() && j.node.as_ref().is_some_and(|n| n.0 == self_name));
        let local_statuses = if needs_local {
            fetch_local_statuses(state).await
        } else {
            None
        };

        for job in record.jobs.iter().filter(|j| !j.status.is_terminal()) {
            let Some(node) = &job.node else { continue };
            let outcome = if node.0 == self_name {
                local_statuses
                    .as_deref()
                    .and_then(|statuses| job_outcome(statuses, &job.execution_name, &job.namespace))
            } else {
                fetch_remote_outcome(state, node, &job.execution_name, &job.namespace).await
            };
            if let Some(code) = outcome {
                let status = if code == 0 {
                    JobStatus::Completed
                } else {
                    JobStatus::Failed
                };
                let _ = report_batch_job(
                    state,
                    batch_id,
                    &job.execution_name,
                    &job.namespace,
                    status,
                    Some(code),
                )
                .await;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(PULL_INTERVAL_MS)).await;
    }
}

async fn fetch_local_statuses(state: &ApiState) -> Option<Vec<InstanceStatus>> {
    if let Some(reader) = &state.status {
        return reader.read().await.ok();
    }
    let (status_tx, status_rx) = oneshot::channel();
    state
        .cmd_tx
        .send(AgentCommand::Status {
            response: status_tx,
        })
        .await
        .ok()?;
    status_rx.await.ok()
}

/// Poll a remote node's `/v1/status/{app}/{namespace}` for a job's
/// outcome. Unreachable nodes and missing instances read as "not yet"
/// — the watch deadline bounds how long that can last.
async fn fetch_remote_outcome(
    state: &ApiState,
    node: &NodeId,
    job_name: &str,
    namespace: &str,
) -> Option<i32> {
    let url = node_api_url(state, node).await?;
    let mut request = state.cluster_http.client().get(format!("{url}/v1/status"));
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let statuses: Vec<InstanceStatus> = response.json().await.ok()?;
    job_outcome(&statuses, job_name, namespace)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn plan_batch_admission(
    state: &ApiState,
    jobs: &[BatchJobSubmission],
    executions: &BTreeMap<String, String>,
    self_name: &str,
) -> Result<(u64, crate::meat::batch::BatchAllocation), Box<Response>> {
    let batch_jobs: Vec<BatchJob> = jobs
        .iter()
        .map(|job| BatchJob {
            name: job.name.clone(),
            resources: crate::meat::admission::job_requests(&job.spec),
        })
        .collect();
    for _ in 0..8 {
        let desired = match &state.council {
            Some(council) => Some(council.desired_state().await),
            None => None,
        };
        let expected_log_id = desired.as_ref().and_then(|state| state.last_applied_log);
        let mut capacities = if desired.is_some() {
            let (Some(aggregated_rx), Some(membership)) = (&state.aggregated_rx, &state.membership)
            else {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cluster batch capacity reports are unavailable",
                )
                    .into_response()
                    .into());
            };
            if aggregated_rx.has_changed().is_err() {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cluster capacity publisher is unavailable",
                )
                    .into_response()
                    .into());
            }
            let members = membership.read().await.clone();
            let aggregated = aggregated_rx.borrow().clone();
            if state
                .council
                .as_ref()
                .is_none_or(|council| aggregated.leadership_epoch != Some(council.current_term()))
            {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cluster capacity snapshot belongs to another leadership epoch",
                )
                    .into_response()
                    .into());
            }
            let mut capacities = capacities_from_reports(&members, &aggregated);
            if let Some(desired) = &desired {
                capacities.retain(|capacity| {
                    !desired
                        .security_state
                        .crl
                        .retired_nodes
                        .contains_key(&capacity.node_id.0)
                });
            }
            if capacities.is_empty() {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cluster batch capacity reports are not fresh",
                )
                    .into_response()
                    .into());
            }
            if let Some(desired) = &desired {
                for capacity in &mut capacities {
                    if let Some(report) = aggregated.reports.get(&capacity.node_id) {
                        capacity.allocated = capacity.allocated.saturating_add(
                            &crate::meat::admission::unreported_commitments(
                                desired,
                                &capacity.node_id,
                                report,
                            ),
                        );
                    }
                }
            }
            capacities
        } else {
            local_only_capacity(self_name)
        };
        let allocation = schedule_batch(&batch_jobs, &mut capacities);
        let mut job_records = Vec::with_capacity(jobs.len());
        for job in jobs {
            let node = allocation
                .assignments
                .iter()
                .find(|(name, _)| name == &job.name)
                .map(|(_, node)| node.clone());
            let digest = match crate::meat::batch_execution::spec_digest(
                job.namespace(),
                &job.name,
                &job.spec,
            ) {
                Ok(digest) => digest,
                Err(error) => {
                    return Err((StatusCode::BAD_REQUEST, error.to_string())
                        .into_response()
                        .into());
                }
            };
            job_records.push(BatchJobRecord {
                resources: crate::meat::admission::job_requests(&job.spec),
                name: job.name.clone(),
                execution_name: executions[&job.name].clone(),
                spec_digest: digest,
                namespace: job.namespace().to_string(),
                status: if node.is_some() {
                    JobStatus::Pending
                } else {
                    JobStatus::Unschedulable
                },
                node,
            });
        }
        let record = BatchRecord {
            jobs: job_records,
            submitted_at_epoch_secs: epoch_now_secs(),
        };
        match register_batch(state, record, expected_log_id).await {
            Ok(batch_id) => return Ok((batch_id, allocation)),
            Err(error) if error == "admission revision changed" => continue,
            Err(error) => return Err((StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({"error":format!("cluster batch tracker unavailable: {error}")}))).into_response().into()),
        }
    }
    Err((
        StatusCode::SERVICE_UNAVAILABLE,
        "cluster batch admission remained busy",
    )
        .into_response()
        .into())
}

/// `POST /v1/batch` — leader-forwarded submission.
pub async fn batch_submit_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> Response {
    // Submitting work is a Deployer action (AUTH2 — it used to take no auth).
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    let request: BatchSubmitRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid batch request: {e}") })),
            )
                .into_response();
        }
    };
    if request.jobs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "batch has no jobs" })),
        )
            .into_response();
    }
    // One namespace per job, resolved here and used everywhere (JOB3).
    let mut jobs = match resolve_job_namespaces(request.jobs) {
        Ok(jobs) => jobs,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response();
        }
    };
    if let Err((status, error)) = validate_batch_jobs(&state, &jobs, &headers).await {
        return (status, Json(serde_json::json!({"error": error}))).into_response();
    }
    let permissions = super::api::permission_map(&state).await;
    for job in &jobs {
        if let Err(response) = crate::sesame::auth::authorize_workload(
            auth.as_deref(),
            &job.name,
            job.namespace(),
            job.spec.exec.is_some() || job.spec.script.is_some(),
            &permissions,
        ) {
            return response;
        }
    }
    // Preserve the caller on the second hop so the leader repeats admission
    // against its authoritative grants, without granting system authority.
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_to_leader(&state, council, "/v1/batch", body, &headers).await;
    }
    // Stable input order: together with the scheduler's ordered
    // profile groups this pins the assignment plan (the old
    // allocation-order finding).
    jobs.sort_by(|a, b| a.name.cmp(&b.name));

    let self_name = state
        .node_name
        .clone()
        .unwrap_or_else(|| "local".to_string());

    let executions: BTreeMap<String, String> = jobs
        .iter()
        .map(|job| {
            (
                job.name.clone(),
                format!("batch-{:032x}", rand::random::<u128>()),
            )
        })
        .collect();
    // Replan after a conflicting committed entry, with a finite retry budget.
    // Dispatch starts only after the original snapshot wins the shared CAS.
    let (batch_id, allocation) = match tokio::time::timeout(
        std::time::Duration::from_secs(5), plan_batch_admission(&state, &jobs, &executions, &self_name),
    ).await {
        Ok(Ok(admission)) => admission,
        Ok(Err(response)) => return *response,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "cluster batch admission timed out; outcome unknown; any late allocation stays reserved").into_response(),
    };

    // Group assignments by node and dispatch. A BTreeMap so dispatch
    // order is deterministic too.
    let mut by_node: BTreeMap<NodeId, Vec<BatchJobSubmission>> = BTreeMap::new();
    for (job_name, node_id) in &allocation.assignments {
        if let Some(submission) = jobs.iter().find(|j| &j.name == job_name) {
            by_node
                .entry(node_id.clone())
                .or_default()
                .push(BatchJobSubmission {
                    name: executions[&submission.name].clone(),
                    ..submission.clone()
                });
        }
    }

    let all_labels: BTreeMap<String, BatchExecutionLabel> = jobs
        .iter()
        .map(|job| {
            (
                executions[&job.name].clone(),
                BatchExecutionLabel {
                    name: job.name.clone(),
                    namespace: job.namespace().to_string(),
                },
            )
        })
        .collect();
    let callback_base_url = self_callback_url(&state, &self_name).await;
    for (node_id, node_jobs) in by_node {
        let execution_labels = node_jobs
            .iter()
            .map(|job| (job.name.clone(), all_labels[&job.name].clone()))
            .collect();
        if node_id.0 == self_name {
            if let Err(response) = admit_jobs_and_watch(
                &state,
                batch_id,
                node_jobs,
                execution_labels,
                Reporter::Leader(Box::new(state.clone())),
            )
            .await
            {
                return response;
            }
            continue;
        }
        let Some(url) = node_api_url(&state, &node_id).await else {
            eprintln!(
                "bun: batch {batch_id}: no address for {node_id:?}; execution remains unknown"
            );
            continue;
        };
        let run = BatchRunRequest {
            batch_id,
            callback_base_url: callback_base_url.clone(),
            jobs: node_jobs,
            execution_labels,
        };
        let dispatch_state = state.clone();
        tokio::spawn(async move {
            let client = dispatch_state.cluster_http.client().clone();
            for attempt in 0..DISPATCH_ATTEMPTS {
                let mut request = client.post(format!("{url}/v1/batch/run")).json(&run);
                if let Some(token) = &dispatch_state.service_token {
                    request = request.bearer_auth(token);
                }
                match request.send().await {
                    Ok(response) if response.status().is_success() => return,
                    Ok(response) => eprintln!(
                        "bun: batch dispatch to {url}: {} (attempt {})",
                        response.status(),
                        attempt + 1
                    ),
                    Err(error) => eprintln!("bun: batch dispatch to {url} failed: {error}"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(
                    500 * u64::from(attempt + 1),
                ))
                .await;
            }
            eprintln!(
                "bun: batch {batch_id}: dispatch exhausted; original execution remains unknown"
            );
        });
    }

    // The pull backstop: polls assigned nodes so a lost callback can
    // never strand the batch.
    spawn_batch_watcher(&state, batch_id);

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!(BatchSubmitResponse {
            executions,
            batch_id,
            assigned: allocation.assignments.len(),
            unschedulable: allocation.unschedulable,
        })),
    )
        .into_response()
}

/// `POST /v1/batch/run` — a node receives its share of a batch.
pub async fn batch_run_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    headers: axum::http::HeaderMap,
    Json(run): Json<BatchRunRequest>,
) -> Response {
    // Node-to-node only: reject anything that isn't the system principal
    // (JOB1). Without this a ReadOnly or bootstrap-window caller could run
    // arbitrary jobs and — via the callback below — steal the service token.
    if let Err(resp) = crate::sesame::auth::require_system(auth.as_deref()) {
        return resp;
    }
    let reporter = match run.callback_base_url {
        Some(base_url) => {
            // Only send our service token to a callback that is a real cluster
            // member (defence in depth against a compromised node picking an
            // attacker URL).
            if !callback_is_allowed(&state, &base_url).await {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "callback_base_url is not a known cluster member"
                    })),
                )
                    .into_response();
            }
            Reporter::Callback {
                base_url,
                client: state.cluster_http.client().clone(),
                service_token: state.service_token.clone(),
            }
        }
        // No callback: the submitter is this process (or doesn't care).
        None => Reporter::Leader(Box::new(state.clone())),
    };
    // Dispatched job specs resolve their namespaces the same way the
    // submit path does, so the deploy and the watch agree (JOB3).
    let jobs = match resolve_job_namespaces(run.jobs) {
        Ok(jobs) => jobs,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response();
        }
    };
    if let Err((status, error)) = validate_batch_jobs(&state, &jobs, &headers).await {
        return (status, Json(serde_json::json!({"error": error}))).into_response();
    }
    if run.batch_id == 0
        || run.execution_labels.len() != jobs.len()
        || jobs.iter().any(|job| {
            run.execution_labels.get(&job.name).is_none_or(|label| {
                label.namespace != job.namespace()
                    || !crate::config::valid_workload_label(&label.name)
            })
        })
    {
        return (StatusCode::BAD_REQUEST, "execution_labels must exactly match configured executions with valid labels in the same namespace").into_response();
    }
    if let Some(council) = &state.council {
        let allocation = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let local = super::api::ask_agent_bounded(&state.cmd_tx, |response| {
            AgentCommand::BatchOwnedExecutions {
                identities: jobs
                    .iter()
                    .map(|job| (job.name.clone(), job.namespace().to_string()))
                    .collect(),
                response,
            }
        })
        .await;
        let owned = match local {
            Ok(owned) => owned,
            Err(response) => return Err(response),
        };
        let desired = council.desired_state().await;
        for job in &jobs {
            let id =
                crate::grill::InstanceIdentity::new(job.namespace(), &job.name, 0).instance_id();
            if owned.contains(&id.0) {
                continue;
            }
            let Some(owner) = desired
                .batch_state
                .execution_owner(job.namespace(), &job.name)
            else {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "committed batch allocation is not available for this new execution",
                )
                    .into_response());
            };
            let label = &run.execution_labels[&job.name];
            let digest = match crate::meat::batch_execution::spec_digest(
                job.namespace(),
                &label.name,
                &job.spec,
            ) {
                Ok(digest) => digest,
                Err(error) => return Err((StatusCode::BAD_REQUEST, error.to_string()).into_response()),
            };
            let allocated = desired.batch_state.get(run.batch_id).is_some_and(|record| {
                record.jobs.iter().any(|recorded| {
                    recorded.execution_name == job.name
                        && recorded.namespace == job.namespace()
                        && recorded.name == label.name
                        && recorded.spec_digest == digest
                        && !recorded.status.is_terminal()
                        && recorded.node.as_ref().is_some_and(|node| !desired.security_state.crl.retired_nodes.contains_key(&node.0))
                        && recorded
                            .node
                            .as_ref()
                            .is_some_and(|node| state.node_name.as_deref() == Some(node.0.as_str()))
                })
            });
            if owner.batch_id != run.batch_id
                || owner.logical_name != label.name
                || owner.spec_digest != digest
                || !allocated
            {
                return Err((StatusCode::CONFLICT, "new execution does not match its live committed batch allocation and original specification").into_response());
            }
        }
        Ok(())
        }).await;
        match allocation {
            Ok(Ok(())) => {}
            Ok(Err(response)) => return response,
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "batch allocation metadata unavailable",
                )
                    .into_response();
            }
        }
    }
    if let Err(response) =
        admit_jobs_and_watch(&state, run.batch_id, jobs, run.execution_labels, reporter).await
    {
        return response;
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "accepted": true })),
    )
        .into_response()
}

/// `POST /v1/batch/{id}/report` — a completion callback. Validated
/// (JOB3): unknown batches/jobs are 404, forged states and illegal
/// transitions are 409, duplicate terminal reports are an idempotent
/// 200. Forwarded to the leader when this node is a follower, so
/// reports survive a leadership change mid-batch.
pub async fn batch_report_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath(batch_id): AxumPath<u64>,
    Json(report): Json<BatchReportRequest>,
) -> Response {
    // Node-to-node only (JOB1): a forged report from an untrusted caller must
    // not be able to mark batch jobs completed/failed.
    if let Err(resp) = crate::sesame::auth::require_system(auth.as_deref()) {
        return resp;
    }
    let Some(status) = status_from_wire(&report.status) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("unknown report status {:?}", report.status)
            })),
        )
            .into_response();
    };
    match report_batch_job(
        &state,
        batch_id,
        &report.job_name,
        &report.namespace,
        status,
        report.exit_code,
    )
    .await
    {
        Ok(outcome) => Json(serde_json::json!({
            "recorded": true,
            "duplicate": outcome == ReportOutcome::Duplicate,
        }))
        .into_response(),
        Err(BatchRejection::NotFound(reason)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
        Err(BatchRejection::Conflict(reason)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
        Err(BatchRejection::Unavailable(reason)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
    }
}

/// `GET /v1/batch/{id}` — the tracker's summary (leader-forwarded).
/// Reading a non-terminal batch with no live watcher spawns one: this
/// is how a restarted leader resumes watching in-flight batches (JOB4).
pub async fn batch_status_handler(
    State(state): State<ApiState>,
    AxumPath(batch_id): AxumPath<u64>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_get_to_leader(&state, council, &format!("/v1/batch/{batch_id}")).await;
    }

    match get_batch(&state, batch_id).await {
        Some(record) => {
            if !record.is_terminal() {
                spawn_batch_watcher(&state, batch_id);
            }
            Json(serde_json::json!(
                record.summary(batch_id, epoch_now_secs())
            ))
            .into_response()
        }
        None => {
            // Task arrays share the id space; the summary says `"kind": "array"`.
            let arrays = super::task_array_leader::read_task_arrays(&state).await;
            if let Some(record) = arrays.get(batch_id) {
                if let Err(response) = crate::sesame::auth::authorize_scoped(
                    auth.as_deref(),
                    &record.name,
                    &record.namespace,
                ) {
                    return response;
                }
                let nodes = state.task_arrays.node_views(batch_id).await;
                let progress = record.state.summary();
                let mut summary = super::task_array_api::array_summary(batch_id, record, &nodes);
                summary["rates"] = serde_json::to_value(
                    state
                        .task_arrays
                        .rates(batch_id, (progress.succeeded, progress.failed))
                        .await,
                )
                .expect("finite rates");
                return Json(summary).into_response();
            }
            if let Some(manifest) = arrays.manifest(batch_id) {
                if let Err(response) = crate::sesame::auth::authorize_scoped(
                    auth.as_deref(),
                    &manifest.name,
                    &manifest.namespace,
                ) {
                    return response;
                }
                return Json(
                    super::task_array_api::manifest_summary(
                        batch_id,
                        manifest,
                        &arrays,
                        &state.task_arrays,
                    )
                    .await,
                )
                .into_response();
            }
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("batch {batch_id} not found") })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Leader forwarding + addressing helpers
// ---------------------------------------------------------------------------

pub(crate) async fn forward_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    path: &str,
    body: String,
    headers: &axum::http::HeaderMap,
) -> Response {
    let Some(leader_url) = super::api::leader_api_url(state, council).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no cluster leader known yet; retry shortly" })),
        )
            .into_response();
    };
    let request = state
        .cluster_http
        .client()
        .post(format!("{leader_url}{path}"))
        .header("content-type", "application/json")
        .body(body);
    let request = super::api::copy_forwarded_auth(request, headers);
    proxy_response(request.send().await).await
}

pub(crate) async fn forward_get_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    path: &str,
) -> Response {
    let Some(leader_url) = super::api::leader_api_url(state, council).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no cluster leader known yet; retry shortly" })),
        )
            .into_response();
    };
    let mut request = state
        .cluster_http
        .client()
        .get(format!("{leader_url}{path}"));
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    proxy_response(request.send().await).await
}

async fn proxy_response(result: Result<reqwest::Response, reqwest::Error>) -> Response {
    match result {
        Ok(response) => {
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = response.bytes().await.unwrap_or_default();
            (status, body).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": format!("leader forward failed: {e}") })),
        )
            .into_response(),
    }
}

/// Whether it is safe to send our service token to `base_url` as a batch
/// callback. When a membership table exists, the URL must match a known
/// member (keeps the token from going to an attacker-chosen callback if a
/// node is compromised). With no membership table (standalone / single-node),
/// there are no peers to validate against, so the `require_system` gate on the
/// caller is the guarantee and any callback is accepted.
async fn callback_is_allowed(state: &ApiState, base_url: &str) -> bool {
    let Some(membership) = state.membership.as_ref() else {
        return true;
    };
    let base = base_url.trim_end_matches('/');
    membership
        .read()
        .await
        .iter()
        .any(|m| state.cluster_http.url(&m.address.to_string(), "") == base)
}

/// The API URL peers use to reach a node, from the membership table.
async fn node_api_url(state: &ApiState, node_id: &NodeId) -> Option<String> {
    let membership = state.membership.as_ref()?;
    let members = membership.read().await;
    members
        .iter()
        .find(|m| &m.node_id == node_id)
        .map(|m| state.cluster_http.url(&m.address.to_string(), ""))
}

/// Our own reachable base URL, for completion callbacks. `None` when
/// standalone (local shares report in-process anyway).
async fn self_callback_url(state: &ApiState, self_name: &str) -> Option<String> {
    node_api_url(state, &NodeId(self_name.to_string())).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::types::StateReport;

    #[test]
    fn submit_request_serde_round_trip() {
        let request = BatchSubmitRequest {
            jobs: vec![BatchJobSubmission {
                name: "render".to_string(),
                namespace: Some("default".to_string()),
                spec: toml::from_str(r#"command = ["true"]"#).unwrap(),
            }],
        };
        let json = serde_json::to_string(&request).unwrap();
        let back: BatchSubmitRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.jobs.len(), 1);
        assert_eq!(back.jobs[0].name, "render");
    }

    #[test]
    fn submission_namespace_defaults() {
        let json = r#"{ "name": "j", "spec": { "command": ["true"] } }"#;
        let submission: BatchJobSubmission = serde_json::from_str(json).unwrap();
        assert_eq!(submission.namespace(), "default");
    }

    fn submission(name: &str, namespace: Option<&str>, spec_toml: &str) -> BatchJobSubmission {
        BatchJobSubmission {
            name: name.to_string(),
            namespace: namespace.map(str::to_string),
            spec: toml::from_str(spec_toml).unwrap(),
        }
    }

    #[test]
    fn namespace_resolution_prefers_the_single_source() {
        // Only the spec names one → it wins everywhere.
        let jobs = resolve_job_namespaces(vec![submission(
            "a",
            None,
            r#"command = ["true"]
               namespace = "prod""#,
        )])
        .unwrap();
        assert_eq!(jobs[0].namespace(), "prod");
        assert_eq!(jobs[0].spec.namespace.as_deref(), Some("prod"));

        // Only the submission names one → written into the spec too,
        // so the deploy and the watcher agree (JOB3).
        let jobs = resolve_job_namespaces(vec![submission(
            "a",
            Some("batchns"),
            r#"command = ["true"]"#,
        )])
        .unwrap();
        assert_eq!(jobs[0].namespace(), "batchns");
        assert_eq!(jobs[0].spec.namespace.as_deref(), Some("batchns"));

        // Neither → default.
        let jobs =
            resolve_job_namespaces(vec![submission("a", None, r#"command = ["true"]"#)]).unwrap();
        assert_eq!(jobs[0].namespace(), "default");
    }

    #[test]
    fn conflicting_namespaces_are_rejected() {
        let err = resolve_job_namespaces(vec![submission(
            "a",
            Some("one"),
            r#"command = ["true"]
               namespace = "two""#,
        )])
        .unwrap_err();
        assert!(err.contains("two namespaces"), "{err}");
    }

    #[tokio::test]
    async fn internal_batch_dispatch_validates_specs_and_refuses_unowned_test_resources() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let router = crate::bun::api::router(
            tx, None, None, None, None, None, None, None, None, None, None, None, 0, None,
        )
        .layer(axum::Extension(crate::sesame::auth::system_context()));
        let cases = [
            ("default", serde_json::json!({}), StatusCode::BAD_REQUEST),
            (
                "default",
                serde_json::json!({"image": "busybox", "exec": "true"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                "default",
                serde_json::json!({"exec": "true", "script": "true"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                "rbtest-batch",
                serde_json::json!({"image": "busybox"}),
                StatusCode::CONFLICT,
            ),
            (
                "default",
                serde_json::json!({"image": "localhost:5050/rbtest-image/work:test"}),
                StatusCode::CONFLICT,
            ),
        ];
        for (namespace, spec, expected) in cases {
            let request = Request::post("/v1/batch/run")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"batch_id": 1, "jobs": [
                        {"name": "migration", "namespace": namespace, "spec": spec}
                    ]})
                    .to_string(),
                ))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                expected,
                "namespace={namespace}, spec={spec}"
            );
            assert!(matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        }
    }

    #[test]
    fn duplicate_names_are_refused_even_across_namespaces() {
        for other_namespace in ["one", "two"] {
            let err = resolve_job_namespaces(vec![
                submission("same", Some("one"), r#"command = ["true"]"#),
                submission("same", Some(other_namespace), r#"command = ["false"]"#),
            ])
            .expect_err("duplicate batch identity must be refused before dispatch");
            assert!(err.contains("duplicate"), "{err}");
        }
    }

    #[tokio::test]
    async fn duplicate_submission_never_registers_or_dispatches_jobs() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let router = crate::bun::api::router(
            tx, None, None, None, None, None, None, None, None, None, None, None, 0, None,
        );
        for namespace in ["one", "two"] {
            let request = Request::post("/v1/batch")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"jobs": [
                    {"name": "same", "namespace": "one", "spec": {"image": "busybox", "command": ["true"]}},
                    {"name": "same", "namespace": namespace, "spec": {"image": "busybox", "command": ["false"]}}
                ]}).to_string()))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
            let response = router
                .clone()
                .oneshot(Request::get("/v1/batch/1").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn duplicate_internal_dispatch_never_launches_jobs() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let router = crate::bun::api::router(
            tx, None, None, None, None, None, None, None, None, None, None, None, 0, None,
        )
        .layer(axum::Extension(crate::sesame::auth::system_context()));
        for namespace in ["one", "two"] {
            let request = Request::post("/v1/batch/run")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"batch_id": 1, "jobs": [
                    {"name": "same", "namespace": "one", "spec": {"image": "busybox", "command": ["true"]}},
                    {"name": "same", "namespace": namespace, "spec": {"image": "busybox", "command": ["false"]}}
                ]}).to_string()))
                .unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ));
        }
    }

    #[test]
    fn agreeing_namespaces_are_accepted() {
        let jobs = resolve_job_namespaces(vec![submission(
            "a",
            Some("prod"),
            r#"command = ["true"]
               namespace = "prod""#,
        )])
        .unwrap();
        assert_eq!(jobs[0].namespace(), "prod");
    }

    fn report_with_usage(
        node: &crate::meat::NodeId,
        usage: crate::reporting::types::ResourceUsage,
    ) -> StateReport {
        StateReport {
            has_buildah: false,
            node_id: node.clone(),
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: usage,
            event_log: vec![],
        }
    }

    #[tokio::test]
    async fn a_stalled_actual_council_admission_returns_unknown_without_dispatch() {
        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::{CouncilConfig, CouncilNode, CouncilNodeInfo};
        use std::sync::Arc;
        use tower::ServiceExt;
        let network = InMemoryRaftRouter::new();
        let council = Arc::new(
            CouncilNode::new(
                1,
                CouncilConfig {
                    heartbeat_interval_ms: 50,
                    election_timeout_min_ms: 150,
                    election_timeout_max_ms: 400,
                    snapshot_threshold: 100,
                    max_in_snapshot_log_to_keep: 50,
                },
                InMemoryRaftNetworkFactory::new(1, network.clone()),
                MemLogStore::new(),
                CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap(),
        );
        network.register(1, council.raft().clone()).await;
        council
            .initialize(BTreeMap::from([(
                1,
                CouncilNodeInfo::new("127.0.0.1:9100".parse().unwrap(), "leader"),
            )]))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !council.is_leader().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let node = crate::meat::NodeId::new("worker");
        let mut aggregated = AggregatedState {
            leadership_epoch: Some(council.current_term()),
            ..Default::default()
        };
        aggregated.receive_deadlines.insert(
            node.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        aggregated.reports.insert(
            node.clone(),
            report_with_usage(
                &node,
                crate::reporting::types::ResourceUsage {
                    cpu_total_millicores: 8000,
                    memory_total_mb: 16384,
                    ..Default::default()
                },
            ),
        );
        let (_publisher, reports) = tokio::sync::watch::channel(aggregated);
        let (commands, mut launches) = tokio::sync::mpsc::channel(16);
        let router = crate::bun::api::router_with_upgrade(
            commands,
            None,
            None,
            None,
            None,
            None,
            Some(council.clone()),
            None,
            Some("service-token".into()),
            None,
            Some(Arc::new(tokio::sync::RwLock::new(vec![
                NodeMembershipInfo {
                    node_id: node,
                    address: "127.0.0.1:9101".parse().unwrap(),
                    api_advertised: true,
                },
            ]))),
            None,
            None,
            9117,
            None,
            None,
            Some(reports),
            "default".into(),
            Some("leader".into()),
            crate::bun::build_runner::BuildSettings::with_timeout(900),
            crate::cluster::ClusterHttp::plaintext(),
            5050,
            "http",
            256 * 1024 * 1024,
            false,
            crate::bun::capabilities::StaticCapabilities::default(),
            crate::bun::readiness::ReadinessTracker::new(),
            None,
            None,
            None,
            None,
        )
        .layer(axum::Extension(crate::sesame::auth::system_context()));
        council.hang_writes();
        let response = tokio::time::timeout(std::time::Duration::from_secs(7), router.oneshot(
            axum::http::Request::post("/v1/batch").header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"jobs":[{"name":"held","spec":{"image":"proc-grill:ignored","command":["true"]}}]}"#)).unwrap()
        )).await;
        let records = council.desired_state().await.batch_state.batches.len();
        council.shutdown().await.unwrap();
        assert!(
            launches.try_recv().is_err(),
            "a timed-out admission dispatched worker commands"
        );
        assert_eq!(records, 0);
        assert_eq!(
            response
                .expect("the actual Council hang escaped the admission deadline")
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_report_published_before_its_receive_deadline_expires_between_ticks() {
        use crate::reporting::transport::{InMemoryReportingNetwork, ReportingTransport};
        let network = InMemoryReportingNetwork::new();
        let address = "127.0.0.1:9120".parse().unwrap();
        let transport = network.register(address).await;
        let worker = network.register("127.0.0.1:9121".parse().unwrap()).await;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (mut aggregator, snapshot) = crate::reporting::aggregator::ReportAggregator::new(
            transport,
            crate::config::node::ReportingTreeSection {
                stale_report_timeout_secs: 30,
                ..Default::default()
            },
            shutdown.clone(),
            None,
            None,
            None,
        );
        use std::future::Future;
        use std::task::{Context, Poll};
        let mut run = Box::pin(aggregator.run());
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(run.as_mut().poll(&mut context), Poll::Pending));
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        assert!(matches!(run.as_mut().poll(&mut context), Poll::Pending));
        let node = crate::meat::NodeId::new("worker");
        worker
            .send(
                address,
                &crate::reporting::types::ReportingMessage::Report(report_with_usage(
                    &node,
                    crate::reporting::types::ResourceUsage {
                        cpu_total_millicores: 8000,
                        memory_total_mb: 16384,
                        ..Default::default()
                    },
                )),
            )
            .await
            .unwrap();
        assert!(matches!(run.as_mut().poll(&mut context), Poll::Pending));
        assert!(snapshot.borrow().reports.contains_key(&node));
        tokio::time::advance(std::time::Duration::from_secs(29)).await;
        assert!(matches!(run.as_mut().poll(&mut context), Poll::Pending));
        assert!(snapshot.borrow().stale_nodes.is_empty());
        tokio::time::advance(std::time::Duration::from_secs(2)).await;
        let cached = snapshot.borrow().clone();
        assert_eq!(cached.reports.len(), 1);
        assert!(
            cached.stale_nodes.is_empty(),
            "periodic publication must not have marked this control stale yet"
        );
        let capacities = capacities_from_reports(
            &[NodeMembershipInfo {
                node_id: node,
                address,
                api_advertised: true,
            }],
            &cached,
        );
        shutdown.cancel();
        assert!(matches!(run.as_mut().poll(&mut context), Poll::Ready(())));
        assert!(
            capacities.is_empty(),
            "a cached snapshot outlived its receive-time deadline"
        );
    }

    #[test]
    fn capacities_map_commitments_not_usage() {
        let node = crate::meat::NodeId("worker-1".to_string());
        let usage = crate::reporting::types::ResourceUsage {
            cpu_total_millicores: 8000,
            cpu_used_millicores: 2000,
            memory_total_mb: 16384,
            memory_used_mb: 4096,
            ..Default::default()
        };

        let mut aggregated = AggregatedState::default();
        aggregated.receive_deadlines.insert(
            node.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        aggregated
            .reports
            .insert(node.clone(), report_with_usage(&node, usage));

        let members = vec![NodeMembershipInfo {
            node_id: node,
            address: std::net::SocketAddr::from(([10, 0, 0, 1], 9117)),
            api_advertised: true,
        }];
        let capacities = capacities_from_reports(&members, &aggregated);

        assert_eq!(capacities.len(), 1);
        assert_eq!(capacities[0].total.cpu_millicores, 8000);
        assert_eq!(capacities[0].allocated.memory_bytes, 4096 * 1024 * 1024);
    }

    #[test]
    fn capacities_skip_pre_capacity_nodes() {
        let node = crate::meat::NodeId("fresh".to_string());
        let mut aggregated = AggregatedState::default();
        aggregated.receive_deadlines.insert(
            node.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        aggregated
            .reports
            .insert(node.clone(), report_with_usage(&node, Default::default()));

        let members = vec![NodeMembershipInfo {
            node_id: node,
            address: std::net::SocketAddr::from(([10, 0, 0, 2], 9117)),
            api_advertised: true,
        }];
        assert!(capacities_from_reports(&members, &aggregated).is_empty());
    }

    #[test]
    fn stale_reports_never_offer_batch_capacity() {
        let node = crate::meat::NodeId("stale".into());
        let mut aggregated = AggregatedState::default();
        aggregated.receive_deadlines.insert(
            node.clone(),
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        );
        aggregated.reports.insert(
            node.clone(),
            report_with_usage(
                &node,
                crate::reporting::types::ResourceUsage {
                    cpu_total_millicores: 8000,
                    memory_total_mb: 16384,
                    ..Default::default()
                },
            ),
        );
        aggregated.stale_nodes.push(node.clone());
        let members = vec![NodeMembershipInfo {
            node_id: node,
            address: "127.0.0.1:9117".parse().unwrap(),
            api_advertised: true,
        }];
        assert!(capacities_from_reports(&members, &aggregated).is_empty());
    }

    #[test]
    fn local_capacity_schedules_everything() {
        let mut capacities = local_only_capacity("local");
        let jobs: Vec<BatchJob> = (0..100)
            .map(|i| BatchJob {
                name: format!("job-{i}"),
                resources: Resources::new(100, 1024 * 1024, 0),
            })
            .collect();
        let allocation = schedule_batch(&jobs, &mut capacities);
        assert_eq!(allocation.assignments.len(), 100);
        assert!(allocation.unschedulable.is_empty());
    }

    #[test]
    fn job_outcome_maps_terminal_states() {
        let status = |state: &str, exit: Option<i32>| InstanceStatus {
            id: crate::grill::InstanceIdentity::new("default", "j", 0)
                .instance_id()
                .0,
            app_name: "j".to_string(),
            namespace: "default".to_string(),
            state: state.to_string(),
            restart_count: 0,
            host_port: None,
            exit_code: exit,
            pid: None,
            runtime_unknown: false,
            status_age_ms: None,
        };
        assert_eq!(
            job_outcome(&[status("stopped", Some(0))], "j", "default"),
            Some(0)
        );
        assert_eq!(
            job_outcome(&[status("stopped", None)], "j", "default"),
            None
        );
        assert_eq!(job_outcome(&[status("failed", None)], "j", "default"), None);
        assert_eq!(
            job_outcome(&[status("failed", Some(1))], "j", "default"),
            Some(1)
        );
        // Non-zero stop is retry backoff, not terminal.
        assert_eq!(
            job_outcome(&[status("stopped", Some(1))], "j", "default"),
            None
        );
        // Wrong namespace never matches (JOB3).
        assert_eq!(
            job_outcome(&[status("stopped", Some(0))], "j", "prod"),
            None
        );
        assert_eq!(job_outcome(&[], "j", "default"), None);
    }
}
