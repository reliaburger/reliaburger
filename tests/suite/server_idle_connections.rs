//! Bun's HTTP listeners must not keep dead or idle connections forever.
//!
//! The V02 soak found hundreds of loopback sockets piling up in Bun: Lima's
//! port forwarder never closes the guest side of a connection after the host
//! client leaves, and Hyper has no idle deadline unless someone switches one
//! on. These tests drive the shared accept loops (API and registry) and the
//! ingress proxy with millisecond deadlines and check that idle, silent and
//! stalled connections are closed while real streams keep flowing.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use reliaburger::sesame::connection::{
    ConnectionTimeouts, serve_router_over_tls, serve_router_plain,
};
use reliaburger::wrapper::{proxy::bind_proxy_with_tls, routing::RoutingTable, tls, types};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Deadlines short enough to watch them fire in a test.
fn quick_timeouts() -> ConnectionTimeouts {
    ConnectionTimeouts {
        tls_handshake: Duration::from_millis(300),
        request_head: Duration::from_millis(300),
        http2_ping_interval: Duration::from_millis(200),
        http2_ping_timeout: Duration::from_millis(200),
        write_stall: Duration::from_millis(400),
        tcp_keepalive_idle: Duration::from_secs(60),
        tcp_keepalive_interval: Duration::from_secs(15),
    }
}

/// Upper bound on how long any deadline above may take to close a socket.
const CLOSE_WITHIN: Duration = Duration::from_secs(5);

/// Dropped when the response body stream is dropped, i.e. when the server
/// gives up on the connection carrying it.
struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            let _ = signal.send(());
        }
    }
}

fn router(flood_dropped: Arc<tokio::sync::Mutex<Option<DropSignal>>>) -> axum::Router {
    axum::Router::new()
        .route("/ok", axum::routing::get(|| async { "ok" }))
        .route(
            "/stream",
            axum::routing::get(|| async {
                // One chunk, a silence three times the idle deadline, then a
                // steady trickle: a log follow on a quiet app looks like this.
                let chunks = async_stream_chunks(vec![
                    (Duration::ZERO, "first\n"),
                    (Duration::from_millis(900), "after-silence\n"),
                    (Duration::from_millis(100), "a\n"),
                    (Duration::from_millis(100), "b\n"),
                    (Duration::from_millis(100), "last\n"),
                ]);
                Body::from_stream(chunks)
            }),
        )
        .route(
            "/flood",
            axum::routing::get(move || {
                let flood_dropped = flood_dropped.clone();
                async move {
                    let guard = flood_dropped.lock().await.take();
                    let chunk = axum::body::Bytes::from(vec![b'x'; 64 * 1024]);
                    let stream = futures_util::stream::unfold(guard, move |guard| {
                        let chunk = chunk.clone();
                        async move { Some((Ok::<_, std::io::Error>(chunk), guard)) }
                    });
                    Body::from_stream(stream)
                }
            }),
        )
}

fn async_stream_chunks(
    plan: Vec<(Duration, &'static str)>,
) -> impl futures_util::Stream<Item = Result<&'static str, std::io::Error>> {
    futures_util::stream::unfold(plan.into_iter(), |mut plan| async move {
        let (delay, chunk) = plan.next()?;
        tokio::time::sleep(delay).await;
        Some((Ok(chunk), plan))
    })
}

async fn read_response_head<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 8192, "response head too long");
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

/// Wait for the server to close the connection; returns how long it took.
async fn wait_for_close<S: AsyncRead + Unpin>(stream: &mut S) -> Duration {
    let started = Instant::now();
    let mut buf = [0u8; 1024];
    let closed = tokio::time::timeout(CLOSE_WITHIN, async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "server kept the connection open");
    started.elapsed()
}

/// Send one keep-alive request and read its (short, `ok`) response.
async fn one_keep_alive_request<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) {
    stream
        .write_all(b"GET /ok HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let head = read_response_head(stream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let mut body = [0u8; 2];
    stream.read_exact(&mut body).await.unwrap();
    assert_eq!(&body, b"ok");
}

async fn plain_server(timeouts: ConnectionTimeouts) -> (SocketAddr, CancellationToken) {
    plain_server_with(timeouts, Arc::new(tokio::sync::Mutex::new(None))).await
}

async fn plain_server_with(
    timeouts: ConnectionTimeouts,
    flood_dropped: Arc<tokio::sync::Mutex<Option<DropSignal>>>,
) -> (SocketAddr, CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    tokio::spawn(serve_router_plain(
        listener,
        router(flood_dropped),
        timeouts,
        shutdown.clone(),
    ));
    (address, shutdown)
}

struct TlsServer {
    address: SocketAddr,
    connector: tokio_rustls::TlsConnector,
    shutdown: CancellationToken,
}

impl TlsServer {
    async fn start(timeouts: ConnectionTimeouts) -> Self {
        let (certificate, key) = tls::generate_self_signed_cert().unwrap();
        let config = tls::build_tls_config(vec![certificate.clone()], key).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        tokio::spawn(serve_router_over_tls(
            listener,
            tokio_rustls::TlsAcceptor::from(config),
            router(Arc::new(tokio::sync::Mutex::new(None))),
            timeouts,
            shutdown.clone(),
        ));
        Self {
            address,
            connector: connector_trusting(certificate),
            shutdown,
        }
    }

    async fn connect(&self) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
        self.connector
            .connect(
                rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                tokio::net::TcpStream::connect(self.address).await.unwrap(),
            )
            .await
            .unwrap()
    }
}

