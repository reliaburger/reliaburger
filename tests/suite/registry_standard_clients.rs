//! Stock OCI clients against Pickle: API tokens as HTTP Basic passwords, over
//! TLS the client verifies against the cluster CA.
//!
//! `docker login`, `docker push`, podman, skopeo and crane authenticate with
//! HTTP Basic, not with a bearer header. Pickle accepts a Basic credential
//! whose password is a Reliaburger API token, but only on a TLS connection.
//! These tests drive the real registry router through the real TLS serving
//! loop Bun uses (`serve_router_over_tls`), so they prove the whole chain: the
//! listener marks the connection as TLS, the registry accepts Basic only on
//! such connections, and the role checks are the bearer path's.
//!
//! The portable tests need nothing outside the crate. The client tests are
//! ignored by default; `make test-standard-clients` runs them on a Linux host
//! that has docker, podman, skopeo and crane, prepared by
//! `scripts/standard-clients-host.sh`. They never touch the network beyond
//! this machine.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use reliaburger::pickle::build::{digest_of, oci_blob_upload_url, oci_manifest_put_url};
use reliaburger::sesame::ca::{self, CaHierarchy};
use reliaburger::sesame::identity_store::NodeIdentity;
use reliaburger::sesame::mtls::{CrlHandle, build_api_server_config};
use reliaburger::sesame::types::{ApiRole, SerialNumber, TokenScope};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// The name the client tests reach the registry by. `/etc/hosts` maps it to
/// a non-loopback address of this machine (`scripts/standard-clients-host.sh`
/// does that), because docker treats every `127.0.0.0/8` registry as
/// insecure and would hide a certificate failure behind a plaintext retry.
const CLIENT_HOST: &str = "pickle-clients.test";

/// The media types a docker or OCI client may get a manifest back as.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
     application/vnd.oci.image.index.v1+json, \
     application/vnd.docker.distribution.manifest.v2+json, \
     application/vnd.docker.distribution.manifest.list.v2+json";

/// A node identity named `node_name`, so an ordinary client verifying the
/// certificate against the name it dialled (as docker and crane do) accepts it.
fn node_identity(hierarchy: &CaHierarchy, node_name: &str) -> NodeIdentity {
    let (cert_der, key_der, serial) = ca::issue_node_cert(
        node_name,
        SerialNumber(10),
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    )
    .unwrap();
    let now = SystemTime::now();
    NodeIdentity {
        node_id: node_name.to_string(),
        certificate_der: cert_der,
        private_key_der: key_der,
        serial,
        ca_generation: 0,
        trust: reliaburger::sesame::trust::TrustSet::single(
            hierarchy.node.ca.certificate_der.clone(),
            hierarchy.root.ca.certificate_der.clone(),
        ),
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
    /// A Deployer scoped to namespace `team`.
    team_deployer: String,
    root_ca_der: Vec<u8>,
    /// Every `GET` of a blob the registry served, as `/v2/{name}/blobs/{digest}`.
    blob_reads: Arc<Mutex<Vec<String>>>,
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Registry {
    /// The cluster root CA as PEM, the form `certs.d` and `SSL_CERT_FILE` want.
    fn root_ca_pem(&self) -> String {
        reliaburger::sesame::cert::der_to_pem(&self.root_ca_der, "CERTIFICATE")
    }

    /// Whether a client fetched blob `digest` since the log was last cleared.
    fn served_blob(&self, digest: &str) -> bool {
        self.blob_reads
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.ends_with(&format!("/blobs/{digest}")))
    }
}

/// One registry state on loopback, served twice: over TLS (as a node with an
/// mTLS identity serves it) and over plaintext (as a node without one does).
/// Routable policy: reads and writes both need a principal.
async fn start_registry() -> Registry {
    start_registry_at("127.0.0.1".parse().unwrap(), "localhost").await
}

/// The same registry on `address`, with a node certificate named `node_name`.
async fn start_registry_at(address: std::net::IpAddr, node_name: &str) -> Registry {
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
    let team_deployer = reliaburger::sesame::token::create_token(
        "team-ci",
        ApiRole::Deployer,
        TokenScope {
            apps: None,
            namespaces: Some(vec!["team".to_string()]),
        },
        None,
    )
    .unwrap();
    let tokens = new_token_store();
    tokens.write().await.push(deployer.token);
    tokens.write().await.push(reader.token);
    tokens.write().await.push(team_deployer.token);

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
    let identity = node_identity(&hierarchy, node_name);
    let acceptor = tokio_rustls::TlsAcceptor::from(
        build_api_server_config(&identity, CrlHandle::default()).unwrap(),
    );

    // Record blob downloads, so a client test can prove its pull fetched the
    // layers from the registry rather than finding them in a local cache.
    let blob_reads = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&blob_reads);
    let tls_app = pickle_router(state.clone()).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let recorder = Arc::clone(&recorder);
            async move {
                let path = request.uri().path().to_string();
                let blob_read =
                    request.method() == axum::http::Method::GET && path.contains("/blobs/sha256:");
                let response = next.run(request).await;
                if blob_read && response.status().is_success() {
                    recorder.lock().unwrap().push(path);
                }
                response
            }
        },
    ));

    let shutdown = CancellationToken::new();
    let tls_listener = tokio::net::TcpListener::bind((address, 0)).await.unwrap();
    let tls_port = tls_listener.local_addr().unwrap().port();
    tokio::spawn(reliaburger::sesame::connection::serve_router_over_tls(
        tls_listener,
        acceptor,
        tls_app,
        reliaburger::sesame::connection::ConnectionTimeouts::PRODUCTION,
        shutdown.clone(),
    ));

    let plaintext_listener = tokio::net::TcpListener::bind((address, 0)).await.unwrap();
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
        team_deployer: team_deployer.plaintext,
        root_ca_der: hierarchy.root.ca.certificate_der.clone(),
        blob_reads,
        shutdown,
        _dir: dir,
    }
}

