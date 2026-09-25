//! Stock OCI clients against Pickle: HTTP Basic credentials over TLS.
//!
//! `docker login`, `docker push` and `crane` authenticate with HTTP Basic, not
//! with a bearer header. Pickle accepts a Basic credential whose password is a
//! Reliaburger API token, but only on a TLS connection. These tests drive the
//! real registry router through the real TLS serving loop Bun uses
//! (`serve_router_over_tls`), so they prove the whole chain: the listener marks
//! the connection as TLS, the registry accepts Basic only on such connections,
//! and the role checks are the bearer path's.
//!
//! The portable tests need nothing outside the crate. The `crane` test is
//! ignored by default and runs with `--run-ignored=only` on a machine that has
//! `crane` on its `PATH` (it never touches the network).

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reliaburger::pickle::build::{digest_of, oci_blob_upload_url, oci_manifest_put_url};
use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::{CrlHandle, build_api_server_config};
use reliaburger::sesame::types::{ApiRole, SerialNumber, TokenScope};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// A node identity named `localhost`, so an ordinary client verifying the
/// certificate against the name it dialled (as docker and crane do) accepts it.
fn localhost_identity(hierarchy: &CaHierarchy) -> NodeIdentity {
    let (cert_der, key_der, serial) = ca::issue_node_cert(
        "localhost",
        SerialNumber(10),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let now = SystemTime::now();
    NodeIdentity {
        node_id: "localhost".to_string(),
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

struct Registry {
    tls_port: u16,
    plaintext_port: u16,
    deployer: String,
    reader: String,
    root_ca_der: Vec<u8>,
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

/// One registry state served twice: over TLS (as a node with an mTLS
/// identity serves it) and over plaintext (as a node without one does).
/// Routable policy: reads and writes both need a principal.
async fn start_registry() -> Registry {
    use reliaburger::pickle::api::{PickleState, router as pickle_router};
    use reliaburger::pickle::store::BlobStore;
    use reliaburger::pickle::types::ManifestCatalog;
    use reliaburger::sesame::auth::{AuthState, new_token_store};

    let deployer = reliaburger::sesame::token::create_token(
        "ci",
        ApiRole::Deployer,
        TokenScope::default(),
        None,
    )
    .unwrap();
    let reader = reliaburger::sesame::token::create_token(
        "puller",
        ApiRole::ReadOnly,
        TokenScope::default(),
        None,
    )
    .unwrap();
    let tokens = new_token_store();
    tokens.write().await.push(deployer.token);
    tokens.write().await.push(reader.token);

    let dir = tempfile::tempdir().unwrap();
    let state = PickleState {
        store: Arc::new(BlobStore::new(dir.path().join("blobs"))),
        catalog: Arc::new(RwLock::new(ManifestCatalog::default())),
        node_raft_id: 1,
        council: None,
        forwarder: None,
        test_leases: Default::default(),
        repository_writers: Default::default(),
        persist_path: None,
        auth: Some(AuthState::new(tokens, None)),
        require_read_auth: true,
        allow_unauthenticated_bootstrap: false,
        quota: reliaburger::pickle::registry_auth::QuotaConfig::default(),
        sessions: reliaburger::pickle::registry_auth::UploadSessions::new(
            reliaburger::pickle::registry_auth::DEFAULT_UPLOAD_TTL,
        ),
    };

    let hierarchy = ca::generate_ca_hierarchy("registry-standard-clients", b"ikm").unwrap();
    let identity = localhost_identity(&hierarchy);
    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&identity, CrlHandle::default()).unwrap(),
    );

    let shutdown = CancellationToken::new();
    let tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tls_port = tls_listener.local_addr().unwrap().port();
    tokio::spawn(reliaburger::sesame::connection::serve_router_over_tls(
        tls_listener,
        acceptor,
        pickle_router(state.clone()),
        shutdown.clone(),
    ));

    let plaintext_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plaintext_port = plaintext_listener.local_addr().unwrap().port();
    let plaintext_shutdown = shutdown.clone();
    let plaintext_app = pickle_router(state);
    tokio::spawn(async move {
        axum::serve(plaintext_listener, plaintext_app)
            .with_graceful_shutdown(async move { plaintext_shutdown.cancelled().await })
            .await
            .ok();
    });

    Registry {
        tls_port,
        plaintext_port,
        deployer: deployer.plaintext,
        reader: reader.plaintext,
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        shutdown,
        _dir: dir,
    }
}

/// A client that trusts only the cluster root and verifies the hostname,
/// like docker with the CA in `certs.d`.
fn tls_client(root_ca_der: &[u8]) -> reqwest::Client {
    reqwest::Client::builder()
        .use_rustls_tls()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_der(root_ca_der).unwrap())
        .build()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployer_token_pushes_and_pulls_with_basic_credentials_over_tls() {
    let registry = start_registry().await;
    let client = tls_client(&registry.root_ca_der);
    let base = format!("https://localhost:{}", registry.tls_port);

    // docker's first request: an anonymous probe that must name the scheme.
    let probe = client.get(format!("{base}/v2/")).send().await.unwrap();
    assert_eq!(probe.status().as_u16(), 401);
    assert_eq!(
        probe.headers()["www-authenticate"],
        r#"Basic realm="reliaburger""#
    );
    // docker login: the same probe with the credential.
    let login = client
        .get(format!("{base}/v2/"))
        .basic_auth("ci", Some(&registry.deployer))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status().as_u16(), 200);

    let repo = "team/api";
    let config = br#"{"architecture":"amd64","os":"linux"}"#.to_vec();
    let layer = b"a layer pushed by a stock client".to_vec();
    for blob in [&config, &layer] {
        let url = oci_blob_upload_url("https", registry.tls_port, repo, &digest_of(blob));
        let response = client
            .post(&url)
            .basic_auth("ci", Some(&registry.deployer))
            .body(blob.clone())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
    }
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": digest_of(&config),
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": digest_of(&layer),
            "size": layer.len(),
        }],
    }))
    .unwrap();
    let manifest_url = oci_manifest_put_url("https", registry.tls_port, repo, "v1");
    let put = client
        .put(&manifest_url)
        .basic_auth("ci", Some(&registry.deployer))
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest.clone())
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success(), "{}", put.status());

    // A ReadOnly token pulls the image back, and may not overwrite it.
    let pulled = client
        .get(&manifest_url)
        .basic_auth("puller", Some(&registry.reader))
        .send()
        .await
        .unwrap();
    assert_eq!(pulled.status().as_u16(), 200);
    assert_eq!(&pulled.bytes().await.unwrap()[..], &manifest[..]);
    let overwrite = client
        .put(&manifest_url)
        .basic_auth("puller", Some(&registry.reader))
        .header("content-type", "application/vnd.oci.image.manifest.v1+json")
        .body(manifest)
        .send()
        .await
        .unwrap();
    assert_eq!(overwrite.status().as_u16(), 403);

    registry.shutdown.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn basic_credentials_are_refused_on_the_plaintext_listener() {
    let registry = start_registry().await;
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{}", registry.plaintext_port);

    let response = client
        .get(format!("{base}/v2/"))
        .basic_auth("ci", Some(&registry.deployer))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    assert!(!response.headers().contains_key("www-authenticate"));
    let body = response.text().await.unwrap();
    assert!(body.contains("only accepted over TLS"), "{body}");

    // The same token as a bearer still works in plaintext: this change adds
    // an envelope for TLS clients, it doesn't move the bearer path.
    let bearer = client
        .get(format!("{base}/v2/"))
        .bearer_auth(&registry.deployer)
        .send()
        .await
        .unwrap();
    assert_eq!(bearer.status().as_u16(), 200);

    registry.shutdown.cancel();
}

