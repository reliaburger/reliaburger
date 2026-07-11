//! Asynchronous image builds (Phase 12, F2).
//!
//! The Stage 4 build handler worked but ran synchronously — a
//! minutes-long `buildah` build held the HTTP request open, and the
//! CLI's 300-second client timeout made anything real strand mid-
//! response. This module lifts that handler body into a spawned
//! runner behind a small tracker: `POST /v1/build` answers `202` with
//! a build id immediately, `GET /v1/build/{id}` reports progress, and
//! nodes without `buildah` delegate to a peer that reported the
//! capability in its state reports.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::sync::{Mutex, oneshot};

use super::agent::AgentCommand;
use super::api::ApiState;

/// Request body for `/v1/build` and `/v1/build/run`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuildSubmitRequest {
    pub name: String,
    pub context_digest: String,
    pub registry_port: u16,
    pub spec: crate::config::build::BuildSpec,
}

/// Lifecycle of one tracked build.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum BuildState {
    /// The runner is fetching context / building / pushing.
    Running,
    /// Built, pushed, and (cluster mode) signed.
    Completed { image: String },
    /// Any stage failed; the reason is the last useful stderr.
    Failed { reason: String },
    /// Delegated to a peer builder; status reads proxy through.
    Delegated { url: String, remote_id: u64 },
}

/// Leader-less, node-local build registry. Unlike batches there is no
/// cross-node summary to keep — each build lives where it was
/// submitted, and delegated builds proxy their status reads.
#[derive(Debug, Default)]
pub struct BuildRegistry {
    next_id: u64,
    builds: HashMap<u64, BuildState>,
}

impl BuildRegistry {
    pub fn register(&mut self, state: BuildState) -> u64 {
        self.next_id += 1;
        self.builds.insert(self.next_id, state);
        self.next_id
    }

    pub fn set(&mut self, id: u64, state: BuildState) {
        self.builds.insert(id, state);
    }

    pub fn get(&self, id: u64) -> Option<BuildState> {
        self.builds.get(&id).cloned()
    }
}

