//! F05 I3: token rotation, the default lifetime and the `[permission]`
//! inheritance check, through the real router and a one-node council.

use std::time::{Duration, SystemTime};

use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;
use crate::sesame::token::{CreatedToken, create_token};
use crate::sesame::types::{ApiRole, ApiToken, TokenScope};

/// A router over a seeded council whose store, in Raft and in the auth
/// layer, holds `tokens`; and the event store audit lands in.
struct Fixture {
    app: Router,
    council: Arc<crate::council::CouncilNode>,
    events: Arc<RwLock<crate::bun::events::EventStore>>,
}

async fn fixture(tag: &str, tokens: &[ApiToken]) -> Fixture {
    let council = super::tests::seeded_council(tag).await;
    for token in tokens {
        council
            .write(crate::council::RaftRequest::CreateApiToken(token.clone()))
            .await
            .unwrap();
    }
    let store = crate::sesame::auth::new_token_store();
    *store.write().await = tokens.to_vec();
    let events = Arc::new(RwLock::new(crate::bun::events::EventStore::new()));
    let (cmd_tx, _cmd_rx) = mpsc::channel(32);
    let app = router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        Some(Arc::clone(&council)),
        Some(store),
        None,
        None,
        None,
        None,
        9117,
        Some(Arc::clone(&events)),
    );
    Fixture {
        app,
        council,
        events,
    }
}

fn token(name: &str, role: ApiRole) -> CreatedToken {
    create_token(name, role, TokenScope::default(), None).unwrap()
}

async fn send(app: &Router, request: axum::http::Request<Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn post(
    app: &Router,
    uri: &str,
    bearer: &str,
    body: serde_json::Value,
) -> (StatusCode, String) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, request).await
}

/// A read any valid token may make, so a status says whether it works.
async fn read_with(app: &Router, bearer: &str) -> StatusCode {
    let request = axum::http::Request::builder()
        .uri("/v1/secret/public-key")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    send(app, request).await.0
}

