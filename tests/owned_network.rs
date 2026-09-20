//! Actual namespace cleanup must fence a delayed mutator whose Bun caller died.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use reliaburger::grill::command::{ClaimedCommandExecutor, CommandOutput, RuntimeCommandExecutor};
use reliaburger::grill::netns;
use reliaburger::grill::runc_intent::{IntentConfiguration, IntentJournal, IntentPhase};
use reliaburger::grill::{InstanceId, OciSpec};

const NODE_INDEX: u16 = 32_766;
const CONTAINER_INDEX: u16 = 500;

fn journal(root: &Path) -> IntentJournal {
    IntentJournal::new(
        root.join("intents"),
        IntentConfiguration {
            bundle_directory: root.join("bundles"),
            state_directory: root.join("state"),
            image_directory: root.join("images"),
            runc_program: "runc".into(),
            rootless: false,
            dns_nameserver: None,
            node_index: NODE_INDEX,
        },
    )
}

async fn create_executor(root: &Path, id: &InstanceId) -> ClaimedCommandExecutor {
    let spec: OciSpec = serde_json::from_value(serde_json::json!({
        "root": {"path": "/", "readonly": true},
        "process": {"args": ["original"], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
        "mounts": [], "linux": {"namespaces": []}
    }))
    .unwrap();
    let claim = journal(root)
        .claim(id, None)
        .await
        .unwrap()
        .publish(&spec)
        .await
        .unwrap();
    ClaimedCommandExecutor::new(
        claim
            .supervise_commands(env!("CARGO_BIN_EXE_bun").into())
            .unwrap(),
    )
}

#[tokio::test]
#[ignore = "requires root, ip and nft; mutates isolated owned namespaces"]
async fn owned_network_setup_and_forwarding_retire_through_the_generation() {
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let id = InstanceId(format!("rbtest-owned-net-{}", std::process::id()));
    let executor = create_executor(root.path(), &id).await;
    let network = netns::setup_container_network_with_commands(
        &executor,
        &id,
        NODE_INDEX,
        CONTAINER_INDEX,
        false,
    )
    .await
    .unwrap();
    assert!(network.namespace_path.exists());
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let _publication = netns::add_port_mapping_with_commands(&executor, &network, port, 8080)
        .await
        .unwrap();
    let cleanup = executor.seal(Duration::from_secs(30)).await.unwrap();
    netns::retire_address_forwarding_with_commands(&cleanup, network.container_ip)
        .await
        .unwrap();
    netns::teardown_container_network_with_commands(&cleanup, &network)
        .await
        .unwrap();
    cleanup.finish(None).await.unwrap();
    assert!(!network.namespace_path.exists());
    assert!(!Path::new("/sys/class/net").join(network.host_veth).exists());
    assert_eq!(
        journal(root.path()).inventory().await.unwrap()[0].phase,
        IntentPhase::Retired { exit_code: None }
    );
}

#[tokio::test]
#[ignore = "requires root and ip; SIGKILL during real namespace preparation"]
async fn killed_network_caller_cannot_publish_a_veth_after_recovered_retirement() {
    assert!(nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap();
    let id = InstanceId(format!("rbtest-killed-net-{}", std::process::id()));
    let planned = netns::planned_container_network(&id, NODE_INDEX, CONTAINER_INDEX).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "owned_network_fixture",
            "--nocapture",
        ])
        .env("RELIABURGER_NETWORK_FIXTURE", root.path())
        .env("RELIABURGER_NETWORK_ID", &id.0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        while !root.path().join("ready").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "fixture exited before the delayed mutation"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(planned.namespace_path.exists());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    let journal = journal(root.path());
    let claim = journal
        .claim(&id, journal.observe(&id).await.unwrap())
        .await
        .unwrap();
    let executor = ClaimedCommandExecutor::new(
        claim
            .supervise_commands(env!("CARGO_BIN_EXE_bun").into())
            .unwrap(),
    );
    let cleanup = executor.seal(Duration::from_secs(15)).await.unwrap();
    netns::teardown_container_network_with_commands(&cleanup, &planned)
        .await
        .unwrap();
    cleanup.finish(None).await.unwrap();
    std::fs::write(root.path().join("release"), "continue").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!planned.namespace_path.exists());
    assert!(!Path::new("/sys/class/net").join(planned.host_veth).exists());
    assert_eq!(
        journal.inventory().await.unwrap()[0].phase,
        IntentPhase::Retired { exit_code: None }
    );
}

