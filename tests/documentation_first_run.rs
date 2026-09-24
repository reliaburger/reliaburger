//! Executable contracts for the two first-run paths published in the docs.
//!
//! The standalone path starts one Bun and applies the shipped example. The
//! secure cluster path runs `relish init`, mints the first admin token, deploys
//! and enrols joiners with post-bootstrap join tokens. The drift tests read the
//! READMEs, whitepaper, Relish design doc and book chapters that publish those
//! commands, so CI's docs-only detection (`scripts/ci/select-jobs.sh`) relies on
//! this binary.
//!
//! These are black-box tests on purpose. A parser unit test can prove that a
//! flag exists, but it cannot prove that the documented Bun and Relish
//! processes can actually talk to each other.
//!
//! Crash-recovery and catalogue qualifications that also drive a real Bun live
//! with their subjects: `registry_recovery`, `job_recovery`, `node_renewal` and
//! `relish_test_catalogue`.

use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

#[path = "support/bun_process.rs"]
mod bun_process;
use bun_process::{
    WAIT, assert_success, reserve_address, reserve_ports, run_relish, spawn_bun_with_port_retry,
    wait_for_relish, wait_for_relish_output, write_portable_node_config,
    write_portable_node_config_with_ports,
};

#[test]
fn standalone_first_run_reaches_a_running_workload() {
    let root = tempfile::tempdir().unwrap();
    let (mut bun, address) = spawn_bun_with_port_retry(false, || {
        (
            write_portable_node_config(root.path()),
            reserve_address(),
            root.path().join("bun.log"),
        )
    });
    let endpoint = format!("http://{address}");

    wait_for_relish(&mut bun, &["--endpoint", &endpoint, "status"]);

    // The shipped example hardcodes `target/debug/testapp` for readers following
    // the documented `cargo build --bins` workflow. Under `cargo llvm-cov` the
    // binaries live in a different target dir, so apply a copy pointing at the
    // testapp this test was built against (mirrors the clustered case below).
    let example =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/phase-1/proc-first-run.toml");
    let manifest = std::fs::read_to_string(&example)
        .unwrap()
        .replace("target/debug/testapp", env!("CARGO_BIN_EXE_testapp"));
    let applied = root.path().join("proc-first-run.toml");
    std::fs::write(&applied, manifest).unwrap();
    let apply = run_relish(&["--endpoint", &endpoint, "apply", applied.to_str().unwrap()]);
    assert_success(&apply, "documented standalone apply");

    let status = run_relish(&["--endpoint", &endpoint, "status"]);
    assert_success(&status, "documented standalone status");
    assert!(String::from_utf8_lossy(&status.stdout).contains("hello"));

    let top = run_relish(&["--endpoint", &endpoint, "top"]);
    assert_success(&top, "documented standalone top");
    assert!(String::from_utf8_lossy(&top.stdout).contains("hello"));
}

#[test]
fn spawn_retries_when_a_foreign_listener_answers_on_the_requested_api_port() {
    let root = tempfile::tempdir().unwrap();
    let foreign = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied = foreign.local_addr().unwrap();
    let mut attempts = 0;
    let (mut bun, address) = spawn_bun_with_port_retry(false, || {
        attempts += 1;
        (
            write_portable_node_config(root.path()),
            if attempts == 1 {
                occupied
            } else {
                reserve_address()
            },
            root.path().join(format!("foreign-listener-{attempts}.log")),
        )
    });
    assert_eq!(
        attempts, 2,
        "a foreign listener was mistaken for the launched Bun"
    );
    assert_ne!(address, occupied);
    bun.assert_running();
    assert!(
        std::fs::read_to_string(&bun.log_path)
            .unwrap()
            .contains(&format!("bun: API server listening on {address}"))
    );
}

#[test]
fn spawn_retries_after_a_lost_port_race() {
    // Force the race deterministically: keep listening on a port and hand
    // it to the first attempt as the Raft port (the CI failure shape —
    // gossip comes up, a later cluster bind hits EADDRINUSE and bun
    // exits). The helper's second attempt reserves freely and must boot.
    let root = tempfile::tempdir().unwrap();
    let hostage = TcpListener::bind("127.0.0.1:0").unwrap();
    let stolen_raft = hostage.local_addr().unwrap().port();
    let mut attempt = 0;
    let (mut bun, address) = spawn_bun_with_port_retry(true, || {
        attempt += 1;
        let config = if attempt == 1 {
            let [gossip, _, reporting] = reserve_ports();
            write_portable_node_config_with_ports(root.path(), [gossip, stolen_raft, reporting])
        } else {
            write_portable_node_config(root.path())
        };
        (
            config,
            reserve_address(),
            root.path().join(format!("race-bun-{attempt}.log")),
        )
    });

    bun.assert_running();
    assert!(TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok());
    let first_log = std::fs::read_to_string(root.path().join("race-bun-1.log")).unwrap();
    assert!(
        first_log.contains("Address already in use"),
        "the first attempt should have lost its Raft bind:\n{first_log}"
    );
}

