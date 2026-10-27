//! Common definition/run admission and explicit unknown-outcome replay.

use super::{
    api::ApiState,
    task_array_leader::{TaskArrayWriteError, read_task_arrays, write_task_array},
};
use crate::meat::{
    job::{JobDefinition, JobWrite, RunTrigger},
    task_array_store::TaskArrayWrite,
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

/// A reusable definition, optionally triggered immediately by an idempotent request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSubmitRequest {
    /// Logical workload name.
    pub name: String,
    /// Defaults to the template namespace, then `default`.
    pub namespace: Option<String>,
    /// Complete execution and scheduling policy; omitted task count means one.
    pub definition: JobDefinition,
    /// Required for manual runs; omit to register a cron definition without a run.
    pub request_id: Option<String>,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({"error":message.into()}))).into_response()
}

pub(crate) fn write_error(error: TaskArrayWriteError) -> Response {
    let status = match error {
        TaskArrayWriteError::Refused(_) => StatusCode::CONFLICT,
        TaskArrayWriteError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    self::error(status, error.to_string())
}

/// Authorise and bind the encrypted template before recording runnable work.
#[allow(clippy::result_large_err)]
pub(crate) async fn admit_definition(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    binder: Option<&crate::pickle::binding::ImageBinder>,
    name: &str,
    namespace: &str,
    definition: &mut JobDefinition,
    lease: Option<&crate::testkit::lease::TestLease>,
) -> Result<(), Response> {
    if !crate::config::valid_workload_label(name) || !crate::config::valid_workload_label(namespace)
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "job name and namespace must be DNS labels",
        ));
    }
    if definition
        .template
        .namespace
        .as_deref()
        .is_some_and(|ns| ns != namespace)
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "definition namespace disagrees with submission",
        ));
    }
    // A test lease namespace belongs to its lease; only that lease's jobs
    // may run there, and only they may use the lease's own images.
    if crate::testkit::lease::valid_test_namespace(namespace)
        && lease.is_none_or(|lease| lease.namespace != namespace)
    {
        return Err(error(
            StatusCode::CONFLICT,
            "common jobs cannot use test lease namespaces without lease ownership",
        ));
    }
    crate::testkit::lease::authorise_image_references(
        definition.template.image.as_deref(),
        lease.map(|lease| (lease, crate::testkit::lease::now_unix_millis())),
    )
    .map_err(|e| error(StatusCode::CONFLICT, e.to_string()))?;
    definition.template.namespace = Some(namespace.into());
    definition
        .validate()
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    let permissions = super::api::permission_map(state).await;
    crate::sesame::auth::authorize_workload(
        auth,
        name,
        namespace,
        definition.template.is_host(),
        &permissions,
    )?;
    if state.council.is_none() {
        let resources = super::api::ask_agent_bounded(&state.cmd_tx, |response| {
            super::agent::AgentCommand::CurrentResources { response }
        })
        .await?;
        let app = crate::config::fingerprint::app_resource_key(name, namespace);
        if resources.iter().any(|resource| resource.resource == app) {
            return Err(error(
                StatusCode::CONFLICT,
                "an application already owns this job identity",
            ));
        }
    }
    super::task_array_api::admit_template_image(state, binder, &mut definition.template).await?;
    let refusal = match sole_node_refusal(state, name, namespace, definition) {
        Some(reason) => Some(reason),
        None => cluster_refusal(state, name, namespace, definition).await,
    };
    if let Some(reason) = refusal {
        return Err(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("no node can run job {namespace}/{name}: {reason}"),
        ));
    }
    Ok(())
}

/// Why this node would refuse the definition's host tasks, when it is the
/// only node that could run them (standalone: no council, no gossip peers).
/// Admitting host work nobody can run (most often under `mount_isolation`,
/// #714) would only fail a second later; refusing it names the reason at
/// once. Container work is left to the tick, which fails a run once every
/// node has refused it.
fn sole_node_refusal(
    state: &ApiState,
    name: &str,
    namespace: &str,
    definition: &JobDefinition,
) -> Option<String> {
    if state.council.is_some() || state.membership.is_some() || !definition.template.is_host() {
        return None;
    }
    let node = state.task_arrays.node.as_ref()?;
    let record = definition_record(name, namespace, definition)?;
    let self_node = crate::meat::NodeId::new(state.node_name.as_deref().unwrap_or("local"));
    node.refusal(&super::task_array_leader::assignment_for(
        0, &record, &self_node,
    ))
}

