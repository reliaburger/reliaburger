//! OCI Distribution API handlers for Pickle.
//!
//! Implements the subset of the OCI Distribution Spec needed for
//! `docker push` and `docker pull`: blob uploads, manifest push/pull,
//! and tag listing.

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use serde::Deserialize;
use tokio::sync::RwLock;

use super::registry_auth::{QuotaConfig, UploadSessions, WriteDenied};
use super::store::{BlobStore, compute_sha256};
use super::types::{Digest, ImageManifest, LayerDescriptor, ManifestCatalog, ManifestCommit};
use crate::sesame::auth::AuthState;

/// Maximum bytes buffered for a single registry request (REG4).
///
/// Layers are pushed monolithically or chunked; each PATCH/PUT body is
/// bounded so one request can't buffer an unbounded blob in memory. Large
/// layers arrive as multiple bounded chunks.
///
/// Note (M11): this bounds *one* request, not aggregate memory — the handlers
/// take the body as an in-memory `Bytes`, so N concurrent monolithic pushes
/// can still buffer up to N × this. The peer-pull path already streams to disk
/// (see `pull::pull_layer_from_peer`); streaming the push handlers the same way
/// — consuming the request body as a chunked stream straight into the upload
/// temp — is TODO(Phase 15). Until then, keep this ceiling modest and rely on
/// chunked pushes (buildah/docker default) to keep individual bodies small.
const MAX_REQUEST_BYTES: usize = 512 * 1024 * 1024;

/// Shared state for Pickle API handlers.
#[derive(Clone)]
pub struct PickleState {
    pub store: Arc<BlobStore>,
    pub catalog: Arc<RwLock<ManifestCatalog>>,
    /// This node's raft id, recorded as the holder on pushes. Derived
    /// from the node name even in single-node mode — never a made-up
    /// constant.
    pub node_raft_id: u64,
    /// Council handle for proposing catalog changes to Raft (cluster
    /// council members only; `None` single-node).
    pub council: Option<Arc<crate::council::CouncilNode>>,
    /// Where to persist the catalog after each mutation, so image
    /// metadata survives restarts. `None` disables persistence (tests).
    pub persist_path: Option<std::path::PathBuf>,
    /// Authentication for registry *writes* (REG4). Reuses the cluster's
    /// token store + service token — the same material the agent API uses.
    /// `None` disables the auth gate (single-node/tests): writes are open.
    pub auth: Option<AuthState>,
    /// Whether registry *reads* also need a principal (O1).
    ///
    /// False on the loopback default, where an open read is the point: a
    /// local `docker pull` and the node's own fetches shouldn't need a token.
    /// True once the registry is published on a routable address, where an
    /// open read hands every image in the cluster — including `cache/` copies
    /// of credentialed private upstreams — to anyone who can reach the port.
    pub require_read_auth: bool,
    /// Whether an empty token store may accept writes before the first user
    /// token exists. This is only safe for a loopback-only standalone registry.
    pub allow_unauthenticated_bootstrap: bool,
    /// Storage quotas (REG4). Default is unlimited.
    pub quota: QuotaConfig,
    /// Chunked upload-session tracking for TTL expiry (REG8).
    pub sessions: UploadSessions,
}

impl PickleState {
    /// Extract the Bearer token from an axum request's headers, if any.
    fn bearer(headers: &HeaderMap) -> Option<String> {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::to_string)
    }

    /// Authorise a registry write (REG4). `Ok(())` means the request may
    /// proceed; `Err(response)` is the 401/403 to return. No auth
    /// configured means the gate is off (writes open).
    // `Response` is large but it IS the HTTP reply to send on failure —
    // boxing it would tax every call site for a value that lives one frame.
    #[allow(clippy::result_large_err)]
    async fn authorise_write(&self, headers: &HeaderMap) -> Result<(), Response> {
        let Some(auth) = &self.auth else {
            return Ok(());
        };
        let bearer = Self::bearer(headers);
        match super::registry_auth::authorise_write(
            auth,
            bearer.as_deref(),
            self.allow_unauthenticated_bootstrap,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(WriteDenied::Unauthenticated) => Err(oci_error(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "registry write requires authentication".to_string(),
            )),
            Err(WriteDenied::Forbidden) => Err(oci_error(
                StatusCode::FORBIDDEN,
                "DENIED",
                "insufficient permissions to push to the registry".to_string(),
            )),
        }
    }

    /// Authorise a registry read (O1). A no-op unless the registry is bound
    /// somewhere a stranger could reach it.
    // `Response` is large but it IS the HTTP reply to send on failure —
    // boxing it would tax every call site for a value that lives one frame.
    #[allow(clippy::result_large_err)]
    async fn authorise_read(&self, headers: &HeaderMap) -> Result<(), Response> {
        if !self.require_read_auth {
            return Ok(());
        }
        let Some(auth) = &self.auth else {
            return Ok(());
        };
        let bearer = Self::bearer(headers);
        match super::registry_auth::authorise_read(auth, bearer.as_deref()).await {
            Ok(()) => Ok(()),
            Err(_) => Err(oci_error(
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "registry read requires authentication on a non-loopback bind".to_string(),
            )),
        }
    }

    /// The current stored size of a repository and of the whole registry,
    /// from the authoritative catalogue (REG4 quota accounting).
    async fn stored_sizes(&self, repository: &str) -> (u64, u64) {
        let catalog = self.catalog_snapshot().await;
        let mut repo_bytes = 0u64;
        let mut total_bytes = 0u64;
        for (_, manifest) in &catalog.manifests {
            total_bytes = total_bytes.saturating_add(manifest.total_size);
            if manifest.repository == repository {
                repo_bytes = repo_bytes.saturating_add(manifest.total_size);
            }
        }
        (repo_bytes, total_bytes)
    }

    /// Enforce the storage quota for admitting `incoming` bytes into
    /// `repository` (REG4). `Ok(())` when unlimited or within limits;
    /// `Err(response)` is the 413 to return.
    // `Response` is large but it IS the HTTP reply to send on failure —
    // boxing it would tax every call site for a value that lives one frame.
    #[allow(clippy::result_large_err)]
    async fn enforce_quota(&self, repository: &str, incoming: u64) -> Result<(), Response> {
        if self.quota.is_unlimited() {
            return Ok(());
        }
        let (repo_current, total_current) = self.stored_sizes(repository).await;
        match super::registry_auth::check_quota(
            &self.quota,
            repository,
            incoming,
            repo_current,
            total_current,
        ) {
            Ok(()) => Ok(()),
            Err(e) => Err(oci_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "DENIED",
                e.to_string(),
            )),
        }
    }
}

