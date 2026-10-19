//! Durable apply intent: hooks and ordinary jobs share indexed runs.
use super::{
    agent::{AgentCommand, ApplyEvent},
    api::ApiState,
    task_array_leader::{WRITE_TIMEOUT, read_task_arrays, write_task_array},
};
use crate::{
    config::Config,
    council::types::{CouncilResponse, RaftRequest},
    meat::{
        job::JobDefinition, task_array_state::TaskArrayStatus, task_array_store::TaskArrayWrite,
    },
};
use axum::{
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse, sse::Event},
};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// An acknowledged rerun admits fresh runs of the manifest's jobs, so it
/// must not replay unrelated declarative resources or recurring schedules.
pub(crate) fn validate_rerun(config: &Config) -> Result<(), &'static str> {
    if config.job.is_empty()
        || !config.app.is_empty()
        || !config.namespace.is_empty()
        || !config.permission.is_empty()
        || !config.build.is_empty()
        || config.job.values().any(|spec| spec.schedule.is_some())
    {
        return Err("explicit rerun requires a manifest containing only non-scheduled jobs");
    }
    Ok(())
}

/// Normal apply admission fixes encrypted work before publishing runnable intent.
///
/// `lease` is the caller's job test lease, already checked against the
/// manifest by the apply handler. Its jobs join the lease before their
/// deployment is admitted, so the lease's cleanup always finds them.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply(
    state: ApiState,
    auth: Option<&crate::sesame::auth::AuthContext>,
    binder: Option<&crate::pickle::binding::ImageBinder>,
    mut config: Config,
    headers: &HeaderMap,
    body: String,
    rerun: bool,
    lease: Option<&crate::testkit::lease::TestLease>,
) -> Response {
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return super::batch::forward_to_leader(&state, council, "/v1/apply", body, headers).await;
    }
    let _gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    if let Some(council) = &state.council {
        let namespaces: Vec<_> = council
            .desired_state()
            .await
            .namespaces
            .keys()
            .cloned()
            .collect();
        if let Err(error) = config.validate_against(&namespaces) {
            return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
        }
    }
    if state.council.is_none() && !state.task_arrays.admission_configured() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable standalone job storage is not configured",
        )
            .into_response();
    }
    if let Some(binder) = binder
        && let Err(response) = super::api::apply::bind_images(&state, binder, &mut config).await
    {
        return response;
    }
    for (name, spec) in &mut config.job {
        let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
        let mut definition = JobDefinition::from_spec(spec.clone());
        if let Err(response) = super::job_api::admit_definition(
            &state,
            auth,
            binder,
            name,
            &namespace,
            &mut definition,
            lease,
        )
        .await
        {
            return response;
        }
        spec.namespace = Some(namespace);
        spec.image = definition.template.image;
    }
    let operation_id = match headers.get("idempotency-key") {
        Some(key) => match key.to_str() {
            Ok(key) if key.len() == 32 && key.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
                key.to_owned()
            }
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    "apply Idempotency-Key must contain 32 hexadecimal characters",
                )
                    .into_response();
            }
        },
        None if rerun => format!("{:032x}", rand::random::<u128>()),
        None => {
            use sha2::{Digest, Sha256};
            let bytes = match serde_json::to_vec(&config) {
                Ok(bytes) => bytes,
                Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
            };
            hex::encode(Sha256::digest(bytes))[..32].to_string()
        }
    };
    let _lease_operation = match lease {
        Some(lease) => match join_lease(&state, lease, &config).await {
            Ok(operation) => operation,
            Err(response) => return response,
        },
        None => None,
    };
    match write_task_array(
        &state,
        TaskArrayWrite::DeployBegin {
            operation_id: operation_id.clone(),
            config: Box::new(config),
            now_epoch_secs: crate::meat::batch_ids::epoch_now_secs(),
        },
    )
    .await
    {
        Ok(_) => {}
        Err(error) => return super::job_api::write_error(error),
    }
    let (tx, rx) = mpsc::channel(16);
    let _ = tx.try_send(ApplyEvent::Accepted {
        operation_id: operation_id.clone(),
    });
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(900);
        loop {
            let arrays = read_task_arrays(&state).await;
            let Some((_, record)) = arrays.deployments().find(|(id, _)| *id == operation_id) else {
                let _ = tx
                    .send(ApplyEvent::Error {
                        message: "deployment receipt is no longer retained; query run summaries"
                            .into(),
                    })
                    .await;
                return;
            };
            if record.apps_committed && !record.cancelled {
                let instances: Vec<_> = record
                    .job_runs
                    .values()
                    .map(|id| format!("run-{id}"))
                    .collect();
                let _ = tx
                    .send(ApplyEvent::Complete {
                        created: record.config.app.len() + instances.len(),
                        instances,
                    })
                    .await;
                return;
            }
            if record.cancelled {
                let _ = tx.send(ApplyEvent::Error { message: format!("deployment {operation_id} cancelled or a hook failed; apps remain gated") }).await;
                return;
            }
            if record.runs().any(|id| {
                arrays
                    .jobs()
                    .run(id)
                    .is_some_and(|run| !run.unknown_owners.is_empty())
            }) {
                let _ = tx.send(ApplyEvent::Error { message: format!("deployment {operation_id} retains unknown hook ownership; inspect run summaries and acknowledge replay only if repeating side effects is acceptable") }).await;
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = tx.send(ApplyEvent::Error { message: format!("deployment {operation_id} is still durable and pending; query /v1/deploys/operations") }).await;
                return;
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
    Sse::new(ReceiverStream::new(rx).map(|event| {
        Ok::<_, std::convert::Infallible>(
            Event::default().data(serde_json::to_string(&event).unwrap_or_default()),
        )
    }))
    .into_response()
}

/// Make every job in `config` a resource of `lease`. With a council that is a
/// Raft write the leader's state machine checks; standalone, the node's own
/// lease store records it and holds off expiry cleanup until the returned
/// operation drops.
#[allow(clippy::result_large_err)]
async fn join_lease(
    state: &ApiState,
    lease: &crate::testkit::lease::TestLease,
    config: &Config,
) -> Result<Option<crate::testkit::lease::LocalLeaseOperation>, Response> {
    let jobs: Vec<_> = config
        .job
        .iter()
        .map(|(name, spec)| {
            crate::meat::AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
        })
        .collect();
    let now = crate::testkit::lease::now_unix_millis();
    let Some(council) = &state.council else {
        return state
            .local_test_leases
            .begin_job_operation(&lease.lease_id, &lease.owner_id, jobs, now)
            .await
            .map(Some)
            .map_err(|error| (StatusCode::CONFLICT, error.to_string()).into_response());
    };
    let written = tokio::time::timeout(
        WRITE_TIMEOUT,
        council.write(RaftRequest::TestLeaseJobs {
            lease_id: lease.lease_id.clone(),
            observed_at_unix_ms: now,
            jobs,
        }),
    )
    .await;
    match written {
        Ok(Ok(CouncilResponse::Refused { reason })) => {
            Err((StatusCode::CONFLICT, reason).into_response())
        }
        Ok(Ok(_)) => Ok(None),
        Ok(Err(error)) => Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()),
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the lease ownership write timed out",
        )
            .into_response()),
    }
}

