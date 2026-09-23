//! Registry uploads and test leases recover after the real Bun is SIGKILLed.
//!
//! Each test kills the Bun that owns registry bytes, restarts it (or lets a
//! new council leader take over) and checks that abandoned uploads and
//! expired leased repositories are reclaimed while shared content survives.
//! The two lease tests run a real TLS cluster and wait out a 30-second lease,
//! so `.config/nextest.toml` serialises them in the `cluster-heavy` group.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[path = "support/bun_process.rs"]
mod bun_process;
use bun_process::{
    BunProcess, WAIT, assert_success, reserve_address, run_relish, spawn_bun_with_port_retry,
    wait_for_relish, write_portable_node_config,
};

#[tokio::test]
async fn abandoned_registry_upload_is_reclaimed_after_bun_sigkill() {
    let root = tempfile::tempdir().unwrap();
    let node = write_portable_node_config(root.path());
    let (mut bun, address) = spawn_bun_with_port_retry(false, || {
        (
            node.clone(),
            reserve_address(),
            root.path().join("upload-before.log"),
        )
    });
    let endpoint = format!("http://{address}");
    wait_for_relish(&mut bun, &["--endpoint", &endpoint, "status"]);
    let client = reliaburger::relish::client::BunClient::new(&endpoint);
    let registry = client
        .capabilities()
        .await
        .unwrap()
        .service_endpoints
        .registry
        .unwrap();
    let http = client.registry_http_client(&registry).unwrap();
    let started = http
        .post(format!("{registry}/v2/abandoned/blobs/uploads/"))
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 202);
    let location = started.headers()["location"].to_str().unwrap();
    let id = location.rsplit('/').next().unwrap();
    let path = root.path().join("images/uploads").join(id);
    assert_eq!(
        http.patch(format!("{registry}{location}"))
            .body("partial")
            .send()
            .await
            .unwrap()
            .status(),
        202
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"partial");
    let mut competing = BunProcess::spawn(
        &node,
        "127.0.0.1:0".parse().unwrap(),
        false,
        root.path().join("upload-competing.log"),
    );
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = competing.child.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("competing Bun did not refuse the occupied upload directory");
    assert!(!status.success());
    let output = std::fs::read_to_string(&competing.log_path).unwrap();
    assert!(
        output.contains("registry upload directory is busy"),
        "{output}"
    );
    assert!(!output.contains("API server listening"), "{output}");
    assert_eq!(std::fs::read(&path).unwrap(), b"partial");
    bun.child.kill().unwrap();
    bun.child.wait().unwrap();
    let (mut replacement, address) = spawn_bun_with_port_retry(false, || {
        (
            node.clone(),
            reserve_address(),
            root.path().join("upload-after.log"),
        )
    });
    let endpoint = format!("http://{address}");
    wait_for_relish(&mut replacement, &["--endpoint", &endpoint, "status"]);
    assert!(
        !path.exists(),
        "restart lost the owner but retained the upload bytes"
    );
}

#[tokio::test]
async fn expired_registry_lease_recovers_after_bun_sigkill_and_preserves_shared_content() {
    qualify_registry_owner_crash(false).await;
}

#[tokio::test]
async fn new_leader_retains_registry_cleanup_until_the_killed_writer_returns() {
    qualify_registry_owner_crash(true).await;
}