/// Why no live member of a cluster can run the definition, judged by the
/// leader from the job capabilities every member advertises over gossip
/// (#788) and the allowlists in their capability reports. `None` standalone, while any member might run it, or before
/// gossip names any member. The leader's tick stays the backstop for runs
/// this lets through and for nodes that change after admission.
async fn cluster_refusal(
    state: &ApiState,
    name: &str,
    namespace: &str,
    definition: &JobDefinition,
) -> Option<String> {
    state.council.as_ref()?;
    let record = definition_record(name, namespace, definition)?;
    // Gossip only says whether a node's allowlist names anything; a node's
    // capability report (#702) carries the list itself.
    let allowlists: std::collections::HashMap<crate::meat::NodeId, Vec<std::path::PathBuf>> = state
        .aggregated_rx
        .as_ref()
        .map(|rx| {
            rx.borrow()
                .capabilities
                .iter()
                .map(|(node, report)| {
                    (
                        node.clone(),
                        report.capabilities.process.allowed_binaries.clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let members = state.membership.as_ref()?.read().await;
    let nodes: Vec<_> = members
        .iter()
        .map(|member| super::task_array_leader::AdvertisedNode {
            node: member.node_id.clone(),
            jobs: member.jobs,
            allowed_binaries: allowlists.get(&member.node_id).map(Vec::as_slice),
        })
        .collect();
    super::task_array_leader::admission_refusal(&record, &nodes)
}

/// The record a run of `definition` would have, for asking whether any
/// node could take it before anything is written.
fn definition_record(
    name: &str,
    namespace: &str,
    definition: &JobDefinition,
) -> Option<crate::meat::task_array_store::TaskArrayRecord> {
    Some(crate::meat::task_array_store::TaskArrayRecord {
        name: name.to_string(),
        namespace: namespace.to_string(),
        template: definition.template.clone(),
        state: crate::meat::task_array_state::TaskArrayState::new(definition.tasks.clone(), 0)
            .ok()?,
        terminal_at_epoch_secs: None,
        quota_blocked: None,
        unrunnable: None,
    })
}

/// `POST /v1/jobs/runs`: manual singleton/array run or durable cron registration.
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
    let mut request: JobSubmitRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let namespace = request
        .namespace
        .clone()
        .or_else(|| request.definition.template.namespace.clone())
        .unwrap_or_else(|| "default".into());
    if let Err(response) = crate::sesame::auth::authorize_scoped(auth, &request.name, &namespace) {
        return response;
    }
    crate::bun::audit::annotate_workload(&request.name, &namespace);
    if let Some(council) = super::task_array_api::council_to_forward_to(&state).await {
        return super::batch::forward_to_leader(&state, council, "/v1/jobs/runs", body, &headers)
            .await;
    }
    let _gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    let permissions = super::api::permission_map(&state).await;
    if let Err(response) = crate::sesame::auth::authorize_workload(
        auth,
        &request.name,
        &namespace,
        request.definition.template.is_host(),
        &permissions,
    ) {
        return response;
    }
    if let Err(response) = admit_definition(
        &state,
        auth,
        binder.as_deref(),
        &request.name,
        &namespace,
        &mut request.definition,
        None,
    )
    .await
    {
        return response;
    }
    let trigger = match request.request_id {
        Some(request_id) => Some(RunTrigger::Manual { request_id }),
        None if request.definition.cron.is_some() => None,
        None => return error(StatusCode::BAD_REQUEST, "manual runs require request_id"),
    };
    let count = request.definition.tasks.count;
    let chunks = request.definition.tasks.chunk_count();
    match write_task_array(&state, TaskArrayWrite::Job(Box::new(JobWrite::Put { name: request.name.clone(), namespace: namespace.clone(), definition: Box::new(request.definition), trigger, now_epoch_secs: crate::meat::batch_ids::epoch_now_secs() }))).await {
        Ok(id) => (StatusCode::ACCEPTED, Json(serde_json::json!({"batch_id":id,"name":request.name,"namespace":namespace,"count":count,"chunks":chunks}))).into_response(),
        Err(e) => write_error(e),
    }
}

/// Explicit decision tied to the exact unknown grants, so a retried acknowledgement cannot replay a later owner.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayRequest {
    /// Unknown worker from the run summary.
    pub node: crate::meat::NodeId,
    /// Immutable fingerprint shown beside this unknown owner.
    pub grant_digest: String,
    /// The caller accepts that side effects may occur again.
    pub acknowledged: bool,
}

/// `POST /v1/jobs/runs/{id}/replay`: a user acknowledgement, never automatic service replay.
pub async fn replay_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let auth = auth.as_deref();
    if let Err(response) =
        crate::sesame::auth::authorize_user(auth, crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let request: ReplayRequest = match serde_json::from_str(&body) {
        Ok(request) => request,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    if !request.acknowledged {
        return error(
            StatusCode::BAD_REQUEST,
            "replay requires acknowledged=true and accepts repeated side effects",
        );
    }
    let arrays = read_task_arrays(&state).await;
    let Some(record) = arrays.get(id) else {
        return error(StatusCode::NOT_FOUND, "unknown run");
    };
    let permissions = super::api::permission_map(&state).await;
    if let Err(response) = crate::sesame::auth::authorize_workload(
        auth,
        &record.name,
        &record.namespace,
        record.template.is_host(),
        &permissions,
    ) {
        return response;
    }
    if let Some(council) = super::task_array_api::council_to_forward_to(&state).await {
        return super::batch::forward_to_leader(
            &state,
            council,
            &format!("/v1/jobs/runs/{id}/replay"),
            body,
            &headers,
        )
        .await;
    }
    match write_task_array(
        &state,
        TaskArrayWrite::Replay {
            batch_id: id,
            node: request.node,
            grant_digest: request.grant_digest,
            now_epoch_secs: crate::meat::batch_ids::epoch_now_secs(),
        },
    )
    .await
    {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"batch_id":id,"replay_acknowledged":true})),
        )
            .into_response(),
        Err(e) => write_error(e),
    }
}

