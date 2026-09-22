//! The lifetime limit must survive a real HTTPS-to-WebSocket upgrade.

use std::sync::Arc;
use std::time::Duration;

use reliaburger::config::app::IngressSpec;
use reliaburger::onion::{service_id::ServiceId, service_map::ServiceMap, types::BackendInstance};
use reliaburger::wrapper::{
    proxy::bind_proxy_with_tls, routing::RoutingTable, tls, types::WrapperConfig,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

async fn headers<S: AsyncRead + Unpin>(stream: &mut S) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 8192);
        bytes.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn upgraded_ingress_connection_retires_and_a_new_connection_still_works() {
    let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = backend.local_addr().unwrap().port();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        let (mut socket, _) = backend.accept().await.unwrap();
        assert!(headers(&mut socket).await.to_lowercase().contains("upgrade: websocket"));
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
                    tls: Some("explicit".into()),
                    websocket: Some(true),
                    rate_limit_rps: None,
                    rate_limit_burst: None,
                },
            )]),
        )
        .unwrap();
    let (certificate, key) = tls::generate_self_signed_cert().unwrap();
    let config = tls::build_tls_config(vec![certificate.clone()], key).unwrap();
    let shutdown = CancellationToken::new();
    let proxy = bind_proxy_with_tls(
        WrapperConfig {
            http_port: 0,
            https_port: 0,
            ..Default::default()
        },
        Arc::new(tokio::sync::RwLock::new(routes)),
        None,
        Some(config.cert_resolver.clone()),
        shutdown.clone(),
    )
    .await
    .unwrap();
    let address = (std::net::Ipv4Addr::LOCALHOST, proxy.https_addr.port());
    tasks.spawn(async move { proxy.serve().await.unwrap() });
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate).unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let mut stream = connector
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tokio::net::TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    stream.write_all(b"GET /socket HTTP/1.1\r\nHost: web.example\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap();
    assert!(headers(&mut stream).await.starts_with("HTTP/1.1 101"));
    stream.write_all(b"ping").await.unwrap();
    let mut echo = [0; 4];
    stream.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"ping");

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3601)).await;
    tokio::time::resume();
    let result = tokio::time::timeout(Duration::from_secs(1), stream.read_u8()).await;
    assert!(
        result.is_ok(),
        "an upgraded stream must retire within the connection lifetime"
    );
    assert!(
        result.unwrap().is_err(),
        "the retired stream cannot deliver more bytes"
    );

    let mut fresh = connector
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tokio::net::TcpStream::connect(address).await.unwrap(),
        )
        .await
        .unwrap();
    fresh
        .write_all(b"GET / HTTP/1.1\r\nHost: unknown.example\r\n\r\n")
        .await
        .unwrap();
    assert!(headers(&mut fresh).await.starts_with("HTTP/1.1 404"));
    shutdown.cancel();
}
