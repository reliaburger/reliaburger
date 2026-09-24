//! Existing TLS configs must see renewed node credentials on new connections.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reliaburger::sesame::{
    ca, credentials::LiveNodeIdentity, identity_store::NodeIdentity, mtls, types::SerialNumber,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn identity(hierarchy: &ca::CaHierarchy, node: &str, serial: u64) -> NodeIdentity {
    let (certificate_der, private_key_der, serial) = ca::issue_node_cert(
        node,
        SerialNumber(serial),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    NodeIdentity {
        node_id: node.into(),
        certificate_der,
        private_key_der,
        serial,
        ca_generation: 0,
        node_ca_der: hierarchy.node.ca.certificate_der.clone(),
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        not_before: SystemTime::UNIX_EPOCH,
        not_after: SystemTime::UNIX_EPOCH,
    }
}

fn installed(directory: &std::path::Path, identity: &NodeIdentity) -> LiveNodeIdentity {
    reliaburger::sesame::identity_store::save(directory, identity).unwrap();
    LiveNodeIdentity::load(directory).unwrap()
}

async fn handshake(
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
) -> (Vec<u8>, Vec<u8>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = tokio_rustls::TlsAcceptor::from(server)
                .accept(socket)
                .await
                .unwrap();
            let peer = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
            stream.write_all(b"ready").await.unwrap();
            stream.flush().await.unwrap();
            peer
        };
        let client = async {
            let socket = tokio::net::TcpStream::connect(address).await.unwrap();
            let name = rustls::pki_types::ServerName::IpAddress(address.ip().into());
            let mut stream = tokio_rustls::TlsConnector::from(client)
                .connect(name, socket)
                .await
                .unwrap();
            let mut ready = [0; 5];
            stream.read_exact(&mut ready).await.unwrap();
            assert_eq!(&ready, b"ready");
            stream.get_ref().1.peer_certificates().unwrap()[0].to_vec()
        };
        tokio::join!(server, client)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn existing_server_and_client_configs_observe_replacement_credentials() {
    let hierarchy = ca::generate_ca_hierarchy("live-credentials", b"test-ikm").unwrap();
    let server_directory = tempfile::tempdir().unwrap();
    let client_directory = tempfile::tempdir().unwrap();
    let server = installed(server_directory.path(), &identity(&hierarchy, "server", 10));
    let client = installed(client_directory.path(), &identity(&hierarchy, "client", 11));
    let servers = [
        mtls::build_live_mtls_server_config(&server, mtls::CrlHandle::default()).unwrap(),
        mtls::build_live_api_server_config(&server, mtls::CrlHandle::default()).unwrap(),
    ];
    let clients = [
        mtls::build_live_mtls_client_config(&client, mtls::CrlHandle::default(), Some("server"))
            .unwrap(),
        mtls::build_live_mtls_client_config(&client, mtls::CrlHandle::default(), None).unwrap(),
    ];
    for server_config in &servers {
        for client_config in &clients {
            let (client_leaf, server_leaf) =
                handshake(server_config.clone(), client_config.clone()).await;
            assert_eq!(client_leaf, client.snapshot().certificate_der);
            assert_eq!(server_leaf, server.snapshot().certificate_der);
        }
    }
    let renewed_server = identity(&hierarchy, "server", 20);
    let renewed_client = identity(&hierarchy, "client", 21);
    server.replace(renewed_server.clone()).await.unwrap();
    client.replace(renewed_client.clone()).await.unwrap();
    for server_config in &servers {
        for client_config in &clients {
            let (client_leaf, server_leaf) =
                handshake(server_config.clone(), client_config.clone()).await;
            assert_eq!(client_leaf, renewed_client.certificate_der);
            assert_eq!(server_leaf, renewed_server.certificate_der);
        }
    }
}

#[tokio::test]
async fn replacement_refuses_changed_identity_trust_anchors_keys_or_stale_serials() {
    let hierarchy = ca::generate_ca_hierarchy("live-credentials", b"test-ikm").unwrap();
    let original = identity(&hierarchy, "node", 10);
    let directory = tempfile::tempdir().unwrap();
    let live = installed(directory.path(), &original);
    let mut wrong_key = identity(&hierarchy, "node", 20);
    wrong_key.private_key_der = original.private_key_der.clone();
    let foreign = ca::generate_ca_hierarchy("other-cluster", b"other-ikm").unwrap();
    for replacement in [
        identity(&hierarchy, "different-node", 20),
        identity(&foreign, "node", 20),
        wrong_key,
        identity(&hierarchy, "node", 9),
        identity(&hierarchy, "node", 10),
    ] {
        assert!(live.replace(replacement).await.is_err());
        assert_eq!(live.snapshot().certificate_der, original.certificate_der);
    }
    live.replace(original.clone()).await.unwrap();
    assert_eq!(live.snapshot().certificate_der, original.certificate_der);
}

#[tokio::test]
async fn failed_persistence_never_publishes_replacement_credentials() {
    let hierarchy = ca::generate_ca_hierarchy("live-credentials", b"test-ikm").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let original = identity(&hierarchy, "node", 10);
    let live = installed(directory.path(), &original);
    let key_path = directory.path().join("node.key");
    std::fs::remove_file(&key_path).unwrap();
    std::fs::create_dir(&key_path).unwrap();
    let replacement = identity(&hierarchy, "node", 20);
    assert!(live.replace(replacement.clone()).await.is_err());
    assert_eq!(live.snapshot().certificate_der, original.certificate_der);
    assert_eq!(
        reliaburger::sesame::identity_store::load(directory.path())
            .unwrap()
            .unwrap()
            .certificate_der,
        original.certificate_der
    );
    std::fs::remove_dir(&key_path).unwrap();
    live.replace(replacement.clone()).await.unwrap();
    assert_eq!(live.snapshot().certificate_der, replacement.certificate_der);
    assert_eq!(
        reliaburger::sesame::identity_store::load(directory.path())
            .unwrap()
            .unwrap()
            .certificate_der,
        replacement.certificate_der
    );
}

#[tokio::test]
async fn expired_client_identity_is_rejected_by_optional_mtls_instead_of_omitted() {
    let hierarchy = ca::generate_ca_hierarchy("live-expiry", b"test-ikm").unwrap();
    let server_identity = identity(&hierarchy, "server", 10);
    let mut client_identity = identity(&hierarchy, "client", 11);
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["client".into()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "client");
    params.subject_alt_names.push(rcgen::SanType::URI(
        ca::node_spiffe_uri("client").try_into().unwrap(),
    ));
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&11u64.to_be_bytes()));
    params.extended_key_usages = vec![
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        rcgen::ExtendedKeyUsagePurpose::ClientAuth,
    ];
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::seconds(1);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::seconds(5);
    let issuer = hierarchy
        .node
        .certificate_params
        .clone()
        .self_signed(&hierarchy.node.signing_keypair)
        .unwrap();
    client_identity.certificate_der = params
        .signed_by(&key, &issuer, &hierarchy.node.signing_keypair)
        .unwrap()
        .der()
        .to_vec();
    client_identity.private_key_der = key.serialize_der();
    let directory = tempfile::tempdir().unwrap();
    let live = installed(directory.path(), &client_identity);
    let server =
        mtls::build_api_server_config(&server_identity, mtls::CrlHandle::default()).unwrap();
    let mut client =
        (*mtls::build_mtls_client_config(&live.snapshot(), mtls::CrlHandle::default()).unwrap())
            .clone();
    client.client_auth_cert_resolver = Arc::new(live);
    let client = Arc::new(client);
    handshake(server.clone(), client.clone()).await;
    tokio::time::sleep(Duration::from_secs(6)).await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (server_error, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            match tokio_rustls::TlsAcceptor::from(server).accept(socket).await {
                Ok(mut stream) => {
                    let _ = stream.write_all(b"accepted").await;
                    None
                }
                Err(error) => Some(error.to_string()),
            }
        };
        let client = async {
            let socket = tokio::net::TcpStream::connect(address).await.unwrap();
            if let Ok(mut stream) = tokio_rustls::TlsConnector::from(client)
                .connect(
                    rustls::pki_types::ServerName::IpAddress(address.ip().into()),
                    socket,
                )
                .await
            {
                let _ = stream.read_u8().await;
            }
        };
        tokio::join!(server, client)
    })
    .await
    .unwrap();
    assert!(
        server_error
            .as_ref()
            .is_some_and(|error| error.to_lowercase().contains("expired")),
        "an expired identity must fail TLS, never become anonymous client auth: {server_error:?}"
    );
}

