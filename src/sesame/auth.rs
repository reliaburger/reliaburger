//! API authentication middleware.
//!
//! Validates Bearer tokens from the `Authorization` header against
//! stored API tokens. Each request is checked for role and scope.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::RwLock;

use super::token;
use super::types::{ApiRole, ApiToken};

/// Shared token store, readable by the auth middleware.
///
/// Wrapped in `Arc<RwLock>` so it can be updated via Raft without
/// blocking request handling.
pub type TokenStore = Arc<RwLock<Vec<ApiToken>>>;

/// Create an empty token store.
pub fn new_token_store() -> TokenStore {
    Arc::new(RwLock::new(Vec::new()))
}

/// Reserved name for the internal service principal (the derived service
/// token). Not a real user token; never stored in `SecurityState`.
pub const SYSTEM_PRINCIPAL: &str = "__system";

/// State shared with the auth middleware: the live user-token store plus the
/// optional side-channel service token (see [`super::token::derive_service_token`]).
#[derive(Clone)]
pub struct AuthState {
    /// User tokens, refreshed from Raft. Emptiness gates the bootstrap window.
    pub tokens: TokenStore,
    /// The cluster's internal service token, accepted as the system principal.
    /// `None` in single-node mode (no master key).
    pub service_token: Option<String>,
    /// Browser sessions, exchanged for a token at `POST /ui/session`. A valid
    /// session cookie authenticates a request as a read-only principal.
    pub sessions: super::session::SessionStore,
}

impl AuthState {
    /// Build auth state from a token store and optional service token.
    pub fn new(tokens: TokenStore, service_token: Option<String>) -> Self {
        Self {
            tokens,
            service_token,
            sessions: super::session::SessionStore::new(),
        }
    }
}

/// The result of authenticating a request.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// The name of the authenticated token.
    pub token_name: String,
    /// The role granted by the token.
    pub role: ApiRole,
    /// The scoped app names (if any).
    pub scoped_apps: Option<Vec<String>>,
    /// The scoped namespaces (if any).
    pub scoped_namespaces: Option<Vec<String>>,
}

/// Extract the Bearer token from an Authorization header value.
fn extract_bearer(header_value: &str) -> Option<&str> {
    header_value.strip_prefix("Bearer ")
}

/// Authenticate a request by validating the Bearer token.
///
/// Returns the `AuthContext` if the token is valid, or an HTTP error response.
pub fn authenticate(
    plaintext_token: &str,
    tokens: &[ApiToken],
) -> Result<AuthContext, (StatusCode, String)> {
    // Try to find a matching token. If the hash matches but the token
    // is expired, we want to report "expired" specifically.
    let stored = match token::find_valid_token(plaintext_token, tokens) {
        Ok(t) => t,
        Err(_) => {
            // Check if any token's hash matches but is expired
            for t in tokens {
                // If the hash matches (ignoring expiry), it's an expired token
                if let Some(expires) = t.expires_at
                    && std::time::SystemTime::now() > expires
                {
                    // Could be this token — check hash by creating temp non-expired copy
                    let mut temp = t.clone();
                    temp.expires_at = None;
                    if token::validate_token(plaintext_token, &temp).is_ok() {
                        return Err((StatusCode::UNAUTHORIZED, "token expired".to_string()));
                    }
                }
            }
            return Err((StatusCode::UNAUTHORIZED, "invalid token".to_string()));
        }
    };

    Ok(AuthContext {
        token_name: stored.name.clone(),
        role: stored.role,
        scoped_apps: stored.scope.apps.clone(),
        scoped_namespaces: stored.scope.namespaces.clone(),
    })
}

/// Check that the authenticated context has sufficient role.
pub fn require_role(ctx: &AuthContext, required: ApiRole) -> Result<(), (StatusCode, String)> {
    token::check_role(ctx.role, required).map_err(|_| {
        (
            StatusCode::FORBIDDEN,
            format!("insufficient permissions: requires {} role", required),
        )
    })
}

