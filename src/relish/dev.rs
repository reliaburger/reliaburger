/// Dev cluster management via Lima VMs.
///
/// Spins up real Ubuntu VMs with rootless runc, gossip networking,
/// and Raft consensus. The Reliaburger equivalent of `minikube start`.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::RelishError;

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

/// Get the IP address of a Lima VM.
async fn get_vm_ip(name: &str) -> Result<String, RelishError> {
    let output = limactl(&["shell", name, "hostname", "-I"]).await?;
    let ip = output.split_whitespace().next().unwrap_or("").to_string();
    if ip.is_empty() {
        return Err(RelishError::LimaError {
            command: format!("get IP for {name}"),
            stderr: "no IP address found".to_string(),
        });
    }
    Ok(ip)
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
  - lima: shared
provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux
      apt-get update -qq
      apt-get install -y -qq runc uidmap curl
      ARCH=$(uname -m)
      VERSION="latest"
      curl -fsSL -o /usr/local/bin/bun \
        "https://github.com/reliaburger/reliaburger/releases/${{VERSION}}/download/bun-linux-${{ARCH}}"
      curl -fsSL -o /usr/local/bin/relish \
        "https://github.com/reliaburger/reliaburger/releases/${{VERSION}}/download/relish-linux-${{ARCH}}"
      chmod +x /usr/local/bin/bun /usr/local/bin/relish
      mkdir -p /etc/reliaburger
"#
    )
}

/// Generate a node.toml config for a cluster node.
fn generate_node_config(
    node_name: &str,
    _node_index: usize,
    first_node_ip: Option<&str>,
) -> String {
    let join = match first_node_ip {
        Some(ip) => format!(r#"join = ["{ip}:9443"]"#),
        None => "join = []".to_string(),
    };

    format!(
        r#"[node]
name = "{node_name}"

[cluster]
{join}
gossip_port = 9443
raft_port = 9444
reporting_port = 9445
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
) -> Result<(), RelishError> {
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
    };

    // Create and start each VM
    for i in 1..=nodes {
        let node_name = format!("reliaburger-{i}");
        println!("  creating {node_name} ({cpus} CPUs, {memory} RAM)...");

        // Write Lima YAML to a temp file
        let yaml_path = std::env::temp_dir().join(format!("{node_name}.yaml"));
        std::fs::write(&yaml_path, &yaml)?;

        limactl(&[
            "create",
            &node_name,
            yaml_path.to_str().unwrap(),
            "--tty=false",
        ])
        .await?;
        limactl(&["start", &node_name]).await?;

        // Clean up temp file
        let _ = std::fs::remove_file(&yaml_path);

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

    // Generate and copy node configs
    println!("  configuring cluster...");
    for (i, node) in cluster.nodes.iter().enumerate() {
        let join_ip = if i == 0 { None } else { Some(first_ip) };
        let config = generate_node_config(&node.name, i, join_ip);

        // Write config to temp file and copy into VM
        let config_path = std::env::temp_dir().join(format!("{}-node.toml", node.name));
        std::fs::write(&config_path, &config)?;
        limactl(&[
            "copy",
            config_path.to_str().unwrap(),
            &format!("{}:/etc/reliaburger/node.toml", node.name),
        ])
        .await?;
        let _ = std::fs::remove_file(&config_path);
    }

    // Start bun agents
    println!("  starting bun agents...");
    for node in &cluster.nodes {
        limactl(&[
            "shell",
            &node.name,
            "bash",
            "-c",
            "nohup bun --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117 --runtime runc > /var/log/bun.log 2>&1 &",
        ])
        .await?;
    }

    // Wait briefly for agents to start
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Print summary
    println!();
    for node in &cluster.nodes {
        let ip = node.ip.as_deref().unwrap_or("unknown");
        let role = if node.name.ends_with("-1") {
            "council, leader"
        } else {
            "council"
        };
        println!("  {}: {ip} ({role})", node.name);
    }

    save_cluster(&cluster)?;

    println!();
    println!("Dev cluster \"{name}\" ready ({nodes} nodes)",);
    println!(
        "Run: relish --agent http://{}:9117 nodes",
        cluster.nodes[0].ip.as_deref().unwrap_or("?")
    );

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

    // Restart bun agents
    for node in &cluster.nodes {
        limactl(&[
            "shell",
            &node.name,
            "bash",
            "-c",
            "nohup bun --config /etc/reliaburger/node.toml --listen 0.0.0.0:9117 --runtime runc > /var/log/bun.log 2>&1 &",
        ])
        .await?;
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
memory: "4GiB"
disk: "20GiB"
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
      apt-get install -y -qq runc uidmap slirp4netns curl build-essential pkg-config libssl-dev clang llvm libbpf-dev linux-headers-$(uname -r)
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
pub async fn test(filter: Option<&str>) -> Result<(), RelishError> {
    if !lima_available() {
        eprintln!("error: limactl not found in PATH");
        eprintln!();
        eprintln!("Install Lima:");
        eprintln!("  macOS:  brew install lima");
        eprintln!("  Linux:  https://lima-vm.io/docs/installation/");
        return Err(RelishError::LimaNotFound);
    }

    // Create the test VM if it doesn't exist
    let list = limactl(&["list", "--format", "{{.Name}}"]).await?;
    if !list.lines().any(|l| l.trim() == TEST_VM_NAME) {
        eprintln!("creating test VM ({TEST_VM_NAME})...");
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
        eprintln!("test VM ready.");
    }

    // Check if it's running
    let info = limactl(&["list", "--format", "{{.Name}} {{.Status}}"]).await?;
    let is_running = info
        .lines()
        .any(|l| l.starts_with(TEST_VM_NAME) && l.contains("Running"));
    if !is_running {
        eprintln!("starting test VM...");
        limactl(&["start", TEST_VM_NAME]).await?;
    }

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

    let mut test_cmd = format!(
        "cd {repo_path} && source $HOME/.cargo/env && \
         mkdir -p {vm_target} && \
         CARGO_TARGET_DIR={vm_target} \
         RELIABURGER_RUNC_TESTS=1 \
         RELIABURGER_NETNS_TESTS=1 \
         RELIABURGER_EBPF_TESTS=1 \
         cargo test --features ebpf"
    );

    if let Some(f) = filter {
        test_cmd.push(' ');
        test_cmd.push_str(f);
    }

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
        assert!(yaml.contains("lima: shared"));
        assert!(yaml.contains("runc"));
        assert!(yaml.contains("github.com/reliaburger"));
    }

    #[test]
    fn generate_node_config_first_node_has_empty_join() {
        let config = generate_node_config("reliaburger-1", 0, None);
        assert!(config.contains("join = []"));
        assert!(config.contains("gossip_port = 9443"));
        assert!(config.contains(r#"name = "reliaburger-1""#));
    }

    #[test]
    fn generate_node_config_subsequent_node_joins_first() {
        let config = generate_node_config("reliaburger-2", 1, Some("192.168.105.2"));
        assert!(config.contains(r#"join = ["192.168.105.2:9443"]"#));
        assert!(config.contains(r#"name = "reliaburger-2""#));
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
        };

        let json = serde_json::to_string(&cluster).unwrap();
        let decoded: DevCluster = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.name, "test");
        assert_eq!(decoded.nodes.len(), 2);
        assert_eq!(decoded.nodes[0].ip.as_deref(), Some("192.168.105.2"));
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
