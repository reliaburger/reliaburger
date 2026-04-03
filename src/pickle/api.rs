//! OCI Distribution API handlers for Pickle.
//!
//! Implements the subset of the OCI Distribution Spec needed for
//! `docker push` and `docker pull`: blob uploads, manifest push/pull,
//! and tag listing.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
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
        headers.insert("content-length", size.to_string().parse().unwrap());
        headers.insert("docker-content-digest", digest.as_str().parse().unwrap());
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
            headers.insert("content-length", data.len().to_string().parse().unwrap());
            headers.insert("docker-content-digest", digest.as_str().parse().unwrap());
            headers.insert("content-type", "application/octet-stream".parse().unwrap());
            (StatusCode::OK, headers, data).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `POST /v2/{name}/blobs/uploads/` — initiate a blob upload.
async fn blob_upload_initiate(
    State(state): State<PickleState>,
    Path(name): Path<String>,
) -> Response {
    match state.store.initiate_upload().await {
        Ok(upload_id) => {
            let location = format!("/v2/{name}/blobs/uploads/{upload_id}");
            let mut headers = HeaderMap::new();
            headers.insert("location", location.parse().unwrap());
            headers.insert("docker-upload-uuid", upload_id.parse().unwrap());
            (StatusCode::ACCEPTED, headers).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `PATCH /v2/{name}/blobs/uploads/{upload_id}` — upload a chunk.
async fn blob_upload_patch(
    State(state): State<PickleState>,
    Path((_name, upload_id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    match state.store.write_upload_chunk(&upload_id, &body).await {
        Ok(total) => {
            let mut headers = HeaderMap::new();
            headers.insert("docker-upload-uuid", upload_id.parse().unwrap());
            headers.insert(
                "range",
                format!("0-{}", total.saturating_sub(1)).parse().unwrap(),
            );
            (StatusCode::ACCEPTED, headers).into_response()
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
    if !body.is_empty()
        && state
            .store
            .write_upload_chunk(&upload_id, &body)
            .await
            .is_err()
    {
        return StatusCode::NOT_FOUND.into_response();
    }

    match state.store.complete_upload(&upload_id, &digest).await {
        Ok(()) => {
            let mut headers = HeaderMap::new();
            headers.insert("docker-content-digest", digest.as_str().parse().unwrap());
            (StatusCode::CREATED, headers).into_response()
        }
        Err(super::types::PickleError::DigestMismatch { expected, actual }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("digest mismatch: expected {expected}, got {actual}")
            })),
        )
            .into_response(),
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
    _media_type: Option<String>,
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
}

#[derive(Debug, Deserialize)]
struct OciDescriptor {
    digest: String,
    size: u64,
    #[serde(rename = "mediaType", default)]
    media_type: Option<String>,
}

/// `PUT /v2/{name}/manifests/{reference}` — push a manifest.
async fn manifest_put(
    State(state): State<PickleState>,
    Path((name, reference)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    // Parse the manifest JSON
    let manifest_json: OciManifestJson = match serde_json::from_slice(&body) {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("invalid manifest: {e}")})),
            )
                .into_response();
        }
    };

    // Verify all referenced blobs exist locally
    let config_digest = match Digest::new(&manifest_json.config.digest) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid config digest"})),
            )
                .into_response();
        }
    };
    if !state.store.has_blob(&config_digest) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("missing config blob: {}", config_digest.as_str())})),
        )
            .into_response();
    }

    let mut layers = Vec::new();
    for layer in &manifest_json.layers {
        let layer_digest = match Digest::new(&layer.digest) {
            Ok(d) => d,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": format!("invalid layer digest: {}", layer.digest)})),
                )
                    .into_response();
            }
        };
        if !state.store.has_blob(&layer_digest) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("missing layer: {}", layer_digest.as_str())})),
            )
                .into_response();
        }
        layers.push(LayerDescriptor {
            digest: layer_digest,
            size: layer.size,
            media_type: layer.media_type.clone().unwrap_or_default(),
        });
    }

    // Compute manifest digest
    let manifest_digest = compute_sha256(&body);

    // Store the manifest body as a blob too
    let _ = state.store.write_blob(&body, &manifest_digest);

    let total_size = manifest_json.config.size + layers.iter().map(|l| l.size).sum::<u64>();
    let manifest = ImageManifest {
        digest: manifest_digest.clone(),
        config: LayerDescriptor {
            digest: config_digest,
            size: manifest_json.config.size,
            media_type: manifest_json.config.media_type.unwrap_or_default(),
        },
        layers,
        repository: name.clone(),
        tags: std::collections::BTreeSet::new(),
        total_size,
        pushed_at: std::time::SystemTime::now(),
        pushed_by: 0, // TODO(Phase 5c): set to actual node ID
    };

    // Commit to catalog
    let commit = ManifestCommit {
        manifest,
        tag: reference.clone(),
        holder_nodes: std::collections::BTreeSet::from([0]), // local node
    };
    state.catalog.write().await.apply_manifest_commit(&commit);

    let mut headers = HeaderMap::new();
    headers.insert(
        "docker-content-digest",
        manifest_digest.as_str().parse().unwrap(),
    );
    (StatusCode::CREATED, headers).into_response()
}

/// `GET /v2/{name}/manifests/{reference}` — pull a manifest.
async fn manifest_get(
    State(state): State<PickleState>,
    Path((name, reference)): Path<(String, String)>,
) -> Response {
    let catalog = state.catalog.read().await;

    // Try by tag first, then by digest
    let manifest = catalog
        .get_manifest_by_tag(&name, &reference)
        .or_else(|| catalog.get_manifest(&reference));

    match manifest {
        Some(m) => {
            // Read the manifest blob from store
            match state.store.read_blob(&m.digest) {
                Ok(data) => {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        "content-type",
                        "application/vnd.oci.image.manifest.v1+json"
                            .parse()
                            .unwrap(),
                    );
                    headers.insert("docker-content-digest", m.digest.as_str().parse().unwrap());
                    (StatusCode::OK, headers, data).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            }
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
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

        // Upload data via PATCH
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

        // Complete upload
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

    #[tokio::test]
    async fn push_manifest_with_missing_layer_returns_400() {
        let (state, _dir) = test_state();
        let app = test_router(state);

        // Push config blob only (no layer)
        let config_data = b"config";
        let config_digest = push_blob(&app, "myapp", config_data).await;

        let missing_digest =
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let manifest_json = serde_json::json!({
            "config": {
                "digest": config_digest.as_str(),
                "size": config_data.len()
            },
            "layers": [{
                "digest": missing_digest,
                "size": 100
            }]
        });

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/v2/myapp/manifests/latest")
                    .body(Body::from(serde_json::to_vec(&manifest_json).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
