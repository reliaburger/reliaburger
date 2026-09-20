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
        test_leases: Default::default(),
        repository_writers: Default::default(),
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

#[tokio::test]
async fn catalogue_stages_verified_upstream_bytes_under_its_exact_lease_and_retires_them() {
    use reliaburger::{
        pickle::upstream::OciUpstream,
        sesame::{
            auth, token,
            types::{ApiRole, TokenScope},
        },
        testkit::{lease::TestLease, oci},
    };
    let source_root = tempfile::tempdir().unwrap();
    let mut source = registry_state(source_root.path());
    source.auth = None;
    source.require_read_auth = false;
    source.allow_unauthenticated_bootstrap = true;
    let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source_address = source_listener.local_addr().unwrap();
    let source_base = format!("http://{source_address}");
    let source_server = tokio::spawn(async move {
        axum::serve(source_listener, reliaburger::pickle::api::router(source))
            .await
            .unwrap();
    });
    let plain = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let fixture = oci::build_synthetic_image("source-content");
    oci::push_image(&plain, &source_base, "fixture", "v1", &fixture)
        .await
        .unwrap();

    let root = tempfile::tempdir().unwrap();
    let mut state = registry_state(root.path());
    state.persist_path = Some(root.path().join("catalog.json"));
    let owner = token::create_token(
        "fixture-owner",
        ApiRole::Deployer,
        TokenScope::default(),
        None,
    )
    .unwrap();
    let principal =
        auth::authenticate(&owner.plaintext, std::slice::from_ref(&owner.token)).unwrap();
    let tokens = auth::new_token_store();
    tokens.write().await.push(owner.token);
    state.auth = Some(auth::AuthState::new(tokens, None));
    let now = reliaburger::testkit::lease::now_unix_millis();
    state
        .test_leases
        .create(
            TestLease::new(
                "upload1".into(),
                principal.principal_id,
                "fixture-owner".into(),
                "rbtest-upload1".into(),
                now,
                now + 600_000,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = reliaburger::pickle::api::router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", owner.plaintext).parse().unwrap(),
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let repository = "rbtest-upload1/runnable";
    let upstream = OciUpstream::insecure_http(Default::default());
    let pinned = reliaburger::grill::image::ImageReference::parse(&format!(
        "{source_address}/fixture@{}",
        fixture.manifest_digest
    ))
    .unwrap();
    let mut mutable = pinned.clone();
    mutable.tag = "v1".into();
    assert!(
        oci::stage_upstream_image(&client, &base, repository, "upload1", &upstream, &mutable)
            .await
            .is_err()
    );
    assert!(
        oci::stage_upstream_image(
            &client,
            &base,
            repository,
            "wrong-lease",
            &upstream,
            &pinned
        )
        .await
        .is_err()
    );
    assert!(state.catalog.read().await.manifests.is_empty());
    let digest =
        oci::stage_upstream_image(&client, &base, repository, "upload1", &upstream, &pinned)
            .await
            .unwrap();
    assert_eq!(digest, fixture.manifest_digest);
    assert_eq!(
        oci::fetch_manifest(&client, &base, repository, &digest)
            .await
            .unwrap(),
        fixture.manifest
    );
    assert_eq!(
        state.test_leases.get("upload1").await.unwrap().repositories[repository],
        std::collections::BTreeSet::from([1])
    );
    assert_eq!(
        state.catalog.read().await.repository_owners[repository],
        "upload1"
    );
    state
        .test_leases
        .begin_cleanup("upload1", None)
        .await
        .unwrap();
    state
        .test_leases
        .confirm_workloads_retired("upload1")
        .await
        .unwrap();
    state.reap_registry_leases_once().await.unwrap();
    assert!(state.catalog.read().await.manifests.is_empty());
    assert!(
        oci::stage_upstream_image(&client, &base, repository, "upload1", &upstream, &pinned)
            .await
            .is_err()
    );
    server.abort();
    source_server.abort();
    let _ = server.await;
    let _ = source_server.await;
}
