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
    };

    (state, token_plaintext, wrapping_ikm)
}

// ---------------------------------------------------------------------------
// Join token tests
// ---------------------------------------------------------------------------

#[test]
fn join_token_single_use_enforced() {
    let (mut state, token, ikm) = bootstrap_security_state();

    // First use should succeed
    let result = join::validate_and_issue(&token, "node-02", &mut state, &ikm);
    assert!(result.is_ok());

    // Second use should fail — token is consumed
    let result2 = join::validate_and_issue(&token, "node-03", &mut state, &ikm);
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
    };

    let result = join::validate_and_issue(&token_plaintext, "node-02", &mut state, &wrapping_ikm);
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
        entries: vec![CrlEntry {
            serial: SerialNumber(42),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "node compromised".to_string(),
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
        entries: vec![CrlEntry {
            serial: SerialNumber(42),
            issuer: CaRole::Node,
            revoked_at: SystemTime::now(),
            reason: "node compromised".to_string(),
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
        let token = body["token"].as_str().unwrap_or_default();
        let node_id = body["node_id"].as_str().unwrap_or_default();
        let mut state = issuer.state.lock().unwrap();
        match join::validate_and_issue(token, node_id, &mut state, &issuer.ikm) {
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
