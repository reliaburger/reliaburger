//! API authentication middleware.
//!
//! Validates Bearer tokens from the `Authorization` header against
//! stored API tokens. Each request is checked for role and scope.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tokio::sync::{RwLock, Semaphore};

use super::token;
use super::types::{ApiRole, ApiToken, TokenScope};

/// Ceiling on concurrent Argon2 verifications across the whole process.
///
/// Argon2id is deliberately slow and memory-hungry, so a flood of bad bearers
/// hashing in parallel would otherwise pin every CPU and exhaust memory (a
/// trivial denial of service). We admit at most this many verifications at a
/// time; the rest wait for a permit. Small enough to stay cheap, large enough
/// that honest concurrent callers aren't serialised behind one another.
const MAX_CONCURRENT_VERIFICATIONS: usize = 4;

/// Process-wide permit pool bounding concurrent Argon2 work (AUTH5).
///
/// `LazyLock` initialises this the first time it's read and never again, so
/// every request shares one semaphore. It replaces the old behaviour where
/// verification ran synchronously while the token-store read lock was held,
/// serialising every request behind one another's hashing.
static VERIFY_PERMITS: std::sync::LazyLock<Semaphore> =
    std::sync::LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_VERIFICATIONS));

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

/// Verifies workload-identity JWTs presented as bearer tokens (PKI10).
///
/// The cluster mints these for its workloads (SPIFFE-style, signed with the
/// OIDC key); this is the consumer that lets a workload authenticate to the API
/// as itself. Verification is the constrained path — algorithm, key id, issuer,
/// audience, and `iat` are all checked — so a token minted for another cluster
/// or purpose is refused.
#[derive(Clone)]
pub struct WorkloadJwtVerifier {
    config: Arc<super::types::OidcSigningConfig>,
    constraints: super::oidc::JwtConstraints,
}

impl WorkloadJwtVerifier {
    /// Build a verifier from the cluster's OIDC config and trust-domain name.
    pub fn new(config: super::types::OidcSigningConfig, cluster_name: &str) -> Self {
        let constraints = super::oidc::JwtConstraints::for_config(&config, cluster_name);
        Self {
            config: Arc::new(config),
            constraints,
        }
    }

    /// Return the verified claims if `token` is a valid workload JWT for this
    /// cluster, else `None`.
    pub fn verify(&self, token: &str) -> Option<super::types::WorkloadJwtClaims> {
        super::oidc::verify_jwt_with_constraints(token, &self.config, &self.constraints).ok()
    }
}

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
    /// Verifier for workload-identity JWT bearers. `None` disables JWT auth
    /// (single-node / pre-OIDC), leaving the token and session paths unchanged.
    pub jwt_verifier: Option<Arc<WorkloadJwtVerifier>>,
}

impl AuthState {
    /// Build auth state from a token store and optional service token.
    pub fn new(tokens: TokenStore, service_token: Option<String>) -> Self {
        Self {
            tokens,
            service_token,
            sessions: super::session::SessionStore::new(),
            jwt_verifier: None,
        }
    }

    /// Attach a workload-JWT verifier, enabling JWT bearer authentication.
    pub fn with_jwt_verifier(mut self, verifier: WorkloadJwtVerifier) -> Self {
        self.jwt_verifier = Some(Arc::new(verifier));
        self
    }
}

/// The result of authenticating a request.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// The name of the authenticated token.
    pub token_name: String,
    /// Stable identifier for this exact credential, independent of its
    /// human-readable name.
    pub principal_id: String,
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

    // Defence in depth (AUTH4): the system principal is a side-channel
    // credential the middleware accepts directly (see `auth_middleware`), never
    // a stored user token. If a token named `__system` ever reaches the store —
    // through a future write path that skips `create_token`'s guard — it must
    // still not authenticate *as* the system principal, or it would clear
    // `require_system` and skip `authorize_scoped`/`require_unscoped`.
    if stored.name == SYSTEM_PRINCIPAL {
        return Err((StatusCode::UNAUTHORIZED, "invalid token".to_string()));
    }

    Ok(AuthContext {
        token_name: stored.name.clone(),
        principal_id: token_principal_id(stored),
        role: stored.role,
        scoped_apps: stored.scope.apps.clone(),
        scoped_namespaces: stored.scope.namespaces.clone(),
    })
}

