//! Phase 10 security integration tests.
//!
//! Tests the full lifecycle of join tokens, secret rotation, and CRL
//! using the sesame library directly (no running agent needed).

use std::time::{Duration, SystemTime};

use reliaburger::sesame::ca;
use reliaburger::sesame::cert;
use reliaburger::sesame::join;
use reliaburger::sesame::secret;
use reliaburger::sesame::types::*;

/// Helper: create a SecurityState with a CA hierarchy and a join token.
fn bootstrap_security_state() -> (SecurityState, String, [u8; 32]) {
    let wrapping_ikm = *b"test-wrapping-material-32bytes!!";
    let hierarchy = ca::generate_ca_hierarchy("test-cluster", &wrapping_ikm).unwrap();
    let (age_kp, _identity) =
        secret::generate_age_keypair(AgeKeyScope::ClusterWide, &wrapping_ikm, 0).unwrap();

    let (token_plaintext, token_hash) = ca::generate_join_token().unwrap();
    let join_token = JoinToken {
        token_hash,
        expires_at: SystemTime::now() + Duration::from_secs(900),
        consumed: false,
        attestation_mode: AttestationMode::None,
        node_id: "node-02".to_string(),
    };

    let state = SecurityState {
        certificate_authorities: vec![
            CertificateAuthority {
                private_key_wrapped: None,
                ..hierarchy.root.ca
            },
            hierarchy.node.ca,
            hierarchy.workload.ca,
            hierarchy.ingress.ca,
        ],
        age_keypairs: vec![age_kp],
        api_tokens: vec![],
        join_tokens: vec![join_token],
        next_serial: 6,
        oidc_signing_config: None,
        crl: Crl::default(),
        secret_seals: std::collections::BTreeMap::new(),
        node_leaves: std::collections::BTreeMap::new(),
        pending_intermediates: Vec::new(),
    };

    (state, token_plaintext, wrapping_ikm)
}

// ---------------------------------------------------------------------------
// Join token tests
// ---------------------------------------------------------------------------

/// Issue a node certificate the way the agent handler does: pre-check the
/// token, mark it consumed and allocate a serial atomically (mirroring the
/// Raft `ConsumeJoinTokenForIssue` step), then sign the joiner's CSR.
fn issue_for(
    state: &mut SecurityState,
    token: &str,
    node_id: &str,
    ikm: &[u8],
) -> Result<join::JoinResult, join::JoinError> {
    join::check_join_token(token, node_id, state)?;
    // Atomic consume + serial (single-node stand-in for the Raft entry).
    let jt = state
        .join_tokens
        .iter_mut()
        .find(|jt| ca::verify_join_token(token, &jt.token_hash))
        .unwrap();
    jt.consumed = true;
    let serial = reliaburger::sesame::types::SerialNumber(state.next_serial);
    state.next_serial += 1;

    let (csr_der, _key) = ca::create_node_csr(node_id).unwrap();
    join::sign_join_csr(
        &csr_der,
        node_id,
        serial,
        ca::NODE_LEAF_LIFETIME,
        state,
        ikm,
    )
}

#[test]
fn join_token_single_use_enforced() {
    let (mut state, token, ikm) = bootstrap_security_state();

    // First use should succeed
    let result = issue_for(&mut state, &token, "node-02", &ikm);
    assert!(result.is_ok());

    // Second use should fail — token is consumed
    let result2 = issue_for(&mut state, &token, "node-02", &ikm);
    assert!(result2.is_err());
    let err = format!("{}", result2.unwrap_err());
    assert!(
        err.contains("consumed") || err.contains("invalid"),
        "expected consumed error, got: {err}"
    );
}

