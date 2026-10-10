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
use axum::http::{HeaderMap, StatusCode};
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
pub const DEFAULT_RESULT_ROWS: usize = 1000;

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
    #[serde(default = "singleton_spec")]
    pub spec: TaskArraySpec,
}

fn singleton_spec() -> TaskArraySpec {
    TaskArraySpec::with_count(1)
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
    /// Resume strictly after this task index.
    pub after: Option<u32>,
    /// Address one task directly.
    pub index: Option<u32>,
    /// Internal range endpoints, used by peer fan-out.
    pub start: Option<u32>,
    pub end: Option<u32>,
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
    /// Cursor for the next bounded index window, including empty failure pages.
    pub next_after: Option<u32>,
    /// Detail is retained locally, for at most the configured terminal retention.
    pub retention_seconds: u64,
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
    let mut template = request.template.clone();
    template.schedule = None;
    validate_template(&template).map_err(|e| e.to_string())?;
    if let Some(expression) = &request.template.schedule {
        crate::meat::cron::CronSchedule::parse(expression).map_err(|e| e.to_string())?;
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
    binder: Option<axum::Extension<crate::pickle::binding::ImageBinder>>,
    headers: HeaderMap,
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
        return forward_to_leader(&state, council, "/v1/batch/array", body, &headers).await;
    }
    let count = request.spec.count;
    let chunks = request.spec.chunk_count();
    let schedule = request.template.schedule.take();
    let mut definition = crate::meat::job::JobDefinition {
        template: request.template,
        tasks: request.spec,
        cron: schedule.map(|expression| crate::meat::job::CronPolicy {
            expression,
            overlap: Default::default(),
            missed: Default::default(),
        }),
        replay_unknown: true,
    };
    if let Err(response) = super::job_api::admit_definition(
        &state,
        auth,
        binder.as_deref(),
        &request.name,
        &namespace,
        &mut definition,
    )
    .await
    {
        return response;
    }
    let trigger = if definition.cron.is_some() {
        None
    } else {
        let request_id = match request_identity(&headers) {
            Ok(request_id) => request_id,
            Err(response) => return response,
        };
        Some(crate::meat::job::RunTrigger::Manual { request_id })
    };
    let write = TaskArrayWrite::Job(Box::new(crate::meat::job::JobWrite::Put {
        name: request.name,
        namespace,
        definition: Box::new(definition),
        trigger,
        now_epoch_secs: epoch_now_secs(),
    }));
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
        Ok(None) => (StatusCode::ACCEPTED, Json(serde_json::json!({"batch_id":null,"count":count,"chunks":chunks,"schedule_registered":true}))).into_response(),
        Err(e) => write_error(e),
    }
}

/// The submission's `Idempotency-Key` header, or a fresh random identity when
/// the client sent none. A retry that reuses the key returns the runs the
/// first attempt created; without one, every request is new work.
#[allow(clippy::result_large_err)] // An HTTP refusal is the error.
fn request_identity(headers: &HeaderMap) -> Result<String, Response> {
    match headers.get("idempotency-key") {
        Some(value) => match value.to_str() {
            Ok(value) => Ok(value.to_string()),
            Err(_) => Err(error(StatusCode::BAD_REQUEST, "invalid idempotency-key")),
        },
        None => Ok(hex::encode(rand::random::<[u8; 16]>())),
    }
}

/// Compact mixed-profile input, also accepted as a TOML file by relish.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskManifestRequest {
    pub name: String,
    #[serde(default = "default_namespace")]
    pub namespace: String,
    pub cohort: Vec<crate::meat::task_array_store::ManifestCohort>,
}
fn default_namespace() -> String {
    "default".into()
}

