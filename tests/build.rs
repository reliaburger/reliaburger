//! Integration tests for async image builds (Phase 12, F2).
//!
//! The async shape is the testable half on any platform: submit
//! returns 202 or an honest 503, status polling round-trips, unknown
//! ids 404. The full build (buildah + a real registry push) runs under
//! `RELIABURGER_BUILDAH_TESTS=1` in the Lima VM.

use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::agent::BunAgent;
use reliaburger::bun::api;
use reliaburger::grill::{PortAllocator, ProcessGrill};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The internal `/v1/build/run` endpoint requires the cluster service
/// token; the harness configures one so the delegation path can be
/// exercised as the system principal.
const TEST_SERVICE_TOKEN: &str = "rbrg_test_service_token";

struct Harness {
    base_url: String,
    shutdown: CancellationToken,
}

impl Harness {
    async fn start() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();
        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(44000, 45000);
        let agent_shutdown = shutdown.clone();
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, agent_shutdown);
        let deploy_history = agent.deploy_history_handle();
        tokio::spawn(async move {
            agent.run().await;
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router(
            cmd_tx,
            None,
            None,
            Some(deploy_history),
            None,
            None,
            None,
            None,
            Some(TEST_SERVICE_TOKEN.to_string()),
            None,
            None,
            None,
            9117,
            None,
        );
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
                .await
                .ok();
        });

        let base_url = format!("http://127.0.0.1:{port}");
        for _ in 0..20 {
            if reqwest::get(format!("{base_url}/v1/health")).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self { base_url, shutdown }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn buildah_present() -> bool {
    std::process::Command::new("buildah")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Without buildah and without peers, a submit is an honest 503 — not
/// the old 501, and not a hang.
#[tokio::test]
async fn submit_without_any_builder_is_503() {
    if buildah_present() {
        eprintln!("skipping: this host has buildah, the 503 path can't trigger");
        return;
    }
    let harness = Harness::start().await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/build", harness.base_url))
        .json(&serde_json::json!({
            "name": "app",
            "context_digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "spec": { "context": ".", "destination": "pickle://app:v1" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
}

/// JOB2 residual: the registry destination is server-owned. A request
/// that smuggles a `registry_port` (trying to point a privileged Bun at
/// an arbitrary localhost service) is rejected as a bad request, not
/// silently honoured.
#[tokio::test]
async fn build_submit_rejects_a_smuggled_registry_port() {
    let harness = Harness::start().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/build", harness.base_url))
        .json(&serde_json::json!({
            "name": "app",
            "context_digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "registry_port": 5050,
            "spec": { "context": ".", "destination": "pickle://app:v1" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
}

/// JOB2: `/v1/build/run` (the peer-delegation path) rejects a context digest
/// that isn't a well-formed OCI digest — before the buildah check and before
/// the digest is ever turned into a temp path. `sha256:../../x` must not be
/// able to escape the build sandbox.
#[tokio::test]
async fn build_run_rejects_a_traversal_context_digest() {
    let harness = Harness::start().await;

    for bad in [
        "sha256:../../etc/passwd",
        "sha256:../evil",
        "not-a-digest",
        "sha256:short",
    ] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/build/run", harness.base_url))
            .json(&serde_json::json!({
                "name": "app",
                "context_digest": bad,
                "spec": { "context": ".", "destination": "pickle://app:v1" },
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            400,
            "digest {bad:?} should be rejected with 400"
        );
    }
}

/// JOB1: `/v1/build/run` requires the system principal — a valid digest from a
/// caller without the cluster service token is refused (403) before any build
/// runs. (A malformed digest is rejected earlier with 400 by the test above.)
#[tokio::test]
async fn build_run_requires_the_system_principal() {
    let harness = Harness::start().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/build/run", harness.base_url))
        .json(&serde_json::json!({
            "name": "app",
            "context_digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "spec": { "context": ".", "destination": "pickle://app:v1" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
}

/// JOB6: a build spec whose Dockerfile path tries to escape the context
/// (`../../outside`) is rejected before Buildah runs. Driven through the
/// system-principal `/v1/build/run` path so it exercises the real
/// handler, not just the pure validator.
#[tokio::test]
async fn build_run_rejects_a_dockerfile_escape() {
    let harness = Harness::start().await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/build/run", harness.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&serde_json::json!({
            "name": "app",
            "context_digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "spec": {
                "context": ".",
                "dockerfile": "../../outside",
                "destination": "pickle://app:v1",
            },
        }))
        .send()
        .await
        .unwrap();
    // The build never starts: an escaping Dockerfile is a 503 (no
    // buildah on this host) or a rejected build, never a 202 that runs.
    assert_ne!(
        response.status().as_u16(),
        202,
        "an escaping Dockerfile must not start a build"
    );
}

/// Unknown build ids are 404, not empty objects.
#[tokio::test]
async fn unknown_build_id_is_404() {
    let harness = Harness::start().await;
    let response = reqwest::get(format!("{}/v1/build/999", harness.base_url))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
}

/// Roadmap (Phase 12): a real build — context blob in a real registry,
/// buildah bud + push, manifest lands in the catalog — through the
/// async submit/poll API. Lima only (`RELIABURGER_BUILDAH_TESTS=1`).
#[tokio::test]
async fn buildah_build_lands_in_the_catalog() {
    if std::env::var("RELIABURGER_BUILDAH_TESTS").is_err() {
        eprintln!("skipping buildah test (set RELIABURGER_BUILDAH_TESTS=1 to enable)");
        return;
    }

    // A real Pickle registry for the context blob and the pushed image.
    use reliaburger::pickle::api::{PickleState, router as pickle_router};
    use reliaburger::pickle::store::BlobStore;
    use reliaburger::pickle::types::ManifestCatalog;
    use tokio::sync::RwLock;

    let dir = tempfile::tempdir().unwrap();
    let pickle_state = PickleState {
        store: Arc::new(BlobStore::new(dir.path().join("blobs"))),
        catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
        node_raft_id: 1,
        council: None,
        persist_path: None,
    };
    let registry_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registry_port = registry_listener.local_addr().unwrap().port();
    let registry_app = pickle_router(pickle_state.clone());
    let registry_shutdown = CancellationToken::new();
    let serve_shutdown = registry_shutdown.clone();
    tokio::spawn(async move {
        axum::serve(registry_listener, registry_app)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await
            .ok();
    });

    // Tar a trivial build context and upload it as the context blob.
    let context_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        context_dir.path().join("hello.txt"),
        b"hello from the build",
    )
    .unwrap();
    std::fs::write(
        context_dir.path().join("Dockerfile"),
        b"FROM scratch\nCOPY hello.txt /hello.txt\n",
    )
    .unwrap();
    let tar_bytes = reliaburger::pickle::build::tar_context(context_dir.path()).unwrap();
    let digest = reliaburger::pickle::build::digest_of(&tar_bytes);
    let upload_url = reliaburger::pickle::build::context_upload_url(registry_port, &digest);
    let response = reqwest::Client::new()
        .post(&upload_url)
        .body(tar_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201, "context upload failed");

    // Run the build directly (the handler's spawned body).
    let registry = Arc::new(tokio::sync::Mutex::new(
        reliaburger::bun::build_runner::BuildRegistry::default(),
    ));
    let build_id = registry
        .lock()
        .await
        .register(reliaburger::bun::build_runner::BuildState::Running);
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
    // No agent: drain (and fail) any signing request — standalone
    // builds are unsigned by design.
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            if let reliaburger::bun::agent::AgentCommand::SignImage { response, .. } = cmd {
                let _ = response.send(Err(reliaburger::bun::BunError::FaultRejected {
                    reason: "no council in this test".to_string(),
                }));
            }
        }
    });

    let request = reliaburger::bun::build_runner::BuildSubmitRequest {
        name: "hello".to_string(),
        context_digest: digest.clone(),
        spec: toml::from_str(&format!(
            r#"
            context = "{}"
            destination = "pickle://hello:v1"
            "#,
            context_dir.path().display()
        ))
        .unwrap(),
    };
    reliaburger::bun::build_runner::run_build(
        request,
        build_id,
        Arc::clone(&registry),
        cmd_tx,
        Some(Arc::clone(&pickle_state.catalog)),
        Duration::from_secs(600),
        registry_port,
        256 * 1024 * 1024,
    )
    .await;

    // The build completed and the image is in the catalog.
    let state = registry.lock().await.get(build_id).unwrap();
    assert!(
        matches!(
            &state,
            reliaburger::bun::build_runner::BuildState::Completed { .. }
        ),
        "build did not complete: {state:?}"
    );
    let catalog = pickle_state.catalog.read().await;
    assert!(
        catalog.get_manifest_by_tag("hello", "v1").is_some(),
        "built image missing from the catalog"
    );

    registry_shutdown.cancel();
}
