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

/// Axum middleware that enforces Bearer token authentication.
///
/// Skips authentication for `/v1/health` (liveness probe) and
/// when the token store is empty (pre-init / single-node mode).
///
/// Inserts an `AuthContext` extension into the request on success.
pub async fn auth_middleware(
    axum::extract::State(store): axum::extract::State<TokenStore>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();

    // Skip auth for health check (liveness probes shouldn't need tokens)
    if path == "/v1/health" {
        return next.run(request).await;
    }

    let tokens = store.read().await;

    // If no tokens are configured, allow all requests (pre-init mode)
    if tokens.is_empty() {
        return next.run(request).await;
    }

    // Extract the Authorization header
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    let Some(header_value) = auth_header else {
        return (StatusCode::UNAUTHORIZED, "missing authorization header").into_response();
    };

    let Some(bearer_token) = extract_bearer(header_value) else {
        return (
            StatusCode::UNAUTHORIZED,
            "invalid authorization header format",
        )
            .into_response();
    };

    // Validate the token
    match authenticate(bearer_token, &tokens) {
        Ok(ctx) => {
            request.extensions_mut().insert(ctx);
            next.run(request).await
        }
        Err((status, msg)) => (status, msg).into_response(),
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
}