#[test]
fn join_token_expiry_enforced() {
    let wrapping_ikm = *b"test-wrapping-material-32bytes!!";
    let hierarchy = ca::generate_ca_hierarchy("test-cluster", &wrapping_ikm).unwrap();

    let (token_plaintext, token_hash) = ca::generate_join_token().unwrap();
    // Token expired 1 second ago
    let join_token = JoinToken {
        token_hash,
        expires_at: SystemTime::now() - Duration::from_secs(1),
        consumed: false,
        attestation_mode: AttestationMode::None,
        node_id: "node-02".to_string(),
    };

    let mut state = SecurityState {
        certificate_authorities: vec![
            CertificateAuthority {
                private_key_wrapped: None,
                ..hierarchy.root.ca
            },
            hierarchy.node.ca,
            hierarchy.workload.ca,
            hierarchy.ingress.ca,
        ],
        age_keypairs: vec![],
        api_tokens: vec![],
        join_tokens: vec![join_token],
        next_serial: 6,
        oidc_signing_config: None,
        crl: Crl::default(),
        secret_seals: std::collections::BTreeMap::new(),
        node_leaves: std::collections::BTreeMap::new(),
        pending_intermediates: Vec::new(),
    };

    let result = issue_for(&mut state, &token_plaintext, "node-02", &wrapping_ikm);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(
        err.contains("expired"),
        "expected expired error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Secret rotation tests
// ---------------------------------------------------------------------------

#[test]
fn secret_rotation_dual_key_window() {
    let wrapping_ikm = *b"test-wrapping-material-32bytes!!";

    // Create generation 0 keypair
    let (kp0, _id0) =
        secret::generate_age_keypair(AgeKeyScope::ClusterWide, &wrapping_ikm, 0).unwrap();
    let pubkey0 = kp0.public_key.clone();

    // Create generation 1 keypair (simulates rotation)
    let (kp1, _id1) =
        secret::generate_age_keypair(AgeKeyScope::ClusterWide, &wrapping_ikm, 1).unwrap();
    let pubkey1 = kp1.public_key.clone();

    // Encrypt with old key
    let encrypted_old = secret::encrypt_secret("secret-old", &pubkey0).unwrap();
    assert!(encrypted_old.starts_with("ENC[AGE:"));

    // Encrypt with new key
    let encrypted_new = secret::encrypt_secret("secret-new", &pubkey1).unwrap();
    assert!(encrypted_new.starts_with("ENC[AGE:"));

    // Both should decrypt (dual-key window)
    let id0 = secret::unwrap_age_identity(&kp0, &wrapping_ikm).unwrap();
    let decrypted_old = secret::decrypt_secret(&encrypted_old, &id0).unwrap();
    assert_eq!(decrypted_old, "secret-old");

    let id1 = secret::unwrap_age_identity(&kp1, &wrapping_ikm).unwrap();
    let decrypted_new = secret::decrypt_secret(&encrypted_new, &id1).unwrap();
    assert_eq!(decrypted_new, "secret-new");
}

#[test]
fn secret_rotation_finalize_drops_old_key() {
    let wrapping_ikm = *b"test-wrapping-material-32bytes!!";

    let (mut kp0, _) =
        secret::generate_age_keypair(AgeKeyScope::ClusterWide, &wrapping_ikm, 0).unwrap();
    let (kp1, _) =
        secret::generate_age_keypair(AgeKeyScope::ClusterWide, &wrapping_ikm, 1).unwrap();

    // Mark old as read-only (simulates RotateSecretKey Raft command)
    kp0.read_only = true;

    // Simulate finalize: remove read-only keypairs
    let mut keypairs = vec![kp0, kp1.clone()];
    keypairs.retain(|kp| !kp.read_only);

    // Only generation 1 remains
    assert_eq!(keypairs.len(), 1);
    assert_eq!(keypairs[0].generation, 1);
    assert!(!keypairs[0].read_only);

    // New key still works
    let id1 = secret::unwrap_age_identity(&kp1, &wrapping_ikm).unwrap();
    let encrypted = secret::encrypt_secret("after-finalize", &kp1.public_key).unwrap();
    let decrypted = secret::decrypt_secret(&encrypted, &id1).unwrap();
    assert_eq!(decrypted, "after-finalize");
}

// ---------------------------------------------------------------------------
// CRL tests
// ---------------------------------------------------------------------------

#[test]
fn crl_revoked_cert_rejected() {
    let crl = Crl {
        retired_nodes: Default::default(),
        entries: vec![CrlEntry {
            serial: SerialNumber(42),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "node compromised".to_string(),
            expires_at: None,
        }],
        version: 1,
        updated_at: SystemTime::now(),
    };

    // Revoked serial should be rejected
    let result = cert::check_crl(SerialNumber(42), &crl);
    assert!(result.is_err());
    let err = format!("{}", result.unwrap_err());
    assert!(err.contains("revoked"));
    assert!(err.contains("compromised"));
}

#[test]
fn crl_valid_cert_allowed() {
    let crl = Crl {
        retired_nodes: Default::default(),
        entries: vec![CrlEntry {
            serial: SerialNumber(42),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "node compromised".to_string(),
            expires_at: None,
        }],
        version: 1,
        updated_at: SystemTime::now(),
    };

    // Different serial should pass
    let result = cert::check_crl(SerialNumber(99), &crl);
    assert!(result.is_ok());
}

#[test]
fn crl_empty_allows_all() {
    let crl = Crl::default();
    let result = cert::check_crl(SerialNumber(1), &crl);
    assert!(result.is_ok());
}

// ---------------------------------------------------------------------------
// Join ceremony over HTTP: a joiner fetches and persists its identity
// ---------------------------------------------------------------------------

/// A minimal issuer endpoint: it mirrors the agent's `handle_join_issue`
/// core — validate the token, issue a cert, return the bundle as JSON.
async fn spawn_issuer(
    state: SecurityState,
    ikm: [u8; 32],
) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, extract::State, routing::post};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Issuer {
        state: Arc<Mutex<SecurityState>>,
        ikm: [u8; 32],
    }

    async fn issue(
        State(issuer): State<Issuer>,
        Json(body): Json<serde_json::Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        use base64::Engine as _;
        let token = body["token"].as_str().unwrap_or_default();
        let node_id = body["node_id"].as_str().unwrap_or_default();
        let csr_der = base64::engine::general_purpose::STANDARD
            .decode(body["csr_b64"].as_str().unwrap_or_default())
            .unwrap_or_default();

        let mut state = issuer.state.lock().unwrap();
        let issued = (|| {
            join::check_join_token(token, node_id, &state)?;
            let jt = state
                .join_tokens
                .iter_mut()
                .find(|jt| ca::verify_join_token(token, &jt.token_hash))
                .ok_or(join::JoinError::InvalidToken)?;
            jt.consumed = true;
            let serial = reliaburger::sesame::types::SerialNumber(state.next_serial);
            state.next_serial += 1;
            join::sign_join_csr(
                &csr_der,
                node_id,
                serial,
                ca::NODE_LEAF_LIFETIME,
                &state,
                &issuer.ikm,
            )
        })();

        match issued {
            Ok(result) => Json(join::JoinBundle::from_result(&result)).into_response(),
            Err(e) => (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
        }
    }

    let issuer = Issuer {
        state: Arc::new(Mutex::new(state)),
        ikm,
    };
    let app = Router::new()
        .route(
            "/v1/cluster/ca",
            axum::routing::get(|| async {
                Json(serde_json::json!({"compatibility": reliaburger::compatibility::CURRENT}))
            }),
        )
        // A freshly started Bun answers `/v1/version` only once it has
        // hashed its own binary, which on a saturated host takes longer
        // than a joiner waits (#884). The join must not depend on it.
        .route(
            "/v1/version",
            axum::routing::get(|| std::future::pending::<Json<serde_json::Value>>()),
        )
        .route("/v1/cluster/join", post(issue))
        .with_state(issuer);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1/cluster/join"), handle)
}

#[tokio::test]
async fn joiner_persists_the_identity_returned_by_a_remote_member() {
    let (state, token, ikm) = bootstrap_security_state();
    let (url, server) = spawn_issuer(state, ikm).await;

    let client = reqwest::Client::new();
    let identity = join::request_join(&client, &url, &token, "node-02", None)
        .await
        .expect("join should succeed");
    assert_eq!(identity.node_id, "node-02");

    // Persist and reload, proving the round trip lands on disk usable.
    let dir = tempfile::tempdir().unwrap();
    reliaburger::sesame::identity_store::save(dir.path(), &identity).unwrap();
    let loaded = reliaburger::sesame::identity_store::load(dir.path())
        .unwrap()
        .unwrap();
    cert::validate_chain(
        &loaded.certificate_der,
        &loaded.node_ca_der,
        &loaded.root_ca_der,
    )
    .unwrap();

    server.abort();
}

#[tokio::test]
async fn joiner_checks_compatibility_while_the_members_binary_hash_is_still_running() {
    let (state, token, ikm) = bootstrap_security_state();
    let (url, server) = spawn_issuer(state, ikm).await;

    let client = reqwest::Client::new();
    let identity = tokio::time::timeout(
        Duration::from_secs(5),
        join::request_join(&client, &url, &token, "node-02", None),
    )
    .await
    .expect("the join waited on the member's /v1/version")
    .expect("join should succeed");
    assert_eq!(identity.node_id, "node-02");

    server.abort();
}

#[tokio::test]
async fn joiner_refuses_a_member_with_the_wrong_root_ca_fingerprint() {
    let (state, token, ikm) = bootstrap_security_state();
    let (url, server) = spawn_issuer(state, ikm).await;

    let client = reqwest::Client::new();
    let err = join::request_join(
        &client,
        &url,
        &token,
        "node-02",
        Some("sha256:0000000000000000000000000000000000000000000000000000000000000000"),
    )
    .await
    .expect_err("a fingerprint mismatch must be refused");
    assert!(
        matches!(err, join::JoinClientError::FingerprintMismatch { .. }),
        "got: {err:?}"
    );

    server.abort();
}

#[tokio::test]
async fn joiner_rejects_a_bad_join_token() {
    let (state, _token, ikm) = bootstrap_security_state();
    let (url, server) = spawn_issuer(state, ikm).await;

    let client = reqwest::Client::new();
    let err = join::request_join(&client, &url, "not-a-real-token", "node-02", None)
        .await
        .expect_err("a bad token must be rejected");
    assert!(
        matches!(err, join::JoinClientError::Rejected(_)),
        "got: {err:?}"
    );

    server.abort();
}

#[tokio::test]
async fn incompatible_member_never_receives_the_join_token() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for version in [
        serde_json::json!({"version":"development"}),
        serde_json::json!({"compatibility":{"protocol":3,"state":99}}),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        // `/v1/version` claims the current formats, so only a check of
        // what the pinned member's CA endpoint reports refuses the join.
        let app = Router::new()
            .route(
                "/v1/cluster/ca",
                get(move || {
                    let version = version.clone();
                    async move { Json(version) }
                }),
            )
            .route(
                "/v1/version",
                get(|| async {
                    Json(serde_json::json!({"compatibility": reliaburger::compatibility::CURRENT}))
                }),
            )
            .route(
                "/v1/cluster/join",
                post(move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        "unexpected"
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/cluster/join", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        assert!(
            join::request_join(
                &reqwest::Client::new(),
                &url,
                "must-stay-local",
                "new",
                None
            )
            .await
            .is_err()
        );
        assert_eq!(observed.load(Ordering::SeqCst), 0);
        server.abort();
        let _ = server.await;
    }
}

/// A dashboard session made from the service token is read-only. It used
/// to carry the service principal's name, which was all the node-to-node
/// gate checked, so a stolen cookie could run arbitrary jobs on a node.
#[tokio::test]
async fn service_token_cookie_cannot_run_a_batch_on_a_node() {
    use tower::ServiceExt;

    let service_token = format!("rbrg_{}", "5".repeat(64));
    let user = reliaburger::sesame::token::create_token(
        "operator",
        ApiRole::Admin,
        TokenScope::default(),
        None,
    )
    .unwrap();
    let store = reliaburger::sesame::auth::new_token_store();
    store.write().await.push(user.token);
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
    let app = reliaburger::bun::api::router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(store),
        Some(service_token.clone()),
        None,
        None,
        None,
        0,
        None,
    );

    // Log in to the dashboard with the service token, as a browser would.
    let login = app
        .clone()
        .oneshot(
            axum::http::Request::post("/ui/session")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(axum::body::Body::from(format!("token={service_token}")))
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = login
        .headers()
        .get("set-cookie")
        .expect("the service token opens a dashboard session")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // The leader's sync call is how a node receives job work, so it is the
    // node-to-node route a stolen cookie would try.
    let run = serde_json::to_string(&reliaburger::bun::task_array_node::NodeSyncRequest::default())
        .unwrap();
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::post("/v1/batch/array/sync")
                .header("cookie", &cookie)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(run))
                .unwrap(),
        )
        .await
        .unwrap();
    // 403, not 401: the cookie is a live session, just not a cluster node.
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert!(
        cmd_rx.try_recv().is_err(),
        "the node's agent received work from a read-only session"
    );
}