/// `POST /v1/batch/manifest`: one durable admission for heterogeneous work.
pub async fn manifest_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    binder: Option<axum::Extension<crate::pickle::binding::ImageBinder>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let mut request: TaskManifestRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(error) => return self::error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    if let Err(response) =
        crate::sesame::auth::authorize_scoped(auth, &request.name, &request.namespace)
    {
        return response;
    }
    for cohort in &mut request.cohort {
        let mut array = TaskArraySubmitRequest {
            name: request.name.clone(),
            namespace: Some(request.namespace.clone()),
            template: cohort.template.clone(),
            spec: cohort.spec.clone(),
        };
        if let Err(reason) = validate_submission(&mut array) {
            return error(StatusCode::BAD_REQUEST, reason);
        }
        cohort.template = array.template;
    }
    if let Some(council) = follower_council(&state).await {
        return forward_to_leader(&state, council, "/v1/batch/manifest", body, &headers).await;
    }
    for cohort in &mut request.cohort {
        let mut definition = crate::meat::job::JobDefinition {
            template: cohort.template.clone(),
            tasks: cohort.spec.clone(),
            cron: None,
            replay_unknown: true,
        };
        if let Err(response) = super::job_api::admit_definition(
            &state,
            auth,
            binder.as_deref(),
            &request.name,
            &request.namespace,
            &mut definition,
        )
        .await
        {
            return response;
        }
        cohort.template = definition.template;
    }
    let request_id = match request_identity(&headers) {
        Ok(request_id) => request_id,
        Err(response) => return response,
    };
    let write = TaskArrayWrite::RegisterManifest {
        name: request.name,
        namespace: request.namespace,
        request_id,
        cohorts: request.cohort,
        submitted_at_epoch_secs: epoch_now_secs(),
    };
    match write_task_array(&state, write).await {
        Ok(Some(batch_id)) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"batch_id": batch_id})),
        )
            .into_response(),
        Ok(None) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "manifest admission returned no identity",
        ),
        Err(error) => write_error(error),
    }
}

// Preserve the shared API binder's HTTP refusal response.
#[allow(clippy::result_large_err)]
pub(crate) async fn admit_template_image(
    state: &ApiState,
    binder: Option<&crate::pickle::binding::ImageBinder>,
    template: &mut JobSpec,
) -> Result<(), Response> {
    if let Some(binder) = binder {
        let mut config = crate::config::Config::default();
        config.job.insert("task".into(), template.clone());
        super::api::bind_images(state, binder, &mut config).await?;
        *template = config
            .job
            .remove("task")
            .expect("binder retains job template");
    }
    pin_template_image(state, template)
        .await
        .map_err(|reason| error(StatusCode::BAD_REQUEST, reason))?;
    let Some(image) = template
        .image
        .as_deref()
        .filter(|image| crate::grill::image::looks_like_image_ref(image))
    else {
        return Ok(());
    };
    let catalog = match (&state.council, &state.pickle_catalog) {
        (Some(council), _) => council.manifest_catalog().await,
        (None, Some(catalog)) => catalog.read().await.clone(),
        _ => Default::default(),
    };
    if crate::meat::scheduler::lookup_pickle_manifest(image, &catalog).is_none() {
        crate::pickle::trust::check_upstream(&state.task_arrays.trust_policy, image)
            .map_err(|reason| error(StatusCode::FORBIDDEN, reason.to_string()))?;
        if let Some(check) = crate::pickle::trust::CosignCheck::for_image(
            &state.task_arrays.trust_policy,
            image,
            state.task_arrays.signature_source.as_ref(),
        ) {
            check
                .run()
                .await
                .map_err(|reason| error(StatusCode::FORBIDDEN, reason))?;
        }
    }
    Ok(())
}

async fn pin_template_image(
    state: &ApiState,
    template: &mut crate::config::job::JobSpec,
) -> Result<(), String> {
    if template.image.is_some() && state.task_arrays.trust_policy.require_signatures {
        let council = state
            .council
            .as_ref()
            .ok_or("signed image admission requires cluster trust state")?;
        let security = council.security_state().await;
        let catalog = council.manifest_catalog().await;
        let roots: Vec<Vec<u8>> = security
            .trusted_cas(crate::sesame::types::CaRole::Root)
            .into_iter()
            .map(|ca| ca.certificate_der.clone())
            .collect();
        if let Some(digest) = crate::meat::scheduler::verify_image_signature(
            template.image.as_deref(),
            &catalog,
            &state.task_arrays.trust_policy,
            &roots,
            Some(&security.crl),
        )
        .map_err(|e| e.to_string())?
        {
            template.image = Some(crate::meat::scheduler::pin_image_reference(
                template.image.as_deref().expect("image checked"),
                &digest,
            ));
        }
    }
    Ok(())
}

