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
    pub batch_id: u64,
    pub assigned: usize,
    pub unschedulable: Vec<String>,
}

/// Node-to-node dispatch: run these jobs, report to the callback.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchRunRequest {
    pub batch_id: u64,
    /// Base URL of the submitting (leader) node for completion
    /// reports; `None` when the leader runs its own share in-process.
    pub callback_base_url: Option<String>,
    pub jobs: Vec<BatchJobSubmission>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchReportRequest {
    pub job_name: String,
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
    for job in &mut jobs {
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
        ReportError::IllegalTransition { .. } | ReportError::NotReportable { .. } => {
            BatchRejection::Conflict(error.to_string())
        }
    }
}

/// Register a batch record durably: through Raft when a council
/// exists (the id comes from the replicated counter, JOB4), through
/// the in-memory tracker standalone.
pub(crate) async fn register_batch(state: &ApiState, record: BatchRecord) -> Result<u64, String> {
    match &state.council {
        Some(council) => match council
            .write(crate::council::types::RaftRequest::BatchRegister { batch: record })
            .await
        {
            Ok(crate::council::types::CouncilResponse::BatchRegistered { batch_id }) => {
                Ok(batch_id)
            }
            Ok(other) => Err(format!("unexpected raft response: {other:?}")),
            Err(e) => Err(format!("raft register failed: {e}")),
        },
        None => Ok(state.batch_tracker.lock().await.register(record).0),
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
    status: JobStatus,
) -> Result<ReportOutcome, BatchRejection> {
    let Some(council) = &state.council else {
        return state
            .batch_tracker
            .lock()
            .await
            .report(batch_id, job_name, status)
            .map_err(map_report_error);
    };

    if !council.is_leader().await {
        return forward_report_to_leader(state, council, batch_id, job_name, status).await;
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
    match probe.report(job_name, status) {
        Err(e) => return Err(map_report_error(e)),
        Ok(ReportOutcome::Duplicate) => return Ok(ReportOutcome::Duplicate),
        Ok(ReportOutcome::Applied) => {}
    }
    match council
        .write(crate::council::types::RaftRequest::BatchJobUpdate {
            batch_id,
            job_name: job_name.to_string(),
            status,
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
    status: JobStatus,
) -> Result<ReportOutcome, BatchRejection> {
    let Some(leader_url) = super::api::leader_api_url(state, council).await else {
        return Err(BatchRejection::Unavailable(
            "no cluster leader known yet".to_string(),
        ));
    };
    let body = BatchReportRequest {
        job_name: job_name.to_string(),
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
    async fn report(&self, batch_id: u64, job_name: &str, completed: bool) {
        let status = if completed {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
        match self {
            Reporter::Leader(state) => {
                if let Err(e) = report_batch_job(state, batch_id, job_name, status).await {
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

/// Map instance statuses to a job outcome: `Some(true)` completed,
/// `Some(false)` failed, `None` still running/unknown.
///
/// `stopped` alone is ambiguous: a failing job passes through it
/// between retries (any exit maps to Stopped; the code is tracked
/// separately). Success is stopped with exit 0; a non-zero stop is
/// backoff, not terminal — the agent marks the instance `failed` once
/// retries exhaust. Runtimes without exit codes (runc, review H13)
/// report `None`: treat their stops as success rather than hanging.
fn job_outcome(statuses: &[InstanceStatus], name: &str, namespace: &str) -> Option<bool> {
    for status in statuses
        .iter()
        .filter(|s| s.app_name == name && s.namespace == namespace)
    {
        let outcome = match (status.state.as_str(), status.exit_code) {
            ("failed", _) => Some(false),
            ("stopped", Some(0) | None) => Some(true),
            _ => None,
        };
        if outcome.is_some() {
            return outcome;
        }
    }
    None
}

/// Deploy this node's share of a batch and watch each job to a
/// terminal state, reporting as they finish. Spawned; never blocks a
/// handler.
pub async fn run_jobs_and_watch(
    cmd_tx: mpsc::Sender<AgentCommand>,
    batch_id: u64,
    jobs: Vec<BatchJobSubmission>,
    reporter: Reporter,
) {
    // Synthesise a Config holding only these jobs and deploy it
    // through the normal path (retries, init, records — all standard).
    let mut config = match Config::parse("") {
        Ok(config) => config,
        Err(e) => {
            eprintln!("bun: batch config synthesis failed: {e}");
            for job in &jobs {
                reporter.report(batch_id, &job.name, false).await;
            }
            return;
        }
    };
    for job in &jobs {
        config.job.insert(job.name.clone(), job.spec.clone());
    }

    let (event_tx, mut event_rx) = mpsc::channel(64);
    if cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: event_tx,
        })
        .await
        .is_err()
    {
        for job in &jobs {
            reporter.report(batch_id, &job.name, false).await;
        }
        return;
    }
    // Drain deploy events; a deploy-level error fails the whole share.
    let mut deploy_failed = false;
    while let Some(event) = event_rx.recv().await {
        if matches!(event, crate::bun::agent::ApplyEvent::Error { .. }) {
            deploy_failed = true;
        }
    }
    if deploy_failed {
        for job in &jobs {
            reporter.report(batch_id, &job.name, false).await;
        }
        return;
    }

    // Watch each job to a terminal state.
    let mut pending: Vec<&BatchJobSubmission> = jobs.iter().collect();
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
            break;
        }
        let Ok(statuses) = status_rx.await else { break };

        let mut still_pending = Vec::new();
        for job in pending {
            match job_outcome(&statuses, &job.name, job.namespace()) {
                Some(completed) => reporter.report(batch_id, &job.name, completed).await,
                None => still_pending.push(job),
            }
        }
        pending = still_pending;
    }
    // Anything still pending at the deadline is reported failed — the
    // tracker must reach a terminal count, not hang forever.
    for job in pending {
        reporter.report(batch_id, &job.name, false).await;
    }
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
            for job in record.jobs.iter().filter(|j| !j.status.is_terminal()) {
                let _ = report_batch_job(state, batch_id, &job.name, JobStatus::Failed).await;
            }
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
                    .and_then(|statuses| job_outcome(statuses, &job.name, &job.namespace))
            } else {
                fetch_remote_outcome(state, node, &job.name, &job.namespace).await
            };
            if let Some(completed) = outcome {
                let status = if completed {
                    JobStatus::Completed
                } else {
                    JobStatus::Failed
                };
                let _ = report_batch_job(state, batch_id, &job.name, status).await;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(PULL_INTERVAL_MS)).await;
    }
}

async fn fetch_local_statuses(state: &ApiState) -> Option<Vec<InstanceStatus>> {
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
) -> Option<bool> {
    let url = node_api_url(state, node).await?;
    let mut request = state
        .cluster_http
        .client()
        .get(format!("{url}/v1/status/{job_name}/{namespace}"));
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

/// `POST /v1/batch` — leader-forwarded submission.
pub async fn batch_submit_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    body: String,
) -> Response {
    // Submitting work is a Deployer action (AUTH2 — it used to take no auth).
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    // Followers forward the raw body to the leader (the tracker and
    // the aggregated capacity view live there).
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_to_leader(&state, council, "/v1/batch", body).await;
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
    // Stable input order: together with the scheduler's ordered
    // profile groups this pins the assignment plan (the old
    // allocation-order finding).
    jobs.sort_by(|a, b| a.name.cmp(&b.name));

    let self_name = state
        .node_name
        .clone()
        .unwrap_or_else(|| "local".to_string());

    // Capacity: the aggregated reports when clustered, self otherwise.
    let mut capacities = match (&state.aggregated_rx, &state.membership) {
        (Some(aggregated_rx), Some(membership)) => {
            let members = membership.read().await.clone();
            let capacities = capacities_from_reports(&members, &aggregated_rx.borrow());
            if capacities.is_empty() {
                local_only_capacity(&self_name)
            } else {
                capacities
            }
        }
        _ => local_only_capacity(&self_name),
    };

    let batch_jobs: Vec<BatchJob> = jobs
        .iter()
        .map(|job| BatchJob {
            name: job.name.clone(),
            resources: Resources::new(
                job.spec.cpu.as_ref().map(|r| r.request).unwrap_or(0),
                job.spec.memory.as_ref().map(|r| r.request).unwrap_or(0),
                0,
            ),
        })
        .collect();

    let allocation = schedule_batch(&batch_jobs, &mut capacities);

    // The durable record includes unschedulable jobs (JOB3): they are
    // part of the batch's story, not an omission.
    let mut job_records = Vec::with_capacity(jobs.len());
    for job in &jobs {
        let node = allocation
            .assignments
            .iter()
            .find(|(name, _)| name == &job.name)
            .map(|(_, node)| node.clone());
        job_records.push(BatchJobRecord {
            name: job.name.clone(),
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
    let batch_id = match register_batch(&state, record).await {
        Ok(batch_id) => batch_id,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("cluster batch tracker unavailable: {e}")
                })),
            )
                .into_response();
        }
    };

    // Group assignments by node and dispatch. A BTreeMap so dispatch
    // order is deterministic too.
    let mut by_node: BTreeMap<NodeId, Vec<BatchJobSubmission>> = BTreeMap::new();
    for (job_name, node_id) in &allocation.assignments {
        if let Some(submission) = jobs.iter().find(|j| &j.name == job_name) {
            by_node
                .entry(node_id.clone())
                .or_default()
                .push(submission.clone());
        }
    }

    let callback_base_url = self_callback_url(&state, &self_name).await;
    for (node_id, node_jobs) in by_node {
        if node_id.0 == self_name {
            // Our own share: no HTTP, report straight into the tracker.
            tokio::spawn(run_jobs_and_watch(
                state.cmd_tx.clone(),
                batch_id,
                node_jobs,
                Reporter::Leader(Box::new(state.clone())),
            ));
            continue;
        }

        // Remote share: POST the group to the target node, with
        // bounded retries; exhausted retries fail the jobs honestly
        // instead of leaving them pending forever (JOB3).
        let Some(url) = node_api_url(&state, &node_id).await else {
            eprintln!("bun: batch {batch_id}: no address for {node_id:?}; failing its jobs");
            for job in &node_jobs {
                let _ = report_batch_job(&state, batch_id, &job.name, JobStatus::Failed).await;
            }
            continue;
        };
        let run = BatchRunRequest {
            batch_id,
            callback_base_url: callback_base_url.clone(),
            jobs: node_jobs,
        };
        let dispatch_state = state.clone();
        tokio::spawn(async move {
            let client = dispatch_state.cluster_http.client().clone();
            let token = dispatch_state.service_token.clone();
            for attempt in 0..DISPATCH_ATTEMPTS {
                let mut request = client.post(format!("{url}/v1/batch/run")).json(&run);
                if let Some(token) = &token {
                    request = request.bearer_auth(token);
                }
                match request.send().await {
                    Ok(response) if response.status().is_success() => return,
                    Ok(response) => eprintln!(
                        "bun: batch dispatch to {url}: {} (attempt {})",
                        response.status(),
                        attempt + 1
                    ),
                    Err(e) => {
                        eprintln!(
                            "bun: batch dispatch to {url} failed (attempt {}): {e}",
                            attempt + 1
                        )
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(
                    500 * u64::from(attempt + 1),
                ))
                .await;
            }
            eprintln!(
                "bun: batch {batch_id}: dispatch to {url} exhausted \
                 {DISPATCH_ATTEMPTS} attempts; failing its jobs"
            );
            for job in &run.jobs {
                let _ =
                    report_batch_job(&dispatch_state, batch_id, &job.name, JobStatus::Failed).await;
            }
        });
    }

    // The pull backstop: polls assigned nodes so a lost callback can
    // never strand the batch.
    spawn_batch_watcher(&state, batch_id);

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!(BatchSubmitResponse {
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
    tokio::spawn(run_jobs_and_watch(
        state.cmd_tx.clone(),
        run.batch_id,
        jobs,
        reporter,
    ));
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
    match report_batch_job(&state, batch_id, &report.job_name, status).await {
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
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("batch {batch_id} not found") })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Leader forwarding + addressing helpers
// ---------------------------------------------------------------------------

async fn forward_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    path: &str,
    body: String,
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
        .post(format!("{leader_url}{path}"))
        .header("content-type", "application/json")
        .body(body);
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    proxy_response(request.send().await).await
}

async fn forward_get_to_leader(
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
        aggregated
            .reports
            .insert(node.clone(), report_with_usage(&node, usage));

        let members = vec![NodeMembershipInfo {
            node_id: node,
            address: std::net::SocketAddr::from(([10, 0, 0, 1], 9117)),
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
        aggregated
            .reports
            .insert(node.clone(), report_with_usage(&node, Default::default()));

        let members = vec![NodeMembershipInfo {
            node_id: node,
            address: std::net::SocketAddr::from(([10, 0, 0, 2], 9117)),
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
            id: "i1".to_string(),
            app_name: "j".to_string(),
            namespace: "default".to_string(),
            state: state.to_string(),
            restart_count: 0,
            host_port: None,
            exit_code: exit,
            pid: None,
        };
        assert_eq!(
            job_outcome(&[status("stopped", Some(0))], "j", "default"),
            Some(true)
        );
        assert_eq!(
            job_outcome(&[status("stopped", None)], "j", "default"),
            Some(true)
        );
        assert_eq!(
            job_outcome(&[status("failed", Some(1))], "j", "default"),
            Some(false)
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