/// A single-node council that has elected itself leader.
async fn single_node_leader() -> std::sync::Arc<reliaburger::council::node::CouncilNode> {
    use reliaburger::council::log_store::MemLogStore;
    use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use reliaburger::council::node::CouncilNode;
    use reliaburger::council::state_machine::CouncilStateMachine;
    use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};

    let router = InMemoryRaftRouter::new();
    let node = CouncilNode::new(
        1,
        CouncilConfig {
            heartbeat_interval_ms: 50,
            election_timeout_min_ms: 150,
            election_timeout_max_ms: 400,
            ..CouncilConfig::default()
        },
        InMemoryRaftNetworkFactory::new(1, router.clone()),
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    node.initialize(std::collections::BTreeMap::from([(
        1u64,
        CouncilNodeInfo::new("127.0.0.1:9001".parse().unwrap(), "node-1".to_string()),
    )]))
    .await
    .unwrap();
    let node = std::sync::Arc::new(node);
    for _ in 0..80 {
        if node.is_leader().await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    node
}

/// One run of a Bun's API: a router over `council` whose events go to the
/// audit log in `audit_directory`, opened afresh as a starting Bun does.
async fn bun_api_run(
    council: &std::sync::Arc<reliaburger::council::node::CouncilNode>,
    tokens: Vec<ApiToken>,
    audit_directory: &std::path::Path,
) -> axum::Router {
    let log = reliaburger::bun::audit::AuditLog::open(
        audit_directory.to_path_buf(),
        reliaburger::bun::audit::AuditLimits::from_total(
            reliaburger::bun::audit::DEFAULT_AUDIT_MAX_BYTES,
        ),
    )
    .unwrap();
    let mut events = reliaburger::bun::events::EventStore::new();
    events.set_audit_log(log);
    let store = reliaburger::sesame::auth::new_token_store();
    *store.write().await = tokens;
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::channel(16);
    reliaburger::bun::api::router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        Some(std::sync::Arc::clone(council)),
        Some(store),
        None,
        None,
        None,
        None,
        0,
        Some(std::sync::Arc::new(tokio::sync::RwLock::new(events))),
    )
}

