//! Finite job groups enter the common indexed executor as named count-one runs.
//!
//! `POST /v1/batch` registers the whole group in one task-array write, and
//! `GET /v1/batch/{id}` summarises an array or a group from the same state.
//! The pre-0.2.0 engine (a leader tracker, node-to-node `/v1/batch/run`
//! dispatch and completion callbacks) is gone: every job takes the one
//! lifecycle that task arrays use.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::config::Config;
use crate::config::job::JobSpec;
use crate::meat::batch_ids::epoch_now_secs;

use super::api::ApiState;

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
    /// What the `warn` policies said about the jobs (#1424).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_warnings: Vec<String>,
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
// Handlers
// ---------------------------------------------------------------------------

/// `POST /v1/batch` — leader-forwarded submission.
pub async fn batch_submit_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    binder: Option<axum::Extension<crate::pickle::binding::ImageBinder>>,
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
    let namespaces: std::collections::BTreeSet<&str> =
        jobs.iter().map(|job| job.namespace()).collect();
    crate::bun::audit::annotate(crate::bun::audit::AuditNote {
        app: None,
        namespace: (namespaces.len() == 1)
            .then(|| namespaces.first().map(|namespace| namespace.to_string()))
            .flatten(),
        details: std::collections::BTreeMap::from([
            ("jobs".to_string(), jobs.len().to_string()),
            (
                "namespaces".to_string(),
                namespaces.iter().copied().collect::<Vec<_>>().join(","),
            ),
        ]),
    });
    let permissions = super::api::permission_map(&state).await;
    for job in &jobs {
        if let Err(response) = crate::sesame::auth::authorize_workload(
            auth.as_deref(),
            &job.name,
            job.namespace(),
            job.spec.is_host(),
            &permissions,
        ) {
            return response;
        }
    }
    // Preserve the caller on the second hop so the leader repeats admission
    // against its authoritative grants, without granting system authority.
    if let Some(council) = super::leadership::council_to_forward_to(&state).await {
        return forward_to_leader(&state, council, "/v1/batch", body, &headers).await;
    }
    let _gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    let mut policy_warnings = Vec::new();
    for job in &mut jobs {
        let namespace = job.namespace().to_owned();
        let mut definition = crate::meat::job::JobDefinition::from_spec(job.spec.clone());
        match super::job_api::admit_definition(
            &state,
            auth.as_deref(),
            binder.as_deref(),
            &job.name,
            &namespace,
            &mut definition,
            None,
            super::job_api::PolicyCheck::Judge,
        )
        .await
        {
            Ok(warnings) => policy_warnings.extend(warnings),
            Err(response) => return response,
        }
        // Admission binds the image and settles the namespace. Copy only
        // those back: the run policy stays on the spec for `RegisterJobs`.
        job.spec.image = definition.template.image;
        job.spec.namespace = definition.template.namespace;
    }
    let request_id = match headers.get("idempotency-key") {
        Some(key) => match key.to_str() {
            Ok(key) => key.to_owned(),
            Err(_) => return (StatusCode::BAD_REQUEST, "invalid Idempotency-Key").into_response(),
        },
        None => format!("{:032x}", rand::random::<u128>()),
    };
    let write = crate::meat::task_array_store::TaskArrayWrite::RegisterJobs {
        request_id,
        jobs: jobs.into_iter().map(|job| (job.name, job.spec)).collect(),
        submitted_at_epoch_secs: epoch_now_secs(),
    };
    match super::task_array_leader::write_task_array(&state, write).await {
        Ok(Some(batch_id)) => {
            let arrays = super::task_array_leader::read_task_arrays(&state).await;
            let Some(manifest) = arrays.manifest(batch_id) else {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "committed batch receipt is unavailable",
                )
                    .into_response();
            };
            let executions = manifest
                .cohorts
                .iter()
                .map(|(name, id)| (name.clone(), format!("run-{id}")))
                .collect();
            (
                StatusCode::ACCEPTED,
                Json(BatchSubmitResponse {
                    executions,
                    batch_id,
                    assigned: manifest.cohorts.len(),
                    unschedulable: Vec::new(),
                    policy_warnings,
                }),
            )
                .into_response()
        }
        Ok(None) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "batch registration produced no identity",
        )
            .into_response(),
        Err(error) => super::job_api::write_error(error),
    }
}

