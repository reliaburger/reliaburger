//! `relish test` catalogue groups qualified against a real single-node cluster.
//!
//! Each test starts a secure Bun, runs one catalogue group through the
//! compiled `relish test` and checks both the verdicts and that every case
//! confirmed its cleanup. The `runc_catalogue_*` tests need rootful runc and
//! run under `make test-linux`, which selects them by the `runc_` prefix.

use std::time::{Duration, Instant};

#[path = "support/bun_process.rs"]
mod bun_process;
#[cfg(target_os = "linux")]
use bun_process::spawn_bun_with_runtime_port_retry;
use bun_process::{
    WAIT, assert_success, reserve_address, reserve_ports, run_relish, spawn_bun_with_port_retry,
    wait_for_relish,
};

#[test]
fn secure_catalogue_scoped_token_uses_explicit_ca_and_server_owned_cleanup() {
    qualify_process_catalogue("workload-identity");
}

#[test]
fn secure_catalogue_node_jobs_have_durable_ownership_and_confirmed_cleanup() {
    qualify_process_catalogue("jobs");
}

/// A secure single-node Bun on the process runtime, with an admin token.
struct SecureNode {
    bun: bun_process::BunProcess,
    endpoint: String,
    ca: std::path::PathBuf,
    token: String,
}

fn start_secure_process_node(root: &std::path::Path) -> SecureNode {
    let cluster_dir = root.join("cluster");
    assert_success(
        &run_relish(&[
            "init",
            cluster_dir.to_str().unwrap(),
            "--cluster-name",
            "token-lease",
            "--node-id",
            "node-01",
        ]),
        "initialise scoped-token fixture",
    );
    let node_path = cluster_dir.join("reliaburger.toml");
    let mut node = reliaburger::config::NodeConfig::from_file(&node_path).unwrap();
    node.node.name = Some("node-01".into());
    node.network.advertise_address = Some("127.0.0.1".into());
    node.storage.data = root.join("data");
    node.storage.images = root.join("images");
    node.storage.logs = root.join("logs");
    node.storage.metrics = root.join("metrics");
    node.storage.volumes = root.join("volumes");
    node.images.registry_port = 0;
    node.process_workloads.allowed_binaries =
        vec!["/bin/sh".into(), "/bin/sleep".into(), "/bin/true".into()];
    node.testing.safety_class = reliaburger::testkit::safety::ClusterSafetyClass::Development;
    node.testing
        .allowed_operations
        .insert(reliaburger::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads);
    let (mut bun, address) = spawn_bun_with_port_retry(true, || {
        let [gossip, raft, reporting] = reserve_ports();
        node.cluster.gossip_port = gossip;
        node.cluster.raft_port = raft;
        node.cluster.reporting_port = reporting;
        std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();
        (
            node_path.clone(),
            reserve_address(),
            root.join("token-lease-bun.log"),
        )
    });
    let endpoint = format!("https://{address}");
    let ca_path = cluster_dir.join("identity/root-ca.crt");
    let ca = ca_path.to_str().unwrap();
    wait_for_relish(
        &mut bun,
        &["--endpoint", &endpoint, "--ca-cert", ca, "status"],
    );
    let token = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "token",
        "create",
        "--name",
        "test-admin",
        "--role",
        "admin",
    ]);
    assert_success(&token, "create catalogue admin");
    let token = String::from_utf8(token.stdout).unwrap().trim().to_string();
    let deadline = Instant::now() + WAIT;
    // The auth-store refresh is asynchronous; wait until anonymous management
    // is refused, so the probe cannot accidentally run in bootstrap mode.
    loop {
        let anonymous = run_relish(&["--endpoint", &endpoint, "--ca-cert", ca, "token", "list"]);
        if !anonymous.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "auth store did not adopt the admin token"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    SecureNode {
        bun,
        endpoint,
        ca: ca_path,
        token,
    }
}

