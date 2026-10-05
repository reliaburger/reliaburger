//! Integration tests for async image builds (Phase 12, F2; durability
//! and delegation hardened in 12b.2).
//!
//! The async shape is the testable half on any platform: submit
//! returns 202 or an honest 503, status polling round-trips, unknown
//! ids 404, delegation transfers the context and retries across
//! builders, and Raft-tracked build records survive an API restart.
//! The full build (buildah + a real registry push) runs under
//! `RELIABURGER_BUILDAH_TESTS=1` in the Lima VM.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::bun::agent::BunAgent;
use reliaburger::bun::api;
use reliaburger::bun::api::NodeMembershipInfo;
use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
use reliaburger::grill::{PortAllocator, ProcessGrill};
use tokio::sync::{RwLock, mpsc, watch};
use tokio_util::sync::CancellationToken;

#[path = "support/task_harness.rs"]
mod task_harness;
use task_harness::TestTasks;

/// The internal `/v1/build/run` and `/v1/build/track` endpoints require
/// the cluster service token; the harness configures one so those
/// paths can be exercised as the system principal.
const TEST_SERVICE_TOKEN: &str = "rbrg_test_service_token";

/// Optional pieces for the harness: a council, a membership table, a
/// builder-capability view and the local registry port.
#[derive(Default)]
struct HarnessOptions {
    council: Option<Arc<CouncilNode>>,
    node_name: Option<String>,
    membership: Option<Vec<NodeMembershipInfo>>,
    builder_nodes: Vec<String>,
    registry_port: u16,
    pickle_catalog: Option<Arc<RwLock<reliaburger::pickle::types::ManifestCatalog>>>,
    require_signatures: bool,
    /// Build settings; the gated Buildah tests give each harness its own
    /// Buildah storage so they can inspect what a build left behind.
    build_settings: Option<reliaburger::bun::build_runner::BuildSettings>,
}

struct Harness {
    base_url: String,
    _tasks: TestTasks,
}

impl Harness {
    async fn start(options: HarnessOptions) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let shutdown = CancellationToken::new();
        let grill = ProcessGrill::new();
        let port_allocator = PortAllocator::new(44000, 45000);
        let agent_shutdown = shutdown.clone();
        let mut agent = BunAgent::new(grill, port_allocator, cmd_rx, agent_shutdown);
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        let deploy_history = agent.deploy_history_handle();
        let agent_task = tokio::spawn(async move {
            agent.run().await;
            drop(agent);
            drop(volumes);
        });