/// A client that trusts only the cluster root and verifies the hostname,
/// like docker with the CA in `certs.d`.
fn tls_client(root_ca_der: &[u8]) -> reqwest::Client {
    reqwest::Client::builder()
        .tls_backend_rustls()
        .tls_certs_only([reqwest::Certificate::from_der(root_ca_der).unwrap()])
        .build()
        .unwrap()
}

/// Push one blob with a monolithic upload, as the Deployer.
async fn push_blob(client: &reqwest::Client, registry: &Registry, repo: &str, blob: &[u8]) {
    let url = oci_blob_upload_url("https", registry.tls_port, repo, &digest_of(blob));
    let response = client
        .post(&url)
        .basic_auth("ci", Some(&registry.deployer))
        .body(blob.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 201, "push {repo}");
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
        push_blob(&client, &registry, repo, blob).await;
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

/// A Deployer scoped to namespace `team` pushes `team/api` with Basic over
/// TLS, as `docker push` would, and is refused every other repository: another
/// namespace's and a bare name alike. Unscoped tokens are unaffected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_namespace_scoped_token_pushes_only_into_its_namespace_over_tls() {
    let registry = start_registry().await;
    let client = tls_client(&registry.root_ca_der);
    let config = br#"{"architecture":"arm64","os":"linux"}"#.to_vec();
    let digest = digest_of(&config);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": digest,
            "size": config.len(),
        },
        "layers": [],
    }))
    .unwrap();
    let push = |repository: &'static str, username: &'static str, token: String| {
        let client = client.clone();
        let config = config.clone();
        let manifest = manifest.clone();
        let blob_url = oci_blob_upload_url("https", registry.tls_port, repository, &digest);
        let manifest_url = oci_manifest_put_url("https", registry.tls_port, repository, "v1");
        async move {
            let blob = client
                .post(&blob_url)
                .basic_auth(username, Some(&token))
                .body(config)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16();
            let manifest = client
                .put(&manifest_url)
                .basic_auth(username, Some(&token))
                .header("content-type", "application/vnd.oci.image.manifest.v1+json")
                .body(manifest)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16();
            (blob, manifest)
        }
    };

    let team = registry.team_deployer.clone();
    assert_eq!(push("team/api", "team-ci", team.clone()).await, (201, 201));
    assert_eq!(push("other/api", "team-ci", team.clone()).await, (403, 403));
    assert_eq!(push("api", "team-ci", team).await, (403, 403));
    for repository in ["other/api", "api"] {
        assert_eq!(
            push(repository, "ci", registry.deployer.clone()).await,
            (201, 201),
            "{repository}"
        );
    }

    registry.shutdown.cancel();
}

/// docker's very first request names no credential. The answer must be a
/// 401 that docker can act on: a Basic challenge, the registry API version
/// header docker uses to recognise a v2 registry, and an OCI error body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anonymous_v2_ping_over_tls_gets_a_basic_challenge() {
    let registry = start_registry().await;
    let client = tls_client(&registry.root_ca_der);
    let base = format!("https://localhost:{}", registry.tls_port);

    let probe = client.get(format!("{base}/v2/")).send().await.unwrap();
    assert_eq!(probe.status().as_u16(), 401);
    assert_eq!(
        probe.headers()["www-authenticate"],
        r#"Basic realm="reliaburger""#
    );
    assert_eq!(
        probe.headers()["docker-distribution-api-version"],
        "registry/2.0"
    );
    let body: serde_json::Value = probe.json().await.unwrap();
    assert_eq!(body["errors"][0]["code"], "UNAUTHORIZED", "{body}");

    let login = client
        .get(format!("{base}/v2/"))
        .basic_auth("ci", Some(&registry.deployer))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status().as_u16(), 200);
    assert_eq!(
        login.headers()["docker-distribution-api-version"],
        "registry/2.0"
    );

    registry.shutdown.cancel();
}

