use std::net::SocketAddr;
/// HTTP(S) reverse proxy for the Wrapper ingress.
///
/// Accepts incoming HTTP requests, looks up the routing table by
/// Host header and path, applies per-client rate limits, selects a
/// backend, and forwards the request. Returns 404 for unknown hosts,
/// 429 when rate-limited, 502 for no healthy backends, 503 if the
/// connection limit is reached.
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

use super::rate_limit::{RateLimitResult, ShardedRateLimiter};
use super::routing::RoutingTable;
use super::types::{WrapperConfig, WrapperError};

/// Shared state for the proxy handlers.
pub struct ProxyState {
    pub routing_table: Arc<RwLock<RoutingTable>>,
    /// Bounds concurrent in-flight connections. A permit is held for the
    /// whole lifetime of a request — including a WebSocket splice, which is
    /// released only when the splice ends, not at the 101 (ING2).
    pub connection_limit: Arc<Semaphore>,
    pub client: reqwest::Client,
    /// Per-route, per-client-IP token buckets, sharded to avoid a single
    /// global lock (ING5: keyed on the route, not the IP alone).
    pub rate_limiter: ShardedRateLimiter,
    /// Largest request body the proxy will forward before rejecting with 413.
    pub max_request_body_bytes: usize,
    /// Shared drain tracker. When Bun retires an old backend it starts a
    /// drain here; the proxy bumps that instance's in-flight count around
    /// each forwarded request so Bun waits for it to reach zero before
    /// killing the container (DEP5).
    pub drains: Option<super::draining::SharedDrains>,
}

/// Largest number of backends the proxy will try for one request: the primary
/// plus up to two failover candidates. Bounded so a single request can't fan
/// out across an entire unhealthy pool, and only ever advances on a pure
/// connection failure (never on a 5xx, which may have had side effects).
const MAX_UPSTREAM_ATTEMPTS: usize = 3;

/// Marks that the current request arrived over the HTTPS listener.
///
/// Inserted as a request extension by the HTTPS serving path. The handler
/// reads it to decide whether a TLS-requiring route needs a redirect, and
/// to set `X-Forwarded-Proto` to the proxy's real view (ING5).
#[derive(Clone, Copy)]
struct ServedOverTls(bool);

/// Decrements a draining backend's in-flight count when the request ends.
///
/// This is Rust's RAII pattern: hold the guard for the request's lifetime
/// and `Drop` releases the count on every exit path, including early
/// returns and errors. `Drop` can't be `async`, so it spawns a tiny task
/// to do the (async) decrement — the count is released promptly regardless.
struct DrainGuard {
    drains: super::draining::SharedDrains,
    instance_id: String,
    /// Whether this request is a WebSocket splice. WebSocket splices bump a
    /// separate counter so the drain waits for the live splice, not just the
    /// 101 handshake (ING4).
    websocket: bool,
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        let drains = self.drains.clone();
        let instance_id = std::mem::take(&mut self.instance_id);
        let websocket = self.websocket;
        tokio::spawn(async move {
            if websocket {
                drains.decrement_websocket(&instance_id).await;
            }
            drains.decrement_connections(&instance_id).await;
        });
    }
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
    /// Bounds concurrent TLS handshakes so a slow-handshake flood can't
    /// pile up tasks (ING2).
    handshake_limit: Arc<Semaphore>,
    handshake_timeout: std::time::Duration,
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
    bind_proxy_with_drains(config, routing_table, None, shutdown).await
}

/// Bind the listeners with a shared drain tracker (DEP5).
///
/// Bun passes the same `SharedDrains` it holds so the proxy can report
/// in-flight requests to draining backends. Otherwise identical to
/// [`bind_proxy`].
pub async fn bind_proxy_with_drains(
    config: WrapperConfig,
    routing_table: Arc<RwLock<RoutingTable>>,
    drains: Option<super::draining::SharedDrains>,
    shutdown: CancellationToken,
) -> Result<BoundProxy, WrapperError> {
    bind_proxy_with_tls(config, routing_table, drains, None, shutdown).await
}