/// Hash-and-store a blob off the async runtime (REG4).
///
/// `write_blob` re-hashes the whole blob to verify the digest — CPU-bound
/// work that must not run on a Tokio worker, or a large push stalls the
/// executor. `spawn_blocking` moves it to the blocking pool; the `move`
/// closure takes ownership of the bytes so nothing is borrowed across the
/// `.await`.
async fn store_blob_off_runtime(
    state: &PickleState,
    data: Vec<u8>,
    digest: Digest,
) -> Result<(), super::types::PickleError> {
    let store = Arc::clone(&state.store);
    tokio::task::spawn_blocking(move || store.write_blob(&data, &digest))
        .await
        .map_err(|e| super::types::PickleError::CatalogPersist(format!("hash task failed: {e}")))?
}

impl PickleState {
    /// A point-in-time catalog view: the council's Raft-replicated
    /// catalog when clustered, the local one otherwise. P2P pulls and
    /// the pull-through cache plan against this.
    pub async fn catalog_snapshot(&self) -> ManifestCatalog {
        match &self.council {
            Some(council) => council.manifest_catalog().await,
            None => self.catalog.read().await.clone(),
        }
    }
}

/// The durability a push achieved (REG7/D11).
///
/// The registry commits a push locally and to Raft, then the heal loop
/// drives it up to the configured replica count. A push therefore reports
/// what it *actually* achieved at commit time, never a durable-redundancy
/// success it hasn't reached yet:
///
/// - `Authoritative` — committed to the cluster's Raft catalogue (or
///   locally in single-node mode). Replication to the configured replica
///   count is still pending and the heal loop will complete it.
/// - `RaftUncommitted` — this node is a council member but the Raft
///   proposal failed; the bytes are stored and the local catalogue is
///   persisted, but the cluster catalogue does **not** yet know the push.
///   The push is reported as accepted-but-not-durable, not a clean success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitOutcome {
    Authoritative,
    RaftUncommitted,
}

/// Record a committed manifest: apply to the local catalog, persist it
/// to disk, and — when this node is a council member — propose it to
/// Raft so the cluster-wide catalog tracks the real holder.
///
/// Returns the [`CommitOutcome`] so the caller can reflect it honestly
/// (D11). A failed Raft proposal on a council member is surfaced, not
/// silently swallowed: the local catalogue is still persisted (so a
/// restart and the heal loop can recover), but the push did not reach the
/// authoritative catalogue.
pub(crate) async fn record_commit(
    state: &PickleState,
    manifest: ImageManifest,
    tag: String,
) -> CommitOutcome {
    let commit = ManifestCommit {
        manifest,
        tag,
        holder_nodes: std::collections::BTreeSet::from([state.node_raft_id]),
    };

    let snapshot = {
        let mut catalog = state.catalog.write().await;
        catalog.apply_manifest_commit(&commit);
        state.persist_path.as_ref().map(|_| catalog.clone())
    };
    if let (Some(path), Some(snapshot)) = (state.persist_path.clone(), snapshot) {
        // File IO off the runtime; the catalog is small (KBs).
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = snapshot.persist_to(&path) {
                eprintln!("pickle: failed to persist catalog: {e}");
            }
        })
        .await;
    }

    if let Some(council) = &state.council {
        if let Err(e) = council
            .write(crate::council::types::RaftRequest::ManifestCommit(commit))
            .await
        {
            eprintln!(
                "pickle: manifest commit not written to raft ({e}); \
                 local catalog persisted, replication will reconcile"
            );
            return CommitOutcome::RaftUncommitted;
        }
        return CommitOutcome::Authoritative;
    }
    // Single-node mode: the local catalogue *is* authoritative.
    CommitOutcome::Authoritative
}

/// Build the OCI Distribution API router.
///
/// Repository names can be multi-segment (`team/app`, `cache/<host>/<repo>`
/// — REG8). Axum can't put a wildcard *before* a fixed suffix like
/// `/blobs/…`, so instead of one route per shape we capture the whole path
/// after `/v2/` with a trailing wildcard and split off the OCI operation
/// suffix ourselves in [`dispatch_v2`]. The repository name is then
/// whatever precedes that suffix, however many segments it spans.
pub fn router(state: PickleState) -> Router {
    Router::new()
        .route("/v2/", get(v2_check))
        .route(
            "/v2/{*rest}",
            get(dispatch_v2)
                .head(dispatch_v2)
                .post(dispatch_v2)
                .patch(dispatch_v2)
                .put(dispatch_v2),
        )
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}

/// The parsed shape of an OCI `/v2/{name}/…` request.
enum V2Route {
    /// `/v2/{name}/blobs/{digest}`
    Blob { name: String, digest: String },
    /// `/v2/{name}/blobs/uploads/`
    UploadInitiate { name: String },
    /// `/v2/{name}/blobs/uploads/{upload_id}`
    UploadSession { name: String, upload_id: String },
    /// `/v2/{name}/manifests/{reference}`
    Manifest { name: String, reference: String },
    /// `/v2/{name}/tags/list`
    Tags { name: String },
}

/// Parse the path after `/v2/` (with a leading `/` stripped) into a
/// [`V2Route`], recovering the multi-segment repository name (REG8).
///
/// The repository name is everything before the recognised operation
/// suffix. `rest` never contains the leading `/v2/` — axum's `{*rest}`
/// captures only what follows.
fn parse_v2_route(rest: &str) -> Option<V2Route> {
    // `/v2/{name}/blobs/uploads/` and `…/uploads/{id}`
    if let Some((name, tail)) = rest.rsplit_once("/blobs/uploads/") {
        if tail.is_empty() {
            return Some(V2Route::UploadInitiate {
                name: name.to_string(),
            });
        }
        // A further `/` in the upload id shape isn't expected; take the tail.
        return Some(V2Route::UploadSession {
            name: name.to_string(),
            upload_id: tail.to_string(),
        });
    }
    if let Some((name, digest)) = rest.rsplit_once("/blobs/") {
        return Some(V2Route::Blob {
            name: name.to_string(),
            digest: digest.to_string(),
        });
    }
    if let Some((name, reference)) = rest.rsplit_once("/manifests/") {
        return Some(V2Route::Manifest {
            name: name.to_string(),
            reference: reference.to_string(),
        });
    }
    if let Some(name) = rest.strip_suffix("/tags/list") {
        return Some(V2Route::Tags {
            name: name.to_string(),
        });
    }
    None
}

