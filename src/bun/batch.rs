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
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_to_leader(&state, council, "/v1/batch", body, &headers).await;
    }
    let _gate = if state.council.is_none() {
        Some(state.task_arrays.apply_gate.clone().lock_owned().await)
    } else {
        None
    };
    for job in &mut jobs {
        let namespace = job.namespace().to_owned();
        let mut definition = crate::meat::job::JobDefinition::from_spec(job.spec.clone());
        if let Err(response) = super::job_api::admit_definition(
            &state,
            auth.as_deref(),
            binder.as_deref(),
            &job.name,
            &namespace,
            &mut definition,
            None,
        )
        .await
        {
            return response;
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
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
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
    if path == "/v1/apply" {
        return match request.send().await {
            Ok(response) => {
                let mut builder = Response::builder().status(response.status());
                if let Some(content_type) = response.headers().get(axum::http::header::CONTENT_TYPE)
                {
                    builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
                }
                builder
                    .body(axum::body::Body::from_stream(response.bytes_stream()))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
            }
            Err(error) => (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
        };
    }
    proxy_response(request.send().await).await
}

pub(crate) async fn forward_get_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    path: &str,
    headers: &axum::http::HeaderMap,
) -> Response {
    let Some(leader_url) = super::api::leader_api_url(state, council).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no cluster leader known yet; retry shortly" })),
        )
            .into_response();
    };
    let request = super::api::copy_forwarded_auth(
        state
            .cluster_http
            .client()
            .get(format!("{leader_url}{path}")),
        headers,
    );
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

#[cfg(test)]
mod tests {
    use super::*;

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
