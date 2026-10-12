//! #697: a pushed image survives losing the node it was pushed to.
//!
//! Three real `bun --cluster` processes form a TLS council. Pickle addresses
//! its peers by gossip IP and one cluster-wide registry port, so each node
//! needs its own address: the nodes use 127.0.0.1, .2 and .3, which only
//! Linux routes without configuration. The test pushes an image to node 1,
//! waits for heal to make the second copy, kills node 1 for good, and then
//! waits for heal to notice the dead copy and make a new one on the third
//! node. Both survivors must then serve every blob, and nothing is Lost.
//!
//! Heal ticks once a minute, so this takes a few minutes: it runs only with
//! `RELIABURGER_CLUSTER_TESTS=1`, through `make test-cluster`.

#![cfg(target_os = "linux")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[path = "support/bun_process.rs"]
mod bun_process;
use bun_process::{
    BunProcess, WAIT, assert_success, reserve_port_block_lease, run_relish,
    spawn_bun_with_port_retry, wait_for_relish,
};

use reliaburger::pickle::capability::RegistryCapabilityEvidence;
use reliaburger::relish::client::BunClient;

/// One cluster node: its process, an authenticated client and its address.
struct Node {
    name: String,
    process: BunProcess,
    client: BunClient,
    ip: Ipv4Addr,
}

impl Node {
    /// This node's live registry evidence, or `None` while it can't say.
    async fn registry(&self) -> Option<RegistryCapabilityEvidence> {
        let capabilities = self.client.capabilities().await.ok()?;
        Some(capabilities.placement?.registry)
    }
}