/// Before uploading a layer, docker, podman and skopeo ask whether the
/// registry already has it with `HEAD`. The answer must carry the digest and
/// the real length, or the client re-uploads or rejects the blob.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_on_a_pushed_blob_returns_its_digest_and_length() {
    let registry = start_registry().await;
    let client = tls_client(&registry.root_ca_der);
    let layer = b"a layer a client probes before uploading".to_vec();
    let digest = digest_of(&layer);
    let url = format!(
        "https://localhost:{}/v2/team/api/blobs/{digest}",
        registry.tls_port
    );

    let missing = client
        .head(&url)
        .basic_auth("ci", Some(&registry.deployer))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status().as_u16(), 404);

    push_blob(&client, &registry, "team/api", &layer).await;
    let present = client
        .head(&url)
        .basic_auth("ci", Some(&registry.deployer))
        .send()
        .await
        .unwrap();
    assert_eq!(present.status().as_u16(), 200);
    assert_eq!(present.headers()["docker-content-digest"], digest.as_str());
    assert_eq!(
        present.headers()["content-length"],
        layer.len().to_string().as_str()
    );

    registry.shutdown.cancel();
}

/// skopeo and docker ask to mount a layer from another repository before
/// uploading it (`?mount=…&from=…`). Pickle doesn't mount, so it must answer
/// as the spec allows: `202` with an upload location, and the client then
/// uploads the bytes normally into the new repository.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_repository_mount_request_falls_back_to_a_normal_upload() {
    let registry = start_registry().await;
    let client = tls_client(&registry.root_ca_der);
    let base = format!("https://localhost:{}", registry.tls_port);
    let layer = b"a layer another repository already holds".to_vec();
    let digest = digest_of(&layer);
    push_blob(&client, &registry, "team/source", &layer).await;

    let mount = client
        .post(format!(
            "{base}/v2/team/target/blobs/uploads/?mount={digest}&from=team/source"
        ))
        .basic_auth("ci", Some(&registry.deployer))
        .send()
        .await
        .unwrap();
    assert_eq!(mount.status().as_u16(), 202);
    let location = mount.headers()["location"].to_str().unwrap().to_string();
    assert!(
        location.starts_with("/v2/team/target/blobs/uploads/"),
        "{location}"
    );

    let separator = if location.contains('?') { '&' } else { '?' };
    let complete = client
        .put(format!("{base}{location}{separator}digest={digest}"))
        .basic_auth("ci", Some(&registry.deployer))
        .body(layer.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(complete.status().as_u16(), 201);
    assert_eq!(complete.headers()["docker-content-digest"], digest.as_str());

    let pulled = client
        .get(format!("{base}/v2/team/target/blobs/{digest}"))
        .basic_auth("ci", Some(&registry.deployer))
        .send()
        .await
        .unwrap();
    assert_eq!(pulled.status().as_u16(), 200);
    assert_eq!(&pulled.bytes().await.unwrap()[..], &layer[..]);

    registry.shutdown.cancel();
}

// ---------------------------------------------------------------------------
// Real clients. Linux only, prepared by scripts/standard-clients-host.sh.
// ---------------------------------------------------------------------------

/// The non-loopback address `CLIENT_HOST` resolves to. Panics with the fix
/// when the host isn't prepared: these tests fail rather than skip.
async fn client_host_address() -> std::net::IpAddr {
    let resolved: Vec<std::net::SocketAddr> = tokio::net::lookup_host((CLIENT_HOST, 0))
        .await
        .map(|addresses| addresses.collect())
        .unwrap_or_default();
    resolved
        .iter()
        .map(|socket| socket.ip())
        .find(|ip| ip.is_ipv4() && !ip.is_loopback())
        .unwrap_or_else(|| {
            panic!(
                "{CLIENT_HOST} must resolve to a non-loopback address of this host; \
                 run `sudo scripts/standard-clients-host.sh --provision` first"
            )
        })
}

/// A registry the client tests reach as `CLIENT_HOST:<port>`.
async fn start_client_registry() -> Registry {
    start_registry_at(client_host_address().await, CLIENT_HOST).await
}

/// A root CA no Pickle node chains to: what a client sees when it trusts the
/// wrong cluster.
fn foreign_ca_pem() -> String {
    let other = ca::generate_ca_hierarchy("someone-elses-cluster", b"other ikm").unwrap();
    reliaburger::sesame::cert::der_to_pem(&other.root.ca.certificate_der, "CERTIFICATE")
}

/// A CA installed where a container engine looks for a registry's trust:
/// `<certs.d>/<host:port>/ca.crt`. Removed again when dropped.
struct TrustedCa {
    directory: PathBuf,
}

