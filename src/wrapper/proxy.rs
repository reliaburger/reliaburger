use std::sync::Arc;
/// HTTP reverse proxy for the Wrapper ingress.
///
/// Accepts incoming HTTP requests, looks up the routing table by
/// Host header and path, selects a backend, and forwards the
/// request. Returns 404 for unknown hosts, 502 for no healthy
/// backends, 503 if the connection limit is reached.
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::routing::RoutingTable;
use super::types::WrapperConfig;

/// Shared state for the proxy handlers.
pub struct ProxyState {
    pub routing_table: Arc<RwLock<RoutingTable>>,
    pub active_connections: AtomicUsize,
    pub max_connections: usize,
    pub client: reqwest::Client,
}

/// Run the Wrapper HTTP proxy on the configured port.
///
/// This runs on a dedicated tokio runtime to isolate ingress
/// traffic from cluster operations. Blocks until the shutdown
/// token is cancelled.
pub async fn run_proxy(
    config: WrapperConfig,
    routing_table: Arc<RwLock<RoutingTable>>,
    shutdown: CancellationToken,
) -> Result<(), super::types::WrapperError> {
    let state = Arc::new(ProxyState {
        routing_table,
        active_connections: AtomicUsize::new(0),
        max_connections: config.max_connections,
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(32)
            .build()
            .unwrap(),
    });

    let app = axum::Router::new()
        .fallback(proxy_handler)
        .with_state(state);

    let addr: std::net::SocketAddr = ([0, 0, 0, 0], config.http_port).into();
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        super::types::WrapperError::BindFailed {
            addr,
            reason: e.to_string(),
        }
    })?;

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.cancelled().await;
        })
        .await
        .map_err(|e| super::types::WrapperError::ProxyFailed(e.to_string()))?;

    Ok(())
}

/// The main proxy handler. Routes every incoming request.
async fn proxy_handler(State(state): State<Arc<ProxyState>>, req: Request<Body>) -> Response {
    // Connection limit check
    let current = state.active_connections.fetch_add(1, Ordering::Relaxed);
    if current >= state.max_connections {
        state.active_connections.fetch_sub(1, Ordering::Relaxed);
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    let response = do_proxy(&state, req).await;

    state.active_connections.fetch_sub(1, Ordering::Relaxed);
    response
}

/// Route and proxy a single request.
async fn do_proxy(state: &ProxyState, req: Request<Body>) -> Response {
    // Extract host from the Host header
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let path = req.uri().path();

    // Look up the route
    let table = state.routing_table.read().await;
    let route = match table.lookup(host, path) {
        Some(r) => r,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // Select a backend
    let backend = match route.select_backend() {
        Some(b) => b.addr,
        None => return StatusCode::BAD_GATEWAY.into_response(),
    };

    // Drop the read lock before making the HTTP call
    drop(table);

    // Build the upstream URL
    let upstream_uri = match build_upstream_uri(&backend, req.uri()) {
        Some(u) => u,
        None => return StatusCode::BAD_GATEWAY.into_response(),
    };

    // Forward the request
    let (parts, body) = req.into_parts();

    let mut upstream_req = state.client.request(parts.method, upstream_uri.to_string());

    // Forward headers (skip host — the backend doesn't need it)
    for (name, value) in &parts.headers {
        if name != "host" {
            upstream_req = upstream_req.header(name, value);
        }
    }

    // Forward the body
    let body_bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    if !body_bytes.is_empty() {
        upstream_req = upstream_req.body(body_bytes);
    }

    // Send the request to the backend
    match upstream_req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = Response::builder().status(status);

            for (name, value) in resp.headers() {
                response = response.header(name, value);
            }

            match resp.bytes().await {
                Ok(bytes) => response
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response()),
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

/// Build the upstream URI from the backend address and request URI.
fn build_upstream_uri(backend: &std::net::SocketAddr, request_uri: &Uri) -> Option<Uri> {
    let path_and_query = request_uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    format!("http://{backend}{path_and_query}").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_upstream_uri_with_path() {
        let addr: std::net::SocketAddr = "10.0.2.2:30001".parse().unwrap();
        let req_uri: Uri = "/api/v1/users?page=2".parse().unwrap();
        let upstream = build_upstream_uri(&addr, &req_uri).unwrap();
        assert_eq!(
            upstream.to_string(),
            "http://10.0.2.2:30001/api/v1/users?page=2"
        );
    }

    #[test]
    fn build_upstream_uri_root_path() {
        let addr: std::net::SocketAddr = "10.0.2.2:30001".parse().unwrap();
        let req_uri: Uri = "/".parse().unwrap();
        let upstream = build_upstream_uri(&addr, &req_uri).unwrap();
        assert_eq!(upstream.to_string(), "http://10.0.2.2:30001/");
    }
}
