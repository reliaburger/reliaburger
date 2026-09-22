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
        let log = root.join("bun.log");
        let offset = std::fs::metadata(&log).map_or(0, |m| m.len()) as usize;
        let output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bun"))
            .arg("--config")
            .arg(root.join("node.toml"))
            .args([
                "--listen",
                "127.0.0.1:0",
                "--runtime",
                "runc",
                "--experimental-owned-runc",
            ])
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
                    let client = BunClient::new(&format!("http://{address}"));
                    if client.health().await.is_ok() {
                        break client;
                    }
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "Bun exited: {contents}"
                );
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
