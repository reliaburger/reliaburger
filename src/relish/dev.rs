/// Dev cluster management via Lima VMs.
///
/// Spins up real Ubuntu VMs with rootless runc, gossip networking,
/// and Raft consensus. The Reliaburger equivalent of `minikube start`.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::RelishError;

/// Command to launch the bun agent in a cluster VM. `--cluster` is essential:
/// without it the agent ignores the `[cluster]` config and runs single-node.
/// Launched via `sudo bash -c` so bun gets root (runc/netns) and can write the
/// log file under `/var/log`.
/// The command that launches a node's `bun` agent under the given runtime.
///
/// `runc` is the default for a realistic cluster; `process` runs workloads as
/// host processes, which is what the built-in `relish test` suite needs — its
/// `testapp` workload is a `bun` subcommand, so a process-runtime node runs it
/// straight from the installed binary with no image.
fn agent_launch_cmd(runtime: &str) -> String {
    format!(
        "nohup bun --cluster --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117 \
         --runtime {runtime} > /var/log/bun.log 2>&1 &"
    )
}

/// The runtime a dev cluster uses when its state file predates the field.
fn default_runtime() -> String {
    "runc".to_string()
}

// ---------------------------------------------------------------------------
// Lima wrapper
// ---------------------------------------------------------------------------