/// `POST /v1/batch/{id}/cancel`: stop a task array. Tasks that haven't
/// started never will; running ones get SIGTERM, then SIGKILL.
pub async fn cancel_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath(batch_id): AxumPath<u64>,
    headers: HeaderMap,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let arrays = read_task_arrays(&state).await;
    let (name, namespace, write) = if let Some(record) = arrays.get(batch_id) {
        (
            &record.name,
            &record.namespace,
            TaskArrayWrite::Cancel {
                now_epoch_secs: crate::meat::batch_tracker::epoch_now_secs(),
                batch_id,
            },
        )
    } else if let Some(record) = arrays.manifest(batch_id) {
        if let Err(response) = authorize_manifest(auth, record, &arrays) {
            return response;
        }
        (
            &record.name,
            &record.namespace,
            TaskArrayWrite::CancelManifest {
                batch_id,
                now_epoch_secs: epoch_now_secs(),
            },
        )
    } else {
        return error(
            StatusCode::NOT_FOUND,
            format!("task submission {batch_id} not found"),
        );
    };
    if arrays
        .manifest(batch_id)
        .is_none_or(|manifest| !manifest.common_jobs)
        && let Err(response) = crate::sesame::auth::authorize_scoped(auth, name, namespace)
    {
        return response;
    }
    let permissions = super::api::permission_map(&state).await;
    let targets: Vec<_> = if let Some(manifest) = arrays
        .manifest(batch_id)
        .filter(|manifest| manifest.common_jobs)
    {
        manifest
            .cohorts
            .iter()
            .filter_map(|(_, id)| arrays.get(*id))
            .map(|record| (record.name.as_str(), record.namespace.as_str()))
            .collect()
    } else {
        vec![(name.as_str(), namespace.as_str())]
    };
    for (name, namespace) in targets {
        if let Err(response) = crate::sesame::auth::authorize_permission(
            auth,
            crate::config::PermissionAction::Deploy,
            name,
            namespace,
            &permissions,
        ) {
            return response;
        }
    }
    if let Some(council) = follower_council(&state).await {
        let path = format!("/v1/batch/{batch_id}/cancel");
        return forward_to_leader(&state, council, &path, String::new(), &headers).await;
    }
    match write_task_array(&state, write).await {
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
    let elapsed = record
        .terminal_at_epoch_secs
        .unwrap_or_else(epoch_now_secs)
        .saturating_sub(state.submitted_at_epoch_secs);
    let callers = nodes
        .iter()
        .try_fold(0u64, |sum, node| sum.checked_add(node.counters.running));
    let active = if summary.status.is_terminal() {
        Some(0)
    } else if nodes.is_empty() {
        None
    } else {
        nodes
            .iter()
            .filter(|node| node.refused.is_none())
            .try_fold(0u64, |sum, node| {
                sum.checked_add(node.counters.active_commands?)
            })
    };
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
        "active_commands": active,
        "other_in_flight_attempts": active.and_then(|count| callers.and_then(|callers| callers.checked_sub(count))),
        "activity_semantics": "last_node_sync_verified_start_to_positive_cleanup",
        "elapsed_seconds": elapsed,
        "whole_run_successes_per_second": (elapsed > 0).then(|| summary.succeeded as f64 / elapsed as f64),
        "chunks": state.spec.chunk_count(),
        "chunks_done": summary.chunks_done,
        "failed_indices": failed,
        "failed_overflow": state.failed_overflow,
        "submitted_at_epoch_secs": state.submitted_at_epoch_secs,
        "nodes": nodes.iter().take(64).collect::<Vec<_>>(),
        "nodes_truncated": nodes.len()>64,
        "duration_final_attempt_ms": { "bounds": [1,2,4,8,16,32,64,128,256,512,1024,2048,4096,8192,16384,null], "counts": state.duration_counts() },
        "cpu_request_millicores": record.template.cpu.map_or(1000, |r| r.request),
        "memory_request_bytes": record.template.memory.map_or(64 << 20, |r| r.request),
        "age_seconds": epoch_now_secs().saturating_sub(state.submitted_at_epoch_secs),
        "details_retention_seconds": crate::meat::task_array_store::TERMINAL_RETENTION_SECS,
        "execution_semantics": "at_least_once",
        "runtime": record.template.runtime,
        "idle_executor_reservation": if matches!(record.template.runtime, crate::config::job::JobRuntime::SharedRunc | crate::config::job::JobRuntime::Process) {
            super::reusable_executor::ExecutorProfile::new(&record.template).ok().map(|profile| serde_json::json!({"cpu_millicores":profile.reservation.cpu_millicores,"memory_bytes":profile.reservation.memory_bytes}))
        } else { None },
        "idle_executor_reservation_semantics": "profile_per_compatible_executor; process_requires_rootful_linux_native_backend; not_live_node_usage",
    })
}

/// Attach immutable common-run provenance and a bounded unknown-owner view.
pub fn enrich_summary(
    summary: &mut serde_json::Value,
    arrays: &crate::meat::task_array_store::TaskArrays,
    id: u64,
) {
    if let Some(run) = arrays.jobs().run(id) {
        summary["run"] = serde_json::json!(run);
        summary["execution_semantics"] = serde_json::json!(if run.replay_unknown {
            "at_least_once"
        } else {
            "acknowledged_unknown_replay"
        });
        summary["unknown_owners"] = serde_json::json!(
            run.unknown_owners
                .iter()
                .filter_map(|node| arrays
                    .owner_fingerprint(id, node)
                    .map(|digest| serde_json::json!({"node":node,"grant_digest":digest})))
                .collect::<Vec<_>>()
        );
        if !run.unknown_owners.is_empty() && summary["done"] != true {
            summary["status"] = serde_json::json!("Unknown");
        }
    }
}

