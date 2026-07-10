//! Batch job submission, dispatch, and tracking (Phase 12, F1).
//!
//! The flow: `POST /v1/batch` (leader-forwarded) carries full job
//! specs; the leader maps the reporting pipeline's `AggregatedState`
//! into scheduler capacities, runs the library `schedule_batch`
//! bin-packer, registers the batch in a leader-side `BatchTracker`,
//! and dispatches per-node job groups — locally for itself, via
//! `POST /v1/batch/run` for peers. Running nodes watch their jobs to a
//! terminal state and report through `POST /v1/batch/{id}/report`;
//! `GET /v1/batch/{id}` serves the tracker's summary.
//!
//! Batch deliberately does NOT ride the deploy placements reconciler:
//! that machinery *converges desired state* — a completed job looks
//! like drift to it, and moving an assignment would kill a running
//! job. Run-to-completion work wants dispatch + completion callbacks.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::config::Config;
use crate::config::job::JobSpec;
use crate::meat::batch::{BatchJob, schedule_batch};
use crate::meat::batch_tracker::BatchTracker;
use crate::meat::types::{NodeCapacity, NodeId, Resources};
use crate::reporting::aggregator::AggregatedState;

use super::agent::AgentCommand;
use super::api::{ApiState, NodeMembershipInfo};

/// How long a dispatched job may run before the watcher gives up and
/// reports it failed.
const JOB_WATCH_TIMEOUT_SECS: u64 = 3600;

/// One job in a batch submission: the full spec travels with the
/// request, so target nodes need no prior deploy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BatchJobSubmission {
    pub name: String,
    /// Namespace the job deploys into. Plain `[job.*]` configs land in
    /// `default`; the watcher matches instances on (name, namespace).
    #[serde(default = "default_namespace")]
    pub namespace: String,
    pub spec: JobSpec,
}

