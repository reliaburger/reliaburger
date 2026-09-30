//! B1 regression: a clustered registry push must authenticate with the
//! internal service token as a *bearer*.
//!
//! A real `rbrg build` on a routable cluster needs buildah and a multi-node
//! cluster with a non-loopback advertise address, which is impractical to
//! stand up here (it runs under `RELIABURGER_BUILDAH_TESTS=1` in the Lima VM).
//! What this test *can* pin down without buildah is the auth path the fix
//! turns on: against a registry with the routable-cluster policy
//! (`require_read_auth = true`, no unauthenticated bootstrap window), a
//! bearer-less push — which is all `buildah push docker://…` can send — is
//! refused, and the OCI-layout upload the runner performs instead (every blob
//! by monolithic `POST`, then the top manifest `PUT` by tag, all carrying the
//! service token as a bearer) round-trips.
//!
//! It needs no external tooling, so it runs in the portable suite.

use std::sync::Arc;

use reliaburger::pickle::build::{
    digest_of, oci_blob_upload_url, oci_manifest_put_url, parse_oci_index, upload_oci_layout,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

const SERVICE_TOKEN: &str = "rbrg_test_service_token";

/// A Pickle registry with the routable-cluster auth policy: reads and writes
/// both require a principal, and there is no open bootstrap window.
async fn start_authenticated_registry() -> (u16, CancellationToken, tempfile::TempDir) {
    use reliaburger::pickle::api::{PickleState, router as pickle_router};
    use reliaburger::pickle::store::BlobStore;
    use reliaburger::pickle::types::ManifestCatalog;
    use reliaburger::sesame::auth::{AuthState, new_token_store};

    let dir = tempfile::tempdir().unwrap();
    let state = PickleState {
        store: Arc::new(BlobStore::new(dir.path().join("blobs"))),
        catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
        node_raft_id: 1,
        council: None,
        forwarder: None,
        test_leases: Default::default(),
        repository_writers: Default::default(),
        persist_path: None,
        auth: Some(AuthState::new(
            new_token_store(),
            Some(SERVICE_TOKEN.to_string()),
        )),
        // The routable-cluster policy: a stranger who can reach the port must
        // present a principal, and writes never fall into an open bootstrap.
        require_read_auth: true,
        allow_unauthenticated_bootstrap: false,
        quota: reliaburger::pickle::registry_auth::QuotaConfig::default(),
        sessions: reliaburger::pickle::registry_auth::UploadSessions::new(
            reliaburger::pickle::registry_auth::DEFAULT_UPLOAD_TTL,
        ),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = pickle_router(state);
    let shutdown = CancellationToken::new();
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await
            .ok();
    });
    (port, shutdown, dir)
}

/// Write a minimal single-platform OCI image layout (`oci-layout`,
/// `index.json`, and `blobs/sha256/<hex>` for config, layer and manifest) into
/// `dir`, exactly the shape `buildah push oci:<dir>:<tag>` produces. Returns
/// the config and layer digests so the test can upload them.
fn write_oci_layout(dir: &std::path::Path) -> (String, String) {
    let config = br#"{"architecture":"amd64","os":"linux"}"#.to_vec();
    let layer = b"a fake layer blob".to_vec();
    let config_digest = digest_of(&config);
    let layer_digest = digest_of(&layer);

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": layer_digest,
            "size": layer.len(),
        }],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = digest_of(&manifest_bytes);

    let blobs = dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).unwrap();
    for (digest, bytes) in [
        (&config_digest, &config),
        (&layer_digest, &layer),
        (&manifest_digest, &manifest_bytes),
    ] {
        let hex = digest.strip_prefix("sha256:").unwrap();
        std::fs::write(blobs.join(hex), bytes).unwrap();
    }

    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest,
            "size": manifest_bytes.len(),
        }],
    });
    std::fs::write(dir.join("index.json"), serde_json::to_vec(&index).unwrap()).unwrap();
    std::fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();

    (config_digest, layer_digest)
}