struct DelayedVeth {
    executor: ClaimedCommandExecutor,
    root: std::path::PathBuf,
}

impl RuntimeCommandExecutor for DelayedVeth {
    async fn output(&self, program: &str, arguments: &[&str]) -> std::io::Result<CommandOutput> {
        if program == "ip" && arguments.starts_with(&["link", "add"]) {
            let ready = self.root.join("ready");
            let release = self.root.join("release");
            let mut gated = vec![
                "-c",
                "ready=$1; release=$2; shift 2; touch \"$ready\"; attempts=0; while [ ! -f \"$release\" ]; do attempts=$((attempts+1)); [ \"$attempts\" -le 1000 ] || exit 1; sleep 0.02; done; exec ip \"$@\"",
                "fixture",
                ready.to_str().unwrap(),
                release.to_str().unwrap(),
            ];
            gated.extend_from_slice(arguments);
            self.executor.output("/bin/sh", &gated).await
        } else {
            self.executor.output(program, arguments).await
        }
    }
}

#[tokio::test]
#[ignore = "subprocess fixture for owned namespace crash recovery"]
async fn owned_network_fixture() {
    let Some(root) = std::env::var_os("RELIABURGER_NETWORK_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let id = InstanceId(std::env::var("RELIABURGER_NETWORK_ID").unwrap());
    let executor = DelayedVeth {
        executor: create_executor(&root, &id).await,
        root,
    };
    netns::setup_container_network_with_commands(
        &executor,
        &id,
        NODE_INDEX,
        CONTAINER_INDEX,
        false,
    )
    .await
    .unwrap();
}

struct UncertainDeletion {
    added: std::sync::atomic::AtomicBool,
}

impl RuntimeCommandExecutor for UncertainDeletion {
    async fn output(&self, program: &str, arguments: &[&str]) -> std::io::Result<CommandOutput> {
        if program == "ip" || arguments.starts_with(&["delete", "element"]) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "old deletion may still execute",
            ));
        }
        if arguments.starts_with(&["add", "element"]) {
            self.added.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout: b"masquerade @portmap".to_vec(),
            stderr: Vec::new(),
        })
    }
}

#[tokio::test]
async fn port_publication_refuses_an_uncertain_previous_deletion() {
    let executor = UncertainDeletion {
        added: std::sync::atomic::AtomicBool::new(false),
    };
    let network =
        netns::planned_container_network(&InstanceId("no-kernel-effects".into()), 1, 1).unwrap();
    let result = netns::add_port_mapping_with_commands(&executor, &network, 12000, 8080).await;
    assert!(
        result.is_err(),
        "unknown deletion was mistaken for a completed absent mapping"
    );
    assert!(!executor.added.load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test]
async fn teardown_refuses_unknown_deletion_even_when_resources_currently_appear_absent() {
    let executor = UncertainDeletion {
        added: std::sync::atomic::AtomicBool::new(false),
    };
    let network =
        netns::planned_container_network(&InstanceId("no-kernel-effects".into()), 1, 1).unwrap();
    assert!(
        netns::teardown_container_network_with_commands(&executor, &network)
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires root, ip and ping; creates two isolated container networks"]
async fn same_node_containers_have_independent_host_and_peer_routes() {
    assert!(nix::unistd::geteuid().is_root());
    let mut networks = Vec::new();
    let exercise = async {
        for index in 0..2 {
            let id = InstanceId(format!("rbtest-peer-route-{}-{index}", std::process::id()));
            networks.push(netns::setup_container_network(&id, NODE_INDEX - 1, index, false).await?);
        }
        for source in 0..2 {
            let destination = networks[1 - source].container_ip.to_string();
            let namespace = networks[source]
                .namespace_path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap();
            for (label, args) in [
                (
                    "host",
                    vec!["ping", "-c", "1", "-W", "1", destination.as_str()],
                ),
                (
                    "peer",
                    vec![
                        "ip",
                        "netns",
                        "exec",
                        namespace,
                        "ping",
                        "-c",
                        "1",
                        "-W",
                        "1",
                        destination.as_str(),
                    ],
                ),
            ] {
                let output = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::process::Command::new(args[0])
                        .args(&args[1..])
                        .kill_on_drop(true)
                        .output(),
                )
                .await??;
                anyhow::ensure!(
                    output.status.success(),
                    "{label} cannot reach {destination}: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    for network in networks.iter().rev() {
        netns::teardown_container_network(network).await.unwrap();
    }
    exercise.unwrap();
}