fn qualify_process_catalogue(group: &str) {
    let root = tempfile::tempdir().unwrap();
    let secure = start_secure_process_node(root.path());
    let _bun = secure.bun;
    let endpoint = secure.endpoint;
    let ca = secure.ca.to_str().unwrap();
    let token = secure.token.as_str();
    let node_data = root.path().join("data");
    let output = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "--token",
        token,
        "--output",
        "json",
        "test",
        "--filter",
        group,
        "--timeout",
        if group == "jobs" { "90s" } else { "15s" },
    ]);
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid catalogue JSON: {error}; stdout={}; stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
    if group == "jobs" {
        assert_success(&output, "qualify node-local jobs catalogue");
        let results = report["results"].as_array().unwrap();
        assert_eq!(results.len(), 3, "{report}");
        for case in results {
            assert_eq!(case["outcome"]["status"], "pass", "{case}");
            assert_eq!(case["cleanup"]["status"], "confirmed", "{case}");
        }
        let leases = std::fs::read_to_string(node_data.join("node-test-leases.json")).unwrap();
        assert!(!leases.contains("node-jobs-"), "{leases}");
        return;
    }
    let case = report["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "namespace_scoped_token_is_rejected_elsewhere")
        .unwrap();
    assert_eq!(case["outcome"]["status"], "pass", "{case}");
    assert_eq!(case["cleanup"]["status"], "confirmed", "{case}");
    let listed = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "--token",
        token,
        "token",
        "list",
    ]);
    assert_success(&listed, "inspect token cleanup");
    let listed = String::from_utf8(listed.stdout).unwrap();
    assert!(listed.contains("test-admin"));
    assert!(
        !listed.contains("rbtest-"),
        "test token survived cleanup: {listed}"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires rootful runc, networking tools and registry access"]
fn runc_catalogue_decrypts_secrets_using_only_the_public_key_api() {
    qualify_runc_catalogue("secrets-config");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires rootful runc, networking tools and registry access"]
fn runc_catalogue_verifies_workload_spiffe_certificates() {
    qualify_runc_catalogue("workload-identity");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires rootful runc, networking tools and registry access"]
fn runc_catalogue_deploys_the_exact_image_pushed_to_pickle() {
    qualify_runc_catalogue("image-registry");
}

/// The V02 soak ran `relish test --endpoint <a node's forwarded API>` from the
/// Mac. `--endpoint` bypassed the managed context, so the registry cases
/// pushed to the node's own listener port, which nothing forwards. Relish now
/// keeps the context's host forwards for a connection pinned to the same
/// cluster CA. Here the forward is a counting TCP proxy on another port: the
/// cases must pass *through it*.
#[test]
fn registry_cases_push_through_the_contexts_forward_with_an_explicit_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let secure = start_secure_process_node(root.path());
    let _bun = secure.bun;
    let ca = secure.ca.to_str().unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let reported = runtime
        .block_on(
            reliaburger::relish::client::BunClient::new_with_ca(
                &secure.endpoint,
                Some(&secure.token),
                &std::fs::read(&secure.ca).unwrap(),
            )
            .unwrap()
            .capabilities_as_reported(),
        )
        .unwrap();
    let listener = reported
        .service_endpoints
        .registry
        .expect("the node reports its registry listener");
    let registry_port = url::Url::parse(&listener)
        .unwrap()
        .port_or_known_default()
        .unwrap();

    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let forward = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let forward_port = forward.local_addr().unwrap().port();
    assert_ne!(forward_port, registry_port);
    let counted = connections.clone();
    runtime.spawn(async move {
        while let Ok((mut inbound, _)) = forward.accept().await {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                if let Ok(mut outbound) =
                    tokio::net::TcpStream::connect(("127.0.0.1", registry_port)).await
                {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });

    // The managed context a quickstart would write, declaring that forward.
    let home = root.path().join("relish-home");
    reliaburger::relish::local_context::LocalContext {
        schema: 1,
        owner: "forward-fixture".to_string(),
        endpoint: secure.endpoint.clone(),
        token: secure.token.clone(),
        ca_cert: secure.ca.clone(),
        service_endpoints: reliaburger::bun::capabilities::ServiceEndpoints {
            registry: Some(format!("https://127.0.0.1:{forward_port}")),
            ..Default::default()
        },
    }
    .save(&home.join("context.json"))
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_relish"))
        .args([
            "--endpoint",
            &secure.endpoint,
            "--ca-cert",
            ca,
            "--token",
            &secure.token,
            "--output",
            "json",
            "test",
            "--filter",
            "image-registry",
            "--timeout",
            "30s",
        ])
        .env("RELIABURGER_HOME", &home)
        .env_remove("RELIABURGER_ENDPOINT")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid catalogue JSON: {error}; stdout={}; stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
    for name in [
        "push_and_pull_image_roundtrip",
        "manifest_catalog_lists_pushed_image",
    ] {
        let case = report["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing from {report}"));
        assert_eq!(case["outcome"]["status"], "pass", "{case}");
    }
    assert!(
        connections.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the registry cases never used the declared forward: {report}"
    );
}

#[cfg(target_os = "linux")]
fn qualify_runc_catalogue(group: &str) {
    assert!(
        nix::unistd::geteuid().is_root(),
        "run this qualification as root"
    );
    let root = tempfile::tempdir().unwrap();
    let cluster_dir = root.path().join("cluster");
    assert_success(
        &run_relish(&[
            "init",
            cluster_dir.to_str().unwrap(),
            "--cluster-name",
            "identity-catalogue",
            "--node-id",
            "node-01",
        ]),
        "initialise catalogue fixture",
    );
    let node_path = cluster_dir.join("reliaburger.toml");
    let mut node = reliaburger::config::NodeConfig::from_file(&node_path).unwrap();
    node.node.name = Some("node-01".into());
    node.network.advertise_address = Some("127.0.0.1".into());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;
    // Pull the pinned workload from the local test mirror when the harness
    // runs one, so a slow public registry can't fail the timed catalogue.
    node.images.mirrors = reliaburger::testkit::pinned_images::local_test_mirrors().unwrap();
    node.testing.safety_class = reliaburger::testkit::safety::ClusterSafetyClass::Development;
    node.testing
        .allowed_operations
        .insert(reliaburger::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads);
    let (mut bun, address) = spawn_bun_with_runtime_port_retry(true, "runc", || {
        let [gossip, raft, reporting] = reserve_ports();
        node.cluster.gossip_port = gossip;
        node.cluster.raft_port = raft;
        node.cluster.reporting_port = reporting;
        std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();
        (
            node_path.clone(),
            reserve_address(),
            root.path().join("secrets-bun.log"),
        )
    });
    let endpoint = format!("https://{address}");
    let ca = cluster_dir.join("identity/root-ca.crt");
    let ca = ca.to_str().unwrap();
    wait_for_relish(
        &mut bun,
        &["--endpoint", &endpoint, "--ca-cert", ca, "status"],
    );
    let token = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "token",
        "create",
        "--name",
        "catalogue-test-admin",
        "--role",
        "admin",
    ]);
    assert_success(&token, "create catalogue administrator");
    let token = String::from_utf8(token.stdout).unwrap();
    let deadline = Instant::now() + WAIT;
    while run_relish(&["--endpoint", &endpoint, "--ca-cert", ca, "token", "list"])
        .status
        .success()
    {
        assert!(
            Instant::now() < deadline,
            "authentication never left bootstrap"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let output = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "--token",
        token.trim(),
        "--output",
        "json",
        "test",
        "--filter",
        group,
        "--timeout",
        "90s",
        "--parallel",
        "1",
    ]);
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "invalid catalogue report: {error}; stdout={}; stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        });
    let results = report["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "{report}");
    for case in results {
        assert_eq!(case["outcome"]["status"], "pass", "{report}");
        assert_eq!(case["cleanup"]["status"], "confirmed", "{report}");
    }
    assert_success(&output, "qualify runtime catalogue");
    if group == "image-registry" {
        let listed = run_relish(&[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            token.trim(),
            "--output",
            "json",
            "images",
        ]);
        assert_success(&listed, "inspect catalogue after confirmed cleanup");
        let catalogue: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
        assert!(
            catalogue["images"].as_array().unwrap().is_empty(),
            "fixture repositories survived confirmed cleanup: {catalogue}"
        );
    }
    if group == "workload-identity" {
        // A Node CA can authenticate the API, but cannot validate a workload
        // leaf issued by the separate Workload CA. The mounted bundle must
        // never be promoted into the client's own trust anchors.
        let node_ca = cluster_dir.join("identity/node-ca.crt");
        let output = run_relish(&[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            node_ca.to_str().unwrap(),
            "--token",
            token.trim(),
            "--output",
            "json",
            "test",
            "--filter",
            "workload-identity",
            "--timeout",
            "90s",
            "--parallel",
            "1",
        ]);
        let report: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "negative trust probe returned no report: {error}; stderr={}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        let results = report["results"].as_array().unwrap();
        assert_eq!(results.len(), 3, "{report}");
        let certificate = results
            .iter()
            .find(|case| case["name"] == "workload_receives_spiffe_certificate")
            .unwrap();
        assert_eq!(certificate["outcome"]["status"], "fail", "{report}");
        assert!(
            certificate["outcome"]["reason"]
                .as_str()
                .unwrap()
                .contains("workload certificate chain is invalid"),
            "{report}"
        );
        for case in results {
            assert_eq!(case["cleanup"]["status"], "confirmed", "{report}");
        }
        assert!(
            !output.status.success(),
            "untrusted workload certificate passed"
        );
    }
}