fn token_principal_id(token: &ApiToken) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, &token.token_hash);
    format!("token:{}", hex::encode(digest.as_ref()))
}

/// Authenticate a bearer without holding the token-store lock (AUTH5).
///
/// Takes ownership of a cloned token list (the caller already released the
/// read lock). A bearer that doesn't even look like one of our tokens is
/// rejected by a string check, so a flood of junk never reaches Argon2. The
/// real verification runs on a blocking thread under a process-wide semaphore,
/// so concurrent bad bearers can't pin every core hashing in parallel.
///
/// `async fn` returning `Result` is Rust's way of saying "this may await and
/// may fail"; `.await` on the `spawn_blocking` handle yields until the
/// blocking thread finishes without parking the async worker.
async fn authenticate_off_lock(
    plaintext: &str,
    tokens: Vec<ApiToken>,
) -> Result<AuthContext, (StatusCode, String)> {
    // Cheap shape check first: reject obviously-malformed bearers before we
    // spend an Argon2 hash on them (the AUTH5 short-circuit index).
    if !token::looks_like_token(plaintext) {
        return Err((StatusCode::UNAUTHORIZED, "invalid token".to_string()));
    }

    // Bound concurrent Argon2 work. The semaphore is a `static`, so its
    // permits borrow for `'static` and can cross into the blocking closure.
    let permit = match VERIFY_PERMITS.acquire().await {
        Ok(permit) => permit,
        // The semaphore is a `static` we never close, so this is unreachable
        // in practice; treat a closed semaphore as a transient failure.
        Err(_) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "authentication temporarily unavailable".to_string(),
            ));
        }
    };

    let candidate = plaintext.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        authenticate(&candidate, &tokens)
    })
    .await;

    match result {
        Ok(inner) => inner,
        // The blocking task panicked; don't leak the panic, fail closed.
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "authentication failed".to_string(),
        )),
    }
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
        principal_id: SYSTEM_PRINCIPAL.to_string(),
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

    // Snapshot the token store under the read lock, then drop it (AUTH5).
    //
    // The old code held this read lock across the whole Argon2 verification.
    // Argon2 is slow on purpose, so every authenticated request serialised
    // behind every other one's hashing, and a Raft-driven token refresh (a
    // writer) queued behind them all. We clone the small `Vec<ApiToken>` out
    // and release the lock immediately; the hashing then happens off-lock and
    // under a concurrency bound.
    let tokens = {
        let guard = state.tokens.read().await;
        guard.clone()
    };

    // No user tokens configured yet: allow all (pre-init / single-node mode).
    if tokens.is_empty() {
        return next.run(request).await;
    }

    if let Some(bearer_token) = bearer {
        // A workload-identity JWT (three dot-separated parts) authenticates the
        // presenting workload as itself, confined read-only to its own
        // app/namespace. Tried before the Argon2 token path because a JWT can
        // never match an `rbrg_` hash, and skipped when no verifier is set.
        if let Some(verifier) = &state.jwt_verifier
            && looks_like_jwt(bearer_token)
            && let Some(claims) = verifier.verify(bearer_token)
        {
            request.extensions_mut().insert(workload_context(&claims));
            return next.run(request).await;
        }
        return match authenticate_off_lock(bearer_token, tokens).await {
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
        && let Some(identity) = state.sessions.validate(&id).await
    {
        request
            .extensions_mut()
            .insert(readonly_session_context(&identity));
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
///
/// The originating token's **scope** is preserved (C3): a token confined to one
/// app/namespace stays confined when it rides a session cookie, so exchanging
/// it at `/ui/session` cannot widen it to cluster-wide reads.
fn readonly_session_context(identity: &super::session::SessionIdentity) -> AuthContext {
    AuthContext {
        token_name: identity.token_name.clone(),
        principal_id: format!("session:{}", identity.token_name),
        role: ApiRole::ReadOnly,
        scoped_apps: identity.scope.apps.clone(),
        scoped_namespaces: identity.scope.namespaces.clone(),
    }
}

/// Cheap shape check: does the bearer look like a compact JWS (three
/// non-empty dot-separated segments)? Used only to route a bearer to the JWT
/// verifier instead of the Argon2 token path; the real check is the signature.
fn looks_like_jwt(candidate: &str) -> bool {
    let mut parts = candidate.split('.');
    let ok = matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), Some(c)) if !a.is_empty() && !b.is_empty() && !c.is_empty());
    ok && parts.next().is_none()
}