/// Poll `check` every second until it returns true or `timeout` passes.
async fn wait_until(what: &str, timeout: Duration, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test]
#[ignore = "three real Bun processes over several heal ticks; run with make test-cluster"]
async fn image_survives_loss_of_the_pushing_node() {
    if std::env::var("RELIABURGER_CLUSTER_TESTS").is_err() {
        eprintln!("skipping: set RELIABURGER_CLUSTER_TESTS=1");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let cluster_dir = root.path().join("cluster");
    assert_success(
        &run_relish(&[
            "init",
            cluster_dir.to_str().unwrap(),
            "--cluster-name",
            "durability",
            "--node-id",
            "node-01",
        ]),
        "initialise the durability cluster",
    );
    let ca = cluster_dir.join("identity/root-ca.crt");
    let ca_text = ca.to_str().unwrap().to_string();
    let ca_bytes = std::fs::read(&ca).unwrap();
    let mut base =
        reliaburger::config::NodeConfig::from_file(&cluster_dir.join("reliaburger.toml")).unwrap();
    base.ebpf.enabled = false;
    base.dns.enabled = false;
    base.images.redundancy = 2;
    // Every node serves its registry on the same port, on its own address.
    let registry_lease = reserve_port_block_lease(1);
    base.images.registry_port = registry_lease.base;

    // Each node's ports stay claimed for the whole test, including after a
    // node is killed.
    let mut leases = vec![registry_lease];
    let mut spawn = |name: &str, ip: Ipv4Addr, node: &mut reliaburger::config::NodeConfig| {
        let node_root = root.path().join(name);
        node.node.name = Some(name.to_string());
        node.network.advertise_address = Some(ip.to_string());
        node.images.registry_bind = ip.to_string();
        node.storage.data = node_root.join("data");
        node.storage.images = node_root.join("images");
        node.storage.logs = node_root.join("logs");
        node.storage.metrics = node_root.join("metrics");
        node.storage.volumes = node_root.join("volumes");
        std::fs::create_dir_all(&node_root).unwrap();
        let config = node_root.join("node.toml");
        spawn_bun_with_port_retry(true, || {
            let lease = reserve_port_block_lease(4);
            let [gossip, raft, reporting, api] =
                [lease.base, lease.base + 1, lease.base + 2, lease.base + 3];
            leases.push(lease);
            node.cluster.gossip_port = gossip;
            node.cluster.raft_port = raft;
            node.cluster.reporting_port = reporting;
            std::fs::write(&config, toml::to_string_pretty(&*node).unwrap()).unwrap();
            (
                config.clone(),
                SocketAddr::new(IpAddr::V4(ip), api),
                node_root.join("bun.log"),
            )
        })
    };

    let mut first = base.clone();
    let (mut first_process, first_address) =
        spawn("node-01", Ipv4Addr::new(127, 0, 0, 1), &mut first);
    let first_endpoint = format!("https://{first_address}");
    wait_for_relish(
        &mut first_process,
        &[
            "--endpoint",
            &first_endpoint,
            "--ca-cert",
            &ca_text,
            "status",
        ],
    );
    let created = run_relish(&[
        "--endpoint",
        &first_endpoint,
        "--ca-cert",
        &ca_text,
        "token",
        "create",
        "--name",
        "durability-admin",
        "--role",
        "admin",
    ]);
    assert_success(&created, "create the durability administrator");
    let token = String::from_utf8(created.stdout)
        .unwrap()
        .trim()
        .to_string();
    let deadline = Instant::now() + WAIT;
    while run_relish(&[
        "--endpoint",
        &first_endpoint,
        "--ca-cert",
        &ca_text,
        "token",
        "list",
    ])
    .status
    .success()
    {
        assert!(
            Instant::now() < deadline,
            "authentication never left bootstrap"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fingerprint = {
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        let root_der = CertificateDer::pem_slice_iter(&ca_bytes)
            .next()
            .unwrap()
            .unwrap();
        reliaburger::sesame::identity_store::root_ca_fingerprint(&root_der)
    };

    let mut nodes = vec![Node {
        name: "node-01".into(),
        process: first_process,
        client: BunClient::new_with_ca(&first_endpoint, Some(&token), &ca_bytes).unwrap(),
        ip: Ipv4Addr::new(127, 0, 0, 1),
    }];
    for number in 2..=3u8 {
        let name = format!("node-{number:02}");
        let issued = run_relish(&[
            "--endpoint",
            &first_endpoint,
            "--ca-cert",
            &ca_text,
            "--token",
            &token,
            "join-token",
            "create",
            "--node-id",
            &name,
        ]);
        assert_success(&issued, "mint a join token");
        let identity_dir: PathBuf = root.path().join(&name).join("identity");
        assert_success(
            &run_relish(&[
                "join",
                "--token",
                String::from_utf8(issued.stdout).unwrap().trim(),
                "--node-id",
                &name,
                "--identity-dir",
                identity_dir.to_str().unwrap(),
                "--ca-fingerprint",
                &fingerprint,
                &first_endpoint,
            ]),
            "enrol a follower",
        );
        let ip = Ipv4Addr::new(127, 0, 0, number);
        let mut follower = base.clone();
        follower.security.identity_dir = Some(identity_dir);
        follower.security.bootstrap_path = None;
        follower.cluster.join = vec![format!("127.0.0.1:{}", first.cluster.gossip_port)];
        let (mut process, address) = spawn(&name, ip, &mut follower);
        let endpoint = format!("https://{address}");
        wait_for_relish(
            &mut process,
            &[
                "--endpoint",
                &endpoint,
                "--ca-cert",
                &ca_text,
                "--token",
                &token,
                "status",
            ],
        );
        nodes.push(Node {
            name,
            process,
            client: BunClient::new_with_ca(&endpoint, Some(&token), &ca_bytes).unwrap(),
            ip,
        });
    }
    wait_until(
        "three live registries",
        Duration::from_secs(120),
        async || {
            let mut ready = true;
            for node in &nodes {
                ready &= node
                    .registry()
                    .await
                    .is_some_and(|registry| registry.ready && registry.known_nodes == 3);
            }
            ready
        },
    )
    .await;

    // Push to node 1, then let heal make the second copy somewhere else.
    let registry = format!("https://{}:{}", nodes[0].ip, base.images.registry_port);
    let http = nodes[0].client.registry_http_client(&registry).unwrap();
    let image = reliaburger::testkit::oci::build_synthetic_image("survives-node-loss");
    reliaburger::testkit::oci::push_image(&http, &registry, "durable", "v1", &image)
        .await
        .unwrap();
    wait_until(
        "heal to reach two copies",
        Duration::from_secs(240),
        async || {
            nodes[0]
                .registry()
                .await
                .is_some_and(|registry| registry.under_replicated_layers == 0)
        },
    )
    .await;

    // Node 1 dies for good. Its copy no longer counts, so heal must make a
    // new one on the third node from the survivor that holds it.
    let mut lost = nodes.remove(0);
    lost.process.child.kill().unwrap();
    lost.process.child.wait().unwrap();
    eprintln!("killed {}", lost.name);
    wait_until(
        "heal to restore two live copies after the node loss",
        Duration::from_secs(360),
        async || {
            let mut healed = true;
            for node in &nodes {
                healed &= node.registry().await.is_some_and(|registry| {
                    registry.known_nodes == 2
                        && registry.under_replicated_layers == 0
                        && registry.lost_image_count == 0
                });
            }
            healed
        },
    )
    .await;

    // A deploy on node 3 resolves the tag and pulls every blob from the
    // surviving copies; both survivors must hold all of them now.
    for node in &nodes {
        let survivor = format!("https://{}:{}", node.ip, base.images.registry_port);
        let http = node.client.registry_http_client(&survivor).unwrap();
        let manifest = reliaburger::testkit::oci::fetch_manifest(&http, &survivor, "durable", "v1")
            .await
            .unwrap();
        assert_eq!(
            reliaburger::testkit::oci::sha256_digest(&manifest),
            image.manifest_digest
        );
        for (digest, bytes) in [
            (&image.manifest_digest, &image.manifest),
            (&image.config_digest, &image.config),
            (&image.layer_digest, &image.layer),
        ] {
            let response = http
                .get(format!("{survivor}/v2/durable/blobs/{digest}"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200, "{} lacks {digest}", node.name);
            assert_eq!(&response.bytes().await.unwrap().to_vec(), bytes);
        }
    }
}
