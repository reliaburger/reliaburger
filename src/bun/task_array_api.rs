//! HTTP routes for task arrays.
//!
//! Operator-facing routes go to the leader (followers forward them):
//!
//! - `POST /v1/batch/array` submits an array.
//! - `POST /v1/batch/{id}/cancel` stops one.
//! - `GET /v1/batch/{id}` (in `batch.rs`) shows an array's summary when the
//!   id names one; see [`array_summary`].
//! - `GET /v1/batch/{id}/results` gathers per-task outcomes from every
//!   node's ledger.
//! - `GET /v1/batch/{id}/tasks/{index}/logs` finds the node that kept a
//!   failed task's output.
//!
//! Node-to-node routes (system principal only) are what the leader calls:
//! `POST /v1/batch/array/sync` and the two `local` reads.

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use super::api::ApiState;
use super::batch::{forward_get_to_leader, forward_to_leader};
use super::task_array_leader::{NodeView, TaskArrayWriteError, read_task_arrays, write_task_array};
use super::task_array_node::{MAX_RESULT_ROWS, NodeSyncRequest, TaskArrayNodeError, TaskResultRow};
use crate::config::job::JobSpec;
use crate::meat::NodeId;
use crate::meat::batch_tracker::epoch_now_secs;
use crate::meat::task_array::{TaskArraySpec, validate_template};
use crate::meat::task_array_store::{TaskArrayRecord, TaskArrayWrite};

/// Default rows `GET /v1/batch/{id}/results` returns.
pub const DEFAULT_RESULT_ROWS: usize = 10_000;

/// Most failed-index ranges a summary lists.
const SUMMARY_FAILED_RANGES: usize = 20;

/// `POST /v1/batch/array` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskArraySubmitRequest {
    /// Name for the array.
    pub name: String,
    /// Namespace; must agree with the template's if both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// What every task runs.
    pub template: JobSpec,
    /// Count and policy.
    pub spec: TaskArraySpec,
}

/// `POST /v1/batch/array` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskArraySubmitResponse {
    /// The array's id; `relish batch-status` takes it.
    pub batch_id: u64,
    /// Tasks.
    pub count: u32,
    /// Chunks the tasks are grouped into.
    pub chunks: u32,
}

/// `GET /v1/batch/{id}/results` query.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResultsQuery {
    /// Only failed tasks.
    #[serde(default)]
    pub failed: bool,
    /// Most rows to return.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /v1/batch/{id}/results` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResults {
    /// The array.
    pub batch_id: u64,
    /// Outcomes in index order.
    pub rows: Vec<TaskResultRow>,
    /// More rows existed than `limit` allowed.
    pub truncated: bool,
    /// Nodes that couldn't be asked; their tasks are missing.
    pub unreachable: Vec<NodeId>,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

fn write_error(failure: TaskArrayWriteError) -> Response {
    let status = match failure {
        // The state machine's own checks (limits, a stopped array): the
        // request, not the cluster, is at fault.
        TaskArrayWriteError::Refused(_) => StatusCode::BAD_REQUEST,
        TaskArrayWriteError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    error(status, failure.to_string())
}

/// Check a submission and settle its namespace. Everything here is also
/// enforced by the state machine; checking first gives a precise 400.
fn validate_submission(request: &mut TaskArraySubmitRequest) -> Result<String, String> {
    let namespace = match (&request.namespace, &request.template.namespace) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "the request names namespace {a:?} but the template names {b:?}"
            ));
        }
        (Some(a), _) => a.clone(),
        (None, Some(b)) => b.clone(),
        (None, None) => "default".to_string(),
    };
    request.namespace = Some(namespace.clone());
    request.template.namespace = Some(namespace.clone());
    if request.name.trim().is_empty() {
        return Err("a task array needs a name".to_string());
    }
    request.spec.validate().map_err(|e| e.to_string())?;
    validate_template(&request.template).map_err(|e| e.to_string())?;
    if request
        .template
        .env
        .values()
        .any(|value| value.is_encrypted())
    {
        return Err("task arrays don't take encrypted environment values yet".to_string());
    }
    Ok(namespace)
}