/// The `AuthContext` for a verified workload-identity JWT: read-only and
/// confined to the workload's own app and namespace, so a workload can read its
/// own resources through the API but never mutate or reach another tenant's.
fn workload_context(claims: &super::types::WorkloadJwtClaims) -> AuthContext {
    AuthContext {
        token_name: format!("workload:{}/{}", claims.namespace, claims.app),
        principal_id: claims.sub.clone(),
        role: ApiRole::ReadOnly,
        scoped_apps: Some(vec![claims.app.clone()]),
        scoped_namespaces: Some(vec![claims.namespace.clone()]),
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

/// Like [`authorize`], but refuses the internal **system principal** (AUTH4).
///
/// The service token (`__system`) exists only for node-to-node fan-out, which
/// lands on `System`-tagged routes guarded by [`require_system`]. A shared,
/// cluster-wide Admin credential handed to every node is a lateral-movement
/// risk: steal it off one node and you can mint user tokens or rotate secrets
/// on the whole cluster. So the genuinely user-facing management routes (token
/// create/list/revoke, secret rotate, identity sign, chaos, fault) authorise
/// through this helper, which grants the role check to real users but never to
/// `__system`. Node fan-out doesn't call these routes, so nothing breaks.
#[allow(clippy::result_large_err)]
pub fn authorize_user(ctx: Option<&AuthContext>, required: ApiRole) -> Result<(), Response> {
    if let Some(ctx) = ctx
        && ctx.token_name == SYSTEM_PRINCIPAL
    {
        return Err((
            StatusCode::FORBIDDEN,
            "the internal service principal may not call user-management routes",
        )
            .into_response());
    }
    authorize(ctx, required)
}

/// Enforce a token's app/namespace scope on a specific target (AUTH1).
///
/// The role check answers "may this caller deploy at all?"; this answers "may
/// this caller deploy *this app in this namespace*?". A `Deployer` scoped to
/// namespace `a` presenting a request against namespace `b` clears the role
/// gate but fails here with 403. An unscoped token (no `apps`/`namespaces`
/// restriction) allows everything, so the common case is unaffected.
///
/// Call it *after* the role check in every handler that mutates a named
/// app/namespace. Pre-init requests (`None`) and the system principal pass:
/// the former has no scope to enforce, the latter is already confined to
/// node-to-node routes by [`authorize_user`].
#[allow(clippy::result_large_err)]
pub fn authorize_scoped(
    ctx: Option<&AuthContext>,
    app: &str,
    namespace: &str,
) -> Result<(), Response> {
    let Some(ctx) = ctx else {
        return Ok(());
    };
    if ctx.token_name == SYSTEM_PRINCIPAL {
        return Ok(());
    }
    let scope = TokenScope {
        apps: ctx.scoped_apps.clone(),
        namespaces: ctx.scoped_namespaces.clone(),
    };
    if scope.allows(app, namespace) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            format!("token scope does not allow {app} in namespace {namespace}"),
        )
            .into_response())
    }
}

/// Enforce a principal's `[permission]` spec on a specific action + target.
///
/// Permissions are an **additional** allow-list keyed by token name, layered on
/// top of the role and scope checks — they can restrict a principal to named
/// actions/apps/namespaces but never grant beyond its role. The rules:
///
/// - A pre-init request (`None`) or the system principal passes untouched.
/// - A principal with **no** spec is governed by role + scope alone, so
///   permissions are opt-in: defining none preserves today's behaviour.
/// - A principal **with** a spec must have it grant `(action, app, namespace)`,
///   or the request is refused with 403.
///
/// Call it *after* the role and scope checks in a gated handler, passing the
/// replicated permission map (`DesiredState.permissions`).
#[allow(clippy::result_large_err)]
pub fn authorize_permission(
    ctx: Option<&AuthContext>,
    action: crate::config::PermissionAction,
    app: &str,
    namespace: &str,
    permissions: &std::collections::BTreeMap<String, crate::config::PermissionSpec>,
) -> Result<(), Response> {
    let Some(ctx) = ctx else {
        return Ok(());
    };
    if ctx.token_name == SYSTEM_PRINCIPAL {
        return Ok(());
    }
    let Some(spec) = permissions.get(&ctx.token_name) else {
        return Ok(());
    };
    if spec.allows(action, app, namespace) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            format!(
                "permission for {:?} does not grant {} on {app} in namespace {namespace}",
                ctx.token_name,
                action.as_str()
            ),
        )
            .into_response())
    }
}

