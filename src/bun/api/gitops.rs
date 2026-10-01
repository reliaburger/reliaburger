//! The GitOps webhook route.

use super::*;

/// GitOps webhook handler (public, HMAC-authenticated).
///
/// Accepts POST from git hosting providers (GitHub, GitLab, Gitea). The
/// route is public because providers can't present a Reliaburger bearer
/// token, so the request is authenticated here instead: the HMAC-SHA256
/// signature over the raw body must match the `[gitops] webhook_secret`,
/// the delivery id must not be a replay, and the rate limit must not be
/// exceeded (GIT3). Only then is the sync loop nudged.
///
/// Returns 202 on success, 401 on a bad/missing signature or replay, 429
/// when rate-limited, and 503 when GitOps or the webhook secret isn't
/// configured.
pub(super) async fn gitops_webhook_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(tx) = &state.gitops_webhook_tx else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gitops not configured" })),
        )
            .into_response();
    };

    // Fail closed: without a configured secret we can't authenticate the
    // caller, and a public unauthenticated trigger is a DoS lever.
    let Some(validator) = &state.gitops_webhook_validator else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "webhook secret not configured" })),
        )
            .into_response();
    };

    // Reserve before recording the delivery ID: a full queue must remain
    // retryable, and a closed receiver must never produce a success response.
    let permit = match tx.try_reserve() {
        Ok(permit) => permit,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("gitops sync queue unavailable: {error}")
                })),
            )
                .into_response();
        }
    };

    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    let gitlab_token = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok());
    let delivery_id = header_delivery_id(&headers);
    let branch = header_branch(&headers).unwrap_or_else(|| "main".to_string());

    let mut guard = validator.lock().await;
    let result = if let Some(token) = gitlab_token {
        // GitLab sends the shared secret verbatim in `X-Gitlab-Token`.
        guard.validate_gitlab(&body, token, delivery_id.as_deref(), &branch)
    } else {
        // GitHub/Gitea sign the body: `X-Hub-Signature-256: sha256=<hex>`.
        guard.validate(&body, signature, delivery_id.as_deref(), &branch)
    };
    drop(guard);

    match result {
        Ok(_) => {
            permit.send(());
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "message": "sync queued" })),
            )
                .into_response()
        }
        Err(e) => {
            let message = e.to_string();
            // A rate-limit rejection is a 429; everything else (bad
            // signature, replay, missing header) is a 401.
            let code = if message.contains("rate limit") {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::UNAUTHORIZED
            };
            (code, Json(serde_json::json!({ "error": message }))).into_response()
        }
    }
}

/// The provider's delivery id header, if any (GitHub / Gitea).
pub(super) fn header_delivery_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-github-delivery")
        .or_else(|| headers.get("x-gitlab-event-uuid"))
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// The branch a push targeted, parsed from `X-Reliaburger-Branch` if the
/// caller set it. Providers don't send the branch in a header, so this is
/// advisory; the sync loop tracks the configured branch regardless.
pub(super) fn header_branch(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-reliaburger-branch")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}