fn default_namespace() -> String {
    "default".to_string()
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
    /// `"completed"` or `"failed"`.
    pub status: String,
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
// Running and watching
// ---------------------------------------------------------------------------

/// Where completion reports go.
pub enum Reporter {
    /// The submitting node itself — mark the tracker directly.
    Local(Arc<Mutex<BatchTracker>>),
    /// A remote submitter — POST to its report endpoint.
    Callback {
        base_url: String,
        client: reqwest::Client,
        service_token: Option<String>,
    },
}

impl Reporter {
    async fn report(&self, batch_id: u64, job_name: &str, completed: bool) {
        match self {
            Reporter::Local(tracker) => {
                let mut tracker = tracker.lock().await;
                if completed {
                    tracker.mark_completed(batch_id, job_name);
                } else {
                    tracker.mark_failed(batch_id, job_name);
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
                    status: if completed { "completed" } else { "failed" }.to_string(),
                };
                let mut request = client.post(&url).json(&body);
                if let Some(token) = service_token {
                    request = request.bearer_auth(token);
                }
                if let Err(e) = request.send().await {
                    eprintln!("bun: batch report to {url} failed: {e}");
                }
            }
        }
    }
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
            // `stopped` alone is ambiguous: a failing job passes
            // through it between retries (any exit maps to Stopped;
            // the code is tracked separately). Success is stopped with
            // exit 0; a non-zero stop is backoff, not terminal — the
            // agent marks the instance `failed` once retries exhaust.
            // Runtimes without exit codes (runc, review H13) report
            // None: treat their stops as success rather than hanging.
            let mut outcome = None;
            for status in statuses
                .iter()
                .filter(|s| s.app_name == job.name && s.namespace == job.namespace)
            {
                outcome = match (status.state.as_str(), status.exit_code) {
                    ("failed", _) => Some(false),
                    ("stopped", Some(0) | None) => Some(true),
                    _ => None,
                };
                if outcome.is_some() {
                    break;
                }
            }
            match outcome {
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
// Handlers
// ---------------------------------------------------------------------------

/// `POST /v1/batch` — leader-forwarded submission.
pub async fn batch_submit_handler(State(state): State<ApiState>, body: String) -> Response {
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

    let batch_jobs: Vec<BatchJob> = request
        .jobs
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
    let batch_id = state
        .batch_tracker
        .lock()
        .await
        .register(&allocation.assignments)
        .0;

    // Group assignments by node and dispatch.
    let mut by_node: std::collections::HashMap<NodeId, Vec<BatchJobSubmission>> =
        std::collections::HashMap::new();
    for (job_name, node_id) in &allocation.assignments {
        if let Some(submission) = request.jobs.iter().find(|j| &j.name == job_name) {
            by_node
                .entry(node_id.clone())
                .or_default()
                .push(submission.clone());
        }
    }

    let callback_base_url = self_callback_url(&state, &self_name).await;
    for (node_id, jobs) in by_node {
        if node_id.0 == self_name {
            // Our own share: no HTTP, report straight into the tracker.
            tokio::spawn(run_jobs_and_watch(
                state.cmd_tx.clone(),
                batch_id,
                jobs,
                Reporter::Local(Arc::clone(&state.batch_tracker)),
            ));
            continue;
        }

        // Remote share: POST the group to the target node.
        let Some(url) = node_api_url(&state, &node_id).await else {
            eprintln!("bun: batch {batch_id}: no address for {node_id:?}; jobs will time out");
            continue;
        };
        let run = BatchRunRequest {
            batch_id,
            callback_base_url: callback_base_url.clone(),
            jobs,
        };
        let client = state.http_client.clone();
        let token = state.service_token.clone();
        tokio::spawn(async move {
            let mut request = client.post(format!("{url}/v1/batch/run")).json(&run);
            if let Some(token) = &token {
                request = request.bearer_auth(token);
            }
            if let Err(e) = request.send().await {
                eprintln!("bun: batch dispatch to {url} failed: {e}");
            }
        });
    }

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
    Json(run): Json<BatchRunRequest>,
) -> Response {
    let reporter = match run.callback_base_url {
        Some(base_url) => Reporter::Callback {
            base_url,
            client: state.http_client.clone(),
            service_token: state.service_token.clone(),
        },
        // No callback: the submitter is this process (or doesn't care).
        None => Reporter::Local(Arc::clone(&state.batch_tracker)),
    };
    tokio::spawn(run_jobs_and_watch(
        state.cmd_tx.clone(),
        run.batch_id,
        run.jobs,
        reporter,
    ));
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "accepted": true })),
    )
        .into_response()
}

/// `POST /v1/batch/{id}/report` — a completion callback.
pub async fn batch_report_handler(
    State(state): State<ApiState>,
    AxumPath(batch_id): AxumPath<u64>,
    Json(report): Json<BatchReportRequest>,
) -> Response {
    let mut tracker = state.batch_tracker.lock().await;
    if report.status == "completed" {
        tracker.mark_completed(batch_id, &report.job_name);
    } else {
        tracker.mark_failed(batch_id, &report.job_name);
    }
    Json(serde_json::json!({ "recorded": true })).into_response()
}

/// `GET /v1/batch/{id}` — the tracker's summary (leader-forwarded).
pub async fn batch_status_handler(
    State(state): State<ApiState>,
    AxumPath(batch_id): AxumPath<u64>,
) -> Response {
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_get_to_leader(&state, council, &format!("/v1/batch/{batch_id}")).await;
    }

    match state.batch_tracker.lock().await.summary(batch_id) {
        Some(summary) => Json(serde_json::json!(summary)).into_response(),
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
        .http_client
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
    let mut request = state.http_client.get(format!("{leader_url}{path}"));
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

/// The API URL peers use to reach a node, from the membership table.
async fn node_api_url(state: &ApiState, node_id: &NodeId) -> Option<String> {
    let membership = state.membership.as_ref()?;
    let members = membership.read().await;
    members
        .iter()
        .find(|m| &m.node_id == node_id)
        .map(|m| format!("http://{}", m.address))
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
                namespace: "default".to_string(),
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
        assert_eq!(submission.namespace, "default");
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
}