impl TrustedCa {
    fn install(certs_d: &str, registry_host: &str, ca_pem: &str) -> TrustedCa {
        let directory = Path::new(certs_d).join(registry_host);
        std::fs::create_dir_all(&directory)
            .and_then(|()| std::fs::write(directory.join("ca.crt"), ca_pem))
            .unwrap_or_else(|error| {
                panic!(
                    "writing {}/ca.crt: {error}; run \
                     `sudo scripts/standard-clients-host.sh --provision` first",
                    directory.display()
                )
            });
        TrustedCa { directory }
    }

    /// Replace the trusted CA, as an operator fixing a bad install would.
    fn replace(&self, ca_pem: &str) {
        std::fs::write(self.directory.join("ca.crt"), ca_pem).unwrap();
    }
}

impl Drop for TrustedCa {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// One client binary with the environment every call shares.
#[derive(Clone)]
struct Client {
    program: &'static str,
    prefix: Vec<String>,
    envs: Vec<(String, String)>,
}

impl Client {
    fn new(program: &'static str) -> Client {
        Client {
            program,
            prefix: Vec::new(),
            envs: Vec::new(),
        }
    }

    /// Global flags that go before every subcommand (`--config`, `--root`).
    fn with_prefix(mut self, prefix: &[&str]) -> Client {
        self.prefix = prefix.iter().map(|arg| arg.to_string()).collect();
        self
    }

    fn with_env(mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) -> Client {
        self.envs.push((
            key.to_string(),
            value.as_ref().to_string_lossy().into_owned(),
        ));
        self
    }

    async fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, None).await
    }

    /// Run to completion off the async runtime, feeding `stdin` if given
    /// (passwords go in this way, never on the command line).
    async fn run_with_stdin(&self, args: &[&str], stdin: Option<&str>) -> Output {
        use std::io::Write;
        let mut command = std::process::Command::new(self.program);
        command
            .args(&self.prefix)
            .args(args)
            .envs(self.envs.iter().map(|(key, value)| (key, value)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let program = self.program;
        let stdin = stdin.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let mut child = command
                .spawn()
                .unwrap_or_else(|error| panic!("{program} must be on PATH: {error}"));
            let mut pipe = child.stdin.take().unwrap();
            if let Some(input) = stdin {
                pipe.write_all(input.as_bytes()).unwrap();
            }
            drop(pipe);
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    }

    /// Run, and fail the test with the client's own output if it fails.
    async fn succeeds(&self, args: &[&str]) -> Output {
        let output = self.run(args).await;
        assert_succeeded(self.program, args, &output);
        output
    }
}

fn describe(output: &Output) -> String {
    format!(
        "status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_succeeded(program: &str, args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "{program} {} failed: {}",
        args.join(" "),
        describe(output)
    );
}

/// The client failed, and said why in words containing one of `reasons`.
fn assert_refused(what: &str, output: &Output, reasons: &[&str]) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_lowercase();
    assert!(
        !output.status.success(),
        "{what} succeeded: {}",
        describe(output)
    );
    assert!(
        reasons.iter().any(|reason| text.contains(reason)),
        "{what} failed for an unexpected reason (wanted one of {reasons:?}): {}",
        describe(output)
    );
}

/// Words clients use when a certificate doesn't chain to a trusted root.
const UNTRUSTED_CA: &[&str] = &["x509", "certificate", "unknown authority"];
/// Words clients use for a 401.
const UNAUTHORISED: &[&str] = &[
    "401",
    "unauthorized",
    "authentication required",
    "invalid username/password",
];
/// Words clients use for a 403 `DENIED`.
const DENIED: &[&str] = &["denied", "403", "forbidden"];

/// A two-layer build context (`FROM scratch` and two `COPY`s), unique to this
/// run so no client finds its layers cached from an earlier one.
fn two_layer_context(directory: &Path, nonce: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(
        directory.join("Containerfile"),
        "FROM scratch\nCOPY a.txt /a.txt\nCOPY b.txt /b.txt\n",
    )
    .unwrap();
    std::fs::write(directory.join("a.txt"), format!("first layer {nonce}\n")).unwrap();
    std::fs::write(directory.join("b.txt"), format!("second layer {nonce}\n")).unwrap();
}

/// A nonce no earlier run used.
fn nonce(registry: &Registry) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}-{nanos}", registry.tls_port)
}