/// A bearer-less push — everything `buildah push docker://…` can send against
/// Pickle — is refused on a routable cluster; the same requests carrying the
/// service-token bearer (what the runner's OCI-layout upload sends) succeed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_routable_push_needs_the_service_token_bearer() {
    let (port, shutdown, _dir) = start_authenticated_registry().await;
    let layout = tempfile::tempdir().unwrap();
    let (config_digest, _layer_digest) = write_oci_layout(layout.path());
    let client = reqwest::Client::new();
    let repo = "team-a/api";

    // Read the layout the way the runner does.
    let index_bytes = std::fs::read(layout.path().join("index.json")).unwrap();
    let top = parse_oci_index(&index_bytes).unwrap();

    // 1. Bearer-less: the blob upload buildah would attempt is a 401.
    let blob_url = oci_blob_upload_url("http", port, repo, &config_digest);
    let config_bytes = std::fs::read(
        layout
            .path()
            .join("blobs")
            .join("sha256")
            .join(config_digest.strip_prefix("sha256:").unwrap()),
    )
    .unwrap();
    let refused = client
        .post(&blob_url)
        .body(config_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(
        refused.status().as_u16(),
        401,
        "a bearer-less push must be refused on a routable cluster"
    );

    // 2. With the service-token bearer: the runner's own upload (every blob,
    //    then the top manifest by tag).
    let published = upload_oci_layout(
        &client,
        "http",
        port,
        repo,
        "v1",
        layout.path(),
        Some(SERVICE_TOKEN),
    )
    .await
    .unwrap();
    assert_eq!(published.top.digest, top.digest);
    assert!(published.platform_manifests.is_empty());
    let top_hex = top.digest.strip_prefix("sha256:").unwrap();
    let manifest_bytes = std::fs::read(layout.path().join("blobs/sha256").join(top_hex)).unwrap();

    // 3. The pushed image is now pullable by tag (with a principal).
    let pull = client
        .get(oci_manifest_put_url("http", port, repo, "v1"))
        .bearer_auth(SERVICE_TOKEN)
        .send()
        .await
        .unwrap();
    assert!(
        pull.status().is_success(),
        "manifest pull failed: {}",
        pull.status()
    );
    let pulled = pull.bytes().await.unwrap();
    assert_eq!(
        &pulled[..],
        &manifest_bytes[..],
        "the registry returned different manifest bytes than were pushed"
    );

    shutdown.cancel();
}

/// Write a two-platform OCI layout (`linux/amd64` and `linux/arm64`), the
/// shape `buildah manifest push --all <list> oci:<dir>:<tag>` produces: one
/// index in `index.json`, and each platform's manifest, config and layer as
/// blobs. Returns the index digest and the platform manifests' digests.
fn write_multi_platform_layout(dir: &std::path::Path) -> (String, Vec<String>) {
    let blobs = dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).unwrap();
    let write_blob = |bytes: &[u8]| {
        let digest = digest_of(bytes);
        std::fs::write(blobs.join(digest.strip_prefix("sha256:").unwrap()), bytes).unwrap();
        digest
    };

    let mut entries = Vec::new();
    let mut platform_digests = Vec::new();
    for architecture in ["amd64", "arm64"] {
        let config = format!(r#"{{"architecture":"{architecture}","os":"linux"}}"#).into_bytes();
        let layer = format!("the {architecture} layer").into_bytes();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": write_blob(&config),
                "size": config.len(),
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": write_blob(&layer),
                "size": layer.len(),
            }],
        }))
        .unwrap();
        let digest = write_blob(&manifest);
        entries.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest,
            "size": manifest.len(),
            "platform": { "os": "linux", "architecture": architecture },
        }));
        platform_digests.push(digest);
    }
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": entries,
    }))
    .unwrap();
    let index_digest = write_blob(&index);
    let top = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "digest": index_digest,
            "size": index.len(),
        }],
    });
    std::fs::write(dir.join("index.json"), serde_json::to_vec(&top).unwrap()).unwrap();
    std::fs::write(dir.join("oci-layout"), br#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
    (index_digest, platform_digests)
}

/// A multi-platform build stores every platform: each platform manifest is
/// published by digest before the index goes up under the tag, so the tag
/// resolves to the index and each platform resolves by its digest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_multi_platform_layout_publishes_every_platform() {
    let (port, shutdown, _dir) = start_authenticated_registry().await;
    let layout = tempfile::tempdir().unwrap();
    let (index_digest, platform_digests) = write_multi_platform_layout(layout.path());
    let client = reqwest::Client::new();
    let repo = "team-a/burger";

    let published = upload_oci_layout(
        &client,
        "http",
        port,
        repo,
        "v1",
        layout.path(),
        Some(SERVICE_TOKEN),
    )
    .await
    .unwrap();
    assert_eq!(published.top.digest, index_digest);
    let platforms: Vec<&str> = published
        .platform_manifests
        .iter()
        .map(|m| m.platform.as_str())
        .collect();
    assert_eq!(platforms, vec!["linux/amd64", "linux/arm64"]);
    assert_eq!(published.manifest_digests().len(), 3);

    let get = |reference: String| {
        client
            .get(oci_manifest_put_url("http", port, repo, &reference))
            .bearer_auth(SERVICE_TOKEN)
            .send()
    };
    let by_tag = get("v1".to_string()).await.unwrap();
    assert!(by_tag.status().is_success(), "tag: {}", by_tag.status());
    assert_eq!(
        by_tag.headers()["docker-content-digest"].to_str().unwrap(),
        index_digest,
        "the tag must name the index, not one platform"
    );
    for digest in &platform_digests {
        let by_digest = get(digest.clone()).await.unwrap();
        assert!(
            by_digest.status().is_success(),
            "platform manifest {digest} must be pullable by digest: {}",
            by_digest.status()
        );
    }

    shutdown.cancel();
}