/// Check if limactl is available in PATH.
pub fn lima_available() -> bool {
    std::process::Command::new("limactl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run a limactl command and return stdout.
async fn limactl(args: &[&str]) -> Result<String, RelishError> {
    let output = tokio::process::Command::new("limactl")
        .args(args)
        .output()
        .await
        .map_err(|e| RelishError::LimaError {
            command: format!("limactl {}", args.join(" ")),
            stderr: e.to_string(),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(RelishError::LimaError {
            command: format!("limactl {}", args.join(" ")),
            stderr,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get the inter-VM IP address of a Lima VM.
///
/// Prefers the `lima0` interface (Lima's `user-v2` network), whose address is
/// reachable *between* VMs. Falls back to the first `hostname -I` address that
/// isn't loopback or Lima's default user-mode network (`192.168.5.x`,
/// identical across VMs and not inter-reachable).
async fn get_vm_ip(name: &str) -> Result<String, RelishError> {
    // `ip -4 -o addr show lima0` → "2: lima0    inet 192.168.104.3/24 brd ..."
    if let Ok(out) = limactl(&["shell", name, "ip", "-4", "-o", "addr", "show", "lima0"]).await
        && let Some(ip) = out
            .split_whitespace()
            .skip_while(|t| *t != "inet")
            .nth(1)
            .and_then(|cidr| cidr.split('/').next())
        && !ip.is_empty()
    {
        return Ok(ip.to_string());
    }

    let output = limactl(&["shell", name, "hostname", "-I"]).await?;
    let ip = output
        .split_whitespace()
        .find(|ip| !ip.starts_with("127.") && !ip.starts_with("192.168.5."))
        .or_else(|| output.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    if ip.is_empty() {
        return Err(RelishError::LimaError {
            command: format!("get IP for {name}"),
            stderr: "no IP address found".to_string(),
        });
    }
    Ok(ip)
}

// ---------------------------------------------------------------------------
// Building binaries for the VMs
// ---------------------------------------------------------------------------

/// Ensure the Linux build VM (`reliaburger-test`) exists and is running.
/// Shared by `dev test` and `dev create` (which builds the cluster binaries
/// here). Creating it is a one-time cost; it persists with its cargo cache.
async fn ensure_build_vm() -> Result<(), RelishError> {
    let list = limactl(&["list", "--format", "{{.Name}}"]).await?;
    if !list.lines().any(|l| l.trim() == TEST_VM_NAME) {
        eprintln!("creating build VM ({TEST_VM_NAME})...");
        let yaml = generate_test_vm_yaml();
        let yaml_path = std::env::temp_dir().join("reliaburger-test.yaml");
        std::fs::write(&yaml_path, &yaml)?;
        limactl(&[
            "create",
            "--name",
            TEST_VM_NAME,
            yaml_path.to_str().unwrap(),
        ])
        .await?;
        limactl(&["start", TEST_VM_NAME]).await?;
        eprintln!("build VM ready.");
    }

    let info = limactl(&["list", "--format", "{{.Name}} {{.Status}}"]).await?;
    let running = info
        .lines()
        .any(|l| l.starts_with(TEST_VM_NAME) && l.contains("Running"));
    if !running {
        limactl(&["start", TEST_VM_NAME]).await?;
    }
    Ok(())
}

/// The VM-local cargo target dir for the current repo (matches `dev test`),
/// chosen so the host-mounted `target/` isn't used over virtiofs.
fn vm_target_dir(repo_path: &str) -> String {
    format!("/tmp/reliaburger-target{}", repo_path.replace('/', "-"))
}

/// Build `bun` and `relish` (release, Linux) inside the build VM and copy the
/// artefacts out to host temp files. Returns `(bun_path, relish_path)`.
async fn build_binaries_in_vm() -> Result<(PathBuf, PathBuf), RelishError> {
    ensure_build_vm().await?;

    let repo_dir = std::env::current_dir().map_err(|e| RelishError::LimaError {
        command: "get cwd".to_string(),
        stderr: e.to_string(),
    })?;
    let repo_path = repo_dir.to_string_lossy();
    let vm_target = vm_target_dir(&repo_path);

    println!("  building bun + relish for Linux in the build VM (first run is slow)...");
    // Debug build — much faster than release, and fine for a dev cluster.
    // Run with `sudo -E` to match `dev test`, so the shared cargo cache in the
    // VM has consistent (root) ownership and isn't half root / half user.
    let build_cmd = format!(
        "cd {repo_path} && source $HOME/.cargo/env && mkdir -p {vm_target} && \
         sudo -E env PATH=\"$PATH\" CARGO_TARGET_DIR={vm_target} \
         cargo build --bin bun --bin relish -j 2"
    );
    // Stream build output so the user sees progress.
    let status = std::process::Command::new("limactl")
        .args(["shell", TEST_VM_NAME, "bash", "-c", &build_cmd])
        .status()
        .map_err(|e| RelishError::LimaError {
            command: "build in VM".to_string(),
            stderr: e.to_string(),
        })?;
    if !status.success() {
        return Err(RelishError::LimaError {
            command: "cargo build (VM)".to_string(),
            stderr: format!(
                "build failed with exit code {}",
                status.code().unwrap_or(-1)
            ),
        });
    }

    let tmp = std::env::temp_dir();
    let bun_path = tmp.join("reliaburger-dev-bun");
    let relish_path = tmp.join("reliaburger-dev-relish");
    limactl(&[
        "copy",
        &format!("{TEST_VM_NAME}:{vm_target}/debug/bun"),
        bun_path.to_str().unwrap(),
    ])
    .await?;
    limactl(&[
        "copy",
        &format!("{TEST_VM_NAME}:{vm_target}/debug/relish"),
        relish_path.to_str().unwrap(),
    ])
    .await?;
    Ok((bun_path, relish_path))
}

/// Copy a host binary into a cluster VM at `/usr/local/bin/<name>` (via a
/// user-writable temp path + `sudo install`, since `limactl copy` runs as the
/// unprivileged lima user).
async fn install_binary(
    vm: &str,
    host_path: &std::path::Path,
    name: &str,
) -> Result<(), RelishError> {
    let tmp_dest = format!("/tmp/{name}");
    limactl(&[
        "copy",
        host_path.to_str().ok_or_else(|| RelishError::LimaError {
            command: "install binary".to_string(),
            stderr: format!("non-UTF-8 path for {name}"),
        })?,
        &format!("{vm}:{tmp_dest}"),
    ])
    .await?;
    limactl(&[
        "shell",
        vm,
        "sudo",
        "install",
        "-m",
        "755",
        &tmp_dest,
        &format!("/usr/local/bin/{name}"),
    ])
    .await?;
    Ok(())
}

/// Copy a host file into a VM at `dest` with mode 0600 (via a user-writable
/// temp path + `sudo install`, since `limactl copy` runs unprivileged). Used
/// for the master key and security bootstrap — both secrets.
async fn install_secret_file(
    vm: &str,
    host_path: &std::path::Path,
    dest: &str,
) -> Result<(), RelishError> {
    let file_name = host_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| RelishError::LimaError {
            command: "install secret".to_string(),
            stderr: "non-UTF-8 file name".to_string(),
        })?;
    let tmp_dest = format!("/tmp/{file_name}");
    limactl(&[
        "copy",
        host_path.to_str().ok_or_else(|| RelishError::LimaError {
            command: "install secret".to_string(),
            stderr: format!("non-UTF-8 path for {dest}"),
        })?,
        &format!("{vm}:{tmp_dest}"),
    ])
    .await?;
    limactl(&[
        "shell", vm, "sudo", "install", "-D", "-m", "600", &tmp_dest, dest,
    ])
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Lima YAML generation
// ---------------------------------------------------------------------------

/// Generate a Lima YAML config for a dev cluster node.
fn generate_lima_yaml(cpus: usize, memory: &str) -> String {
    format!(
        r#"images:
  - location: "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img"
    arch: "aarch64"
  - location: "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
    arch: "x86_64"
cpus: {cpus}
memory: "{memory}"
disk: "10GiB"
networks:
  - lima: user-v2
provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux
      apt-get update -qq
      apt-get install -y -qq runc uidmap btrfs-progs
      mkdir -p /etc/reliaburger
"#
    )
}

/// Generate a node.toml config for a cluster node.
///
/// `node_ip` is the node's own shared-network IP; it's advertised so gossip,
/// Raft, and reporting bind a peer-reachable address (not loopback).
///
/// Every node points at the shared master key. Only the bootstrap node (the
/// one with an empty `join`) also points at the security bootstrap: it seeds
/// the `SecurityState` into Raft once, and it replicates to everyone else.
fn generate_node_config(
    cluster_name: &str,
    node_name: &str,
    node_ip: &str,
    first_node_ip: Option<&str>,
) -> String {
    let join = match first_node_ip {
        Some(ip) => format!(r#"join = ["{ip}:9443"]"#),
        None => "join = []".to_string(),
    };

    // first_node_ip is None only for the bootstrap node.
    let bootstrap_line = if first_node_ip.is_none() {
        "\nbootstrap_path = \"/etc/reliaburger/security-bootstrap.json\""
    } else {
        ""
    };

    format!(
        r#"# WARNING: DEVELOPMENT-ONLY PLAINTEXT CLUSTER TRANSPORTS.
# `relish dev create` isolates these nodes in local Lima VMs. Do not reuse
# this config on a shared or production network.

[node]
name = "{node_name}"

[network]
advertise_address = "{node_ip}"

[cluster]
name = "{cluster_name}"
{join}
gossip_port = 9443
raft_port = 9444
reporting_port = 9445

[security]
master_key_path = "/etc/reliaburger/master.key"{bootstrap_line}
require_mtls = false
# Deliberate local dev cluster: accept plaintext cluster transports on the
# inter-VM (routable) network. Never do this on a shared or production network.
allow_insecure_cluster = true
"#
    )
}

// ---------------------------------------------------------------------------
// Cluster state persistence
// ---------------------------------------------------------------------------

/// Persistent state for a dev cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevCluster {
    pub name: String,
    pub nodes: Vec<DevNode>,
    /// The container runtime every node runs (`runc` or `process`). Persisted
    /// so `start` relaunches with the same runtime `create` chose.
    #[serde(default = "default_runtime")]
    pub runtime: String,
}

/// A single node in the dev cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevNode {
    pub name: String,
    pub ip: Option<String>,
    pub cpus: usize,
    pub memory: String,
}

/// Directory for dev cluster state files.
fn state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".reliaburger")
        .join("dev")
        .join("clusters")
}

fn state_path(cluster_name: &str) -> PathBuf {
    state_dir().join(format!("{cluster_name}.json"))
}

fn save_cluster(cluster: &DevCluster) -> Result<(), RelishError> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(cluster).map_err(RelishError::SerialiseJson)?;
    std::fs::write(state_path(&cluster.name), json)?;
    Ok(())
}

fn load_cluster(name: &str) -> Result<DevCluster, RelishError> {
    let path = state_path(name);
    if !path.exists() {
        return Err(RelishError::DevClusterNotFound {
            name: name.to_string(),
        });
    }
    let json = std::fs::read_to_string(path)?;
    let cluster: DevCluster = serde_json::from_str(&json).map_err(|e| RelishError::LimaError {
        command: "load cluster state".to_string(),
        stderr: e.to_string(),
    })?;
    Ok(cluster)
}

fn delete_cluster_state(name: &str) {
    let _ = std::fs::remove_file(state_path(name));
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Create a new dev cluster.
pub async fn create(
    name: &str,
    nodes: usize,
    cpus: usize,
    memory: &str,
    runtime: &str,
    bun: Option<PathBuf>,
    relish: Option<PathBuf>,
) -> Result<(), RelishError> {
    let cluster_identity = crate::config::node::ClusterSection {
        name: name.to_string(),
        ..crate::config::node::ClusterSection::default()
    };
    cluster_identity
        .validate()
        .map_err(|error| RelishError::InitFailed(error.to_string()))?;
    if !lima_available() {
        eprintln!("error: limactl not found in PATH");
        eprintln!();
        eprintln!("Install Lima:");
        eprintln!("  macOS:  brew install lima");
        eprintln!("  Linux:  https://lima-vm.io/docs/installation/");
        return Err(RelishError::LimaNotFound);
    }

    if state_path(name).exists() {
        return Err(RelishError::DevClusterAlreadyExists {
            name: name.to_string(),
        });
    }

    println!("Creating dev cluster \"{name}\" with {nodes} nodes...");

    let yaml = generate_lima_yaml(cpus, memory);
    let mut cluster = DevCluster {
        name: name.to_string(),
        nodes: Vec::new(),
        runtime: runtime.to_string(),
    };

    // Create and start each VM. Idempotent: an existing VM (e.g. from an
    // interrupted create) is reused and just (re)started, so re-running
    // `create` resumes rather than erroring on "instance already exists".
    let existing = limactl(&["list", "--format", "{{.Name}}"]).await?;
    for i in 1..=nodes {
        // Embed the cluster name so two dev clusters with different `--name`
        // don't collide on `reliaburger-1`, `-2`, … and share VMs (O14).
        let node_name = format!("reliaburger-{name}-{i}");

        if existing.lines().any(|l| l.trim() == node_name) {
            println!("  {node_name} already exists — reusing");
            limactl(&["start", &node_name]).await?;
        } else {
            println!("  creating {node_name} ({cpus} CPUs, {memory} RAM)...");
            let yaml_path = std::env::temp_dir().join(format!("{node_name}.yaml"));
            std::fs::write(&yaml_path, &yaml)?;
            limactl(&[
                "create",
                "--name",
                &node_name,
                yaml_path.to_str().unwrap(),
                "--tty=false",
            ])
            .await?;
            limactl(&["start", &node_name]).await?;
            let _ = std::fs::remove_file(&yaml_path);
        }

        cluster.nodes.push(DevNode {
            name: node_name,
            ip: None,
            cpus,
            memory: memory.to_string(),
        });
    }

    // Discover IPs
    println!("  discovering node IPs...");
    for node in &mut cluster.nodes {
        node.ip = Some(get_vm_ip(&node.name).await?);
    }

    let first_ip = cluster.nodes[0].ip.as_deref().unwrap();

    // Generate the cluster's security material once, on the host. Every node
    // gets the master key (to build its wrapping IKM); only the bootstrap node
    // gets the security bootstrap (it seeds SecurityState into Raft, which then
    // replicates to the rest). Both are written 0600 and installed 0600.
    println!("  generating security material...");
    println!("  WARNING: dev-cluster internal transports are plaintext inside the Lima network");
    let sec_dir = std::env::temp_dir().join(format!("{name}-dev-security"));
    std::fs::create_dir_all(&sec_dir)?;
    let init = crate::sesame::init::initialize_cluster(name, &cluster.nodes[0].name, &sec_dir)
        .map_err(|e| RelishError::InitFailed(e.to_string()))?;
    let master_key_path = sec_dir.join("master.key");
    std::fs::write(&master_key_path, hex::encode(init.master_secret))?;
    let bootstrap_path = sec_dir.join("security-bootstrap.json");
    std::fs::write(
        &bootstrap_path,
        serde_json::to_string(&init.security_state).map_err(RelishError::SerialiseJson)?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&master_key_path, std::fs::Permissions::from_mode(0o600))?;
        std::fs::set_permissions(&bootstrap_path, std::fs::Permissions::from_mode(0o600))?;
    }

    // Generate and copy node configs + security material
    println!("  configuring cluster...");
    for (i, node) in cluster.nodes.iter().enumerate() {
        let join_ip = if i == 0 { None } else { Some(first_ip) };
        let node_ip = node.ip.as_deref().unwrap_or("127.0.0.1");
        let config = generate_node_config(name, &node.name, node_ip, join_ip);

        // Write config to a temp file, copy to a user-writable path in the VM
        // (limactl copy runs as the unprivileged lima user), then install it
        // into /etc/reliaburger with sudo.
        let config_path = std::env::temp_dir().join(format!("{}-node.toml", node.name));
        std::fs::write(&config_path, &config)?;
        limactl(&[
            "copy",
            config_path.to_str().unwrap(),
            &format!("{}:/tmp/node.toml", node.name),
        ])
        .await?;
        limactl(&[
            "shell",
            &node.name,
            "sudo",
            "install",
            "-D",
            "-m",
            "644",
            "/tmp/node.toml",
            "/etc/reliaburger/node.toml",
        ])
        .await?;
        let _ = std::fs::remove_file(&config_path);

        // Master key on every node; security bootstrap on the bootstrap node.
        install_secret_file(&node.name, &master_key_path, "/etc/reliaburger/master.key").await?;
        if i == 0 {
            install_secret_file(
                &node.name,
                &bootstrap_path,
                "/etc/reliaburger/security-bootstrap.json",
            )
            .await?;
        }
    }
    let _ = std::fs::remove_dir_all(&sec_dir);

    // Build (or use the provided) Linux binaries, then install on every node.
    let need_build = bun.is_none() || relish.is_none();
    let built = if need_build {
        Some(build_binaries_in_vm().await?)
    } else {
        None
    };
    let bun_path = bun.unwrap_or_else(|| built.as_ref().unwrap().0.clone());
    let relish_path = relish.unwrap_or_else(|| built.unwrap().1);

    println!("  installing binaries on nodes...");
    for node in &cluster.nodes {
        install_binary(&node.name, &bun_path, "bun").await?;
        install_binary(&node.name, &relish_path, "relish").await?;
    }

    // Start bun agents
    println!("  starting bun agents...");
    let launch = agent_launch_cmd(&cluster.runtime);
    for node in &cluster.nodes {
        limactl(&["shell", &node.name, "sudo", "bash", "-c", &launch]).await?;
    }

    save_cluster(&cluster)?;

    // Give the cluster time to gossip, elect a leader, and form the council,
    // then report the actual state (best-effort — don't fail create if the
    // API isn't ready yet).
    println!("  waiting for the cluster to converge...");
    tokio::time::sleep(std::time::Duration::from_secs(8)).await;

    // The nodes sit on Lima's user-v2 network, which the host can't route to,
    // so query the council from *inside* node 1 (where `relish` talks to the
    // local agent on 127.0.0.1). Best-effort — don't fail create if it's slow.
    let first = &cluster.nodes[0].name;
    println!();
    match limactl(&["shell", first, "relish", "council"]).await {
        Ok(out) if !out.trim().is_empty() => {
            println!("Dev cluster \"{name}\" ready:");
            print!("{out}");
        }
        _ => {
            println!("Dev cluster \"{name}\" created ({nodes} nodes) — still converging.");
            for node in &cluster.nodes {
                println!(
                    "  {}: {}",
                    node.name,
                    node.ip.as_deref().unwrap_or("unknown")
                );
            }
        }
    }

    println!();
    println!("Check: limactl shell {first} relish nodes   (also: council)");

    Ok(())
}

/// Show dev cluster status.
pub async fn status(name: &str) -> Result<(), RelishError> {
    let cluster = load_cluster(name)?;

    println!("{:<12} {:<6} {:<10}", "CLUSTER", "NODES", "STATUS");
    println!(
        "{:<12} {:<6} {:<10}",
        cluster.name,
        cluster.nodes.len(),
        "created"
    );
    println!();

    println!(
        "{:<20} {:<18} {:<6} {:<8} {:<10}",
        "NODE", "IP", "CPUS", "MEM", "STATUS"
    );
    for node in &cluster.nodes {
        let ip = node.ip.as_deref().unwrap_or("unknown");

        // Check if VM is running via limactl
        let vm_status = limactl(&["list", "--json"])
            .await
            .ok()
            .and_then(|json| {
                // Lima outputs one JSON object per line
                json.lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .find(|v| v["name"].as_str() == Some(&node.name))
                    .and_then(|v| v["status"].as_str().map(String::from))
            })
            .unwrap_or_else(|| "unknown".to_string());

        println!(
            "{:<20} {:<18} {:<6} {:<8} {:<10}",
            node.name, ip, node.cpus, node.memory, vm_status
        );
    }

    Ok(())
}

/// Open a shell on a dev cluster node.
pub async fn shell(node_name: &str) -> Result<(), RelishError> {
    // Use std::process::Command for interactive shell (not tokio — needs PTY)
    let status = std::process::Command::new("limactl")
        .args(["shell", node_name])
        .status()
        .map_err(|e| RelishError::LimaError {
            command: format!("limactl shell {node_name}"),
            stderr: e.to_string(),
        })?;

    if !status.success() {
        return Err(RelishError::LimaError {
            command: format!("limactl shell {node_name}"),
            stderr: "shell exited with error".to_string(),
        });
    }

    Ok(())
}

/// Stop all VMs in a dev cluster.
pub async fn stop(name: &str) -> Result<(), RelishError> {
    let cluster = load_cluster(name)?;
    println!("Stopping {} nodes...", cluster.nodes.len());
    for node in &cluster.nodes {
        let _ = limactl(&["stop", &node.name]).await;
    }
    println!("Dev cluster \"{name}\" stopped.");
    Ok(())
}

/// Start all VMs in a stopped dev cluster.
pub async fn start(name: &str) -> Result<(), RelishError> {
    let cluster = load_cluster(name)?;
    println!("Starting {} nodes...", cluster.nodes.len());
    for node in &cluster.nodes {
        limactl(&["start", &node.name]).await?;
    }

    // Restart bun agents with the runtime this cluster was created with.
    let launch = agent_launch_cmd(&cluster.runtime);
    for node in &cluster.nodes {
        limactl(&["shell", &node.name, "sudo", "bash", "-c", &launch]).await?;
    }

    println!("Dev cluster \"{name}\" started.");
    Ok(())
}

/// Destroy a dev cluster (stop and delete all VMs).
pub async fn destroy(name: &str) -> Result<(), RelishError> {
    let cluster = load_cluster(name)?;
    println!("Stopping and deleting {} nodes...", cluster.nodes.len());
    for node in &cluster.nodes {
        let _ = limactl(&["stop", &node.name]).await;
        let _ = limactl(&["delete", &node.name, "-f"]).await;
    }
    delete_cluster_state(name);
    println!("Dev cluster \"{name}\" destroyed.");

    // The build VM (`reliaburger-test`) is shared across all dev clusters, so
    // destroying one cluster must NOT delete it (O14) — that would break every
    // other cluster's next `create`/`test`. Remove it explicitly with
    // `relish dev test-vm-destroy` when you actually want it gone.

    Ok(())
}

// ---------------------------------------------------------------------------
// Test VM
// ---------------------------------------------------------------------------

const TEST_VM_NAME: &str = "reliaburger-test";

/// Generate Lima YAML for the test VM.
///
/// Installs Rust toolchain, runc, slirp4netns, and build tools.
/// The home directory is mounted read-write so the repo and cargo
/// cache are shared with the host.
fn generate_test_vm_yaml() -> String {
    r#"images:
  - location: "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img"
    arch: "aarch64"
  - location: "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img"
    arch: "x86_64"
cpus: 4
memory: "8GiB"
disk: "50GiB"
mountType: "virtiofs"
mounts:
  - location: "~"
    writable: true
provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux
      apt-get update -qq
      apt-get install -y -qq runc uidmap slirp4netns curl buildah btrfs-progs build-essential pkg-config libssl-dev clang llvm libbpf-dev linux-headers-$(uname -r)
  - mode: user
    script: |
      #!/bin/bash
      set -eux
      if [ ! -f "$HOME/.cargo/bin/rustup" ]; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
      fi
"#
    .to_string()
}

/// Run cargo test inside a Lima VM with all Linux test env vars.
///
/// Creates the test VM on first run, reuses it afterwards. The
/// repo is mounted from the host, so no code copying needed. The
/// Rust toolchain and cargo cache persist inside the VM.
pub async fn test(filter: Option<&str>, recreate: bool) -> Result<(), RelishError> {
    if !lima_available() {
        eprintln!("error: limactl not found in PATH");
        eprintln!();
        eprintln!("Install Lima:");
        eprintln!("  macOS:  brew install lima");
        eprintln!("  Linux:  https://lima-vm.io/docs/installation/");
        return Err(RelishError::LimaNotFound);
    }

    // Delete the existing test VM if --recreate was requested
    let list = limactl(&["list", "--format", "{{.Name}}"]).await?;
    if recreate && list.lines().any(|l| l.trim() == TEST_VM_NAME) {
        eprintln!("deleting test VM ({TEST_VM_NAME})...");
        limactl(&["delete", "--force", TEST_VM_NAME]).await?;
    }

    // Ensure the build/test VM exists and is running.
    ensure_build_vm().await?;

    // Build the cargo test command
    let repo_dir = std::env::current_dir().map_err(|e| RelishError::LimaError {
        command: "get cwd".to_string(),
        stderr: e.to_string(),
    })?;
    let repo_path = repo_dir.to_string_lossy();

    // Use a VM-local target directory to avoid virtiofs overhead.
    // The host-mounted repo has thousands of small files in target/
    // that are very slow over the filesystem bridge. A native ext4
    // target dir makes incremental builds 5-10x faster.
    let vm_target = format!("/tmp/reliaburger-target{}", repo_path.replace('/', "-"));

    // Use sudo for tests that need root (netns, runc, eBPF).
    // Pass through the cargo env and HOME so rustup/cargo work.
    // Use --test-threads=1 because netns/veth tests can't run in
    // parallel (they share the host network namespace).
    // Use -j 2 to limit parallel compile/link jobs — DataFusion + Arrow
    // produce large binaries and the linker can OOM the VM at -j 4.
    let mut test_cmd = format!(
        "cd {repo_path} && source $HOME/.cargo/env && \
         mkdir -p {vm_target} && \
         sudo -E env PATH=\"$PATH\" \
         CARGO_TARGET_DIR={vm_target} \
         RELIABURGER_RUNC_TESTS=1 \
         RELIABURGER_NETNS_TESTS=1 \
         RELIABURGER_EBPF_TESTS=1 \
         RELIABURGER_BTRFS_TESTS=1 \
         RELIABURGER_BUILDAH_TESTS=1 \
         cargo test -j 2 --features ebpf"
    );

    if let Some(f) = filter {
        test_cmd.push(' ');
        test_cmd.push_str(f);
    }
    test_cmd.push_str(" -- --test-threads=1");

    eprintln!("running tests in VM...");
    eprintln!();

    // Run interactively so output streams to the terminal
    let status = std::process::Command::new("limactl")
        .args(["shell", TEST_VM_NAME, "bash", "-c", &test_cmd])
        .status()
        .map_err(|e| RelishError::LimaError {
            command: "run tests".to_string(),
            stderr: e.to_string(),
        })?;

    if !status.success() {
        return Err(RelishError::LimaError {
            command: "cargo test".to_string(),
            stderr: format!(
                "tests failed with exit code {}",
                status.code().unwrap_or(-1)
            ),
        });
    }

    Ok(())
}

/// Show disk usage inside the test VM.
///
/// Prints filesystem usage and the size of the cargo target directory.
pub async fn disk() -> Result<(), RelishError> {
    if !lima_available() {
        return Err(RelishError::LimaNotFound);
    }

    let list = limactl(&["list", "--format", "{{.Name}}"]).await?;
    if !list.lines().any(|l| l.trim() == TEST_VM_NAME) {
        eprintln!("test VM ({TEST_VM_NAME}) does not exist — nothing to show");
        return Ok(());
    }

    let cmd = "echo '=== Filesystem ===' && df -h / && \
               echo && echo '=== Cargo target dirs ===' && \
               du -sh /tmp/reliaburger-target-* 2>/dev/null || echo '(none)'";

    let status = std::process::Command::new("limactl")
        .args(["shell", TEST_VM_NAME, "bash", "-c", cmd])
        .status()
        .map_err(|e| RelishError::LimaError {
            command: "disk usage".to_string(),
            stderr: e.to_string(),
        })?;

    if !status.success() {
        return Err(RelishError::LimaError {
            command: "disk usage".to_string(),
            stderr: "failed to query disk usage".to_string(),
        });
    }

    Ok(())
}

/// Clean cargo build artefacts inside the test VM.
///
/// Removes the VM-local target directories to free disk space without
/// destroying the VM (which would require re-downloading the Rust
/// toolchain and system packages).
pub async fn clean() -> Result<(), RelishError> {
    if !lima_available() {
        return Err(RelishError::LimaNotFound);
    }

    let list = limactl(&["list", "--format", "{{.Name}}"]).await?;
    if !list.lines().any(|l| l.trim() == TEST_VM_NAME) {
        eprintln!("test VM ({TEST_VM_NAME}) does not exist — nothing to clean");
        return Ok(());
    }

    eprintln!("cleaning cargo build artefacts in test VM...");

    let cmd = "sudo du -sh /tmp/reliaburger-target-* 2>/dev/null && \
               sudo rm -rf /tmp/reliaburger-target-* && \
               echo 'cleaned.' || echo 'nothing to clean'";

    let status = std::process::Command::new("limactl")
        .args(["shell", TEST_VM_NAME, "bash", "-c", cmd])
        .status()
        .map_err(|e| RelishError::LimaError {
            command: "clean".to_string(),
            stderr: e.to_string(),
        })?;

    if !status.success() {
        return Err(RelishError::LimaError {
            command: "clean".to_string(),
            stderr: "failed to clean artefacts".to_string(),
        });
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Release signing tooling (Phase 14)
// ---------------------------------------------------------------------------

/// Generate an Ed25519 release signing keypair into `out`.
///
/// Writes `release.key` (PKCS#8 private key, mode 0600 — keep it outside
/// any repository) and `release.pub` (`ed25519:` + base64 public key).
pub fn keygen(out: &std::path::Path) -> Result<(), RelishError> {
    use crate::upgrade::signing;

    std::fs::create_dir_all(out)?;
    let key_path = out.join("release.key");
    let pub_path = out.join("release.pub");
    if key_path.exists() {
        return Err(RelishError::FileExists {
            path: key_path.display().to_string(),
        });
    }
    if pub_path.exists() {
        return Err(RelishError::FileExists {
            path: pub_path.display().to_string(),
        });
    }

    let (pkcs8, public) = signing::generate_keypair()?;
    std::fs::write(&key_path, &pkcs8)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }
    let encoded = signing::encode_public_key(&public);
    std::fs::write(&pub_path, &encoded)?;

    println!(
        "keygen: private key {} (keep it safe, never commit it)",
        key_path.display()
    );
    println!("keygen: public key  {}", pub_path.display());
    println!("public key: {encoded}");
    Ok(())
}

/// Sign a binary with a release key (and optionally an external key),
/// writing a detached signature envelope next to it (or to `out`).
pub fn sign_binary(
    key: &std::path::Path,
    binary: &std::path::Path,
    external_key: Option<&std::path::Path>,
    out: Option<&std::path::Path>,
) -> Result<(), RelishError> {
    use crate::upgrade::signing::{self, SignatureEnvelope};

    let bytes = std::fs::read(binary)?;
    let pkcs8 = std::fs::read(key)?;
    let embedded = signing::sign(&pkcs8, &bytes)?;
    let external = match external_key {
        Some(path) => Some(signing::sign(&std::fs::read(path)?, &bytes)?),
        None => None,
    };
    let envelope = SignatureEnvelope {
        schema: 1,
        sha256: signing::sha256_hex(&bytes),
        embedded,
        external,
    };

    // Default output appends ".sig" (with_extension would eat "0" from
    // a name like bun-v0.2.0).
    let sig_path = match out {
        Some(path) => path.to_path_buf(),
        None => {
            let mut name = binary
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            name.push(".sig");
            binary.with_file_name(name)
        }
    };
    envelope.store(&sig_path)?;
    println!("sign-binary: wrote {}", sig_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_lima_yaml_contains_essentials() {
        let yaml = generate_lima_yaml(4, "4GiB");
        assert!(yaml.contains("cpus: 4"));
        assert!(yaml.contains(r#"memory: "4GiB""#));
        assert!(yaml.contains("noble-server-cloudimg"));
        // user-v2 gives VM-to-VM networking with no socket_vmnet / sudo.
        assert!(yaml.contains("lima: user-v2"));
        assert!(yaml.contains("runc"));
        // Binaries are now built locally and copied in — not downloaded.
        assert!(!yaml.contains("github.com/reliaburger"));
        assert!(!yaml.contains("curl -fsSL -o /usr/local/bin"));
    }

    #[test]
    fn agent_launches_in_cluster_mode() {
        // The agent must start with --cluster or it ignores [cluster] config.
        let launch = agent_launch_cmd("runc");
        assert!(launch.contains("--cluster"));
        assert!(launch.contains("--config /etc/reliaburger/node.toml"));
    }

    #[test]
    fn agent_launch_honours_the_chosen_runtime() {
        assert!(agent_launch_cmd("runc").contains("--runtime runc"));
        assert!(agent_launch_cmd("process").contains("--runtime process"));
    }

    #[test]
    fn generate_node_config_first_node_has_empty_join() {
        let config = generate_node_config("dev", "reliaburger-1", "192.168.105.2", None);
        assert!(config.contains("join = []"));
        assert!(config.contains("gossip_port = 9443"));
        assert!(config.contains(r#"name = "reliaburger-1""#));
        assert!(config.contains("[cluster]\nname = \"dev\""));
        // Advertises its own reachable IP so cluster subsystems don't bind loopback.
        assert!(config.contains(r#"advertise_address = "192.168.105.2""#));
    }

    #[test]
    fn generate_node_config_subsequent_node_joins_first() {
        let config = generate_node_config(
            "dev",
            "reliaburger-2",
            "192.168.105.3",
            Some("192.168.105.2"),
        );
        assert!(config.contains(r#"join = ["192.168.105.2:9443"]"#));
        assert!(config.contains(r#"name = "reliaburger-2""#));
        assert!(config.contains(r#"advertise_address = "192.168.105.3""#));
    }

    #[test]
    fn generate_node_config_includes_master_key_path_for_all_nodes() {
        let first = generate_node_config("dev", "reliaburger-1", "192.168.105.2", None);
        let joiner = generate_node_config(
            "dev",
            "reliaburger-2",
            "192.168.105.3",
            Some("192.168.105.2"),
        );
        for config in [first, joiner] {
            assert!(config.contains("[security]"));
            assert!(config.contains(r#"master_key_path = "/etc/reliaburger/master.key""#));
        }
    }

    #[test]
    fn generate_node_config_marks_the_deliberate_development_plaintext_mode() {
        let config = generate_node_config("dev", "reliaburger-1", "192.168.105.2", None);
        assert!(config.contains("DEVELOPMENT-ONLY PLAINTEXT CLUSTER TRANSPORTS"));
        assert!(config.contains("require_mtls = false"));
    }

    #[test]
    fn generate_node_config_includes_bootstrap_path_only_for_first_node() {
        let first = generate_node_config("dev", "reliaburger-1", "192.168.105.2", None);
        assert!(
            first.contains(r#"bootstrap_path = "/etc/reliaburger/security-bootstrap.json""#),
            "bootstrap node must load the security bootstrap"
        );
    }

    #[test]
    fn generate_node_config_omits_bootstrap_path_for_joiners() {
        let joiner = generate_node_config(
            "dev",
            "reliaburger-2",
            "192.168.105.3",
            Some("192.168.105.2"),
        );
        assert!(
            !joiner.contains("bootstrap_path"),
            "joiners get SecurityState from Raft, not a local bootstrap file"
        );
    }

    #[test]
    fn cluster_state_round_trip() {
        let cluster = DevCluster {
            name: "test".to_string(),
            nodes: vec![
                DevNode {
                    name: "reliaburger-1".to_string(),
                    ip: Some("192.168.105.2".to_string()),
                    cpus: 2,
                    memory: "2GiB".to_string(),
                },
                DevNode {
                    name: "reliaburger-2".to_string(),
                    ip: Some("192.168.105.3".to_string()),
                    cpus: 2,
                    memory: "2GiB".to_string(),
                },
            ],
            runtime: "process".to_string(),
        };

        let json = serde_json::to_string(&cluster).unwrap();
        let decoded: DevCluster = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.name, "test");
        assert_eq!(decoded.nodes.len(), 2);
        assert_eq!(decoded.nodes[0].ip.as_deref(), Some("192.168.105.2"));
        assert_eq!(decoded.runtime, "process");
    }

    /// A state file written before the runtime field existed still loads,
    /// defaulting to runc.
    #[test]
    fn cluster_state_without_runtime_defaults_to_runc() {
        let legacy = r#"{"name":"old","nodes":[]}"#;
        let decoded: DevCluster = serde_json::from_str(legacy).unwrap();
        assert_eq!(decoded.runtime, "runc");
    }

    #[test]
    fn test_vm_yaml_has_rust_provisioning() {
        let yaml = generate_test_vm_yaml();
        assert!(yaml.contains("rustup"));
        assert!(yaml.contains("runc"));
    }

    #[test]
    fn yaml_generation_for_different_sizes() {
        for (cpus, mem) in [(1, "1GiB"), (2, "2GiB"), (4, "8GiB")] {
            let yaml = generate_lima_yaml(cpus, mem);
            assert!(yaml.contains(&format!("cpus: {cpus}")));
            assert!(yaml.contains(mem));
        }
    }
}