/// A replacement leader completes durable intent using accepted results, never launch receipts.
pub(crate) async fn settle(state: &ApiState) {
    // The leader calls this every tick, and most ticks have nothing to
    // settle: look before copying every array.
    let pending = super::task_array_leader::read_control(state, |view| {
        view.arrays
            .deployments()
            .any(|(_, record)| !record.completed)
    })
    .await
    .unwrap_or(false);
    if !pending {
        return;
    }
    let arrays = read_task_arrays(state).await;
    for (id, record) in arrays
        .deployments()
        .filter(|(_, record)| !record.completed)
        .take(16)
    {
        if let Some(council) = &state.council
            && !council.is_leader().await
        {
            return;
        }
        let now = crate::meat::batch_ids::epoch_now_secs();
        let failed = record.hook_runs.values().any(|run| {
            arrays.get(*run).is_some_and(|run| {
                run.state.status().is_terminal() && run.state.status() != TaskArrayStatus::Succeeded
            })
        });
        if failed && !record.cancelled {
            let _ = write_task_array(
                state,
                TaskArrayWrite::DeployCancel {
                    operation_id: id.into(),
                    now_epoch_secs: now,
                },
            )
            .await;
            continue;
        }
        if record.apps_committed || record.cancelled {
            if record.runs().all(|run| {
                arrays
                    .get(run)
                    .is_some_and(|run| run.state.status().is_terminal())
            }) {
                let _ = write_task_array(
                    state,
                    TaskArrayWrite::DeployRelease {
                        operation_id: id.into(),
                    },
                )
                .await;
            }
            continue;
        }
        if !record.hook_runs.values().all(|run| {
            arrays
                .get(*run)
                .is_some_and(|run| run.state.status() == TaskArrayStatus::Succeeded)
        }) {
            continue;
        }
        if let Some(council) = &state.council {
            let result = tokio::time::timeout(
                WRITE_TIMEOUT,
                council.write(RaftRequest::JobApplyCommit {
                    operation_id: id.into(),
                    now_epoch_secs: now,
                }),
            )
            .await;
            if let Ok(Ok(CouncilResponse::Refused { reason })) = result {
                eprintln!("bun: deployment {id}: publication refused: {reason}");
            }
        } else {
            // Admission, cancellation and local app mutation share this gate. It is
            // held by an independent worker until the app actor positively settles.
            let Ok(gate) = state.task_arrays.apply_gate.clone().try_lock_owned() else {
                continue;
            };
            let latest = read_task_arrays(state).await;
            if latest
                .deployment(id)
                .is_none_or(|current| current.cancelled || current.apps_committed)
            {
                continue;
            }
            if record.config.app.is_empty()
                && record.config.namespace.is_empty()
                && record.config.permission.is_empty()
                && record.config.build.is_empty()
            {
                let _ = write_task_array(
                    state,
                    TaskArrayWrite::DeployCommitted {
                        operation_id: id.into(),
                        now_epoch_secs: now,
                    },
                )
                .await;
                continue;
            }
            if !state.task_arrays.publishing.lock().await.insert(id.into()) {
                continue;
            }
            let state = state.clone();
            let id = id.to_string();
            let mut config = *record.config.clone();
            config.job.clear();
            tokio::spawn(async move {
                let _gate = gate;
                let (events, mut stream) = mpsc::channel(64);
                if state
                    .cmd_tx
                    .send(AgentCommand::Deploy { config, events })
                    .await
                    .is_err()
                {
                    state.task_arrays.publishing.lock().await.remove(&id);
                    return;
                }
                while let Some(event) = stream.recv().await {
                    match event {
                        ApplyEvent::Complete { .. } => {
                            let _ = write_task_array(
                                &state,
                                TaskArrayWrite::DeployCommitted {
                                    operation_id: id.clone(),
                                    now_epoch_secs: crate::meat::batch_ids::epoch_now_secs(),
                                },
                            )
                            .await;
                            break;
                        }
                        ApplyEvent::Error { .. } => {
                            let _ = write_task_array(
                                &state,
                                TaskArrayWrite::DeployCancel {
                                    operation_id: id.clone(),
                                    now_epoch_secs: crate::meat::batch_ids::epoch_now_secs(),
                                },
                            )
                            .await;
                            break;
                        }
                        _ => {}
                    }
                }
                state.task_arrays.publishing.lock().await.remove(&id);
            });
        }
    }
}