/// Dispatch every `/v2/{name}/…` request, recovering the multi-segment
/// repository name and routing to the concrete handler (REG8).
#[allow(clippy::too_many_arguments)]
async fn dispatch_v2(
    State(state): State<PickleState>,
    method: axum::http::Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    headers: HeaderMap,
    Query(init_query): Query<InitiateUploadQuery>,
    Query(complete_query): Query<CompleteUploadQueryOpt>,
    body: axum::body::Bytes,
) -> Response {
    use axum::http::Method;

    // Recover the path after `/v2/`. `OriginalUri` preserves percent
    // encoding; we only split on literal ASCII markers, so a repository
    // name that legitimately contains those markers is unaffected.
    let path = uri.path();
    let Some(rest) = path.strip_prefix("/v2/") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(route) = parse_v2_route(rest) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // O1: writes authorise per handler (they have different role bars and
    // quota checks); reads are uniform, so one gate covers every GET/HEAD.
    if matches!(method, Method::GET | Method::HEAD)
        && let Err(response) = state.authorise_read(&headers).await
    {
        return response;
    }

    match (method, route) {
        (Method::HEAD, V2Route::Blob { name, digest }) => blob_head(&state, &name, &digest).await,
        (Method::GET, V2Route::Blob { name, digest }) => blob_get(&state, &name, &digest).await,
        (Method::POST, V2Route::UploadInitiate { name }) => {
            blob_upload_initiate(&state, &name, init_query, &headers, body).await
        }
        (Method::PATCH, V2Route::UploadSession { name, upload_id }) => {
            blob_upload_patch(&state, &name, &upload_id, &headers, body).await
        }
        (Method::PUT, V2Route::UploadSession { name, upload_id }) => {
            let Some(digest) = complete_query.digest else {
                return oci_error(
                    StatusCode::BAD_REQUEST,
                    "DIGEST_INVALID",
                    "upload completion requires a digest query parameter".to_string(),
                );
            };
            blob_upload_complete(&state, &name, &upload_id, &digest, &headers, body).await
        }
        (Method::PUT, V2Route::Manifest { name, reference }) => {
            manifest_put(&state, &name, &reference, &headers, body).await
        }
        (Method::GET, V2Route::Manifest { name, reference }) => {
            manifest_get(&state, &name, &reference).await
        }
        (Method::GET, V2Route::Tags { name }) => tags_list(&state, &name).await,
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Version check
// ---------------------------------------------------------------------------

/// `GET /v2/` — OCI version check. Returns 200 OK.
async fn v2_check(State(state): State<PickleState>, headers: HeaderMap) -> Response {
    // The OCI version probe is how a client discovers whether it needs
    // credentials, so it answers 401 like every other read when it must.
    if let Err(response) = state.authorise_read(&headers).await {
        return response;
    }
    Json(serde_json::json!({})).into_response()
}

// ---------------------------------------------------------------------------
// Blob operations
// ---------------------------------------------------------------------------

/// `HEAD /v2/{name}/blobs/{digest}` — check if a blob exists.
async fn blob_head(state: &PickleState, _name: &str, digest_str: &str) -> Response {
    let Ok(digest) = Digest::new(digest_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    if state.store.has_blob(&digest) {
        let size = state.store.blob_size(&digest).unwrap_or(0);
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-length",
            size.to_string().parse().expect("ASCII header value"),
        );
        headers.insert(
            "docker-content-digest",
            digest.as_str().parse().expect("ASCII header value"),
        );
        (StatusCode::OK, headers).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// `GET /v2/{name}/blobs/{digest}` — download a blob.
async fn blob_get(state: &PickleState, _name: &str, digest_str: &str) -> Response {
    let Ok(digest) = Digest::new(digest_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    // read_blob is a synchronous `std::fs::read` of the whole blob (up to the
    // request ceiling). Run it on the blocking pool (M7) so a large GET doesn't
    // stall an async runtime worker while the file is read.
    let store = Arc::clone(&state.store);
    let read_digest = digest.clone();
    let read = tokio::task::spawn_blocking(move || store.read_blob(&read_digest)).await;
    match read {
        Ok(Ok(data)) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                "content-length",
                data.len().to_string().parse().expect("ASCII header value"),
            );
            headers.insert(
                "docker-content-digest",
                digest.as_str().parse().expect("ASCII header value"),
            );
            headers.insert(
                "content-type",
                "application/octet-stream"
                    .parse()
                    .expect("ASCII header value"),
            );
            (StatusCode::OK, headers, data).into_response()
        }
        Ok(Err(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Query params for POST /v2/{name}/blobs/uploads/ (monolithic upload).
#[derive(Deserialize, Default)]
struct InitiateUploadQuery {
    /// If present, this is a monolithic upload — the body contains the
    /// entire blob and `digest` is the expected content digest.
    digest: Option<String>,
    /// Cross-repository mount source (not implemented, accepted and ignored).
    #[serde(default)]
    _mount: Option<String>,
    /// Cross-repository mount source repository (not implemented).
    #[serde(default)]
    _from: Option<String>,
}

/// `POST /v2/{name}/blobs/uploads/` — initiate (or complete) a blob upload.
///
/// Docker may include `?digest=sha256:...` for monolithic uploads where
/// the entire blob is in the POST body. Without the digest param, this
/// starts a chunked upload session.
async fn blob_upload_initiate(
    state: &PickleState,
    name: &str,
    query: InitiateUploadQuery,
    headers_in: &HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Registry writes require a principal once auth is configured (REG4).
    if let Err(response) = state.authorise_write(headers_in).await {
        return response;
    }

    // Monolithic upload: body + digest in one POST
    if let Some(ref digest_str) = query.digest
        && !body.is_empty()
    {
        let Ok(digest) = Digest::new(digest_str) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        // Quota: a monolithic blob is admitted whole (REG4).
        if let Err(response) = state.enforce_quota(name, body.len() as u64).await {
            return response;
        }
        match store_blob_off_runtime(state, body.to_vec(), digest.clone()).await {
            Ok(()) => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    "location",
                    format!("/v2/{name}/blobs/{digest_str}")
                        .parse()
                        .expect("ASCII header value"),
                );
                headers.insert(
                    "docker-content-digest",
                    digest_str.parse().expect("ASCII header value"),
                );
                return (StatusCode::CREATED, headers).into_response();
            }
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
    }

    // Chunked upload: start a session and register it for TTL tracking.
    match state.store.initiate_upload().await {
        Ok(upload_id) => {
            state
                .sessions
                .register(&upload_id, name, std::time::SystemTime::now())
                .await;
            let location = format!("/v2/{name}/blobs/uploads/{upload_id}");
            let mut headers = HeaderMap::new();
            headers.insert("location", location.parse().expect("ASCII header value"));
            headers.insert("range", "0-0".parse().expect("ASCII header value"));
            headers.insert(
                "docker-upload-uuid",
                upload_id.parse().expect("ASCII header value"),
            );
            (StatusCode::ACCEPTED, headers).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `PATCH /v2/{name}/blobs/uploads/{upload_id}` — upload a chunk.
async fn blob_upload_patch(
    state: &PickleState,
    name: &str,
    upload_id: &str,
    headers_in: &HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = state.authorise_write(headers_in).await {
        return response;
    }
    // An upload session that outlived its TTL is refused and swept (REG8),
    // so an abandoned push can't dribble chunks into a stale temp forever.
    let now = std::time::SystemTime::now();
    if !state
        .sessions
        .touch(upload_id, body.len() as u64, now)
        .await
    {
        state.store.cancel_upload(upload_id).await;
        return oci_error(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session unknown or expired".to_string(),
        );
    }
    match state.store.write_upload_chunk(upload_id, &body).await {
        Ok(total) => {
            let mut headers = HeaderMap::new();
            // The OCI distribution spec requires a Location on every
            // chunk response — the URL for the next chunk or final PUT.
            // containers/image 5.29+ (buildah 1.33) reads it strictly:
            // without it, `buildah push` dies with "determining upload
            // URL: http: no Location header in response" (12b.2).
            headers.insert(
                "location",
                format!("/v2/{name}/blobs/uploads/{upload_id}")
                    .parse()
                    .expect("ASCII header value"),
            );
            headers.insert(
                "docker-upload-uuid",
                upload_id.parse().expect("ASCII header value"),
            );
            headers.insert(
                "range",
                format!("0-{}", total.saturating_sub(1))
                    .parse()
                    .expect("ASCII header value"),
            );
            (StatusCode::ACCEPTED, headers).into_response()
        }
        Err(super::types::PickleError::InvalidUploadId(_)) => {
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Query params for PUT upload completion. `digest` is optional here so
/// the dispatcher can return a clean OCI error when it's missing, rather
/// than axum's opaque 400.
#[derive(Deserialize, Default)]
struct CompleteUploadQueryOpt {
    digest: Option<String>,
}

/// `PUT /v2/{name}/blobs/uploads/{upload_id}?digest=` — complete upload.
async fn blob_upload_complete(
    state: &PickleState,
    name: &str,
    upload_id: &str,
    digest_str: &str,
    headers_in: &HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = state.authorise_write(headers_in).await {
        return response;
    }
    let Ok(digest) = Digest::new(digest_str) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid digest"})),
        )
            .into_response();
    };

    // If there's a body with the PUT, write it first
    if !body.is_empty() {
        match state.store.write_upload_chunk(upload_id, &body).await {
            Ok(_) => {}
            Err(super::types::PickleError::InvalidUploadId(_)) => {
                return StatusCode::BAD_REQUEST.into_response();
            }
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        }
    }

    // Enforce the storage quota against the fully-assembled blob before it is
    // committed (M10). The monolithic POST path checks at initiate time, but a
    // chunked or bare-PUT upload only knows its total size here — the on-disk
    // temp file is authoritative. Over quota: drop the temp and the session so
    // nothing lands in the blob store.
    match state.store.upload_size(upload_id).await {
        Ok(incoming) => {
            if let Err(response) = state.enforce_quota(name, incoming).await {
                state.store.cancel_upload(upload_id).await;
                state.sessions.complete(upload_id).await;
                return response;
            }
        }
        Err(super::types::PickleError::InvalidUploadId(_)) => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    }

    let result = state.store.complete_upload(upload_id, &digest).await;
    // Whatever the outcome, the session is finished (REG8).
    state.sessions.complete(upload_id).await;
    match result {
        Ok(()) => {
            let mut headers = HeaderMap::new();
            // Location of the created blob (OCI distribution spec).
            headers.insert(
                "location",
                format!("/v2/{name}/blobs/{}", digest.as_str())
                    .parse()
                    .expect("ASCII header value"),
            );
            headers.insert(
                "docker-content-digest",
                digest.as_str().parse().expect("ASCII header value"),
            );
            (StatusCode::CREATED, headers).into_response()
        }
        Err(super::types::PickleError::DigestMismatch { expected, actual }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("digest mismatch: expected {expected}, got {actual}")
            })),
        )
            .into_response(),
        Err(super::types::PickleError::InvalidUploadId(_)) => {
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Manifest operations
// ---------------------------------------------------------------------------

/// OCI manifest JSON as received from the client.
#[derive(Debug, Deserialize)]
struct OciManifestJson {
    #[serde(rename = "schemaVersion", default)]
    _schema_version: Option<u32>,
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
    config: Option<OciDescriptor>,
    #[serde(default)]
    layers: Vec<OciDescriptor>,
    /// Present in manifest lists / OCI image indexes.
    #[serde(default)]
    manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Deserialize)]
struct OciDescriptor {
    digest: String,
    size: u64,
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
}

/// Media types accepted for single-platform manifests.
const MANIFEST_MEDIA_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// Media types accepted for image indexes / manifest lists.
const INDEX_MEDIA_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

/// An OCI Distribution error body: `{"errors": [{code, message}]}` —
/// the shape real clients (docker, podman, buildah) know how to print.
fn oci_error(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({
            "errors": [{ "code": code, "message": message }]
        })),
    )
        .into_response()
}

/// Validate one manifest descriptor against the local blob store:
/// well-formed digest, blob present (OCI push order puts blobs before
/// the manifest), and size matching what's actually on disk.
///
/// The rejection `Response` is boxed to keep the `Err` variant small
/// (clippy::result_large_err) — rejections are the cold path.
fn check_descriptor(
    store: &BlobStore,
    what: &str,
    descriptor: &OciDescriptor,
) -> Result<LayerDescriptor, Box<Response>> {
    let digest = Digest::new(&descriptor.digest).map_err(|e| {
        Box::new(oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_INVALID",
            format!("{what} digest {:?} is invalid: {e}", descriptor.digest),
        ))
    })?;
    if !store.has_blob(&digest) {
        return Err(Box::new(oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_BLOB_UNKNOWN",
            format!("{what} blob {digest} is not present in the registry"),
        )));
    }
    let actual_size = store.blob_size(&digest).unwrap_or(0);
    if actual_size != descriptor.size {
        return Err(Box::new(oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_INVALID",
            format!(
                "{what} blob {digest} size mismatch: descriptor says {}, stored blob is {actual_size}",
                descriptor.size
            ),
        )));
    }
    Ok(LayerDescriptor {
        digest,
        size: descriptor.size,
        media_type: descriptor.media_type.clone().unwrap_or_default(),
    })
}

/// `PUT /v2/{name}/manifests/{reference}` — push a manifest.
///
/// Validates before storing or committing anything (REG3): the body
/// must parse as JSON, carry a known media type (in the body or the
/// `Content-Type` header), and every referenced blob — config and
/// layers for a single-platform manifest, sub-manifests for an image
/// index — must already exist locally with a matching size. Rejections
/// use OCI Distribution error bodies, so real clients print something
/// sensible. Only then are the raw bytes stored as a content-addressed
/// blob and the catalogue commit recorded.
/// Whether `name` is in the reserved pull-through cache namespace (M3).
///
/// The cache stores upstream images under `cache/<host>/<repo>`; only the
/// internal cache-fill path may write there. A bare `cache` repo is fine — the
/// reservation is the `cache/` prefix.
fn is_reserved_cache_repo(name: &str) -> bool {
    name == "cache" || name.starts_with("cache/")
}

async fn manifest_put(
    state: &PickleState,
    name: &str,
    reference: &str,
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // Registry writes require a principal once auth is configured (REG4).
    if let Err(response) = state.authorise_write(headers).await {
        return response;
    }

    // The `cache/` namespace is reserved for the pull-through cache, filled
    // internally via record_commit (M3). A client push there would let a
    // Deployer plant an image under `cache/<host>/<repo>` that the scheduler
    // exempts from signature checks and upstream::decide treats as a cache hit
    // — poisoning every node's pull of that image and bypassing
    // require_signatures. Refuse it.
    if is_reserved_cache_repo(name) {
        return oci_error(
            StatusCode::FORBIDDEN,
            "DENIED",
            "the cache/ namespace is reserved for the pull-through cache and \
             cannot be pushed to directly"
                .to_string(),
        );
    }

    // Hash the manifest bytes off the async runtime (REG4) — small for a
    // manifest, but this keeps the whole write path off the executor.
    let manifest_digest = {
        let bytes = body.clone();
        match tokio::task::spawn_blocking(move || compute_sha256(&bytes)).await {
            Ok(digest) => digest,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "manifest hashing failed"})),
                )
                    .into_response();
            }
        }
    };

    // A digest in the reference position (docker pushes sub-manifests
    // as `PUT …/manifests/sha256:…`) must name the bytes it carries.
    if let Ok(reference_digest) = Digest::new(reference)
        && reference_digest != manifest_digest
    {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "DIGEST_INVALID",
            format!(
                "reference digest {reference_digest} does not match body digest {manifest_digest}"
            ),
        );
    }

    let manifest_json: OciManifestJson = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                format!("manifest is not valid json: {e}"),
            );
        }
    };

    // Media type from the body, falling back to Content-Type (the OCI
    // spec lets clients omit the embedded field and set the header).
    let media_type = manifest_json.media_type.clone().or_else(|| {
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    });
    let Some(media_type) = media_type else {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_INVALID",
            "manifest has no mediaType and the request has no content-type header".to_string(),
        );
    };

    let manifest = if INDEX_MEDIA_TYPES.contains(&media_type.as_str()) {
        // Image index / manifest list: every sub-manifest must already
        // be in the store (docker pushes them by digest first). The
        // catalogue entry pins the index blob (as its own config
        // descriptor) plus each sub-manifest, so GC keeps them all.
        if manifest_json.manifests.is_empty() {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "image index has no manifests".to_string(),
            );
        }
        let mut sub_manifests = Vec::new();
        for descriptor in &manifest_json.manifests {
            match check_descriptor(&state.store, "sub-manifest", descriptor) {
                Ok(layer) => sub_manifests.push(layer),
                Err(response) => return *response,
            }
        }
        let total_size = body.len() as u64 + sub_manifests.iter().map(|l| l.size).sum::<u64>();
        ImageManifest {
            digest: manifest_digest.clone(),
            config: LayerDescriptor {
                digest: manifest_digest.clone(),
                size: body.len() as u64,
                media_type: media_type.clone(),
            },
            layers: sub_manifests,
            repository: name.to_string(),
            tags: std::collections::BTreeSet::new(),
            total_size,
            pushed_at: std::time::SystemTime::now(),
            pushed_by: state.node_raft_id,
            signature: None,
        }
    } else if MANIFEST_MEDIA_TYPES.contains(&media_type.as_str()) {
        let Some(config) = &manifest_json.config else {
            return oci_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "manifest has no config descriptor".to_string(),
            );
        };
        let config = match check_descriptor(&state.store, "config", config) {
            Ok(layer) => layer,
            Err(response) => return *response,
        };
        let mut layers = Vec::new();
        for descriptor in &manifest_json.layers {
            match check_descriptor(&state.store, "layer", descriptor) {
                Ok(layer) => layers.push(layer),
                Err(response) => return *response,
            }
        }
        let total_size = config.size + layers.iter().map(|l| l.size).sum::<u64>();
        ImageManifest {
            digest: manifest_digest.clone(),
            config,
            layers,
            repository: name.to_string(),
            tags: std::collections::BTreeSet::new(),
            total_size,
            pushed_at: std::time::SystemTime::now(),
            pushed_by: state.node_raft_id,
            signature: None,
        }
    } else {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_INVALID",
            format!("unsupported manifest mediaType: {media_type}"),
        );
    };

    // Quota: charge the whole image (manifest + config + layers) against
    // the repository and registry ceilings before committing (REG4).
    if let Err(response) = state.enforce_quota(name, manifest.total_size).await {
        return response;
    }

    // Validation passed: store the exact bytes (content addressing must
    // see what the client sent, not a re-serialisation) off the runtime,
    // then commit.
    if let Err(e) = store_blob_off_runtime(state, body.to_vec(), manifest_digest.clone()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to store manifest blob: {e}")})),
        )
            .into_response();
    }
    let outcome = record_commit(state, manifest, reference.to_string()).await;

    let mut headers = HeaderMap::new();
    headers.insert(
        "docker-content-digest",
        manifest_digest
            .as_str()
            .parse()
            .expect("ASCII header value"),
    );
    // Honest push semantics (REG7/D11): the manifest is committed, but
    // replication to the configured replica count is driven by the heal
    // loop afterwards. Advertise that so a client (or an operator reading
    // headers) never mistakes acceptance for full redundancy. A Raft
    // proposal that failed on a council member is reported distinctly —
    // the push is accepted locally but is not yet authoritative.
    match outcome {
        CommitOutcome::Authoritative => {
            headers.insert(
                "oci-replication",
                "pending".parse().expect("ASCII header value"),
            );
            (StatusCode::CREATED, headers).into_response()
        }
        CommitOutcome::RaftUncommitted => {
            headers.insert(
                "oci-replication",
                "raft-uncommitted".parse().expect("ASCII header value"),
            );
            // 202 Accepted: stored and persisted, but not yet in the
            // authoritative cluster catalogue — not a clean 201.
            (StatusCode::ACCEPTED, headers).into_response()
        }
    }
}