/// `GET /v1/jobs/definitions`: bounded, scoped reusable definitions and cron cursors.
pub async fn definitions_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    let arrays = read_task_arrays(&state).await;
    let rows: Vec<_> = arrays.jobs().definitions().filter_map(|(key, record)| {
        let (namespace, name) = key.split_once('/')?;
        crate::sesame::auth::authorize_scoped(auth.as_deref(), name, namespace).ok()?;
        Some(serde_json::json!({"name":name,"namespace":namespace,"revision":record.revision,"count":record.definition.tasks.count,"cron":record.definition.cron,"last_observed_minute":record.last_observed_minute,"skipped":record.skipped,"skipped_count":record.skipped_count,"history":record.history}))
    }).collect();
    Json(serde_json::json!({"definitions":rows})).into_response()
}

/// Disable future cron occurrences; existing runs retain their own cancellation contract.
pub async fn disable_schedule_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    Path((name, namespace)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    if let Err(response) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &name, &namespace)
    {
        return response;
    }
    let permissions = super::api::permission_map(&state).await;
    if let Err(response) = crate::sesame::auth::authorize_permission(
        auth.as_deref(),
        crate::config::PermissionAction::Deploy,
        &name,
        &namespace,
        &permissions,
    ) {
        return response;
    }
    if let Some(council) = super::task_array_api::council_to_forward_to(&state).await {
        return super::batch::forward_to_leader(
            &state,
            council,
            &format!("/v1/jobs/definitions/{name}/{namespace}/disable"),
            String::new(),
            &headers,
        )
        .await;
    }
    let arrays = read_task_arrays(&state).await;
    let Some(record) = arrays.jobs().definition(&namespace, &name) else {
        return error(StatusCode::NOT_FOUND, "unknown definition");
    };
    let mut definition = record.definition.clone();
    definition.cron = None;
    match write_task_array(
        &state,
        TaskArrayWrite::Job(Box::new(JobWrite::Put {
            name,
            namespace,
            definition: Box::new(definition),
            trigger: None,
            now_epoch_secs: crate::meat::batch_ids::epoch_now_secs(),
        })),
    )
    .await
    {
        Ok(_) => Json(serde_json::json!({"schedule_disabled":true})).into_response(),
        Err(e) => write_error(e),
    }
}

/// Existing stop/delete commands use the common definition and run state.
pub(crate) async fn stop_definition(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    name: &str,
    namespace: &str,
    forget: bool,
    headers: &HeaderMap,
) -> Option<Response> {
    let arrays = read_task_arrays(state).await;
    arrays.jobs().definition(namespace, name)?;
    if let Some(council) = super::task_array_api::council_to_forward_to(state).await {
        return Some(
            super::batch::forward_to_leader(
                state,
                council,
                &format!(
                    "/v1/{}/{name}/{namespace}",
                    if forget { "delete" } else { "stop" }
                ),
                String::new(),
                headers,
            )
            .await,
        );
    }
    if let Err(response) = crate::sesame::auth::authorize_scoped(auth, name, namespace) {
        return Some(response);
    }
    let _gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    Some(match write_task_array(state, TaskArrayWrite::StopDefinition { name: name.into(), namespace: namespace.into(), forget, now_epoch_secs: crate::meat::batch_ids::epoch_now_secs() }).await {
        Ok(_) => (StatusCode::ACCEPTED, Json(serde_json::json!({"name":name,"namespace":namespace,"schedule_disabled":true,"runs_stopping":true}))).into_response(), Err(error) => write_error(error),
    })
}
