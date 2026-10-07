//! Deploy routes: cancel, active and recorded operations, history and
//! rollback.

use super::*;

/// Request node-local cooperative cancellation under the same authority as apply.
pub(super) async fn deploy_cancel_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return response;
    }
    let snapshot = match deploy_operation_snapshot(&state).await {
        Ok(snapshot) => snapshot,
        Err(response) => return response,
    };
    let Some(operation) = snapshot
        .active_deploys
        .iter()
        .chain(&snapshot.history)
        .find(|operation| operation.id.as_str() == id)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "deploy operation not found on this node"})),
        )
            .into_response();
    };
    let permissions = permission_map(&state).await;
    for target in &operation.targets {
        if let Err(response) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), &target.name, &target.namespace)
        {
            return response;
        }
        if let Err(response) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            &target.name,
            &target.namespace,
            &permissions,
        ) {
            return response;
        }
    }
    let arrays = crate::bun::task_array_leader::read_task_arrays(&state).await;
    if arrays.deployments().any(|(operation, _)| operation == id) {
        if let Some(council) = &state.council
            && !council.is_leader().await
        {
            return crate::bun::batch::forward_to_leader(
                &state,
                council,
                &format!("/v1/deploys/operations/{id}/cancel"),
                String::new(),
                &headers,
            )
            .await;
        }
        let _gate = if state.council.is_none() {
            Some(state.task_arrays.apply_gate.clone().lock_owned().await)
        } else {
            None
        };
        if let Err(error) = crate::bun::task_array_leader::write_task_array(
            &state,
            crate::meat::task_array_store::TaskArrayWrite::DeployCancel {
                operation_id: id.clone(),
                now_epoch_secs: crate::meat::batch_tracker::epoch_now_secs(),
            },
        )
        .await
        {
            return crate::bun::job_api::write_error(error);
        }
        let arrays = crate::bun::task_array_leader::read_task_arrays(&state).await;
        if let Some((_, record)) = arrays.deployments().find(|(operation, _)| *operation == id) {
            let operation = durable_operation(&id, record, &arrays);
            return (
                if operation.outcome.is_some() {
                    StatusCode::OK
                } else {
                    StatusCode::ACCEPTED
                },
                Json(operation),
            )
                .into_response();
        }
        return (StatusCode::NOT_FOUND, "deploy operation no longer retained").into_response();
    }
    let (response, result) = oneshot::channel();
    let request = async {
        state
            .cmd_tx
            .send(AgentCommand::CancelDeploy {
                operation_id: id.into(),
                response,
            })
            .await
            .ok()?;
        result.await.ok()
    };
    match tokio::time::timeout(std::time::Duration::from_secs(2), request).await {
        Ok(Some(Some(operation))) => {
            let status = if operation.outcome.is_some() {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (status, Json(operation)).into_response()
        }
        Ok(Some(None)) => {
            (StatusCode::NOT_FOUND, "deploy operation no longer retained").into_response()
        }
        Ok(None) => (StatusCode::SERVICE_UNAVAILABLE, "agent unavailable").into_response(),
        Err(_) => (
            StatusCode::GATEWAY_TIMEOUT,
            "cancellation receipt unknown; query or retry the same operation ID",
        )
            .into_response(),
    }
}

/// `GET /v1/deploys/active` — bounded, scoped operation summaries.
pub(super) async fn deploys_active_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    match scoped_snapshot(&state, auth.as_deref()).await {
        Ok(snapshot) => Json(crate::bun::deploy_operations::ActiveDeployOperations {
            active_deploys: snapshot.active_deploys,
        })
        .into_response(),
        Err(response) => response,
    }
}

/// `GET /v1/deploys/operations` — active intent and bounded settled receipts.
pub(super) async fn deploys_operations_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    match scoped_snapshot(&state, auth.as_deref()).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(response) => response,
    }
}

#[allow(clippy::result_large_err)]
async fn scoped_snapshot(
    state: &ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
) -> Result<crate::bun::deploy_operations::DeployOperationSnapshot, Response> {
    let mut snapshot = deploy_operation_snapshot(state).await?;
    let visible = |operation: &crate::bun::deploy_operations::DeployOperation| {
        operation.targets.iter().all(|target| {
            crate::sesame::auth::authorize_scoped(auth, &target.name, &target.namespace).is_ok()
        })
    };
    snapshot.active_deploys.retain(visible);
    snapshot.history.retain(visible);
    Ok(snapshot)
}