/// The layer digests of `repository:tag` as the registry stores them. When
/// the tag names an index (docker with the containerd image store pushes one,
/// with BuildKit's attestations beside the image), follow it to the image.
async fn registry_layers(registry: &Registry, repository: &str, tag: &str) -> Vec<String> {
    let fetch = |reference: String| async move {
        let manifest: serde_json::Value = tls_client(&registry.root_ca_der)
            .get(format!(
                "https://{CLIENT_HOST}:{}/v2/{repository}/manifests/{reference}",
                registry.tls_port
            ))
            .basic_auth("ci", Some(&registry.deployer))
            .header("accept", MANIFEST_ACCEPT)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        manifest
    };
    let mut manifest = fetch(tag.to_string()).await;
    if let Some(entries) = manifest["manifests"].as_array() {
        let image = entries
            .iter()
            .find(|entry| entry["platform"]["os"] != "unknown")
            .unwrap_or_else(|| panic!("no image in the index: {manifest}"));
        let digest = image["digest"].as_str().unwrap().to_string();
        manifest = fetch(digest).await;
    }
    manifest["layers"]
        .as_array()
        .unwrap_or_else(|| panic!("not an image manifest: {manifest}"))
        .iter()
        .map(|layer| layer["digest"].as_str().unwrap().to_string())
        .collect()
}