fn connector_trusting(
    certificate: rustls::pki_types::CertificateDer<'static>,
) -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

#[tokio::test]
async fn plain_listener_closes_many_idle_keep_alive_connections() {
    let (address, shutdown) = plain_server(quick_timeouts()).await;
    let mut clients = Vec::new();
    for _ in 0..40 {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        one_keep_alive_request(&mut stream).await;
        clients.push(stream);
    }
    for mut stream in clients {
        wait_for_close(&mut stream).await;
    }
    shutdown.cancel();
}

#[tokio::test]
async fn keep_alive_connection_serves_requests_until_it_goes_idle() {
    let (address, shutdown) = plain_server(quick_timeouts()).await;
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // Gaps shorter than the idle deadline keep the connection usable.
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        one_keep_alive_request(&mut stream).await;
    }
    let idle_for = wait_for_close(&mut stream).await;
    assert!(
        idle_for >= Duration::from_millis(200),
        "closed after {idle_for:?}, before the idle deadline"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn plain_listener_drops_connections_that_never_send_a_request() {
    let (address, shutdown) = plain_server(quick_timeouts()).await;
    let mut clients = Vec::new();
    for _ in 0..40 {
        clients.push(tokio::net::TcpStream::connect(address).await.unwrap());
    }
    for mut stream in clients {
        wait_for_close(&mut stream).await;
    }
    shutdown.cancel();
}

#[tokio::test]
async fn half_sent_request_head_is_dropped() {
    let (address, shutdown) = plain_server(quick_timeouts()).await;
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /ok HTTP/1.1\r\nHost:")
        .await
        .unwrap();
    wait_for_close(&mut stream).await;
    shutdown.cancel();
}

#[tokio::test]
async fn streaming_response_outlives_the_idle_deadline() {
    let (address, shutdown) = plain_server(quick_timeouts()).await;
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    let head = read_response_head(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut buf = [0u8; 256];
        while !String::from_utf8_lossy(&body).contains("last\n") {
            let read = stream.read(&mut buf).await.unwrap();
            assert!(
                read > 0,
                "stream cut short: {:?}",
                String::from_utf8_lossy(&body)
            );
            body.extend_from_slice(&buf[..read]);
        }
    })
    .await
    .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("after-silence"), "{body}");
    shutdown.cancel();
}

#[tokio::test]
async fn reader_that_stops_draining_a_stream_is_disconnected() {
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let signal = Arc::new(tokio::sync::Mutex::new(Some(DropSignal(Some(dropped_tx)))));
    let (address, shutdown) = plain_server_with(quick_timeouts(), signal).await;
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /flood HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    // Never read: the socket buffers fill and every server write stalls.
    tokio::time::timeout(CLOSE_WITHIN, dropped_rx)
        .await
        .expect("a stalled response must be abandoned")
        .unwrap();
    drop(stream);
    shutdown.cancel();
}

#[tokio::test]
async fn http2_connection_that_stops_answering_pings_is_closed() {
    let (address, shutdown) = plain_server(quick_timeouts()).await;
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // Connection preface plus an empty SETTINGS frame, then silence: a peer
    // that vanished behind a proxy never acknowledges the server's pings.
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .await
        .unwrap();
    wait_for_close(&mut stream).await;
    shutdown.cancel();
}

#[tokio::test]
async fn tls_listener_drops_a_client_that_never_handshakes() {
    let server = TlsServer::start(quick_timeouts()).await;
    let mut stream = tokio::net::TcpStream::connect(server.address)
        .await
        .unwrap();
    wait_for_close(&mut stream).await;
    server.shutdown.cancel();
}

#[tokio::test]
async fn tls_listener_closes_silent_and_idle_sessions() {
    let server = TlsServer::start(quick_timeouts()).await;
    let mut silent = Vec::new();
    let mut idle = Vec::new();
    for _ in 0..20 {
        silent.push(server.connect().await);
        let mut stream = server.connect().await;
        one_keep_alive_request(&mut stream).await;
        idle.push(stream);
    }
    for mut stream in silent.into_iter().chain(idle) {
        wait_for_close(&mut stream).await;
    }
    server.shutdown.cancel();
}