async fn follower_council(state: &ApiState) -> Option<&crate::council::CouncilNode> {
    let council = state.council.as_deref()?;
    if council.is_leader().await {
        None
    } else {
        Some(council)
    }
}

/// `POST /v1/batch/array`: submit a task array.
pub async fn submit_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    body: String,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let mut request: TaskArraySubmitRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("invalid task array: {e}")),
    };
    let namespace = match validate_submission(&mut request) {
        Ok(namespace) => namespace,
        Err(reason) => return error(StatusCode::BAD_REQUEST, reason),
    };
    if let Err(response) = crate::sesame::auth::authorize_scoped(auth, &request.name, &namespace) {
        return response;
    }
    if let Some(council) = follower_council(&state).await {
        return forward_to_leader(&state, council, "/v1/batch/array", body).await;
    }
    let count = request.spec.count;
    let chunks = request.spec.chunk_count();
    let write = TaskArrayWrite::Register {
        name: request.name,
        namespace,
        template: Box::new(request.template),
        spec: request.spec,
        submitted_at_epoch_secs: epoch_now_secs(),
    };
    match write_task_array(&state, write).await {
        Ok(Some(batch_id)) => (
            StatusCode::ACCEPTED,
            Json(TaskArraySubmitResponse {
                batch_id,
                count,
                chunks,
            }),
        )
            .into_response(),
        Ok(None) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the registration returned no id",
        ),
        Err(e) => write_error(e),
    }
}

/// `POST /v1/batch/{id}/cancel`: stop a task array. Tasks that haven't
/// started never will; running ones get SIGTERM, then SIGKILL.
pub async fn cancel_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath(batch_id): AxumPath<u64>,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let arrays = read_task_arrays(&state).await;
    let Some(record) = arrays.get(batch_id) else {
        return error(
            StatusCode::NOT_FOUND,
            format!("task array {batch_id} not found (only task arrays can be cancelled)"),
        );
    };
    if let Err(response) =
        crate::sesame::auth::authorize_scoped(auth, &record.name, &record.namespace)
    {
        return response;
    }
    if let Some(council) = follower_council(&state).await {
        let path = format!("/v1/batch/{batch_id}/cancel");
        return forward_to_leader(&state, council, &path, String::new()).await;
    }
    match write_task_array(&state, TaskArrayWrite::Cancel { batch_id }).await {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({ "batch_id": batch_id, "cancelled": true })),
        )
            .into_response(),
        Err(e) => write_error(e),
    }
}

/// The JSON `GET /v1/batch/{id}` serves for a task array. `done` is true
/// once nothing will change, which is what `relish batch-status --wait`
/// polls for.
pub fn array_summary(
    batch_id: u64,
    record: &TaskArrayRecord,
    nodes: &[NodeView],
) -> serde_json::Value {
    let state = &record.state;
    let summary = state.summary();
    let failed: Vec<[u32; 2]> = state
        .failed_indices()
        .ranges()
        .take(SUMMARY_FAILED_RANGES)
        .map(|range| [*range.start(), *range.end()])
        .collect();
    serde_json::json!({
        "batch_id": batch_id,
        "kind": "array",
        "name": record.name,
        "namespace": record.namespace,
        "status": summary.status,
        "done": summary.status.is_terminal(),
        "total": summary.total,
        "succeeded": summary.succeeded,
        "failed": summary.failed,
        "not_run": summary.not_run,
        "retried": summary.retried,
        "queued": summary.queued,
        "held": summary.held,
        "chunks": state.spec.chunk_count(),
        "chunks_done": summary.chunks_done,
        "failed_indices": failed,
        "failed_overflow": state.failed_overflow,
        "submitted_at_epoch_secs": state.submitted_at_epoch_secs,
        "nodes": nodes,
    })
}

/// `POST /v1/batch/array/sync`: the leader's once-a-second call.
pub async fn sync_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    Json(request): Json<NodeSyncRequest>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    match &state.task_arrays.node {
        Some(node) => Json(node.sync(&request).await).into_response(),
        None => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "this node has no task-array executor",
        ),
    }
}

