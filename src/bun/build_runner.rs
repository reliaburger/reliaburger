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
///
/// `deny_unknown_fields` (JOB2): the registry destination is now
/// server-owned — a caller that smuggles a `registry_port` (or any
/// other unexpected field) to point a privileged Bun at an arbitrary
/// localhost service gets a 400, not a silently-ignored field.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSubmitRequest {
    pub name: String,
    pub context_digest: String,
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

/// An owned build directory that deletes itself on drop (JOB6).
///
/// Rust has no destructors you call by hand; instead the `Drop` trait
/// runs code the moment a value goes out of scope, on *every* exit path
/// — normal return, early `return`, `?`, or a panic unwinding the
/// stack. Wrapping the temp dir in one of these means a build that
/// fails, times out, or panics still leaves nothing behind. This is the
/// RAII pattern (Resource Acquisition Is Initialisation) that C++ calls
/// the same name; Rust makes it the default way to manage resources.
struct ScopedDir {
    path: std::path::PathBuf,
}

impl ScopedDir {
    /// Create a unique per-build directory under `base`. The random
    /// suffix means two concurrent builds of the *same* context digest
    /// never share a directory (the old digest-derived path did).
    fn new(base: &std::path::Path, build_id: u64) -> std::io::Result<Self> {
        use rand::Rng as _;
        let suffix: u64 = rand::thread_rng().r#gen();
        let path = base.join(format!("{build_id}-{suffix:016x}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Why a capped context download stopped short.
enum DownloadError {
    /// The body exceeded the configured byte cap.
    TooLarge,
    /// A transport or disk error.
    Io(String),
}

/// Stream a context download to `path`, aborting once more than
/// `max_bytes` have arrived (JOB6). Counting bytes as they stream keeps
/// peak memory bounded — the whole body is never buffered.
async fn download_to_file_capped(
    response: reqwest::Response,
    path: &std::path::Path,
    max_bytes: u64,
) -> Result<(), DownloadError> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DownloadError::Io(e.to_string()))?;
        written += chunk.len() as u64;
        if written > max_bytes {
            return Err(DownloadError::TooLarge);
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;
    }
    file.flush()
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    Ok(())
}

/// Why a bounded subprocess run failed to produce output.
enum BoundedError {
    /// The process outlived its timeout and was killed.
    Timeout,
    /// The process could not be spawned or waited on.
    Spawn(std::io::Error),
}

/// SIGKILL an entire process group by its leader pid (JOB6). Buildah
/// spawns children; killing only the parent orphans them. We put the
/// build in its own process group and signal the whole group.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    // A negative pid signals the process group whose id is `pid`.
    let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
}

/// Run a subprocess in `dir`, bounded by `timeout`. On Unix the child
/// leads its own process group and `kill_on_drop` is a backstop, so a
/// timeout kills Buildah *and its children*, not just the parent (JOB6).
async fn run_bounded(
    program: &str,
    args: &[String],
    dir: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<std::process::Output, BoundedError> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let child = command.spawn().map_err(BoundedError::Spawn)?;
    #[cfg(unix)]
    let pid = child.id();

    // Take stdout/stderr so we can collect them while waiting.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(BoundedError::Spawn(e)),
        Err(_) => {
            // Dropping the wait future here also drops `child`, so
            // `kill_on_drop` SIGKILLs the direct child; the group kill
            // reaches its grandchildren too.
            #[cfg(unix)]
            if let Some(pid) = pid {
                kill_process_group(pid);
            }
            Err(BoundedError::Timeout)
        }
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
#[allow(clippy::too_many_arguments)]
pub async fn run_build(
    request: BuildSubmitRequest,
    build_id: u64,
    registry: Arc<Mutex<BuildRegistry>>,
    cmd_tx: tokio::sync::mpsc::Sender<AgentCommand>,
    pickle_catalog: Option<Arc<tokio::sync::RwLock<crate::pickle::types::ManifestCatalog>>>,
    timeout: std::time::Duration,
    registry_port: u16,
    max_context_bytes: u64,
) {
    let fail = |reason: String| async {
        eprintln!("bun: build {build_id} ({}) failed: {reason}", request.name);
        registry
            .lock()
            .await
            .set(build_id, BuildState::Failed { reason });
    };

    // The registry destination is the node's own config, never the
    // request (JOB2): a caller cannot point this privileged process at
    // an arbitrary localhost service.
    let job = match crate::pickle::build::execute_build(
        &request.spec,
        &request.context_digest,
        Some(registry_port),
    ) {
        Ok(job) => job,
        Err(e) => return fail(format!("invalid build spec: {e}")).await,
    };

    // A unique, self-cleaning build directory. `ScopedDir::drop` removes
    // it on every exit path below, so no failure leaves a stray context.
    let base = std::env::temp_dir().join("reliaburger-build");
    let build_dir = match ScopedDir::new(&base, build_id) {
        Ok(dir) => dir,
        Err(e) => return fail(format!("failed to create build directory: {e}")).await,
    };
    // The tar lands beside the context, not inside it, so `context.tar`
    // never becomes part of the build.
    let tar_path = build_dir.path().join("context.tar");
    let ctx_dir = build_dir.path().join("ctx");

    // Stream the context blob to disk with a hard byte cap (no whole-body
    // buffer), then extract through the hardened unpacker (bounds
    // size/entries, rejects traversal/symlinks, strips setuid) off the
    // async runtime.
    let context_url =
        crate::pickle::build::context_download_url(registry_port, &request.context_digest);
    let response = match reqwest::get(&context_url).await {
        Ok(response) if response.status().is_success() => response,
        _ => {
            return fail(format!(
                "context blob {} not found in the registry",
                request.context_digest
            ))
            .await;
        }
    };
    match download_to_file_capped(response, &tar_path, max_context_bytes).await {
        Ok(()) => {}
        Err(DownloadError::TooLarge) => {
            return fail(format!(
                "build context exceeds the {max_context_bytes} byte limit"
            ))
            .await;
        }
        Err(DownloadError::Io(e)) => return fail(format!("context read failed: {e}")).await,
    }

    let extract_path = ctx_dir.clone();
    let dockerfile = request.spec.dockerfile.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&tar_path)?;
        crate::pickle::build::unpack_context(
            std::io::BufReader::new(file),
            &extract_path,
            max_context_bytes,
        )?;
        // Prove the Dockerfile resolves inside the extracted context
        // before Buildah ever reads it (JOB6).
        crate::pickle::build::confine_dockerfile(&extract_path, &dockerfile)?;
        Ok::<_, crate::pickle::build::BuildError>(())
    })
    .await;
    match extracted {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return fail(format!("failed to prepare build context: {e}")).await,
        Err(e) => return fail(format!("context extraction task failed: {e}")).await,
    }

    // Build, then push. The push targets the local registry, so the
    // image lands in Pickle through the standard handlers (real
    // holders, catalog persistence, Raft propose — all for free).
    for (label, cmd) in [("build", &job.build_cmd), ("push", &job.push_cmd)] {
        let (program, args) = match cmd.split_first() {
            Some(pair) => pair,
            None => continue,
        };
        match run_bounded(program, args, &ctx_dir, timeout).await {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let tail: String = stderr.lines().rev().take(10).collect::<Vec<_>>().join("\n");
                return fail(format!("buildah {label} failed: {tail}")).await;
            }
            Err(BoundedError::Timeout) => {
                return fail(format!(
                    "buildah {label} exceeded build_timeout_secs ({}s)",
                    timeout.as_secs()
                ))
                .await;
            }
            Err(BoundedError::Spawn(e)) => {
                return fail(format!("failed to run buildah {label}: {e}")).await;
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

/// Parse a build request body, returning a 400 response on malformed
/// JSON or any unexpected field. `deny_unknown_fields` on the struct
/// means a smuggled `registry_port` (JOB2) is a 400, not a silent
/// no-op.
#[allow(clippy::result_large_err)]
fn parse_build_request(body: &str) -> Result<BuildSubmitRequest, Response> {
    serde_json::from_str(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid build request: {e}") })),
        )
            .into_response()
    })
}