/// The `AuthContext` for the internal service principal (Admin-equivalent).
fn system_context() -> AuthContext {
    AuthContext {
        token_name: SYSTEM_PRINCIPAL.to_string(),
        role: ApiRole::Admin,
        scoped_apps: None,
        scoped_namespaces: None,
    }
}

/// Constant-time comparison of two token strings, to avoid leaking the service
/// token byte-by-byte through response timing. The length is not secret (all
/// tokens share the fixed `rbrg_` + 64-hex shape), so an early length check is
/// fine; the byte comparison itself is constant-time.
pub(crate) fn tokens_equal(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Axum middleware that enforces Bearer token authentication.
///
/// Bypasses when no user tokens are configured (the bootstrap window: a fresh
/// cluster is open until the operator creates the first token). The internal
/// service token, if present, is accepted as the system principal and does not
/// count towards that check. Route-level exemptions (health, UI, JWKS) are
/// handled by the router, not here.
///
/// Inserts an `AuthContext` extension into the request on success.
pub async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AuthState>,
    mut request: Request,
    next: Next,
) -> Response {
    let bearer = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(extract_bearer);

    // The service token is a system principal, accepted even during the
    // bootstrap window so internal fan-out works from the first moment.
    if let (Some(bearer), Some(service)) = (bearer, state.service_token.as_deref())
        && tokens_equal(bearer, service)
    {
        request.extensions_mut().insert(system_context());
        return next.run(request).await;
    }

    let tokens = state.tokens.read().await;

    // No user tokens configured yet: allow all (pre-init / single-node mode).
    if tokens.is_empty() {
        return next.run(request).await;
    }

    if let Some(bearer_token) = bearer {
        return match authenticate(bearer_token, &tokens) {
            Ok(ctx) => {
                request.extensions_mut().insert(ctx);
                next.run(request).await
            }
            Err((status, msg)) => (status, msg).into_response(),
        };
    }

    // No bearer: fall back to a browser session cookie. A valid session
    // authenticates as a read-only principal (see `session` module).
    let session_id = request
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(super::session::session_id_from_cookie_header)
        .map(str::to_string);
    if let Some(id) = session_id
        && let Some(token_name) = state.sessions.validate(&id).await
    {
        request
            .extensions_mut()
            .insert(readonly_session_context(&token_name));
        return next.run(request).await;
    }

    // Unauthenticated. A browser navigating to a page gets redirected to the
    // login form; an API client gets a plain 401.
    if wants_html(&request) {
        return axum::response::Redirect::to("/ui/login").into_response();
    }
    (StatusCode::UNAUTHORIZED, "missing authorization header").into_response()
}

/// The `AuthContext` for a browser session: always read-only, so a request
/// riding a stolen or forged cookie can read the dashboard but never mutate.
fn readonly_session_context(token_name: &str) -> AuthContext {
    AuthContext {
        token_name: token_name.to_string(),
        role: ApiRole::ReadOnly,
        scoped_apps: None,
        scoped_namespaces: None,
    }
}

/// Whether the request prefers an HTML response (a browser navigation).
fn wants_html(request: &Request) -> bool {
    request
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"))
}

/// Role check for handlers that receive an optional `AuthContext`.
///
/// `None` (the bootstrap window, where the middleware inserted no context) is
/// allowed — enforcement only kicks in once user tokens exist. Otherwise the
/// context's role must satisfy `required`.
#[allow(clippy::result_large_err)]
pub fn authorize(ctx: Option<&AuthContext>, required: ApiRole) -> Result<(), Response> {
    match ctx {
        None => Ok(()),
        Some(ctx) => {
            require_role(ctx, required).map_err(|(status, msg)| (status, msg).into_response())
        }
    }
}

