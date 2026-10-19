//! Building and pushing a tiny synthetic OCI image, for the image-registry
//! cases.
//!
//! The harness needs no image builder: it constructs an image itself — a config blob, one gzipped tar layer,
//! and a manifest tying them together — and speaks the raw `/v2` protocol to
//! push it. The construction is pure and unit-tested here; the push runs
//! against a live registry and is covered by the registry-upload integration test.

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tar::{Builder, Header};

/// A synthetic image: the three blobs a registry needs, each with its digest.
pub struct SyntheticImage {
    pub config: Vec<u8>,
    pub config_digest: String,
    pub layer: Vec<u8>,
    pub layer_digest: String,
    pub manifest: Vec<u8>,
    pub manifest_digest: String,
}

/// `sha256:<hex>` for `data`, the digest form the registry keys blobs by.
pub fn sha256_digest(data: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(data))
}

/// One gzipped tar layer carrying a single marker file.
fn build_layer() -> Vec<u8> {
    let content = b"reliaburger testkit fixture\n";
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut tar = Builder::new(&mut encoder);
        let mut header = Header::new_gnu();
        header.set_path("marker.txt").expect("static path is valid");
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append(&header, &content[..])
            .expect("append to in-memory tar");
        tar.finish().expect("finish in-memory tar");
    }
    encoder.finish().expect("finish gzip")
}

/// Construct a fresh synthetic image. `salt` varies the configuration so two
/// calls produce distinct digests when a test needs uniqueness.
pub fn build_synthetic_image(salt: &str) -> SyntheticImage {
    // Vary the config's created field via the salt so digests differ per call.
    let config = serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {},
        "rootfs": { "type": "layers", "diff_ids": [] },
        "comment": format!("reliaburger-testkit-{salt}"),
    });
    let config = serde_json::to_vec(&config).expect("config serialises");
    let config_digest = sha256_digest(&config);

    let layer = build_layer();
    let layer_digest = sha256_digest(&layer);

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "digest": config_digest,
            "size": config.len(),
            "mediaType": "application/vnd.oci.image.config.v1+json",
        },
        "layers": [{
            "digest": layer_digest,
            "size": layer.len(),
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
        }],
    });
    let manifest = serde_json::to_vec(&manifest).expect("manifest serialises");
    let manifest_digest = sha256_digest(&manifest);

    SyntheticImage {
        config,
        config_digest,
        layer,
        layer_digest,
        manifest,
        manifest_digest,
    }
}

/// Resolve a `Location` header (which may be a path or a full URL) against the
/// registry base.
fn resolve_location(base: &str, location: &str) -> Result<String, String> {
    let base =
        url::Url::parse(base).map_err(|error| format!("invalid registry origin: {error}"))?;
    let resolved = base
        .join(location)
        .map_err(|error| format!("invalid upload location: {error}"))?;
    if resolved.origin() != base.origin()
        || !resolved.username().is_empty()
        || resolved.password().is_some()
        || resolved.fragment().is_some()
    {
        return Err("upload location must remain within the declared registry origin".into());
    }
    Ok(resolved.into())
}

/// How long a registry write keeps asking again after a 503, and how long it
/// waits between attempts.
///
/// Pickle answers a write with 503 when it can't confirm the write with the
/// council leader right now: no leader is known, or the leader couldn't
/// confirm its own leadership in time. Nothing was accepted, so the write can
/// simply be made again. The case deadline still bounds the whole wait.
#[derive(Debug, Clone, Copy)]
struct RegistryRetry {
    window: std::time::Duration,
    pause: std::time::Duration,
}

impl RegistryRetry {
    /// Long enough to ride out a leader change or a leader briefly missing its
    /// heartbeat round, the same window lease creation uses.
    const DEFAULT: RegistryRetry = RegistryRetry {
        window: std::time::Duration::from_secs(30),
        pause: std::time::Duration::from_millis(500),
    };
}