        // The capability view peers report through the state pipeline.
        let aggregated = {
            let mut state = reliaburger::reporting::aggregator::AggregatedState::default();
            for name in &options.builder_nodes {
                let node = reliaburger::meat::NodeId(name.clone());
                state.reports.insert(
                    node.clone(),
                    reliaburger::reporting::types::StateReport {
                        node_id: node,
                        timestamp: std::time::SystemTime::UNIX_EPOCH,
                        running_apps: vec![],
                        cached_specs: vec![],
                        resource_usage: Default::default(),
                        event_log: vec![],
                        has_buildah: true,
                    },
                );
            }
            state
        };
        // A dropped sender is fine: `borrow()` keeps serving the last value.
        let (_aggregated_tx, aggregated_rx) = watch::channel(aggregated);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = api::router_with_upgrade(
            cmd_tx,
            None,
            None,
            Some(deploy_history),
            options.pickle_catalog,
            None,
            options.council,
            None,
            Some(TEST_SERVICE_TOKEN.to_string()),
            None,
            options
                .membership
                .map(|members| Arc::new(RwLock::new(members))),
            None,
            None,
            9117,
            None,
            None,
            Some(aggregated_rx),
            "default".to_string(),
            options.node_name,
            options.build_settings.unwrap_or_else(|| {
                reliaburger::bun::build_runner::BuildSettings::with_timeout(600)
            }),
            reliaburger::cluster::ClusterHttp::plaintext(),
            if options.registry_port == 0 {
                5050
            } else {
                options.registry_port
            },
            "http",
            256 * 1024 * 1024,
            options.require_signatures,
            reliaburger::bun::capabilities::StaticCapabilities::default(),
            reliaburger::bun::readiness::ReadinessTracker::new(),
            None,
            None,
            None,
        );
        let server_shutdown = shutdown.clone();
        let server_task = tokio::spawn(async move {
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
        Self {
            base_url,
            _tasks: TestTasks::new(shutdown, vec![agent_task, server_task]),
        }
    }
}

fn fast_config() -> CouncilConfig {
    CouncilConfig {
        heartbeat_interval_ms: 50,
        election_timeout_min_ms: 150,
        election_timeout_max_ms: 400,
        snapshot_threshold: 100,
        max_in_snapshot_log_to_keep: 50,
    }
}

/// A single-node council, initialised so it becomes leader.
async fn single_node_leader() -> Arc<CouncilNode> {
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let node = CouncilNode::new(
        1,
        fast_config(),
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    let mut members = BTreeMap::new();
    members.insert(
        1u64,
        CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1".to_string()),
    );
    node.initialize(members).await.unwrap();

    let node = Arc::new(node);
    for _ in 0..40 {
        if node.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node
}

/// A single-node leader council with a bootstrapped CA hierarchy + OIDC
/// config, so it can sign the build-signer CSR (code-signing EKU) that
/// `require_signatures` builds need.
async fn single_node_leader_with_security() -> Arc<CouncilNode> {
    let wrapping_ikm = b"test-wrapping-material-32bytes!!";
    let router = InMemoryRaftRouter::new();
    let network = InMemoryRaftNetworkFactory::new(1, router.clone());
    let node = CouncilNode::new(
        1,
        fast_config(),
        network,
        MemLogStore::new(),
        CouncilStateMachine::new(),
        Some(*wrapping_ikm),
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    let mut members = BTreeMap::new();
    members.insert(
        1u64,
        CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1".to_string()),
    );
    node.initialize(members).await.unwrap();
    let node = Arc::new(node);
    for _ in 0..40 {
        if node.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let hierarchy =
        reliaburger::sesame::ca::generate_ca_hierarchy("test-cluster", wrapping_ikm).unwrap();
    let oidc_config = reliaburger::sesame::oidc::generate_oidc_keypair(
        "https://test.reliaburger.dev",
        wrapping_ikm,
    )
    .unwrap();
    let security_state = reliaburger::sesame::types::SecurityState {
        certificate_authorities: vec![
            reliaburger::sesame::types::CertificateAuthority {
                private_key_wrapped: None,
                ..hierarchy.root.ca
            },
            hierarchy.node.ca,
            hierarchy.workload.ca,
            hierarchy.ingress.ca,
        ],
        age_keypairs: vec![],
        api_tokens: vec![],
        join_tokens: vec![],
        next_serial: 10,
        oidc_signing_config: Some(oidc_config),
        crl: reliaburger::sesame::types::Crl::default(),
        secret_seals: std::collections::BTreeMap::new(),
        node_leaves: std::collections::BTreeMap::new(),
    };
    node.write(reliaburger::council::types::RaftRequest::SecurityStateInit(
        Box::new(security_state),
    ))
    .await
    .unwrap();
    node
}

fn buildah_present() -> bool {
    std::process::Command::new("buildah")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// A pickle registry on an ephemeral port; returns (port, catalog, guard).
async fn start_registry() -> (
    u16,
    Arc<RwLock<reliaburger::pickle::types::ManifestCatalog>>,
    CancellationToken,
    tempfile::TempDir,
) {
    start_registry_with_council(None).await
}

/// A pickle registry backed by the supplied council when the test needs
/// registry writes and subsequent Raft commands to share one catalogue.
async fn start_registry_with_council(
    council: Option<Arc<CouncilNode>>,
) -> (
    u16,
    Arc<RwLock<reliaburger::pickle::types::ManifestCatalog>>,
    CancellationToken,
    tempfile::TempDir,
) {
    use reliaburger::pickle::api::router as pickle_router;
    use reliaburger::pickle::types::ManifestCatalog;

    let dir = tempfile::tempdir().unwrap();
    let catalog = Arc::new(RwLock::new(ManifestCatalog::default()));
    let state = registry_state(dir.path(), Arc::clone(&catalog), council);
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
    (port, catalog, shutdown, dir)
}

/// Pickle state over the blobs under `dir` and `catalog`. The test registry
/// serves one; a test that pulls as a node builds a second over the same
/// directory, the way a node's runtime shares its registry's blob store.
fn registry_state(
    dir: &std::path::Path,
    catalog: Arc<RwLock<reliaburger::pickle::types::ManifestCatalog>>,
    council: Option<Arc<CouncilNode>>,
) -> reliaburger::pickle::api::PickleState {
    reliaburger::pickle::api::PickleState {
        store: Arc::new(reliaburger::pickle::store::BlobStore::new(
            dir.join("blobs"),
        )),
        catalog,
        node_raft_id: 1,
        council,
        forwarder: None,
        test_leases: Default::default(),
        repository_writers: Default::default(),
        persist_path: None,
        auth: None,
        require_read_auth: false,
        allow_unauthenticated_bootstrap: true,
        quota: reliaburger::pickle::registry_auth::QuotaConfig::default(),
        sessions: reliaburger::pickle::registry_auth::UploadSessions::new(
            reliaburger::pickle::registry_auth::DEFAULT_UPLOAD_TTL,
        ),
    }
}

/// Tar a trivial context and upload it to the given registry port;
/// returns the digest.
async fn upload_trivial_context(registry_port: u16) -> String {
    let context_dir = tempfile::tempdir().unwrap();
    std::fs::write(context_dir.path().join("hello.txt"), b"hello").unwrap();
    std::fs::write(
        context_dir.path().join("Dockerfile"),
        b"FROM scratch\nCOPY hello.txt /hello.txt\n",
    )
    .unwrap();
    let tar_bytes = reliaburger::pickle::build::tar_context(context_dir.path()).unwrap();
    let digest = reliaburger::pickle::build::digest_of(&tar_bytes);
    let upload_url = reliaburger::pickle::build::context_upload_url("http", registry_port, &digest);
    let response = reqwest::Client::new()
        .post(&upload_url)
        .body(tar_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201, "context upload failed");
    digest
}

/// A mock builder node: accepts `/v1/build/run` (or refuses it) and
/// serves a canned status for the remote build id.
async fn start_mock_builder(refuse_runs: bool) -> (std::net::SocketAddr, CancellationToken) {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/v1/build/run",
            post(move || async move {
                if refuse_runs {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({ "error": "boom" })),
                    )
                } else {
                    (
                        axum::http::StatusCode::ACCEPTED,
                        axum::Json(serde_json::json!({ "build_id": 7 })),
                    )
                }
            }),
        )
        .route(
            "/v1/build/7",
            get(|| async {
                axum::Json(serde_json::json!({ "status": "completed", "image": "hello:v1" }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await
            .ok();
    });
    (addr, shutdown)
}

/// Without buildah and without peers, a submit is an honest 503 — not
/// the old 501, and not a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_without_any_builder_is_503() {
    assert!(
        !buildah_present(),
        "this negative-path test requires a host without Buildah"
    );
    let harness = Harness::start(HarnessOptions::default()).await;

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_submit_rejects_a_smuggled_registry_port() {
    let harness = Harness::start(HarnessOptions::default()).await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_run_rejects_a_traversal_context_digest() {
    let harness = Harness::start(HarnessOptions::default()).await;

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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_run_requires_the_system_principal() {
    let harness = Harness::start(HarnessOptions::default()).await;
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

/// The durable tracking endpoint is node-to-node only, like the other
/// internal build/batch endpoints (JOB1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_track_requires_the_system_principal() {
    let harness = Harness::start(HarnessOptions::default()).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/build/track", harness.base_url))
        .json(&serde_json::json!({
            "op": "update",
            "build_id": 1,
            "state": { "status": "running" },
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_run_rejects_a_dockerfile_escape() {
    let harness = Harness::start(HarnessOptions::default()).await;
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_build_id_is_404() {
    let harness = Harness::start(HarnessOptions::default()).await;
    let response = reqwest::get(format!("{}/v1/build/999", harness.base_url))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
}

/// 12b.2 (JOB5): delegation copies the context blob between registries
/// before the run request, so a builder whose registry has never seen
/// the blob can still build. Exercised directly against two real
/// registries — the source has the blob, the destination doesn't.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_transfer_copies_the_blob_to_the_builder_registry() {
    let (entry_port, _entry_catalog, entry_shutdown, _d1) = start_registry().await;
    let (builder_port, _builder_catalog, builder_shutdown, _d2) = start_registry().await;
    let digest = upload_trivial_context(entry_port).await;

    let client = reqwest::Client::new();
    reliaburger::bun::build_runner::transfer_context_to_builder(
        &client,
        "http",
        entry_port,
        &format!("127.0.0.1:{builder_port}"),
        &digest,
        256 * 1024 * 1024,
        None,
    )
    .await
    .expect("transfer should succeed");

    // The builder registry now serves the blob.
    let url = reliaburger::pickle::build::context_download_url("http", builder_port, &digest);
    let response = reqwest::get(&url).await.unwrap();
    assert_eq!(response.status().as_u16(), 200, "blob missing on builder");

    // Transferring again is an idempotent no-op (HEAD short-circuit).
    reliaburger::bun::build_runner::transfer_context_to_builder(
        &client,
        "http",
        entry_port,
        &format!("127.0.0.1:{builder_port}"),
        &digest,
        256 * 1024 * 1024,
        None,
    )
    .await
    .expect("repeat transfer should succeed");

    // A digest nobody holds is an honest error, not a silent success.
    let missing = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    let err = reliaburger::bun::build_runner::transfer_context_to_builder(
        &client,
        "http",
        entry_port,
        &format!("127.0.0.1:{builder_port}"),
        missing,
        256 * 1024 * 1024,
        None,
    )
    .await
    .unwrap_err();
    assert!(err.contains("not found"), "{err}");

    entry_shutdown.cancel();
    builder_shutdown.cancel();
}

/// D18: a failing builder no longer fails the build outright — the
/// submit path tries the next capable peer. The first mock builder
/// refuses every run; the second accepts, and the delegated status
/// read proxies through to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn builder_failure_retries_on_another_builder() {
    assert!(
        !buildah_present(),
        "this delegation test requires a host without Buildah"
    );
    let (registry_port, _catalog, registry_shutdown, _dir) = start_registry().await;
    let digest = upload_trivial_context(registry_port).await;

    let (bad_addr, bad_shutdown) = start_mock_builder(true).await;
    let (good_addr, good_shutdown) = start_mock_builder(false).await;

    let harness = Harness::start(HarnessOptions {
        node_name: Some("entry".to_string()),
        membership: Some(vec![
            NodeMembershipInfo {
                node_id: reliaburger::meat::NodeId("bad-builder".to_string()),
                address: bad_addr,
                api_advertised: true,
            },
            NodeMembershipInfo {
                node_id: reliaburger::meat::NodeId("good-builder".to_string()),
                address: good_addr,
                api_advertised: true,
            },
        ]),
        builder_nodes: vec!["bad-builder".to_string(), "good-builder".to_string()],
        registry_port,
        ..Default::default()
    })
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/build", harness.base_url))
        .json(&serde_json::json!({
            "name": "hello",
            "context_digest": digest,
            "spec": { "context": ".", "destination": "pickle://hello:v1" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202, "delegation should retry");
    let body: serde_json::Value = response.json().await.unwrap();
    let build_id = body["build_id"].as_u64().unwrap();

    // The delegated status read proxies to the good builder's record.
    let status: serde_json::Value =
        reqwest::get(format!("{}/v1/build/{build_id}", harness.base_url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(status["status"], "completed", "{status}");

    registry_shutdown.cancel();
    bad_shutdown.cancel();
    good_shutdown.cancel();
}

/// JOB4: with a council, build records live in the Raft state machine.
/// A new API process (a restarted node) still serves them, and the id
/// counter keeps climbing — ids are never reused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_records_survive_an_api_restart_and_ids_stay_monotonic() {
    let council = single_node_leader().await;

    let first = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("n1".to_string()),
        ..Default::default()
    })
    .await;
    let track = serde_json::json!({
        "op": "register",
        "record": {
            "name": "app",
            "runner_node": "some-other-node",
            "state": { "status": "running" },
            "created_at_epoch_secs": reliaburger::meat::batch_tracker::epoch_now_secs(),
        },
    });
    let response = reqwest::Client::new()
        .post(format!("{}/v1/build/track", first.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&track)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    let first_id = body["build_id"].as_u64().unwrap();
    drop(first);

    // A fresh process over the same council: the record is still there…
    let second = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("n1".to_string()),
        ..Default::default()
    })
    .await;
    let status: serde_json::Value =
        reqwest::get(format!("{}/v1/build/{first_id}", second.base_url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(status["status"], "running", "{status}");

    // …and a new registration gets a *new* id.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/build/track", second.base_url))
        .bearer_auth(TEST_SERVICE_TOKEN)
        .json(&track)
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["build_id"].as_u64().unwrap(), first_id + 1);
}

/// JOB4: a `Running` record whose runner is this node, with no live
/// runner task, means the node restarted mid-build. Reading its status
/// terminates it honestly instead of reporting `Running` forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_build_reports_an_honest_failure() {
    let council = single_node_leader().await;
    let response = council
        .write(reliaburger::council::types::RaftRequest::BuildRegister {
            build: reliaburger::bun::build_runner::BuildRecord {
                name: "app".to_string(),
                runner_node: Some("n1".to_string()),
                state: reliaburger::bun::build_runner::BuildState::Running,
                created_at_epoch_secs: reliaburger::meat::batch_tracker::epoch_now_secs(),
            },
        })
        .await
        .unwrap();
    let build_id = match response {
        reliaburger::council::types::CouncilResponse::BuildRegistered { build_id } => build_id,
        other => panic!("unexpected response: {other:?}"),
    };

    // The harness is "n1 after a restart": no live runner task.
    let harness = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("n1".to_string()),
        ..Default::default()
    })
    .await;
    let status: serde_json::Value =
        reqwest::get(format!("{}/v1/build/{build_id}", harness.base_url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(status["status"], "failed", "{status}");
    assert!(
        status["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("restarted"),
        "{status}"
    );

    // The honest failure is durable, not a transient answer.
    let record = council
        .desired_state()
        .await
        .build_state
        .get(build_id)
        .cloned()
        .unwrap();
    assert!(matches!(
        record.state,
        reliaburger::bun::build_runner::BuildState::Failed { .. }
    ));
}

/// JOB6 residue: the CLI's build wait is bounded — a build stuck in
/// `Running` (its runner is another node here, so no honest-failure
/// rewrite fires) ends the wait with an error carrying the last state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_build_wait_times_out_with_the_last_known_state() {
    let council = single_node_leader().await;
    let response = council
        .write(reliaburger::council::types::RaftRequest::BuildRegister {
            build: reliaburger::bun::build_runner::BuildRecord {
                name: "stuck".to_string(),
                runner_node: Some("somewhere-else".to_string()),
                state: reliaburger::bun::build_runner::BuildState::Running,
                created_at_epoch_secs: reliaburger::meat::batch_tracker::epoch_now_secs(),
            },
        })
        .await
        .unwrap();
    let build_id = match response {
        reliaburger::council::types::CouncilResponse::BuildRegistered { build_id } => build_id,
        other => panic!("unexpected response: {other:?}"),
    };

    let harness = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("n1".to_string()),
        ..Default::default()
    })
    .await;
    let client = reliaburger::relish::client::BunClient::new(&harness.base_url);
    let err =
        reliaburger::relish::commands::wait_for_build(&client, build_id, Duration::from_secs(2))
            .await
            .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("timed out"), "{message}");
    assert!(message.contains("running"), "{message}");
}

/// Build settings with Buildah storage of the test's own, so a test can check
/// what a build left behind and parallel tests never prune each other.
fn isolated_build_settings(dir: &std::path::Path) -> reliaburger::bun::build_runner::BuildSettings {
    reliaburger::bun::build_runner::BuildSettings::new(600, dir, 1024 * 1024 * 1024)
}

/// Run `buildah <args>` against `settings`' storage and return stdout.
fn buildah_output(
    settings: &reliaburger::bun::build_runner::BuildSettings,
    args: &[&str],
) -> String {
    let output = std::process::Command::new("buildah")
        .arg("--root")
        .arg(&settings.storage.root)
        .arg("--runroot")
        .arg(&settings.storage.runroot)
        .args(["--storage-driver", "vfs"])
        .args(args)
        .output()
        .expect("buildah runs");
    assert!(
        output.status.success(),
        "buildah {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Roadmap (Phase 12): a real build — context blob in a real registry,
/// buildah bud + push, manifest lands in the catalog — through the
/// async submit/poll API. Lima only (`RELIABURGER_BUILDAH_TESTS=1`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Buildah and RELIABURGER_BUILDAH_TESTS=1; run with make test-linux"]
async fn buildah_build_lands_in_the_catalog() {
    assert!(
        std::env::var("RELIABURGER_BUILDAH_TESTS").is_ok(),
        "set RELIABURGER_BUILDAH_TESTS=1 after installing Buildah"
    );

    let (registry_port, catalog, registry_shutdown, _dir) = start_registry().await;
    let digest = upload_trivial_context(registry_port).await;
    let storage = tempfile::tempdir().unwrap();
    let settings = isolated_build_settings(storage.path());

    // Buildah is present on this host, so the submit runs locally.
    let harness = Harness::start(HarnessOptions {
        node_name: Some("builder".to_string()),
        registry_port,
        pickle_catalog: Some(Arc::clone(&catalog)),
        build_settings: Some(settings.clone()),
        ..Default::default()
    })
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/build", harness.base_url))
        .json(&serde_json::json!({
            "name": "hello",
            "context_digest": digest,
            "spec": { "context": ".", "destination": "pickle://hello:v1" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202, "submit should accept");
    let body: serde_json::Value = response.json().await.unwrap();
    let build_id = body["build_id"].as_u64().unwrap();

    // Poll to a terminal state.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        let status: serde_json::Value =
            reqwest::get(format!("{}/v1/build/{build_id}", harness.base_url))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        match status["status"].as_str() {
            Some("completed") => break,
            Some("failed") => panic!("build failed: {status}"),
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "build did not finish: {status}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // The spec names no platform, so the build targets the default two. The
    // tag must name an index over both, and each platform manifest must be
    // catalogued by digest, so a node of either architecture can pull it.
    let catalog = catalog.read().await;
    let index = catalog
        .get_manifest_by_tag("hello", "v1")
        .expect("built image missing from the catalog");
    assert!(
        index.is_index(),
        "a two-platform build must land as an index"
    );
    assert_eq!(index.layers.len(), 2, "one manifest per platform");
    for platform in &index.layers {
        let manifest = catalog
            .get_repository_manifest("hello", platform.digest.as_str())
            .unwrap_or_else(|| panic!("platform manifest {} not catalogued", platform.digest));
        assert!(!manifest.is_index());
    }

    // Nothing the build made stays in Buildah's storage: no containers and,
    // as a `FROM scratch` build has no base image to cache, no images.
    assert_eq!(buildah_output(&settings, &["containers", "-q"]).trim(), "");
    assert_eq!(buildah_output(&settings, &["images", "-q"]).trim(), "");

    registry_shutdown.cancel();
}

/// Roadmap (12b.3 IMG2/IMG3/JOB7): a real build under `require_signatures`
/// signs with the persistent build signer, and the pushed manifest carries a
/// signature that passes the deploy-time check (build-sign → deploy-verify
/// round-trip). Lima only (`RELIABURGER_BUILDAH_TESTS=1`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Buildah and RELIABURGER_BUILDAH_TESTS=1; run with make test-linux"]
async fn buildah_build_signs_and_the_signature_verifies_on_deploy() {
    assert!(
        std::env::var("RELIABURGER_BUILDAH_TESTS").is_ok(),
        "set RELIABURGER_BUILDAH_TESTS=1 after installing Buildah"
    );

    let council = single_node_leader_with_security().await;
    let (registry_port, catalog, registry_shutdown, _dir) =
        start_registry_with_council(Some(Arc::clone(&council))).await;
    let digest = upload_trivial_context(registry_port).await;

    let root_ca_der = council
        .security_state()
        .await
        .active_ca(reliaburger::sesame::types::CaRole::Root)
        .map(|ca| ca.certificate_der.clone())
        .expect("root CA present");

    let storage = tempfile::tempdir().unwrap();
    let harness = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("builder".to_string()),
        registry_port,
        pickle_catalog: Some(Arc::clone(&catalog)),
        require_signatures: true,
        build_settings: Some(isolated_build_settings(storage.path())),
        ..Default::default()
    })
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/build", harness.base_url))
        .json(&serde_json::json!({
            "name": "hello",
            "context_digest": digest,
            "spec": { "context": ".", "destination": "pickle://hello:v1" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202, "submit should accept");
    let build_id = response.json::<serde_json::Value>().await.unwrap()["build_id"]
        .as_u64()
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        let status: serde_json::Value =
            reqwest::get(format!("{}/v1/build/{build_id}", harness.base_url))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        match status["status"].as_str() {
            // Completed only counts under require_signatures once signing
            // succeeded (JOB7) — so reaching Completed already proves signing.
            Some("completed") => break,
            Some("failed") => panic!("build failed: {status}"),
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "build did not finish: {status}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // The index the tag names, and each platform manifest, carry a
    // signature that verifies under the deploy-time trust policy and the
    // cluster root CA.
    let authoritative = council.manifest_catalog().await;
    let index = authoritative
        .get_manifest_by_tag("hello", "v1")
        .cloned()
        .expect("built image missing from the authoritative catalog");
    let mut signed = vec![index.clone()];
    for platform in &index.layers {
        signed.push(
            authoritative
                .get_repository_manifest("hello", platform.digest.as_str())
                .cloned()
                .expect("platform manifest missing from the authoritative catalog"),
        );
    }
    assert_eq!(signed.len(), 3, "the index and two platform manifests");
    for manifest in signed {
        let signature = manifest
            .signature
            .unwrap_or_else(|| panic!("{} must carry a signature", manifest.digest));
        reliaburger::pickle::signing::verify_signature(
            &signature,
            &manifest.digest,
            &reliaburger::config::node::TrustPolicySection {
                require_signatures: true,
                keys: vec![],
            },
            Some(&root_ca_der),
            None,
        )
        .expect("the build signature must verify on deploy");
    }

    registry_shutdown.cancel();
}

/// The OCI name of this host's architecture, as a platform in an index names it.
#[cfg(target_os = "linux")]
fn host_oci_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("no OCI platform for host architecture {other}"),
    }
}

/// Mixed architectures (#248): a real two-platform build lands in Pickle as an
/// index over `linux/amd64` and `linux/arm64`, and a node pulls and runs its
/// own platform's image under runc. Only the host's platform carries a
/// runnable binary, and each platform carries a marker naming itself, so the
/// container answering at all, and answering with the host's architecture,
/// shows the node picked the right entry. CI's privileged Linux job runs this
/// on x86_64, which makes it the amd64 half of a mixed cluster; an arm64 host
/// (the Lima VM) checks the other half.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires root, runc, Buildah, static /usr/bin/busybox, RELIABURGER_BUILDAH_TESTS=1 and RELIABURGER_RUNC_TESTS=1; run with make test-linux"]
async fn a_two_platform_build_runs_the_nodes_own_platform_under_runc() {
    use reliaburger::grill::runc::RuncGrill;
    use reliaburger::grill::{Grill, ImageStore, InstanceId, OciSpec};

    assert!(
        std::env::var("RELIABURGER_BUILDAH_TESTS").is_ok()
            && std::env::var("RELIABURGER_RUNC_TESTS").is_ok(),
        "set RELIABURGER_BUILDAH_TESTS=1 and RELIABURGER_RUNC_TESTS=1 on a host with Buildah and runc"
    );
    assert!(nix::unistd::geteuid().is_root(), "runc needs root");
    let host = host_oci_architecture();

    // `COPY $TARGETARCH/ /` gives each platform its own directory. The host's
    // holds busybox; the other holds only its marker, so running it fails.
    let (registry_port, catalog, registry_shutdown, registry_dir) = start_registry().await;
    let context = tempfile::tempdir().unwrap();
    for architecture in ["amd64", "arm64"] {
        let dir = context.path().join(architecture);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("platform"), architecture).unwrap();
    }
    std::fs::copy(
        "/usr/bin/busybox",
        context.path().join(host).join("busybox"),
    )
    .unwrap();
    std::fs::write(
        context.path().join("Dockerfile"),
        "FROM scratch\nARG TARGETARCH\nCOPY $TARGETARCH/ /\n",
    )
    .unwrap();
    let tar_bytes = reliaburger::pickle::build::tar_context(context.path()).unwrap();
    let context_digest = reliaburger::pickle::build::digest_of(&tar_bytes);
    let response = reqwest::Client::new()
        .post(reliaburger::pickle::build::context_upload_url(
            "http",
            registry_port,
            &context_digest,
        ))
        .body(tar_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201, "context upload failed");

    let storage = tempfile::tempdir().unwrap();
    let harness = Harness::start(HarnessOptions {
        node_name: Some("builder".to_string()),
        registry_port,
        pickle_catalog: Some(Arc::clone(&catalog)),
        build_settings: Some(isolated_build_settings(storage.path())),
        ..Default::default()
    })
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/build", harness.base_url))
        .json(&serde_json::json!({
            "name": "mixed",
            "context_digest": context_digest,
            "spec": { "context": ".", "destination": "pickle://mixed:v1" },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 202, "submit should accept");
    let build_id = response.json::<serde_json::Value>().await.unwrap()["build_id"]
        .as_u64()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        let status: serde_json::Value =
            reqwest::get(format!("{}/v1/build/{build_id}", harness.base_url))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        match status["status"].as_str() {
            Some("completed") => break,
            Some("failed") => panic!("build failed: {status}"),
            _ => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "build did not finish: {status}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Both platforms are in Pickle, under one index.
    let index = catalog
        .read()
        .await
        .get_manifest_by_tag("mixed", "v1")
        .cloned()
        .expect("built image missing from the catalog");
    assert!(
        index.is_index(),
        "a two-platform build must land as an index"
    );
    // Buildah may add an arm64 variant (`linux/arm64/v8`); the node matches
    // on `os/architecture`, so that's what the test compares.
    let mut platforms: Vec<String> = index
        .layers
        .iter()
        .map(|entry| {
            let platform = entry
                .platform
                .as_deref()
                .expect("an index entry records its platform");
            platform.split('/').take(2).collect::<Vec<_>>().join("/")
        })
        .collect();
    platforms.sort();
    assert_eq!(platforms, ["linux/amd64", "linux/arm64"]);

    // The node's runtime shares the registry's blob store and catalogue, as
    // Bun wires it, and resolves `mixed:v1` through Pickle.
    let runtime_root = tempfile::tempdir().unwrap();
    let images = ImageStore::new(runtime_root.path().join("images"));
    images.set_cluster_source(Arc::new(reliaburger::pickle::p2p::ClusterSource {
        state: registry_state(registry_dir.path(), Arc::clone(&catalog), None),
        members: None,
        registry_port,
        peer_scheme: "http".to_string(),
        concurrency: 4,
        client: reqwest::Client::new(),
        upstream: None,
        pull_through: false,
        cache_recheck_secs: 0,
        fill_lock: tokio::sync::Mutex::new(()),
    }));
    let runtime = RuncGrill::new(
        runtime_root.path().join("bundles"),
        images,
        false,
        runtime_root.path().join("state"),
        env!("CARGO_BIN_EXE_bun").into(),
    )
    .unwrap();
    let id = InstanceId(format!(
        "rbtest-mixed-arch-{}",
        runtime_root
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches('.')
    ));
    let mut spec: OciSpec = serde_json::from_value(serde_json::json!({
        "root": {"path": "mixed:v1", "readonly": true},
        "process": {"args": ["/busybox", "sleep", "60"], "env": ["PATH=/"], "cwd": "/", "user": {"uid": 0, "gid": 0}},
        "mounts": [], "linux": {"namespaces": []}
    }))
    .unwrap();
    spec.mounts = reliaburger::grill::oci::standard_mounts();
    spec.linux.namespaces = reliaburger::grill::oci::standard_namespaces(None);

    runtime.create(&id, &spec).await.unwrap();
    runtime.start(&id).await.unwrap();
    let answer = runtime
        .exec(&id, &["/busybox".into(), "cat".into(), "/platform".into()])
        .await;
    runtime.kill(&id).await.unwrap();
    assert_eq!(
        answer.expect("the node's own platform must run").trim(),
        host,
        "the node must run linux/{host} from the index"
    );

    registry_shutdown.cancel();
}

// --- Signing what a build pushed (#331) ---
//
// Only the leader can sign: the build signer's CSR reads the CA through a
// linearised read, and `AttachSignature` is a Raft write. A build that runs
// on a follower asks the leader through `/v1/build/sign`.

/// The CA hierarchy and OIDC config a council needs to sign a build-signer
/// CSR, written through `leader`.
async fn bootstrap_security(leader: &CouncilNode) {
    let wrapping_ikm = b"test-wrapping-material-32bytes!!";
    let hierarchy =
        reliaburger::sesame::ca::generate_ca_hierarchy("test-cluster", wrapping_ikm).unwrap();
    let oidc_config = reliaburger::sesame::oidc::generate_oidc_keypair(
        "https://test.reliaburger.dev",
        wrapping_ikm,
    )
    .unwrap();
    let security_state = reliaburger::sesame::types::SecurityState {
        certificate_authorities: vec![
            reliaburger::sesame::types::CertificateAuthority {
                private_key_wrapped: None,
                ..hierarchy.root.ca
            },
            hierarchy.node.ca,
            hierarchy.workload.ca,
            hierarchy.ingress.ca,
        ],
        age_keypairs: vec![],
        api_tokens: vec![],
        join_tokens: vec![],
        next_serial: 10,
        oidc_signing_config: Some(oidc_config),
        crl: reliaburger::sesame::types::Crl::default(),
        secret_seals: std::collections::BTreeMap::new(),
        node_leaves: std::collections::BTreeMap::new(),
    };
    leader
        .write(reliaburger::council::types::RaftRequest::SecurityStateInit(
            Box::new(security_state),
        ))
        .await
        .unwrap();
}

/// A two-member council with security bootstrapped: `(leader, follower)`.
async fn two_node_council_with_security() -> (Arc<CouncilNode>, Arc<CouncilNode>) {
    let wrapping_ikm = *b"test-wrapping-material-32bytes!!";
    let router = InMemoryRaftRouter::new();
    let mut nodes = Vec::new();
    for id in [1u64, 2] {
        let node = CouncilNode::new(
            id,
            fast_config(),
            InMemoryRaftNetworkFactory::new(id, router.clone()),
            MemLogStore::new(),
            CouncilStateMachine::new(),
            Some(wrapping_ikm),
        )
        .await
        .unwrap();
        router.register(id, node.raft().clone()).await;
        nodes.push(Arc::new(node));
    }
    let members = BTreeMap::from([
        (
            1u64,
            CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1".to_string()),
        ),
        (
            2u64,
            CouncilNodeInfo::new("127.0.0.1:9002".parse().unwrap(), "node-2".to_string()),
        ),
    ]);
    nodes[0].initialize(members).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let leader_index = loop {
        if nodes[0].is_leader().await {
            break 0;
        }
        if nodes[1].is_leader().await {
            break 1;
        }
        assert!(tokio::time::Instant::now() < deadline, "no leader elected");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let leader = Arc::clone(&nodes[leader_index]);
    let follower = Arc::clone(&nodes[1 - leader_index]);
    bootstrap_security(&leader).await;
    // The follower must know who leads before it can forward.
    while follower.current_leader().await.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the follower never learned the leader"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (leader, follower)
}

/// Record a build through the leader, returning its id.
async fn register_build(
    leader: &CouncilNode,
    state: reliaburger::bun::build_runner::BuildState,
) -> u64 {
    let response = leader
        .write(reliaburger::council::types::RaftRequest::BuildRegister {
            build: reliaburger::bun::build_runner::BuildRecord {
                name: "app".to_string(),
                runner_node: Some("node-2".to_string()),
                state,
                created_at_epoch_secs: reliaburger::meat::batch_tracker::epoch_now_secs(),
            },
        })
        .await
        .unwrap();
    match response {
        reliaburger::council::types::CouncilResponse::BuildRegistered { build_id } => build_id,
        other => panic!("unexpected response: {other:?}"),
    }
}

/// Commit an unsigned manifest to the catalogue, as a push would.
async fn commit_pushed_manifest(leader: &CouncilNode) -> reliaburger::pickle::types::Digest {
    use reliaburger::pickle::types::{Digest, ImageManifest, LayerDescriptor, ManifestCommit};
    let digest = Digest::from_sha256_hex(&"ab".repeat(32));
    let commit = ManifestCommit {
        observed_gc_generation: 0,
        manifest: ImageManifest {
            digest: digest.clone(),
            config: LayerDescriptor {
                digest: Digest::from_sha256_hex(&"cd".repeat(32)),
                size: 512,
                media_type: String::new(),
                platform: None,
            },
            layers: vec![],
            repository: "app".to_string(),
            tags: std::collections::BTreeSet::new(),
            total_size: 512,
            pushed_at: std::time::SystemTime::UNIX_EPOCH,
            pushed_by: 1,
            signature: None,
        },
        tag: "v1".to_string(),
        holder_nodes: std::collections::BTreeSet::from([1]),
    };
    leader
        .write(reliaburger::council::types::RaftRequest::ManifestCommit(
            commit,
        ))
        .await
        .unwrap();
    digest
}

async fn post_build_sign(
    base_url: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base_url}/v1/build/sign"))
        .json(&body);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    request.send().await.unwrap()
}

/// Whether `digest` carries a signature that verifies against the cluster
/// root CA, the way a deploy checks it.
async fn carries_a_verified_signature(
    council: &CouncilNode,
    digest: &reliaburger::pickle::types::Digest,
) -> bool {
    let catalog = council.manifest_catalog().await;
    let Some(signature) = catalog
        .get_manifest_by_tag("app", "v1")
        .filter(|manifest| &manifest.digest == digest)
        .and_then(|manifest| manifest.signature.clone())
    else {
        return false;
    };
    let security = council.security_state().await;
    let root = security
        .active_ca(reliaburger::sesame::types::CaRole::Root)
        .map(|ca| ca.certificate_der.clone());
    reliaburger::pickle::signing::verify_signature(
        &signature,
        digest,
        &reliaburger::config::node::TrustPolicySection {
            require_signatures: true,
            keys: vec![],
        },
        root.as_deref(),
        None,
    )
    .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn build_sign_requires_the_system_principal() {
    let harness = Harness::start(HarnessOptions::default()).await;
    let response = post_build_sign(
        &harness.base_url,
        None,
        serde_json::json!({ "build_id": 1, "namespace": "default", "digests": [] }),
    )
    .await;
    assert_eq!(response.status().as_u16(), 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_leader_signs_what_a_running_build_pushed() {
    let council = single_node_leader_with_security().await;
    let build_id = register_build(
        &council,
        reliaburger::bun::build_runner::BuildState::Running,
    )
    .await;
    let digest = commit_pushed_manifest(&council).await;
    let harness = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("node-1".to_string()),
        ..Default::default()
    })
    .await;

    let response = post_build_sign(
        &harness.base_url,
        Some(TEST_SERVICE_TOKEN),
        serde_json::json!({
            "build_id": build_id,
            "namespace": "default",
            "digests": [digest.as_str()],
        }),
    )
    .await;
    let status = response.status().as_u16();
    assert_eq!(status, 204, "{:?}", response.text().await);
    assert!(carries_a_verified_signature(&council, &digest).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_leader_refuses_to_sign_for_a_build_that_is_not_running() {
    let council = single_node_leader_with_security().await;
    let finished = register_build(
        &council,
        reliaburger::bun::build_runner::BuildState::Completed {
            image: "app:v1".to_string(),
        },
    )
    .await;
    let digest = commit_pushed_manifest(&council).await;
    let harness = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&council)),
        node_name: Some("node-1".to_string()),
        ..Default::default()
    })
    .await;

    for build_id in [finished, finished + 100] {
        let response = post_build_sign(
            &harness.base_url,
            Some(TEST_SERVICE_TOKEN),
            serde_json::json!({
                "build_id": build_id,
                "namespace": "default",
                "digests": [digest.as_str()],
            }),
        )
        .await;
        assert_eq!(response.status().as_u16(), 409, "build {build_id}");
    }
    assert!(!carries_a_verified_signature(&council, &digest).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_on_a_follower_is_signed_by_the_leader() {
    let (leader, follower) = two_node_council_with_security().await;
    let leader_id = leader.current_leader().await.unwrap();
    let leader_name = format!("node-{leader_id}");
    let build_id =
        register_build(&leader, reliaburger::bun::build_runner::BuildState::Running).await;
    let digest = commit_pushed_manifest(&leader).await;
    let leader_harness = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&leader)),
        node_name: Some(leader_name.clone()),
        ..Default::default()
    })
    .await;
    let leader_address: std::net::SocketAddr = leader_harness
        .base_url
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    let follower_harness = Harness::start(HarnessOptions {
        council: Some(Arc::clone(&follower)),
        node_name: Some(format!("node-{}", 3 - leader_id)),
        membership: Some(vec![NodeMembershipInfo {
            node_id: reliaburger::meat::NodeId::new(&leader_name),
            address: leader_address,
            api_advertised: true,
        }]),
        ..Default::default()
    })
    .await;

    // What the follower's build runner asks for after its push.
    let response = post_build_sign(
        &follower_harness.base_url,
        Some(TEST_SERVICE_TOKEN),
        serde_json::json!({
            "build_id": build_id,
            "namespace": "default",
            "digests": [digest.as_str()],
        }),
    )
    .await;
    let status = response.status().as_u16();
    assert_eq!(status, 204, "{:?}", response.text().await);
    assert!(carries_a_verified_signature(&leader, &digest).await);
}