/// Require the internal **system principal** — the caller must have presented
/// the cluster service token.
///
/// This guards node-to-node endpoints (batch/build run and report) that only
/// another cluster node should ever call. Unlike [`authorize`], it does **not**
/// pass through on `None`: a request that arrives with no authenticated
/// context (including the bootstrap window) is refused, because these routes
/// are never part of first-run setup — they exist only once a cluster, and its
/// service token, exist.
#[allow(clippy::result_large_err)]
pub fn require_system(ctx: Option<&AuthContext>) -> Result<(), Response> {
    match ctx {
        Some(ctx) if ctx.token_name == SYSTEM_PRINCIPAL => Ok(()),
        _ => Err((
            StatusCode::FORBIDDEN,
            "internal endpoint: cluster node identity required",
        )
            .into_response()),
    }
}

/// Build a GET request builder, attaching a `Bearer` token when present.
///
/// Cross-node fan-out uses this to present the internal service token to peers,
/// so their auth layer accepts the call as the system principal.
pub fn bearer_get(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
) -> reqwest::RequestBuilder {
    match token {
        Some(t) => client.get(url).bearer_auth(t),
        None => client.get(url),
    }
}

/// Helper to extract the `AuthContext` from a request's extensions.
///
/// Returns `None` in pre-init mode (when no tokens are configured
/// and auth was skipped).
pub fn get_auth_context(extensions: &axum::http::Extensions) -> Option<&AuthContext> {
    extensions.get::<AuthContext>()
}

