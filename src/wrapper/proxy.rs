use std::net::SocketAddr;
/// HTTP(S) reverse proxy for the Wrapper ingress.
///
/// Accepts incoming HTTP requests, looks up the routing table by
/// Host header and path, applies per-client rate limits, selects a
/// backend, and forwards the request. Returns 404 for unknown hosts,
/// 429 when rate-limited, 502 for no healthy backends, 503 if the
/// connection limit is reached.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use super::rate_limit::{RateLimitResult, RateLimiter};
use super::routing::RoutingTable;
use super::types::{WrapperConfig, WrapperError};

/// Shared state for the proxy handlers.
pub struct ProxyState {
    pub routing_table: Arc<RwLock<RoutingTable>>,
    pub active_connections: AtomicUsize,
    pub max_connections: usize,
    pub client: reqwest::Client,
    /// Per-client-IP token buckets, shared across routes.
    pub rate_limiter: Mutex<RateLimiter>,
}

/// A proxy whose listeners are bound but not yet serving.
///
/// Binding and serving are split so callers learn the real addresses
/// before traffic flows — tests bind port 0 and read the assigned
/// port from `http_addr`.
pub struct BoundProxy {
    /// Address the HTTP listener is bound to.
    pub http_addr: SocketAddr,
    /// Address the HTTPS listener is bound to.
    pub https_addr: SocketAddr,
    http_listener: TcpListener,
    https_listener: TcpListener,
    tls_acceptor: tokio_rustls::TlsAcceptor,
    state: Arc<ProxyState>,
    shutdown: CancellationToken,
}

/// Bind the Wrapper HTTP and HTTPS listeners.
///
/// TLS uses the certificate and key from `config.tls_cert_path` /
/// `config.tls_key_path` when both are set, and a self-signed
/// certificate generated at startup otherwise.
pub async fn bind_proxy(
    config: WrapperConfig,
    routing_table: Arc<RwLock<RoutingTable>>,
    shutdown: CancellationToken,
) -> Result<BoundProxy, WrapperError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .build()
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build http client: {e}")))?;

    let state = Arc::new(ProxyState {
        routing_table,
        active_connections: AtomicUsize::new(0),
        max_connections: config.max_connections,
        client,
        rate_limiter: Mutex::new(RateLimiter::new()),
    });

    let http_listener = bind(config.http_port).await?;
    let http_addr = local_addr(&http_listener)?;

    let (certs, key) = match (&config.tls_cert_path, &config.tls_key_path) {
        (Some(cert), Some(key)) => super::tls::load_certs_from_disk(cert, key)
            .map_err(|e| WrapperError::ProxyFailed(format!("failed to load TLS files: {e}")))?,
        _ => {
            let (cert, key) = super::tls::generate_self_signed_cert()
                .map_err(|e| WrapperError::ProxyFailed(format!("failed to self-sign: {e}")))?;
            (vec![cert], key)
        }
    };
    let tls_config = super::tls::build_tls_config(certs, key)
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build TLS config: {e}")))?;
    let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config);

    let https_listener = bind(config.https_port).await?;
    let https_addr = local_addr(&https_listener)?;

    Ok(BoundProxy {
        http_addr,
        https_addr,
        http_listener,
        https_listener,
        tls_acceptor,
        state,
        shutdown,
    })
}

async fn bind(port: u16) -> Result<TcpListener, WrapperError> {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    TcpListener::bind(addr)
        .await
        .map_err(|e| WrapperError::BindFailed {
            addr,
            reason: e.to_string(),
        })
}

fn local_addr(listener: &TcpListener) -> Result<SocketAddr, WrapperError> {
    listener
        .local_addr()
        .map_err(|e| WrapperError::ProxyFailed(format!("no local address: {e}")))
}

impl BoundProxy {
    /// Serve both listeners until the shutdown token fires.
    pub async fn serve(self) -> Result<(), WrapperError> {
        let router = axum::Router::new()
            .fallback(proxy_handler)
            .with_state(Arc::clone(&self.state));

        let http = {
            let app = router
                .clone()
                .into_make_service_with_connect_info::<SocketAddr>();
            let shutdown = self.shutdown.clone();
            let listener = self.http_listener;
            async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        shutdown.cancelled().await;
                    })
                    .await
                    .map_err(|e| WrapperError::ProxyFailed(e.to_string()))
            }
        };

        let https = serve_tls(
            self.https_listener,
            self.tls_acceptor,
            router,
            self.shutdown.clone(),
        );

        tokio::try_join!(http, https)?;
        Ok(())
    }
}