/// Require a token whose scope covers the **whole cluster**.
///
/// Some endpoints take no app or namespace to check a scope against —
/// `/v1/logs/sql` runs operator-supplied SQL over the node's entire `logs`
/// table. There is no safe way to rewrite an arbitrary query into a
/// tenant-filtered one, so rather than pretend, a scoped token is refused
/// outright and pointed at the per-app endpoint that *can* filter (C3).
///
/// An unauthenticated context passes through for the same reason
/// [`authorize`] does: the bootstrap window is handled by the middleware,
/// not here.
#[allow(clippy::result_large_err)]
pub fn require_unscoped(ctx: Option<&AuthContext>) -> Result<(), Response> {
    let Some(ctx) = ctx else {
        return Ok(());
    };
    if ctx.token_name == SYSTEM_PRINCIPAL {
        return Ok(());
    }
    if ctx.scoped_apps.is_none() && ctx.scoped_namespaces.is_none() {
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        "this endpoint reads across every app and namespace, so a scoped token \
         cannot use it — query /v1/logs/query/{app}/{namespace} instead",
    )
        .into_response())
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
            principal_id: "token:admin".to_string(),
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
            principal_id: "token:readonly".to_string(),
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
            principal_id: "token:deployer".to_string(),
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

    // --- AUTH1 scope enforcement ---

    fn scoped_ctx(apps: Option<Vec<&str>>, namespaces: Option<Vec<&str>>) -> AuthContext {
        AuthContext {
            token_name: "scoped".to_string(),
            principal_id: "token:scoped".to_string(),
            role: ApiRole::Deployer,
            scoped_apps: apps.map(|a| a.into_iter().map(String::from).collect()),
            scoped_namespaces: namespaces.map(|n| n.into_iter().map(String::from).collect()),
        }
    }

    #[test]
    fn scoped_deployer_is_refused_outside_its_namespace() {
        let ctx = scoped_ctx(None, Some(vec!["a"]));
        let err = authorize_scoped(Some(&ctx), "web", "b").unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn scoped_deployer_is_allowed_inside_its_namespace() {
        let ctx = scoped_ctx(None, Some(vec!["a"]));
        assert!(authorize_scoped(Some(&ctx), "web", "a").is_ok());
    }

    #[test]
    fn scoped_deployer_is_refused_outside_its_apps() {
        let ctx = scoped_ctx(Some(vec!["web"]), None);
        let err = authorize_scoped(Some(&ctx), "api", "default").unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn unscoped_token_is_allowed_everywhere() {
        let ctx = scoped_ctx(None, None);
        assert!(authorize_scoped(Some(&ctx), "anything", "anywhere").is_ok());
    }

    #[test]
    fn pre_init_and_system_pass_the_scope_check() {
        assert!(authorize_scoped(None, "web", "a").is_ok());
        assert!(authorize_scoped(Some(&system_context()), "web", "a").is_ok());
    }

    // --- AUTH4 system principal restriction ---

    #[test]
    fn system_principal_is_refused_on_user_management_routes() {
        let err = authorize_user(Some(&system_context()), ApiRole::Admin).unwrap_err();
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn real_admin_still_passes_user_management_routes() {
        let admin = AuthContext {
            token_name: "alice".to_string(),
            principal_id: "token:alice".to_string(),
            role: ApiRole::Admin,
            scoped_apps: None,
            scoped_namespaces: None,
        };
        assert!(authorize_user(Some(&admin), ApiRole::Admin).is_ok());
    }

    #[test]
    fn require_system_accepts_only_the_system_principal() {
        // The system principal passes.
        assert!(require_system(Some(&system_context())).is_ok());
        // A normal Admin user does not (it is not the internal principal).
        let admin = AuthContext {
            token_name: "alice".to_string(),
            principal_id: "token:alice".to_string(),
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
    async fn a_valid_bearer_authenticates_through_the_off_lock_path() {
        let user = create_token("u", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let state = AuthState::new(store, None);
        let header = format!("Bearer {}", user.plaintext);
        let status = status_of(guarded_router(state), Some(&header)).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_burst_of_invalid_bearers_does_not_hold_the_store_lock() {
        // The whole point of AUTH5: bad bearers must not serialise on the
        // store lock. We hammer the middleware with junk bearers while a
        // writer keeps grabbing the write lock; if the middleware held the
        // read lock across Argon2, the writer would starve. Here it doesn't,
        // because the middleware clones out and hashes off-lock.
        let user = create_token("u", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let state = AuthState::new(store.clone(), None);

        // A writer that must make progress even under the auth burst.
        let writer_store = store.clone();
        let writer = tokio::spawn(async move {
            for _ in 0..20 {
                let _guard = writer_store.write().await;
                tokio::task::yield_now().await;
            }
        });

        // A burst of malformed bearers. `looks_like_token` rejects these
        // before Argon2, so they can't pile up on permits either.
        let mut requests = Vec::new();
        for i in 0..32 {
            let router = guarded_router(state.clone());
            let header = format!("Bearer rbrg_not_a_real_token_{i}");
            requests.push(tokio::spawn(async move {
                status_of(router, Some(&header)).await
            }));
        }

        // The writer completing promptly is the assertion that it wasn't
        // starved behind the auth burst.
        let writer_done = tokio::time::timeout(std::time::Duration::from_secs(5), writer).await;
        assert!(writer_done.is_ok(), "writer starved by the auth burst");

        for req in requests {
            assert_eq!(req.await.unwrap(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn a_valid_session_cookie_authenticates_when_a_token_exists() {
        let user = create_token("u", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let state = AuthState::new(store, Some("rbrg_service".to_string()));
        let id = state.sessions.create("u", TokenScope::default()).await;

        let cookie = format!("rb_session={id}");
        let status = respond(guarded_router(state), &[("cookie", &cookie)])
            .await
            .status();
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn a_stored_token_named_system_never_authenticates() {
        // Defence in depth (AUTH4): even if a token named `__system` reaches the
        // store through some future write path that skips `create_token`'s
        // guard, it must not authenticate as the system principal.
        let created = create_token("legit", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let mut smuggled = created.token.clone();
        smuggled.name = SYSTEM_PRINCIPAL.to_string();
        let result = authenticate(&created.plaintext, &[smuggled]);
        assert!(result.is_err(), "a stored __system token authenticated");
    }

    fn mint_test_workload_jwt(
        app: &str,
        namespace: &str,
    ) -> (String, super::super::types::OidcSigningConfig) {
        let ikm = b"test-oidc-wrapping-material-32b!".to_vec();
        let config =
            super::super::oidc::generate_oidc_keypair("https://test.reliaburger.dev", &ikm)
                .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = super::super::types::WorkloadJwtClaims {
            iss: "https://test.reliaburger.dev".to_string(),
            sub: format!("spiffe://test/ns/{namespace}/app/{app}"),
            aud: vec!["spiffe://test".to_string()],
            exp: now + 3600,
            iat: now,
            namespace: namespace.to_string(),
            app: app.to_string(),
            cluster: "test".to_string(),
            node: "node-01".to_string(),
            instance: format!("{app}-g1-0"),
        };
        let jwt = super::super::oidc::mint_jwt(&claims, &config, &ikm).unwrap();
        (jwt, config)
    }

    #[test]
    fn a_workload_jwt_verifies_to_a_scoped_read_only_context() {
        let (jwt, config) = mint_test_workload_jwt("api", "team-a");
        let verifier = WorkloadJwtVerifier::new(config, "test");

        let claims = verifier
            .verify(&jwt)
            .expect("valid workload JWT should verify");
        let ctx = workload_context(&claims);
        assert_eq!(ctx.role, ApiRole::ReadOnly);
        assert_eq!(ctx.scoped_apps.as_deref(), Some(&["api".to_string()][..]));
        assert_eq!(
            ctx.scoped_namespaces.as_deref(),
            Some(&["team-a".to_string()][..])
        );
        // Confined to its own app/namespace, refused on the cluster-wide path.
        assert!(authorize_scoped(Some(&ctx), "api", "team-a").is_ok());
        assert!(authorize_scoped(Some(&ctx), "api", "team-b").is_err());
        assert!(require_unscoped(Some(&ctx)).is_err());
    }

    #[test]
    fn a_workload_jwt_for_another_cluster_is_rejected() {
        let (jwt, config) = mint_test_workload_jwt("api", "team-a");
        // A verifier for a different trust domain expects a different audience.
        let verifier = WorkloadJwtVerifier::new(config, "other-cluster");
        assert!(verifier.verify(&jwt).is_none());
    }

    #[test]
    fn looks_like_jwt_distinguishes_jwts_from_bearer_tokens() {
        assert!(looks_like_jwt("aaa.bbb.ccc"));
        assert!(!looks_like_jwt("rbrg_deadbeef"));
        assert!(!looks_like_jwt("aaa.bbb"));
        assert!(!looks_like_jwt("aaa..ccc"));
        assert!(!looks_like_jwt("aaa.bbb.ccc.ddd"));
    }

    #[test]
    fn authorize_permission_enforces_specs_only_for_principals_that_have_one() {
        use crate::config::{PermissionAction, PermissionSpec};
        let ctx = AuthContext {
            token_name: "ci".to_string(),
            principal_id: "token:ci".to_string(),
            role: ApiRole::Deployer,
            scoped_apps: None,
            scoped_namespaces: None,
        };
        let mut permissions = std::collections::BTreeMap::new();

        // No spec for this principal → governed by role/scope alone (allow).
        assert!(
            authorize_permission(
                Some(&ctx),
                PermissionAction::Deploy,
                "web",
                "prod",
                &permissions
            )
            .is_ok()
        );

        // A spec that grants deploy on web/prod but nothing else.
        permissions.insert(
            "ci".to_string(),
            PermissionSpec {
                actions: vec!["deploy".to_string()],
                apps: vec!["web".to_string()],
                namespaces: Some(vec!["prod".to_string()]),
            },
        );
        assert!(
            authorize_permission(
                Some(&ctx),
                PermissionAction::Deploy,
                "web",
                "prod",
                &permissions
            )
            .is_ok()
        );
        // Wrong action / app / namespace are refused.
        assert!(
            authorize_permission(
                Some(&ctx),
                PermissionAction::Exec,
                "web",
                "prod",
                &permissions
            )
            .is_err()
        );
        assert!(
            authorize_permission(
                Some(&ctx),
                PermissionAction::Deploy,
                "api",
                "prod",
                &permissions
            )
            .is_err()
        );

        // Pre-init (None) and the system principal are never gated.
        assert!(
            authorize_permission(None, PermissionAction::Exec, "web", "prod", &permissions).is_ok()
        );
        let system = system_context();
        assert!(
            authorize_permission(
                Some(&system),
                PermissionAction::Exec,
                "web",
                "prod",
                &permissions
            )
            .is_ok()
        );
    }

    #[test]
    fn a_session_context_inherits_the_token_scope() {
        // A tenant-scoped token that exchanges itself for a session cookie must
        // stay confined — its session context carries the same scope (C3).
        let identity = super::super::session::SessionIdentity {
            token_name: "scoped".to_string(),
            scope: TokenScope {
                apps: Some(vec!["web".to_string()]),
                namespaces: Some(vec!["team-a".to_string()]),
            },
        };
        let ctx = readonly_session_context(&identity);
        assert_eq!(ctx.role, ApiRole::ReadOnly);
        assert_eq!(ctx.scoped_apps.as_deref(), Some(&["web".to_string()][..]));
        assert_eq!(
            ctx.scoped_namespaces.as_deref(),
            Some(&["team-a".to_string()][..])
        );
        // The scoped session is refused by the cluster-wide endpoints it must
        // not reach, and confined on the per-app ones.
        assert!(require_unscoped(Some(&ctx)).is_err());
        assert!(authorize_scoped(Some(&ctx), "web", "team-a").is_ok());
        assert!(authorize_scoped(Some(&ctx), "web", "team-b").is_err());
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
