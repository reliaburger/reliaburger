//! Operator certificate replacement through a running Wrapper listener.

use std::sync::Arc;
use std::time::Duration;

use reliaburger::wrapper::{proxy::bind_proxy, routing::RoutingTable, types::WrapperConfig};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_util::sync::CancellationToken;

use crate::task_harness;

async fn connect(
    address: std::net::SocketAddr,
    roots: &rustls::RootCertStore,
) -> TlsStream<TcpStream> {
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots.clone())
    .with_no_client_auth();
    tokio::time::timeout(Duration::from_secs(3), async {
        TlsConnector::from(Arc::new(config))
            .connect(
                ServerName::try_from("web.example").unwrap(),
                TcpStream::connect(address).await.unwrap(),
            )
            .await
            .unwrap()
    })
    .await
    .unwrap()
}

fn leaf(stream: &TlsStream<TcpStream>) -> CertificateDer<'static> {
    stream.get_ref().1.peer_certificates().unwrap()[0].clone()
}

async fn request(stream: &mut TlsStream<TcpStream>) {
    tokio::time::timeout(Duration::from_secs(3), async {
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: web.example\r\n\r\n")
            .await
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            assert!(headers.len() < 4096);
            headers.push(stream.read_u8().await.unwrap());
        }
        assert!(headers.starts_with(b"HTTP/1.1 404"));
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_files_reload_only_as_a_valid_pair_and_keep_existing_connections() {
    let first = rcgen::generate_simple_self_signed(vec!["web.example".into()]).unwrap();
    let second = rcgen::generate_simple_self_signed(vec!["web.example".into()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(first.cert.der().clone()).unwrap();
    roots.add(second.cert.der().clone()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cert_path = directory.path().join("cert.pem");
    let key_path = directory.path().join("key.pem");
    std::fs::write(&cert_path, first.cert.pem()).unwrap();
    std::fs::write(&key_path, first.key_pair.serialize_pem()).unwrap();
    let shutdown = CancellationToken::new();
    let proxy = bind_proxy(
        WrapperConfig {
            http_port: 0,
            https_port: 0,
            tls_cert_path: Some(cert_path.clone()),
            tls_key_path: Some(key_path.clone()),
            ..Default::default()
        },
        Arc::new(tokio::sync::RwLock::new(RoutingTable::new())),
        shutdown.clone(),
    )
    .await
    .unwrap();
    let address = (std::net::Ipv4Addr::LOCALHOST, proxy.https_addr.port()).into();
    let task = tokio::spawn(async move { proxy.serve().await.unwrap() });
    let _tasks = task_harness::TestTasks::new(shutdown, vec![task]);
    let mut existing = connect(address, &roots).await;
    request(&mut existing).await;
    assert_eq!(leaf(&existing), *first.cert.der());

    // A non-atomic operator update must never install the new cert with the old key.
    std::fs::write(&cert_path, second.cert.pem()).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(leaf(&connect(address, &roots).await), *first.cert.der());
    std::fs::write(&key_path, second.key_pair.serialize_pem()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if leaf(&connect(address, &roots).await) == *second.cert.der() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("a matching replacement must become visible without restarting Wrapper");
    request(&mut existing).await;
    assert_eq!(leaf(&existing), *first.cert.der());

    // Bad replacements must leave the previously validated bundle available.
    std::fs::write(&cert_path, b"not a certificate").unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(leaf(&connect(address, &roots).await), *second.cert.der());
    drop(existing);
}

#[tokio::test]
async fn operator_certificate_must_be_currently_valid_before_binding() {
    for offset in [-120, 120] {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["web.example".into()]).unwrap();
        params.not_before = time::OffsetDateTime::now_utc() + time::Duration::seconds(offset);
        params.not_after = params.not_before + time::Duration::seconds(60);
        let certificate = params.self_signed(&key).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let cert_path = directory.path().join("cert.pem");
        let key_path = directory.path().join("key.pem");
        std::fs::write(&cert_path, certificate.pem()).unwrap();
        std::fs::write(&key_path, key.serialize_pem()).unwrap();
        let result = bind_proxy(
            WrapperConfig {
                http_port: 0,
                https_port: 0,
                tls_cert_path: Some(cert_path),
                tls_key_path: Some(key_path),
                ..Default::default()
            },
            Arc::new(tokio::sync::RwLock::new(RoutingTable::new())),
            CancellationToken::new(),
        )
        .await;
        assert!(
            result.is_err(),
            "refuse a certificate outside its validity window"
        );
    }
}

#[test]
fn operator_pem_inputs_are_bounded_regular_files() {
    let certificate = rcgen::generate_simple_self_signed(vec!["web.example".into()]).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cert_path = directory.path().join("cert.pem");
    let key_path = directory.path().join("key.pem");
    std::fs::write(&key_path, certificate.key_pair.serialize_pem()).unwrap();
    let oversized = format!("{}{}", certificate.cert.pem(), " ".repeat(1024 * 1024));
    std::fs::write(&cert_path, oversized).unwrap();
    let error = reliaburger::wrapper::tls::load_certs_from_disk(&cert_path, &key_path).unwrap_err();
    assert!(error.to_string().contains("exceeds 1 MiB"));
    #[cfg(unix)]
    {
        let fifo = directory.path().join("cert.fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let error = reliaburger::wrapper::tls::load_certs_from_disk(&fifo, &key_path).unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }
}
