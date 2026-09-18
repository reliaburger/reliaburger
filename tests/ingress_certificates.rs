//! Real TLS handshakes for the ingress certificate lifecycle.

use std::sync::Arc;

#[tokio::test]
async fn cluster_ingress_sends_the_issuer_chain_to_root_trusting_clients() {
    let hierarchy =
        reliaburger::sesame::ca::generate_ca_hierarchy("ingress-chain", b"test-ikm").unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            hierarchy.root.ca.certificate_der.clone(),
        ))
        .unwrap();
    let mut map = reliaburger::onion::service_map::ServiceMap::new();
    map.register_app("web", "default", 8080, None).unwrap();
    let mut routes = reliaburger::wrapper::routing::RoutingTable::new();
    routes
        .rebuild(
            &map,
            &std::collections::HashMap::from([(
                ("default".into(), "web".into()),
                reliaburger::config::app::IngressSpec {
                    host: "web.example".into(),
                    path: None,
                    tls: Some("cluster".into()),
                    websocket: None,
                    rate_limit_rps: None,
                    rate_limit_burst: None,
                },
            )]),
        )
        .unwrap();
    let (default_certificate, default_key) =
        reliaburger::wrapper::tls::generate_self_signed_cert().unwrap();
    let resolver = reliaburger::wrapper::tls::IngressCertResolver::new(
        hierarchy.ingress.signing_keypair,
        hierarchy.ingress.certificate_params,
        rustls::pki_types::CertificateDer::from(hierarchy.ingress.ca.certificate_der),
        std::time::Duration::from_secs(3600),
        Arc::new(tokio::sync::RwLock::new(routes)),
        vec![default_certificate],
        default_key,
    )
    .unwrap();
    let server_config =
        reliaburger::wrapper::tls::build_tls_config_with_resolver(Arc::new(resolver)).unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), acceptor.accept(socket))
            .await
            .unwrap()
    });
    let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        connector.connect(
            rustls::pki_types::ServerName::try_from("web.example").unwrap(),
            socket,
        ),
    )
    .await
    .unwrap();
    assert!(
        result.is_ok(),
        "the cluster root alone must validate the served chain: {result:?}"
    );
    let client = result.unwrap();
    assert_eq!(client.get_ref().1.peer_certificates().unwrap().len(), 2);
    assert!(server.await.unwrap().is_ok());
}
