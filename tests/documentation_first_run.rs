//! Executable contracts for the two first-run paths published in the docs.
//!
//! These are black-box tests on purpose. A parser unit test can prove that a
//! flag exists, but it cannot prove that the documented Bun and Relish
//! processes can actually talk to each other.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(30);

struct BunProcess {
    child: Child,
    log_path: PathBuf,
}

impl BunProcess {
    fn spawn(config: &Path, address: SocketAddr, clustered: bool, log_path: PathBuf) -> Self {
        let log = std::fs::File::create(&log_path).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bun"));
        command
            .arg("--config")
            .arg(config)
            .arg("--listen")
            .arg(address.to_string())
            .arg("--runtime")
            .arg("process")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        if clustered {
            command.arg("--cluster");
        }
        Self {
            child: command.spawn().unwrap(),
            log_path,
        }
    }

    fn assert_running(&mut self) {
        if let Some(status) = self.child.try_wait().unwrap() {
            let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
            panic!("bun exited before the first-run command ({status}):\n{log}");
        }
    }
}

impl Drop for BunProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        #[cfg(unix)]
        {
            let pid = nix::unistd::Pid::from_raw(self.child.id() as i32);
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn reserve_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

fn reserve_ports() -> [u16; 3] {
    [
        reserve_address().port(),
        reserve_address().port(),
        reserve_address().port(),
    ]
}

fn write_portable_node_config(root: &Path) -> PathBuf {
    let config = root.join("node.toml");
    let [gossip_port, raft_port, reporting_port] = reserve_ports();
    std::fs::write(
        &config,
        format!(
            r#"
[node]
name = "first-run-{gossip_port}"

[cluster]
gossip_port = {gossip_port}
raft_port = {raft_port}
reporting_port = {reporting_port}

[network]
advertise_address = "127.0.0.1"

[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"

[images]
registry_bind = "127.0.0.1"
registry_port = 0
"#,
            root = root.display(),
        ),
    )
    .unwrap();
    config
}

fn run_relish(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(args)
        .env_remove("RELIABURGER_ENDPOINT")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap()
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_relish(bun: &mut BunProcess, args: &[&str]) -> Output {
    let deadline = Instant::now() + WAIT;
    loop {
        bun.assert_running();
        let output = run_relish(args);
        if output.status.success() {
            return output;
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(&bun.log_path).unwrap_or_default();
            panic!(
                "relish never reached bun\nstdout={}\nstderr={}\nbun log={log}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_relish_output(bun: &mut BunProcess, args: &[&str], expected: &str) -> Output {
    let deadline = Instant::now() + WAIT;
    loop {
        bun.assert_running();
        let output = run_relish(args);
        if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(expected) {
            return output;
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(&bun.log_path).unwrap_or_default();
            panic!(
                "relish output never contained {expected:?}\nstdout={}\nstderr={}\nbun log={log}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_tcp(bun: &mut BunProcess, address: SocketAddr) {
    let deadline = Instant::now() + WAIT;
    loop {
        bun.assert_running();
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "bun never listened on {address}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn standalone_first_run_reaches_a_running_workload() {
    let root = tempfile::tempdir().unwrap();
    let address = reserve_address();
    let config = write_portable_node_config(root.path());
    let mut bun = BunProcess::spawn(&config, address, false, root.path().join("bun.log"));
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
    let [gossip_port, raft_port, reporting_port] = reserve_ports();
    node.node.name = Some("node-01".to_string());
    node.cluster.gossip_port = gossip_port;
    node.cluster.raft_port = raft_port;
    node.cluster.reporting_port = reporting_port;
    node.network.advertise_address = Some("127.0.0.1".to_string());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;
    std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();

    let address = reserve_address();
    let mut bun = BunProcess::spawn(
        &node_path,
        address,
        true,
        root.path().join("cluster-bun.log"),
    );
    wait_for_tcp(&mut bun, address);

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

    wait_for_relish_output(
        &mut bun,
        &[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            token,
            "status",
        ],
        "cluster-hello",
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
    let [gossip_port, raft_port, reporting_port] = reserve_ports();
    node.node.name = Some("node-01".to_string());
    node.cluster.gossip_port = gossip_port;
    node.cluster.raft_port = raft_port;
    node.cluster.reporting_port = reporting_port;
    node.network.advertise_address = Some("127.0.0.1".to_string());
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;
    std::fs::write(&node_path, toml::to_string_pretty(&node).unwrap()).unwrap();

    let address = reserve_address();
    let mut bun = BunProcess::spawn(
        &node_path,
        address,
        true,
        root.path().join("join-token-bun.log"),
    );
    wait_for_tcp(&mut bun, address);
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
    ]);
    assert_eq!(denied.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&denied.stderr).contains("403"));

    let mut issued = Vec::new();
    for _ in 0..2 {
        let output = run_relish(&[
            "--endpoint",
            &endpoint,
            "--ca-cert",
            ca,
            "--token",
            admin,
            "join-token",
            "create",
            "--ttl",
            "15m",
        ]);
        assert_success(&output, "mint post-bootstrap join token");
        issued.push(String::from_utf8(output.stdout).unwrap().trim().to_string());
    }
    assert_ne!(issued[0], issued[1]);

    let root_ca = std::fs::read(&ca_path).unwrap();
    let root_ca_der = rustls_pemfile::certs(&mut root_ca.as_slice())
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

    let reused = run_relish(&[
        "join",
        "--token",
        &issued[0],
        "--node-id",
        "node-04",
        "--identity-dir",
        root.path().join("node-04").to_str().unwrap(),
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
    for (index, node_id) in ["node-02", "node-03"].iter().enumerate() {
        let mut joiner = node.clone();
        let [joiner_gossip, joiner_raft, joiner_reporting] = reserve_ports();
        joiner.node.name = Some((*node_id).to_string());
        joiner.cluster.join = vec![format!("127.0.0.1:{gossip_port}")];
        joiner.cluster.gossip_port = joiner_gossip;
        joiner.cluster.raft_port = joiner_raft;
        joiner.cluster.reporting_port = joiner_reporting;
        joiner.security.bootstrap_path = None;
        joiner.security.identity_dir = Some(root.path().join(node_id));
        let state = root.path().join(format!("state-{node_id}"));
        joiner.storage.data = state.join("data");
        joiner.storage.images = state.join("images");
        joiner.storage.logs = state.join("logs");
        joiner.storage.metrics = state.join("metrics");
        joiner.storage.volumes = state.join("volumes");
        let config = root.path().join(format!("{node_id}.toml"));
        std::fs::write(&config, toml::to_string_pretty(&joiner).unwrap()).unwrap();
        let api = reserve_address();
        joiners.push(BunProcess::spawn(
            &config,
            api,
            true,
            root.path().join(format!("{node_id}.log")),
        ));
        wait_for_tcp(&mut joiners[index], api);
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
            text.contains("relish apply") && !text.contains("relish apply -f"),
            "{} publishes the wrong apply syntax",
            document.display()
        );
        assert!(
            text.contains("--node-id"),
            "{} omits the required join/init node id",
            document.display()
        );
    }

    let whitepaper = std::fs::read_to_string(root.join("docs/whitepaper.md")).unwrap();
    assert!(!whitepaper.contains("✓ Started Bun agent"));
    assert!(!whitepaper.contains("Dashboard: https://10.0.1.5:9443"));
    assert!(whitepaper.contains("127.0.0.1:9117"));

    let security_book = std::fs::read_to_string(root.join("docs/book/04-trust-no-one.md")).unwrap();
    assert!(security_book.contains("--node-id node-02"));
    assert!(security_book.contains("https://10.0.1.5:9117"));
    assert!(security_book.contains("join-token create --ttl 15m"));
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
    let stale_apply = run_relish(&["apply", "-f", app.to_str().unwrap()]);
    assert_eq!(
        stale_apply.status.code(),
        Some(2),
        "the old -f shape should remain a visible clap error"
    );

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