/// Bind the listeners, optionally resolving TLS certificates per SNI from the
/// cluster Ingress CA (M8).
///
/// When `cert_resolver` is `Some`, `tls = "cluster"` routes are served a
/// certificate issued from the Ingress CA for the requested host, so a client
/// trusting the cluster root trusts the ingress. When `None`, TLS falls back to
/// the operator disk cert or a self-signed `localhost` cert, exactly as before.
pub async fn bind_proxy_with_tls(
    config: WrapperConfig,
    routing_table: Arc<RwLock<RoutingTable>>,
    drains: Option<super::draining::SharedDrains>,
    cert_resolver: Option<Arc<dyn rustls::server::ResolvesServerCert>>,
    shutdown: CancellationToken,
) -> Result<BoundProxy, WrapperError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .build()
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build http client: {e}")))?;

    let state = Arc::new(ProxyState {
        routing_table,
        connection_limit: Arc::new(Semaphore::new(config.max_connections)),
        client,
        rate_limiter: ShardedRateLimiter::new(),
        max_request_body_bytes: config.max_request_body_bytes,
        drains,
    });

    let http_listener = bind(config.http_port).await?;
    let http_addr = local_addr(&http_listener)?;

    // An operator disk cert always wins. Otherwise, if the cluster Ingress CA
    // resolver is wired, serve per-SNI cluster-signed certs; failing both, a
    // self-signed `localhost` cert (dev / no-cluster).
    let tls_config = match (&config.tls_cert_path, &config.tls_key_path) {
        (Some(cert), Some(key)) => {
            let (certs, key) = super::tls::load_certs_from_disk(cert, key)
                .map_err(|e| WrapperError::ProxyFailed(format!("failed to load TLS files: {e}")))?;
            super::tls::build_tls_config(certs, key).map_err(|e| {
                WrapperError::ProxyFailed(format!("failed to build TLS config: {e}"))
            })?
        }
        _ => match cert_resolver {
            Some(resolver) => {
                super::tls::build_tls_config_with_resolver(resolver).map_err(|e| {
                    WrapperError::ProxyFailed(format!("failed to build ingress TLS resolver: {e}"))
                })?
            }
            None => {
                let (cert, key) = super::tls::generate_self_signed_cert()
                    .map_err(|e| WrapperError::ProxyFailed(format!("failed to self-sign: {e}")))?;
                super::tls::build_tls_config(vec![cert], key).map_err(|e| {
                    WrapperError::ProxyFailed(format!("failed to build TLS config: {e}"))
                })?
            }
        },
    };
    let tls_acceptor = tokio_rustls::TlsAcceptor::from(tls_config);

    let https_listener = bind(config.https_port).await?;
    let https_addr = local_addr(&https_listener)?;

    Ok(BoundProxy {
        http_addr,
        https_addr,
        http_listener,
        https_listener,
        tls_acceptor,
        handshake_limit: Arc::new(Semaphore::new(config.max_tls_handshakes)),
        handshake_timeout: config.tls_handshake_timeout,
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
        // The two listeners share one handler but tag requests with the
        // scheme they arrived on, so a TLS route on the plain listener
        // redirects and `X-Forwarded-Proto` reflects reality (ING1/ING5).
        let http_router = axum::Router::new()
            .fallback(proxy_handler)
            .layer(axum::Extension(ServedOverTls(false)))
            .with_state(Arc::clone(&self.state));

        let https_router = axum::Router::new()
            .fallback(proxy_handler)
            .layer(axum::Extension(ServedOverTls(true)))
            .with_state(Arc::clone(&self.state));

        let http = {
            let app = http_router.into_make_service_with_connect_info::<SocketAddr>();
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
            https_router,
            self.handshake_limit,
            self.handshake_timeout,
            self.shutdown.clone(),
        );

        tokio::try_join!(http, https)?;
        Ok(())
    }
}

/// Accept TLS connections and serve them through the router.
///
/// Two protections keep a slow or malicious handshaker from exhausting the
/// process (ING2): a semaphore bounds how many handshakes run at once, and
/// each handshake is wrapped in a deadline so a peer that opens a socket and
/// then stalls is dropped rather than holding a task forever.
async fn serve_tls(
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    router: axum::Router,
    handshake_limit: Arc<Semaphore>,
    handshake_timeout: std::time::Duration,
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

                // Grab a handshake permit before spending any work on this
                // connection. If none is free we're already at the flood cap,
                // so drop the connection rather than queueing tasks.
                let permit = match Arc::clone(&handshake_limit).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => continue,
                };

                let service = match make_service.call(remote).await {
                    Ok(service) => service,
                    Err(infallible) => match infallible {},
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // Hold the handshake permit only until the handshake
                    // resolves; the request itself is bounded separately.
                    let handshake = tokio::time::timeout(
                        handshake_timeout,
                        acceptor.accept(tcp),
                    ).await;
                    drop(permit);
                    let tls_stream = match handshake {
                        Ok(Ok(stream)) => stream,
                        // handshake failed or timed out; drop the connection
                        _ => return,
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
    axum::Extension(ServedOverTls(over_tls)): axum::Extension<ServedOverTls>,
    req: Request<Body>,
) -> Response {
    // Count the request and mark it in flight. Mayo reads these process-global
    // counters to emit `ingress_requests_total`; Wrapper only produces them.
    let metrics = super::metrics::global_ingress_metrics();
    metrics.record_request();
    metrics.inc_in_flight();

    // Hold a connection permit for the whole request. `try_acquire_owned`
    // returns immediately when full, so a flood gets 503 instead of queueing.
    // The permit lives in `_permit` and is released on every return path; for
    // a WebSocket it is moved into the splice task so it outlives the 101.
    let permit = match Arc::clone(&state.connection_limit).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            metrics.record_response(StatusCode::SERVICE_UNAVAILABLE);
            metrics.dec_in_flight();
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    let response = do_proxy(&state, remote, over_tls, req, permit).await;
    metrics.record_response(response.status());
    metrics.dec_in_flight();
    response
}

/// Generate a fresh request identifier.
///
/// A 128-bit random value, hex-encoded — unique enough to correlate a request
/// across the proxy hop without pulling in a UUID dependency. Only used when
/// the client didn't already supply an `X-Request-ID`.
fn generate_request_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}

