//! Service discovery routes: resolve and the routing table.

use super::*;

/// Resolve a service name to its VIP and backends.
pub(super) async fn resolve_handler(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Resolve {
        app_name: name.clone(),
        response,
    })
    .await
    {
        Ok(Some(info)) => Json(serde_json::json!(info)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("service {name:?} not found") })),
        )
            .into_response(),
        Err(response) => response,
    }
}

/// List all registered services.
pub(super) async fn resolve_all_handler(State(state): State<ApiState>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::ResolveAll {
        response,
    })
    .await
    {
        Ok(entries) => Json(serde_json::json!(entries)).into_response(),
        Err(response) => response,
    }
}

/// List all ingress routes.
pub(super) async fn routes_handler(State(state): State<ApiState>) -> Response {
    match ask_agent(&state.cmd_tx, |response| AgentCommand::Routes { response }).await {
        Ok(routes) => Json(serde_json::json!(routes)).into_response(),
        Err(response) => response,
    }
}