fn durable_operation(
    id: &str,
    record: &crate::meat::job_deploy::DeploymentRecord,
    arrays: &crate::meat::task_array_store::TaskArrays,
) -> crate::bun::deploy_operations::DeployOperation {
    use crate::bun::deploy_operations::{
        DeployOperation, DeployOperationOutcome, DeployOperationPhase, DeployTarget,
        DeployTargetKind,
    };
    let mut targets: Vec<_> = record
        .config
        .app
        .iter()
        .map(|(name, spec)| DeployTarget {
            kind: DeployTargetKind::App,
            name: name.clone(),
            namespace: spec.namespace.clone().unwrap_or_else(|| "default".into()),
        })
        .chain(record.config.job.iter().map(|(name, spec)| DeployTarget {
            kind: DeployTargetKind::Job,
            name: name.clone(),
            namespace: spec.namespace.clone().unwrap_or_else(|| "default".into()),
        }))
        .collect();
    targets.sort();
    let unknown = record.runs().any(|id| {
        arrays
            .jobs()
            .run(id)
            .is_some_and(|run| !run.unknown_owners.is_empty())
    });
    let outcome = record.outcome.map(|outcome| match outcome {
        crate::meat::job_deploy::DeploymentOutcome::Completed => DeployOperationOutcome::Completed,
        crate::meat::job_deploy::DeploymentOutcome::Failed => DeployOperationOutcome::Failed,
        crate::meat::job_deploy::DeploymentOutcome::Cancelled => DeployOperationOutcome::Cancelled,
    });
    DeployOperation {
        id: id.into(),
        phase: if record.completed {
            DeployOperationPhase::Finished
        } else if record.apps_committed {
            DeployOperationPhase::DeployingJobs
        } else {
            DeployOperationPhase::Accepted
        },
        outcome,
        started_at: record.submitted_at_epoch_secs,
        phase_changed_at: record.finished_at.unwrap_or(record.submitted_at_epoch_secs),
        finished_at: record.finished_at,
        cancellation_requested_at: record.cancelled.then_some(record.submitted_at_epoch_secs),
        targets,
        current_target: None,
        message: if record.completed {
            "durable deployment settled"
        } else if unknown {
            "unknown run ownership; inspect runs before acknowledging replay"
        } else if record.cancelled {
            "cancellation recorded; waiting for positive retirement"
        } else if record.apps_committed {
            "apps published; jobs admitted"
        } else {
            "waiting for accepted successful hooks"
        }
        .into(),
    }
}

// Response is the exact HTTP failure returned by callers.
#[allow(clippy::result_large_err)]
pub(super) async fn deploy_operation_snapshot(
    state: &ApiState,
) -> Result<crate::bun::deploy_operations::DeployOperationSnapshot, Response> {
    use crate::bun::deploy_operations::DeployOperationSnapshot;
    let arrays = crate::bun::task_array_leader::read_task_arrays(state).await;
    let mut snapshot = DeployOperationSnapshot {
        active_deploys: Vec::new(),
        history: Vec::new(),
    };
    for (id, record) in arrays.deployments() {
        let operation = durable_operation(id, record, &arrays);
        if record.completed {
            snapshot.history.push(operation);
        } else {
            snapshot.active_deploys.push(operation);
        }
    }
    let (response, result) = oneshot::channel();
    let request = async {
        state
            .cmd_tx
            .send(AgentCommand::DeployOperations { response })
            .await
            .ok()?;
        result.await.ok()
    };
    match tokio::time::timeout(std::time::Duration::from_secs(2), request).await {
        Ok(Some(local)) => {
            snapshot.active_deploys.extend(local.active_deploys);
            snapshot.history.extend(local.history);
        }
        _ if snapshot.active_deploys.is_empty() && snapshot.history.is_empty() => {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "agent unavailable").into_response());
        }
        _ => {}
    }
    snapshot
        .history
        .sort_by_key(|operation| std::cmp::Reverse(operation.finished_at));
    snapshot.history.truncate(70);
    Ok(snapshot)
}

#[derive(Deserialize)]
pub(super) struct NamespaceQuery {
    pub(super) namespace: Option<String>,
    /// Answer from this node's records only (set on fan-out requests).
    #[serde(default)]
    pub(super) local: bool,
}

