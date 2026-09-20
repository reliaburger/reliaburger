//! Authenticated catalogue uploads retain same-origin OCI compatibility.

use std::sync::Arc;

#[tokio::test]
async fn catalogue_uploads_accept_relative_and_same_origin_absolute_locations() {
    for absolute in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let state = registry_state(root.path());
        let mut router = reliaburger::pickle::api::router(state);
        if absolute {
            let origin = origin.clone();
            router = router.layer(axum::middleware::map_response(
                move |mut response: axum::response::Response| {
                    let origin = origin.clone();
                    async move {
                        if let Some(location) = response.headers().get(axum::http::header::LOCATION)
                        {
                            let location = format!("{origin}{}", location.to_str().unwrap());
                            response
                                .headers_mut()
                                .insert(axum::http::header::LOCATION, location.parse().unwrap());
                        }
                        response
                    }
                },
            ));
        }
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            "Bearer fixture-registry-token".parse().unwrap(),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let image = reliaburger::testkit::oci::build_synthetic_image("origin");
        let pushed =
            reliaburger::testkit::oci::push_image(&http, &origin, "test", "v1", &image).await;
        assert!(pushed.is_ok(), "absolute={absolute}: {pushed:?}");
        let manifest = reliaburger::testkit::oci::fetch_manifest(&http, &origin, "test", "v1")
            .await
            .unwrap();
        assert_eq!(
            reliaburger::testkit::oci::sha256_digest(&manifest),
            image.manifest_digest
        );
        server.abort();
        let _ = server.await;
    }
}

fn registry_state(root: &std::path::Path) -> reliaburger::pickle::api::PickleState {
    reliaburger::pickle::api::PickleState {
        store: Arc::new(reliaburger::pickle::store::BlobStore::new(root)),
        catalog: Default::default(),
        node_raft_id: 1,
        council: None,
        forwarder: None,
        persist_path: None,
        auth: Some(reliaburger::sesame::auth::AuthState::new(
            Default::default(),
            Some("fixture-registry-token".into()),
        )),
        require_read_auth: true,
        allow_unauthenticated_bootstrap: false,
        quota: Default::default(),
        sessions: reliaburger::pickle::registry_auth::UploadSessions::new(
            reliaburger::pickle::registry_auth::DEFAULT_UPLOAD_TTL,
        ),
    }
}

#[tokio::test]
async fn failed_upload_cleanup_stays_fenced_and_retries_without_blocking_other_uploads() {
    let root = tempfile::tempdir().unwrap();
    let state = registry_state(root.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let router = reliaburger::pickle::api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();
    let started = http
        .post(format!("{origin}/v2/owned/blobs/uploads/"))
        .bearer_auth("fixture-registry-token")
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 202);
    let location = started.headers()["location"].to_str().unwrap();
    let id = location.rsplit('/').next().unwrap();
    let path = root.path().join("uploads").join(id);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let response = http
        .patch(format!("{origin}{location}"))
        .bearer_auth("fixture-registry-token")
        .body("partial")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(
        state
            .sessions
            .claim_writer(id, "owned", None)
            .await
            .is_none()
    );

    let other = state.store.initiate_upload().await.unwrap();
    state
        .sessions
        .register(&other, "other", None, std::time::SystemTime::UNIX_EPOCH)
        .await;
    let failures = state
        .sessions
        .cleanup_expired(&state.store, std::time::SystemTime::now())
        .await;
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, id);
    assert!(!root.path().join("uploads").join(&other).exists());
    assert_eq!(
        state.sessions.sweep(std::time::SystemTime::now()).await,
        vec![id]
    );

    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, b"recoverable partial").unwrap();
    assert!(
        state
            .sessions
            .cleanup_expired(&state.store, std::time::SystemTime::now())
            .await
            .is_empty()
    );
    assert!(!path.exists());
    assert!(
        state
            .sessions
            .sweep(std::time::SystemTime::now())
            .await
            .is_empty()
    );
    server.abort();
    let _ = server.await;
}