/// `GET /v1/batch/{id}`: an array's or a job group's summary
/// (leader-forwarded).
pub async fn batch_status_handler(
    State(state): State<ApiState>,
    AxumPath(batch_id): AxumPath<u64>,
    headers: axum::http::HeaderMap,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
) -> Response {
    if let Some(council) = super::leadership::council_to_forward_to(&state).await {
        return forward_get_to_leader(&state, council, &format!("/v1/batch/{batch_id}"), &headers)
            .await;
    }

    let arrays = super::task_array_leader::read_task_arrays(&state).await;
    if let Some(record) = arrays.get(batch_id) {
        if let Err(response) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), &record.name, &record.namespace)
        {
            return response;
        }
        let nodes = state.task_arrays.node_views(batch_id).await;
        let progress = record.state.summary();
        let mut summary = super::task_array_api::array_summary(batch_id, record, &nodes);
        super::task_array_api::enrich_summary(&mut summary, &arrays, batch_id);
        summary["state_bytes"] = serde_json::json!(super::task_array_api::state_bytes([record]));
        summary["rates"] = serde_json::to_value(
            state
                .task_arrays
                .rates(batch_id, (progress.succeeded, progress.failed))
                .await,
        )
        .unwrap_or_default();
        return Json(summary).into_response();
    }
    if let Some(manifest) = arrays.manifest(batch_id) {
        if let Err(response) =
            super::task_array_api::authorize_manifest(auth.as_deref(), manifest, &arrays)
        {
            return response;
        }
        let mut summary = super::task_array_api::manifest_summary(
            batch_id,
            manifest,
            &arrays,
            &state.task_arrays,
        )
        .await;
        let cohorts: Option<Vec<_>> = manifest
            .cohorts
            .iter()
            .map(|(_, id)| arrays.get(*id))
            .collect();
        summary["state_bytes"] =
            serde_json::json!(cohorts.and_then(super::task_array_api::state_bytes));
        return Json(summary).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": format!("batch {batch_id} not found") })),
    )
        .into_response()
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
    forward_post_to_leader(state, council, path, body, headers, 0).await
}

/// Forward a job apply naming `images` distinct images to the leader and
/// stream its events back (see [`job_apply_header_deadline`]).
pub(crate) async fn forward_job_apply_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    body: String,
    headers: &axum::http::HeaderMap,
    images: usize,
) -> Response {
    forward_post_to_leader(state, council, "/v1/apply", body, headers, images).await
}

async fn forward_post_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    path: &str,
    body: String,
    headers: &axum::http::HeaderMap,
    images: usize,
) -> Response {
    let leader_url = match super::api::leader_api_url(state, council, headers).await {
        Ok(url) => url,
        Err(refusal) => return refusal.into_response(),
    };
    let request = super::api::mark_leader_forwarded(
        state
            .cluster_http
            .client()
            .post(format!("{leader_url}{path}"))
            .header("content-type", "application/json")
            .body(body),
    );
    let mut request = super::api::copy_forwarded_auth(request, headers);
    for key in [
        "idempotency-key",
        "x-reliaburger-rerun-jobs",
        "x-reliaburger-test-lease",
    ] {
        if let Some(value) = headers.get(key) {
            request = request.header(key, value.as_bytes());
        }
    }
    // A job apply answers with an event stream that runs until the
    // deployment settles: only the wait for its headers is bounded, and the
    // leader binds every image before it sends them.
    if path == "/v1/apply" {
        return relay_leader_stream(request, job_apply_header_deadline(images)).await;
    }
    relay_leader_answer(request, LEADER_FORWARD_TIMEOUT).await
}

/// The longest a follower waits for the headers of a job apply it
/// forwarded, when the manifest names `images` distinct images.
///
/// Before the leader starts its event stream it binds each image against
/// its upstream registry, one after another and up to
/// [`crate::pickle::binding::UPSTREAM_BIND_TIMEOUT`] each, proves it leads
/// and writes the deployment to the council. A fixed deadline would answer
/// 504 for a manifest with a few slow images while the leader goes on to
/// commit it. Never less than [`LEADER_FORWARD_TIMEOUT`], and never more
/// than [`JOB_APPLY_HEADER_CAP`].
pub(crate) fn job_apply_header_deadline(images: usize) -> std::time::Duration {
    let images = u32::try_from(images).unwrap_or(u32::MAX);
    crate::pickle::binding::UPSTREAM_BIND_TIMEOUT
        .saturating_mul(images)
        .saturating_add(crate::council::node::LEADER_CONFIRMATION)
        .saturating_add(crate::council::node::WRITE_TIMEOUT.saturating_mul(2))
        .clamp(LEADER_FORWARD_TIMEOUT, JOB_APPLY_HEADER_CAP)
}