fn node_error(error: TaskArrayNodeError) -> Response {
    match error {
        TaskArrayNodeError::UnknownArray { .. } | TaskArrayNodeError::NoOutput { .. } => {
            self::error(StatusCode::NOT_FOUND, error.to_string())
        }
        other => self::error(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    }
}

/// `GET /v1/batch/array/{id}/local/results`: this node's ledger.
pub async fn local_results_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath(batch_id): AxumPath<u64>,
    Query(query): Query<ResultsQuery>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    let Some(node) = &state.task_arrays.node else {
        return error(StatusCode::NOT_FOUND, "this node runs no task arrays");
    };
    let limit = query.limit.unwrap_or(DEFAULT_RESULT_ROWS);
    match node.results(batch_id, query.failed, limit).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => node_error(e),
    }
}

/// `GET /v1/batch/array/{id}/local/tasks/{index}/logs`: a failed task's
/// output, if this node kept it.
pub async fn local_logs_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath((batch_id, index)): AxumPath<(u64, u32)>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    let Some(node) = &state.task_arrays.node else {
        return error(StatusCode::NOT_FOUND, "this node runs no task arrays");
    };
    match node.task_output(batch_id, index).await {
        Ok(bytes) => bytes.into_response(),
        Err(e) => node_error(e),
    }
}

/// Every node that might hold files for an array, with its URL (`None`
/// for this node).
async fn every_node(state: &ApiState) -> Vec<(NodeId, Option<String>)> {
    let self_name = state
        .node_name
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let mut nodes = vec![(NodeId::new(self_name.clone()), None)];
    if let Some(membership) = &state.membership {
        for member in membership.read().await.iter() {
            if member.node_id.0 != self_name {
                let url = state.cluster_http.url(&member.address.to_string(), "");
                nodes.push((member.node_id.clone(), Some(url)));
            }
        }
    }
    nodes
}

async fn fetch_from_node(state: &ApiState, url: &str) -> Result<reqwest::Response, String> {
    let mut request = state.cluster_http.client().get(url);
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    tokio::time::timeout(super::task_array_leader::NODE_SYNC_TIMEOUT, request.send())
        .await
        .map_err(|_| "timed out".to_string())?
        .map_err(|e| e.to_string())
}

/// One node's result rows. `Ok(None)` means the node holds nothing for
/// the array (it never ran any of it).
async fn node_results(
    state: &ApiState,
    url: Option<&str>,
    batch_id: u64,
    failed: bool,
    limit: usize,
) -> Result<Option<Vec<TaskResultRow>>, String> {
    let Some(url) = url else {
        let Some(node) = &state.task_arrays.node else {
            return Ok(None);
        };
        return match node.results(batch_id, failed, limit).await {
            Ok(rows) => Ok(Some(rows)),
            Err(TaskArrayNodeError::UnknownArray { .. }) => Ok(None),
            Err(e) => Err(e.to_string()),
        };
    };
    let url =
        format!("{url}/v1/batch/array/{batch_id}/local/results?failed={failed}&limit={limit}");
    let response = fetch_from_node(state, &url).await?;
    match response.status().as_u16() {
        404 => Ok(None),
        code if (200..300).contains(&code) => {
            response.json().await.map(Some).map_err(|e| e.to_string())
        }
        code => Err(format!("answered {code}")),
    }
}

/// Merge rows from several nodes. An index can appear twice when a chunk
/// ran partly on a node that was then written off; the row that agrees
/// with the leader's record of failures wins.
pub fn merge_rows(
    record: &TaskArrayRecord,
    per_node: Vec<Vec<TaskResultRow>>,
    limit: usize,
) -> (Vec<TaskResultRow>, bool) {
    let failures = record.state.failed_indices();
    let mut merged: std::collections::BTreeMap<u32, TaskResultRow> = Default::default();
    for row in per_node.into_iter().flatten() {
        let agrees = |row: &TaskResultRow| row.succeeded != failures.contains(row.index);
        match merged.get(&row.index) {
            Some(existing) if agrees(existing) || !agrees(&row) => {}
            _ => {
                merged.insert(row.index, row);
            }
        }
    }
    let truncated = merged.len() > limit;
    (merged.into_values().take(limit).collect(), truncated)
}