/// Finite groups require access to every real logical child, never a fabricated parent namespace.
#[allow(clippy::result_large_err)]
pub(crate) fn authorize_manifest(
    auth: Option<&crate::sesame::auth::AuthContext>,
    manifest: &crate::meat::task_array_store::TaskManifest,
    arrays: &crate::meat::task_array_store::TaskArrays,
) -> Result<(), Response> {
    if !manifest.common_jobs {
        crate::sesame::auth::authorize_scoped(auth, &manifest.name, &manifest.namespace)?;
    }
    for (_, id) in &manifest.cohorts {
        let record = arrays.get(*id).ok_or_else(|| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "manifest profile is unavailable",
            )
        })?;
        crate::sesame::auth::authorize_scoped(auth, &record.name, &record.namespace)?;
    }
    Ok(())
}

/// Summary for a mixed manifest. Profile rows remain bounded by admission;
/// their tasks are represented only by counts and a mergeable histogram.
pub async fn manifest_summary(
    batch_id: u64,
    manifest: &crate::meat::task_array_store::TaskManifest,
    arrays: &crate::meat::task_array_store::TaskArrays,
    service: &super::task_array_leader::TaskArrayService,
) -> serde_json::Value {
    let mut total = 0;
    let mut succeeded = 0;
    let mut failed = 0;
    let mut not_run = 0;
    let mut retried = 0;
    let mut queued = 0;
    let mut held = 0;
    let mut done = true;
    let mut durations = [0u64; 16];
    let mut cohorts = Vec::new();
    let mut stopped_failed = false;
    let mut stopping = false;
    let mut unknown = false;
    for (name, id) in &manifest.cohorts {
        if let Some(record) = arrays.get(*id) {
            let s = record.state.summary();
            unknown |= arrays
                .jobs()
                .run(*id)
                .is_some_and(|run| !run.unknown_owners.is_empty());
            total += s.total;
            succeeded += s.succeeded;
            failed += s.failed;
            not_run += s.not_run;
            retried += s.retried;
            queued += s.queued;
            held += s.held;
            done &= s.status.is_terminal();
            stopped_failed |= matches!(
                s.status,
                crate::meat::task_array_state::TaskArrayStatus::Failed
            );
            stopping |= matches!(
                s.status,
                crate::meat::task_array_state::TaskArrayStatus::Stopping
            );
            for (total, count) in durations.iter_mut().zip(record.state.duration_counts()) {
                *total += count;
            }
            let mut cohort = array_summary(*id, record, &service.node_views(*id).await);
            enrich_summary(&mut cohort, arrays, *id);
            cohort["profile"] = serde_json::json!(name);
            cohorts.push(cohort);
        } else {
            done = false;
        }
    }
    let aggregate = |key: &str| {
        (cohorts.len() == manifest.cohorts.len())
            .then(|| {
                cohorts
                    .iter()
                    .try_fold(0u64, |sum, cohort| sum.checked_add(cohort[key].as_u64()?))
            })
            .flatten()
    };
    let active = aggregate("active_commands");
    let other = aggregate("other_in_flight_attempts");
    let end = if done {
        manifest
            .cohorts
            .iter()
            .filter_map(|(_, id)| {
                arrays
                    .get(*id)
                    .and_then(|record| record.terminal_at_epoch_secs)
            })
            .max()
            .unwrap_or(manifest.submitted_at_epoch_secs)
    } else {
        epoch_now_secs()
    };
    let elapsed = end.saturating_sub(manifest.submitted_at_epoch_secs);
    serde_json::json!({ "batch_id":batch_id,"kind":"manifest","name":manifest.name,"namespace":manifest.namespace,"total":total,"succeeded":succeeded,"failed":failed,"not_run":not_run,"retried":retried,"queued":queued,"held":held,"done":done,
        "status":if unknown && !done {"Unknown"} else if stopping {"Stopping"} else if !done {"Running"} else if stopped_failed {"Failed"} else if not_run>0 {"Cancelled"} else if failed>0 {"CompletedWithFailures"} else {"Succeeded"},
        "active_commands":active,"other_in_flight_attempts":other,"elapsed_seconds":elapsed,"whole_run_successes_per_second":(elapsed>0).then(||succeeded as f64/elapsed as f64),"activity_semantics":"last_node_sync_verified_start_to_positive_cleanup","cohorts":cohorts,"duration_final_attempt_ms":{"bounds":[1,2,4,8,16,32,64,128,256,512,1024,2048,4096,8192,16384,null],"counts":durations},"rates":service.rates(batch_id,(succeeded,failed)).await,"details_retention_seconds":crate::meat::task_array_store::TERMINAL_RETENTION_SECS,"execution_semantics":if manifest.common_jobs {"acknowledged_unknown_replay"} else {"at_least_once"} })
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
    let start = query
        .start
        .or(query.index)
        .unwrap_or_else(|| query.after.map_or(0, |a| a.saturating_add(1)));
    let end = query
        .end
        .unwrap_or_else(|| {
            if query.index.is_some() {
                start.saturating_add(1)
            } else {
                start.saturating_add(super::task_array_node::RESULT_PAGE_SPAN)
            }
        })
        .min(start.saturating_add(super::task_array_node::RESULT_PAGE_SPAN));
    match node
        .results_page(batch_id, query.failed, limit, start, end)
        .await
    {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => node_error(e),
    }
}