/// The cap on [`job_apply_header_deadline`]: under the five minutes
/// `relish` waits for an answer, so the caller hears the 504 and its
/// "outcome unknown" rather than a client timeout.
pub(crate) const JOB_APPLY_HEADER_CAP: std::time::Duration = std::time::Duration::from_secs(280);

pub(crate) async fn forward_get_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    path: &str,
    headers: &axum::http::HeaderMap,
) -> Response {
    let leader_url = match super::api::leader_api_url(state, council, headers).await {
        Ok(url) => url,
        Err(refusal) => return refusal.into_response(),
    };
    let request = super::api::copy_forwarded_auth(
        super::api::mark_leader_forwarded(
            state
                .cluster_http
                .client()
                .get(format!("{leader_url}{path}")),
        ),
        headers,
    );
    relay_leader_answer(request, LEADER_FORWARD_TIMEOUT).await
}

/// How long a follower waits for the leader to answer a request it
/// forwarded. Past it the follower answers 504 and the outcome is unknown:
/// the leader may still finish the request.
///
/// The leader's own work is bounded (a council write gives up at
/// [`crate::council::node::WRITE_TIMEOUT`], an upstream image bind at
/// [`crate::pickle::binding::UPSTREAM_BIND_TIMEOUT`]), so a leader that
/// is merely slow answers well inside this. One that never answers, behind
/// a blackholed path or a stalled process, used to hold the caller for as
/// long as the TCP connection lived.
pub(crate) const LEADER_FORWARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Send `request`, already addressed to the leader, and relay its status,
/// content type and body. The whole exchange, body included, must finish
/// within `deadline`: 502 when the leader can't be reached or its answer
/// breaks off, 504 when it doesn't answer in time.
pub(crate) async fn relay_leader_answer(
    request: reqwest::RequestBuilder,
    deadline: std::time::Duration,
) -> Response {
    let exchange = async {
        let response = request.send().await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .cloned();
        let body = response.bytes().await?;
        Ok::<_, reqwest::Error>((status, content_type, body))
    };
    match tokio::time::timeout(deadline, exchange).await {
        Ok(Ok((status, content_type, body))) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = (status, body).into_response();
            if let Some(content_type) = content_type
                && let Ok(value) = axum::http::HeaderValue::from_bytes(content_type.as_bytes())
            {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, value);
            }
            response
        }
        Ok(Err(error)) => leader_unreachable(&error),
        Err(_) => leader_timed_out(deadline),
    }
}

/// Send `request`, already addressed to the leader, and stream its answer
/// back. Only the wait for the leader's status line and headers is bounded
/// by `deadline`; the body then streams for as long as the leader writes
/// it, since an event stream legitimately runs for minutes.
pub(crate) async fn relay_leader_stream(
    request: reqwest::RequestBuilder,
    deadline: std::time::Duration,
) -> Response {
    let response = match tokio::time::timeout(deadline, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return leader_unreachable(&error),
        Err(_) => return leader_timed_out(deadline),
    };
    let mut builder = Response::builder().status(response.status().as_u16());
    if let Some(content_type) = response.headers().get(reqwest::header::CONTENT_TYPE) {
        builder = builder.header(axum::http::header::CONTENT_TYPE, content_type.as_bytes());
    }
    builder
        .body(axum::body::Body::from_stream(response.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn leader_unreachable(error: &reqwest::Error) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({ "error": format!("leader forward failed: {error}") })),
    )
        .into_response()
}

fn leader_timed_out(deadline: std::time::Duration) -> Response {
    (
        StatusCode::GATEWAY_TIMEOUT,
        Json(serde_json::json!({
            "error": format!(
                "the leader didn't answer within {}s; the outcome is unknown, so check \
                 before repeating a request that isn't idempotent",
                deadline.as_secs()
            ),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_apply_header_deadline_grows_with_its_images_up_to_the_cap() {
        assert_eq!(job_apply_header_deadline(0), LEADER_FORWARD_TIMEOUT);
        assert_eq!(job_apply_header_deadline(1), LEADER_FORWARD_TIMEOUT);
        let three = job_apply_header_deadline(3);
        assert!(
            three
                >= 3 * crate::pickle::binding::UPSTREAM_BIND_TIMEOUT
                    + 2 * crate::council::node::WRITE_TIMEOUT
        );
        assert!(three < JOB_APPLY_HEADER_CAP);
        assert_eq!(job_apply_header_deadline(50), JOB_APPLY_HEADER_CAP);
        assert_eq!(job_apply_header_deadline(usize::MAX), JOB_APPLY_HEADER_CAP);
    }

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
}
