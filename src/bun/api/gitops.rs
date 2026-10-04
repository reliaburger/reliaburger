//! The GitOps webhook route.

use super::*;

const WEBHOOK_FORWARDED_HEADER: &str = "x-reliaburger-webhook-forwarded";
const WEBHOOK_ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn webhook_unavailable(message: impl ToString) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": message.to_string()})),
    )
        .into_response()
}

/// Public provider-authenticated GitOps admission. Standalone requests reserve
/// queue capacity; cluster requests return 202 only after replicated admission.
/// Followers preserve the provider's original body and authentication headers.
/// Failed or canceled admissions release local replay/rate reservations.
pub(super) async fn gitops_webhook_handler(
    State(state): State<ApiState>,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // The admission budget includes validator contention and leader metadata,
    // as well as forwarding/replication. Canceling the inner future drops its
    // reservation guard before releasing the validator, so retries remain safe.
    match tokio::time::timeout(
        WEBHOOK_ADMISSION_TIMEOUT,
        admit_gitops_webhook(state, directory, headers, body),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => webhook_unavailable("webhook admission timed out; retry shortly"),
    }
}

async fn admit_gitops_webhook(
    state: ApiState,
    directory: Option<axum::Extension<LeaderDirectory>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    use sha2::{Digest, Sha256};
    let Some(tx) = &state.gitops_webhook_tx else {
        return webhook_unavailable("gitops not configured");
    };
    let Some(validator) = &state.gitops_webhook_validator else {
        return webhook_unavailable("webhook secret not configured");
    };
    // Queue admission remains the standalone contract. Cluster admission is
    // replicated; the bounded local channel is only a best-effort wakeup.
    let permit = if state.council.is_none() {
        match tx.try_reserve() {
            Ok(permit) => Some(permit),
            Err(error) => {
                return webhook_unavailable(format!("gitops sync queue unavailable: {error}"));
            }
        }
    } else {
        None
    };
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok());
    let gitlab_token = headers
        .get("x-gitlab-token")
        .and_then(|value| value.to_str().ok());
    let delivery = header_delivery_id(&headers);
    let branch = header_branch(&headers).unwrap_or_else(|| "main".to_owned());
    let mut validator = validator.lock().await;
    let admission =
        match validator.reserve(&body, signature, gitlab_token, delivery.as_deref(), &branch) {
            Ok(admission) => admission,
            Err(error) => {
                let status = if error.to_string().contains("rate limit") {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::UNAUTHORIZED
                };
                return (
                    status,
                    Json(serde_json::json!({"error": error.to_string()})),
                )
                    .into_response();
            }
        };
    if let Some(council) = &state.council {
        let leads = {
            let metrics = council.metrics();
            let metrics = metrics.borrow();
            metrics.current_leader == Some(metrics.id)
        };
        if !leads {
            if headers.contains_key(WEBHOOK_FORWARDED_HEADER) {
                return webhook_unavailable(
                    "webhook leader changed; retry after the election settles",
                );
            }
            let advertised =
                directory
                    .as_ref()
                    .and_then(|axum::Extension(LeaderDirectory(directory))| {
                        let metrics = council.metrics();
                        let metrics = metrics.borrow();
                        crate::cluster::directory::leader_api_address(&metrics, &directory.borrow())
                    });
            let leader = match advertised {
                Some(address) => state.cluster_http.url(&address.to_string(), ""),
                None => match leader_api_url(&state, council).await {
                    Some(leader) => leader,
                    None => {
                        return webhook_unavailable("no cluster leader known yet; retry shortly");
                    }
                },
            };
            let mut request = state
                .cluster_http
                .client()
                .post(format!("{leader}/v1/gitops/webhook"))
                .header(WEBHOOK_FORWARDED_HEADER, "1")
                .body(body);
            // Preserve the raw body and provider credentials. The receiving
            // leader authenticates the same bytes; no service token substitutes.
            for name in [
                "x-hub-signature-256",
                "x-gitlab-token",
                "x-github-delivery",
                "x-gitlab-event-uuid",
                "x-reliaburger-branch",
                "content-type",
            ] {
                if let Some(value) = headers.get(name) {
                    request = request.header(name, value.clone());
                }
            }
            let exchange = async {
                let response = request.send().await?;
                let status = response.status();
                let bytes = response.bytes().await?;
                Ok::<_, reqwest::Error>((status, bytes))
            };
            return match tokio::time::timeout(WEBHOOK_ADMISSION_TIMEOUT, exchange).await {
                Ok(Ok((status, bytes))) => {
                    if status == reqwest::StatusCode::ACCEPTED {
                        admission.commit();
                    }
                    (
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        bytes,
                    )
                        .into_response()
                }
                Ok(Err(error)) => {
                    webhook_unavailable(format!("webhook leader unavailable: {error}"))
                }
                Err(_) => webhook_unavailable("webhook leader admission timed out; retry shortly"),
            };
        }
        if tx.is_closed() {
            return webhook_unavailable("gitops sync loop closed");
        }
        let Some(delivery) = delivery.as_deref() else {
            return webhook_unavailable("validated webhook has no delivery ID");
        };
        let receipt = Sha256::digest(delivery.as_bytes());
        let write = council.write(crate::council::types::RaftRequest::GitOpsSyncRequested {
            delivery: receipt.into(),
        });
        match tokio::time::timeout(WEBHOOK_ADMISSION_TIMEOUT, write).await {
            Ok(Ok(crate::council::types::CouncilResponse::GitOpsSyncRequested { generation })) => {
                admission.commit();
                let _ = tx.try_send(());
                return (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({"message": "sync admitted", "generation": generation})),
                )
                    .into_response();
            }
            Ok(Ok(crate::council::types::CouncilResponse::Refused { reason })) => {
                let status = if reason.contains("replay") {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                return (status, Json(serde_json::json!({"error": reason}))).into_response();
            }
            Ok(Ok(_)) => return webhook_unavailable("unexpected webhook admission response"),
            Ok(Err(error)) => {
                return webhook_unavailable(format!("webhook admission failed: {error}"));
            }
            Err(_) => return webhook_unavailable("webhook admission timed out; retry shortly"),
        }
    }
    let Some(permit) = permit else {
        return webhook_unavailable("standalone webhook queue reservation missing");
    };
    permit.send(());
    admission.commit();
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"message": "sync queued"})),
    )
        .into_response()
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
