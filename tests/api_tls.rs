//! The agent API served over mTLS.
//!
//! Proves the API server config and cluster HTTP client complete a mutually
//! authenticated HTTPS round trip with issued node identities.

use std::time::{Duration, SystemTime};

use axum::Router;
use axum::routing::get;
use tokio_util::sync::CancellationToken;

use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::{CrlHandle, build_api_server_config, build_cluster_http_client};
use reliaburger::sesame::types::SerialNumber;

fn identity(hierarchy: &CaHierarchy, node_id: &str, serial: u64) -> NodeIdentity {
    let (cert_der, key_der, serial) = ca::issue_node_cert(
        node_id,
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let now = SystemTime::now();
    NodeIdentity {
        node_id: node_id.to_string(),
        certificate_der: cert_der,
        private_key_der: key_der,
        serial,
        ca_generation: 0,
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: now,
        not_after: now + Duration::from_secs(365 * 24 * 3600),
    }
}

/// Serve a tiny router over TLS on an ephemeral port; return its address.
async fn spawn_tls_api(
    acceptor: tokio_rustls::TlsAcceptor,
    shutdown: CancellationToken,
    saw_client_certificate: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::net::SocketAddr {
    use tower::Service;

    let router = Router::new().route("/ping", get(|| async { "pong" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let mut make_service = router.into_make_service();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                accepted = listener.accept() => {
                    let Ok((tcp, _)) = accepted else { continue };
                    let acceptor = acceptor.clone();
                    let saw_client_certificate = saw_client_certificate.clone();
                    let service = match make_service.call(()).await {
                        Ok(s) => s,
                        Err(infallible) => match infallible {},
                    };
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
                        saw_client_certificate.store(
                            tls.get_ref()
                                .1
                                .peer_certificates()
                                .is_some_and(|certificates| !certificates.is_empty()),
                            std::sync::atomic::Ordering::SeqCst,
                        );
                        let svc = hyper_util::service::TowerToHyperService::new(service);
                        let _ = hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
                        .await;
                    });
                }
            }
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_api_round_trip_presents_the_calling_nodes_identity() {
    let hierarchy = ca::generate_ca_hierarchy("api-tls-test", b"ikm").unwrap();
    let server_id = identity(&hierarchy, "node-01", 10);
    let client_id = identity(&hierarchy, "node-02", 11);

    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&server_id, CrlHandle::default()).unwrap(),
    );
    let shutdown = CancellationToken::new();
    let saw_client_certificate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let addr = spawn_tls_api(acceptor, shutdown.clone(), saw_client_certificate.clone()).await;

    let client = build_cluster_http_client(&client_id, CrlHandle::default()).unwrap();
    let resp = client
        .get(format!("https://{addr}/ping"))
        .send()
        .await
        .expect("HTTPS request should succeed");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "pong");
    assert!(
        saw_client_certificate.load(std::sync::atomic::Ordering::SeqCst),
        "the peer API call completed without presenting its configured node identity"
    );

    shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_does_not_trust_the_cluster_ca_is_refused() {
    let hierarchy = ca::generate_ca_hierarchy("api-tls-test", b"ikm").unwrap();
    let server_id = identity(&hierarchy, "node-01", 10);

    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&server_id, CrlHandle::default()).unwrap(),
    );
    let shutdown = CancellationToken::new();
    let addr = spawn_tls_api(
        acceptor,
        shutdown.clone(),
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    )
    .await;

    // A stock client doesn't trust our private CA, so the TLS handshake fails.
    let stock = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert!(
        stock
            .get(format!("https://{addr}/ping"))
            .send()
            .await
            .is_err(),
        "a client that doesn't trust the cluster CA must be refused"
    );

    shutdown.cancel();
}
