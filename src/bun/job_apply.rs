//! Durable apply intent: hooks and ordinary jobs share indexed runs.
use super::{
    agent::{AgentCommand, ApplyEvent},
    api::ApiState,
    task_array_leader::{WRITE_TIMEOUT, read_control, write_task_array},
};
use crate::{
    config::Config,
    council::types::{CouncilResponse, RaftRequest},
    meat::{
        job::JobDefinition,
        job_deploy::DeploymentRecord,
        task_array_state::TaskArrayStatus,
        task_array_store::{TaskArrayWrite, TaskArrays},
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
        || !config.policy.is_empty()
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
    if let Some(council) = super::leadership::council_to_forward_to(&state).await {
        let images = crate::pickle::binding::distinct_images(&config);
        return super::batch::forward_job_apply_to_leader(&state, council, body, headers, images)
            .await;
    }
    let _gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    let mut owner_notes = Vec::new();
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
        // The resource owner rules hold here as on any apply (#1125, #1429):
        // an app a controller owns is skipped unless the manifest declares
        // its owner, and only that owner's token (or an admin) may.
        // Decided before policy, so a skipped app is neither judged nor
        // written.
        owner_notes = match super::api::apply::skip_owned_apps(
            council,
            &mut config,
            &super::api::apply::owner_caller(auth),
        )
        .await
        {
            Ok(notes) => notes,
            Err(error) => {
                super::api::apply::record_owner_refusal(&state, auth, &error).await;
                return super::api::apply::owner_refusal(&error);
            }
        };
        // An app in a job manifest takes an ingress host under the same
        // rule as on the app path: a scoped token can't route a host that
        // an app outside its scope serves.
        if config.app.values().any(|spec| spec.ingress.is_some())
            && let Err(response) = super::api::apply::authorize_ingress_hosts(
                auth,
                &config,
                &council.desired_state().await,
            )
        {
            return response;
        }
        // The apps and jobs in a job manifest are judged by the policies in
        // force, as `relish apply` judges any app (#1117, #1119). The state
        // machine commits them later, once their jobs have run, so their
        // writes carry no policy fence: a policy committed in between
        // doesn't stop them.
        if !config.app.is_empty() || !config.job.is_empty() {
            let (policies, generation) = council
                .read_desired(|desired| (desired.policies.clone(), desired.policy_generation))
                .await;
            match crate::policy::admit_blocking(
                policies,
                generation,
                config.clone(),
                super::api::apply::policy_caller(auth),
                crate::policy::Source::Apply,
            )
            .await
            {
                // The apps commit with their policy defaults filled in
                // (#1118). Job manifests never come from GitOps, so there's
                // no declared spec to keep beside them.
                Ok(admitted) => {
                    super::api::apply::record_policy_audits(&state, auth, &admitted.audits).await;
                    for (name, spec) in config.app.iter_mut() {
                        let app_id = crate::meat::AppId::new(
                            name,
                            spec.namespace.as_deref().unwrap_or("default"),
                        );
                        if let Some(app) = admitted.defaulted.get(&app_id) {
                            *spec = app.effective.clone();
                        }
                    }
                }
                Err(error) => {
                    return super::api::apply::policy_refusal(&state, auth, &error).await;
                }
            }
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
            super::job_api::PolicyCheck::Judged,
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
        for message in owner_notes {
            let _ = tx.send(ApplyEvent::Progress { message }).await;
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(900);
        loop {
            // Borrow the control state for this one deployment: the stream
            // polls ten times a second, and copying the whole store each time
            // cost every other reader and writer (#2445).
            let id = operation_id.clone();
            let progress = read_control(&state, move |view| deployment_progress(view.arrays, &id))
                .await
                .unwrap_or(DeploymentProgress::Gone);
            match progress {
                DeploymentProgress::Gone => {
                    let _ = tx
                        .send(ApplyEvent::Error {
                            status: None,
                            message:
                                "deployment receipt is no longer retained; query run summaries"
                                    .into(),
                        })
                        .await;
                    return;
                }
                DeploymentProgress::Complete { created, instances } => {
                    let _ = tx.send(ApplyEvent::Complete { created, instances }).await;
                    return;
                }
                DeploymentProgress::Cancelled => {
                    let _ = tx.send(ApplyEvent::Error { status: None, message: format!("deployment {operation_id} cancelled or a hook failed; apps remain gated") }).await;
                    return;
                }
                DeploymentProgress::UnknownOwners => {
                    let _ = tx.send(ApplyEvent::Error { status: None, message: format!("deployment {operation_id} retains unknown hook ownership; inspect run summaries and acknowledge replay only if repeating side effects is acceptable") }).await;
                    return;
                }
                DeploymentProgress::Pending => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = tx.send(ApplyEvent::Error { status: None, message: format!("deployment {operation_id} is still durable and pending; query /v1/deploys/operations") }).await;
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
    .keep_alive(axum::response::sse::KeepAlive::default())
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
    // settle. Copy only the unsettled records, never the whole store (#2445).
    let pending = read_control(state, pending_deployments)
        .await
        .unwrap_or_default();
    for PendingDeployment {
        id,
        record,
        hook_failed,
        runs_terminal,
        hooks_succeeded,
    } in pending
    {
        let id = id.as_str();
        if let Some(council) = &state.council
            // One round: the deployment hooks run again next tick (#1393).
            && !council.is_leader().await
        {
            return;
        }
        let now = crate::meat::batch_ids::epoch_now_secs();
        if hook_failed && !record.cancelled() {
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
        if record.apps_committed || record.cancelled() {
            if runs_terminal {
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
        if !hooks_succeeded {
            continue;
        }
        if let Some(council) = &state.council {
            let result = tokio::time::timeout(
                state.task_arrays.write_timeout(),
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
            let owned = id.to_string();
            let settled = read_control(state, move |view| {
                view.arrays
                    .deployment(&owned)
                    .is_none_or(|current| current.cancelled() || current.apps_committed)
            })
            .await
            .unwrap_or(true);
            if settled {
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

/// What the apply stream reports for one deployment.
#[derive(Debug, PartialEq)]
enum DeploymentProgress {
    /// The receipt has been trimmed (or was never there).
    Gone,
    /// Apps are published; these runs were admitted with them.
    Complete {
        created: usize,
        instances: Vec<String>,
    },
    /// Cancelled, or a hook failed.
    Cancelled,
    /// A hook's outcome is unknown and needs an acknowledged replay.
    UnknownOwners,
    /// Still waiting on hooks.
    Pending,
}

/// Read one deployment's progress from borrowed control state, copying only
/// what the stream sends.
fn deployment_progress(arrays: &TaskArrays, operation_id: &str) -> DeploymentProgress {
    let Some((_, record)) = arrays.deployments().find(|(id, _)| *id == operation_id) else {
        return DeploymentProgress::Gone;
    };
    if record.apps_committed && !record.cancelled() {
        let instances: Vec<_> = record
            .job_runs
            .values()
            .map(|id| format!("run-{id}"))
            .collect();
        return DeploymentProgress::Complete {
            created: record.config.app.len() + instances.len(),
            instances,
        };
    }
    if record.cancelled() {
        return DeploymentProgress::Cancelled;
    }
    if record.runs().any(|id| {
        arrays
            .jobs()
            .run(id)
            .is_some_and(|run| !run.unknown_owners.is_empty())
    }) {
        return DeploymentProgress::UnknownOwners;
    }
    DeploymentProgress::Pending
}

/// An unsettled deployment and what `settle` needs to know about its runs.
struct PendingDeployment {
    id: String,
    record: DeploymentRecord,
    hook_failed: bool,
    runs_terminal: bool,
    hooks_succeeded: bool,
}

/// Up to 16 unsettled deployments, each with its run statuses folded in,
/// so `settle` copies these records and not the whole store.
fn pending_deployments(view: super::task_array_leader::ControlView<'_>) -> Vec<PendingDeployment> {
    let arrays = view.arrays;
    let status = |run: u64| arrays.get(run).map(|run| run.state.status());
    arrays
        .deployments()
        .filter(|(_, record)| !record.completed)
        .take(16)
        .map(|(id, record)| PendingDeployment {
            id: id.to_string(),
            record: record.clone(),
            hook_failed: record.hook_runs.values().any(|run| {
                status(*run)
                    .is_some_and(|run| run.is_terminal() && run != TaskArrayStatus::Succeeded)
            }),
            runs_terminal: record
                .runs()
                .all(|run| status(run).is_some_and(|run| run.is_terminal())),
            hooks_succeeded: record
                .hook_runs
                .values()
                .all(|run| status(*run) == Some(TaskArrayStatus::Succeeded)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(arrays: &mut TaskArrays, i: u64) -> String {
        let operation_id = format!("{i:032x}");
        arrays
            .apply(
                &TaskArrayWrite::DeployBegin {
                    operation_id: operation_id.clone(),
                    config: Box::new(
                        Config::parse(&format!("[app.web-{i}]\nimage='web:v1'")).unwrap(),
                    ),
                    now_epoch_secs: 100,
                },
                || 0,
            )
            .unwrap();
        operation_id
    }

    #[test]
    fn apply_watcher_reads_only_its_own_deployment() {
        let mut arrays = TaskArrays::default();
        let committed = begin(&mut arrays, 1);
        let waiting = begin(&mut arrays, 2);
        arrays
            .apply(
                &TaskArrayWrite::DeployCommitted {
                    operation_id: committed.clone(),
                    now_epoch_secs: 101,
                },
                || 0,
            )
            .unwrap();
        assert_eq!(
            deployment_progress(&arrays, &committed),
            DeploymentProgress::Complete {
                created: 1,
                instances: Vec::new()
            }
        );
        assert_eq!(
            deployment_progress(&arrays, &waiting),
            DeploymentProgress::Pending
        );
        assert_eq!(
            deployment_progress(&arrays, &format!("{:032x}", 3)),
            DeploymentProgress::Gone
        );
        arrays
            .apply(
                &TaskArrayWrite::DeployCancel {
                    operation_id: waiting.clone(),
                    now_epoch_secs: 102,
                },
                || 0,
            )
            .unwrap();
        assert_eq!(
            deployment_progress(&arrays, &waiting),
            DeploymentProgress::Cancelled
        );
    }
}