/// Accept TLS connections and serve them through the router.
///
/// The TLS handshake happens in a spawned task per connection, so a
/// slow (or malicious) handshaker never blocks the accept loop.
async fn serve_tls(
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    shutdown: CancellationToken,
) -> Result<(), WrapperError> {
    use tower::Service;

    let mut make_service = router.into_make_service_with_connect_info::<SocketAddr>();

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            accepted = listener.accept() => {
                let (tcp, remote) = match accepted {
                    Ok(pair) => pair,
                    // Transient accept errors (EMFILE, resets) must not
                    // kill the ingress; skip and keep accepting.
                    Err(_) => continue,
                };
                let service = match make_service.call(remote).await {
                    Ok(service) => service,
                    Err(infallible) => match infallible {},
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls_stream) = acceptor.accept(tcp).await else {
                        return; // handshake failed; drop the connection
                    };
                    let hyper_service = hyper_util::service::TowerToHyperService::new(service);
                    let _ = hyper_util::server::conn::auto::Builder::new(
                        hyper_util::rt::TokioExecutor::new(),
                    )
                    .serve_connection_with_upgrades(
                        hyper_util::rt::TokioIo::new(tls_stream),
                        hyper_service,
                    )
                    .await;
                });
            }
        }
    }
}

/// Run the Wrapper proxy on the configured ports. Convenience wrapper
/// around [`bind_proxy`] + [`BoundProxy::serve`]; blocks until the
/// shutdown token is cancelled.
pub async fn run_proxy(
    config: WrapperConfig,
    routing_table: Arc<RwLock<RoutingTable>>,
    shutdown: CancellationToken,
) -> Result<(), WrapperError> {
    let bound = bind_proxy(config, routing_table, shutdown).await?;
    bound.serve().await
}

/// The main proxy handler. Routes every incoming request.
async fn proxy_handler(
    State(state): State<Arc<ProxyState>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    // Connection limit check
    let current = state.active_connections.fetch_add(1, Ordering::Relaxed);
    if current >= state.max_connections {
        state.active_connections.fetch_sub(1, Ordering::Relaxed);
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    let response = do_proxy(&state, remote, req).await;

    state.active_connections.fetch_sub(1, Ordering::Relaxed);
    response
}

/// Route and proxy a single request.
async fn do_proxy(state: &ProxyState, remote: SocketAddr, req: Request<Body>) -> Response {
    // Check for WebSocket upgrade before anything else
    let is_ws = super::websocket::is_websocket_upgrade(&req);

    // Extract host from the Host header
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let path = req.uri().path();

    // Look up the route, copying what we need out so the read lock
    // is released before any await point.
    let (route_allows_ws, backend, rate_limit) = {
        let table = state.routing_table.read().await;
        let route = match table.lookup(host, path) {
            Some(r) => r,
            None => return StatusCode::NOT_FOUND.into_response(),
        };
        (
            route.websocket,
            route.select_backend().map(|b| b.addr),
            route.rate_limit.clone(),
        )
    };

    // Rate limit before touching the backend: a limited client gets
    // 429 even when the backend pool is empty or unhealthy.
    if let Some(config) = rate_limit {
        let result = state.rate_limiter.lock().await.check(remote.ip(), &config);
        if let RateLimitResult::Denied { retry_after_secs } = result {
            return Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("retry-after", retry_after_secs.to_string())
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::TOO_MANY_REQUESTS.into_response());
        }
    }

    // WebSocket upgrade: check if route allows it
    if is_ws && !route_allows_ws {
        return StatusCode::BAD_REQUEST.into_response();
    }

    // Select a backend
    let backend = match backend {
        Some(b) => b,
        None => return StatusCode::BAD_GATEWAY.into_response(),
    };

    // WebSocket: delegate to the upgrade handler (no body buffering)
    if is_ws {
        return super::websocket::handle_websocket_upgrade(req, backend).await;
    }

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