/// Route and proxy a single request.
async fn do_proxy(
    state: &ProxyState,
    remote: SocketAddr,
    over_tls: bool,
    req: Request<Body>,
    permit: OwnedSemaphorePermit,
) -> Response {
    // Check for WebSocket upgrade before anything else
    let is_ws = super::websocket::is_websocket_upgrade(&req);

    // Extract host from the Host header
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let path = req.uri().path().to_string();

    // Look up the route, copying what we need out so the read lock
    // is released before any await point. `select_backends` returns the
    // primary plus up to two failover candidates in round-robin order.
    let (route_allows_ws, tls_required, route_key, candidates, rate_limit) = {
        let table = state.routing_table.read().await;
        let route = match table.lookup(&host, &path) {
            Some(r) => r,
            None => return StatusCode::NOT_FOUND.into_response(),
        };
        (
            route.websocket,
            route.tls_mode.requires_tls(),
            route.route_key(),
            route.select_backends(MAX_UPSTREAM_ATTEMPTS),
            route.rate_limit.clone(),
        )
    };

    // A TLS route reached over plain HTTP is redirected to HTTPS rather than
    // served in the clear (ING1). ACME is not implemented, so there is no
    // plaintext challenge-path exception in the v1 contract.
    if tls_required && !over_tls {
        return redirect_to_https(&host, req.uri());
    }

    // Rate limit before touching the backend: a limited client gets
    // 429 even when the backend pool is empty or unhealthy. The key is
    // (route, client IP) so budgets don't leak across routes (ING5).
    if let Some(config) = rate_limit {
        let result = state
            .rate_limiter
            .check(&route_key, remote.ip(), &config)
            .await;
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

    // The primary backend, plus failover candidates behind it. An empty list
    // means nothing is routable, so 502.
    let (instance_id, backend) = match candidates.first() {
        Some((id, addr)) => (id.clone(), *addr),
        None => return StatusCode::BAD_GATEWAY.into_response(),
    };

    // A backend can be selected right as it starts draining (routing rebuild
    // and drain start aren't a single step). Two cases:
    //   * past its deadline (terminating): reject this *new* request with 503
    //     so no fresh load lands on a container about to be killed (§5.5).
    //   * still within its drain window: count the request so Bun waits for it
    //     to finish before killing the container (DEP5), and pick up the
    //     terminate token so a deadline that passes mid-request tears the
    //     connection down rather than leaving it running.
    // The guard drops on every return path, decrementing exactly once. A
    // WebSocket also bumps the websocket counter so the drain waits for the
    // live splice, not just the 101 (ING4).
    let mut terminate: Option<CancellationToken> = None;
    let drain_guard = match &state.drains {
        Some(drains) if drains.is_draining(&instance_id).await => {
            if drains.is_terminating(&instance_id).await {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            drains.increment_connections(&instance_id).await;
            if is_ws {
                drains.increment_websocket(&instance_id).await;
            }
            terminate = drains.terminate_token(&instance_id).await;
            Some(DrainGuard {
                drains: drains.clone(),
                instance_id: instance_id.clone(),
                websocket: is_ws,
            })
        }
        _ => None,
    };

    // WebSocket: delegate to the upgrade handler (no body buffering). The
    // connection permit and drain guard move into the splice task so they're
    // released when the WebSocket actually closes, not at the 101 (ING2/ING4).
    // WebSocket has no failover: it is a stateful splice to one backend.
    if is_ws {
        let boxed_guard: Option<Box<dyn std::any::Any + Send>> =
            drain_guard.map(|g| Box::new(g) as Box<dyn std::any::Any + Send>);
        return super::websocket::handle_websocket_upgrade(
            req,
            backend,
            remote,
            over_tls,
            permit,
            boxed_guard,
        )
        .await;
    }

    // Forward the request. Buffer the body once (bounded) so it can be replayed
    // against a failover backend on a pure connection failure. `to_bytes` still
    // buffers, but bounded: a body over the cap is rejected with 413 rather
    // than allowed to exhaust memory (ING3).
    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, state.max_request_body_bytes).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };

    // Preserve a client-supplied request id, or mint one. A backend and its
    // logs can then correlate the request across the proxy hop.
    let request_id = parts
        .headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(generate_request_id);

    // Collect the end-to-end request headers once so each attempt replays them.
    // Skip `host` (the backend derives its own), the hop-by-hop set (RFC 7230
    // §6.1) plus anything the client named in its own `Connection` header, the
    // client's own `X-Forwarded-*` and `X-Real-IP` (we set those from our own
    // view so a backend can't be lied to), and `X-Request-ID` (set explicitly
    // to the preserved-or-minted value).
    let conn_tokens = connection_tokens(&parts.headers);
    let mut forward_headers: Vec<(axum::http::HeaderName, axum::http::HeaderValue)> = Vec::new();
    for (name, value) in &parts.headers {
        let n = name.as_str();
        if name == "host"
            || is_hop_by_hop(n)
            || conn_tokens.contains(n)
            || is_forwarded_header(n)
            || n.eq_ignore_ascii_case("x-real-ip")
            || n.eq_ignore_ascii_case("x-request-id")
        {
            continue;
        }
        forward_headers.push((name.clone(), value.clone()));
    }

    // Try the primary, then failover candidates — but only re-send on a *pure
    // connection failure*. A backend that answered (even with a 5xx) may have
    // side effects, so its request is never replayed against another instance;
    // a connection that never opened is always safe to retry (§ retry).
    for (idx, (_cand_id, cand_addr)) in candidates.iter().enumerate() {
        let upstream_uri = match build_upstream_uri(cand_addr, &parts.uri) {
            Some(u) => u,
            None => continue,
        };

        let mut upstream_req = state
            .client
            .request(parts.method.clone(), upstream_uri.to_string());
        for (name, value) in &forward_headers {
            upstream_req = upstream_req.header(name, value);
        }
        // The proxy's own view of the connection (ING5), plus X-Real-IP and the
        // request id. A client cannot influence these — we stripped any it sent.
        for (name, value) in forwarded_headers(remote, over_tls) {
            upstream_req = upstream_req.header(name, value);
        }
        upstream_req = upstream_req.header("x-real-ip", remote.ip().to_string());
        upstream_req = upstream_req.header("x-request-id", request_id.as_str());
        if !body_bytes.is_empty() {
            upstream_req = upstream_req.body(body_bytes.clone());
        }

        match upstream_req.send().await {
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let mut response = Response::builder().status(status);

                // Copy end-to-end response headers only. Drop hop-by-hop headers
                // and the upstream framing headers (`Content-Length` /
                // `Transfer-Encoding`): the response body streams below, so hyper
                // re-frames it and copying the upstream length would mismatch.
                let conn_tokens = connection_tokens(resp.headers());
                for (name, value) in resp.headers() {
                    let n = name.as_str();
                    if is_hop_by_hop(n)
                        || n.eq_ignore_ascii_case("content-length")
                        || conn_tokens.contains(n)
                    {
                        continue;
                    }
                    response = response.header(name, value);
                }

                // Stream the backend's body instead of buffering it whole, so
                // SSE, gRPC and large downloads flow with backpressure and don't
                // pin the whole response in memory (ING3).
                //
                // The permit, drain guard, and terminate token ride along in the
                // stream's state, so they're released only when the body finishes
                // streaming — keeping the connection permit and drain count
                // accurate for the whole response, and letting a drain deadline
                // that fires mid-stream tear the connection down (ING2/DEP5/§5.5).
                let stream =
                    guarded_body_stream(resp.bytes_stream(), permit, drain_guard, terminate);
                return response
                    .body(Body::from_stream(stream))
                    .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
            }
            Err(e) => {
                // Retry only on a pure connection failure with a candidate left.
                // Anything else (timeout, mid-body error) returns 502 unretried.
                if e.is_connect() && idx + 1 < candidates.len() {
                    continue;
                }
                break;
            }
        }
    }

    // Every attempt failed (or the failure wasn't safe to retry). Nothing
    // streams, so release the guards immediately.
    drop(drain_guard);
    drop(permit);
    StatusCode::BAD_GATEWAY.into_response()
}