/// `GET /v1/batch/{id}/results`: every task's outcome, gathered from the
/// nodes' ledgers.
pub async fn results_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath(batch_id): AxumPath<u64>,
    Query(query): Query<ResultsQuery>,
) -> Response {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_RESULT_ROWS)
        .min(MAX_RESULT_ROWS);
    let arrays = read_task_arrays(&state).await;
    let Some(record) = arrays.get(batch_id) else {
        return error(
            StatusCode::NOT_FOUND,
            format!("task array {batch_id} not found"),
        );
    };
    if let Err(response) =
        crate::sesame::auth::authorize_scoped(auth.as_deref(), &record.name, &record.namespace)
    {
        return response;
    }
    if let Some(council) = follower_council(&state).await {
        let path = format!(
            "/v1/batch/{batch_id}/results?failed={}&limit={limit}",
            query.failed
        );
        return forward_get_to_leader(&state, council, &path).await;
    }
    let mut per_node = Vec::new();
    let mut unreachable = Vec::new();
    for (node, url) in every_node(&state).await {
        // One row more than asked for, so a full answer can say it's truncated.
        let asked = limit.saturating_add(1);
        match node_results(&state, url.as_deref(), batch_id, query.failed, asked).await {
            Ok(Some(rows)) => per_node.push(rows),
            Ok(None) => {}
            Err(reason) => {
                eprintln!("bun: task array {batch_id}: results from {node}: {reason}");
                unreachable.push(node);
            }
        }
    }
    let (rows, truncated) = merge_rows(record, per_node, limit);
    Json(TaskResults {
        batch_id,
        rows,
        truncated,
        unreachable,
    })
    .into_response()
}