/// An app's deploy history across the cluster.
///
/// Every node records its own rollout of the replicas placed on it, so the
/// full history is the merge of every live member's records. `local_only`
/// answers with this node's records alone, as a peer does for a fan-out.
pub(super) async fn cluster_deploy_history(
    state: &ApiState,
    app: &str,
    namespace: &str,
    local_only: bool,
) -> crate::bun::cluster_view::ClusterDeployHistory {
    let local: Vec<DeployHistoryEntry> = match &state.deploy_history {
        Some(history) => history
            .read()
            .await
            .iter()
            .filter(|entry| entry.app_id.name == app && entry.app_id.namespace == namespace)
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let mut answers = vec![(local_node_name(state), local)];
    let mut warnings = Vec::new();
    if !local_only {
        let app_segment: String = url::form_urlencoded::byte_serialize(app.as_bytes()).collect();
        let namespace_value: String =
            url::form_urlencoded::byte_serialize(namespace.as_bytes()).collect();
        let path =
            format!("/v1/deploys/history/{app_segment}?namespace={namespace_value}&local=true");
        let (peers, failures) = fan_out_to_peers::<crate::bun::cluster_view::ClusterDeployHistory>(
            state,
            &path,
            CLUSTER_STATUS_TIMEOUT,
        )
        .await;
        answers.extend(peers.into_iter().map(|(node, view)| {
            let entries = view.history.into_iter().map(|entry| entry.row).collect();
            (node, entries)
        }));
        warnings = failures;
    }
    crate::bun::cluster_view::ClusterDeployHistory {
        app: app.to_string(),
        namespace: namespace.to_string(),
        history: crate::bun::cluster_view::merge_deploy_history(answers),
        warnings,
    }
}

/// `GET /v1/deploys/history/{app}` — deploy history for an app, from every
/// node, each entry tagged with the node that recorded it.
///
/// The namespace rides in as a query parameter rather than a path segment
/// so the route (and every client bookmarking it) keeps its shape. It
/// matters for more than tidiness: since DEP1 two apps of the same name can
/// coexist in different namespaces, so filtering on the bare name returned
/// both tenants' history to whoever asked (C3).
pub(super) async fn deploys_history_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(app): Path<String>,
    Query(query): Query<NamespaceQuery>,
) -> Response {
    let namespace = query.namespace.as_deref().unwrap_or("default");
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, namespace) {
        return resp;
    }
    Json(cluster_deploy_history(&state, &app, namespace, query.local).await).into_response()
}

/// `POST /v1/rollback/{app}/{namespace}` — redeploy the app's previous
/// successful spec (X3).
///
/// "Previous" means the last-but-one distinct successful deploy: the
/// most recent completed entry is the *current* version, so rollback
/// targets the one before it. Re-applies through the same path as
/// `apply` (Raft in cluster mode, local deploy otherwise). The recorded
/// spec carries the digest it ran, so the rollback runs those bytes; only
/// a record from before binding (a bare tag) is resolved again.
pub(super) async fn rollback_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    binder: Option<axum::Extension<crate::pickle::binding::ImageBinder>>,
    State(state): State<ApiState>,
    Path((app, namespace)): Path<(String, String)>,
) -> Response {
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    if let Err(resp) = crate::sesame::auth::authorize_scoped(auth.as_deref(), &app, &namespace) {
        return resp;
    }
    if let Err(resp) = enforce_permission(
        &state,
        auth.as_deref(),
        crate::config::PermissionAction::Deploy,
        &app,
        &namespace,
    )
    .await
    {
        return resp;
    }
    if state.deploy_history.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "deploy history unavailable"})),
        )
            .into_response();
    }

    // Every node's records, not just this one's: the node asked may never
    // have run the app. The spec in Raft is the current version.
    let history: Vec<DeployHistoryEntry> = cluster_deploy_history(&state, &app, &namespace, false)
        .await
        .history
        .into_iter()
        .map(|tagged| tagged.row)
        .collect();
    let current = match &state.council {
        Some(council) => council
            .desired_state()
            .await
            .apps
            .get(&crate::meat::AppId::new(&app, &namespace))
            .cloned(),
        None => None,
    };
    let target_spec = crate::bun::cluster_view::rollback_target(&history, current.as_ref());

    let Some(mut spec) = target_spec else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("no previous successful deploy to roll {app} back to")
            })),
        )
            .into_response();
    };

    // This node recorded its own share, under the ordinals the leader gave
    // it. Those belong to the placement, not the spec, and an apply refuses
    // them; the leader numbers the replicas again (#398).
    spec.ordinals = None;
    // Re-apply the previous spec through the standard deploy path.
    let mut config = Config::default();
    config.app.insert(app.clone(), spec);
    let raw = toml::to_string(&config).unwrap_or_default();
    let binder = binder.map(|extension| extension.0);

    if let Some(council) = &state.council {
        return cluster_apply(
            state.clone(),
            Arc::clone(council),
            config,
            raw,
            None,
            HeaderMap::new(),
            None,
            binder,
        )
        .await;
    }

    let bindings = match &binder {
        Some(binder) => match super::apply::bind_images(&state, binder, &mut config).await {
            Ok(bindings) => bindings,
            Err(response) => return response,
        },
        None => Vec::new(),
    };
    let (event_tx, event_rx) = mpsc::channel::<ApplyEvent>(32 + bindings.len());
    for binding in &bindings {
        let _ = event_tx
            .send(ApplyEvent::Progress {
                message: binding.to_string(),
            })
            .await;
    }
    if state
        .cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: event_tx,
        })
        .await
        .is_err()
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "agent unavailable"})),
        )
            .into_response();
    }
    let stream = ReceiverStream::new(event_rx).map(|e| {
        Ok::<_, std::convert::Infallible>(
            Event::default().data(serde_json::to_string(&e).unwrap_or_default()),
        )
    });
    Sse::new(stream).into_response()
}