/// Why one registry write attempt failed.
#[derive(Debug)]
enum WriteFailure {
    /// The registry answered 503: it can't confirm writes right now.
    Unavailable(String),
    /// Anything else, which asking again won't change.
    Refused(String),
}

impl WriteFailure {
    /// Classify a non-success `status`, described by `message`.
    fn status(status: reqwest::StatusCode, message: String) -> Self {
        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            WriteFailure::Unavailable(message)
        } else {
            WriteFailure::Refused(message)
        }
    }
}

impl From<String> for WriteFailure {
    fn from(message: String) -> Self {
        WriteFailure::Refused(message)
    }
}

/// Run one registry write, making it again from the start after a 503 until
/// `retry.window` has passed. Any other failure ends it at once.
async fn retry_unavailable<F, Fut>(retry: RegistryRetry, mut write: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), WriteFailure>>,
{
    let give_up = std::time::Instant::now() + retry.window;
    loop {
        match write().await {
            Ok(()) => return Ok(()),
            Err(WriteFailure::Unavailable(_)) if std::time::Instant::now() < give_up => {
                tokio::time::sleep(retry.pause).await;
            }
            Err(WriteFailure::Unavailable(message) | WriteFailure::Refused(message)) => {
                return Err(message);
            }
        }
    }
}

/// Push one blob through the `/v2` monolithic-upload dance: POST to start,
/// PATCH the bytes, PUT with the digest to finish. A 503 at any step starts
/// the dance again; the registry's reaper retires the abandoned session.
async fn push_blob(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    data: &[u8],
    digest: &str,
    lease_id: Option<&str>,
    retry: RegistryRetry,
) -> Result<(), String> {
    retry_unavailable(retry, || {
        push_blob_once(http, base, repo, data, digest, lease_id)
    })
    .await
}

async fn push_blob_once(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    data: &[u8],
    digest: &str,
    lease_id: Option<&str>,
) -> Result<(), WriteFailure> {
    let start = with_lease(
        http.post(format!("{base}/v2/{repo}/blobs/uploads/")),
        lease_id,
    )
    .send()
    .await
    .map_err(|e| format!("blob upload POST failed: {e}"))?;
    if !start.status().is_success() {
        return Err(WriteFailure::status(
            start.status(),
            format!("blob upload POST returned {}", start.status()),
        ));
    }
    let location = start
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|l| resolve_location(base, l))
        .ok_or_else(|| "upload POST returned no Location".to_string())??;

    let patched = with_lease(http.patch(&location), lease_id)
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| format!("blob PATCH failed: {e}"))?;
    if !patched.status().is_success() {
        return Err(WriteFailure::status(
            patched.status(),
            format!("blob PATCH returned {}", patched.status()),
        ));
    }
    let location = patched
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(|l| resolve_location(base, l))
        .transpose()?
        .unwrap_or(location);

    let separator = if location.contains('?') { '&' } else { '?' };
    let finish = with_lease(
        http.put(format!("{location}{separator}digest={digest}")),
        lease_id,
    )
    .send()
    .await
    .map_err(|e| format!("blob PUT failed: {e}"))?;
    if !finish.status().is_success() {
        return Err(WriteFailure::status(
            finish.status(),
            format!("blob PUT returned {}", finish.status()),
        ));
    }
    Ok(())
}

/// Push a synthetic image to `base` (e.g. `http://127.0.0.1:5050`) as
/// `repo:tag`: both blobs, then the manifest.
pub async fn push_image(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    tag: &str,
    image: &SyntheticImage,
) -> Result<(), String> {
    push_image_retrying(http, base, repo, tag, image, None, RegistryRetry::DEFAULT).await
}

/// Push a fixture under its server-issued repository lease on every write.
pub async fn push_leased_image(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    tag: &str,
    image: &SyntheticImage,
    lease_id: &str,
) -> Result<(), String> {
    push_image_retrying(
        http,
        base,
        repo,
        tag,
        image,
        Some(lease_id),
        RegistryRetry::DEFAULT,
    )
    .await
}