/// `POST /v1/build`: run locally when buildah is present, else
/// delegate to a peer that reported the capability, else 503.
pub async fn build_submit_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    body: String,
) -> Response {
    // Submitting a build is a Deployer action (AUTH2).
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    let request = match parse_build_request(&body) {
        Ok(request) => request,
        Err(resp) => return resp,
    };
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
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    body: String,
) -> Response {
    let request = match parse_build_request(&body) {
        Ok(request) => request,
        Err(resp) => return resp,
    };
    // Reject a malformed context digest first (JOB2) — an unconditional shape
    // check with no side effects. This endpoint is reached by a delegating
    // peer, and the digest is later used to build a temp path; `Digest::new`
    // allows only `sha256:` + 64 hex, which cannot contain `.`/`/`.
    if let Err(e) = crate::pickle::types::Digest::new(&request.context_digest) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid context digest: {e}") })),
        )
            .into_response();
    }
    // Node-to-node delegation only (JOB1): a build runs an arbitrary Buildah
    // context as a privileged process, so only a cluster node (system
    // principal) may drive it.
    if let Err(resp) = crate::sesame::auth::require_system(auth.as_deref()) {
        return resp;
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

    // Validate the spec synchronously so a bad destination or an
    // escaping Dockerfile path (JOB6) is a 400 here, not a build that
    // 202s and then fails asynchronously.
    if let Err(e) = crate::pickle::build::validate_build(&request.spec) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid build spec: {e}") })),
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
        state.registry_port,
        state.max_context_bytes,
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
    fn build_request_rejects_unknown_fields() {
        // A smuggled registry destination (JOB2) fails to deserialise.
        let body = r#"{
            "name": "app",
            "context_digest": "sha256:abc",
            "registry_port": 5050,
            "spec": { "context": ".", "destination": "pickle://app:v1" }
        }"#;
        assert!(serde_json::from_str::<BuildSubmitRequest>(body).is_err());
    }

    #[test]
    fn scoped_dir_removes_itself_on_drop() {
        let base = std::env::temp_dir().join("reliaburger-build-test");
        let path = {
            let dir = ScopedDir::new(&base, 42).unwrap();
            let path = dir.path().to_path_buf();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists(), "ScopedDir should delete itself on drop");
    }

    #[test]
    fn scoped_dir_gives_concurrent_builds_distinct_paths() {
        let base = std::env::temp_dir().join("reliaburger-build-test");
        let a = ScopedDir::new(&base, 7).unwrap();
        let b = ScopedDir::new(&base, 7).unwrap();
        assert_ne!(a.path(), b.path(), "same build id must not share a dir");
    }

    /// JOB6: a Buildah run that outlives its timeout is killed along with
    /// its children. A shell shim backgrounds a long `sleep` (a
    /// grandchild), records its pid, and waits; after `run_bounded`
    /// times out, that grandchild must be gone — proving the whole
    /// process group was signalled, not just the direct child.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_bounded_kills_the_whole_process_group_on_timeout() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let args = vec!["-c".to_string(), script];

        let result = run_bounded("sh", &args, dir.path(), Duration::from_millis(300)).await;
        assert!(matches!(result, Err(BoundedError::Timeout)));

        // Read the grandchild pid the shim recorded.
        let mut child_pid = None;
        for _ in 0..50 {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                child_pid = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid = child_pid.expect("shim never recorded its child pid");

        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        // Poll until the grandchild is gone (reaped after the group kill).
        // `kill` with `None` sends no signal, only an existence check.
        let mut gone = false;
        for _ in 0..100 {
            if kill(Pid::from_raw(pid), None).is_err() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !gone {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
        }
        assert!(gone, "grandchild sleep survived the process-group kill");
    }

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