/// Route-level role check. Returns 403 if the token doesn't have
/// the required role.
#[allow(clippy::result_large_err)]
pub fn check_route_role(
    extensions: &axum::http::Extensions,
    required: ApiRole,
) -> Result<(), Response> {
    // If no auth context (pre-init mode), allow
    let Some(ctx) = get_auth_context(extensions) else {
        return Ok(());
    };
    require_role(ctx, required).map_err(|(status, msg)| (status, msg).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::token::create_token;
    use crate::sesame::types::TokenScope;
    use std::time::{Duration, SystemTime};

    #[test]
    fn authenticate_valid_token() {
        let created = create_token("test", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let tokens = vec![created.token.clone()];

        let ctx = authenticate(&created.plaintext, &tokens).unwrap();
        assert_eq!(ctx.token_name, "test");
        assert_eq!(ctx.role, ApiRole::Admin);
    }

    #[test]
    fn authenticate_wrong_token_returns_401() {
        let created = create_token("test", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let tokens = vec![created.token];

        let err = authenticate("rbrg_wrong_token", &tokens).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn authenticate_expired_token_returns_401() {
        let expired = SystemTime::now() - Duration::from_secs(60);
        let created =
            create_token("test", ApiRole::Admin, TokenScope::default(), Some(expired)).unwrap();
        let tokens = vec![created.token];

        let err = authenticate(&created.plaintext, &tokens).unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
        assert!(err.1.contains("expired"));
    }

    #[test]
    fn require_role_admin_passes() {
        let ctx = AuthContext {
            token_name: "admin-token".to_string(),
            role: ApiRole::Admin,
            scoped_apps: None,
            scoped_namespaces: None,
        };
        require_role(&ctx, ApiRole::Admin).unwrap();
        require_role(&ctx, ApiRole::Deployer).unwrap();
        require_role(&ctx, ApiRole::ReadOnly).unwrap();
    }

    #[test]
    fn require_role_readonly_rejects_deployer() {
        let ctx = AuthContext {
            token_name: "ro-token".to_string(),
            role: ApiRole::ReadOnly,
            scoped_apps: None,
            scoped_namespaces: None,
        };
        let err = require_role(&ctx, ApiRole::Deployer).unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn extract_bearer_token() {
        assert_eq!(extract_bearer("Bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer("Basic abc123"), None);
        assert_eq!(extract_bearer("bearer abc123"), None);
    }

    #[test]
    fn authorize_allows_when_no_context_present() {
        assert!(authorize(None, ApiRole::Admin).is_ok());
    }

    #[test]
    fn authorize_allows_system_principal_for_admin_routes() {
        let ctx = system_context();
        assert!(authorize(Some(&ctx), ApiRole::Admin).is_ok());
    }

    #[test]
    fn authorize_rejects_deployer_context_on_admin_route() {
        let ctx = AuthContext {
            token_name: "dep".to_string(),
            role: ApiRole::Deployer,
            scoped_apps: None,
            scoped_namespaces: None,
        };
        let err = authorize(Some(&ctx), ApiRole::Admin).unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    // --- middleware behaviour ---

    use axum::body::Body;
    use axum::routing::get;
    use axum::{Router, middleware::from_fn_with_state};
    use tower::ServiceExt as _;

    fn guarded_router(state: AuthState) -> Router {
        Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer(from_fn_with_state(state, auth_middleware))
    }

    async fn status_of(router: Router, header: Option<&str>) -> StatusCode {
        let mut req = axum::http::Request::builder().uri("/x");
        if let Some(h) = header {
            req = req.header("authorization", h);
        }
        router
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// Send a request with the given headers, returning the whole response.
    async fn respond(router: Router, headers: &[(&str, &str)]) -> Response {
        let mut req = axum::http::Request::builder().uri("/x");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        router
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn middleware_bypasses_when_no_user_tokens() {
        let state = AuthState::new(new_token_store(), Some("rbrg_service".to_string()));
        assert_eq!(status_of(guarded_router(state), None).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_accepts_the_service_token_as_system() {
        // A non-empty user store, so the bootstrap bypass is off.
        let user = create_token("u", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let state = AuthState::new(store, Some("rbrg_service".to_string()));
        let status = status_of(guarded_router(state), Some("Bearer rbrg_service")).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_rejects_an_unknown_bearer_once_a_user_token_exists() {
        let user = create_token("u", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let state = AuthState::new(store, Some("rbrg_service".to_string()));
        let status = status_of(guarded_router(state), Some("Bearer rbrg_nope")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn require_system_accepts_only_the_system_principal() {
        // The system principal passes.
        assert!(require_system(Some(&system_context())).is_ok());
        // A normal Admin user does not (it is not the internal principal).
        let admin = AuthContext {
            token_name: "alice".to_string(),
            role: ApiRole::Admin,
            scoped_apps: None,
            scoped_namespaces: None,
        };
        assert!(require_system(Some(&admin)).is_err());
        // The bootstrap window (no context) is refused too — internal
        // endpoints are never part of first-run setup.
        assert!(require_system(None).is_err());
    }

    #[tokio::test]
    async fn a_valid_session_cookie_authenticates_when_a_token_exists() {
        let user = create_token("u", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let state = AuthState::new(store, Some("rbrg_service".to_string()));
        let id = state.sessions.create("u").await;

        let cookie = format!("rb_session={id}");
        let status = respond(guarded_router(state), &[("cookie", &cookie)])
            .await
            .status();
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_unknown_session_cookie_is_rejected() {
        let user = create_token("u", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let state = AuthState::new(store, Some("rbrg_service".to_string()));

        let status = respond(guarded_router(state), &[("cookie", "rb_session=deadbeef")])
            .await
            .status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unauthenticated_html_request_redirects_to_login() {
        let user = create_token("u", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let state = AuthState::new(store, Some("rbrg_service".to_string()));

        let resp = respond(guarded_router(state), &[("accept", "text/html")]).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get("location").unwrap(), "/ui/login");
    }

    #[test]
    fn bearer_get_sets_authorization_when_token_present() {
        let client = reqwest::Client::new();
        let req = bearer_get(&client, "http://x/v1/status", Some("rbrg_abc"))
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("authorization").unwrap(),
            "Bearer rbrg_abc"
        );
    }

    #[test]
    fn bearer_get_omits_authorization_when_token_absent() {
        let client = reqwest::Client::new();
        let req = bearer_get(&client, "http://x/v1/status", None)
            .build()
            .unwrap();
        assert!(req.headers().get("authorization").is_none());
    }
}