/// The real thing: `crane auth login` then `crane append` (a push) and
/// `crane manifest` (a pull) against the TLS listener. Needs `crane` on the
/// `PATH`; `--insecure` only skips crane's certificate check (Go on macOS
/// ignores `SSL_CERT_FILE`), the connection is still TLS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires crane on PATH; run with --run-ignored=only"]
async fn crane_logs_in_pushes_and_pulls_over_tls() {
    let registry = start_registry().await;
    let work = tempfile::tempdir().unwrap();
    let docker_config = work.path().join("docker");
    std::fs::create_dir_all(&docker_config).unwrap();
    std::fs::write(work.path().join("hello.txt"), "hello from crane\n").unwrap();
    let layer = work.path().join("layer.tar");
    let tar = std::process::Command::new("tar")
        .arg("-cf")
        .arg(&layer)
        .arg("-C")
        .arg(work.path())
        .arg("hello.txt")
        .status()
        .unwrap();
    assert!(tar.success());

    let host = format!("localhost:{}", registry.tls_port);
    let image = format!("{host}/team/crane:v1");
    let crane = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let docker_config = docker_config.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                std::process::Command::new("crane")
                    .args(&args)
                    .env("DOCKER_CONFIG", &docker_config)
                    .output()
                    .expect("crane must be on PATH for this test")
            })
            .await
            .unwrap()
        }
    };

    // Without a login, crane's push is refused.
    let refused = crane(&[
        "append",
        "--insecure",
        "-f",
        layer.to_str().unwrap(),
        "-t",
        &image,
    ])
    .await;
    assert!(!refused.status.success(), "anonymous push succeeded");

    let login = crane(&["auth", "login", &host, "-u", "ci", "-p", &registry.deployer]).await;
    assert!(
        login.status.success(),
        "{}",
        String::from_utf8_lossy(&login.stderr)
    );

    let push = crane(&[
        "append",
        "--insecure",
        "-f",
        layer.to_str().unwrap(),
        "-t",
        &image,
    ])
    .await;
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );

    let pull = crane(&["manifest", "--insecure", &image]).await;
    assert!(
        pull.status.success(),
        "{}",
        String::from_utf8_lossy(&pull.stderr)
    );
    let manifest: serde_json::Value = serde_json::from_slice(&pull.stdout).unwrap();
    assert_eq!(manifest["layers"].as_array().unwrap().len(), 1);

    registry.shutdown.cancel();
}
