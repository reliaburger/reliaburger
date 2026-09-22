//! OCI Distribution API handlers for Pickle.
//!
//! Implements the subset of the OCI Distribution Spec needed for
//! `docker push` and `docker pull`: blob uploads, manifest push/pull,
//! and tag listing.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::RwLock;

use super::lease::{RegistryWriteAccess, RepositoryReadGuard};
use super::registry_auth::{QuotaConfig, UploadSessions, WriteDenied};
use super::store::{BlobStore, compute_sha256};
use super::types::{Digest, ImageManifest, LayerDescriptor, ManifestCatalog, ManifestCommit};
use crate::sesame::auth::AuthState;

/// Maximum bytes streamed by a single blob request. Larger layers use PATCH chunks.
const MAX_REQUEST_BYTES: usize = 512 * 1024 * 1024;
/// Manifests are small JSON documents, independently bounded from image layers.
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
/// Admission bounds open writers and the small buffers retained by each stream.
const MAX_CONCURRENT_WRITES: usize = 4;
const UPLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

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
    /// Present on every clustered node, including workers without a council.
    pub forwarder: Option<super::authority::RegistryForwarder>,
    /// Standalone lease authority, shared with the agent API and lease reaper.
    pub test_leases: crate::testkit::lease::LocalLeaseStore,
    /// Shared writer exclusion for lease-owned repository retirement.
    pub repository_writers: super::lease::RepositoryWriters,
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

    /// Authenticate a writer and preserve the identity used for upload ownership.
    /// `None` means anonymous standalone mode; errors carry the HTTP refusal.
    // `Response` is large but it IS the HTTP reply to send on failure —
    // boxing it would tax every call site for a value that lives one frame.
    #[allow(clippy::result_large_err)]
    async fn authorise_write(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<crate::sesame::auth::AuthContext>, Response> {
        let Some(auth) = &self.auth else {
            return Ok(None);
        };
        let bearer = Self::bearer(headers);
        match super::registry_auth::authenticate_writer(
            auth,
            bearer.as_deref(),
            self.allow_unauthenticated_bootstrap,
        )
        .await
        {
            Ok(principal) => Ok(principal),
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

    #[allow(clippy::result_large_err)]
    async fn authorise_repository(
        &self,
        name: &str,
        headers: &HeaderMap,
        principal: Option<&crate::sesame::auth::AuthContext>,
    ) -> Result<RegistryWriteAccess, Response> {
        let lease = headers
            .get("x-reliaburger-test-lease")
            .map(|value| value.to_str())
            .transpose()
            .map_err(|_| {
                registry_write_error(super::types::PickleError::LeaseDenied(
                    "invalid lease header".into(),
                ))
            })?;
        let owner = principal.map(|p| p.principal_id.as_str());
        let internal = owner == Some(crate::sesame::auth::SYSTEM_PRINCIPAL);
        self.admit_repository_write(name, lease, owner, internal)
            .await
            .map_err(registry_write_error)
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
    async fn stored_sizes(
        &self,
        repository: &str,
    ) -> Result<(u64, u64), super::types::PickleError> {
        match self
            .registry_query(super::authority::RegistryQuery::Usage {
                repository: repository.into(),
            })
            .await?
        {
            super::authority::RegistryQueryResponse::Usage {
                repository_bytes,
                total_bytes,
            } => Ok((repository_bytes, total_bytes)),
            _ => Err(super::types::PickleError::ReplicationFailed(
                "invalid registry usage response".into(),
            )),
        }
    }

    /// Enforce the storage quota for admitting `incoming` bytes into
    /// `repository` (REG4). `Ok(())` when unlimited or within limits;
    /// `Err(response)` is 413 for a full quota or 503 for unavailable authority.
    // `Response` is large but it IS the HTTP reply to send on failure —
    // boxing it would tax every call site for a value that lives one frame.
    #[allow(clippy::result_large_err)]
    async fn enforce_quota(&self, repository: &str, incoming: u64) -> Result<(), Response> {
        if self.quota.is_unlimited() {
            return Ok(());
        }
        let (repo_current, total_current) = self
            .stored_sizes(repository)
            .await
            .map_err(registry_write_error)?;
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
    writer: Option<RepositoryReadGuard>,
) -> Result<(), super::types::PickleError> {
    let store = Arc::clone(&state.store);
    tokio::task::spawn_blocking(move || {
        let _writer = writer;
        store.write_blob(&data, &digest)
    })
    .await
    .map_err(|e| super::types::PickleError::CatalogPersist(format!("hash task failed: {e}")))?
}

impl PickleState {
    /// A current repository view. Clustered workers and followers query the leader;
    /// unavailable authority is an error, never an empty registry or cache miss.
    pub async fn catalog_snapshot(
        &self,
        repository: &str,
    ) -> Result<ManifestCatalog, super::types::PickleError> {
        match self
            .registry_query(super::authority::RegistryQuery::Repository {
                repository: repository.into(),
            })
            .await?
        {
            super::authority::RegistryQueryResponse::Repository(catalog) => Ok(*catalog),
            _ => Err(super::types::PickleError::ReplicationFailed(
                "invalid registry catalogue response".into(),
            )),
        }
    }
}

/// Persist local ownership before publication. The blocking task owns the write
/// guard through persistence, even when the HTTP caller is cancelled.
pub(crate) async fn record_commit(
    state: &PickleState,
    manifest: ImageManifest,
    tag: String,
) -> Result<(), super::types::PickleError> {
    record_commit_with_access(state, manifest, tag, &RegistryWriteAccess::default()).await
}

async fn record_commit_with_access(
    state: &PickleState,
    manifest: ImageManifest,
    tag: String,
    access: &RegistryWriteAccess,
) -> Result<(), super::types::PickleError> {
    let state = state.clone();
    let access = access.clone();
    tokio::spawn(async move { record_commit_owned(&state, manifest, tag, &access).await })
        .await
        .map_err(|error| {
            super::types::PickleError::CatalogPersist(format!(
                "manifest publication task failed: {error}"
            ))
        })?
}

async fn record_commit_owned(
    state: &PickleState,
    manifest: ImageManifest,
    tag: String,
    access: &RegistryWriteAccess,
) -> Result<(), super::types::PickleError> {
    use super::types::PickleError;
    if super::lease::is_test_repository(&manifest.repository) != access.lease_id.is_some() {
        return Err(PickleError::LeaseDenied(
            "manifest requires matching repository lease admission".into(),
        ));
    }
    let mut commit = ManifestCommit {
        observed_gc_generation: 0,
        manifest,
        tag,
        holder_nodes: std::collections::BTreeSet::from([state.node_raft_id]),
    };
    let local_operation = if state.council.is_none() && state.forwarder.is_none() {
        if let Some(lease_id) = &access.lease_id {
            Some(
                state
                    .test_leases
                    .begin_registry_commit(
                        lease_id,
                        &commit,
                        crate::testkit::lease::now_unix_millis(),
                    )
                    .await
                    .map_err(|e| PickleError::LeaseDenied(e.to_string()))?,
            )
        } else {
            None
        }
    } else {
        None
    };
    let transaction_writer = access.guard.clone();
    let lease_id = access.lease_id.clone();
    let mut catalog = Arc::clone(&state.catalog).write_owned().await;
    commit.observed_gc_generation = state.registry_gc_generation().await?;
    let store = Arc::clone(&state.store);
    let persist = state.persist_path.clone();
    let local_commit = commit.clone();
    let _catalog = tokio::task::spawn_blocking(move || {
        let _writer = transaction_writer;
        let _operation = local_operation;
        if let Some(lease_id) = lease_id
            && catalog
                .repository_owners
                .get(&local_commit.manifest.repository)
                != Some(&lease_id)
        {
            return Err(PickleError::LeaseDenied(
                "local repository generation changed".into(),
            ));
        }
        // GC uses this same guard through physical deletion. A blob validated
        // before waiting for the guard may have been collected in the meantime.
        for digest in local_commit.manifest.referenced_digests() {
            if !store.has_blob(digest) {
                return Err(PickleError::MissingLayer(digest.clone()));
            }
        }
        let mut next = catalog.clone();
        next.apply_manifest_commit(&local_commit);
        if let Some(path) = persist {
            next.persist_to(&path)?;
        }
        *catalog = next;
        Ok(catalog)
    })
    .await
    .map_err(|error| PickleError::CatalogPersist(error.to_string()))??;

    let mutation = match &access.lease_id {
        Some(lease_id) => super::authority::RegistryMutation::LeasedManifest {
            lease_id: lease_id.clone(),
            commit: Box::new(commit),
        },
        None => super::authority::RegistryMutation::Manifest(Box::new(commit)),
    };
    match state.propose(mutation).await? {
        None
        | Some(
            crate::council::CouncilResponse::Ok | crate::council::CouncilResponse::Applied { .. },
        ) => {}
        Some(response) => super::lease::require_acceptance(response)?,
    }

    Ok(())
}

impl PickleState {
    /// Query before proving bytes, while the caller excludes local collection.
    pub(crate) async fn registry_gc_generation(&self) -> Result<u64, super::types::PickleError> {
        match self
            .registry_query(super::authority::RegistryQuery::GcGeneration)
            .await?
        {
            super::authority::RegistryQueryResponse::GcGeneration(generation) => Ok(generation),
            _ => Err(super::types::PickleError::ReplicationFailed(
                "invalid registry GC generation response".into(),
            )),
        }
    }

    pub(crate) async fn propose(
        &self,
        mutation: super::authority::RegistryMutation,
    ) -> Result<Option<crate::council::CouncilResponse>, super::types::PickleError> {
        if let Some(forwarder) = &self.forwarder {
            return forwarder
                .write(self.council.as_ref(), mutation)
                .await
                .map(Some);
        }
        let Some(council) = &self.council else {
            return Ok(None);
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            // A follower's council refuses with ForwardToLeader, so only a
            // leader's own clock is ever stamped here.
            council.write(mutation.request(crate::testkit::lease::now_unix_millis())),
        )
        .await
        .map_err(|_| {
            super::types::PickleError::ReplicationFailed(
                "registry proposal timed out; retry to establish acceptance".into(),
            )
        })?
        .map(Some)
        .map_err(|error| super::types::PickleError::ReplicationFailed(error.to_string()))
    }
}

impl PickleState {
    /// Hold the manifest writer guard from arbitration through physical deletion.
    /// Caller cancellation retains ownership; failed persistence deletes nothing.
    pub async fn collect_garbage(
        &self,
        report: super::types::GcReport,
    ) -> Result<Vec<Digest>, super::types::PickleError> {
        let state = self.clone();
        tokio::spawn(async move { state.collect_garbage_owned(report).await })
            .await
            .map_err(|error| {
                super::types::PickleError::CatalogPersist(format!("GC task failed: {error}"))
            })?
    }

    async fn collect_garbage_owned(
        &self,
        report: super::types::GcReport,
    ) -> Result<Vec<Digest>, super::types::PickleError> {
        use super::types::PickleError;
        let mut catalog = Arc::clone(&self.catalog).write_owned().await;
        let authoritative = match self
            .propose(super::authority::RegistryMutation::GarbageCollection(
                report.clone(),
            ))
            .await?
        {
            Some(crate::council::CouncilResponse::GcApproved { approved }) => Some(approved),
            None => None,
            Some(response) => {
                return Err(PickleError::ReplicationFailed(format!(
                    "GC arbitration refused: {response:?}"
                )));
            }
        };
        let store = Arc::clone(&self.store);
        let persist = self.persist_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut next = catalog.clone();
            let approved = match authoritative {
                Some(approved) => {
                    next.apply_gc_report(&super::types::GcReport {
                        node_id: report.node_id,
                        deleted_layers: approved.clone(),
                    });
                    approved
                }
                None => next.apply_gc_report(&report),
            };
            if approved.is_empty() {
                return Ok(Vec::new());
            }
            if let Some(path) = persist {
                next.persist_to(&path)?;
            }
            *catalog = next;
            let referenced = super::gc::referenced_digests(&catalog);
            Ok(super::gc::delete_approved(&store, &approved, &referenced))
        })
        .await
        .map_err(|error| PickleError::CatalogPersist(error.to_string()))?
    }
}

/// Build the OCI Distribution API router.
///
/// Repository names can be multi-segment (`team/app`, `cache/<host>/<repo>`
/// — REG8). Axum can't put a wildcard *before* a fixed suffix like
/// `/blobs/…`, so instead of one route per shape we capture the whole path
/// after `/v2/` with a trailing wildcard and split off the OCI operation
/// suffix ourselves in `dispatch_v2`. The repository name is then
/// whatever precedes that suffix, however many segments it spans.
pub fn router(state: PickleState) -> Router {
    let writers = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_WRITES));
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
        .layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let writers = Arc::clone(&writers);
                async move {
                    let _permit = if matches!(
                        *request.method(),
                        axum::http::Method::POST
                            | axum::http::Method::PATCH
                            | axum::http::Method::PUT
                    ) {
                        match writers.try_acquire_owned() {
                            Ok(permit) => Some(permit),
                            Err(_) => {
                                return (
                                    StatusCode::TOO_MANY_REQUESTS,
                                    [("retry-after", "1")],
                                    "registry write capacity exhausted",
                                )
                                    .into_response();
                            }
                        }
                    } else {
                        None
                    };
                    next.run(request).await
                }
            },
        ))
        .with_state(state)
}