#[test]
fn secure_cluster_first_run_initialises_authenticates_and_deploys() {
    let root = tempfile::tempdir().unwrap();
    let cluster_dir = root.path().join("cluster");
    let init = run_relish(&[
        "init",
        cluster_dir.to_str().unwrap(),
        "--cluster-name",
        "first-run",
        "--node-id",
        "node-01",
    ]);
    assert_success(&init, "documented cluster init");

    let node_path = cluster_dir.join("reliaburger.toml");
    let mut node = reliaburger::config::NodeConfig::from_file(&node_path).unwrap();
    node.node.name = Some("node-01".to_string());
    node.network.advertise_address = Some("127.0.0.1".to_string());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;

    let (mut bun, address) = spawn_bun_with_port_retry(true, || {
        let [gossip_port, raft_port, reporting_port] = reserve_ports();
        node.cluster.gossip_port = gossip_port;
        node.cluster.raft_port = raft_port;
        node.cluster.reporting_port = reporting_port;
        std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();
        (
            node_path.clone(),
            "127.0.0.1:0".parse().unwrap(),
            root.path().join("cluster-bun.log"),
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
        "first-admin",
        "--role",
        "admin",
    ]);
    assert_success(&token, "documented first-admin token creation");
    let token = String::from_utf8(token.stdout).unwrap();
    let token = token.trim();
    assert!(
        token.starts_with("rbrg_"),
        "unexpected token output: {token}"
    );

    let generated_app = cluster_dir.join("app.toml");
    let dry_run = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "--token",
        token,
        "apply",
        generated_app.to_str().unwrap(),
        "--dry-run",
    ]);
    assert_success(&dry_run, "generated container app dry-run");

    let process_app = root.path().join("cluster-process-app.toml");
    std::fs::write(
        &process_app,
        format!(
            r#"
[app.cluster-hello]
image = "proc-grill:image-ignored"
command = [{testapp:?}, "--port", "0"]
"#,
            testapp = env!("CARGO_BIN_EXE_testapp"),
        ),
    )
    .unwrap();
    let apply = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "--token",
        token,
        "apply",
        process_app.to_str().unwrap(),
    ]);
    assert_success(&apply, "authenticated clustered apply");

    let status = wait_for_relish_output(
        &mut bun,
        &[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            token,
            "--output",
            "json",
            "status",
        ],
        "\"state\": \"running\"",
    );
    let rows: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|row| { row["app_name"] == "cluster-hello" && row["state"] == "running" })
    );
}