#[tokio::test]
async fn concurrent_replacements_cannot_roll_back_the_persisted_or_live_serial() {
    let hierarchy = ca::generate_ca_hierarchy("live-credentials", b"test-ikm").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let live = installed(directory.path(), &identity(&hierarchy, "node", 10));
    let older = identity(&hierarchy, "node", 20);
    let newer = identity(&hierarchy, "node", 30);
    let (_older_result, newer_result) = tokio::join!(live.replace(older), live.replace(newer));
    newer_result.unwrap();
    assert_eq!(live.snapshot().serial, SerialNumber(30));
    assert_eq!(
        reliaburger::sesame::identity_store::load(directory.path())
            .unwrap()
            .unwrap()
            .serial,
        SerialNumber(30)
    );
    let newer = identity(&hierarchy, "node", 50);
    let older = identity(&hierarchy, "node", 40);
    let (newer_result, older_result) = tokio::join!(live.replace(newer), live.replace(older));
    newer_result.unwrap();
    assert!(older_result.is_err());
    assert_eq!(live.snapshot().serial, SerialNumber(50));
    assert_eq!(
        reliaburger::sesame::identity_store::load(directory.path())
            .unwrap()
            .unwrap()
            .serial,
        SerialNumber(50)
    );
}

#[tokio::test]
async fn existing_internal_http_client_presents_replacement_and_keeps_service_token() {
    let hierarchy = ca::generate_ca_hierarchy("live-http", b"test-ikm").unwrap();
    let server_directory = tempfile::tempdir().unwrap();
    let client_directory = tempfile::tempdir().unwrap();
    let server = installed(server_directory.path(), &identity(&hierarchy, "server", 10));
    let client = installed(client_directory.path(), &identity(&hierarchy, "client", 11));
    let acceptor = tokio_rustls::TlsAcceptor::from(
        mtls::build_live_api_server_config(&server, mtls::CrlHandle::default()).unwrap(),
    );
    let http = mtls::build_live_cluster_http_client(
        &client,
        mtls::CrlHandle::default(),
        Some("internal-test-token"),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://{}/probe", listener.local_addr().unwrap());
    for serial in [11, 21] {
        if serial == 21 {
            client
                .replace(identity(&hierarchy, "client", serial))
                .await
                .unwrap();
            server
                .replace(identity(&hierarchy, "server", 20))
                .await
                .unwrap();
        }
        let serve = async {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(tcp).await.unwrap();
            let leaf = tls.get_ref().1.peer_certificates().unwrap()[0].to_vec();
            assert_eq!(leaf, client.snapshot().certificate_der);
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8192);
                request.push(tls.read_u8().await.unwrap());
            }
            assert!(
                String::from_utf8(request)
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains("authorization: bearer internal-test-token\r\n")
            );
            // Force a fresh handshake on the next request using the same client.
            tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            tls.flush().await.unwrap();
        };
        let request = async {
            assert_eq!(
                http.get(&url)
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
                "ok"
            );
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(serve, request);
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn diagnostics_reads_the_current_node_leaf_after_replacement() {
    use axum::{body::Body, http::Request};
    use reliaburger::bun::diagnostics::{DiagnosticSource, LocalDiagnosticSnapshot};
    use tower::ServiceExt;

    let hierarchy = ca::generate_ca_hierarchy("live-diagnostics", b"test-ikm").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let live = installed(directory.path(), &identity(&hierarchy, "node", 10));
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let router = reliaburger::bun::api::router(
        tx, None, None, None, None, None, None, None, None, None, None, None, 0, None,
    )
    .layer(axum::Extension(live.clone()));
    for serial in [10, 20] {
        if serial == 20 {
            live.replace(identity(&hierarchy, "node", serial))
                .await
                .unwrap();
        }
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/diagnostics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let snapshot: LocalDiagnosticSnapshot = serde_json::from_slice(&body).unwrap();
        let DiagnosticSource::Available { value, .. } = snapshot.certificates else {
            panic!("live node certificate metadata is unavailable");
        };
        let current = live.snapshot();
        let expected = reliaburger::bun::diagnostics::public_certificate_metadata(
            "node",
            &current.node_id,
            &current.certificate_der,
            "manual",
            false,
        )
        .unwrap();
        assert_eq!(value, vec![expected]);
    }
}