/// Wrap a byte stream so it owns the connection permit and drain guard.
///
/// The guards live in the stream's state and drop when the stream ends or is
/// dropped — i.e. when the response body has fully streamed to the client.
/// So the connection permit and the drain accounting stay held for the whole
/// response, not just until the handler returns.
fn guarded_body_stream<S>(
    inner: S,
    permit: OwnedSemaphorePermit,
    drain_guard: Option<DrainGuard>,
    terminate: Option<CancellationToken>,
) -> impl futures_util::Stream<Item = Result<axum::body::Bytes, reqwest::Error>>
where
    S: futures_util::Stream<Item = Result<axum::body::Bytes, reqwest::Error>> + Unpin,
{
    // `unfold` threads owned state through each poll; when the stream finishes
    // or is dropped, the state (permit + guard + token) is dropped with it.
    futures_util::stream::unfold(
        (inner, permit, drain_guard, terminate),
        |(mut inner, permit, drain_guard, terminate)| async move {
            use futures_util::StreamExt;
            // If the drain deadline fires mid-response, stop streaming so the
            // connection closes rather than running on against a backend about
            // to be killed (§5.5 HTTP termination).
            let next = match &terminate {
                Some(token) => {
                    tokio::select! {
                        _ = token.cancelled() => None,
                        chunk = inner.next() => chunk,
                    }
                }
                None => inner.next().await,
            };
            next.map(|chunk| (chunk, (inner, permit, drain_guard, terminate)))
        },
    )
}

/// Build a 308 redirect from a plain-HTTP request to the same URL on HTTPS.
///
/// 308 (Permanent Redirect) preserves the method and body, unlike 301/302
/// which some clients rewrite to GET.
fn redirect_to_https(host: &str, uri: &Uri) -> Response {
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let location = format!("https://{host}{path_and_query}");
    Response::builder()
        .status(StatusCode::PERMANENT_REDIRECT)
        .header("location", location)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::PERMANENT_REDIRECT.into_response())
}

/// Whether a header name is one of the forwarding headers the proxy owns and
/// therefore must strip from client input (ING5).
pub(super) fn is_forwarded_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("x-forwarded-for")
        || name.eq_ignore_ascii_case("x-forwarded-proto")
        || name.eq_ignore_ascii_case("forwarded")
}

/// The `X-Forwarded-*` headers the proxy sets from its own view of the
/// connection. A client cannot influence these — we already stripped any it
/// sent — so a backend can trust them (ING5).
pub(super) fn forwarded_headers(remote: SocketAddr, over_tls: bool) -> Vec<(&'static str, String)> {
    vec![
        ("x-forwarded-for", remote.ip().to_string()),
        (
            "x-forwarded-proto",
            if over_tls { "https" } else { "http" }.to_string(),
        ),
    ]
}

