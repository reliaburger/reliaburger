//! The agent API served over mTLS.
//!
//! Proves the API server config (optional client auth) and the cluster HTTP
//! client (CA-trusting, no client cert) interoperate over a real HTTPS round
//! trip — the "relish/browser without a node cert" case the API must accept.

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
                    let service = match make_service.call(()).await {
                        Ok(s) => s,
                        Err(infallible) => match infallible {},
                    };
                    tokio::spawn(async move {
                        let Ok(tls) = acceptor.accept(tcp).await else { return };
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
async fn api_serves_https_and_accepts_a_client_without_a_client_cert() {
    let hierarchy = ca::generate_ca_hierarchy("api-tls-test", b"ikm").unwrap();
    let server_id = identity(&hierarchy, "node-01", 10);

    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&server_id, CrlHandle::default()).unwrap(),
    );
    let shutdown = CancellationToken::new();
    let addr = spawn_tls_api(acceptor, shutdown.clone()).await;

    // The cluster HTTP client trusts the cluster CA and presents no client
    // certificate — exactly how a peer or relish reaches the API under mTLS.
    let client = build_cluster_http_client(&server_id).unwrap();
    let resp = client
        .get(format!("https://{addr}/ping"))
        .send()
        .await
        .expect("HTTPS request should succeed");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "pong");

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
    let addr = spawn_tls_api(acceptor, shutdown.clone()).await;

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