/// `GET /v1/batch/{id}/tasks/{index}/logs`: a failed task's first and
/// last bytes of output, from whichever node kept them.
pub async fn logs_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath((batch_id, index)): AxumPath<(u64, u32)>,
) -> Response {
    let arrays = read_task_arrays(&state).await;
    let Some(record) = arrays.get(batch_id) else {
        return error(
            StatusCode::NOT_FOUND,
            format!("task array {batch_id} not found"),
        );
    };
    if let Err(response) =
        crate::sesame::auth::authorize_scoped(auth.as_deref(), &record.name, &record.namespace)
    {
        return response;
    }
    if let Some(council) = follower_council(&state).await {
        let path = format!("/v1/batch/{batch_id}/tasks/{index}/logs");
        return forward_get_to_leader(&state, council, &path).await;
    }
    if index >= record.state.spec.count {
        return error(
            StatusCode::NOT_FOUND,
            format!("task array {batch_id} has no task {index}"),
        );
    }
    for (_, url) in every_node(&state).await {
        let output = match url {
            None => match &state.task_arrays.node {
                Some(node) => node.task_output(batch_id, index).await.ok(),
                None => None,
            },
            Some(url) => {
                let url = format!("{url}/v1/batch/array/{batch_id}/local/tasks/{index}/logs");
                match fetch_from_node(&state, &url).await {
                    Ok(response) if response.status().is_success() => {
                        response.bytes().await.ok().map(|bytes| bytes.to_vec())
                    }
                    _ => None,
                }
            }
        };
        if let Some(bytes) = output {
            return bytes.into_response();
        }
    }
    error(
        StatusCode::NOT_FOUND,
        format!("no node kept output for task {index}; output is kept only for failed tasks"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::EnvValue;
    use crate::meat::index_set::IndexRangeSet;
    use crate::meat::task_array_state::TaskArrayState;

    fn template() -> JobSpec {
        JobSpec {
            image: None,
            command: Some(vec!["{index}".to_string()]),
            schedule: None,
            run_before: Vec::new(),
            memory: None,
            cpu: None,
            env: Default::default(),
            namespace: None,
            exec: Some("/usr/bin/true".into()),
            script: None,
        }
    }

    fn submission() -> TaskArraySubmitRequest {
        TaskArraySubmitRequest {
            name: "render".to_string(),
            namespace: None,
            template: template(),
            spec: TaskArraySpec::with_count(100),
        }
    }

    #[test]
    fn a_submission_settles_one_namespace() {
        let mut request = submission();
        assert_eq!(validate_submission(&mut request).unwrap(), "default");
        assert_eq!(request.template.namespace.as_deref(), Some("default"));

        let mut request = submission();
        request.template.namespace = Some("jobs".to_string());
        assert_eq!(validate_submission(&mut request).unwrap(), "jobs");
        assert_eq!(request.namespace.as_deref(), Some("jobs"));

        let mut request = submission();
        request.namespace = Some("a".to_string());
        request.template.namespace = Some("b".to_string());
        assert!(validate_submission(&mut request).is_err());
    }

    #[test]
    fn invalid_submissions_are_refused_with_the_reason() {
        let mut no_exec = submission();
        no_exec.template.exec = None;
        no_exec.template.image = Some("alpine".to_string());
        assert!(
            validate_submission(&mut no_exec)
                .unwrap_err()
                .contains("exec")
        );

        let mut zero = submission();
        zero.spec.count = 0;
        assert!(
            validate_submission(&mut zero)
                .unwrap_err()
                .contains("count")
        );

        let mut secret = submission();
        secret.template.env.insert(
            "KEY".to_string(),
            EnvValue::Encrypted("ENC[AGE:x]".to_string()),
        );
        assert!(
            validate_submission(&mut secret)
                .unwrap_err()
                .contains("encrypted")
        );

        let mut unnamed = submission();
        unnamed.name = " ".to_string();
        assert!(validate_submission(&mut unnamed).is_err());
    }

    fn record_with_failures(failed: &[u32]) -> TaskArrayRecord {
        let mut state = TaskArrayState::new(
            TaskArraySpec {
                chunk_size: 10,
                ..TaskArraySpec::with_count(10)
            },
            1,
        )
        .unwrap();
        let node = NodeId::new("a");
        state
            .grant(&node, &IndexRangeSet::from_range(0..=0))
            .unwrap();
        let mut failed_indices = IndexRangeSet::new();
        for index in failed {
            failed_indices.insert(*index);
        }
        state
            .complete(
                &node,
                &crate::meat::task_array_state::ChunkResult {
                    chunk: crate::meat::task_array::ChunkId(0),
                    attempt: 1,
                    succeeded: 10 - failed.len() as u32,
                    failed_indices,
                    not_run: 0,
                    retried: 0,
                },
            )
            .unwrap();
        TaskArrayRecord {
            name: "render".to_string(),
            namespace: "default".to_string(),
            template: template(),
            state,
        }
    }

    fn row(index: u32, succeeded: bool) -> TaskResultRow {
        TaskResultRow {
            index,
            attempts: 1,
            succeeded,
            exit_code: Some(i32::from(!succeeded)),
            run_ms: 1,
        }
    }

    #[test]
    fn merged_results_are_ordered_deduplicated_and_limited() {
        let record = record_with_failures(&[2]);
        let (rows, truncated) = merge_rows(
            &record,
            vec![
                vec![row(0, true), row(2, true), row(3, true)],
                // Index 2 also ran here, and this run is the one the leader
                // recorded (it failed).
                vec![row(1, true), row(2, false)],
            ],
            10,
        );
        let got: Vec<(u32, bool)> = rows.iter().map(|r| (r.index, r.succeeded)).collect();
        assert_eq!(got, vec![(0, true), (1, true), (2, false), (3, true)]);
        assert!(!truncated);

        let (rows, truncated) = merge_rows(&record, vec![vec![row(0, true), row(1, true)]], 1);
        assert_eq!(rows.len(), 1);
        assert!(truncated);
    }

    #[test]
    fn an_array_summary_says_when_it_is_done() {
        let record = record_with_failures(&[2, 3, 7]);
        let summary = array_summary(5, &record, &[]);
        assert_eq!(summary["kind"], "array");
        assert_eq!(summary["done"], true);
        assert_eq!(summary["status"], "CompletedWithFailures");
        assert_eq!(summary["failed"], 3);
        assert_eq!(summary["succeeded"], 7);
        assert_eq!(
            summary["failed_indices"],
            serde_json::json!([[2, 3], [7, 7]])
        );
    }
}