/// `GET /v1/batch/array/{id}/local/tasks/{index}/logs`: a failed task's
/// output, if this node kept it.
#[derive(Default, Deserialize)]
pub struct OutputQuery {
    grant: Option<u64>,
}

pub async fn local_logs_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath((batch_id, index)): AxumPath<(u64, u32)>,
    Query(query): Query<OutputQuery>,
) -> Response {
    if let Err(response) = crate::sesame::auth::require_system(auth.as_deref()) {
        return response;
    }
    let Some(node) = &state.task_arrays.node else {
        return error(StatusCode::NOT_FOUND, "this node runs no task arrays");
    };
    let result = match query.grant {
        Some(grant) => node.task_output_grant(batch_id, index, grant).await,
        None => node.task_output(batch_id, index).await,
    };
    match result {
        Ok(bytes) => bytes.into_response(),
        Err(e) => node_error(e),
    }
}

/// Every node that might hold files for an array, with its URL (`None`
/// for this node).
pub(crate) async fn every_node(state: &ApiState) -> Vec<(NodeId, Option<String>)> {
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
    start: u32,
    end: u32,
) -> Result<Option<Vec<TaskResultRow>>, String> {
    let Some(url) = url else {
        let Some(node) = &state.task_arrays.node else {
            return Ok(None);
        };
        return match node.results_page(batch_id, failed, limit, start, end).await {
            Ok(rows) => Ok(Some(rows)),
            Err(TaskArrayNodeError::UnknownArray { .. }) => Ok(None),
            Err(e) => Err(e.to_string()),
        };
    };
    let url = format!(
        "{url}/v1/batch/array/{batch_id}/local/results?failed={failed}&limit={limit}&start={start}&end={end}"
    );
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
/// with the leader's accepted node and grant generation wins.
pub fn merge_rows(
    record: &TaskArrayRecord,
    per_node: Vec<(NodeId, Vec<TaskResultRow>)>,
    limit: usize,
) -> (Vec<TaskResultRow>, bool) {
    let mut merged: std::collections::BTreeMap<u32, TaskResultRow> = Default::default();
    for (node, rows) in per_node {
        for row in rows {
            let Some(chunk) = record.state.spec.chunk_of(row.index) else {
                continue;
            };
            if record.state.accepted_grant(chunk) == Some((node.clone(), row.grant_attempt)) {
                merged.insert(row.index, row);
            }
        }
    }
    let truncated = merged.len() > limit;
    (merged.into_values().take(limit).collect(), truncated)
}

fn result_window(
    record: &TaskArrayRecord,
    query: &ResultsQuery,
) -> (u32, u32, std::collections::BTreeSet<NodeId>) {
    let start = query
        .index
        .unwrap_or_else(|| query.after.map_or(0, |a| a.saturating_add(1)));
    let end = if query.index.is_some() {
        start.saturating_add(1)
    } else {
        start.saturating_add(super::task_array_node::RESULT_PAGE_SPAN)
    }
    .min(record.state.spec.count);
    let mut owners = std::collections::BTreeSet::new();
    let mut end = end;
    let mut cursor = start;
    while cursor < end {
        let Some(chunk) = record.state.spec.chunk_of(cursor) else {
            break;
        };
        if let Some((node, _)) = record.state.accepted_grant(chunk) {
            if !owners.contains(&node) && owners.len() == 8 {
                end = cursor;
                break;
            }
            owners.insert(node.clone());
        }
        // Walk chunk boundaries, rather than visiting every index in a large chunk.
        cursor = ((u64::from(chunk.0) + 1) * u64::from(record.state.spec.chunk_size))
            .min(u64::from(end)) as u32;
    }
    (start, end, owners)
}

/// `GET /v1/batch/{id}/results`: every task's outcome, gathered from the
/// nodes' ledgers.
pub async fn results_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    AxumPath(batch_id): AxumPath<u64>,
    Query(query): Query<ResultsQuery>,
    headers: HeaderMap,
) -> Response {
    if query.limit == Some(0) {
        return error(StatusCode::BAD_REQUEST, "result limit must be positive");
    }
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
            "/v1/batch/{batch_id}/results?failed={}&limit={limit}{}{}",
            query.failed,
            query.after.map_or(String::new(), |a| format!("&after={a}")),
            query.index.map_or(String::new(), |i| format!("&index={i}"))
        );
        return forward_get_to_leader(&state, council, &path, &headers).await;
    }
    let (start, end, owners) = result_window(record, &query);
    let mut per_node = Vec::new();
    let mut unreachable = Vec::new();
    let mut calls = tokio::task::JoinSet::new();
    let mut known = std::collections::BTreeSet::new();
    for (node, url) in every_node(&state).await {
        if !owners.contains(&node) {
            continue;
        }
        known.insert(node.clone());
        // One row more than asked for, so a full answer can say it's truncated.
        let asked = limit.saturating_add(1);
        let state = state.clone();
        let failed = query.failed;
        calls.spawn(async move {
            (
                node,
                node_results(&state, url.as_deref(), batch_id, failed, asked, start, end).await,
            )
        });
        // Bound simultaneous RPCs and buffered responses even with tiny chunks.
        if calls.len() >= 8
            && let Some(Ok((node, result))) = calls.join_next().await
        {
            match result {
                Ok(Some(rows)) => per_node.push((node, rows)),
                Ok(None) | Err(_) => unreachable.push(node),
            }
        }
    }
    unreachable.extend(owners.difference(&known).cloned());
    while let Some(done) = calls.join_next().await {
        let Ok((node, result)) = done else { continue };
        match result {
            Ok(Some(rows)) => per_node.push((node, rows)),
            Ok(None) => unreachable.push(node),
            Err(reason) => {
                eprintln!("bun: task array {batch_id}: results from {node}: {reason}");
                unreachable.push(node);
            }
        }
    }
    let scanned_cap = per_node
        .iter()
        .filter(|(_, rows)| rows.len() > limit)
        .filter_map(|(_, rows)| rows.get(limit - 1).map(|r| r.index))
        .min();
    let (rows, capped) = merge_rows(record, per_node, limit);
    let next_after = if query.index.is_some() || end == 0 {
        None
    } else if capped {
        rows.last().map(|r| r.index)
    } else if let Some(cursor) = scanned_cap {
        Some(cursor)
    } else if end < record.state.spec.count {
        Some(end - 1)
    } else {
        None
    };
    let truncated = capped || next_after.is_some();
    Json(TaskResults {
        next_after,
        retention_seconds: crate::meat::task_array_store::TERMINAL_RETENTION_SECS,
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
    headers: HeaderMap,
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
    let permissions = super::api::permission_map(&state).await;
    if let Err(response) = crate::sesame::auth::authorize_permission(
        auth.as_deref(),
        crate::config::PermissionAction::Logs,
        &record.name,
        &record.namespace,
        &permissions,
    ) {
        return response;
    }
    if let Some(council) = follower_council(&state).await {
        let path = format!("/v1/batch/{batch_id}/tasks/{index}/logs");
        return forward_get_to_leader(&state, council, &path, &headers).await;
    }
    if index >= record.state.spec.count {
        return error(
            StatusCode::NOT_FOUND,
            format!("task array {batch_id} has no task {index}"),
        );
    }
    let chunk = record.state.spec.chunk_of(index).expect("validated index");
    let Some((owner, grant)) = record.state.accepted_grant(chunk) else {
        return error(StatusCode::NOT_FOUND, "task has no accepted outcome yet");
    };
    let Some((_, url)) = every_node(&state)
        .await
        .into_iter()
        .find(|(node, _)| node == &owner)
    else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "accepted task owner is unavailable",
        );
    };
    let output = match url {
        None => match &state.task_arrays.node {
            Some(node) => node.task_output_grant(batch_id, index, grant).await.ok(),
            None => None,
        },
        Some(url) => {
            let url =
                format!("{url}/v1/batch/array/{batch_id}/local/tasks/{index}/logs?grant={grant}");
            match fetch_from_node(&state, &url).await {
                Ok(response) if response.status().is_success() => {
                    response.bytes().await.ok().map(|bytes| bytes.to_vec())
                }
                Ok(response) if response.status() == StatusCode::NOT_FOUND => None,
                _ => {
                    return error(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "accepted task owner's output is unavailable",
                    );
                }
            }
        }
    };
    if let Some(bytes) = output {
        return bytes.into_response();
    }
    error(
        StatusCode::NOT_FOUND,
        format!(
            "no node kept output for task {index}; singleton and failed-task output is retained"
        ),
    )
}

