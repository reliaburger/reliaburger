//! OCI Distribution API handlers for Pickle.
//!
//! Implements the subset of the OCI Distribution Spec needed for
//! `docker push` and `docker pull`: blob uploads, manifest push/pull,
//! and tag listing.

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, head, patch, post, put};
use serde::Deserialize;
use tokio::sync::RwLock;

use super::store::{BlobStore, compute_sha256};
use super::types::{Digest, ImageManifest, LayerDescriptor, ManifestCatalog, ManifestCommit};

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

/// Record a committed manifest: apply to the local catalog, persist it
/// to disk, and — when this node is a council member — propose it to
/// Raft so the cluster-wide catalog tracks the real holder.
///
/// The Raft propose is best-effort: workers outside the council can't
/// write to Raft yet (proposal forwarding arrives with the scheduler
/// wiring), and a failed propose must not fail the push — the local
/// catalog is already persisted and the replication loop reconciles
/// holder sets from it later.
pub(crate) async fn record_commit(state: &PickleState, manifest: ImageManifest, tag: String) {
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

    if let Some(council) = &state.council
        && let Err(e) = council
            .write(crate::council::types::RaftRequest::ManifestCommit(commit))
            .await
    {
        eprintln!(
            "pickle: manifest commit not written to raft ({e}); \
             local catalog persisted, replication will reconcile"
        );
    }
}

/// Build the OCI Distribution API router.
pub fn router(state: PickleState) -> Router {
    Router::new()
        .route("/v2/", get(v2_check))
        .route("/v2/{name}/blobs/{digest}", head(blob_head).get(blob_get))
        .route("/v2/{name}/blobs/uploads/", post(blob_upload_initiate))
        .route(
            "/v2/{name}/blobs/uploads/{upload_id}",
            patch(blob_upload_patch).put(blob_upload_complete),
        )
        .route(
            "/v2/{name}/manifests/{reference}",
            put(manifest_put).get(manifest_get),
        )
        .route("/v2/{name}/tags/list", get(tags_list))
        .layer(DefaultBodyLimit::max(512 * 1024 * 1024)) // 512 MB for large layers
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Version check
// ---------------------------------------------------------------------------

/// `GET /v2/` — OCI version check. Returns 200 OK.
async fn v2_check() -> impl IntoResponse {
    Json(serde_json::json!({}))
}

// ---------------------------------------------------------------------------
// Blob operations
// ---------------------------------------------------------------------------

/// `HEAD /v2/{name}/blobs/{digest}` — check if a blob exists.
async fn blob_head(
    State(state): State<PickleState>,
    Path((_name, digest_str)): Path<(String, String)>,
) -> Response {
    let Ok(digest) = Digest::new(&digest_str) else {
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
async fn blob_get(
    State(state): State<PickleState>,
    Path((_name, digest_str)): Path<(String, String)>,
) -> Response {
    let Ok(digest) = Digest::new(&digest_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    match state.store.read_blob(&digest) {
        Ok(data) => {
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
        Err(_) => StatusCode::NOT_FOUND.into_response(),
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
    State(state): State<PickleState>,
    Path(name): Path<String>,
    Query(query): Query<InitiateUploadQuery>,
    body: axum::body::Bytes,
) -> Response {
    // Monolithic upload: body + digest in one POST
    if let Some(ref digest_str) = query.digest
        && !body.is_empty()
    {
        let Ok(digest) = Digest::new(digest_str) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        match state.store.write_blob(&body, &digest) {
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

    // Chunked upload: start a session
    match state.store.initiate_upload().await {
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
    State(state): State<PickleState>,
    Path((name, upload_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    match state.store.write_upload_chunk(&upload_id, &body).await {
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

#[derive(Deserialize)]
struct CompleteUploadQuery {
    digest: String,
}

/// `PUT /v2/{name}/blobs/uploads/{upload_id}?digest=` — complete upload.
async fn blob_upload_complete(
    State(state): State<PickleState>,
    Path((_name, upload_id)): Path<(String, String)>,
    Query(query): Query<CompleteUploadQuery>,
    body: axum::body::Bytes,
) -> Response {
    let Ok(digest) = Digest::new(&query.digest) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid digest"})),
        )
            .into_response();
    };

    // If there's a body with the PUT, write it first
    if !body.is_empty() {
        match state.store.write_upload_chunk(&upload_id, &body).await {
            Ok(_) => {}
            Err(super::types::PickleError::InvalidUploadId(_)) => {
                return StatusCode::BAD_REQUEST.into_response();
            }
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        }
    }

    match state.store.complete_upload(&upload_id, &digest).await {
        Ok(()) => {
            let mut headers = HeaderMap::new();
            // Location of the created blob (OCI distribution spec).
            headers.insert(
                "location",
                format!("/v2/{_name}/blobs/{}", digest.as_str())
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
async fn manifest_put(
    State(state): State<PickleState>,
    Path((name, reference)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let manifest_digest = compute_sha256(&body);

    // A digest in the reference position (docker pushes sub-manifests
    // as `PUT …/manifests/sha256:…`) must name the bytes it carries.
    if let Ok(reference_digest) = Digest::new(&reference)
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
            repository: name.clone(),
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
            repository: name.clone(),
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

    // Validation passed: store the exact bytes (content addressing
    // must see what the client sent, not a re-serialisation) and
    // commit.
    if let Err(e) = state.store.write_blob(&body, &manifest_digest) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("failed to store manifest blob: {e}")})),
        )
            .into_response();
    }
    record_commit(&state, manifest, reference.clone()).await;

    let mut headers = HeaderMap::new();
    headers.insert(
        "docker-content-digest",
        manifest_digest
            .as_str()
            .parse()
            .expect("ASCII header value"),
    );
    (StatusCode::CREATED, headers).into_response()
}

/// `GET /v2/{name}/manifests/{reference}` — pull a manifest.
///
/// `reference` can be a tag (e.g. `latest`) or a digest (e.g. `sha256:abc...`).
/// Docker pulls sub-manifests by digest when resolving manifest lists.
async fn manifest_get(
    State(state): State<PickleState>,
    Path((_name, reference)): Path<(String, String)>,
) -> Response {
    // If the reference looks like a digest, try reading it directly
    // from the blob store (Docker pulls sub-manifests by digest).
    if let Ok(digest) = Digest::new(&reference)
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

    // Try the catalog by tag
    let catalog = state.catalog.read().await;
    let manifest = catalog.get_manifest_by_tag(&_name, &reference);

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
async fn tags_list(
    State(state): State<PickleState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let catalog = state.catalog.read().await;
    let tags = catalog.tags_for_repository(&name);
    Json(serde_json::json!({
        "name": name,
        "tags": tags,
    }))
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
        };
        (state, dir)
    }

    fn test_router(state: PickleState) -> Router {
        router(state)
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
}