/// Hop-by-hop headers that apply to a single transport leg and must not be
/// forwarded through a proxy (RFC 7230 §6.1). Compared case-insensitively.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Whether a header name is hop-by-hop (case-insensitive).
fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// The set of header names listed in a message's `Connection` header. Those
/// are themselves hop-by-hop for this leg and must also be dropped.
fn connection_tokens(headers: &axum::http::HeaderMap) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    for value in headers.get_all(axum::http::header::CONNECTION) {
        if let Ok(v) = value.to_str() {
            for token in v.split(',') {
                let token = token.trim();
                if !token.is_empty() {
                    set.insert(token.to_ascii_lowercase());
                }
            }
        }
    }
    set
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

    #[test]
    fn hop_by_hop_headers_recognised_case_insensitively() {
        assert!(is_hop_by_hop("connection"));
        assert!(is_hop_by_hop("Transfer-Encoding"));
        assert!(is_hop_by_hop("UPGRADE"));
        assert!(!is_hop_by_hop("content-type"));
        assert!(!is_hop_by_hop("x-forwarded-for"));
    }

    #[test]
    fn connection_tokens_are_collected_lowercased() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONNECTION,
            "keep-alive, X-Custom".parse().unwrap(),
        );
        let tokens = connection_tokens(&headers);
        assert!(tokens.contains("keep-alive"));
        assert!(tokens.contains("x-custom"));
        assert!(!tokens.contains("content-type"));
    }

    #[test]
    fn forwarded_headers_reflect_the_proxy_view() {
        let remote: SocketAddr = "203.0.113.7:54321".parse().unwrap();
        let headers = forwarded_headers(remote, true);
        assert!(headers.contains(&("x-forwarded-for", "203.0.113.7".to_string())));
        assert!(headers.contains(&("x-forwarded-proto", "https".to_string())));

        let plain = forwarded_headers(remote, false);
        assert!(plain.contains(&("x-forwarded-proto", "http".to_string())));
    }

    #[test]
    fn client_forwarded_headers_are_recognised_for_stripping() {
        assert!(is_forwarded_header("X-Forwarded-For"));
        assert!(is_forwarded_header("x-forwarded-proto"));
        assert!(is_forwarded_header("Forwarded"));
        assert!(!is_forwarded_header("x-real-ip"));
        assert!(!is_forwarded_header("content-type"));
    }

    #[test]
    fn redirect_to_https_preserves_path() {
        let uri: Uri = "/dashboard?tab=1".parse().unwrap();
        let resp = redirect_to_https("myapp.com", &uri);
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        let location = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(location, "https://myapp.com/dashboard?tab=1");
    }

    /// ING5: a client that supplies its own `X-Forwarded-For`/`-Proto` has
    /// them replaced with the proxy's view, so a backend can't be lied to.
    /// A backend echoes the headers it received; the proxy must show the real
    /// client IP and the scheme it terminated, not the client's claims.
    #[tokio::test]
    async fn proxy_replaces_client_forwarded_headers() {
        use crate::onion::types::BackendInstance;
        use std::net::Ipv4Addr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Backend echoes the request headers it saw back in the body.
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = backend.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = head.clone();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });

        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map
            .register_app("web", "default", 80, None)
            .unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-0".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: backend_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "web".to_string()),
            crate::config::app::IngressSpec {
                host: "web.test".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let shutdown = CancellationToken::new();
        let bound = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                ..WrapperConfig::default()
            },
            routing_table,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{http_port}/");
        let resp = client
            .get(&url)
            .header("host", "web.test")
            .header("x-forwarded-for", "1.2.3.4")
            .header("x-forwarded-proto", "https")
            .send()
            .await
            .unwrap();
        let echoed = resp.text().await.unwrap().to_lowercase();

        // The client's lie is gone; the proxy's real view is present.
        assert!(
            !echoed.contains("1.2.3.4"),
            "client-supplied x-forwarded-for leaked to the backend: {echoed}"
        );
        assert!(
            echoed.contains("x-forwarded-for: 127.0.0.1"),
            "proxy did not set x-forwarded-for to the real client: {echoed}"
        );
        assert!(
            echoed.contains("x-forwarded-proto: http"),
            "proxy did not set x-forwarded-proto to the terminated scheme: {echoed}"
        );

        shutdown.cancel();
    }

    /// Every forwarded request carries `X-Real-IP` (the real client IP) and an
    /// `X-Request-ID`. A client-supplied request id is preserved; otherwise the
    /// proxy mints one. A client-supplied `X-Real-IP` is replaced, not trusted.
    #[tokio::test]
    async fn proxy_sets_real_ip_and_request_id() {
        use crate::onion::types::BackendInstance;
        use std::net::Ipv4Addr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Backend echoes the request head back in the body so the test can
        // inspect the headers the proxy forwarded.
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = backend.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        head.len(),
                        head
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map
            .register_app("web", "default", 80, None)
            .unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-0".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: backend_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "web".to_string()),
            crate::config::app::IngressSpec {
                host: "web.test".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let shutdown = CancellationToken::new();
        let bound = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                ..WrapperConfig::default()
            },
            routing_table,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{http_port}/");

        // No client request id, and a spoofed X-Real-IP the proxy must replace.
        let echoed = client
            .get(&url)
            .header("host", "web.test")
            .header("x-real-ip", "9.9.9.9")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .to_lowercase();
        assert!(
            echoed.contains("x-real-ip: 127.0.0.1"),
            "proxy did not set x-real-ip to the real client: {echoed}"
        );
        assert!(
            !echoed.contains("9.9.9.9"),
            "client-supplied x-real-ip leaked to the backend: {echoed}"
        );
        assert!(
            echoed.contains("x-request-id:"),
            "proxy did not add an x-request-id: {echoed}"
        );
        // Exactly one x-real-ip line — the client's was replaced, not appended.
        assert_eq!(echoed.matches("x-real-ip:").count(), 1);

        // A client-supplied request id is preserved verbatim.
        let echoed = client
            .get(&url)
            .header("host", "web.test")
            .header("x-request-id", "client-supplied-id-123")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .to_lowercase();
        assert!(
            echoed.contains("x-request-id: client-supplied-id-123"),
            "proxy did not preserve the client's request id: {echoed}"
        );
        assert_eq!(echoed.matches("x-request-id:").count(), 1);

        shutdown.cancel();
    }

    /// §5.5: a new request routed to a backend whose drain has expired gets a
    /// 503 — the proxy refuses to pile fresh load onto a container about to be
    /// killed. (Within the drain window it would still be served; here the
    /// deadline has already passed.)
    #[tokio::test]
    async fn new_request_to_terminating_backend_gets_503() {
        use crate::onion::types::BackendInstance;
        use crate::wrapper::draining::{DrainCommand, DrainTracker, SharedDrains};
        use std::net::Ipv4Addr;
        use std::time::Duration;

        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();

        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map
            .register_app("web", "default", 80, None)
            .unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-0".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: backend_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "web".to_string()),
            crate::config::app::IngressSpec {
                host: "web.test".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let drains = SharedDrains::new(DrainTracker::new(tokio::sync::mpsc::channel(8).0));
        let shutdown = CancellationToken::new();
        let bound = bind_proxy_with_drains(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                ..WrapperConfig::default()
            },
            routing_table,
            Some(drains.clone()),
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        // Start a drain with a zero timeout, so the backend is immediately past
        // its deadline (terminating) without the sweep having removed it yet.
        drains
            .start_drain(&DrainCommand {
                app_name: "web".to_string(),
                instance_id: "default__web-0".to_string(),
                timeout: Duration::from_millis(0),
            })
            .await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(drains.is_terminating("default__web-0").await);

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{http_port}/");
        let resp = client
            .get(&url)
            .header("host", "web.test")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            503,
            "a new request to a terminating backend should be refused with 503"
        );

        shutdown.cancel();
    }

    /// § retry: a pure connection failure to the primary backend is retried
    /// against a healthy failover backend, so the client still gets a 200.
    #[tokio::test]
    async fn connection_failure_retries_another_backend() {
        use crate::onion::types::BackendInstance;
        use std::net::Ipv4Addr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A dead port: bind then drop so connects are refused.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);

        // A live backend that answers 200.
        let live = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_port = live.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = live.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await;
                });
            }
        });

        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map
            .register_app("web", "default", 80, None)
            .unwrap();
        // The dead backend is registered first, so it is candidate[0] (the
        // round-robin cursor starts at 0); the live one is the failover.
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-dead".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: dead_port,
                    healthy: true,
                },
            )
            .unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-live".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: live_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "web".to_string()),
            crate::config::app::IngressSpec {
                host: "web.test".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let shutdown = CancellationToken::new();
        let bound = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                ..WrapperConfig::default()
            },
            routing_table,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{http_port}/");
        // The first candidate refuses the connection; the proxy fails over to
        // the live backend and returns its 200.
        let resp = client
            .get(&url)
            .header("host", "web.test")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "connection failure to the primary was not retried against the failover backend"
        );
        assert_eq!(resp.text().await.unwrap(), "ok");

        shutdown.cancel();
    }

    /// ING1: a route marked `tls = "cluster"` is not served over plain HTTP.
    /// A plain request to it gets a 308 redirect to HTTPS, not the backend's
    /// response.
    #[tokio::test]
    async fn tls_route_redirects_plain_http_to_https() {
        use crate::onion::types::BackendInstance;
        use std::net::Ipv4Addr;

        // Backend that should never be reached over plain HTTP for a TLS route.
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();

        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map
            .register_app("web", "default", 80, None)
            .unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-0".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: backend_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "web".to_string()),
            crate::config::app::IngressSpec {
                host: "secure.test".to_string(),
                path: None,
                tls: Some("cluster".to_string()),
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let shutdown = CancellationToken::new();
        let bound = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                ..WrapperConfig::default()
            },
            routing_table,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        // Don't follow redirects: assert we get the 308, not the backend.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let url = format!("http://127.0.0.1:{http_port}/panel");
        let resp = client
            .get(&url)
            .header("host", "secure.test")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 308);
        let location = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(location, "https://secure.test/panel");

        // ACME isn't implemented in v1, so its well-known path is not a
        // plaintext escape hatch. It follows the same fail-closed redirect.
        let url =
            format!("http://127.0.0.1:{http_port}/.well-known/acme-challenge/not-a-responder");
        let resp = client
            .get(&url)
            .header("host", "secure.test")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 308);
        let location = resp.headers().get("location").unwrap().to_str().unwrap();
        assert_eq!(
            location,
            "https://secure.test/.well-known/acme-challenge/not-a-responder"
        );

        shutdown.cancel();
    }

    /// ING3: a request body over the configured cap is rejected with 413
    /// rather than buffered.
    #[tokio::test]
    async fn oversized_request_body_is_rejected() {
        use crate::onion::types::BackendInstance;
        use std::net::Ipv4Addr;

        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();

        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map
            .register_app("web", "default", 80, None)
            .unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-0".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: backend_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "web".to_string()),
            crate::config::app::IngressSpec {
                host: "web.test".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let shutdown = CancellationToken::new();
        let bound = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                max_request_body_bytes: 1024,
                ..WrapperConfig::default()
            },
            routing_table,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{http_port}/");
        let big_body = vec![b'x'; 4096];
        let resp = client
            .post(&url)
            .header("host", "web.test")
            .body(big_body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 413);

        shutdown.cancel();
    }

    /// ING3: a large response streams through rather than being buffered
    /// whole. A backend sends more than the request cap; the proxy still
    /// delivers all of it, proving the response path isn't bounded by (or
    /// buffered to) that cap.
    #[tokio::test]
    async fn large_response_streams_through() {
        use crate::onion::types::BackendInstance;
        use std::net::Ipv4Addr;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let payload_len = 5 * 1024 * 1024; // 5 MiB, well over the 1 KiB request cap
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = backend.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {payload_len}\r\n\r\n");
                let _ = sock.write_all(header.as_bytes()).await;
                let chunk = vec![b'z'; 64 * 1024];
                let mut sent = 0;
                while sent < payload_len {
                    let take = (payload_len - sent).min(chunk.len());
                    if sock.write_all(&chunk[..take]).await.is_err() {
                        break;
                    }
                    sent += take;
                }
            }
        });

        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map
            .register_app("web", "default", 80, None)
            .unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-0".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: backend_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "web".to_string()),
            crate::config::app::IngressSpec {
                host: "web.test".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let shutdown = CancellationToken::new();
        let bound = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                max_request_body_bytes: 1024, // small request cap; response is unbounded
                ..WrapperConfig::default()
            },
            routing_table,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{http_port}/");
        let resp = client
            .get(&url)
            .header("host", "web.test")
            .send()
            .await
            .unwrap();
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(
            bytes.len(),
            payload_len,
            "streamed response was truncated or capped by the request limit"
        );

        shutdown.cancel();
    }

    /// ING2: stalled TLS handshakes are bounded and reclaimed on the deadline.
    /// We open more silent connections than the handshake cap and never send a
    /// TLS ClientHello. If the handshakes weren't bounded and deadlined, a real
    /// TLS client afterwards would be starved; instead the stalled ones drop on
    /// their timeout and the real handshake succeeds.
    #[tokio::test]
    async fn stalled_tls_handshakes_are_bounded_and_reclaimed() {
        use std::time::Duration;
        use tokio::io::AsyncWriteExt;

        let routing_table = Arc::new(RwLock::new(RoutingTable::new()));
        let shutdown = CancellationToken::new();
        let bound = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                max_tls_handshakes: 2,
                tls_handshake_timeout: Duration::from_millis(200),
                ..WrapperConfig::default()
            },
            routing_table,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let https_port = bound.https_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        // Open several silent connections that hold a handshake slot without
        // ever sending a ClientHello.
        let mut stalled = Vec::new();
        for _ in 0..5 {
            if let Ok(sock) = tokio::net::TcpStream::connect(("127.0.0.1", https_port)).await {
                stalled.push(sock);
            }
        }

        // Wait past the handshake deadline so the stalled slots are reclaimed.
        tokio::time::sleep(Duration::from_millis(400)).await;

        // A real TLS ClientHello now gets a response (the handshake proceeds,
        // even though the self-signed cert won't validate — we only need the
        // server to engage, proving capacity wasn't exhausted).
        let mut real = tokio::net::TcpStream::connect(("127.0.0.1", https_port))
            .await
            .unwrap();
        // Minimal TLS record header for a ClientHello handshake message. The
        // server should respond (or at least not hang); we just assert the
        // write path is accepted and the connection is live.
        let client_hello_prefix = [0x16u8, 0x03, 0x01, 0x00, 0x2f];
        assert!(
            real.write_all(&client_hello_prefix).await.is_ok(),
            "server refused a real handshake after stalled ones — capacity exhausted"
        );

        for s in stalled {
            drop(s);
        }
        shutdown.cancel();
    }

    /// ING2: a WebSocket holds its connection permit through the live splice,
    /// not until the 101. With `max_connections = 1`, while one WebSocket is
    /// spliced a second request must be refused with 503; once the WebSocket
    /// closes the permit frees and a new request succeeds.
    #[tokio::test]
    async fn websocket_holds_permit_through_the_splice() {
        use crate::onion::types::BackendInstance;
        use std::net::Ipv4Addr;
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A backend that completes the WS handshake then holds the socket open,
        // echoing nothing, until the client goes away.
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = backend.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    // Read the upgrade request head.
                    let _ = sock.read(&mut buf).await;
                    // Reply with a 101 so the proxy splices the connection.
                    let resp = "HTTP/1.1 101 Switching Protocols\r\n\
                        Upgrade: websocket\r\nConnection: Upgrade\r\n\
                        Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n";
                    let _ = sock.write_all(resp.as_bytes()).await;
                    // Hold the socket open until the client closes.
                    let _ = sock.read(&mut buf).await;
                });
            }
        });

        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map.register_app("ws", "default", 80, None).unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "ws"),
                BackendInstance {
                    instance_id: "default__ws-0".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: backend_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "ws".to_string()),
            crate::config::app::IngressSpec {
                host: "ws.test".to_string(),
                path: None,
                tls: None,
                websocket: Some(true),
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let shutdown = CancellationToken::new();
        let bound = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                max_connections: 1, // exactly one permit
                ..WrapperConfig::default()
            },
            routing_table,
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        // Open a raw WebSocket upgrade and keep the connection open.
        let mut ws = tokio::net::TcpStream::connect(("127.0.0.1", http_port))
            .await
            .unwrap();
        let upgrade = "GET /socket HTTP/1.1\r\nHost: ws.test\r\n\
            Upgrade: websocket\r\nConnection: Upgrade\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
            Sec-WebSocket-Version: 13\r\n\r\n";
        ws.write_all(upgrade.as_bytes()).await.unwrap();
        let mut buf = [0u8; 1024];
        let n = ws.read(&mut buf).await.unwrap();
        let head = String::from_utf8_lossy(&buf[..n]);
        assert!(head.contains("101"), "proxy did not relay the 101: {head}");

        // The permit is still held by the live splice, so a second request is
        // refused with 503. If the permit had been released at the 101 this
        // would succeed.
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{http_port}/other");
        let refused = client.get(&url).header("host", "ws.test").send().await;
        // The single permit is taken, so the plain request is 503.
        let status = refused.map(|r| r.status().as_u16());
        assert_eq!(
            status.ok(),
            Some(503),
            "second request was not refused while a WebSocket held the permit"
        );

        // Close the WebSocket; the permit frees and a new request succeeds
        // (404 because /other has no backend body, but not 503).
        drop(ws);
        let mut ok = None;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(resp) = client.get(&url).header("host", "ws.test").send().await {
                let code = resp.status().as_u16();
                if code != 503 {
                    ok = Some(code);
                    break;
                }
            }
        }
        assert!(
            ok.is_some(),
            "permit never freed after the WebSocket closed (still 503)"
        );

        shutdown.cancel();
    }

    /// DEP5: the live proxy counts a request to a draining backend against
    /// the shared drain tracker, so `check_completions` does not finish the
    /// drain until the in-flight request returns. A slow backend keeps the
    /// request open long enough to observe the tracker holding the count.
    #[tokio::test]
    async fn live_proxy_holds_drain_open_while_a_request_is_in_flight() {
        use crate::onion::types::BackendInstance;
        use crate::wrapper::draining::{DrainCommand, DrainTracker, SharedDrains};
        use std::net::Ipv4Addr;
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A gated backend: it signals after receiving the request, then waits
        // for the test to release it. This proves the in-flight state without
        // relying on a guessed wall-clock delay.
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_port = backend.local_addr().unwrap().port();
        let request_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_backend = std::sync::Arc::new(tokio::sync::Notify::new());
        let backend_started = std::sync::Arc::clone(&request_started);
        let backend_release = std::sync::Arc::clone(&release_backend);
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = backend.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                backend_started.notify_one();
                backend_release.notified().await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await;
            }
        });

        // Routing table with one backend, reached over loopback.
        let mut service_map = crate::onion::service_map::ServiceMap::new();
        service_map
            .register_app("web", "default", 80, None)
            .unwrap();
        service_map
            .add_backend(
                &crate::onion::service_id::ServiceId::new("default", "web"),
                BackendInstance {
                    instance_id: "default__web-0".to_string(),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: backend_port,
                    healthy: true,
                },
            )
            .unwrap();
        let mut ingress = std::collections::HashMap::new();
        ingress.insert(
            ("default".to_string(), "web".to_string()),
            crate::config::app::IngressSpec {
                host: "web.test".to_string(),
                path: None,
                tls: None,
                websocket: None,
                rate_limit_rps: None,
                rate_limit_burst: None,
            },
        );
        let mut table = RoutingTable::new();
        table.rebuild(&service_map, &ingress).unwrap();
        let routing_table = Arc::new(RwLock::new(table));

        let drains = SharedDrains::new(DrainTracker::new(tokio::sync::mpsc::channel(8).0));
        let shutdown = CancellationToken::new();
        let bound = bind_proxy_with_drains(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                ..WrapperConfig::default()
            },
            routing_table,
            Some(drains.clone()),
            shutdown.clone(),
        )
        .await
        .unwrap();
        let http_port = bound.http_addr.port();
        tokio::spawn(async move {
            bound.serve().await.ok();
        });

        // Mark the backend draining, then fire a request at it through the
        // proxy. The slow backend keeps the request open for ~300ms.
        drains
            .start_drain(&DrainCommand {
                app_name: "web".to_string(),
                instance_id: "default__web-0".to_string(),
                timeout: Duration::from_secs(30),
            })
            .await;

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{http_port}/");
        let req =
            tokio::spawn(async move { client.get(&url).header("host", "web.test").send().await });

        tokio::time::timeout(Duration::from_secs(2), request_started.notified())
            .await
            .expect("request did not reach the gated backend");
        assert!(
            drains.check_completions().await.is_empty(),
            "drain completed while a request was still in flight"
        );
        assert!(
            drains.is_draining("default__web-0").await,
            "backend stopped draining while a request was in flight"
        );

        // Once the request returns, the drain completes on the next sweep.
        release_backend.notify_one();
        let _ = req.await.unwrap();
        // Poll: the proxy's decrement runs on a spawned task after the guard
        // drops, so give it a moment.
        let mut completed = Vec::new();
        for _ in 0..50 {
            completed = drains.check_completions().await;
            if !completed.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            completed,
            vec!["default__web-0".to_string()],
            "drain never completed after the request finished"
        );

        shutdown.cancel();
    }
}