async fn push_image_retrying(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    tag: &str,
    image: &SyntheticImage,
    lease_id: Option<&str>,
    retry: RegistryRetry,
) -> Result<(), String> {
    push_blob(
        http,
        base,
        repo,
        &image.config,
        &image.config_digest,
        lease_id,
        retry,
    )
    .await?;
    push_blob(
        http,
        base,
        repo,
        &image.layer,
        &image.layer_digest,
        lease_id,
        retry,
    )
    .await?;
    push_manifest(http, base, repo, tag, &image.manifest, lease_id, retry).await
}

fn with_lease(request: reqwest::RequestBuilder, lease_id: Option<&str>) -> reqwest::RequestBuilder {
    match lease_id {
        Some(id) => request.header("x-reliaburger-test-lease", id),
        None => request,
    }
}

/// Publish a manifest under `tag`, making the same PUT again after a 503.
async fn push_manifest(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    tag: &str,
    manifest: &[u8],
    lease_id: Option<&str>,
    retry: RegistryRetry,
) -> Result<(), String> {
    retry_unavailable(retry, || {
        push_manifest_once(http, base, repo, tag, manifest, lease_id)
    })
    .await
}

async fn push_manifest_once(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    tag: &str,
    manifest: &[u8],
    lease_id: Option<&str>,
) -> Result<(), WriteFailure> {
    let response = with_lease(
        http.put(format!("{base}/v2/{repo}/manifests/{tag}")),
        lease_id,
    )
    .header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
    .body(manifest.to_vec())
    .send()
    .await
    .map_err(|error| format!("manifest PUT failed: {error}"))?;
    if response.status() != reqwest::StatusCode::CREATED {
        return Err(WriteFailure::status(
            response.status(),
            format!(
                "manifest PUT did not confirm publication: {}",
                response.status()
            ),
        ));
    }
    Ok(())
}

/// Stage a pinned, platform-resolved upstream fixture in a leased repository.
/// Each blob is size/digest checked before upload; the returned digest names the
/// exact verified child manifest. Fixture content is limited to 64 MiB total.
pub async fn stage_upstream_image(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    lease_id: &str,
    upstream: &dyn crate::pickle::upstream::UpstreamRegistry,
    image: &crate::grill::image::ImageReference,
) -> Result<String, String> {
    crate::pickle::types::Digest::new(&image.tag)
        .map_err(|_| "runnable fixture requires a valid pinned upstream digest".to_owned())?;
    let manifest = upstream
        .fetch_manifest(image)
        .await
        .map_err(|error| error.to_string())?;
    let mut total = manifest.manifest_bytes.len() as u64;
    total = total
        .checked_add(manifest.config.size)
        .ok_or("fixture size overflow")?;
    for layer in &manifest.layers {
        total = total
            .checked_add(layer.size)
            .ok_or("fixture size overflow")?;
    }
    if total > 64 * 1024 * 1024 || manifest.layers.len() > 128 {
        return Err("runnable fixture exceeds its size limit".into());
    }
    if sha256_digest(&manifest.manifest_bytes) != manifest.digest.as_str() {
        return Err("upstream fixture manifest digest mismatch".into());
    }
    verify_fixture_blob(&manifest.config_bytes, &manifest.config)?;
    push_blob(
        http,
        base,
        repo,
        &manifest.config_bytes,
        manifest.config.digest.as_str(),
        Some(lease_id),
        RegistryRetry::DEFAULT,
    )
    .await?;
    for layer in &manifest.layers {
        let bytes = upstream
            .fetch_blob(image, layer)
            .await
            .map_err(|error| error.to_string())?;
        verify_fixture_blob(&bytes, layer)?;
        push_blob(
            http,
            base,
            repo,
            &bytes,
            layer.digest.as_str(),
            Some(lease_id),
            RegistryRetry::DEFAULT,
        )
        .await?;
    }
    push_manifest(
        http,
        base,
        repo,
        "runnable",
        &manifest.manifest_bytes,
        Some(lease_id),
        RegistryRetry::DEFAULT,
    )
    .await?;
    Ok(manifest.digest.as_str().to_owned())
}