/// `GET /v2/{name}/manifests/{reference}` — pull a manifest.
///
/// `reference` can be a tag (e.g. `latest`) or a digest (e.g. `sha256:abc...`).
/// Docker pulls sub-manifests by digest when resolving manifest lists.
async fn manifest_get(state: &PickleState, _name: &str, reference: &str) -> Response {
    // If the reference looks like a digest, try reading it directly
    // from the blob store (Docker pulls sub-manifests by digest).
    if let Ok(digest) = Digest::new(reference)
        && let Ok(data) = state.store.read_blob(&digest)
    {
        let content_type = detect_manifest_content_type(&data);
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            content_type.parse().expect("ASCII header value"),
        );
        headers.insert(
            "docker-content-digest",
            reference.parse().expect("ASCII header value"),
        );
        return (StatusCode::OK, headers, data).into_response();
    }

    // Try the catalogue by tag. Read the *authoritative* catalogue (the
    // council's Raft-replicated one when clustered, local otherwise) so a
    // manifest committed by a peer — or by a non-council push forwarded to
    // Raft — is visible here even before a heal tick reconciles the local
    // projection (REG2).
    let catalog = state.catalog_snapshot().await;
    let manifest = catalog.get_manifest_by_tag(_name, reference);

    match manifest {
        Some(m) => match state.store.read_blob(&m.digest) {
            Ok(data) => {
                let content_type = detect_manifest_content_type(&data);
                let mut headers = HeaderMap::new();
                headers.insert(
                    "content-type",
                    content_type.parse().expect("ASCII header value"),
                );
                headers.insert(
                    "docker-content-digest",
                    m.digest.as_str().parse().expect("ASCII header value"),
                );
                (StatusCode::OK, headers, data).into_response()
            }
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        },
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Detect the correct content-type for a manifest blob.
fn detect_manifest_content_type(data: &[u8]) -> &'static str {
    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(data)
        && json.get("manifests").is_some()
    {
        return "application/vnd.oci.image.index.v1+json";
    }
    "application/vnd.oci.image.manifest.v1+json"
}

