//! Reconnecting clients must validate the current ingress certificate.

use std::sync::Arc;
use std::time::Duration;

use reliaburger::wrapper::tls::{
    build_tls_config, build_tls_config_with_resolver, generate_self_signed_cert,
};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn ingress_reconnections_use_full_handshakes_for_current_certificate_validation() {
    let (certificate, key) = generate_self_signed_cert().unwrap();
    let static_config = build_tls_config(vec![certificate.clone()], key).unwrap();
    let resolved_config =
        build_tls_config_with_resolver(static_config.cert_resolver.clone()).unwrap();
    for (config, version) in [
        (static_config.clone(), &rustls::version::TLS12),
        (resolved_config.clone(), &rustls::version::TLS12),
        (static_config, &rustls::version::TLS13),
        (resolved_config, &rustls::version::TLS13),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            for _ in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(socket).await.unwrap();
                stream.write_all(b"ready").await.unwrap();
                stream.flush().await.unwrap();
            }
        });
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certificate.clone()).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[version])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        for _ in 0..2 {
            let kind = tokio::time::timeout(Duration::from_secs(3), async {
                let socket = tokio::net::TcpStream::connect(address).await.unwrap();
                let mut stream = connector
                    .connect(ServerName::try_from("localhost").unwrap(), socket)
                    .await
                    .unwrap();
                let mut message = [0; 5];
                // Consume post-handshake tickets, so the shared client can try resumption.
                stream.read_exact(&mut message).await.unwrap();
                assert_eq!(&message, b"ready");
                stream.get_ref().1.handshake_kind()
            })
            .await
            .unwrap();
            assert_eq!(
                kind,
                Some(rustls::HandshakeKind::Full),
                "resumption must not bypass the current resolver and certificate validity"
            );
        }
        server.await.unwrap();
    }
}
