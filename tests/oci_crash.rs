//! Actual Bun death around opt-in owned OCI operations, using real Runc.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use reliaburger::config::Config;
use reliaburger::grill::{ContainerState, Grill, ImageStore, runc::RuncGrill};
use reliaburger::relish::client::BunClient;

struct Node {
    child: tokio::process::Child,
    client: BunClient,
}

impl Node {
    async fn start(root: &Path) -> Self {
        Self::start_with_restarts(root, 0).await
    }

    async fn start_with_restarts(root: &Path, mut remaining: usize) -> Self {
        let log = root.join("bun.log");
        let offset = std::fs::metadata(&log).map_or(0, |m| m.len()) as usize;
        let output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        let executable = if root.join("upgrade-bin/bun").exists() {
            root.join("upgrade-bin/bun")
        } else {
            env!("CARGO_BIN_EXE_bun").into()
        };
        let listen =
            std::fs::read_to_string(root.join("listen")).unwrap_or_else(|_| "127.0.0.1:0".into());
        let mut command = tokio::process::Command::new(executable);
        if root.join("cluster").exists() {
            command.arg("--cluster");
        }
        if !root.join("production").exists() {
            command.arg("--experimental-owned-runc");
        }
        if let Ok(cgroup) = std::fs::read_to_string(root.join("service-cgroup")) {
            let procs = std::path::PathBuf::from(cgroup.trim()).join("cgroup.procs");
            // SAFETY: the closure runs in the forked child before exec and only
            // performs open/write/close syscalls through std's File API on a
            // path allocated before the fork; it takes no locks.
            unsafe {
                command.pre_exec(move || std::fs::write(&procs, "0"));
            }
        }
        let mut child = command
            .arg("--config")
            .arg(root.join("node.toml"))
            .args(["--listen", &listen, "--runtime", "runc"])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("OCI_CRASH_ROOT", root)
            .env("LD_PRELOAD", root.join("admission.so"))
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let client = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let contents = std::fs::read_to_string(&log).unwrap();
                if let Some(address) = contents[offset..]
                    .lines()
                    .find_map(|line| line.strip_prefix("bun: API server listening on "))
                {
                    let client = if root.join("cluster").exists() {
                        let secret =
                            std::fs::read_to_string(root.join("activation-master.key")).unwrap();
                        let token = reliaburger::sesame::token::derive_service_token(
                            &hex::decode(secret.trim()).unwrap().try_into().unwrap(),
                        )
                        .unwrap();
                        let token =
                            std::fs::read_to_string(root.join("operator-token")).unwrap_or(token);
                        BunClient::new_with_ca(
                            &format!("https://{address}"),
                            Some(&token),
                            &std::fs::read(root.join("identity/root-ca.crt")).unwrap(),
                        )
                        .unwrap()
                    } else {
                        BunClient::new(&format!("http://{address}"))
                    };
                    if client.health().await.is_ok() {
                        break client;
                    }
                }
                if child.try_wait().unwrap().is_some() {
                    assert!(remaining > 0, "Bun exited: {contents}");
                    remaining -= 1;
                    child = command.spawn().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Bun recovery timed out: {}",
                std::fs::read_to_string(log).unwrap()
            )
        });
        Self { child, client }
    }

    async fn crash(&mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}

fn runtime(root: &Path) -> RuncGrill {
    let directory = root.join("data/instances/runc");
    RuncGrill::new(
        directory.join("bundles"),
        ImageStore::new(root.join("images")),
        false,
        directory.join("state"),
    )
    .with_owner(env!("CARGO_BIN_EXE_bun").into())
    .unwrap()
}

fn install_wrappers(root: &Path) {
    let compilation = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/oci_admission_gate.c"
        ))
        .arg("-o")
        .arg(root.join("admission.so"))
        .arg("-ldl")
        .output()
        .unwrap();
    assert!(
        compilation.status.success(),
        "{}",
        String::from_utf8_lossy(&compilation.stderr)
    );
    std::fs::create_dir(root.join("bin")).unwrap();
    std::fs::create_dir(root.join("shared")).unwrap();
    // OCI workloads run as the configured non-root user.
    std::fs::set_permissions(root.join("shared"), std::fs::Permissions::from_mode(0o777)).unwrap();
    for program in ["ip", "runc"] {
        let located = std::process::Command::new("sh")
            .args(["-c", &format!("command -v {program}")])
            .output()
            .unwrap();
        assert!(located.status.success());
        let real = String::from_utf8(located.stdout).unwrap();
        let script = format!(
            r#"#!/bin/bash
set -eu
root=$OCI_CRASH_ROOT
phase=$(cat "$root/phase")
arguments=" $* "
if [[ '{program}' == runc && "$arguments" == *' run '* ]]; then
    previous=''
    for argument in "$@"; do
        if [[ $previous == --bundle ]]; then bundle=$argument; fi
        previous=$argument
    done
    mkdir -p "$bundle/rootfs/bin" "$bundle/rootfs/work"
    cp /usr/bin/busybox "$bundle/rootfs/bin/busybox"
    python3 - "$bundle/config.json" "$root/shared" <<'PY'
import json, sys
path, shared = sys.argv[1:]
with open(path) as stream: spec = json.load(stream)
spec['mounts'].append({{'destination':'/work','source':shared,'type':'bind','options':['bind','rw']}})
with open(path, 'w') as stream: json.dump(spec, stream)
PY
fi
if [[ -f "$root/armed" && "$arguments" == *'__init-0'* ]]; then
    if [[ ( "$phase" == preparation || "$phase" == retry ) && '{program}' == ip && "$arguments" == *' netns add '* ]] ||
       [[ "$phase" == start && '{program}' == runc && "$arguments" == *' run '* ]] ||
       [[ "$phase" == retirement && '{program}' == ip && "$arguments" == *' netns del '* ]]; then
        touch "$root/ready"
        for ((i=0; i<3000; i++)); do
            [[ -f "$root/release" ]] && break
            sleep 0.02
        done
        [[ -f "$root/release" ]] || exit 90
    fi
fi
exec '{real}' "$@"
"#,
            real = real.trim()
        );
        let path = root.join("bin").join(program);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn manifest(running_init: bool, name: &str) -> Config {
    let init = if running_init {
        "printf 'init\\n' >> /work/initialisers; /bin/busybox touch /work/init-ready; exec /bin/busybox sleep 60"
    } else {
        "printf 'init\\n' >> /work/initialisers"
    };
    let mut config = Config::parse(
        r#"
[app.oci-crash]
image = "/empty-fixture"
command = ["/bin/busybox", "sh", "-c", "printf 'main\\n' >> /work/main; trap 'exit 0' TERM; while :; do /bin/busybox sleep 1; done"]
[[app.oci-crash.init]]
command = ["/bin/busybox", "true"]
[[app.oci-crash.init]]
command = ["/bin/busybox", "sh", "-c", "printf 'second\\n' >> /work/second"]
"#,
    )
    .unwrap();
    config.app.get_mut("oci-crash").unwrap().init[0].command =
        vec!["/bin/busybox".into(), "sh".into(), "-c".into(), init.into()];
    let app = config.app.remove("oci-crash").unwrap();
    config.app.insert(name.to_owned(), app);
    config
}

async fn wait_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(25), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture never reached {}", path.display()));
}

