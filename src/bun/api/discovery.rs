//! Service discovery routes: resolve and the routing table.
//!
//! Every container gets a workload JWT confined to its own app and namespace,
//! and these routes answer any authenticated caller, so each one filters its
//! answer to the caller's scope. An unscoped token still sees the whole
//! cluster.

use super::*;

/// Resolve a service name to its VIP and backends.
///
/// An unscoped caller gets the first match in any namespace, as the CLI
/// expects. A scoped caller gets the first match it may see, and a name it
/// can't see anywhere is simply not found.
pub(super) async fn resolve_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Response {
    let found = if crate::sesame::auth::require_unscoped(auth.as_deref()).is_ok() {
        ask_agent(&state.cmd_tx, |response| AgentCommand::Resolve {
            app_name: name.clone(),
            response,
        })
        .await
    } else {
        ask_agent(&state.cmd_tx, |response| AgentCommand::ResolveAll {
            response,
        })
        .await
        .map(|entries| {
            entries
                .into_iter()
                .find(|entry| entry.app_name == name && service_visible_to(auth.as_deref(), entry))
        })
    };
    match found {
        Ok(Some(info)) => Json(serde_json::json!(info)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("service {name:?} not found") })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// List the registered services the caller may see.
pub(super) async fn resolve_all_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::ResolveAll {
        response,
    })
    .await
    {
        Ok(entries) => {
            let visible: Vec<_> = entries
                .into_iter()
                .filter(|entry| service_visible_to(auth.as_deref(), entry))
                .collect();
            Json(serde_json::json!(visible)).into_response()
        }
        Err(response) => response,
    }
}

/// List the ingress routes the caller may see.
pub(super) async fn routes_handler(
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    State(state): State<ApiState>,
) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Routes { response }).await {
        Ok(routes) => {
            let visible: Vec<_> = routes
                .into_iter()
                .filter(|route| {
                    crate::sesame::auth::authorize_scoped(
                        auth.as_deref(),
                        &route.app_name,
                        &route.namespace,
                    )
                    .is_ok()
                })
                .collect();
            Json(serde_json::json!(visible)).into_response()
        }
        Err(response) => response,
    }
}

fn service_visible_to(
    auth: Option<&crate::sesame::auth::AuthContext>,
    entry: &crate::onion::types::ResolveResponse,
) -> bool {
    crate::sesame::auth::authorize_scoped(auth, &entry.app_name, &entry.namespace).is_ok()
}