/// Gather bounded parent/profile summaries, excluding a parent's child arrays.
pub async fn summaries(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> Vec<serde_json::Value> {
    let arrays = read_task_arrays(state).await;
    let children: std::collections::BTreeSet<_> = arrays
        .manifests()
        .flat_map(|(_, m)| m.cohorts.iter().map(|(_, id)| *id))
        .collect();
    let mut rows = Vec::new();
    for (id, manifest) in arrays.manifests() {
        if authorize_manifest(auth, manifest, &arrays).is_ok() {
            rows.push(manifest_summary(id, manifest, &arrays, &state.task_arrays).await);
        }
    }
    for (id, record) in arrays.iter() {
        if children.contains(&id)
            || crate::sesame::auth::authorize_scoped(auth, &record.name, &record.namespace).is_err()
        {
            continue;
        }
        let mut row = array_summary(id, record, &state.task_arrays.node_views(id).await);
        let counts = record.state.summary();
        enrich_summary(&mut row, &arrays, id);
        row["rates"] = serde_json::json!(
            state
                .task_arrays
                .rates(id, (counts.succeeded, counts.failed))
                .await
        );
        rows.push(row);
    }
    for (key, record) in arrays.jobs().definitions() {
        let Some(cron) = &record.definition.cron else {
            continue;
        };
        let Some((namespace, name)) = key.split_once('/') else {
            continue;
        };
        if crate::sesame::auth::authorize_scoped(auth, name, namespace).is_err() {
            continue;
        }
        rows.push(
            serde_json::json!({"kind":"schedule", "name":name, "namespace":namespace,
            "status":"Scheduled", "batch_id":null, "revision":record.revision,
            "cron":cron, "last_observed_minute":record.last_observed_minute,
            "total":record.definition.tasks.count}),
        );
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r["batch_id"].as_u64().unwrap_or(0)));
    rows
}