#[tokio::test]
#[ignore = "requires isolated Linux root, real runc/ip/nft and static BusyBox"]
async fn actual_bun_sigkill_and_cancelled_caller_preserve_oci_init_and_retry_ownership() {
    assert!(nix::unistd::geteuid().is_root());
    for phase in [
        "admission",
        "preparation",
        "start",
        "running",
        "retirement",
        "adopted",
        "retry",
    ] {
        let root = tempfile::tempdir().unwrap().keep();
        println!("qualifying {phase}: {}", root.as_path().display());
        let name = format!(
            "oci-crash-{}",
            root.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .trim_start_matches('.')
                .to_ascii_lowercase()
        );
        install_wrappers(root.as_path());
        std::fs::write(root.as_path().join("phase"), phase).unwrap();
        std::fs::write(root.as_path().join("armed"), "armed").unwrap();
        std::fs::write(
            root.as_path().join("node.toml"),
            format!(
                r#"
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
                root = root.as_path().display()
            ),
        )
        .unwrap();
        let mut node = Node::start(root.as_path()).await;
        if phase == "retry" {
            std::fs::remove_file(root.join("armed")).unwrap();
            node.client.apply(&manifest(false, &name)).await.unwrap();
            node.client.stop(&name, "default").await.unwrap();
            for file in ["main", "second", "initialisers"] {
                std::fs::remove_file(root.join("shared").join(file)).unwrap();
            }
            std::fs::write(root.join("armed"), "armed").unwrap();
        }
        let client = node.client.clone();
        let config = manifest(phase == "running", &name);
        let mut request = tokio::spawn(async move { client.apply(&config).await });
        let marker = match phase {
            "running" => root.as_path().join("shared/init-ready"),
            "adopted" => root.as_path().join("shared/main"),
            _ => root.as_path().join("ready"),
        };
        let completed = tokio::select! {
            _ = wait_file(&marker) => false,
            result = &mut request => {
                result.unwrap().unwrap();
                wait_file(&marker).await;
                true
            }
        };
        if !completed {
            if phase == "adopted" {
                request.await.unwrap().unwrap();
            } else {
                request.abort();
                let _ = request.await;
            }
        }
        if phase == "admission" {
            let initialiser = format!("default__{name}-0__init-0");
            assert!(
                !root
                    .join("data/instances/runc/bundles/.intents/records")
                    .join(initialiser)
                    .exists(),
                "admission fixture ran after runtime intent publication"
            );
        }
        node.crash().await;
        std::fs::remove_file(root.as_path().join("armed")).unwrap();
        let mut recovered = Node::start(root.as_path()).await;
        let inventory = runtime(root.as_path())
            .launch_inventory()
            .await
            .unwrap()
            .unwrap();
        if phase == "adopted" {
            assert_eq!(
                std::fs::read_to_string(root.as_path().join("shared/main")).unwrap(),
                "main\n"
            );
            assert_eq!(recovered.client.status().await.unwrap().len(), 1);
            recovered.client.stop(&name, "default").await.unwrap();
        } else {
            for launch in &inventory {
                assert_eq!(
                    runtime(root.as_path())
                        .state(&launch.instance_id)
                        .await
                        .unwrap(),
                    ContainerState::Stopped,
                    "unadopted {phase} execution survived recovery"
                );
            }
            assert!(!root.as_path().join("shared/second").exists());
            assert!(!root.as_path().join("shared/main").exists());
            assert!(recovered.client.status().await.unwrap().is_empty());
        }
        // A delayed pre-crash command must not execute after retirement.
        std::fs::write(root.as_path().join("release"), "release").unwrap();
        recovered
            .client
            .apply(&manifest(false, &name))
            .await
            .unwrap();
        wait_file(&root.as_path().join("shared/main")).await;
        let expected = if phase == "adopted" {
            "main\nmain\n"
        } else {
            "main\n"
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            while std::fs::read_to_string(root.join("shared/main")).unwrap() != expected {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("explicit retry must execute the main payload exactly once");
        recovered.client.stop(&name, "default").await.unwrap();
        recovered.crash().await;
        println!(
            "qualified {phase}: interrupted request, actual Bun death, recovery and explicit retry"
        );
    }
}

#[cfg(feature = "ebpf")]
fn durable_fixture(root: &Path) {
    install_wrappers(root);
    std::fs::write(root.join("production"), "normal startup").unwrap();
    std::fs::write(root.join("phase"), "none").unwrap();
    std::fs::write(
        root.join("node.toml"),
        format!(
            r#"
[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"
[images]
registry_bind = "127.0.0.1"
registry_port = 0
[ebpf]
enabled = true
"#,
            root = root.display()
        ),
    )
    .unwrap();
}

#[cfg(feature = "ebpf")]
fn durable_app(name: &str) -> Config {
    let mut config = manifest(false, name);
    config.app.get_mut(name).unwrap().port = Some(8080);
    config
}

#[cfg(feature = "ebpf")]
fn kernel_manifest(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(root.join("data/kernel-policy/owner.json")).unwrap())
        .unwrap()
}

#[cfg(feature = "ebpf")]
fn retire_kernel(root: &Path) {
    let owner = kernel_manifest(root);
    reliaburger::onion::ebpf::loader::OnionEbpf::retire_owned_state(
        Path::new(owner["cgroup_path"].as_str().unwrap()),
        &root.join("data/kernel-policy"),
        Path::new(owner["pin_directory"].as_str().unwrap()),
    )
    .unwrap();
    std::fs::remove_dir(owner["pin_directory"].as_str().unwrap()).unwrap();
}

#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "requires isolated Linux root, bpffs, real runc/ip/nft and static BusyBox"]
async fn normal_standalone_bun_recovers_durable_kernel_and_discovery() {
    let root = tempfile::tempdir().unwrap().keep();
    durable_fixture(&root);
    let mut node = Node::start(&root).await;
    let activated =
        root.join("data/kernel-policy/owner.json").exists() && root.join("data/discovery").is_dir();
    if !activated {
        node.crash().await;
    }
    assert!(
        activated,
        "normal standalone startup did not activate durable ownership"
    );
    let name = format!(
        "durable-{}",
        root.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches('.')
            .to_ascii_lowercase()
    );
    node.client.apply(&durable_app(&name)).await.unwrap();
    wait_file(&root.join("shared/main")).await;
    let original = kernel_manifest(&root);
    node.crash().await;
    let mut recovered = Node::start(&root).await;
    assert_eq!(kernel_manifest(&root), original);
    assert_eq!(recovered.client.status().await.unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(root.join("shared/main")).unwrap(),
        "main\n"
    );
    recovered.client.stop(&name, "default").await.unwrap();
    recovered.crash().await;
    let journal =
        reliaburger::bun::discovery_owners::DiscoveryJournal::open(&root.join("data/discovery"))
            .unwrap();
    assert!(journal.inventory().services.is_empty());
    drop(journal);
    let config = std::fs::read_to_string(root.join("node.toml")).unwrap();
    std::fs::write(
        root.join("node.toml"),
        config.replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    assert_startup_refused(&root, "refusing a mode change").await;
    std::fs::write(root.join("node.toml"), config).unwrap();
    let checkpoint = root.join("data/discovery/discovery.json");
    let saved = root.join("saved-discovery.json");
    std::fs::rename(&checkpoint, &saved).unwrap();
    assert_startup_refused(&root, "cannot recover durable discovery ownership").await;
    assert!(
        !checkpoint.exists(),
        "lost discovery evidence was silently recreated"
    );
    std::fs::rename(saved, checkpoint).unwrap();
    retire_kernel(&root);
}

/// systemd's `KillMode=mixed`/`control-group` stop: everything left in the
/// unit's cgroup dies at once, including detached owners and Runc launchers.
/// Containers survive because Runc gives them cgroups of their own.
#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "requires isolated Linux root, cgroup v2, bpffs, real runc/ip/nft and static BusyBox"]
async fn service_cgroup_kill_of_bun_and_owners_retires_the_launch_and_redeploys() {
    let root = tempfile::tempdir().unwrap().keep();
    durable_fixture(&root);
    let name = format!(
        "cgkill-{}",
        root.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches('.')
            .to_ascii_lowercase()
    );
    let cgroup = Path::new("/sys/fs/cgroup").join(format!("reliaburger-{name}"));
    std::fs::create_dir(&cgroup).unwrap();
    std::fs::write(root.join("service-cgroup"), cgroup.to_str().unwrap()).unwrap();
    let mut node = Node::start(&root).await;
    node.client.apply(&durable_app(&name)).await.unwrap();
    wait_file(&root.join("shared/main")).await;
    std::fs::write(cgroup.join("cgroup.kill"), "1").unwrap();
    node.child.wait().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !std::fs::read_to_string(cgroup.join("cgroup.procs"))
            .unwrap()
            .trim()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut recovered = Node::start(&root).await;
    // The launcher that supervised the container died with its owner, so Bun
    // can't adopt it. It must retire the launch and clean up rather than wedge
    // on the dead owners' records, and the same app must deploy again.
    assert!(recovered.client.status().await.unwrap().is_empty());
    std::fs::remove_file(root.join("shared/main")).unwrap();
    recovered.client.apply(&durable_app(&name)).await.unwrap();
    wait_file(&root.join("shared/main")).await;
    let running = recovered.client.status().await.unwrap();
    assert!(
        running
            .iter()
            .any(|instance| instance.app_name == name && instance.state == "running"),
        "{running:?}"
    );
    recovered.client.stop(&name, "default").await.unwrap();
    recovered.crash().await;
    std::fs::remove_dir(&cgroup).unwrap();
    retire_kernel(&root);
}

#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "requires isolated Linux root, bpffs, real runc/ip/nft and static BusyBox"]
async fn automatic_restart_bun_death_before_adoption_retires_the_unrecorded_successor() {
    let root = tempfile::tempdir().unwrap().keep();
    println!("qualifying automatic restart: {}", root.display());
    durable_fixture(&root);
    let name = "restart-crash";
    let mut config = durable_app(name);
    config.app.get_mut(name).unwrap().command = vec![
        "/bin/busybox".into(), "sh".into(), "-c".into(),
        "printf 'main\\n' >> /work/main; while [ ! -f /work/exit ]; do /bin/busybox sleep 0.1; done; /bin/busybox rm /work/exit; exit 7".into(),
    ];
    let mut node = Node::start(&root).await;
    node.client.apply(&config).await.unwrap();
    wait_file(&root.join("shared/main")).await;
    let record = root.join("data/instances/default__restart-crash-0.json");
    assert!(record.exists());
    let original_kernel = kernel_manifest(&root);
    let original = runtime(&root).launch_inventory().await.unwrap().unwrap();
    let id = reliaburger::grill::InstanceId("default__restart-crash-0".into());
    let original = original
        .iter()
        .find(|launch| launch.instance_id == id)
        .unwrap();
    std::fs::write(root.join("phase"), "restart-adoption").unwrap();
    std::fs::write(root.join("armed"), "armed").unwrap();
    std::fs::write(root.join("shared/exit"), "exit the actual application").unwrap();
    wait_file(&root.join("ready")).await;
    assert!(
        !record.exists(),
        "predecessor adoption survived into the successor"
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while std::fs::read_to_string(root.join("shared/main")).unwrap() != "main\nmain\n" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    node.crash().await;
    let interrupted = runtime(&root).launch_inventory().await.unwrap().unwrap();
    let interrupted = interrupted
        .iter()
        .find(|launch| launch.instance_id == id)
        .unwrap();
    assert_ne!(interrupted.generation, original.generation);
    assert_eq!(
        runtime(&root).state(&id).await.unwrap(),
        ContainerState::Running
    );
    std::fs::remove_file(root.join("armed")).unwrap();
    let mut recovered = Node::start(&root).await;
    assert!(recovered.client.status().await.unwrap().is_empty());
    assert_eq!(
        runtime(&root).state(&id).await.unwrap(),
        ContainerState::Stopped
    );
    assert!(!record.exists());
    assert_eq!(kernel_manifest(&root), original_kernel);
    recovered.client.apply(&config).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while std::fs::read_to_string(root.join("shared/main")).unwrap() != "main\nmain\nmain\n" {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    recovered.client.stop(name, "default").await.unwrap();
    recovered.crash().await;
    let journal =
        reliaburger::bun::discovery_owners::DiscoveryJournal::open(&root.join("data/discovery"))
            .unwrap();
    assert!(journal.inventory().services.is_empty());
    assert!(journal.inventory().references.is_empty());
    drop(journal);
    retire_kernel(&root);
}

#[cfg(feature = "ebpf")]
fn upgrade_fixture(root: &Path) -> Vec<u8> {
    use reliaburger::upgrade::signing;
    let directory = root.join("upgrade-bin");
    std::fs::create_dir(&directory).unwrap();
    for version in ["v0.1.0", "v0.2.0"] {
        std::fs::copy(
            env!("CARGO_BIN_EXE_bun"),
            directory.join(format!("bun-{version}")),
        )
        .unwrap();
        std::fs::write(directory.join(format!("bun-{version}.version")), version).unwrap();
    }
    std::os::unix::fs::symlink("bun-v0.1.0", directory.join("bun")).unwrap();
    let (key, public) = signing::generate_keypair().unwrap();
    let mut config =
        reliaburger::config::node::NodeConfig::from_file(&root.join("node.toml")).unwrap();
    config.upgrades.binary_dir = Some(directory);
    config.upgrades.release_keys_override = Some(vec![signing::encode_public_key(&public)]);
    config.upgrades.boot_grace_secs = 2;
    std::fs::write(root.join("node.toml"), toml::to_string(&config).unwrap()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(
        root.join("listen"),
        listener.local_addr().unwrap().to_string(),
    )
    .unwrap();
    key
}

#[cfg(feature = "ebpf")]
fn owned_upgrade_directive(
    root: &Path,
    key: &[u8],
) -> reliaburger::upgrade::types::UpgradeDirective {
    use reliaburger::upgrade::{
        signing,
        types::{BinarySource, UpgradeDirective},
    };
    let path = root.join("upgrade-bin/bun-v0.2.0");
    let bytes = std::fs::read(&path).unwrap();
    UpgradeDirective {
        upgrade_id: "owned-runtime-upgrade".into(),
        target_version: "v0.2.0".parse().unwrap(),
        binary_sha256: signing::sha256_hex(&bytes),
        embedded_signature: signing::sign(key, &bytes).unwrap(),
        external_signature: None,
        source: BinarySource::LocalFile { path },
        network_provenance: false,
    }
}

#[cfg(feature = "ebpf")]
async fn failed_owned_upgrade_reverts(root: &Path, node: &mut Node, key: &[u8]) {
    let original = node.client.status().await.unwrap().remove(0);
    std::fs::write(
        root.join("upgrade-bin/bun-v0.2.0.fail-boot"),
        "broken candidate",
    )
    .unwrap();
    let mut directive = owned_upgrade_directive(root, key);
    directive.upgrade_id = "owned-runtime-failed-candidate".into();
    node.client.upgrade_apply(&directive).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), node.child.wait())
        .await
        .unwrap()
        .unwrap();
    *node = Node::start_with_restarts(root, 2).await;
    assert_eq!(node.client.node_version().await.unwrap(), "v0.1.0");
    let status = node.client.upgrade_status().await.unwrap();
    assert!(status["in_flight"].is_null());
    assert!(
        status["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["outcome"] == "Reverted")
    );
    let adopted = node.client.status().await.unwrap().remove(0);
    assert_eq!(adopted.id, original.id);
    assert_eq!(adopted.pid, original.pid);
    assert_eq!(adopted.host_port, original.host_port);
    assert_eq!(
        std::fs::read_to_string(root.join("shared/main")).unwrap(),
        "main\n"
    );
}

#[cfg(feature = "ebpf")]
async fn upgrade_and_rollback(root: &Path, node: &Node, key: &[u8]) {
    let original = node.client.status().await.unwrap().remove(0);
    let directive = owned_upgrade_directive(root, key);
    for version in ["v0.2.0", "v0.1.0"] {
        if version == "v0.2.0" {
            node.client.upgrade_apply(&directive).await.unwrap();
        } else {
            node.client.upgrade_node_rollback(None).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                if let Ok(status) = node.client.upgrade_status().await
                    && status["running_version"] == version
                    && status["in_flight"].is_null()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("owned runtime never settled on {version}"));
        let adopted = node.client.status().await.unwrap().remove(0);
        assert_eq!(adopted.id, original.id);
        assert_eq!(adopted.pid, original.pid);
        assert_eq!(adopted.host_port, original.host_port);
        assert_eq!(
            std::fs::read_link(root.join("upgrade-bin/bun")).unwrap(),
            Path::new(&format!("bun-{version}"))
        );
        assert_eq!(
            std::fs::read_to_string(root.join("shared/main")).unwrap(),
            "main\n"
        );
    }
}

#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "requires isolated Linux root, bpffs, real runc/ip/nft and static BusyBox"]
async fn normal_owned_bun_upgrade_and_rollback_preserve_runtime_and_kernel() {
    let root = tempfile::tempdir().unwrap().keep();
    durable_fixture(&root);
    let key = upgrade_fixture(&root);
    let mut node = Node::start(&root).await;
    node.client
        .apply(&durable_app("upgrade-owned"))
        .await
        .unwrap();
    wait_file(&root.join("shared/main")).await;
    let original = kernel_manifest(&root);
    upgrade_and_rollback(&root, &node, &key).await;
    failed_owned_upgrade_reverts(&root, &mut node, &key).await;
    assert_eq!(kernel_manifest(&root), original);
    node.client.stop("upgrade-owned", "default").await.unwrap();
    node.crash().await;
    let journal =
        reliaburger::bun::discovery_owners::DiscoveryJournal::open(&root.join("data/discovery"))
            .unwrap();
    assert!(journal.inventory().services.is_empty());
    drop(journal);
    retire_kernel(&root);
}

#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "requires unprivileged Linux user, rootless runc/slirp and static BusyBox"]
async fn normal_rootless_bun_recovers_owned_forward_and_discovery() {
    assert!(!nix::unistd::geteuid().is_root());
    let root = tempfile::tempdir().unwrap().keep();
    durable_fixture(&root);
    let config = std::fs::read_to_string(root.join("node.toml")).unwrap();
    std::fs::write(
        root.join("node.toml"),
        config.replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    let key = upgrade_fixture(&root);
    let mut node = Node::start(&root).await;
    let active = root.join("data/discovery/discovery.json").exists();
    if !active {
        node.crash().await;
    }
    assert!(
        active,
        "normal rootless startup did not activate durable discovery"
    );
    assert!(!root.join("data/kernel-policy").exists());
    let mut app = durable_app("rootless-owned");
    app.app.get_mut("rootless-owned").unwrap().command = vec![
        "/bin/busybox".into(), "sh".into(), "-c".into(),
        "printf 'main\n' >> /work/main; printf owned > /work/index.html; exec /bin/busybox httpd -f -p 8080 -h /work".into(),
    ];
    node.client.apply(&app).await.unwrap();
    wait_file(&root.join("shared/main")).await;
    let original = node.client.status().await.unwrap().remove(0);
    let url = format!("http://127.0.0.1:{}/", original.host_port.unwrap());
    assert_eq!(
        reqwest::get(&url).await.unwrap().text().await.unwrap(),
        "owned"
    );
    node.crash().await;
    let mut recovered = Node::start(&root).await;
    let adopted = recovered.client.status().await.unwrap().remove(0);
    assert_eq!(adopted.pid, original.pid);
    assert_eq!(adopted.host_port, original.host_port);
    assert_eq!(
        reqwest::get(&url).await.unwrap().text().await.unwrap(),
        "owned"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("shared/main")).unwrap(),
        "main\n"
    );
    upgrade_and_rollback(&root, &recovered, &key).await;
    failed_owned_upgrade_reverts(&root, &mut recovered, &key).await;
    assert_eq!(
        reqwest::get(&url).await.unwrap().text().await.unwrap(),
        "owned"
    );
    recovered
        .client
        .stop("rootless-owned", "default")
        .await
        .unwrap();
    recovered.crash().await;
    let journal =
        reliaburger::bun::discovery_owners::DiscoveryJournal::open(&root.join("data/discovery"))
            .unwrap();
    assert!(journal.inventory().services.is_empty());
    assert!(reqwest::get(&url).await.is_err());
}

#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "requires isolated Linux root, bpffs, real runc/ip/nft and static BusyBox"]
async fn normal_clustered_bun_recovers_enrolled_consumer_before_adoption() {
    use reliaburger::config::node::NodeConfig;
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap().keep();
    durable_fixture(&root);
    reliaburger::relish::commands::init(&root, "activation", "activation-node").unwrap();
    let base = NodeConfig::from_file(&root.join("node.toml")).unwrap();
    let mut config = NodeConfig::from_file(&root.join("reliaburger.toml")).unwrap();
    config.storage = base.storage;
    config.images = base.images;
    config.ebpf = base.ebpf;
    config.network.advertise_address = Some("127.0.0.1".into());
    let gossip = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let raft = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let reporting = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.cluster.gossip_port = gossip.local_addr().unwrap().port();
    config.cluster.raft_port = raft.local_addr().unwrap().port();
    config.cluster.reporting_port = reporting.local_addr().unwrap().port();
    std::fs::write(root.join("node.toml"), toml::to_string(&config).unwrap()).unwrap();
    std::fs::write(root.join("cluster"), "enrolled").unwrap();
    drop((gossip, raft, reporting));
    let identity = reliaburger::sesame::identity_store::load(&root.join("identity"))
        .unwrap()
        .unwrap();
    let expected = reliaburger::bun::consumer_owners::ConsumerIdentity {
        node_id: reliaburger::meat::NodeId::new("activation-node"),
        cluster_identity: Sha256::digest(&identity.root_ca_der).into(),
    };
    let mut node = Node::start(&root).await;
    let active = root.join("data/discovery/discovery.json").exists();
    if !active {
        node.crash().await;
    }
    assert!(
        active,
        "normal clustered startup did not activate consumer ownership"
    );
    node.client
        .apply(&durable_app("cluster-owned"))
        .await
        .unwrap();
    wait_file(&root.join("shared/main")).await;
    wait_cluster_publication(&node.client).await;
    let original = node.client.status().await.unwrap().remove(0);
    node.crash().await;
    let journal =
        reliaburger::bun::discovery_owners::DiscoveryJournal::open(&root.join("data/discovery"))
            .unwrap();
    assert_eq!(
        journal.inventory().consumer.as_ref().unwrap().identity,
        expected
    );
    drop(journal);
    let mut recovered = Node::start(&root).await;
    wait_cluster_publication(&recovered.client).await;
    let adopted = recovered.client.status().await.unwrap().remove(0);
    assert_eq!(adopted.pid, original.pid);
    assert_eq!(adopted.host_port, original.host_port);
    assert_eq!(
        std::fs::read_to_string(root.join("shared/main")).unwrap(),
        "main\n"
    );
    recovered
        .client
        .stop("cluster-owned", "default")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let checkpoint: serde_json::Value = serde_json::from_slice(
                &std::fs::read(root.join("data/discovery/discovery.json")).unwrap(),
            )
            .unwrap();
            let inventory = &checkpoint["inventory"];
            if inventory["services"].as_array().unwrap().is_empty()
                && inventory["references"].as_array().unwrap().is_empty()
                && inventory["consumer"]["receipts"]
                    .as_object()
                    .unwrap()
                    .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "cluster retirement failed: {}",
            std::fs::read_to_string(root.join("bun.log")).unwrap()
        )
    });
    recovered.crash().await;
    let journal =
        reliaburger::bun::discovery_owners::DiscoveryJournal::open(&root.join("data/discovery"))
            .unwrap();
    assert_eq!(
        journal.inventory().consumer.as_ref().unwrap().identity,
        expected
    );
    drop(journal);
    retire_kernel(&root);
}

#[cfg(feature = "ebpf")]
async fn wait_cluster_publication(client: &BunClient) {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Ok(service) = client.resolve("cluster-owned").await
                && service.healthy_backends == 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("cluster never confirmed its workload publication");
}

#[cfg(feature = "ebpf")]
async fn enrolled_upgrade_fixture(
    root: &Path,
    name: &str,
    seed: Option<(&Path, &Node)>,
) -> (Node, Vec<u8>) {
    use reliaburger::config::node::NodeConfig;
    durable_fixture(root);
    let base = NodeConfig::from_file(&root.join("node.toml")).unwrap();
    let mut config = if let Some((seed_root, seed_node)) = seed {
        let mut config = NodeConfig::from_file(&seed_root.join("node.toml")).unwrap();
        std::fs::copy(
            seed_root.join("operator-token"),
            root.join("operator-token"),
        )
        .unwrap();
        let token = seed_node.client.join_token_create(name, 300).await.unwrap();
        let identity = reliaburger::sesame::identity_store::load(&seed_root.join("identity"))
            .unwrap()
            .unwrap();
        let fingerprint =
            reliaburger::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der);
        reliaburger::relish::commands::join(
            &token,
            seed_node.client.base_url(),
            name,
            Some(&root.join("identity")),
            Some(&fingerprint),
        )
        .await
        .unwrap();
        std::fs::copy(
            seed_root.join("activation-master.key"),
            root.join("activation-master.key"),
        )
        .unwrap();
        config.security.bootstrap_path = None;
        config.security.identity_dir = Some(root.join("identity"));
        config.security.master_key_path = Some(root.join("activation-master.key"));
        config.cluster.join = vec![format!("127.0.0.1:{}", config.cluster.gossip_port)];
        config
    } else {
        reliaburger::relish::commands::init(root, "activation", name).unwrap();
        let admin = reliaburger::sesame::token::create_token(
            "qualification-operator",
            reliaburger::sesame::types::ApiRole::Admin,
            Default::default(),
            None,
        )
        .unwrap();
        let path = root.join("activation-security-bootstrap.json");
        let mut state: reliaburger::sesame::types::SecurityState =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state.api_tokens.push(admin.token);
        std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
        std::fs::write(root.join("operator-token"), admin.plaintext).unwrap();
        std::fs::set_permissions(
            root.join("operator-token"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        NodeConfig::from_file(&root.join("reliaburger.toml")).unwrap()
    };
    config.node.name = Some(name.into());
    config.node.labels.insert("fixture".into(), name.into());
    config.storage = base.storage;
    config.images = base.images;
    config.ebpf = base.ebpf;
    config.network.advertise_address = Some("127.0.0.1".into());
    // Council discovery requires the same gossip-to-Raft offset on every node.
    let (gossip, raft, reporting) = loop {
        let gossip = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = gossip.local_addr().unwrap().port();
        let Some(reporting_port) = port.checked_add(2) else {
            continue;
        };
        let Ok(raft) = std::net::TcpListener::bind(("127.0.0.1", port + 1)) else {
            continue;
        };
        let Ok(reporting) = std::net::TcpListener::bind(("127.0.0.1", reporting_port)) else {
            continue;
        };
        break (gossip, raft, reporting);
    };
    config.cluster.gossip_port = gossip.local_addr().unwrap().port();
    config.cluster.raft_port = raft.local_addr().unwrap().port();
    config.cluster.reporting_port = reporting.local_addr().unwrap().port();
    std::fs::write(root.join("node.toml"), toml::to_string(&config).unwrap()).unwrap();
    std::fs::write(root.join("cluster"), "enrolled").unwrap();
    let key = upgrade_fixture(root);
    drop((gossip, raft, reporting));
    (Node::start(root).await, key)
}

#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "requires isolated Linux root, bpffs, real runc/ip/nft and static BusyBox"]
async fn three_enrolled_oci_nodes_preserve_ownership_through_upgrade_and_rollback() {
    let root = tempfile::tempdir().unwrap().keep();
    let roots: Vec<_> = (0..3)
        .map(|i| {
            let path = root.join(format!("node{i}"));
            std::fs::create_dir(&path).unwrap();
            path
        })
        .collect();
    let mut nodes = Vec::new();
    let mut keys = Vec::new();
    let (node, key) = enrolled_upgrade_fixture(&roots[0], "rolling-0", None).await;
    nodes.push(node);
    keys.push(key);
    for (i, node_root) in roots.iter().enumerate().skip(1) {
        let (node, key) = enrolled_upgrade_fixture(
            node_root,
            &format!("rolling-{i}"),
            Some((&roots[0], &nodes[0])),
        )
        .await;
        nodes.push(node);
        keys.push(key);
    }
    let mut last_views = vec![];
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut ready = true;
            last_views.clear();
            for node in &nodes {
                let council = node.client.council().await;
                let membership = node.client.nodes().await;
                ready &= council
                    .as_ref()
                    .is_ok_and(|c| c.members.len() == 3 && c.leader.is_some());
                ready &= membership.as_ref().is_ok_and(|members| {
                    members
                        .iter()
                        .filter(|n| n.state.eq_ignore_ascii_case("alive"))
                        .count()
                        == 3
                });
                last_views.push(format!("council={council:?}; membership={membership:?}"));
            }
            if ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("three enrolled voters did not converge: {last_views:?}"));
    let mut app = durable_app("cluster-owned");
    app.app.get_mut("cluster-owned").unwrap().placement =
        Some(reliaburger::config::app::PlacementSpec {
            required: vec!["fixture=rolling-0".into()],
            preferred: vec![],
        });
    nodes[0].client.apply(&app).await.unwrap();
    for node in &nodes {
        wait_cluster_publication(&node.client).await;
    }
    let original = nodes[0].client.status().await.unwrap().remove(0);
    let manifests: Vec<_> = roots.iter().map(|root| kernel_manifest(root)).collect();
    for version in ["v0.2.0", "v0.1.0"] {
        let leader = nodes[0].client.council().await.unwrap().leader.unwrap();
        let mut order: Vec<_> = (0..3).collect();
        order.sort_by_key(|i| format!("rolling-{i}") == leader);
        for i in order {
            if version == "v0.2.0" {
                nodes[i]
                    .client
                    .upgrade_apply(&owned_upgrade_directive(&roots[i], &keys[i]))
                    .await
                    .unwrap();
            } else {
                nodes[i].client.upgrade_node_rollback(None).await.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if let Ok(status) = nodes[i].client.upgrade_status().await
                        && status["running_version"] == version
                        && status["in_flight"].is_null()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("clustered owned upgrade did not settle");
            for node in &nodes {
                wait_cluster_publication(&node.client).await;
            }
            let current = nodes[0].client.status().await.unwrap().remove(0);
            assert_eq!(current.id, original.id);
            assert_eq!(current.pid, original.pid);
            assert_eq!(current.host_port, original.host_port);
            assert_eq!(
                std::fs::read_to_string(roots[0].join("shared/main")).unwrap(),
                "main\n"
            );
            for (root, original) in roots.iter().zip(&manifests) {
                assert_eq!(kernel_manifest(root), *original);
            }
        }
    }
    nodes[0]
        .client
        .stop("cluster-owned", "default")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let mut cleared = true;
            for root in &roots {
                let saved: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(root.join("data/discovery/discovery.json")).unwrap(),
                )
                .unwrap();
                let inventory = &saved["inventory"];
                cleared &= inventory["services"].as_array().unwrap().is_empty()
                    && inventory["references"].as_array().unwrap().is_empty()
                    && inventory["consumer"]["receipts"]
                        .as_object()
                        .unwrap()
                        .is_empty();
            }
            if cleared {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("cluster retained withdrawal obligations after all consumers confirmed");
    for node in &mut nodes {
        node.crash().await;
    }
    for root in &roots {
        retire_kernel(root);
    }
}

#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "requires Linux root and private mount namespaces"]
async fn managed_guest_startup_establishes_bpffs_before_bun() {
    let command = reliaburger::relish::quickstart::provision::SERVICE
        .lines()
        .find_map(|line| line.strip_prefix("ExecStartPre="))
        .expect("durable startup requires a BPF mount preflight");
    let script = format!(
        "set -eu\numount /sys/fs/bpf\n{command}\ntest \"$(stat -f -c %T /sys/fs/bpf)\" = bpf_fs\n{command}\ntest \"$(stat -f -c %T /sys/fs/bpf)\" = bpf_fs\n"
    );
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("unshare")
            .args(["--mount", "--propagation", "private", "sh", "-c", &script])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(feature = "ebpf")]
async fn assert_startup_refused(root: &Path, expected: &str) {
    let output = tokio::time::timeout(
        Duration::from_secs(20),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_bun"))
            .arg("--config")
            .arg(root.join("node.toml"))
            .args(["--runtime", "runc", "--listen", "127.0.0.1:0"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.contains(expected),
        "{stderr}"
    );
    assert!(!stderr.contains("API server listening"));
}

#[cfg(feature = "ebpf")]
#[tokio::test]
#[ignore = "run through scripts/release/qualify-discovery-reboot.sh on a disposable VM"]
async fn actual_bun_kernel_discovery_host_reboot() {
    let Ok(directory) = std::env::var("RELIABURGER_DISCOVERY_REBOOT_DIRECTORY") else {
        return;
    };
    let root = Path::new(&directory);
    let name = format!(
        "reboot-{}",
        root.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .rsplit('.')
            .next()
            .unwrap()
            .to_ascii_lowercase()
    );
    let name = name.as_str();
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    match std::env::var("RELIABURGER_REBOOT_PHASE").unwrap().as_str() {
        "prepare" => {
            assert!(!root.join("proof.json").exists());
            durable_fixture(root);
            let node = Node::start(root).await;
            node.client.apply(&durable_app(name)).await.unwrap();
            wait_file(&root.join("shared/main")).await;
            let discovery: serde_json::Value = serde_json::from_slice(
                &std::fs::read(root.join("data/discovery/discovery.json")).unwrap(),
            )
            .unwrap();
            let reference: reliaburger::grill::runc_intent::NetworkReference =
                serde_json::from_value(
                    discovery["inventory"]["references"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|owner| {
                            owner["reference"]["instance_id"] == format!("default__{name}-0")
                        })
                        .unwrap()["reference"]
                        .clone(),
                )
                .unwrap();
            assert_eq!(
                discovery["inventory"]["services"].as_array().unwrap().len(),
                1
            );
            assert_eq!(kernel_manifest(root)["boot_id"], boot.trim());
            std::fs::write(
                root.join("proof.json"),
                serde_json::to_vec(&serde_json::json!({
                    "boot": boot, "kernel": kernel_manifest(root), "discovery": discovery,
                    "reference": reference
                }))
                .unwrap(),
            )
            .unwrap();
            // Keep the actual Bun and workload alive for the external power-cut.
            // File-backed stdout survives this test process exiting.
            std::mem::forget(node);
        }
        "verify" => {
            let proof: serde_json::Value =
                serde_json::from_slice(&std::fs::read(root.join("proof.json")).unwrap()).unwrap();
            assert_ne!(
                proof["boot"].as_str().unwrap(),
                boot,
                "kernel never rebooted"
            );
            let pins = Path::new(proof["kernel"]["pin_directory"].as_str().unwrap());
            assert!(!pins.exists(), "original kernel pins survived reboot");
            let reference: reliaburger::grill::runc_intent::NetworkReference =
                serde_json::from_value(proof["reference"].clone()).unwrap();
            assert!(!reliaburger::grill::netns::namespace_path(&reference.instance_id).exists());
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(
                    &std::fs::read(root.join("data/discovery/discovery.json")).unwrap()
                )
                .unwrap(),
                proof["discovery"],
                "reboot erased durable discovery obligations"
            );
            let mut node = Node::start(root).await;
            assert_eq!(kernel_manifest(root)["boot_id"], boot.trim());
            assert!(
                node.client.status().await.unwrap().is_empty(),
                "old runtime was republished"
            );
            assert_eq!(
                runtime(root).state(&reference.instance_id).await.unwrap(),
                ContainerState::Stopped
            );
            assert_eq!(runtime(root).exit_code(&reference.instance_id).await, None);
            assert_eq!(
                std::fs::read_to_string(root.join("shared/main")).unwrap(),
                "main\n"
            );
            let discovery: serde_json::Value = serde_json::from_slice(
                &std::fs::read(root.join("data/discovery/discovery.json")).unwrap(),
            )
            .unwrap();
            assert!(
                discovery["inventory"]["services"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert!(
                discovery["inventory"]["references"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            node.client.apply(&durable_app(name)).await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while std::fs::read_to_string(root.join("shared/main")).unwrap() != "main\nmain\n" {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            node.crash().await;
            assert!(
                runtime(root)
                    .release_network_reference(&reference)
                    .await
                    .is_err(),
                "old release reached a successor"
            );
            let mut node = Node::start(root).await;
            node.client.stop(name, "default").await.unwrap();
            node.crash().await;
            retire_kernel(root);
            std::fs::write(root.join("verified-boot"), boot).unwrap();
        }
        "cleanup" => {
            let mut node = Node::start(root).await;
            assert!(node.client.status().await.unwrap().is_empty());
            node.crash().await;
            retire_kernel(root);
        }
        phase => panic!("unknown reboot phase {phase}"),
    }
}