/// The parsed shape of an OCI `/v2/{name}/…` request.
enum V2Route {
    /// Internal storage-node confirmation of a complete existing image.
    Copy { name: String, digest: String },
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
    if let Some((name, digest)) = rest.rsplit_once("/copies/") {
        return Some(V2Route::Copy {
            name: name.to_string(),
            digest: digest.to_string(),
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
    body: axum::body::Body,
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
        (Method::POST, V2Route::Copy { name, digest }) => {
            let principal = match state.authorise_write(&headers).await {
                Ok(principal) => principal,
                Err(response) => return response,
            };
            let internal = principal.as_ref().is_some_and(|context| {
                context.principal_id == crate::sesame::auth::SYSTEM_PRINCIPAL
            });
            let anonymous_local = principal.is_none()
                && state.council.is_none()
                && state.forwarder.is_none()
                && state.allow_unauthenticated_bootstrap
                && !super::lease::is_test_repository(&name);
            if !internal && !anonymous_local {
                return StatusCode::FORBIDDEN.into_response();
            }
            let digest = match Digest::new(&digest) {
                Ok(digest) => digest,
                Err(error) => return registry_write_error(error),
            };
            match tokio::time::timeout(UPLOAD_TIMEOUT, state.confirm_image_copy(&name, &digest))
                .await
            {
                Ok(Ok(receipt)) => Json(receipt).into_response(),
                Ok(Err(error)) => registry_write_error(error),
                Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
            }
        }
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
            if let Err(response) = state.authorise_write(&headers).await {
                return response;
            }
            match tokio::time::timeout(
                UPLOAD_TIMEOUT,
                axum::body::to_bytes(body, MAX_MANIFEST_BYTES),
            )
            .await
            {
                Ok(Ok(bytes)) => manifest_put(&state, &name, &reference, &headers, bytes).await,
                Ok(Err(_)) => StatusCode::PAYLOAD_TOO_LARGE.into_response(),
                Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
            }
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
    body: axum::body::Body,
) -> Response {
    // Registry writes require a principal once auth is configured (REG4).
    let principal = match state.authorise_write(headers_in).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let principal_id = principal
        .as_ref()
        .map(|context| context.principal_id.as_str());
    let access = match state
        .authorise_repository(name, headers_in, principal.as_ref())
        .await
    {
        Ok(access) => access,
        Err(response) => return response,
    };

    // Register monolithic requests too, so cancellation is covered by the TTL reaper.
    if let Some(digest_str) = query.digest {
        if Digest::new(&digest_str).is_err() {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let upload_id = match state
            .initiate_owned_upload(name, principal_id, &access)
            .await
        {
            Ok(id) => id,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        drop(access);
        return blob_upload_complete(state, name, &upload_id, &digest_str, headers_in, body).await;
    }

    // Chunked upload: start a session and register it for TTL tracking.
    match state
        .initiate_owned_upload(name, principal_id, &access)
        .await
    {
        Ok(upload_id) => {
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
    body: axum::body::Body,
) -> Response {
    let principal = match state.authorise_write(headers_in).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let principal_id = principal
        .as_ref()
        .map(|context| context.principal_id.as_str());
    let access = match state
        .authorise_repository(name, headers_in, principal.as_ref())
        .await
    {
        Ok(access) => access,
        Err(response) => return response,
    };
    let Some(writer) = state
        .sessions
        .claim_writer(upload_id, name, principal_id)
        .await
    else {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session unknown, busy or belongs to another repository or principal"
                .to_string(),
        );
    };
    // An upload session that outlived its TTL is refused and swept (REG8),
    // so an abandoned push can't dribble chunks into a stale temp forever.
    let now = std::time::SystemTime::now();
    if !state.sessions.touch(upload_id, 0, now).await {
        discard_upload(state, upload_id).await;
        return oci_error(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session unknown or expired".to_string(),
        );
    }
    let _writer = writer;
    let _repository_writer = access;
    match stream_upload(state, upload_id, body).await {
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
        Err(response) => response,
    }
}

/// The calling request owns the writer; retain a fenced session on cleanup error.
async fn discard_upload(state: &PickleState, upload_id: &str) {
    state.sessions.retire(upload_id).await;
    match state.store.cancel_upload(upload_id).await {
        Ok(()) => {
            state.sessions.complete(upload_id).await;
        }
        Err(error) => eprintln!("pickle: upload {upload_id} cleanup will retry: {error}"),
    }
}

/// Consume one request incrementally. On failure discard its partial upload.
#[allow(clippy::result_large_err)]
async fn stream_upload(
    state: &PickleState,
    upload_id: &str,
    body: axum::body::Body,
) -> Result<u64, Response> {
    let write = async {
        let mut stream = body.into_data_stream();
        let mut received = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
            received = received.saturating_add(chunk.len());
            if received > MAX_REQUEST_BYTES {
                return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
            }
            state
                .store
                .write_upload_chunk(upload_id, &chunk)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
        }
        state
            .store
            .upload_size(upload_id)
            .await
            .map_err(|_| StatusCode::BAD_REQUEST.into_response())
    };
    let result = match tokio::time::timeout(UPLOAD_TIMEOUT, write).await {
        Ok(result) => result,
        Err(_) => Err(StatusCode::REQUEST_TIMEOUT.into_response()),
    };
    if result.is_err() {
        discard_upload(state, upload_id).await;
    }
    result
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
    body: axum::body::Body,
) -> Response {
    let principal = match state.authorise_write(headers_in).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    let principal_id = principal
        .as_ref()
        .map(|context| context.principal_id.as_str());
    let access = match state
        .authorise_repository(name, headers_in, principal.as_ref())
        .await
    {
        Ok(access) => access,
        Err(response) => return response,
    };
    let Some(writer) = state
        .sessions
        .claim_writer(upload_id, name, principal_id)
        .await
    else {
        return oci_error(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session unknown, busy or belongs to another repository or principal"
                .to_string(),
        );
    };
    if !state
        .sessions
        .touch(upload_id, 0, std::time::SystemTime::now())
        .await
    {
        discard_upload(state, upload_id).await;
        return oci_error(
            StatusCode::BAD_REQUEST,
            "BLOB_UPLOAD_UNKNOWN",
            "upload session unknown or expired".to_string(),
        );
    }
    let Ok(digest) = Digest::new(digest_str) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid digest"})),
        )
            .into_response();
    };

    if let Err(response) = stream_upload(state, upload_id, body).await {
        return response;
    }

    // Enforce the storage quota against the fully-assembled blob before it is
    // committed (M10). Every upload path only knows its total size here — the on-disk
    // temp file is authoritative. Over quota: drop the temp and the session so
    // nothing lands in the blob store.
    match state.store.upload_size(upload_id).await {
        Ok(incoming) => {
            if let Err(response) = state.enforce_quota(name, incoming).await {
                discard_upload(state, upload_id).await;
                return response;
            }
        }
        Err(super::types::PickleError::InvalidUploadId(_)) => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    }

    state.sessions.retire(upload_id).await;
    let result = state
        .store
        .complete_upload_guarded(upload_id, &digest, Some(writer), access.guard.clone())
        .await;
    if result.is_ok() {
        state.sessions.complete(upload_id).await;
    } else {
        discard_upload(state, upload_id).await;
    }
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

fn registry_write_error(error: super::types::PickleError) -> Response {
    let status = match error {
        super::types::PickleError::LeaseDenied(_) => StatusCode::FORBIDDEN,
        super::types::PickleError::ReplicationFailed(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    oci_error(status, "DENIED", error.to_string())
}

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
    let principal = match state.authorise_write(headers).await {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if super::lease::is_test_repository(name)
        && principal
            .as_ref()
            .is_some_and(|p| p.principal_id == crate::sesame::auth::SYSTEM_PRINCIPAL)
    {
        return registry_write_error(super::types::PickleError::LeaseDenied(
            "internal replication cannot publish a leased manifest".into(),
        ));
    }
    let access = match state
        .authorise_repository(name, headers, principal.as_ref())
        .await
    {
        Ok(access) => access,
        Err(response) => return response,
    };

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
    if let Err(e) = store_blob_off_runtime(
        state,
        body.to_vec(),
        manifest_digest.clone(),
        access.guard.clone(),
    )
    .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to store manifest blob: {e}")})),
        )
            .into_response();
    }
    if let Err(error) =
        record_commit_with_access(state, manifest, reference.to_string(), &access).await
    {
        return registry_write_error(error);
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "docker-content-digest",
        manifest_digest
            .as_str()
            .parse()
            .expect("ASCII header value"),
    );
    // Cluster metadata is committed. Blob redundancy still converges through
    // the heal loop, so acceptance must not claim that replication has finished.
    headers.insert(
        "oci-replication",
        axum::http::HeaderValue::from_static("pending"),
    );
    (StatusCode::CREATED, headers).into_response()
}

/// `GET /v2/{name}/manifests/{reference}` — pull a manifest.
///
/// `reference` can be a tag (e.g. `latest`) or a digest (e.g. `sha256:abc...`).
/// Docker pulls sub-manifests by digest when resolving manifest lists.
async fn manifest_get(state: &PickleState, name: &str, reference: &str) -> Response {
    // Shared blob bytes do not establish that this repository still exists.
    // Tags and digests must both resolve through its current metadata.
    let catalog = match state.catalog_snapshot(name).await {
        Ok(catalog) => catalog,
        Err(error) => return registry_write_error(error),
    };
    let manifest = match Digest::new(reference) {
        Ok(digest) => catalog.get_repository_manifest(name, digest.as_str()),
        Err(_) => catalog.get_manifest_by_tag(name, reference),
    };
    let Some(manifest) = manifest else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let digest = manifest.digest.clone();
    let store = state.store.clone();
    let read_digest = digest.clone();
    let data = match tokio::task::spawn_blocking(move || store.read_blob(&read_digest)).await {
        Ok(Ok(data)) => data,
        Ok(Err(_)) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", detect_manifest_content_type(&data))
        .header("docker-content-digest", digest.as_str())
        .body(axum::body::Body::from(data))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
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
    let catalog = match state.catalog_snapshot(name).await {
        Ok(catalog) => catalog,
        Err(error) => return registry_write_error(error),
    };
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

    #[tokio::test]
    async fn copy_confirmation_rehashes_all_blobs_and_persists_without_replaying_tags() {
        let (mut state, directory) = test_state();
        state.persist_path = Some(directory.path().join("catalog.json"));
        let config = compute_sha256(b"configuration");
        state.store.write_blob(b"configuration", &config).unwrap();
        let body = manifest_body(&config, 13);
        let digest = compute_sha256(&body);
        let app = router(state.clone());
        assert_eq!(
            put_manifest(&app, "/v2/ordinary/manifests/latest", body.clone())
                .await
                .status(),
            StatusCode::CREATED
        );
        // Model committed metadata from another storage node.
        for (_, holders) in &mut state.catalog.write().await.layer_locations {
            *holders = std::collections::BTreeSet::from([99]);
        }
        let tags = state.catalog.read().await.tags.clone();
        std::fs::write(state.store.blob_path(&config), b"corrupt").unwrap();
        assert!(matches!(
            state.confirm_image_copy("ordinary", &digest).await,
            Err(super::super::types::PickleError::DigestMismatch { .. })
        ));
        assert_eq!(
            state.catalog.read().await.layer_holders(digest.as_str()),
            std::collections::BTreeSet::from([99])
        );
        std::fs::write(state.store.blob_path(&config), b"configuration").unwrap();
        state.store.delete_blob(&digest).unwrap();
        assert!(state.confirm_image_copy("ordinary", &digest).await.is_err());
        state.store.write_blob(&body, &digest).unwrap();
        let receipt = state.confirm_image_copy("ordinary", &digest).await.unwrap();
        assert_eq!(receipt.node_id, state.node_raft_id);
        let persisted = ManifestCatalog::load_from(state.persist_path.as_ref().unwrap()).unwrap();
        for blob in [&config, &digest] {
            assert_eq!(
                persisted.layer_holders(blob.as_str()),
                std::collections::BTreeSet::from([7, 99])
            );
        }
        assert_eq!(persisted.tags, tags);
        assert!(state.confirm_image_copy("missing", &digest).await.is_err());
        state.catalog.write().await.retire_repository("ordinary");
        assert!(state.confirm_image_copy("ordinary", &digest).await.is_err());
    }

    #[tokio::test]
    async fn copy_confirmation_requires_service_authority_when_authentication_is_configured() {
        let (mut state, _directory) = test_state();
        let token = crate::sesame::token::create_token(
            "admin",
            crate::sesame::types::ApiRole::Admin,
            crate::sesame::types::TokenScope::default(),
            None,
        )
        .unwrap();
        let tokens = crate::sesame::auth::new_token_store();
        tokens.write().await.push(token.token);
        state.auth = Some(crate::sesame::auth::AuthState::new(
            tokens,
            Some("internal".into()),
        ));
        state.allow_unauthenticated_bootstrap = false;
        let config = compute_sha256(b"config");
        state.store.write_blob(b"config", &config).unwrap();
        let body = manifest_body(&config, 6);
        let digest = compute_sha256(&body);
        // Use the same authenticated OCI publication path before testing the internal route.
        let app = router(state.clone());
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::put("/v2/ordinary/manifests/latest")
                    .header("authorization", format!("Bearer {}", token.plaintext))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        for (bearer, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(token.plaintext.as_str()), StatusCode::FORBIDDEN),
            (Some("internal"), StatusCode::OK),
        ] {
            let mut request =
                axum::http::Request::post(format!("/v2/ordinary/copies/{}", digest.as_str()));
            if let Some(bearer) = bearer {
                request = request.header("authorization", format!("Bearer {bearer}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            assert_eq!(status, expected, "{}", String::from_utf8_lossy(&body));
        }
    }

    #[tokio::test]
    async fn monolithic_upload_reaches_disk_before_the_request_finishes() {
        let (state, dir) = test_state();
        let store = Arc::clone(&state.store);
        let digest = compute_sha256(b"firstsecond");
        let app = router(state);
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::io::Error>>(1);
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/v2/web/blobs/uploads/?digest={}", digest.as_str()))
            .body(Body::from_stream(
                tokio_stream::wrappers::ReceiverStream::new(rx),
            ))
            .unwrap();
        let task = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
        tx.send(Ok(axum::body::Bytes::from_static(b"first")))
            .await
            .unwrap();
        let reached_disk = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(mut entries) = tokio::fs::read_dir(dir.path().join("uploads")).await {
                    while let Some(entry) = entries.next_entry().await.unwrap() {
                        if entry.metadata().await.unwrap().len() == 5 {
                            return;
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        if reached_disk.is_err() {
            task.abort();
        }
        assert!(
            reached_disk.is_ok(),
            "request was buffered until EOF instead of streamed to disk"
        );
        tx.send(Ok(axum::body::Bytes::from_static(b"second")))
            .await
            .unwrap();
        drop(tx);
        assert_eq!(task.await.unwrap().status(), StatusCode::CREATED);
        assert_eq!(store.read_blob(&digest).unwrap(), b"firstsecond");
    }

    #[tokio::test]
    async fn unauthorised_upload_is_refused_without_reading_the_body() {
        let (state, _dir, _) = read_gated_state().await;
        let app = router(state);
        let body = Body::from_stream(futures_util::stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v2/web/blobs/uploads/")
            .body(body)
            .unwrap();
        let response =
            tokio::time::timeout(std::time::Duration::from_millis(200), app.oneshot(request)).await;
        assert_eq!(
            response
                .expect("auth must precede body consumption")
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn registry_refuses_excess_writers_without_buffering_their_bodies() {
        let (state, _dir) = test_state();
        let app = router(state);
        let mut writers = Vec::new();
        let digest = compute_sha256(b"data");
        for _ in 0..MAX_CONCURRENT_WRITES {
            let body = Body::from_stream(futures_util::stream::pending::<
                Result<axum::body::Bytes, std::io::Error>,
            >());
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(format!("/v2/web/blobs/uploads/?digest={}", digest.as_str()))
                .body(body)
                .unwrap();
            let mut writer = Box::pin(app.clone().oneshot(request));
            assert!(futures_util::poll!(writer.as_mut()).is_pending());
            writers.push(writer);
        }
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v2/web/blobs/uploads/")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "1");
        drop(writers);
    }

    async fn serve_peer_body(
        body: Vec<u8>,
    ) -> (super::super::replication::Peer, tokio::task::JoinHandle<()>) {
        let app = Router::new().fallback(axum::routing::get(move || {
            let body = body.clone();
            async move { body }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = super::super::replication::Peer {
            node_id: 2,
            base_url: format!("http://{}", listener.local_addr().unwrap()),
        };
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (peer, server)
    }

    #[tokio::test]
    async fn peer_pull_cleans_up_corrupt_and_unpublishable_uploads() {
        let digest = compute_sha256(b"valid");
        for rename_failure in [false, true] {
            let (state, directory) = test_state();
            let bytes = if rename_failure { b"valid" } else { b"wrong" };
            let (peer, server) = serve_peer_body(bytes.to_vec()).await;
            if rename_failure {
                std::fs::create_dir_all(state.store.blob_path(&digest)).unwrap();
            }
            assert!(
                state
                    .pull_peer_blob(
                        &peer,
                        "ordinary",
                        &digest,
                        &reqwest::Client::new(),
                        std::time::Duration::from_secs(2)
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                std::fs::read_dir(directory.path().join("uploads"))
                    .unwrap()
                    .count(),
                0
            );
            assert!(
                state
                    .sessions
                    .sweep(std::time::SystemTime::now() + std::time::Duration::from_secs(7200))
                    .await
                    .is_empty()
            );
            server.abort();
            let _ = server.await;
        }
    }

    #[tokio::test]
    async fn peer_pull_retains_failed_temporary_file_deletion_for_retry() {
        let (state, directory) = test_state();
        let (peer, server) = serve_peer_body(b"valid".to_vec()).await;
        let pause = state.sessions.pause_registration().await;
        let owner = state.clone();
        let pull = tokio::spawn(async move {
            owner
                .pull_peer_blob(
                    &peer,
                    "ordinary",
                    &compute_sha256(b"valid"),
                    &reqwest::Client::new(),
                    std::time::Duration::from_secs(2),
                )
                .await
        });
        let uploads = directory.path().join("uploads");
        let file = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(entries) = std::fs::read_dir(&uploads)
                    && let Some(entry) = entries.flatten().next()
                {
                    break entry.path();
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let id = file.file_name().unwrap().to_str().unwrap().to_owned();
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        drop(pause);
        let error = pull.await.unwrap().unwrap_err();
        assert!(
            error.to_string().contains("cleanup remains pending"),
            "{error}"
        );
        assert_eq!(
            state.sessions.sweep(std::time::SystemTime::now()).await,
            vec![id]
        );
        assert_eq!(
            state
                .sessions
                .cleanup_expired(&state.store, std::time::SystemTime::now())
                .await
                .len(),
            1
        );
        std::fs::remove_dir(&file).unwrap();
        assert!(
            state
                .sessions
                .cleanup_expired(&state.store, std::time::SystemTime::now())
                .await
                .is_empty()
        );
        assert!(
            state
                .sessions
                .sweep(std::time::SystemTime::now())
                .await
                .is_empty()
        );
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn peer_pull_requires_active_repository_ownership_even_for_cached_bytes() {
        let (state, directory, _owner, _other) = leased_registry_state().await;
        let repository = "rbtest-run1/web";
        let digest = compute_sha256(b"valid");
        state.store.write_blob(b"valid", &digest).unwrap();
        let peer = super::super::replication::Peer {
            node_id: 2,
            base_url: "http://127.0.0.1:1".into(),
        };
        let client = reqwest::Client::new();
        let timeout = std::time::Duration::from_millis(50);
        assert!(
            state
                .pull_peer_blob(&peer, repository, &digest, &client, timeout)
                .await
                .is_err()
        );
        let lease = state.test_leases.get("run1").await.unwrap();
        state
            .test_leases
            .register_registry_writer(
                "run1",
                repository,
                99,
                Some(&lease.owner_id),
                crate::testkit::lease::now_unix_millis(),
            )
            .await
            .unwrap();
        state
            .pull_peer_blob(&peer, repository, &digest, &client, timeout)
            .await
            .unwrap();
        assert!(
            state.test_leases.get("run1").await.unwrap().repositories[repository]
                .contains(&state.node_raft_id)
        );
        assert_eq!(
            ManifestCatalog::load_from(&directory.path().join("catalog.json"))
                .unwrap()
                .repository_owners[repository],
            "run1"
        );
        state.test_leases.begin_cleanup("run1", None).await.unwrap();
        assert!(
            state
                .pull_peer_blob(&peer, repository, &digest, &client, timeout)
                .await
                .is_err()
        );
        assert_eq!(state.store.read_blob(&digest).unwrap(), b"valid");
    }

    #[tokio::test]
    async fn admitted_parallel_pull_finishes_while_repository_cleanup_is_queued() {
        let (state, _directory, _owner, _other) = leased_registry_state().await;
        let repository = "rbtest-run1/web";
        let lease = state.test_leases.get("run1").await.unwrap();
        let access = state
            .admit_repository_write(repository, Some("run1"), Some(&lease.owner_id), false)
            .await
            .unwrap();
        state.test_leases.begin_cleanup("run1", None).await.unwrap();
        state
            .test_leases
            .confirm_workloads_retired("run1")
            .await
            .unwrap();
        let receipt = super::super::authority::RegistryRetirement {
            lease_id: "run1".into(),
            repository: repository.into(),
        };
        let mut retire = Box::pin(state.retire_registry_repository(&receipt));
        assert!(futures_util::poll!(retire.as_mut()).is_pending());
        let (peer, server) = serve_peer_body(b"valid".to_vec()).await;
        let plan = super::super::p2p::DownloadPlan {
            fetches: vec![super::super::p2p::LayerFetch {
                digest: compute_sha256(b"valid"),
                peer: peer.clone(),
            }],
            unavailable: vec![],
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::super::p2p::pull_layers_parallel(
                plan,
                repository,
                &ManifestCatalog::default(),
                &[peer],
                &state,
                &access,
                &reqwest::Client::new(),
                1,
                std::time::Duration::from_secs(1),
            ),
        )
        .await
        .expect("already-admitted pulls must reuse their guard behind a queued cleanup")
        .unwrap();
        drop(access);
        retire.await.unwrap();
        assert!(state.test_leases.get("run1").await.unwrap().repositories[repository].is_empty());
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn peer_pull_stalled_body_expires_and_retires_its_upload() {
        let (state, directory) = test_state();
        let app = Router::new().fallback(axum::routing::get(|| async {
            Body::from_stream(futures_util::stream::pending::<
                Result<axum::body::Bytes, std::io::Error>,
            >())
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = super::super::replication::Peer {
            node_id: 2,
            base_url: format!("http://{}", listener.local_addr().unwrap()),
        };
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let error = state
            .pull_peer_blob(
                &peer,
                "ordinary",
                &compute_sha256(b"valid"),
                &reqwest::Client::new(),
                std::time::Duration::from_millis(150),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert_eq!(
            std::fs::read_dir(directory.path().join("uploads"))
                .unwrap()
                .count(),
            0
        );
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn cancelled_peer_pull_keeps_its_temporary_upload_owned() {
        use futures_util::StreamExt as _;
        let (state, directory) = test_state();
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        let digest = compute_sha256(b"first-last");
        let app = Router::new().route(
            &format!("/v2/ordinary/blobs/{}", digest.as_str()),
            axum::routing::get(move || {
                let release = released.clone();
                async move {
                    Body::from_stream(
                        futures_util::stream::iter([Ok::<_, std::io::Error>(
                            axum::body::Bytes::from_static(b"first-"),
                        )])
                        .chain(futures_util::stream::once(async move {
                            release.notified().await;
                            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"last"))
                        })),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = super::super::replication::Peer {
            node_id: 2,
            base_url: format!("http://{}", listener.local_addr().unwrap()),
        };
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let owner = state.clone();
        let requested = digest.clone();
        let caller = tokio::spawn(async move {
            owner
                .pull_peer_blob(
                    &peer,
                    "ordinary",
                    &requested,
                    &reqwest::Client::new(),
                    std::time::Duration::from_secs(5),
                )
                .await
        });
        let uploads = directory.path().join("uploads");
        let id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(entries) = std::fs::read_dir(&uploads) {
                    for entry in entries.flatten() {
                        if entry.metadata().unwrap().len() == 6 {
                            return entry.file_name().to_str().unwrap().to_owned();
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            state
                .sessions
                .is_active(&id, std::time::SystemTime::now())
                .await,
            "a peer temporary file must have an upload owner before bytes arrive"
        );
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !state.store.has_blob(&digest)
                || state
                    .sessions
                    .is_active(&id, std::time::SystemTime::now())
                    .await
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.store.read_blob(&digest).unwrap(), b"first-last");
        assert_eq!(std::fs::read_dir(&uploads).unwrap().count(), 0);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn test_repository_upload_requires_an_authenticated_lease_before_creating_files() {
        let (state, _directory) = test_state();
        let store = state.store.clone();
        let response = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/v2/rbtest-unowned/web/blobs/uploads/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let uploads = store.base_dir().join("uploads");
        assert!(!uploads.exists() || std::fs::read_dir(uploads).unwrap().count() == 0);
    }

    async fn leased_registry_state() -> (PickleState, tempfile::TempDir, String, String) {
        use crate::sesame::{
            auth, token,
            types::{ApiRole, TokenScope},
        };
        let (mut state, directory) = test_state();
        let owner =
            token::create_token("publisher", ApiRole::Deployer, TokenScope::default(), None)
                .unwrap();
        let other =
            token::create_token("publisher", ApiRole::Deployer, TokenScope::default(), None)
                .unwrap();
        let principal =
            auth::authenticate(&owner.plaintext, std::slice::from_ref(&owner.token)).unwrap();
        let tokens = auth::new_token_store();
        *tokens.write().await = vec![owner.token, other.token];
        state.auth = Some(auth::AuthState::new(tokens, Some("internal".into())));
        state.allow_unauthenticated_bootstrap = false;
        state.persist_path = Some(directory.path().join("catalog.json"));
        let now = crate::testkit::lease::now_unix_millis();
        state
            .test_leases
            .create(
                crate::testkit::lease::TestLease::new(
                    "run1".into(),
                    principal.principal_id,
                    "publisher".into(),
                    "rbtest-run1".into(),
                    now,
                    now + 60_000,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        (state, directory, owner.plaintext, other.plaintext)
    }

    fn lease_request(
        method: &str,
        path: &str,
        bearer: &str,
        lease: Option<&str>,
        body: Vec<u8>,
    ) -> axum::http::Request<Body> {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/vnd.oci.image.manifest.v1+json");
        if let Some(lease) = lease {
            request = request.header("x-reliaburger-test-lease", lease);
        }
        request.body(Body::from(body)).unwrap()
    }

    #[tokio::test]
    async fn leased_registry_uploads_require_exact_owner_and_wait_for_workload_retirement() {
        let (state, directory, owner, other) = leased_registry_state().await;
        let app = router(state.clone());
        let repository = "rbtest-run1/web";
        let initiate = "/v2/rbtest-run1/web/blobs/uploads/";
        for (bearer, lease) in [
            (&other, Some("run1")),
            (&owner, None),
            (&owner, Some("missing")),
            (&owner, Some("")),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(lease_request("POST", initiate, bearer, lease, vec![]))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            app.clone()
                .oneshot(lease_request(
                    "POST",
                    "/v2/ordinary/blobs/uploads/",
                    &owner,
                    Some("run1"),
                    vec![]
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert!(
            state
                .test_leases
                .get("run1")
                .await
                .unwrap()
                .repositories
                .is_empty()
        );
        let response = app
            .clone()
            .oneshot(lease_request(
                "POST",
                initiate,
                &owner,
                Some("run1"),
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let location = response.headers()["location"].to_str().unwrap().to_owned();
        let id = response.headers()["docker-upload-uuid"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            ManifestCatalog::load_from(&directory.path().join("catalog.json"))
                .unwrap()
                .repository_owners[repository],
            "run1"
        );
        assert_eq!(
            state.test_leases.get("run1").await.unwrap().repositories[repository],
            std::collections::BTreeSet::from([state.node_raft_id])
        );
        assert_eq!(
            app.clone()
                .oneshot(lease_request(
                    "PATCH",
                    &location,
                    &owner,
                    Some("run1"),
                    b"partial".to_vec()
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        state.test_leases.begin_cleanup("run1", None).await.unwrap();
        state.reap_registry_leases_once().await.unwrap();
        assert_eq!(
            state.store.upload_size(&id).await.unwrap(),
            7,
            "Cleaning alone cannot delete an image still used by workloads"
        );
        assert_eq!(
            app.clone()
                .oneshot(lease_request(
                    "PATCH",
                    &location,
                    &owner,
                    Some("run1"),
                    b"late".to_vec()
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.oneshot(lease_request("POST", initiate, "internal", None, vec![]))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        state
            .test_leases
            .confirm_workloads_retired("run1")
            .await
            .unwrap();
        state.reap_registry_leases_once().await.unwrap();
        assert!(state.store.upload_size(&id).await.is_err());
        assert!(state.test_leases.get("run1").await.unwrap().repositories[repository].is_empty());
        assert!(
            !ManifestCatalog::load_from(&directory.path().join("catalog.json"))
                .unwrap()
                .repository_owners
                .contains_key(repository)
        );
        state.test_leases.finish_cleanup("run1").await.unwrap();
    }

    #[tokio::test]
    async fn leased_manifest_cleanup_preserves_ordinary_shared_content() {
        let (state, _directory, owner, _) = leased_registry_state().await;
        let app = router(state.clone());
        let config = compute_sha256(b"shared");
        let complete = format!(
            "/v2/rbtest-run1/web/blobs/uploads/?digest={}",
            config.as_str()
        );
        assert_eq!(
            app.clone()
                .oneshot(lease_request(
                    "POST",
                    &complete,
                    &owner,
                    Some("run1"),
                    b"shared".to_vec()
                ))
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
        let body = manifest_body(&config, 6);
        let digest = compute_sha256(&body);
        for (path, lease) in [
            ("/v2/rbtest-run1/web/manifests/latest", Some("run1")),
            ("/v2/ordinary/manifests/latest", None),
        ] {
            assert_eq!(
                app.clone()
                    .oneshot(lease_request("PUT", path, &owner, lease, body.clone()))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CREATED
            );
        }
        // System replication may receive blobs, but cannot publish user manifests.
        assert_eq!(
            app.oneshot(lease_request(
                "PUT",
                "/v2/rbtest-run1/web/manifests/system",
                "internal",
                None,
                body
            ))
            .await
            .unwrap()
            .status(),
            StatusCode::FORBIDDEN
        );
        state.test_leases.begin_cleanup("run1", None).await.unwrap();
        state
            .test_leases
            .confirm_workloads_retired("run1")
            .await
            .unwrap();
        state.reap_registry_leases_once().await.unwrap();
        let catalog = state.catalog.read().await;
        assert!(
            catalog
                .get_manifest_by_tag("rbtest-run1/web", "latest")
                .is_none()
        );
        assert!(catalog.get_manifest_by_tag("ordinary", "latest").is_some());
        assert!(catalog.referenced_digest_set().contains(digest.as_str()));
        drop(catalog);
        assert!(
            state
                .collect_garbage(super::super::types::GcReport {
                    node_id: state.node_raft_id,
                    deleted_layers: vec![config.clone(), digest.clone()]
                })
                .await
                .unwrap()
                .is_empty()
        );
        assert!(state.store.has_blob(&config));
        assert!(state.store.has_blob(&digest));
        let reader = router(state.clone());
        for (repository, expected) in [
            ("rbtest-run1/web", StatusCode::NOT_FOUND),
            ("ordinary", StatusCode::OK),
        ] {
            let response = reader
                .clone()
                .oneshot(
                    axum::http::Request::get(format!(
                        "/v2/{repository}/manifests/{}",
                        digest.as_str()
                    ))
                    .body(Body::empty())
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                expected,
                "digest reads must respect repository retirement"
            );
        }
    }

    #[tokio::test]
    async fn failed_registry_generation_persistence_never_creates_an_upload() {
        let (mut state, directory, owner, _) = leased_registry_state().await;
        let blocked = directory.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        state.persist_path = Some(blocked.join("catalog.json"));
        let response = router(state.clone())
            .oneshot(lease_request(
                "POST",
                "/v2/rbtest-run1/web/blobs/uploads/",
                &owner,
                Some("run1"),
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let uploads = state.store.base_dir().join("uploads");
        assert!(!uploads.exists() || std::fs::read_dir(uploads).unwrap().count() == 0);
        assert!(
            !state
                .test_leases
                .get("run1")
                .await
                .unwrap()
                .registry_retirement_confirmed()
        );
        assert!(state.catalog.read().await.repository_owners.is_empty());
    }

    #[tokio::test]
    async fn failed_registry_retirement_retains_the_receipt_for_retry() {
        let (mut state, directory, owner, _) = leased_registry_state().await;
        let response = router(state.clone())
            .oneshot(lease_request(
                "POST",
                "/v2/rbtest-run1/web/blobs/uploads/",
                &owner,
                Some("run1"),
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let id = response.headers()["docker-upload-uuid"].to_str().unwrap();
        let upload = directory.path().join("uploads").join(id);
        std::fs::remove_file(&upload).unwrap();
        std::fs::create_dir(&upload).unwrap();
        state.test_leases.begin_cleanup("run1", None).await.unwrap();
        state
            .test_leases
            .confirm_workloads_retired("run1")
            .await
            .unwrap();
        assert!(state.reap_registry_leases_once().await.is_err());
        assert!(state.test_leases.finish_cleanup("run1").await.is_err());
        assert_eq!(
            state.test_leases.get("run1").await.unwrap().repositories["rbtest-run1/web"],
            std::collections::BTreeSet::from([state.node_raft_id])
        );
        std::fs::remove_dir(upload).unwrap();
        let blocked = directory.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        state.persist_path = Some(blocked.join("catalog.json"));
        assert!(state.reap_registry_leases_once().await.is_err());
        assert_eq!(
            state.catalog.read().await.repository_owners["rbtest-run1/web"],
            "run1"
        );
        assert!(state.test_leases.finish_cleanup("run1").await.is_err());
        state.persist_path = Some(directory.path().join("catalog.json"));
        state.reap_registry_leases_once().await.unwrap();
        state.test_leases.finish_cleanup("run1").await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_upload_creation_remains_owned_until_registration_and_cleanup() {
        let (state, directory, owner, _) = leased_registry_state().await;
        let paused = state.sessions.pause_registration().await;
        let app = router(state.clone());
        let upload = tokio::spawn(async move {
            app.oneshot(lease_request(
                "POST",
                "/v2/rbtest-run1/web/blobs/uploads/",
                &owner,
                Some("run1"),
                vec![],
            ))
            .await
        });
        let uploads = directory.path().join("uploads");
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if uploads.exists() && std::fs::read_dir(&uploads).unwrap().count() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        upload.abort();
        assert!(upload.await.unwrap_err().is_cancelled());
        state.test_leases.begin_cleanup("run1", None).await.unwrap();
        state
            .test_leases
            .confirm_workloads_retired("run1")
            .await
            .unwrap();
        let mut cleanup = Box::pin(state.reap_registry_leases_once());
        assert!(
            futures_util::poll!(cleanup.as_mut()).is_pending(),
            "unregistered creation still holds repository ownership"
        );
        drop(paused);
        tokio::time::timeout(std::time::Duration::from_secs(3), cleanup)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read_dir(&uploads).unwrap().count(), 0);
        assert!(
            state
                .test_leases
                .get("run1")
                .await
                .unwrap()
                .registry_retirement_confirmed()
        );
    }

    #[tokio::test]
    async fn standalone_manifest_commit_rechecks_cleanup_after_writer_admission() {
        let (state, _directory, owner, _) = leased_registry_state().await;
        let headers = lease_request("POST", "/", &owner, Some("run1"), vec![])
            .headers()
            .clone();
        let principal = state.authorise_write(&headers).await.unwrap();
        let access = state
            .authorise_repository("rbtest-run1/web", &headers, principal.as_ref())
            .await
            .unwrap();
        state.test_leases.begin_cleanup("run1", None).await.unwrap();
        let digest = compute_sha256(b"content");
        state.store.write_blob(b"content", &digest).unwrap();
        let manifest = ImageManifest {
            digest: digest.clone(),
            config: LayerDescriptor {
                digest,
                size: 7,
                media_type: "config".into(),
            },
            layers: vec![],
            repository: "rbtest-run1/web".into(),
            tags: Default::default(),
            total_size: 7,
            pushed_at: std::time::SystemTime::now(),
            pushed_by: state.node_raft_id,
            signature: None,
        };
        assert!(matches!(
            record_commit_with_access(&state, manifest, "late".into(), &access).await,
            Err(super::super::types::PickleError::LeaseDenied(_))
        ));
        assert!(state.catalog.read().await.manifests.is_empty());
    }

    #[tokio::test]
    async fn a_busy_registry_writer_does_not_starve_other_repository_retirements() {
        let (state, _directory, owner, _) = leased_registry_state().await;
        let headers = lease_request("POST", "/", &owner, Some("run1"), vec![])
            .headers()
            .clone();
        let principal = state.authorise_write(&headers).await.unwrap();
        let busy = state
            .authorise_repository("rbtest-run1/a", &headers, principal.as_ref())
            .await
            .unwrap();
        let response = router(state.clone())
            .oneshot(lease_request(
                "POST",
                "/v2/rbtest-run1/z/blobs/uploads/",
                &owner,
                Some("run1"),
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let id = response.headers()["docker-upload-uuid"].to_str().unwrap();
        state.test_leases.begin_cleanup("run1", None).await.unwrap();
        state
            .test_leases
            .confirm_workloads_retired("run1")
            .await
            .unwrap();
        let mut cleanup = Box::pin(state.reap_registry_leases_once());
        tokio::time::pause();
        assert!(futures_util::poll!(cleanup.as_mut()).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        tokio::time::resume();
        assert!(cleanup.await.is_err());
        let lease = state.test_leases.get("run1").await.unwrap();
        assert!(!lease.repositories["rbtest-run1/a"].is_empty());
        assert!(lease.repositories["rbtest-run1/z"].is_empty());
        assert!(state.store.upload_size(id).await.is_err());
        drop(busy);
        state.reap_registry_leases_once().await.unwrap();
        state.test_leases.finish_cleanup("run1").await.unwrap();
    }

    #[tokio::test]
    async fn upload_session_cannot_be_written_through_a_different_repository() {
        let (state, _dir) = test_state();
        let id = state.store.initiate_upload().await.unwrap();
        state
            .sessions
            .register(&id, "team-a/web", None, std::time::SystemTime::now())
            .await;
        let store = Arc::clone(&state.store);
        let app = router(state);
        let request = axum::http::Request::builder()
            .method("PATCH")
            .uri(format!("/v2/team-b/web/blobs/uploads/{id}"))
            .body(Body::from("unwanted"))
            .unwrap();
        assert_eq!(
            app.oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(store.upload_size(&id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn upload_session_belongs_to_the_exact_authenticated_credential() {
        use crate::sesame::{
            auth, token,
            types::{ApiRole, TokenScope},
        };
        let (mut state, _directory) = test_state();
        // Reusing the human-readable token name must not inherit ownership.
        let alice =
            token::create_token("publisher", ApiRole::Deployer, TokenScope::default(), None)
                .unwrap();
        let bob = token::create_token("publisher", ApiRole::Deployer, TokenScope::default(), None)
            .unwrap();
        let tokens = auth::new_token_store();
        *tokens.write().await = vec![alice.token.clone(), bob.token.clone()];
        state.auth = Some(auth::AuthState::new(
            tokens.clone(),
            Some("internal".into()),
        ));
        state.allow_unauthenticated_bootstrap = false;
        let store = state.store.clone();
        let app = router(state);
        let request = |method: &str, uri: &str, bearer: &str, data: &'static str| {
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::from(data))
                .unwrap()
        };
        let response = app
            .clone()
            .oneshot(request(
                "POST",
                "/v2/team/web/blobs/uploads/",
                &alice.plaintext,
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let location = response.headers()["location"].to_str().unwrap().to_owned();
        let id = response.headers()["docker-upload-uuid"]
            .to_str()
            .unwrap()
            .to_owned();
        let digest = compute_sha256(b"owned");
        let complete = format!("{location}?digest={}", digest.as_str());
        for bearer in [&bob.plaintext, "internal"] {
            for (method, uri) in [("PATCH", &location), ("PUT", &complete)] {
                let response = app
                    .clone()
                    .oneshot(request(method, uri, bearer, "owned"))
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "{method} must refuse another principal"
                );
                assert_eq!(store.upload_size(&id).await.unwrap(), 0);
                assert!(!store.has_blob(&digest));
            }
        }
        *tokens.write().await = vec![bob.token];
        assert_eq!(
            app.clone()
                .oneshot(request("PATCH", &location, &alice.plaintext, "owned"))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        tokens.write().await.push(alice.token);
        assert_eq!(
            app.clone()
                .oneshot(request("PATCH", &location, &alice.plaintext, "owned"))
                .await
                .unwrap()
                .status(),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            app.oneshot(request("PUT", &complete, &alice.plaintext, ""))
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(store.read_blob(&digest).unwrap(), b"owned");
    }

    #[tokio::test]
    async fn expired_upload_cannot_be_completed() {
        let (state, _dir) = test_state();
        let id = state.store.initiate_upload().await.unwrap();
        state
            .sessions
            .register(&id, "web", None, std::time::SystemTime::UNIX_EPOCH)
            .await;
        let digest = compute_sha256(b"data");
        let request = axum::http::Request::builder()
            .method("PUT")
            .uri(format!(
                "/v2/web/blobs/uploads/{id}?digest={}",
                digest.as_str()
            ))
            .body(Body::from("data"))
            .unwrap();
        let store = Arc::clone(&state.store);
        assert_eq!(
            router(state).oneshot(request).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        assert!(!store.has_blob(&digest));
    }

    #[tokio::test]
    async fn a_manifest_push_requires_a_committed_cluster_catalogue() {
        use crate::council::CouncilNode;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::types::{CouncilConfig, CouncilNodeInfo};
        let (mut state, directory) = test_state();
        let network = InMemoryRaftRouter::new();
        let council = Arc::new(
            CouncilNode::new(
                state.node_raft_id,
                CouncilConfig::default(),
                InMemoryRaftNetworkFactory::new(state.node_raft_id, network.clone()),
                crate::council::log_store::MemLogStore::new(),
                crate::council::state_machine::CouncilStateMachine::new(),
                None,
            )
            .await
            .unwrap(),
        );
        network
            .register(state.node_raft_id, council.raft().clone())
            .await;
        state.council = Some(council.clone());
        state.persist_path = Some(directory.path().join("catalog.json"));
        let app = test_router(state.clone());
        let config = push_blob(&app, "ordinary", b"config").await;
        let body = manifest_body(&config, 6);
        let response = put_manifest(&app, "/v2/ordinary/manifests/latest", body.clone()).await;
        // OCI clients must receive a retryable error, not a success-class
        // response that only a custom replication header contradicts.
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(council.manifest_catalog().await.manifests.is_empty());
        assert!(state.store.has_blob(&config));
        council
            .initialize(std::collections::BTreeMap::from([(
                state.node_raft_id,
                CouncilNodeInfo {
                    addr: "127.0.0.1:19001".parse().unwrap(),
                    name: "registry".into(),
                },
            )]))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !council.is_leader().await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            put_manifest(&app, "/v2/ordinary/manifests/latest", body)
                .await
                .status(),
            StatusCode::CREATED
        );
        assert!(
            council
                .manifest_catalog()
                .await
                .get_manifest_by_tag("ordinary", "latest")
                .is_some()
        );
        council.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_catalogue_persistence_does_not_acknowledge_or_publish_a_manifest() {
        let (mut state, directory) = test_state();
        let blocked = directory.path().join("catalog.json");
        std::fs::create_dir(&blocked).unwrap();
        state.persist_path = Some(blocked.clone());
        let catalog = state.catalog.clone();
        let app = test_router(state);
        let config = b"config";
        let digest = push_blob(&app, "myapp", config).await;
        let body = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"digest": digest.as_str(), "size": config.len()},
            "layers": []
        }))
        .unwrap();
        let response = put_manifest(&app, "/v2/myapp/manifests/latest", body.clone()).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            catalog
                .read()
                .await
                .get_manifest_by_tag("myapp", "latest")
                .is_none()
        );
        std::fs::remove_dir(&blocked).unwrap();
        let response = put_manifest(&app, "/v2/myapp/manifests/latest", body).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(
            ManifestCatalog::load_from(&blocked)
                .unwrap()
                .get_manifest_by_tag("myapp", "latest")
                .is_some()
        );
    }

    fn manifest_body(config: &Digest, size: usize) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"digest": config.as_str(), "size": size},
            "layers": []
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn cancelled_manifest_publication_retains_its_catalogue_guard_until_authority_replies() {
        cancelled_publication_owns_guard(false).await;
    }

    #[tokio::test]
    async fn cancelled_copy_confirmation_retains_its_catalogue_guard_until_authority_replies() {
        cancelled_publication_owns_guard(true).await;
    }

    async fn cancelled_publication_owns_guard(copy: bool) {
        use super::super::authority::{
            REGISTRY_PROPOSAL_PATH, REGISTRY_QUERY_PATH, RegistryForwarder, RegistryQueryResponse,
        };
        let (mut state, directory) = test_state();
        state.persist_path = Some(directory.path().join("catalog.json"));
        let digest = compute_sha256(b"publication");
        state.store.write_blob(b"publication", &digest).unwrap();
        let manifest = ImageManifest {
            repository: "ordinary".into(),
            digest: digest.clone(),
            tags: Default::default(),
            config: LayerDescriptor {
                digest,
                size: 11,
                media_type: "config".into(),
            },
            layers: vec![],
            total_size: 11,
            pushed_by: state.node_raft_id,
            pushed_at: std::time::SystemTime::now(),
            signature: None,
        };
        if copy {
            record_commit(&state, manifest.clone(), "latest".into())
                .await
                .unwrap();
        }
        let remote_catalogue = state.catalog.read().await.clone();
        let proposed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = proposed.clone();
        let released = release.clone();
        let app = Router::new()
            .route(
                REGISTRY_QUERY_PATH,
                axum::routing::post(
                    move |Json(request): Json<super::super::authority::RegistryQueryRequest>| {
                        let catalogue = remote_catalogue.clone();
                        async move {
                            match request.query {
                                super::super::authority::RegistryQuery::Repository { .. } => {
                                    Json(RegistryQueryResponse::Repository(Box::new(catalogue)))
                                }
                                super::super::authority::RegistryQuery::GcGeneration => {
                                    Json(RegistryQueryResponse::GcGeneration(0))
                                }
                                other => panic!("unexpected query: {other:?}"),
                            }
                        }
                    },
                ),
            )
            .route(
                REGISTRY_PROPOSAL_PATH,
                axum::routing::post(
                    move |Json(proposal): Json<super::super::authority::RegistryProposal>| {
                        assert_eq!(
                            matches!(
                                proposal.mutation,
                                super::super::authority::RegistryMutation::Copy(_)
                            ),
                            copy
                        );
                        let seen = seen.clone();
                        let release = released.clone();
                        async move {
                            seen.notify_one();
                            release.notified().await;
                            Json(crate::council::CouncilResponse::Ok)
                        }
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (_tx, rx) = tokio::sync::watch::channel(crate::mustard::directory::NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: crate::meat::NodeId::new("leader"),
                term: 1,
                api_address: address,
                reporting_address: address,
            }),
            ..Default::default()
        });
        state.forwarder = Some(RegistryForwarder::new(
            crate::cluster::ClusterHttp::plaintext().with_bearer(Some("internal".into())),
            rx,
        ));
        let publisher = state.clone();
        let caller = tokio::spawn(async move {
            if copy {
                publisher
                    .confirm_image_copy("ordinary", &manifest.digest)
                    .await
                    .map(|_| ())
            } else {
                record_commit(&publisher, manifest, "latest".into()).await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), proposed.notified())
            .await
            .unwrap();
        assert!(
            state.catalog.try_write().is_err(),
            "publication must own the local guard while authority is outstanding"
        );
        caller.abort();
        let _ = caller.await;
        assert!(
            state.catalog.try_write().is_err(),
            "a disconnected publisher must not release its pending transaction"
        );
        release.notify_one();
        let completed =
            tokio::time::timeout(std::time::Duration::from_secs(2), state.catalog.read())
                .await
                .unwrap();
        assert!(
            completed
                .get_manifest_by_tag("ordinary", "latest")
                .is_some()
        );
        assert!(
            ManifestCatalog::load_from(&directory.path().join("catalog.json"))
                .unwrap()
                .get_manifest_by_tag("ordinary", "latest")
                .is_some()
        );
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn gc_owns_the_catalogue_before_arbitration_and_through_caller_cancellation() {
        use super::super::authority::{
            REGISTRY_PROPOSAL_PATH, RegistryForwarder, RegistryMutation, RegistryProposal,
        };
        let (mut state, _directory) = test_state();
        let digest = compute_sha256(b"collectable");
        state.store.write_blob(b"collectable", &digest).unwrap();
        let proposed = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = proposed.clone();
        let released = release.clone();
        let app = Router::new().route(
            REGISTRY_PROPOSAL_PATH,
            axum::routing::post(move |Json(request): Json<RegistryProposal>| {
                let seen = seen.clone();
                let release = released.clone();
                async move {
                    let RegistryMutation::GarbageCollection(report) = request.mutation else {
                        panic!("unexpected proposal");
                    };
                    seen.notify_one();
                    release.notified().await;
                    Json(crate::council::CouncilResponse::GcApproved {
                        approved: report.deleted_layers,
                    })
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (_tx, rx) = tokio::sync::watch::channel(crate::mustard::directory::NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: crate::meat::NodeId::new("leader"),
                term: 1,
                api_address: address,
                reporting_address: address,
            }),
            ..Default::default()
        });
        state.forwarder = Some(RegistryForwarder::new(
            crate::cluster::ClusterHttp::plaintext().with_bearer(Some("internal".into())),
            rx,
        ));
        let gate = state.catalog.write().await;
        let owner = state.clone();
        let collected = digest.clone();
        let caller = tokio::spawn(async move {
            owner
                .collect_garbage(super::super::types::GcReport {
                    node_id: owner.node_raft_id,
                    deleted_layers: vec![collected],
                })
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), proposed.notified())
                .await
                .is_err(),
            "GC must not request approval while another transaction owns its catalogue"
        );
        drop(gate);
        tokio::time::timeout(std::time::Duration::from_secs(2), proposed.notified())
            .await
            .unwrap();
        assert!(state.catalog.try_write().is_err());
        caller.abort();
        let _ = caller.await;
        assert!(
            state.catalog.try_write().is_err(),
            "cancelling the caller must not release physical transaction ownership"
        );
        release.notify_one();
        let _finished =
            tokio::time::timeout(std::time::Duration::from_secs(2), state.catalog.write())
                .await
                .unwrap();
        assert!(!state.store.has_blob(&digest));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn garbage_collection_persistence_failure_retains_blobs_until_retry() {
        let (mut state, directory) = test_state();
        let blocked = directory.path().join("catalog.json");
        std::fs::create_dir(&blocked).unwrap();
        state.persist_path = Some(blocked.clone());
        let digest = compute_sha256(b"orphan");
        state.store.write_blob(b"orphan", &digest).unwrap();
        let report = super::super::types::GcReport {
            node_id: state.node_raft_id,
            deleted_layers: vec![digest.clone()],
        };
        assert!(state.collect_garbage(report.clone()).await.is_err());
        assert!(state.store.has_blob(&digest));
        std::fs::remove_dir(&blocked).unwrap();
        assert_eq!(
            state.collect_garbage(report).await.unwrap(),
            vec![digest.clone()]
        );
        assert!(!state.store.has_blob(&digest));
        assert!(
            ManifestCatalog::load_from(&blocked)
                .unwrap()
                .manifests
                .is_empty()
        );
    }

    #[tokio::test]
    async fn garbage_collection_retries_failed_deletion_after_reloading_its_decision() {
        let (mut state, directory) = test_state();
        let path = directory.path().join("catalog.json");
        state.persist_path = Some(path.clone());
        let digest = compute_sha256(b"extra");
        let blob = state.store.blob_path(&digest);
        // A directory at the blob path deterministically refuses file deletion,
        // including when privileged qualification runs as root.
        std::fs::create_dir_all(&blob).unwrap();
        state.catalog.write().await.apply_update_locations(
            &super::super::types::UpdateLayerLocations {
                updates: vec![(
                    digest.clone(),
                    std::collections::BTreeSet::from([state.node_raft_id, 99]),
                )],
            },
        );
        let report = super::super::types::GcReport {
            node_id: state.node_raft_id,
            deleted_layers: vec![digest.clone()],
        };
        assert!(
            state
                .collect_garbage(report.clone())
                .await
                .unwrap()
                .is_empty()
        );
        state.catalog = Arc::new(RwLock::new(ManifestCatalog::load_from(&path).unwrap()));
        assert_eq!(
            state.catalog.read().await.layer_holders(digest.as_str()),
            std::collections::BTreeSet::from([99])
        );
        std::fs::remove_dir(&blob).unwrap();
        state.store.write_blob(b"extra", &digest).unwrap();
        assert_eq!(
            state.collect_garbage(report).await.unwrap(),
            vec![digest.clone()]
        );
        assert!(!state.store.has_blob(&digest));
    }

    #[tokio::test]
    async fn push_rechecks_blobs_after_waiting_for_garbage_collection() {
        let (mut state, directory) = test_state();
        state.persist_path = Some(directory.path().join("catalog.json"));
        let app = test_router(state.clone());
        let config = push_blob(&app, "myapp", b"config").await;
        let body = manifest_body(&config, 6);
        let manifest = compute_sha256(&body);
        let guard = state.catalog.write().await;
        let mut gc = Box::pin(state.collect_garbage(super::super::types::GcReport {
            node_id: state.node_raft_id,
            deleted_layers: vec![config.clone()],
        }));
        assert!(futures_util::poll!(gc.as_mut()).is_pending());
        let push =
            tokio::spawn(
                async move { put_manifest(&app, "/v2/myapp/manifests/latest", body).await },
            );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !state.store.has_blob(&manifest) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // The HTTP path has validated and stored the manifest. FIFO lock
        // admission lets already-queued GC finish before its catalogue commit.
        drop(guard);
        assert_eq!(gc.await.unwrap(), vec![config]);
        assert_eq!(
            push.await.unwrap().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(
            state
                .catalog
                .read()
                .await
                .get_manifest_by_tag("myapp", "latest")
                .is_none()
        );
    }

    #[tokio::test]
    async fn stale_collection_report_preserves_a_committed_manifest_and_shared_config() {
        let (state, _directory) = test_state();
        let app = test_router(state.clone());
        let config = push_blob(&app, "ordinary", b"shared").await;
        let body = manifest_body(&config, 6);
        let manifest = compute_sha256(&body);
        assert_eq!(
            put_manifest(&app, "/v2/ordinary/manifests/latest", body)
                .await
                .status(),
            StatusCode::CREATED
        );
        let deleted = state
            .collect_garbage(super::super::types::GcReport {
                node_id: state.node_raft_id,
                deleted_layers: vec![config.clone(), manifest.clone()],
            })
            .await
            .unwrap();
        assert!(deleted.is_empty());
        assert!(state.store.has_blob(&config));
        assert!(state.store.has_blob(&manifest));
    }

    #[tokio::test]
    async fn simultaneous_pushes_leave_every_acknowledged_tag_on_disk() {
        let (mut state, directory) = test_state();
        let path = directory.path().join("catalog.json");
        state.persist_path = Some(path.clone());
        let app = test_router(state.clone());
        let config = push_blob(&app, "ordinary", b"shared").await;
        let body = manifest_body(&config, 6);
        let guard = state.catalog.write().await;
        let tasks: Vec<_> = (0..4)
            .map(|index| {
                let app = app.clone();
                let body = body.clone();
                tokio::spawn(async move {
                    put_manifest(&app, &format!("/v2/ordinary/manifests/tag-{index}"), body).await
                })
            })
            .collect();
        drop(guard);
        for task in tasks {
            assert_eq!(task.await.unwrap().status(), StatusCode::CREATED);
        }
        let reloaded = ManifestCatalog::load_from(&path).unwrap();
        for index in 0..4 {
            assert!(
                reloaded
                    .get_manifest_by_tag("ordinary", &format!("tag-{index}"))
                    .is_some()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    fn test_state() -> (PickleState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path());
        let state = PickleState {
            store: Arc::new(store),
            catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
            node_raft_id: 7,
            council: None,
            forwarder: None,
            test_leases: Default::default(),
            repository_writers: Default::default(),
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

    #[tokio::test]
    async fn unavailable_catalogue_authority_refuses_reads_and_quota_admission() {
        let (mut state, _dir) = test_state();
        let (_tx, rx) =
            tokio::sync::watch::channel(crate::mustard::directory::NodeDirectory::default());
        state.forwarder = Some(super::super::authority::RegistryForwarder::new(
            crate::cluster::ClusterHttp::plaintext(),
            rx,
        ));
        state.quota = QuotaConfig {
            per_repository_bytes: 10,
            total_bytes: 0,
        };
        assert_eq!(
            state
                .enforce_quota("ordinary", 1)
                .await
                .unwrap_err()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let app = test_router(state.clone());
        for path in ["/v2/ordinary/manifests/latest", "/v2/ordinary/tags/list"] {
            assert_eq!(
                app.clone()
                    .oneshot(axum::http::Request::get(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        assert!(state.catalog_snapshot("ordinary").await.is_err());
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
            forwarder: None,
            test_leases: Default::default(),
            repository_writers: Default::default(),
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
            forwarder: None,
            test_leases: Default::default(),
            repository_writers: Default::default(),
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
            forwarder: None,
            test_leases: Default::default(),
            repository_writers: Default::default(),
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