// ---------------------------------------------------------------------------
// Tags
// ---------------------------------------------------------------------------

/// `GET /v2/{name}/tags/list` — list tags for a repository.
async fn tags_list(state: &PickleState, name: &str) -> Response {
    // Read the authoritative catalogue (REG2): a repository a peer pushed
    // must list its tags here too, not only where the PUT landed.
    let catalog = state.catalog_snapshot().await;
    let tags = catalog.tags_for_repository(name);
    Json(serde_json::json!({
        "name": name,
        "tags": tags,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> (PickleState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let state = PickleState {
            store: Arc::new(store),
            catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
            node_raft_id: 7,
            council: None,
            persist_path: None,
            auth: None,
            require_read_auth: false,
            allow_unauthenticated_bootstrap: true,
            quota: QuotaConfig::default(),
            sessions: UploadSessions::new(super::super::registry_auth::DEFAULT_UPLOAD_TTL),
        };
        (state, dir)
    }

    fn test_router(state: PickleState) -> Router {
        router(state)
    }

    /// A state whose registry is published on a routable address and holds
    /// one ReadOnly token, so the O1 read gate is live.
    async fn read_gated_state() -> (PickleState, tempfile::TempDir, String) {
        let (mut state, dir) = test_state();
        let created = crate::sesame::token::create_token(
            "puller",
            crate::sesame::types::ApiRole::ReadOnly,
            crate::sesame::types::TokenScope::default(),
            None,
        )
        .unwrap();
        let tokens = crate::sesame::auth::new_token_store();
        tokens.write().await.push(created.token);
        state.auth = Some(crate::sesame::auth::AuthState::new(tokens, None));
        state.require_read_auth = true;
        (state, dir, created.plaintext)
    }

    /// O1: on a routable bind, an anonymous client could enumerate and pull
    /// every image in the cluster — including `cache/` copies of private
    /// upstreams pulled with the operator's credentials.
    #[tokio::test]
    async fn anonymous_reads_are_refused_on_a_routable_bind() {
        let (state, _dir, _token) = read_gated_state().await;
        let app = test_router(state);

        for uri in ["/v2/", "/v2/team/app/tags/list"] {
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{uri} served an anonymous client on a routable bind"
            );
        }
    }

    /// Pulling is exactly what a ReadOnly token is for, so the bar for reads
    /// is any valid token — not the Deployer that writes require.
    #[tokio::test]
    async fn a_readonly_token_may_still_pull() {
        let (state, _dir, token) = read_gated_state().await;
        let app = test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v2/")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The loopback default is unchanged: a local pull needs no token, which
    /// is the whole point of binding to loopback.
    #[tokio::test]
    async fn loopback_reads_stay_open() {
        let (mut state, _dir, _token) = read_gated_state().await;
        state.require_read_auth = false;
        let app = test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v2/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn body_bytes(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    #[tokio::test]
    async fn v2_check_returns_200() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v2/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn blob_head_not_found() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("HEAD")
                    .uri(format!("/v2/myapp/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn blob_get_not_found() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v2/myapp/blobs/{digest}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Helper: push a single blob through the upload API.
    async fn push_blob(app: &Router, name: &str, data: &[u8]) -> Digest {
        let digest = compute_sha256(data);

        // Initiate upload
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/v2/{name}/blobs/uploads/"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let location = resp.headers()["location"].to_str().unwrap().to_string();

        // Upload data via PATCH. The 202 must carry a Location for the
        // next request — containers/image (buildah 1.33+) reads it
        // strictly, and its absence broke real `buildah push` (12b.2).
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri(&location)
                    .body(Body::from(data.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let location = resp.headers()["location"].to_str().unwrap().to_string();

        // Complete upload; the 201 names the created blob's location.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri(format!("{location}?digest={}", digest.as_str()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert!(resp.headers().contains_key("location"));

        digest
    }

    #[tokio::test]
    async fn full_push_pull_round_trip() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        // Push config blob
        let config_data = b"config blob data";
        let config_digest = push_blob(&app, "myapp", config_data).await;

        // Push layer blob
        let layer_data = b"layer blob data here";
        let layer_digest = push_blob(&app, "myapp", layer_data).await;

        // Push manifest
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": config_digest.as_str(),
                "size": config_data.len(),
                "mediaType": "application/vnd.oci.image.config.v1+json"
            },
            "layers": [{
                "digest": layer_digest.as_str(),
                "size": layer_data.len(),
                "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip"
            }]
        });

        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/v2/myapp/manifests/latest")
                    .body(Body::from(serde_json::to_vec(&manifest_json).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Pull manifest back by tag
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v2/myapp/manifests/latest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let manifest_body = body_bytes(resp).await;
        assert!(!manifest_body.is_empty());

        // Pull layer blob back
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v2/myapp/blobs/{}", layer_digest.as_str()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let blob_body = body_bytes(resp).await;
        assert_eq!(blob_body, layer_data);
    }

    /// Push a manifest body and return the response.
    async fn put_manifest(app: &Router, uri: &str, body: Vec<u8>) -> Response {
        app.clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Assert an OCI error body carries the expected error code.
    async fn assert_oci_error(resp: Response, code: &str) {
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_bytes(resp).await;
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["errors"][0]["code"], code, "body: {json}");
    }

    #[test]
    fn cache_namespace_is_reserved() {
        assert!(is_reserved_cache_repo("cache"));
        assert!(is_reserved_cache_repo("cache/docker.io/library/redis"));
        // Not the reserved namespace.
        assert!(!is_reserved_cache_repo("cached"));
        assert!(!is_reserved_cache_repo("myteam/cache-warmer"));
        assert!(!is_reserved_cache_repo("redis"));
    }

    /// M3: a client push to the reserved `cache/` namespace is refused, so it
    /// can't poison the pull-through cache to bypass signature checks.
    #[tokio::test]
    async fn push_to_cache_namespace_is_forbidden() {
        let (state, _dir) = test_state();
        let app = test_router(state);
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "digest": format!("sha256:{}", "b".repeat(64)), "size": 1 },
            "layers": []
        });
        let resp = put_manifest(
            &app,
            "/v2/cache/docker.io/library/redis/manifests/7",
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// REG3: a manifest referencing a blob the registry never received
    /// is rejected with `MANIFEST_BLOB_UNKNOWN` — the previous version
    /// of this test asserted Created, encoding the bug.
    #[tokio::test]
    async fn push_manifest_with_missing_layer_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        // Push config blob only (no layer)
        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;

        let missing_digest = format!("sha256:{}", "a".repeat(64));
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": config_digest.as_str(),
                "size": config_data.len()
            },
            "layers": [{
                "digest": missing_digest,
                "size": 100
            }]
        });

        let resp = put_manifest(
            &app,
            "/v2/myapp/manifests/latest",
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await;
        assert_oci_error(resp, "MANIFEST_BLOB_UNKNOWN").await;
    }

    #[tokio::test]
    async fn push_manifest_with_invalid_json_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let resp = put_manifest(
            &app,
            "/v2/myapp/manifests/latest",
            b"not json at all {{{".to_vec(),
        )
        .await;
        assert_oci_error(resp, "MANIFEST_INVALID").await;
    }

    #[tokio::test]
    async fn push_manifest_without_media_type_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "config": { "digest": config_digest.as_str(), "size": config_data.len() },
            "layers": []
        });

        // No embedded mediaType and no Content-Type header.
        let resp = put_manifest(
            &app,
            "/v2/myapp/manifests/latest",
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await;
        assert_oci_error(resp, "MANIFEST_INVALID").await;
    }

    #[tokio::test]
    async fn push_manifest_with_unknown_media_type_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.example.made-up.v1+json",
            "config": { "digest": "sha256:0", "size": 1 },
        });

        let resp = put_manifest(
            &app,
            "/v2/myapp/manifests/latest",
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await;
        assert_oci_error(resp, "MANIFEST_INVALID").await;
    }

    #[tokio::test]
    async fn push_manifest_with_content_type_header_media_type_is_accepted() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;
        // Embedded mediaType omitted; carried in the header instead.
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "config": { "digest": config_digest.as_str(), "size": config_data.len() },
            "layers": []
        });

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/v2/myapp/manifests/latest")
                    .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                    .body(Body::from(serde_json::to_vec(&manifest_json).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn push_manifest_with_size_mismatch_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": config_digest.as_str(),
                "size": config_data.len() + 5, // lies about the size
            },
            "layers": []
        });

        let resp = put_manifest(
            &app,
            "/v2/myapp/manifests/latest",
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await;
        assert_oci_error(resp, "MANIFEST_INVALID").await;
    }

    #[tokio::test]
    async fn push_manifest_with_malformed_descriptor_digest_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "digest": config_digest.as_str(), "size": config_data.len() },
            "layers": [{ "digest": "md5:definitely-not-a-digest", "size": 4 }]
        });

        let resp = put_manifest(
            &app,
            "/v2/myapp/manifests/latest",
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await;
        assert_oci_error(resp, "MANIFEST_INVALID").await;
    }

    /// REG3: a rejected manifest leaves no trace — no blob, no tag.
    #[tokio::test]
    async fn rejected_manifest_is_not_stored_or_tagged() {
        let (state, _dir) = test_state();
        let store = Arc::clone(&state.store);
        let catalog = Arc::clone(&state.catalog);
        let app = test_router(state);

        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "digest": format!("sha256:{}", "b".repeat(64)),
                "size": 4
            },
            "layers": []
        }))
        .unwrap();
        let manifest_digest = compute_sha256(&body);

        let resp = put_manifest(&app, "/v2/myapp/manifests/latest", body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!store.has_blob(&manifest_digest));
        assert!(
            catalog
                .read()
                .await
                .get_manifest_by_tag("myapp", "latest")
                .is_none()
        );
    }

    /// The manifest GET must return the exact bytes that were pushed:
    /// content addressing sees the client's bytes, not a re-serialise.
    #[tokio::test]
    async fn manifest_get_returns_byte_identical_body() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let config_data = b"config bytes";
        let config_digest = push_blob(&app, "myapp", config_data).await;
        // Deliberately quirky formatting: whitespace and key order
        // must survive the round trip.
        let body = format!(
            "{{\"schemaVersion\": 2,\n  \"layers\": [],\n  \"config\": {{\"size\": {}, \"digest\": \"{}\"}},\n  \"mediaType\": \"application/vnd.oci.image.manifest.v1+json\"}}",
            config_data.len(),
            config_digest.as_str()
        )
        .into_bytes();

        let resp = put_manifest(&app, "/v2/myapp/manifests/latest", body.clone()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v2/myapp/manifests/latest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_bytes(resp).await, body);
    }

    /// Multi-arch push order: sub-manifests by digest, then the index.
    #[tokio::test]
    async fn push_image_index_after_sub_manifests_succeeds() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;
        let sub_manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "digest": config_digest.as_str(), "size": config_data.len() },
            "layers": []
        }))
        .unwrap();
        let sub_digest = compute_sha256(&sub_manifest);

        // Sub-manifest pushed by digest reference, like docker does.
        let resp = put_manifest(
            &app,
            &format!("/v2/myapp/manifests/{}", sub_digest.as_str()),
            sub_manifest.clone(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "digest": sub_digest.as_str(),
                "size": sub_manifest.len(),
                "platform": { "architecture": "arm64", "os": "linux" }
            }]
        }))
        .unwrap();
        let resp = put_manifest(&app, "/v2/myapp/manifests/latest", index).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    /// REG3: an index whose sub-manifest was never pushed is refused.
    #[tokio::test]
    async fn push_image_index_with_missing_sub_manifest_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let index = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [{
                "digest": format!("sha256:{}", "c".repeat(64)),
                "size": 100
            }]
        }))
        .unwrap();
        let resp = put_manifest(&app, "/v2/myapp/manifests/latest", index).await;
        assert_oci_error(resp, "MANIFEST_BLOB_UNKNOWN").await;
    }

    /// A digest reference must name the bytes it carries.
    #[tokio::test]
    async fn push_manifest_by_mismatched_digest_reference_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "digest": config_digest.as_str(), "size": config_data.len() },
            "layers": []
        }))
        .unwrap();

        let wrong = format!("sha256:{}", "d".repeat(64));
        let resp = put_manifest(&app, &format!("/v2/myapp/manifests/{wrong}"), body).await;
        assert_oci_error(resp, "DIGEST_INVALID").await;
    }

    #[tokio::test]
    async fn tags_list_empty() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v2/myapp/tags/list")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_bytes(resp).await;
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["tags"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn manifest_get_not_found() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v2/myapp/manifests/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn blob_head_returns_size() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let data = b"some blob";
        let digest = push_blob(&app, "myapp", data).await;

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("HEAD")
                    .uri(format!("/v2/myapp/blobs/{}", digest.as_str()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()["content-length"].to_str().unwrap(),
            data.len().to_string()
        );
    }

    /// REG7/D11: a single-node push commits authoritatively and advertises
    /// that replication is still pending — never a durable-redundancy
    /// success it hasn't reached.
    #[tokio::test]
    async fn push_advertises_replication_pending() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;
        let manifest_json = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "digest": config_digest.as_str(), "size": config_data.len() },
            "layers": []
        });
        let resp = put_manifest(
            &app,
            "/v2/myapp/manifests/latest",
            serde_json::to_vec(&manifest_json).unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(
            resp.headers().get("oci-replication").unwrap(),
            "pending",
            "push must honestly report replication is pending"
        );
    }

    /// REG4: a push that would breach the repository quota is refused with
    /// 413, and nothing is stored.
    #[tokio::test]
    async fn push_over_repository_quota_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(BlobStore::new(dir.path()));
        let state = PickleState {
            store: Arc::clone(&store),
            catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
            node_raft_id: 7,
            council: None,
            persist_path: None,
            auth: None,
            require_read_auth: false,
            allow_unauthenticated_bootstrap: true,
            quota: QuotaConfig {
                per_repository_bytes: 4,
                total_bytes: 0,
            },
            sessions: UploadSessions::new(super::super::registry_auth::DEFAULT_UPLOAD_TTL),
        };
        let app = test_router(state);

        // A monolithic blob larger than the 4-byte repository quota.
        let blob = b"way too big for the quota".to_vec();
        let digest = compute_sha256(&blob);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/v2/web/blobs/uploads/?digest={}", digest.as_str()))
                    .body(Body::from(blob))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            !store.has_blob(&digest),
            "over-quota blob must not be stored"
        );
    }

    /// REG4/M10: a chunked upload whose assembled size breaches the quota is
    /// refused at completion, and nothing lands in the blob store. The
    /// monolithic path checks at initiate; this covers the chunked/bare-PUT
    /// path that previously skipped quota entirely.
    #[tokio::test]
    async fn chunked_upload_over_repository_quota_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(BlobStore::new(dir.path()));
        let state = PickleState {
            store: Arc::clone(&store),
            catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
            node_raft_id: 7,
            council: None,
            persist_path: None,
            auth: None,
            require_read_auth: false,
            allow_unauthenticated_bootstrap: true,
            quota: QuotaConfig {
                per_repository_bytes: 4,
                total_bytes: 0,
            },
            sessions: UploadSessions::new(super::super::registry_auth::DEFAULT_UPLOAD_TTL),
        };
        let app = test_router(state);

        // Initiate a chunked session (no digest, no body).
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v2/web/blobs/uploads/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let location = resp.headers()["location"].to_str().unwrap().to_string();

        // Push a chunk larger than the 4-byte quota.
        let blob = b"way too big for the quota".to_vec();
        let digest = compute_sha256(&blob);
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri(&location)
                    .body(Body::from(blob.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // Completing must be refused with 413 and store nothing.
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri(format!("{location}?digest={}", digest.as_str()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(
            !store.has_blob(&digest),
            "over-quota chunked blob must not be committed"
        );
    }

    /// REG8: a chunk against an expired upload session is refused, and the
    /// stale temp is swept.
    #[tokio::test]
    async fn expired_upload_session_chunk_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(BlobStore::new(dir.path()));
        let state = PickleState {
            store: Arc::clone(&store),
            catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
            node_raft_id: 7,
            council: None,
            persist_path: None,
            auth: None,
            require_read_auth: false,
            allow_unauthenticated_bootstrap: true,
            quota: QuotaConfig::default(),
            // A zero TTL: any session is immediately expired on the next
            // chunk, which is exactly what we want to assert.
            sessions: UploadSessions::new(std::time::Duration::ZERO),
        };
        let app = test_router(state.clone());

        // Initiate a chunked upload.
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v2/web/blobs/uploads/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let location = resp.headers()["location"].to_str().unwrap().to_string();

        // The very next chunk is past the zero TTL: refused, not written.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PATCH")
                    .uri(&location)
                    .body(Body::from(b"data".to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// REG4: hashing a large blob runs off the async runtime, so it doesn't
    /// stall other requests. We push a sizable monolithic blob while a
    /// cheap request runs concurrently; the cheap one must not be blocked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn large_blob_hash_does_not_block_other_requests() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        // 16 MiB monolithic blob → real hashing work.
        let big = vec![0xABu8; 16 * 1024 * 1024];
        let digest = compute_sha256(&big);
        let push_app = app.clone();
        let push = tokio::spawn(async move {
            push_app
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(format!("/v2/web/blobs/uploads/?digest={}", digest.as_str()))
                        .body(Body::from(big))
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        });

        // A cheap GET /v2/ must complete promptly even while the hash runs.
        let cheap = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.oneshot(
                axum::http::Request::builder()
                    .uri("/v2/")
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await;
        assert!(cheap.is_ok(), "cheap request blocked by hashing");
        assert_eq!(cheap.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(push.await.unwrap(), StatusCode::CREATED);
    }
}