fn verify_fixture_blob(
    bytes: &[u8],
    descriptor: &crate::pickle::types::LayerDescriptor,
) -> Result<(), String> {
    if bytes.len() as u64 != descriptor.size || sha256_digest(bytes) != descriptor.digest.as_str() {
        return Err(format!(
            "upstream fixture blob fails size/digest verification: {}",
            descriptor.digest.as_str()
        ));
    }
    Ok(())
}

/// Fetch a manifest's raw bytes back, for a round-trip digest check.
pub async fn fetch_manifest(
    http: &reqwest::Client,
    base: &str,
    repo: &str,
    reference: &str,
) -> Result<Vec<u8>, String> {
    let response = http
        .get(format!("{base}/v2/{repo}/manifests/{reference}"))
        .header("Accept", "application/vnd.oci.image.manifest.v1+json")
        .send()
        .await
        .map_err(|e| format!("manifest GET failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("manifest GET returned {}", response.status()));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("reading manifest body failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn staging_rejects_invalid_or_oversized_metadata_before_uploading_or_fetching_layers() {
        use crate::pickle::{
            types::{Digest as ImageDigest, LayerDescriptor, PickleError},
            upstream::{UpstreamFuture, UpstreamManifest, UpstreamRegistry},
        };
        struct Source(UpstreamManifest);
        impl UpstreamRegistry for Source {
            fn head_manifest_digest<'a>(
                &'a self,
                _: &'a crate::grill::image::ImageReference,
            ) -> UpstreamFuture<'a, ImageDigest> {
                Box::pin(async { panic!("pinned staging must not resolve a mutable tag") })
            }
            fn fetch_manifest<'a>(
                &'a self,
                _: &'a crate::grill::image::ImageReference,
            ) -> UpstreamFuture<'a, UpstreamManifest> {
                Box::pin(async { Ok(self.0.clone()) })
            }
            fn fetch_root<'a>(
                &'a self,
                _: &'a crate::grill::image::ImageReference,
            ) -> UpstreamFuture<'a, crate::pickle::upstream::UpstreamRoot> {
                Box::pin(async { panic!("staging reads the platform manifest, not the root") })
            }
            fn fetch_blob<'a>(
                &'a self,
                _: &'a crate::grill::image::ImageReference,
                _: &'a LayerDescriptor,
            ) -> UpstreamFuture<'a, Vec<u8>> {
                Box::pin(async {
                    Err(PickleError::ReplicationFailed(
                        "layer read must not happen for refused fixture metadata".into(),
                    ))
                })
            }
        }
        let fixture = build_synthetic_image("bounds");
        let valid = UpstreamManifest {
            digest: ImageDigest::new(&fixture.manifest_digest).unwrap(),
            manifest_bytes: fixture.manifest,
            config: LayerDescriptor {
                digest: ImageDigest::new(&fixture.config_digest).unwrap(),
                size: fixture.config.len() as u64,
                media_type: String::new(),
                platform: None,
            },
            config_bytes: fixture.config,
            layers: vec![LayerDescriptor {
                digest: ImageDigest::new(&fixture.layer_digest).unwrap(),
                size: fixture.layer.len() as u64,
                media_type: String::new(),
                platform: None,
            }],
        };
        let image = crate::grill::image::ImageReference::parse(&format!(
            "example.com/fixture@{}",
            fixture.manifest_digest
        ))
        .unwrap();
        let http = reqwest::Client::new();
        for mode in ["manifest", "configuration", "oversized", "overflow"] {
            let mut manifest = valid.clone();
            let expected = match mode {
                "manifest" => {
                    manifest.manifest_bytes.push(b' ');
                    "manifest digest mismatch"
                }
                "configuration" => {
                    manifest.config_bytes.push(b' ');
                    "size/digest verification"
                }
                "oversized" => {
                    manifest.layers[0].size = 65 * 1024 * 1024;
                    "size limit"
                }
                _ => {
                    manifest.layers[0].size = u64::MAX;
                    "size overflow"
                }
            };
            let error = stage_upstream_image(
                &http,
                "http://127.0.0.1:9",
                "rbtest-fixture/image",
                "fixture",
                &Source(manifest),
                &image,
            )
            .await
            .unwrap_err();
            assert!(error.contains(expected), "{mode}: {error}");
        }
    }

    #[tokio::test]
    async fn upload_locations_cannot_forward_credentials_to_another_origin() {
        use axum::{
            Router,
            http::{HeaderMap, StatusCode, header::LOCATION},
            routing::{patch, post},
        };
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let observed = captured.clone();
        let attacker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let attacker_address = attacker.local_addr().unwrap();
        let attack_router = Router::new().route(
            "/steal",
            patch(move |headers: HeaderMap| {
                let observed = observed.clone();
                async move {
                    *observed.lock().await = headers.get("authorization").cloned();
                    StatusCode::BAD_REQUEST
                }
            }),
        );
        let attack_task =
            tokio::spawn(async move { axum::serve(attacker, attack_router).await.unwrap() });
        let registry = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let registry_address = registry.local_addr().unwrap();
        let router = Router::new().route(
            "/v2/test/blobs/uploads/",
            post(move || async move {
                (
                    StatusCode::ACCEPTED,
                    [(LOCATION, format!("http://{attacker_address}/steal"))],
                )
            }),
        );
        let registry_task =
            tokio::spawn(async move { axum::serve(registry, router).await.unwrap() });
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            "Bearer private-registry-token".parse().unwrap(),
        );
        let http = reqwest::Client::builder()
            .no_proxy()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let result = push_image(
            &http,
            &format!("http://{registry_address}"),
            "test",
            "v1",
            &build_synthetic_image("scope"),
        )
        .await;
        let leaked = captured.lock().await.is_some();
        attack_task.abort();
        registry_task.abort();
        assert!(
            !leaked,
            "upload Location forwarded the registry bearer to another origin"
        );
        assert!(result.unwrap_err().contains("origin"));
    }

    /// A fake registry that refuses its first `refusals` upload starts and
    /// manifest writes with `status`, then accepts, counting each request.
    async fn refusing_registry(
        refusals: usize,
        status: axum::http::StatusCode,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Router,
            http::{StatusCode, header::LOCATION},
            response::IntoResponse,
            routing::{patch, post, put},
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        let starts = std::sync::Arc::new(AtomicUsize::new(0));
        let manifests = std::sync::Arc::new(AtomicUsize::new(0));
        let (start_count, manifest_count) = (starts.clone(), manifests.clone());
        let router = Router::new()
            .route(
                "/v2/test/blobs/uploads/",
                post(move || {
                    let seen = start_count.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if seen < refusals {
                            return status.into_response();
                        }
                        (
                            StatusCode::ACCEPTED,
                            [(LOCATION, "/v2/test/blobs/uploads/session")],
                        )
                            .into_response()
                    }
                }),
            )
            .route(
                "/v2/test/blobs/uploads/session",
                patch(|| async { StatusCode::ACCEPTED }).put(|| async { StatusCode::CREATED }),
            )
            .route(
                "/v2/test/manifests/v1",
                put(move || {
                    let seen = manifest_count.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if seen < refusals {
                            status
                        } else {
                            StatusCode::CREATED
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, starts, manifests, task)
    }

    const QUICK_RETRY: RegistryRetry = RegistryRetry {
        window: std::time::Duration::from_secs(5),
        pause: std::time::Duration::from_millis(10),
    };

    #[tokio::test]
    async fn a_registry_write_refused_as_unavailable_is_pushed_again() {
        // The 0.2.0 soak's pulse failed on one 503 from a blob upload start
        // while the council leader briefly couldn't confirm its leadership.
        let (base, starts, manifests, task) =
            refusing_registry(2, axum::http::StatusCode::SERVICE_UNAVAILABLE).await;
        let result = push_image_retrying(
            &reqwest::Client::new(),
            &base,
            "test",
            "v1",
            &build_synthetic_image("unavailable"),
            None,
            QUICK_RETRY,
        )
        .await;
        task.abort();
        assert_eq!(result, Ok(()));
        // Two refused starts, then one for each blob.
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 4);
        assert_eq!(manifests.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn other_registry_refusals_fail_the_push_at_once() {
        for status in [
            axum::http::StatusCode::FORBIDDEN,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
        ] {
            let (base, starts, _, task) = refusing_registry(1, status).await;
            let error = push_image_retrying(
                &reqwest::Client::new(),
                &base,
                "test",
                "v1",
                &build_synthetic_image("refused"),
                None,
                QUICK_RETRY,
            )
            .await
            .unwrap_err();
            task.abort();
            assert!(error.contains(status.as_str()), "{status}: {error}");
            assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn a_registry_that_stays_unavailable_fails_after_the_retry_window() {
        let (base, starts, _, task) =
            refusing_registry(usize::MAX, axum::http::StatusCode::SERVICE_UNAVAILABLE).await;
        let started = std::time::Instant::now();
        let error = push_image_retrying(
            &reqwest::Client::new(),
            &base,
            "test",
            "v1",
            &build_synthetic_image("lasting"),
            None,
            RegistryRetry {
                window: std::time::Duration::from_millis(300),
                pause: std::time::Duration::from_millis(20),
            },
        )
        .await
        .unwrap_err();
        task.abort();
        assert!(error.contains("503"), "{error}");
        assert!(starts.load(std::sync::atomic::Ordering::SeqCst) > 1);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_synthetic_image_has_well_formed_digests() {
        let image = build_synthetic_image("a");
        for digest in [
            &image.config_digest,
            &image.layer_digest,
            &image.manifest_digest,
        ] {
            assert!(digest.starts_with("sha256:"), "{digest}");
            assert_eq!(digest.len(), "sha256:".len() + 64, "{digest}");
        }
        // The digest of the bytes matches the recorded digest.
        assert_eq!(sha256_digest(&image.config), image.config_digest);
        assert_eq!(sha256_digest(&image.layer), image.layer_digest);
        assert_eq!(sha256_digest(&image.manifest), image.manifest_digest);
    }

    #[test]
    fn the_manifest_references_both_blobs() {
        let image = build_synthetic_image("b");
        let manifest: serde_json::Value = serde_json::from_slice(&image.manifest).unwrap();
        assert_eq!(
            manifest["config"]["digest"].as_str(),
            Some(image.config_digest.as_str())
        );
        assert_eq!(
            manifest["layers"][0]["digest"].as_str(),
            Some(image.layer_digest.as_str())
        );
        assert_eq!(
            manifest["config"]["size"].as_u64(),
            Some(image.config.len() as u64)
        );
    }

    #[test]
    fn the_salt_changes_the_digest() {
        assert_ne!(
            build_synthetic_image("one").config_digest,
            build_synthetic_image("two").config_digest
        );
    }

    #[test]
    fn upload_locations_reject_protocol_relative_origins_downgrades_and_credentials() {
        for location in [
            "//other.example/upload",
            "http://registry.example/upload",
            "https://registry.example:444/upload",
            "https://user:secret@registry.example/upload",
            "/upload#fragment",
        ] {
            assert!(
                resolve_location("https://registry.example", location).is_err(),
                "accepted {location}"
            );
        }
        assert_eq!(
            resolve_location("https://registry.example", "upload?session=one").unwrap(),
            "https://registry.example/upload?session=one"
        );
    }

    #[test]
    fn a_relative_location_resolves_against_the_base() {
        assert_eq!(
            resolve_location("http://127.0.0.1:5050", "/v2/x/blobs/uploads/1").unwrap(),
            "http://127.0.0.1:5050/v2/x/blobs/uploads/1"
        );
        assert_eq!(
            resolve_location("http://127.0.0.1:5050", "http://127.0.0.1:5050/abs").unwrap(),
            "http://127.0.0.1:5050/abs"
        );
    }
}