/// `RootFS.Layers` (the uncompressed diff ids) of a local image.
async fn local_diff_ids(client: &Client, image: &str) -> String {
    let output = client
        .succeeds(&[
            "image",
            "inspect",
            "--format",
            "{{json .RootFS.Layers}}",
            image,
        ])
        .await;
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// docker as users run it: the CA in `/etc/docker/certs.d/<host:port>/`, an
/// API token as the `docker login` password, then build, push, delete and
/// pull back. The pull must fetch every layer from Pickle and rebuild the
/// same image. A wrong token is refused at login.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires docker on PATH and a prepared host; run with make test-standard-clients"]
async fn docker_logs_in_pushes_and_pulls_layers_over_verified_tls() {
    let registry = start_client_registry().await;
    let host = format!("{CLIENT_HOST}:{}", registry.tls_port);
    let _trust = TrustedCa::install("/etc/docker/certs.d", &host, &registry.root_ca_pem());
    let work = tempfile::tempdir().unwrap();
    let config = work.path().join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    let docker = Client::new("docker").with_prefix(&["--config", config.to_str().unwrap()]);
    let context = work.path().join("context");
    two_layer_context(&context, &nonce(&registry));
    let image = format!("{host}/team/docker:v1");

    docker
        .succeeds(&[
            "build",
            "-f",
            context.join("Containerfile").to_str().unwrap(),
            "-t",
            &image,
            context.to_str().unwrap(),
        ])
        .await;

    let wrong = docker
        .run_with_stdin(
            &["login", &host, "-u", "ci", "--password-stdin"],
            Some("not-a-token"),
        )
        .await;
    assert_refused("docker login with a wrong token", &wrong, UNAUTHORISED);

    let login = ["login", &host, "-u", "ci", "--password-stdin"];
    let output = docker
        .run_with_stdin(&login, Some(&registry.deployer))
        .await;
    assert_succeeded("docker", &login, &output);
    docker.succeeds(&["push", &image]).await;

    let pushed = registry_layers(&registry, "team/docker", "v1").await;
    assert_eq!(pushed.len(), 2, "{pushed:?}");
    let built = local_diff_ids(&docker, &image).await;

    // Forget the image and the build cache, so the pull has to download.
    docker.succeeds(&["image", "rm", &image]).await;
    docker
        .succeeds(&["builder", "prune", "--all", "--force"])
        .await;
    registry.blob_reads.lock().unwrap().clear();
    docker.succeeds(&["pull", &image]).await;
    assert_eq!(local_diff_ids(&docker, &image).await, built);
    for digest in &pushed {
        assert!(
            registry.served_blob(digest),
            "docker never fetched {digest}"
        );
    }

    docker.succeeds(&["image", "rm", &image]).await;
    docker.succeeds(&["logout", &host]).await;
    registry.shutdown.cancel();
}

/// docker refuses a registry whose certificate doesn't chain to the CA in
/// `certs.d`: here the operator installed another cluster's root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires docker on PATH and a prepared host; run with make test-standard-clients"]
async fn docker_refuses_a_registry_signed_by_another_ca() {
    let registry = start_client_registry().await;
    let host = format!("{CLIENT_HOST}:{}", registry.tls_port);
    let _trust = TrustedCa::install("/etc/docker/certs.d", &host, &foreign_ca_pem());
    let work = tempfile::tempdir().unwrap();
    let docker = Client::new("docker").with_prefix(&["--config", work.path().to_str().unwrap()]);

    let login = docker
        .run_with_stdin(
            &["login", &host, "-u", "ci", "--password-stdin"],
            Some(&registry.deployer),
        )
        .await;
    assert_refused("docker login against an untrusted CA", &login, UNTRUSTED_CA);

    registry.shutdown.cancel();
}

/// A Deployer token scoped to namespace `team` logs in (the probe names no
/// repository) and pushes into `team/`, but `docker push` into another
/// namespace is denied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires docker on PATH and a prepared host; run with make test-standard-clients"]
async fn docker_push_with_a_scoped_token_outside_its_namespace_is_denied() {
    let registry = start_client_registry().await;
    let host = format!("{CLIENT_HOST}:{}", registry.tls_port);
    let _trust = TrustedCa::install("/etc/docker/certs.d", &host, &registry.root_ca_pem());
    let work = tempfile::tempdir().unwrap();
    let config = work.path().join("docker-config");
    std::fs::create_dir_all(&config).unwrap();
    let docker = Client::new("docker").with_prefix(&["--config", config.to_str().unwrap()]);
    let context = work.path().join("context");
    two_layer_context(&context, &nonce(&registry));
    let inside = format!("{host}/team/scoped:v1");
    let outside = format!("{host}/other/scoped:v1");
    docker
        .succeeds(&[
            "build",
            "-f",
            context.join("Containerfile").to_str().unwrap(),
            "-t",
            &inside,
            context.to_str().unwrap(),
        ])
        .await;
    docker.succeeds(&["tag", &inside, &outside]).await;

    let login = ["login", &host, "-u", "team-ci", "--password-stdin"];
    let output = docker
        .run_with_stdin(&login, Some(&registry.team_deployer))
        .await;
    assert_succeeded("docker", &login, &output);
    docker.succeeds(&["push", &inside]).await;
    let refused = docker.run(&["push", &outside]).await;
    assert_refused(
        "docker push outside the token's namespace",
        &refused,
        DENIED,
    );

    docker.succeeds(&["image", "rm", &inside, &outside]).await;
    registry.shutdown.cancel();
}

/// podman (rootless) with the CA in `/etc/containers/certs.d/<host:port>/`:
/// an untrusted CA, a wrong token and an out-of-namespace push are refused;
/// then build, push, and pull into a second, empty image store, so every
/// layer comes back from Pickle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires podman on PATH and a prepared host; run with make test-standard-clients"]
async fn podman_logs_in_pushes_and_pulls_layers_over_verified_tls() {
    let registry = start_client_registry().await;
    let host = format!("{CLIENT_HOST}:{}", registry.tls_port);
    let trust = TrustedCa::install("/etc/containers/certs.d", &host, &foreign_ca_pem());
    let work = tempfile::tempdir().unwrap();
    let auth = work.path().join("auth.json");
    let store = |name: &str| {
        let root = work.path().join(name);
        Client::new("podman")
            .with_prefix(&[
                "--root",
                root.join("storage").to_str().unwrap(),
                "--runroot",
                root.join("run").to_str().unwrap(),
                "--storage-driver",
                "vfs",
            ])
            .with_env("REGISTRY_AUTH_FILE", &auth)
    };
    let pusher = store("pusher");
    let puller = store("puller");
    let context = work.path().join("context");
    two_layer_context(&context, &nonce(&registry));
    let image = format!("{host}/team/podman:v1");
    let outside = format!("{host}/other/podman:v1");
    let login = |user: &'static str| ["login", host.as_str(), "-u", user, "--password-stdin"];

    let untrusted = pusher
        .run_with_stdin(&login("ci"), Some(&registry.deployer))
        .await;
    assert_refused(
        "podman login against an untrusted CA",
        &untrusted,
        UNTRUSTED_CA,
    );
    trust.replace(&registry.root_ca_pem());

    let wrong = pusher
        .run_with_stdin(&login("ci"), Some("not-a-token"))
        .await;
    assert_refused("podman login with a wrong token", &wrong, UNAUTHORISED);

    pusher
        .succeeds(&[
            "build",
            "--layers",
            "-f",
            context.join("Containerfile").to_str().unwrap(),
            "-t",
            &image,
            context.to_str().unwrap(),
        ])
        .await;
    pusher.succeeds(&["tag", &image, &outside]).await;

    let output = pusher
        .run_with_stdin(&login("team-ci"), Some(&registry.team_deployer))
        .await;
    assert_succeeded("podman", &login("team-ci"), &output);
    let refused = pusher.run(&["push", &outside]).await;
    assert_refused(
        "podman push outside the token's namespace",
        &refused,
        DENIED,
    );

    let output = pusher
        .run_with_stdin(&login("ci"), Some(&registry.deployer))
        .await;
    assert_succeeded("podman", &login("ci"), &output);
    pusher.succeeds(&["push", &image]).await;
    let pushed = registry_layers(&registry, "team/podman", "v1").await;
    assert_eq!(pushed.len(), 2, "{pushed:?}");

    registry.blob_reads.lock().unwrap().clear();
    puller.succeeds(&["pull", &image]).await;
    assert_eq!(
        local_diff_ids(&puller, &image).await,
        local_diff_ids(&pusher, &image).await
    );
    for digest in &pushed {
        assert!(
            registry.served_blob(digest),
            "podman never fetched {digest}"
        );
    }

    // vfs layers are owned by subordinate ids, so only podman's user
    // namespace can delete them.
    for name in ["pusher", "puller"] {
        let root = work.path().join(name);
        let _ = Client::new("podman")
            .run(&["unshare", "rm", "-rf", root.to_str().unwrap()])
            .await;
    }
    registry.shutdown.cancel();
}

/// Write a two-layer OCI image layout, tagged `v1`, and return its layer
/// digests (uncompressed tars).
fn two_layer_oci_layout(layout: &Path, scratch: &Path, nonce: &str) -> Vec<String> {
    let blobs = layout.join("blobs/sha256");
    std::fs::create_dir_all(&blobs).unwrap();
    let write_blob = |data: &[u8]| -> (String, usize) {
        let digest = digest_of(data);
        let hex = digest.trim_start_matches("sha256:");
        std::fs::write(blobs.join(hex), data).unwrap();
        (digest, data.len())
    };

    let mut layers = Vec::new();
    for name in ["a.txt", "b.txt"] {
        let source = scratch.join(name.trim_end_matches(".txt"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(name), format!("{name} {nonce}\n")).unwrap();
        let tar = scratch.join(format!("{name}.tar"));
        let status = std::process::Command::new("tar")
            .arg("-cf")
            .arg(&tar)
            .arg("-C")
            .arg(&source)
            .arg(name)
            .status()
            .unwrap();
        assert!(status.success());
        layers.push(write_blob(&std::fs::read(&tar).unwrap()));
    }
    let diff_ids: Vec<&str> = layers.iter().map(|(digest, _)| digest.as_str()).collect();
    let config = serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": diff_ids},
    }))
    .unwrap();
    let (config_digest, config_size) = write_blob(&config);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config_size,
        },
        "layers": layers.iter().map(|(digest, size)| serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": digest,
            "size": size,
        })).collect::<Vec<_>>(),
    }))
    .unwrap();
    let (manifest_digest, manifest_size) = write_blob(&manifest);
    std::fs::write(
        layout.join("index.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "manifests": [{
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": manifest_digest,
                "size": manifest_size,
                "annotations": {"org.opencontainers.image.ref.name": "v1"},
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        layout.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    layers.into_iter().map(|(digest, _)| digest).collect()
}

/// skopeo with `--cert-dir`: refused with an untrusted CA, a wrong token and
/// outside its namespace; then a copy from a local layout into one
/// repository, a registry-to-registry copy into another (the client offers a
/// cross-repository mount, which Pickle declines), `inspect`, and a copy back
/// to disk whose blobs match their digests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires skopeo on PATH and a prepared host; run with make test-standard-clients"]
async fn skopeo_copies_between_repositories_over_verified_tls() {
    let registry = start_client_registry().await;
    let host = format!("{CLIENT_HOST}:{}", registry.tls_port);
    let work = tempfile::tempdir().unwrap();
    let certs = work.path().join("certs");
    let foreign = work.path().join("foreign");
    std::fs::create_dir_all(&certs).unwrap();
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(certs.join("ca.crt"), registry.root_ca_pem()).unwrap();
    std::fs::write(foreign.join("ca.crt"), foreign_ca_pem()).unwrap();
    let certs = certs.to_str().unwrap();
    let skopeo =
        Client::new("skopeo").with_env("REGISTRY_AUTH_FILE", work.path().join("auth.json"));

    let layout = work.path().join("layout");
    two_layer_oci_layout(&layout, &work.path().join("scratch"), &nonce(&registry));
    let source = format!("oci:{}:v1", layout.display());
    let first = format!("docker://{host}/team/skopeo-a:v1");
    let second = format!("docker://{host}/team/skopeo-b:v1");
    let deployer = format!("ci:{}", registry.deployer);
    let push = |cert_dir: &str, creds: &str, destination: &str| {
        let skopeo = skopeo.clone();
        let args: Vec<String> = [
            "copy",
            "--dest-cert-dir",
            cert_dir,
            "--dest-creds",
            creds,
            source.as_str(),
            destination,
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            skopeo.run(&args).await
        }
    };

    let untrusted = push(foreign.to_str().unwrap(), &deployer, &first).await;
    assert_refused("skopeo copy to an untrusted CA", &untrusted, UNTRUSTED_CA);
    let wrong = push(certs, "ci:not-a-token", &first).await;
    assert_refused("skopeo copy with a wrong token", &wrong, UNAUTHORISED);
    let outside = push(
        certs,
        &format!("team-ci:{}", registry.team_deployer),
        &format!("docker://{host}/other/skopeo:v1"),
    )
    .await;
    assert_refused(
        "skopeo copy outside the token's namespace",
        &outside,
        DENIED,
    );

    let copied = push(certs, &deployer, &first).await;
    assert_succeeded("skopeo", &["copy", "oci:…", &first], &copied);
    skopeo
        .succeeds(&[
            "copy",
            "--src-cert-dir",
            certs,
            "--src-creds",
            &deployer,
            "--dest-cert-dir",
            certs,
            "--dest-creds",
            &deployer,
            &first,
            &second,
        ])
        .await;

    let inspected = skopeo
        .succeeds(&[
            "inspect",
            "--cert-dir",
            certs,
            "--creds",
            &deployer,
            &second,
        ])
        .await;
    let inspected: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    let pushed = registry_layers(&registry, "team/skopeo-a", "v1").await;
    assert_eq!(pushed.len(), 2, "{pushed:?}");
    assert_eq!(inspected["Layers"], serde_json::json!(pushed));
    assert_eq!(
        registry_layers(&registry, "team/skopeo-b", "v1").await,
        pushed
    );

    registry.blob_reads.lock().unwrap().clear();
    let out = work.path().join("out");
    let destination = format!("dir:{}", out.display());
    skopeo
        .succeeds(&[
            "copy",
            "--src-cert-dir",
            certs,
            "--src-creds",
            &deployer,
            &second,
            &destination,
        ])
        .await;
    for digest in &pushed {
        let hex = digest.trim_start_matches("sha256:");
        let blob = std::fs::read(out.join(hex)).unwrap();
        assert_eq!(&digest_of(&blob), digest);
        assert!(
            registry.served_blob(digest),
            "skopeo never fetched {digest}"
        );
    }

    registry.shutdown.cancel();
}

/// crane verifying the cluster CA (`SSL_CERT_FILE`, no `--insecure`):
/// refused with an untrusted CA, anonymously and outside its namespace; then
/// `crane auth login`, `crane append` (a push), and `crane blob` back for
/// every layer, checked against its digest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires crane on PATH and a prepared host; run with make test-standard-clients"]
async fn crane_pulls_layers_back_with_ca_verification() {
    let registry = start_client_registry().await;
    let host = format!("{CLIENT_HOST}:{}", registry.tls_port);
    let work = tempfile::tempdir().unwrap();
    let docker_config = work.path().join("docker");
    std::fs::create_dir_all(&docker_config).unwrap();
    let ca = work.path().join("ca.pem");
    let foreign = work.path().join("foreign.pem");
    std::fs::write(&ca, registry.root_ca_pem()).unwrap();
    std::fs::write(&foreign, foreign_ca_pem()).unwrap();
    std::fs::write(
        work.path().join("hello.txt"),
        format!("hello from crane {}\n", nonce(&registry)),
    )
    .unwrap();
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
    let layer = layer.to_str().unwrap();

    let trusting = |pem: &Path| {
        Client::new("crane")
            .with_env("DOCKER_CONFIG", &docker_config)
            .with_env("SSL_CERT_FILE", pem)
    };
    let crane = trusting(&ca);
    let image = format!("{host}/team/crane:v1");
    let outside = format!("{host}/other/crane:v1");

    let anonymous = crane.run(&["append", "-f", layer, "-t", &image]).await;
    assert_refused("anonymous crane append", &anonymous, UNAUTHORISED);

    crane
        .succeeds(&["auth", "login", &host, "-u", "ci", "-p", &registry.deployer])
        .await;
    let untrusted = trusting(&foreign)
        .run(&["append", "-f", layer, "-t", &image])
        .await;
    assert_refused(
        "crane append against an untrusted CA",
        &untrusted,
        UNTRUSTED_CA,
    );

    crane
        .succeeds(&[
            "auth",
            "login",
            &host,
            "-u",
            "team-ci",
            "-p",
            &registry.team_deployer,
        ])
        .await;
    let refused = crane.run(&["append", "-f", layer, "-t", &outside]).await;
    assert_refused(
        "crane append outside the token's namespace",
        &refused,
        DENIED,
    );

    crane
        .succeeds(&["auth", "login", &host, "-u", "ci", "-p", &registry.deployer])
        .await;
    crane.succeeds(&["append", "-f", layer, "-t", &image]).await;
    let manifest = crane.succeeds(&["manifest", &image]).await;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest.stdout).unwrap();
    let layers = manifest["layers"].as_array().unwrap();
    assert_eq!(layers.len(), 1);

    registry.blob_reads.lock().unwrap().clear();
    for layer in layers {
        let digest = layer["digest"].as_str().unwrap();
        let blob = crane
            .succeeds(&["blob", &format!("{host}/team/crane@{digest}")])
            .await;
        assert_eq!(digest_of(&blob.stdout), digest);
        assert!(registry.served_blob(digest), "crane never fetched {digest}");
    }
    let exported = crane.succeeds(&["export", &image, "-"]).await;
    assert!(
        String::from_utf8_lossy(&exported.stdout).contains("hello from crane"),
        "crane export lost the layer's file"
    );

    registry.shutdown.cancel();
}
