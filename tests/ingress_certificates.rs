//! Real TLS handshakes for the ingress certificate lifecycle.

use std::sync::Arc;

fn server_config(
    lifetime: std::time::Duration,
) -> (Arc<rustls::ServerConfig>, rustls::RootCertStore) {
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
        lifetime,
        Arc::new(tokio::sync::RwLock::new(routes)),
        vec![default_certificate],
        default_key,
    )
    .unwrap();
    let server_config =
        reliaburger::wrapper::tls::build_tls_config_with_resolver(Arc::new(resolver)).unwrap();
    (server_config, roots)
}

async fn handshake(
    server_config: Arc<rustls::ServerConfig>,
    roots: rustls::RootCertStore,
) -> Vec<rustls::pki_types::CertificateDer<'static>> {
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
    let chain = client.get_ref().1.peer_certificates().unwrap().to_vec();
    assert!(server.await.unwrap().is_ok());
    chain
}

#[tokio::test]
async fn cluster_ingress_sends_the_issuer_chain_to_root_trusting_clients() {
    let (config, roots) = server_config(std::time::Duration::from_secs(3600));
    assert_eq!(handshake(config, roots).await.len(), 2);
}

#[tokio::test]
async fn cached_ingress_leaves_renew_before_expiry_and_after_an_idle_expiry() {
    // Validity is whole wall-clock seconds, so these times cannot be faked.
    // An eight-second leaf is due for renewal four seconds after issue, so
    // the cached handshake has over two seconds of margin, and the renewal
    // below lands roughly two seconds before the original expires.
    let (config, roots) = server_config(std::time::Duration::from_secs(8));
    let first = handshake(config.clone(), roots.clone()).await;
    let cached = handshake(config.clone(), roots.clone()).await;
    assert_eq!(
        first, cached,
        "ordinary cache hits should retain the current certificate"
    );
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let renewed = handshake(config.clone(), roots.clone()).await;
    assert_ne!(
        first[0], renewed[0],
        "renew before the original eight-second leaf expires"
    );
    assert_eq!(
        first[1], renewed[1],
        "renewal must retain the trusted issuer"
    );
    // Longer than the renewed leaf's whole lifetime.
    tokio::time::sleep(std::time::Duration::from_secs(9)).await;
    let after_idle = handshake(config, roots).await;
    assert_ne!(
        renewed[0], after_idle[0],
        "an expired cache entry must never be served"
    );
}