/// The audit trail outlives the process that wrote it (#685): a token
/// created before a Bun restart is still listed by `GET /v1/audit` after it,
/// from the log on disk rather than the event ring that the restart emptied.
#[tokio::test]
async fn audit_log_lists_a_token_creation_after_a_bun_restart() {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let admin = reliaburger::sesame::token::create_token(
        "operator",
        ApiRole::Admin,
        TokenScope::default(),
        None,
    )
    .unwrap();
    let council = single_node_leader().await;
    council
        .write(reliaburger::council::types::RaftRequest::CreateApiToken(
            admin.token.clone(),
        ))
        .await
        .unwrap();
    let data = tempfile::tempdir().unwrap();
    let audit_directory = data.path().join("audit");

    let before = bun_api_run(&council, vec![admin.token.clone()], &audit_directory).await;
    let created = before
        .oneshot(
            axum::http::Request::post("/v1/token/create")
                .header("authorization", format!("Bearer {}", admin.plaintext))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({"name": "ci", "role": "deployer"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), axum::http::StatusCode::OK);

    // The Bun restarts: a new event store, empty, and the log reopened.
    let after = bun_api_run(&council, vec![admin.token.clone()], &audit_directory).await;
    let listed = after
        .oneshot(
            axum::http::Request::get("/v1/audit?action=token.created")
                .header("authorization", format!("Bearer {}", admin.plaintext))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), axum::http::StatusCode::OK);
    let body = listed.into_body().collect().await.unwrap().to_bytes();
    let listing: reliaburger::bun::audit::AuditListing = serde_json::from_slice(&body).unwrap();
    assert_eq!(listing.records.len(), 1, "{listing:?}");
    let record = &listing.records[0];
    assert_eq!(record.action, "token.created");
    assert_eq!(
        record.details.get("token_name").map(String::as_str),
        Some("operator")
    );
    council.shutdown().await.unwrap();
}