/// Aggregate admission-bounded batch list; no per-task objects are returned.
pub async fn summaries_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    Json(serde_json::json!({ "batches": summaries(&state, auth.as_deref()).await })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::EnvValue;
    use crate::meat::index_set::IndexRangeSet;
    use crate::meat::task_array_state::TaskArrayState;

    fn template() -> JobSpec {
        JobSpec {
            runtime: crate::config::job::JobRuntime::Process,
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
            max_attempts: None,
            task_timeout_secs: None,
            overlap: None,
            replay_unknown: false,
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
    fn tiny_chunks_page_across_at_most_eight_workers_without_skipping_indexes() {
        use crate::meat::task_array::ChunkId;
        use crate::meat::task_array_state::ChunkResult;
        let mut state = TaskArrayState::new(
            TaskArraySpec {
                chunk_size: 1,
                ..TaskArraySpec::with_count(16)
            },
            0,
        )
        .unwrap();
        for chunk in 0..16 {
            let node = NodeId::new(format!("worker-{chunk}"));
            state
                .grant(&node, &IndexRangeSet::from_range(chunk..=chunk))
                .unwrap();
            state
                .complete(
                    &node,
                    &ChunkResult {
                        chunk: ChunkId(chunk),
                        attempt: 1,
                        succeeded: 1,
                        failed_count: 0,
                        failed_indices: IndexRangeSet::new(),
                        not_run: 0,
                        retried: 0,
                        duration_counts: [0; 16],
                    },
                )
                .unwrap();
        }
        let record = TaskArrayRecord {
            name: "tiny".into(),
            namespace: "default".into(),
            template: template(),
            state,
            terminal_at_epoch_secs: Some(1),
        };
        let (start, end, owners) = result_window(&record, &ResultsQuery::default());
        assert_eq!((start, end, owners.len()), (0, 8, 8));
        let (start, end, owners) = result_window(
            &record,
            &ResultsQuery {
                after: Some(7),
                ..Default::default()
            },
        );
        assert_eq!((start, end, owners.len()), (8, 16, 8));
        let (start, end, owners) = result_window(
            &record,
            &ResultsQuery {
                index: Some(14),
                ..Default::default()
            },
        );
        assert_eq!((start, end, owners.len()), (14, 15, 1));
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
        no_exec.template.image = None;
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
        assert_eq!(validate_submission(&mut secret).unwrap(), "default");

        let mut oversized = submission();
        oversized.template.command = Some(vec!["x".repeat(16 * 1024)]);
        assert!(
            validate_submission(&mut oversized)
                .unwrap_err()
                .contains("16 KiB")
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
                    duration_counts: [0; 16],
                    chunk: crate::meat::task_array::ChunkId(0),
                    attempt: 1,
                    succeeded: 10 - failed.len() as u32,
                    failed_count: failed_indices.len() as u32,
                    failed_indices,
                    not_run: 0,
                    retried: 0,
                },
            )
            .unwrap();
        TaskArrayRecord {
            terminal_at_epoch_secs: None,
            name: "render".to_string(),
            namespace: "default".to_string(),
            template: template(),
            state,
        }
    }

    fn row(index: u32, succeeded: bool) -> TaskResultRow {
        TaskResultRow {
            grant_attempt: 1,
            index,
            attempts: 1,
            succeeded,
            not_run: false,
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
                (NodeId::new("old-worker"), vec![row(2, true), row(4, true)]),
                (
                    NodeId::new("a"),
                    vec![row(0, true), row(1, true), row(2, false), row(3, true)],
                ),
            ],
            10,
        );
        let got: Vec<(u32, bool)> = rows.iter().map(|r| (r.index, r.succeeded)).collect();
        assert_eq!(got, vec![(0, true), (1, true), (2, false), (3, true)]);
        assert!(!truncated);

        let (rows, truncated) = merge_rows(
            &record,
            vec![(NodeId::new("a"), vec![row(0, true), row(1, true)])],
            1,
        );
        assert_eq!(rows.len(), 1);
        assert!(truncated);
    }

    #[test]
    fn activity_requires_receipts_from_every_reporting_backend() {
        let mut record = record_with_failures(&[]);
        record.state = crate::meat::task_array_state::TaskArrayState::new(
            crate::meat::task_array::TaskArraySpec::with_count(10),
            100,
        )
        .unwrap();
        record.terminal_at_epoch_secs = None;
        assert!(array_summary(9, &record, &[])["active_commands"].is_null());
        let mut nodes = vec![NodeView {
            node: NodeId::new("first"),
            slots: 8,
            refused: None,
            counters: super::super::task_array_node::NodeArrayCounters {
                running: 8,
                active_commands: Some(3),
                ..Default::default()
            },
        }];
        let summary = array_summary(9, &record, &nodes);
        assert_eq!(summary["active_commands"], 3);
        assert_eq!(summary["other_in_flight_attempts"], 5);
        nodes.push(NodeView {
            node: NodeId::new("unsupported"),
            slots: 2,
            refused: None,
            counters: Default::default(),
        });
        assert!(array_summary(9, &record, &nodes)["active_commands"].is_null());
        nodes[1].refused = Some("unsupported".into());
        assert_eq!(array_summary(9, &record, &nodes)["active_commands"], 3);
    }

    #[test]
    fn native_profile_summary_discloses_idle_helper_cost_without_claiming_live_usage() {
        let record = record_with_failures(&[]);
        let summary = array_summary(9, &record, &[]);
        assert_eq!(summary["runtime"], "process");
        assert_eq!(summary["idle_executor_reservation"]["cpu_millicores"], 1010);
        assert_eq!(
            summary["idle_executor_reservation"]["memory_bytes"],
            72 << 20
        );
        assert_eq!(
            summary["idle_executor_reservation_semantics"],
            "profile_per_compatible_executor; process_requires_rootful_linux_native_backend; not_live_node_usage"
        );
    }

    #[test]
    fn whole_run_rate_freezes_at_accepted_terminal_time_and_empty_activity_is_unknown() {
        let mut record = record_with_failures(&[2, 3, 7]);
        record.state.submitted_at_epoch_secs = 100;
        record.terminal_at_epoch_secs = Some(110);
        let summary = array_summary(5, &record, &[]);
        assert_eq!(summary["elapsed_seconds"], 10);
        assert_eq!(summary["whole_run_successes_per_second"], 0.7);
        assert_eq!(summary["active_commands"], 0);
        record.terminal_at_epoch_secs = Some(100);
        assert!(array_summary(5, &record, &[])["whole_run_successes_per_second"].is_null());
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