/// Whether this node can build (probed per call — cheap, and the
/// submit path is rare).
pub async fn local_buildah_available() -> bool {
    tokio::process::Command::new("buildah")
        .arg("--version")
        .output()
        .await
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Pick a peer builder: any member whose latest state report says it
/// has buildah. Returns its API base URL.
pub fn find_peer_builder(
    members: &[super::api::NodeMembershipInfo],
    aggregated: &crate::reporting::aggregator::AggregatedState,
    self_name: Option<&str>,
    cluster_http: &crate::cluster::ClusterHttp,
) -> Option<String> {
    members
        .iter()
        .filter(|m| Some(m.node_id.0.as_str()) != self_name)
        .find(|m| {
            aggregated
                .reports
                .get(&m.node_id)
                .is_some_and(|report| report.has_buildah)
        })
        .map(|m| cluster_http.url(&m.address.to_string(), ""))
}

/// The lifted build body: fetch the context blob from the local
/// registry, extract, run `buildah bud` + `buildah push` (bounded by
/// `[images] build_timeout_secs`), then sign the pushed manifest when
/// a cluster is available. Every failure lands in the registry with a
/// reason; nothing is reported over HTTP from here.
pub async fn run_build(
    request: BuildSubmitRequest,
    build_id: u64,
    registry: Arc<Mutex<BuildRegistry>>,
    cmd_tx: tokio::sync::mpsc::Sender<AgentCommand>,
    pickle_catalog: Option<Arc<tokio::sync::RwLock<crate::pickle::types::ManifestCatalog>>>,
    timeout: std::time::Duration,
) {
    let fail = |reason: String| async {
        eprintln!("bun: build {build_id} ({}) failed: {reason}", request.name);
        registry
            .lock()
            .await
            .set(build_id, BuildState::Failed { reason });
    };

    let job = match crate::pickle::build::execute_build(
        &request.spec,
        &request.context_digest,
        Some(request.registry_port),
    ) {
        Ok(job) => job,
        Err(e) => return fail(format!("invalid build spec: {e}")).await,
    };

    // Fetch the context blob from the local registry.
    let context_url =
        crate::pickle::build::context_download_url(request.registry_port, &request.context_digest);
    let context = match reqwest::get(&context_url).await {
        Ok(response) if response.status().is_success() => match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => return fail(format!("context read failed: {e}")).await,
        },
        _ => {
            return fail(format!(
                "context blob {} not found in the registry",
                request.context_digest
            ))
            .await;
        }
    };

    // Extract the tarred context (blocking IO off the runtime).
    let build_dir = std::env::temp_dir()
        .join("reliaburger-build")
        .join(request.context_digest.replace(':', "-"));
    let extract_dir = build_dir.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&extract_dir)?;
        tar::Archive::new(&context[..]).unpack(&extract_dir)?;
        Ok::<_, std::io::Error>(())
    })
    .await;
    if !matches!(extracted, Ok(Ok(()))) {
        return fail("failed to extract build context".to_string()).await;
    }

    // Build, then push. The push targets the local registry, so the
    // image lands in Pickle through the standard handlers (real
    // holders, catalog persistence, Raft propose — all for free).
    for (label, cmd) in [("build", &job.build_cmd), ("push", &job.push_cmd)] {
        let (program, args) = match cmd.split_first() {
            Some(pair) => pair,
            None => continue,
        };
        let output = tokio::time::timeout(
            timeout,
            tokio::process::Command::new(program)
                .args(args)
                .current_dir(&build_dir)
                .output(),
        )
        .await;
        match output {
            Ok(Ok(out)) if out.status.success() => {}
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let tail: String = stderr.lines().rev().take(10).collect::<Vec<_>>().join("\n");
                return fail(format!("buildah {label} failed: {tail}")).await;
            }
            Ok(Err(e)) => return fail(format!("failed to run buildah {label}: {e}")).await,
            Err(_) => {
                return fail(format!(
                    "buildah {label} exceeded build_timeout_secs ({}s)",
                    timeout.as_secs()
                ))
                .await;
            }
        }
    }

    // Sign the pushed manifest so it deploys under require_signatures.
    // Best-effort: standalone nodes have no council to sign with, and
    // an unsigned build on a trust-free cluster is still useful.
    if let Some(catalog) = &pickle_catalog {
        let digest = catalog
            .read()
            .await
            .get_manifest_by_tag(&job.destination.name, &job.destination.tag)
            .map(|manifest| manifest.digest.as_str().to_string());
        if let Some(digest) = digest {
            let (sign_tx, sign_rx) = oneshot::channel();
            let _ = cmd_tx
                .send(AgentCommand::SignImage {
                    manifest_digest: digest,
                    response: sign_tx,
                })
                .await;
            match sign_rx.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => eprintln!(
                    "bun: build {build_id}: image pushed but signing failed \
                     (deploys need require_signatures = false): {e}"
                ),
                Err(_) => {}
            }
        }
    }

    registry.lock().await.set(
        build_id,
        BuildState::Completed {
            image: format!("{}:{}", job.destination.name, job.destination.tag),
        },
    );
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /v1/build`: run locally when buildah is present, else
/// delegate to a peer that reported the capability, else 503.
pub async fn build_submit_handler(
    State(state): State<ApiState>,
    Json(request): Json<BuildSubmitRequest>,
) -> Response {
    if local_buildah_available().await {
        return accept_build(&state, request).await;
    }

    // Delegate to a capable peer.
    let peer = match (&state.membership, &state.aggregated_rx) {
        (Some(membership), Some(aggregated_rx)) => {
            let members = membership.read().await.clone();
            find_peer_builder(
                &members,
                &aggregated_rx.borrow(),
                state.node_name.as_deref(),
                &state.cluster_http,
            )
        }
        _ => None,
    };
    let Some(peer_url) = peer else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "no builder available: buildah is not on this node's PATH \
                          and no peer reports the capability"
            })),
        )
            .into_response();
    };

    let mut proxied = state
        .cluster_http
        .client()
        .post(format!("{peer_url}/v1/build/run"))
        .json(&request);
    if let Some(token) = &state.service_token {
        proxied = proxied.bearer_auth(token);
    }
    let response = match proxied.send().await {
        Ok(response) => response,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": format!("builder dispatch failed: {e}") })),
            )
                .into_response();
        }
    };
    let remote: serde_json::Value = match response.json().await {
        Ok(value) => value,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": format!("builder response: {e}") })),
            )
                .into_response();
        }
    };
    let Some(remote_id) = remote["build_id"].as_u64() else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": format!("builder rejected the build: {remote}") })),
        )
            .into_response();
    };

    // Track locally as delegated, so the client polls one node.
    let build_id = state
        .build_registry
        .lock()
        .await
        .register(BuildState::Delegated {
            url: peer_url,
            remote_id,
        });
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "build_id": build_id })),
    )
        .into_response()
}

/// `POST /v1/build/run`: the builder-node endpoint — always local.
pub async fn build_run_handler(
    State(state): State<ApiState>,
    Json(request): Json<BuildSubmitRequest>,
) -> Response {
    // Reject a malformed context digest before anything else (JOB2). This
    // endpoint is reached by a delegating peer, so the digest is untrusted
    // and is later used to build a temp path; `Digest::new` allows only
    // `sha256:` + 64 hex, which cannot contain `.`/`/`.
    if let Err(e) = crate::pickle::types::Digest::new(&request.context_digest) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid context digest: {e}") })),
        )
            .into_response();
    }
    if !local_buildah_available().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "buildah is not on this node's PATH"
            })),
        )
            .into_response();
    }
    accept_build(&state, request).await
}

/// Register + spawn a local build; answer 202 with the id.
async fn accept_build(state: &ApiState, request: BuildSubmitRequest) -> Response {
    // Validate the context digest before it is ever used to build a path
    // (JOB2): `context_digest` reaches `run_build` from a peer over
    // `/v1/build/run` and is interpolated into a temp directory. A digest
    // like `sha256:../../x` would escape the build sandbox and the tar
    // unpack would run as a privileged process. `Digest::new` enforces
    // `sha256:` + 64 hex, so `.`/`/` can never appear.
    if let Err(e) = crate::pickle::types::Digest::new(&request.context_digest) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid context digest: {e}") })),
        )
            .into_response();
    }

    let build_id = state
        .build_registry
        .lock()
        .await
        .register(BuildState::Running);
    tokio::spawn(run_build(
        request,
        build_id,
        Arc::clone(&state.build_registry),
        state.cmd_tx.clone(),
        state.pickle_catalog.clone(),
        std::time::Duration::from_secs(state.build_timeout_secs),
    ));
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "build_id": build_id })),
    )
        .into_response()
}

/// `GET /v1/build/{id}`: local state, or a proxy read for delegated
/// builds.
pub async fn build_status_handler(
    State(state): State<ApiState>,
    AxumPath(build_id): AxumPath<u64>,
) -> Response {
    let entry = state.build_registry.lock().await.get(build_id);
    match entry {
        Some(BuildState::Delegated { url, remote_id }) => {
            let mut request = state
                .cluster_http
                .client()
                .get(format!("{url}/v1/build/{remote_id}"));
            if let Some(token) = &state.service_token {
                request = request.bearer_auth(token);
            }
            match request.send().await {
                Ok(response) => {
                    let status = StatusCode::from_u16(response.status().as_u16())
                        .unwrap_or(StatusCode::BAD_GATEWAY);
                    let body = response.bytes().await.unwrap_or_default();
                    (status, body).into_response()
                }
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({ "error": format!("builder unreachable: {e}") })),
                )
                    .into_response(),
            }
        }
        Some(build_state) => Json(serde_json::json!(build_state)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("build {build_id} not found") })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::aggregator::AggregatedState;
    use crate::reporting::types::{ResourceUsage, StateReport};

    #[test]
    fn registry_tracks_lifecycle() {
        let mut registry = BuildRegistry::default();
        let id = registry.register(BuildState::Running);
        assert!(matches!(registry.get(id), Some(BuildState::Running)));

        registry.set(
            id,
            BuildState::Completed {
                image: "myapp:v1".to_string(),
            },
        );
        assert!(matches!(
            registry.get(id),
            Some(BuildState::Completed { .. })
        ));
        assert!(registry.get(id + 1).is_none());
    }

    #[test]
    fn build_state_serialises_with_status_tag() {
        let json = serde_json::to_value(BuildState::Completed {
            image: "myapp:v1".to_string(),
        })
        .unwrap();
        assert_eq!(json["status"], "completed");
        assert_eq!(json["image"], "myapp:v1");
    }

    fn report(node: &crate::meat::NodeId, has_buildah: bool) -> StateReport {
        StateReport {
            node_id: node.clone(),
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: ResourceUsage::default(),
            event_log: vec![],
            has_buildah,
        }
    }

    #[test]
    fn peer_builder_selection_prefers_capable_non_self() {
        let builder = crate::meat::NodeId("builder".to_string());
        let plain = crate::meat::NodeId("plain".to_string());
        let mut aggregated = AggregatedState::default();
        aggregated
            .reports
            .insert(builder.clone(), report(&builder, true));
        aggregated
            .reports
            .insert(plain.clone(), report(&plain, false));

        let members = vec![
            super::super::api::NodeMembershipInfo {
                node_id: plain,
                address: std::net::SocketAddr::from(([10, 0, 0, 1], 9117)),
            },
            super::super::api::NodeMembershipInfo {
                node_id: builder.clone(),
                address: std::net::SocketAddr::from(([10, 0, 0, 2], 9117)),
            },
        ];

        let http = crate::cluster::ClusterHttp::plaintext();
        let url = find_peer_builder(&members, &aggregated, Some("plain"), &http);
        assert_eq!(url.as_deref(), Some("http://10.0.0.2:9117"));

        // The capable node itself is excluded — "peer" means peer.
        let url = find_peer_builder(&members, &aggregated, Some("builder"), &http);
        assert!(url.is_none());
    }
}