async fn rotate(app: &Router, admin: &str, body: serde_json::Value) -> serde_json::Value {
    let (status, body) = post(app, "/v1/token/rotate", admin, body).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn unix_seconds(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
}

#[tokio::test]
async fn rotate_issues_a_new_secret_that_works_at_once_while_the_old_one_keeps_working() {
    let admin = token("admin", ApiRole::Admin);
    let ci = token("ci", ApiRole::Deployer);
    let fx = fixture("rotate-grace", &[admin.token.clone(), ci.token.clone()]).await;

    let before = SystemTime::now();
    let answer = rotate(&fx.app, &admin.plaintext, serde_json::json!({"name": "ci"})).await;
    let new_secret = answer["token"].as_str().unwrap().to_string();
    assert_ne!(new_secret, ci.plaintext);
    let grace_end = answer["previous_valid_until"].as_u64().unwrap();
    assert!(grace_end >= unix_seconds(before + Duration::from_secs(24 * 3_600)));

    assert_eq!(read_with(&fx.app, &new_secret).await, StatusCode::OK);
    assert_eq!(read_with(&fx.app, &ci.plaintext).await, StatusCode::OK);
    // Still one token, under the same name.
    let tokens = fx.council.security_state().await.api_tokens;
    assert_eq!(tokens.iter().filter(|t| t.name == "ci").count(), 1);
}

#[tokio::test]
async fn the_old_secret_gets_401_once_its_grace_period_is_over() {
    let admin = token("admin", ApiRole::Admin);
    let ci = token("ci", ApiRole::Deployer);
    // The council holds `ci` mid-rotation with a grace period that ended a
    // second ago: what a 24-hour grace looks like a day later.
    let mut rotated = ci.token.clone();
    let rotation =
        crate::sesame::token::rotate_token(&ci.token, SystemTime::now(), Duration::from_secs(60))
            .unwrap();
    crate::sesame::token::apply_rotation(&mut rotated, &rotation.rotation);
    if let Some(previous) = rotated.previous_secret.as_mut() {
        previous.valid_until = SystemTime::now() - Duration::from_secs(1);
    }
    let fx = fixture("rotate-after-grace", &[admin.token.clone(), rotated]).await;

    let request = axum::http::Request::builder()
        .uri("/v1/secret/public-key")
        .header("authorization", format!("Bearer {}", ci.plaintext))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&fx.app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("rotated"), "{body}");
    assert_eq!(
        read_with(&fx.app, &rotation.plaintext).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_rotation_without_grace_ends_the_old_secret_and_its_session_at_once() {
    let admin = token("admin", ApiRole::Admin);
    let ci = token("ci", ApiRole::ReadOnly);
    let fx = fixture("rotate-session", &[admin.token.clone(), ci.token.clone()]).await;

    let login = axum::http::Request::builder()
        .method("POST")
        .uri("/ui/session")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(format!("token={}", ci.plaintext)))
        .unwrap();
    let response = fx.app.clone().oneshot(login).await.unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let with_cookie = || {
        axum::http::Request::builder()
            .uri("/v1/secret/public-key")
            .header("cookie", cookie.clone())
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(send(&fx.app, with_cookie()).await.0, StatusCode::OK);

    let answer = rotate(
        &fx.app,
        &admin.plaintext,
        serde_json::json!({"name": "ci", "grace_hours": 0}),
    )
    .await;
    assert!(answer["previous_valid_until"].is_null(), "{answer}");
    assert_eq!(
        read_with(&fx.app, &ci.plaintext).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&fx.app, with_cookie()).await.0,
        StatusCode::UNAUTHORIZED
    );
    let new_secret = answer["token"].as_str().unwrap();
    assert_eq!(read_with(&fx.app, new_secret).await, StatusCode::OK);
}

#[tokio::test]
async fn the_permission_spec_follows_the_name_through_a_rotation() {
    let root = token("root", ApiRole::Admin);
    let ops = token("ops", ApiRole::Admin);
    let fx = fixture("rotate-spec", &[root.token.clone(), ops.token.clone()]).await;
    // `ops` is an Admin narrowed by its spec to reading logs.
    fx.council
        .write(crate::council::RaftRequest::PermissionSpec {
            name: "ops".into(),
            spec: Box::new(crate::config::PermissionSpec {
                actions: vec!["logs".into()],
                apps: vec!["*".into()],
                namespaces: None,
            }),
        })
        .await
        .unwrap();

    let answer = rotate(&fx.app, &root.plaintext, serde_json::json!({"name": "ops"})).await;
    let new_secret = answer["token"].as_str().unwrap();
    // The spec still applies to the new secret: no cluster administration.
    let (status, body) = post(
        &fx.app,
        "/v1/token/revoke",
        new_secret,
        serde_json::json!({"name": "root"}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        fx.council
            .desired_state()
            .await
            .permissions
            .contains_key("ops"),
        "the spec stays keyed by the name"
    );
}

#[tokio::test]
async fn the_last_admin_can_be_rotated() {
    let root = token("root", ApiRole::Admin);
    let fx = fixture("rotate-last-admin", std::slice::from_ref(&root.token)).await;

    let answer = rotate(
        &fx.app,
        &root.plaintext,
        serde_json::json!({"name": "root"}),
    )
    .await;
    let new_secret = answer["token"].as_str().unwrap();
    let request = axum::http::Request::builder()
        .uri("/v1/token/list")
        .header("authorization", format!("Bearer {new_secret}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&fx.app, request).await.0, StatusCode::OK);
    let tokens = fx.council.security_state().await.api_tokens;
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].role, ApiRole::Admin);
}

#[tokio::test]
async fn rotating_an_unknown_token_is_404() {
    let root = token("root", ApiRole::Admin);
    let fx = fixture("rotate-unknown", std::slice::from_ref(&root.token)).await;
    let (status, body) = post(
        &fx.app,
        "/v1/token/rotate",
        &root.plaintext,
        serde_json::json!({"name": "ghost"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn rotation_is_audited_without_the_new_secret() {
    let root = token("root", ApiRole::Admin);
    let ci = token("ci", ApiRole::Deployer);
    let fx = fixture("rotate-audit", &[root.token.clone(), ci.token.clone()]).await;

    let answer = rotate(&fx.app, &root.plaintext, serde_json::json!({"name": "ci"})).await;
    let recorded = fx.events.read().await.recent(20, None, None);
    let event = recorded
        .iter()
        .find(|event| event.action.as_deref() == Some("token.rotated"))
        .unwrap_or_else(|| panic!("no token.rotated event in {recorded:?}"));
    assert_eq!(
        event.principal.as_deref(),
        Some(crate::sesame::auth::token_principal_id(&root.token).as_str())
    );
    assert_eq!(event.details.get("token").map(String::as_str), Some("ci"));
    assert!(
        event.details.contains_key("previous_valid_until"),
        "{event:?}"
    );
    let all = serde_json::to_string(&recorded).unwrap();
    assert!(!all.contains(answer["token"].as_str().unwrap()));
}

async fn create(app: &Router, admin: &str, body: serde_json::Value) -> (StatusCode, String) {
    post(app, "/v1/token/create", admin, body).await
}

fn expiry_of(tokens: &[ApiToken], name: &str) -> Option<SystemTime> {
    tokens.iter().find(|t| t.name == name).unwrap().expires_at
}

#[tokio::test]
async fn a_token_created_without_a_lifetime_gets_the_default_unless_admin_or_opted_out() {
    let root = token("root", ApiRole::Admin);
    let fx = fixture("default-ttl", std::slice::from_ref(&root.token)).await;

    let before = SystemTime::now();
    for (name, role) in [
        ("ci", "deployer"),
        ("reader", "read-only"),
        ("ops", "admin"),
    ] {
        let (status, body) = create(
            &fx.app,
            &root.plaintext,
            serde_json::json!({"name": name, "role": role}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
    }
    let (status, body) = create(
        &fx.app,
        &root.plaintext,
        serde_json::json!({"name": "forever", "role": "deployer", "no_expiry": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let after = SystemTime::now();

    let tokens = fx.council.security_state().await.api_tokens;
    let ninety_days = Duration::from_secs(90 * 86_400);
    for name in ["ci", "reader"] {
        let expiry = expiry_of(&tokens, name).unwrap();
        assert!(expiry >= before + ninety_days && expiry <= after + ninety_days);
    }
    assert_eq!(expiry_of(&tokens, "ops"), None, "Admin tokens are exempt");
    assert_eq!(expiry_of(&tokens, "forever"), None);
}

#[tokio::test]
async fn a_create_answer_says_when_the_token_expires() {
    let root = token("root", ApiRole::Admin);
    let fx = fixture("create-expiry-answer", std::slice::from_ref(&root.token)).await;
    let (_, body) = create(
        &fx.app,
        &root.plaintext,
        serde_json::json!({"name": "ci", "role": "deployer"}),
    )
    .await;
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    let stored = expiry_of(&fx.council.security_state().await.api_tokens, "ci").unwrap();
    assert_eq!(answer["expires_at"].as_u64(), Some(unix_seconds(stored)));
}

#[tokio::test]
async fn ttl_days_and_no_expiry_together_are_refused() {
    let root = token("root", ApiRole::Admin);
    let fx = fixture("ttl-conflict", std::slice::from_ref(&root.token)).await;
    let (status, body) = create(
        &fx.app,
        &root.plaintext,
        serde_json::json!({"name": "ci", "role": "deployer", "ttl_days": 7, "no_expiry": true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// Decision 3: a name with a `[permission]` spec is a decision, not an
/// accident. A token re-created under it would silently inherit the spec.
#[tokio::test]
async fn creating_a_token_under_a_name_with_a_permission_spec_needs_inherit_permissions() {
    let root = token("root", ApiRole::Admin);
    let fx = fixture("inherit-spec", std::slice::from_ref(&root.token)).await;
    fx.council
        .write(crate::council::RaftRequest::PermissionSpec {
            name: "ci".into(),
            spec: Box::new(crate::config::PermissionSpec {
                actions: vec!["deploy".into()],
                apps: vec!["*".into()],
                namespaces: None,
            }),
        })
        .await
        .unwrap();

    let (status, body) = create(
        &fx.app,
        &root.plaintext,
        serde_json::json!({"name": "ci", "role": "deployer"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("--inherit-permissions"), "{body}");
    assert!(
        !fx.council
            .security_state()
            .await
            .api_tokens
            .iter()
            .any(|t| t.name == "ci")
    );

    let (status, body) = create(
        &fx.app,
        &root.plaintext,
        serde_json::json!({"name": "ci", "role": "deployer", "inherit_permissions": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