/// A proxy (like Lima's port forwarder) that forgets to close its upstream
/// side must not make Bun's open connections grow with every request.
#[tokio::test]
async fn forwarder_that_never_closes_upstream_does_not_accumulate_connections() {
    let (address, shutdown) = plain_server(quick_timeouts()).await;
    let mut leaked = Vec::new();
    for _ in 0..30 {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        one_keep_alive_request(&mut stream).await;
        // The forwarder keeps the socket alive but never uses it again.
        leaked.push(stream);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(quick_timeouts().request_head * 3).await;
    let mut still_open = 0;
    for mut stream in leaked {
        let mut byte = [0u8; 1];
        match tokio::time::timeout(Duration::from_millis(50), stream.read(&mut byte)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            _ => still_open += 1,
        }
    }
    assert_eq!(still_open, 0, "server still holds forwarded connections");
    shutdown.cancel();
}

async fn ingress(
    timeouts: ConnectionTimeouts,
) -> (
    reliaburger::wrapper::proxy::BoundProxy,
    tokio_rustls::TlsConnector,
    CancellationToken,
) {
    let (certificate, key) = tls::generate_self_signed_cert().unwrap();
    let config = tls::build_tls_config(vec![certificate.clone()], key).unwrap();
    let shutdown = CancellationToken::new();
    let proxy = bind_proxy_with_tls(
        types::WrapperConfig {
            http_port: 0,
            https_port: 0,
            tls_handshake_timeout: timeouts.tls_handshake,
            ..Default::default()
        },
        Arc::new(tokio::sync::RwLock::new(RoutingTable::new())),
        None,
        Some(config.cert_resolver.clone()),
        None,
        shutdown.clone(),
    )
    .await
    .unwrap()
    .with_connection_timeouts(timeouts);
    (proxy, connector_trusting(certificate), shutdown)
}

async fn unknown_host_request<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S) {
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: unknown.example\r\n\r\n")
        .await
        .unwrap();
    let head = read_response_head(stream).await;
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).await.unwrap();
}

#[tokio::test]
async fn ingress_closes_idle_and_silent_connections_on_both_listeners() {
    let (proxy, connector, shutdown) = ingress(quick_timeouts()).await;
    let http = (std::net::Ipv4Addr::LOCALHOST, proxy.http_addr.port());
    let https = (std::net::Ipv4Addr::LOCALHOST, proxy.https_addr.port());
    tokio::spawn(proxy.serve());

    let mut plain_idle = tokio::net::TcpStream::connect(http).await.unwrap();
    unknown_host_request(&mut plain_idle).await;
    let mut plain_silent = tokio::net::TcpStream::connect(http).await.unwrap();
    let mut no_handshake = tokio::net::TcpStream::connect(https).await.unwrap();
    let mut tls_idle = connector
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tokio::net::TcpStream::connect(https).await.unwrap(),
        )
        .await
        .unwrap();
    unknown_host_request(&mut tls_idle).await;

    wait_for_close(&mut plain_idle).await;
    wait_for_close(&mut plain_silent).await;
    wait_for_close(&mut no_handshake).await;
    wait_for_close(&mut tls_idle).await;
    shutdown.cancel();
}

#[tokio::test]
async fn ingress_websocket_stays_open_past_the_idle_deadline() {
    use reliaburger::config::app::IngressSpec;
    use reliaburger::onion::{
        service_id::ServiceId, service_map::ServiceMap, types::BackendInstance,
    };

    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = backend.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut socket, _) = backend.accept().await.unwrap();
        let head = read_response_head(&mut socket).await;
        assert!(head.to_lowercase().contains("upgrade: websocket"));
        socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n").await.unwrap();
        let (mut read, mut write) = socket.split();
        let _ = tokio::io::copy(&mut read, &mut write).await;
    });
    let mut services = ServiceMap::new();
    services.register_app("web", "default", port, None).unwrap();
    services
        .add_backend(
            &ServiceId::new("default", "web"),
            BackendInstance {
                instance_id: "web-0".into(),
                node_ip: std::net::Ipv4Addr::LOCALHOST,
                host_port: port,
                healthy: true,
                local: false,
            },
        )
        .unwrap();
    let mut routes = RoutingTable::new();
    routes
        .rebuild(
            &services,
            &std::collections::HashMap::from([(
                ("default".into(), "web".into()),
                IngressSpec {
                    host: "web.example".into(),
                    path: None,
                    tls: None,
                    websocket: Some(true),
                    rate_limit_rps: None,
                    rate_limit_burst: None,
                },
            )]),
        )
        .unwrap();
    let shutdown = CancellationToken::new();
    let proxy = reliaburger::wrapper::proxy::bind_proxy(
        types::WrapperConfig {
            http_port: 0,
            https_port: 0,
            ..Default::default()
        },
        Arc::new(tokio::sync::RwLock::new(routes)),
        shutdown.clone(),
    )
    .await
    .unwrap()
    .with_connection_timeouts(quick_timeouts());
    let http = (std::net::Ipv4Addr::LOCALHOST, proxy.http_addr.port());
    tokio::spawn(proxy.serve());

    let mut stream = tokio::net::TcpStream::connect(http).await.unwrap();
    stream.write_all(b"GET /socket HTTP/1.1\r\nHost: web.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap();
    let head = read_response_head(&mut stream).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    // Quiet for several idle deadlines, as a chat socket often is.
    tokio::time::sleep(quick_timeouts().request_head * 4).await;
    stream.write_all(b"ping").await.unwrap();
    let mut echo = [0; 4];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut echo))
        .await
        .expect("websocket closed by the idle deadline")
        .unwrap();
    assert_eq!(&echo, b"ping");
    shutdown.cancel();
}