async fn qualify_registry_owner_crash(with_followers: bool) {
    use reliaburger::pickle::types::ManifestCatalog;
    use reliaburger::relish::client::BunClient;
    use reliaburger::testkit::oci::{build_synthetic_image, push_image, push_leased_image};

    let root = tempfile::tempdir().unwrap();
    let cluster_dir = root.path().join("cluster");
    assert_success(
        &run_relish(&[
            "init",
            cluster_dir.to_str().unwrap(),
            "--cluster-name",
            "registry-crash",
            "--node-id",
            "node-01",
        ]),
        "initialise registry crash fixture",
    );
    let mut config = cluster_dir.join("reliaburger.toml");
    let mut node = reliaburger::config::NodeConfig::from_file(&config).unwrap();
    node.network.advertise_address = Some("127.0.0.1".into());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;
    // The disconnected node must remain the sole byte owner. Other replicas
    // would hide a lost receipt behind a readable surviving copy.
    node.images.redundancy = 1;
    node.testing.safety_class = reliaburger::testkit::safety::ClusterSafetyClass::Development;
    node.testing
        .allowed_operations
        .insert(reliaburger::testkit::safety::OperationPermission::ProvisionIsolatedWorkloads);
    let (mut bun, mut address) = spawn_bun_with_port_retry(true, || {
        let [gossip, raft, reporting, api] = reserve_cluster_port_block();
        node.cluster.gossip_port = gossip;
        node.cluster.raft_port = raft;
        node.cluster.reporting_port = reporting;
        std::fs::write(&config, toml::to_string_pretty(&node).unwrap()).unwrap();
        (
            config.clone(),
            SocketAddr::from(([127, 0, 0, 1], api)),
            root.path().join("before.log"),
        )
    });
    let mut endpoint = format!("https://{address}");
    let ca = cluster_dir.join("identity/root-ca.crt");
    let ca = ca.to_str().unwrap();
    wait_for_relish(
        &mut bun,
        &["--endpoint", &endpoint, "--ca-cert", ca, "status"],
    );
    let output = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "token",
        "create",
        "--name",
        "registry-crash-admin",
        "--role",
        "admin",
    ]);
    assert_success(&output, "create registry crash administrator");
    let token = String::from_utf8(output.stdout).unwrap();
    let token = token.trim();
    let deadline = Instant::now() + WAIT;
    while run_relish(&["--endpoint", &endpoint, "--ca-cert", ca, "token", "list"])
        .status
        .success()
    {
        assert!(
            Instant::now() < deadline,
            "authentication never left bootstrap"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ca_bytes = std::fs::read(ca).unwrap();
    let mut client = BunClient::new_with_ca(&endpoint, Some(token), &ca_bytes).unwrap();
    struct Peer {
        process: BunProcess,
        client: BunClient,
        endpoint: String,
        address: SocketAddr,
        config: PathBuf,
        node: reliaburger::config::NodeConfig,
    }
    let mut followers = Vec::new();
    if with_followers {
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        let root_der = CertificateDer::pem_slice_iter(&ca_bytes)
            .next()
            .unwrap()
            .unwrap();
        let fingerprint = reliaburger::sesame::identity_store::root_ca_fingerprint(&root_der);
        for number in 2..=3 {
            let name = format!("node-{number:02}");
            let issued = run_relish(&[
                "--endpoint",
                &endpoint,
                "--ca-cert",
                ca,
                "--token",
                token,
                "join-token",
                "create",
                "--node-id",
                &name,
            ]);
            assert_success(&issued, "mint registry follower join token");
            let join_token = String::from_utf8(issued.stdout).unwrap();
            let follower_root = root.path().join(&name);
            let identity_dir = follower_root.join("identity");
            assert_success(
                &run_relish(&[
                    "join",
                    "--token",
                    join_token.trim(),
                    "--node-id",
                    &name,
                    "--identity-dir",
                    identity_dir.to_str().unwrap(),
                    "--ca-fingerprint",
                    &fingerprint,
                    &endpoint,
                ]),
                "enrol registry follower",
            );
            let mut follower_node = node.clone();
            follower_node.node.name = Some(name.clone());
            follower_node.security.identity_dir = Some(identity_dir);
            follower_node.security.bootstrap_path = None;
            follower_node.cluster.join = vec![format!("127.0.0.1:{}", node.cluster.gossip_port)];
            follower_node.storage.data = follower_root.join("data");
            follower_node.storage.images = follower_root.join("images");
            follower_node.storage.logs = follower_root.join("logs");
            follower_node.storage.metrics = follower_root.join("metrics");
            follower_node.storage.volumes = follower_root.join("volumes");
            let follower_config = follower_root.join("node.toml");
            let (mut process, address) = spawn_bun_with_port_retry(true, || {
                let [gossip, raft, reporting, api] = reserve_cluster_port_block();
                follower_node.cluster.gossip_port = gossip;
                follower_node.cluster.raft_port = raft;
                follower_node.cluster.reporting_port = reporting;
                std::fs::write(
                    &follower_config,
                    toml::to_string_pretty(&follower_node).unwrap(),
                )
                .unwrap();
                (
                    follower_config.clone(),
                    SocketAddr::from(([127, 0, 0, 1], api)),
                    follower_root.join("bun.log"),
                )
            });
            let url = format!("https://{address}");
            wait_for_relish(
                &mut process,
                &[
                    "--endpoint",
                    &url,
                    "--ca-cert",
                    ca,
                    "--token",
                    token,
                    "status",
                ],
            );
            let observer = BunClient::new_with_ca(&url, Some(token), &ca_bytes).unwrap();
            followers.push(Peer {
                process,
                client: observer,
                endpoint: url,
                address,
                config: follower_config,
                node: follower_node,
            });
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        let leader = loop {
            let mut views = Vec::new();
            let mut all_voters_observed = true;
            for (observer, url) in std::iter::once((&client, &endpoint))
                .chain(followers.iter().map(|peer| (&peer.client, &peer.endpoint)))
            {
                let view: reliaburger::bun::agent::CouncilStatus = observer
                    .http()
                    .unwrap()
                    .get(format!("{url}/v1/cluster/council"))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                let members = observer.nodes().await.unwrap();
                all_voters_observed &= members.len() == 3
                    && members
                        .iter()
                        .all(|member| member.is_council && member.state == "alive");
                views.push(view);
            }
            if all_voters_observed
                && let Some(leader) = &views[0].leader
                && views
                    .iter()
                    .all(|view| view.members.len() == 3 && view.leader.as_ref() == Some(leader))
            {
                break leader.clone();
            }
            assert!(
                Instant::now() < deadline,
                "three TLS council voters did not converge: {views:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        // Startup elections may legitimately replace the bootstrap leader.
        // Make the observed leader the sole writer before killing it.
        if Some(&leader) != node.node.name.as_ref() {
            let peer = followers
                .iter_mut()
                .find(|peer| peer.node.node.name.as_ref() == Some(&leader))
                .unwrap();
            std::mem::swap(&mut bun, &mut peer.process);
            std::mem::swap(&mut client, &mut peer.client);
            std::mem::swap(&mut endpoint, &mut peer.endpoint);
            std::mem::swap(&mut address, &mut peer.address);
            std::mem::swap(&mut config, &mut peer.config);
            std::mem::swap(&mut node, &mut peer.node);
        }
    }
    let registry = client
        .capabilities()
        .await
        .unwrap()
        .service_endpoints
        .registry
        .unwrap();
    let http = client.registry_http_client(&registry).unwrap();
    let image = build_synthetic_image("shared-across-crash");
    push_image(&http, &registry, "ordinary", "v1", &image)
        .await
        .unwrap();
    let lease = client.create_test_lease(30, None).await.unwrap();
    let repository = format!("{}/image", lease.namespace);
    push_leased_image(&http, &registry, &repository, "v1", &image, &lease.lease_id)
        .await
        .unwrap();
    let started = http
        .post(format!("{registry}/v2/{repository}/blobs/uploads/"))
        .header("x-reliaburger-test-lease", &lease.lease_id)
        .send()
        .await
        .unwrap();
    assert_eq!(started.status(), 202);
    let location = started.headers()["location"].to_str().unwrap();
    let upload = node
        .storage
        .images
        .join("uploads")
        .join(location.rsplit('/').next().unwrap());
    let patched = http
        .patch(format!("{registry}{location}"))
        .header("x-reliaburger-test-lease", &lease.lease_id)
        .body("partial leased content")
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 202);
    assert_eq!(std::fs::read(&upload).unwrap(), b"partial leased content");
    let catalogue_path = node.storage.data.join("pickle-catalog.json");
    let catalogue: ManifestCatalog =
        serde_json::from_slice(&std::fs::read(&catalogue_path).unwrap()).unwrap();
    assert_eq!(
        catalogue.repository_owners.get(&repository),
        Some(&lease.lease_id)
    );
    let observed = client
        .http()
        .unwrap()
        .get(format!("{endpoint}/v1/test/leases/{}", lease.lease_id))
        .send()
        .await
        .unwrap();
    assert_eq!(observed.status(), 200);
    let observed: reliaburger::testkit::lease::TestLease = observed.json().await.unwrap();
    assert_eq!(observed.repositories[&repository].len(), 1);

    // Kill the actual owner, abandon the writing client and let the original
    // server-issued deadline expire. The replacement observer never renews or
    // explicitly releases the lease, so only durable recovery can retire it.
    if with_followers {
        let view: reliaburger::bun::agent::CouncilStatus = client
            .http()
            .unwrap()
            .get(format!("{endpoint}/v1/cluster/council"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            view.leader, node.node.name,
            "writer lost leadership before fault injection"
        );
    }
    bun.child.kill().unwrap();
    bun.child.wait().unwrap();
    drop(http);
    drop(client);
    if with_followers {
        let deadline = Instant::now() + Duration::from_secs(70);
        loop {
            let mut confirmed = true;
            for peer in &mut followers {
                peer.process.assert_running();
                let observer = &peer.client;
                let url = &peer.endpoint;
                let view: reliaburger::bun::agent::CouncilStatus = observer
                    .http()
                    .unwrap()
                    .get(format!("{url}/v1/cluster/council"))
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if view.leader.is_none() || view.leader == node.node.name {
                    confirmed = false;
                    continue;
                }
                let response = observer
                    .http()
                    .unwrap()
                    .get(format!("{url}/v1/test/leases/{}", lease.lease_id))
                    .send()
                    .await
                    .unwrap();
                if response.status() == 503 {
                    confirmed = false;
                    continue;
                }
                assert_eq!(
                    response.status(),
                    200,
                    "new leader forgot an unconfirmed writer"
                );
                let pending: reliaburger::testkit::lease::TestLease =
                    response.json().await.unwrap();
                assert_eq!(
                    pending.repositories[&repository],
                    observed.repositories[&repository]
                );
                confirmed &= pending.workloads_retired
                    && matches!(
                        pending.state,
                        reliaburger::testkit::lease::TestLeaseState::Cleaning { .. }
                    );
            }
            if confirmed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "new leader did not retain pending registry cleanup"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            upload.exists(),
            "dead node's bytes vanished without a storage worker"
        );
    }
    let mut replacement = BunProcess::spawn(&config, address, true, root.path().join("after.log"));
    wait_for_relish(
        &mut replacement,
        &[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            token,
            "status",
        ],
    );
    let observer = BunClient::new_with_ca(&endpoint, Some(token), &ca_bytes).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        replacement.assert_running();
        let response = observer
            .http()
            .unwrap()
            .get(format!("{endpoint}/v1/test/leases/{}", lease.lease_id))
            .send()
            .await
            .unwrap();
        if response.status() == 404 {
            break;
        }
        assert_eq!(response.status(), 200);
        let pending = response.text().await.unwrap();
        assert!(
            Instant::now() < deadline,
            "registry lease did not retire: {pending}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for peer in &followers {
        let url = &peer.endpoint;
        let response = peer
            .client
            .http()
            .unwrap()
            .get(format!("{url}/v1/test/leases/{}", lease.lease_id))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            404,
            "retirement did not reach the surviving council"
        );
    }
    let registry = observer
        .capabilities()
        .await
        .unwrap()
        .service_endpoints
        .registry
        .unwrap();
    let http = observer.registry_http_client(&registry).unwrap();
    assert_eq!(
        http.get(format!(
            "{registry}/v2/{repository}/manifests/{}",
            image.manifest_digest
        ))
        .send()
        .await
        .unwrap()
        .status(),
        404
    );
    let ordinary = http
        .get(format!(
            "{registry}/v2/ordinary/manifests/{}",
            image.manifest_digest
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(ordinary.status(), 200);
    assert_eq!(ordinary.bytes().await.unwrap().as_ref(), image.manifest);
    for (digest, expected) in [
        (&image.config_digest, &image.config),
        (&image.layer_digest, &image.layer),
    ] {
        let blob = http
            .get(format!("{registry}/v2/ordinary/blobs/{digest}"))
            .send()
            .await
            .unwrap();
        assert_eq!(blob.status(), 200);
        assert_eq!(blob.bytes().await.unwrap().as_ref(), expected);
    }
    assert!(!upload.exists());
    assert_eq!(
        std::fs::read_dir(node.storage.images.join("uploads"))
            .unwrap()
            .count(),
        0
    );
    let catalogue: ManifestCatalog =
        serde_json::from_slice(&std::fs::read(&catalogue_path).unwrap()).unwrap();
    assert!(!catalogue.repository_owners.contains_key(&repository));
    assert!(
        catalogue
            .manifests
            .iter()
            .all(|(_, manifest)| manifest.repository != repository)
    );
    assert!(
        catalogue
            .manifests
            .iter()
            .any(|(_, manifest)| manifest.repository == "ordinary")
    );
}

/// Gossip derives peer transport addresses using cluster-uniform offsets.
/// Keep TCP and UDP reservations alive together until the whole block is free.
fn reserve_cluster_port_block() -> [u16; 4] {
    for _ in 0..100 {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = first.local_addr().unwrap().port();
        if base > u16::MAX - 3 {
            continue;
        }
        let ports = [base, base + 1, base + 2, base + 3];
        let Ok(_gossip) = std::net::UdpSocket::bind(("127.0.0.1", base)) else {
            continue;
        };
        let remaining: std::io::Result<Vec<_>> = ports[1..]
            .iter()
            .map(|port| TcpListener::bind(("127.0.0.1", *port)))
            .collect();
        if remaining.is_ok() {
            return ports;
        }
    }
    panic!("could not reserve a complete cluster transport port block");
}