#[test]
fn post_bootstrap_join_tokens_enrol_two_distinct_nodes_and_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let cluster_dir = root.path().join("cluster");
    let init = run_relish(&[
        "init",
        cluster_dir.to_str().unwrap(),
        "--cluster-name",
        "join-token-test",
        "--node-id",
        "node-01",
    ]);
    assert_success(&init, "cluster init for join-token test");

    let node_path = cluster_dir.join("reliaburger.toml");
    let mut node = reliaburger::config::NodeConfig::from_file(&node_path).unwrap();
    node.node.name = Some("node-01".to_string());
    node.network.advertise_address = Some("127.0.0.1".to_string());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;

    let (mut bun, address) = spawn_bun_with_port_retry(true, || {
        let [gossip_port, raft_port, reporting_port] = reserve_ports();
        node.cluster.gossip_port = gossip_port;
        node.cluster.raft_port = raft_port;
        node.cluster.reporting_port = reporting_port;
        std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();
        (
            node_path.clone(),
            reserve_address(),
            root.path().join("join-token-bun.log"),
        )
    });
    // The joiners below seed off whichever gossip port the surviving
    // attempt actually bound.
    let gossip_port = node.cluster.gossip_port;
    let endpoint = format!("https://{address}");
    let ca_path = cluster_dir.join("identity/root-ca.crt");
    let ca = ca_path.to_str().unwrap();
    wait_for_relish(
        &mut bun,
        &["--endpoint", &endpoint, "--ca-cert", ca, "status"],
    );

    let admin = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "token",
        "create",
        "--name",
        "join-admin",
        "--role",
        "admin",
    ]);
    assert_success(&admin, "create join-token administrator");
    let admin = String::from_utf8(admin.stdout).unwrap();
    let admin = admin.trim();

    let deployer = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "--token",
        admin,
        "token",
        "create",
        "--name",
        "not-an-admin",
        "--role",
        "deployer",
    ]);
    assert_success(&deployer, "create non-admin bearer");
    let deployer = String::from_utf8(deployer.stdout).unwrap();
    let deployer = deployer.trim();

    // Bun refreshes the middleware's replicated token snapshot on a short
    // interval. Wait on a read-only Admin route so we don't mint anything in
    // the loopback bootstrap window while that first snapshot catches up.
    let auth_deadline = Instant::now() + WAIT;
    loop {
        let probe = run_relish(&[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            deployer,
            "token",
            "list",
        ]);
        if probe.status.code() == Some(1) && String::from_utf8_lossy(&probe.stderr).contains("403")
        {
            break;
        }
        assert!(
            Instant::now() < auth_deadline,
            "the replicated token store never enforced the Deployer role"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let denied = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "--token",
        deployer,
        "join-token",
        "create",
        "--node-id",
        "node-02",
    ]);
    assert_eq!(denied.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&denied.stderr).contains("403"));

    let mut issued = Vec::new();
    for index in 0..2 {
        let node_id = format!("node-{:02}", index + 2);
        let output = run_relish(&[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            admin,
            "join-token",
            "create",
            "--node-id",
            &node_id,
            "--ttl",
            "15m",
        ]);
        assert_success(&output, "mint post-bootstrap join token");
        issued.push(String::from_utf8(output.stdout).unwrap().trim().to_string());
    }
    assert_ne!(issued[0], issued[1]);

    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let root_ca = std::fs::read(&ca_path).unwrap();
    let root_ca_der = CertificateDer::pem_slice_iter(&root_ca)
        .next()
        .unwrap()
        .unwrap();
    let fingerprint = reliaburger::sesame::identity_store::root_ca_fingerprint(&root_ca_der);

    for (index, token) in issued.iter().enumerate() {
        let node_id = format!("node-{:02}", index + 2);
        let identity_dir = root.path().join(&node_id);
        let join = run_relish(&[
            "join",
            "--token",
            token,
            "--node-id",
            &node_id,
            "--identity-dir",
            identity_dir.to_str().unwrap(),
            "--ca-fingerprint",
            &fingerprint,
            &endpoint,
        ]);
        assert_success(&join, "enrol CSR-bearing node");
        let identity = reliaburger::sesame::identity_store::load(&identity_dir)
            .unwrap()
            .unwrap();
        assert_eq!(identity.node_id, node_id);
    }

    // issued[0] is bound to node-02 and was already consumed by it, so
    // re-enrolling node-02 is refused as consumed (single-use).
    let reused = run_relish(&[
        "join",
        "--token",
        &issued[0],
        "--node-id",
        "node-02",
        "--identity-dir",
        root.path().join("node-02-reuse").to_str().unwrap(),
        &endpoint,
    ]);
    assert_eq!(reused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&reused.stderr).contains("consumed"));

    let expiring = run_relish(&[
        "--endpoint",
        &endpoint,
        "--ca-cert",
        ca,
        "--token",
        admin,
        "join-token",
        "create",
        "--node-id",
        "node-05",
        "--ttl",
        "1s",
    ]);
    assert_success(&expiring, "mint expiring join token");
    let expiring = String::from_utf8(expiring.stdout).unwrap();
    std::thread::sleep(Duration::from_millis(1_100));
    let expired = run_relish(&[
        "join",
        "--token",
        expiring.trim(),
        "--node-id",
        "node-05",
        "--identity-dir",
        root.path().join("node-05").to_str().unwrap(),
        &endpoint,
    ]);
    assert_eq!(expired.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&expired.stderr).contains("expired"));

    // Provision the two enrolled identities as real Bun nodes. Each joiner
    // gets its own ports and state paths, shares the cluster wrapping key,
    // and discovers node-01 through its gossip address.
    let mut joiners = Vec::new();
    for node_id in ["node-02", "node-03"] {
        let mut joiner = node.clone();
        joiner.node.name = Some(node_id.to_string());
        joiner.cluster.join = vec![format!("127.0.0.1:{gossip_port}")];
        joiner.security.bootstrap_path = None;
        joiner.security.identity_dir = Some(root.path().join(node_id));
        let state = root.path().join(format!("state-{node_id}"));
        joiner.storage.data = state.join("data");
        joiner.storage.images = state.join("images");
        joiner.storage.logs = state.join("logs");
        joiner.storage.metrics = state.join("metrics");
        joiner.storage.volumes = state.join("volumes");
        let config = root.path().join(format!("{node_id}.toml"));
        let (joiner_bun, _api) = spawn_bun_with_port_retry(true, || {
            let [joiner_gossip, joiner_raft, joiner_reporting] = reserve_ports();
            joiner.cluster.gossip_port = joiner_gossip;
            joiner.cluster.raft_port = joiner_raft;
            joiner.cluster.reporting_port = joiner_reporting;
            std::fs::write(&config, toml::to_string_pretty(&joiner).unwrap()).unwrap();
            (
                config.clone(),
                reserve_address(),
                root.path().join(format!("{node_id}.log")),
            )
        });
        joiners.push(joiner_bun);
    }

    let convergence_deadline = Instant::now() + WAIT;
    loop {
        bun.assert_running();
        for joiner in &mut joiners {
            joiner.assert_running();
        }
        let nodes = run_relish(&[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            admin,
            "nodes",
        ]);
        let council = run_relish(&[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            admin,
            "council",
        ]);
        let nodes_out = String::from_utf8_lossy(&nodes.stdout);
        let council_out = String::from_utf8_lossy(&council.stdout);
        if nodes.status.success()
            && council.status.success()
            && ["node-01", "node-02", "node-03"]
                .iter()
                .all(|name| nodes_out.contains(name) && council_out.contains(name))
        {
            break;
        }
        assert!(
            Instant::now() < convergence_deadline,
            "enrolled nodes did not converge into a three-voter council\nnodes={nodes_out}\ncouncil={council_out}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn published_first_run_snippets_do_not_drift() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let documents = [
        root.join("README.md"),
        root.join("docs/README.md"),
        root.join("docs/whitepaper.md"),
        root.join("docs/design/cli-relish.md"),
        root.join("docs/book/02-finding-friends.md"),
    ];
    for document in documents {
        let text = std::fs::read_to_string(&document).unwrap();
        assert!(
            text.contains("relish apply"),
            "{} no longer shows how to apply a manifest",
            document.display()
        );
        // A published join command must show the required node id. The
        // top-level README no longer carries the enrolment walkthrough (it
        // moved to docs/README.md and the manual), so the guard applies
        // wherever the command itself is published.
        if text.contains("relish join") {
            assert!(
                text.contains("--node-id"),
                "{} publishes a join command without the required node id",
                document.display()
            );
        }
    }

    let whitepaper = std::fs::read_to_string(root.join("docs/whitepaper.md")).unwrap();
    assert!(!whitepaper.contains("✓ Started Bun agent"));
    assert!(!whitepaper.contains("Dashboard: https://10.0.1.5:9443"));
    assert!(whitepaper.contains("127.0.0.1:9117"));

    let security_book = std::fs::read_to_string(root.join("docs/book/04-trust-no-one.md")).unwrap();
    assert!(security_book.contains("--node-id node-02"));
    assert!(security_book.contains("https://10.0.1.5:9117"));
    assert!(security_book.contains("join-token create --node-id node-02 --ttl 15m"));
    assert!(!security_book.contains("Right now, you\ncan't"));
    assert!(!security_book.contains("relish join --token rbrg_join_1_a7f3b9c2... 10.0.1.5:9443"));
}

#[test]
fn published_apply_and_join_shapes_reach_their_handlers() {
    let root = tempfile::tempdir().unwrap();
    let app = root.path().join("app.toml");
    std::fs::write(
        &app,
        r#"
[app.hello]
image = "proc-grill:image-ignored"
command = ["true"]
"#,
    )
    .unwrap();

    let apply = run_relish(&["apply", app.to_str().unwrap(), "--dry-run"]);
    assert_success(&apply, "positional apply path");
    // Z1.4: `-f` is the kubectl spelling, and it takes Kubernetes YAML too.
    let file_apply = run_relish(&["apply", "-f", app.to_str().unwrap(), "--dry-run"]);
    assert_success(&file_apply, "apply -f path");

    // Port 1 refuses immediately. Exit 1 proves clap accepted the complete
    // documented shape and dispatched the join handler; a syntax error is 2.
    let join = run_relish(&[
        "join",
        "--token",
        "documentation-only-token",
        "--node-id",
        "node-02",
        "https://127.0.0.1:1",
    ]);
    assert_eq!(join.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&join.stderr).contains("join failed"));
}
