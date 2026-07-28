/// Apple Container runtime (macOS only).
///
/// Implements the `Grill` trait by calling Apple's `container` CLI
/// (github.com/apple/container). Runs Linux containers in lightweight
/// VMs on Apple Silicon. Each VM gets its own vmnet interface with a
/// unique IP address, providing network isolation without us touching
/// any kernel networking.
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::oci::OciSpec;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// Entry for an Apple Container-managed instance.
struct AppleEntry {
    #[allow(dead_code)]
    spec: OciSpec,
    #[allow(dead_code)]
    image: String,
    /// The container's IP address, discovered via `container inspect`.
    /// Set after the container is started.
    container_ip: Option<Ipv4Addr>,
}

/// Apple Container-based Grill implementation.
///
/// Calls the `container` CLI for each operation. Requires Apple's
/// container tool installed and `container system start` to have been run.
/// macOS 15+ on Apple Silicon only.
#[derive(Clone)]
pub struct AppleContainerGrill {
    entries: Arc<Mutex<HashMap<InstanceId, AppleEntry>>>,
}

impl AppleContainerGrill {
    /// Create a new AppleContainerGrill.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Discover a container's IP address from `container inspect`.
    ///
    /// Apple Container VMs get their IP via vmnet. The inspect output
    /// contains the IP in various possible JSON paths.
    async fn discover_container_ip(instance: &InstanceId) -> Result<Ipv4Addr, GrillError> {
        let output = Self::container_command(&["inspect", &instance.0], instance).await?;

        if !output.status.success() {
            return Err(GrillError::NotFound {
                instance: instance.clone(),
            });
        }

        let inspect: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to parse container inspect: {e}"),
            })?;

        Self::parse_container_ip(&inspect).ok_or_else(|| GrillError::StartFailed {
            instance: instance.clone(),
            reason: "no IPv4 address found in container inspect output".to_string(),
        })
    }

    /// Apple's `container inspect` returns a single-element JSON array. Return
    /// the first object so the field lookups work whether the CLI hands us the
    /// array or (in older shapes) a bare object.
    fn inspect_root(inspect_json: &serde_json::Value) -> &serde_json::Value {
        inspect_json
            .as_array()
            .and_then(|entries| entries.first())
            .unwrap_or(inspect_json)
    }

    /// Extract the container's IPv4 address from an inspect document.
    ///
    /// The `container` CLI reports it at `networks[0].ipv4Address` as a CIDR
    /// string (e.g. `192.168.64.3/24`), so we keep only the address part. The
    /// older guessed paths stay as fallbacks in case the schema shifts again.
    /// Pulled out of `discover_container_ip` so it's testable against fixture
    /// JSON without a running Apple Container.
    fn parse_container_ip(inspect_json: &serde_json::Value) -> Option<Ipv4Addr> {
        let root = Self::inspect_root(inspect_json);
        let ip_str = root["networks"][0]["ipv4Address"]
            .as_str()
            .or_else(|| root["NetworkSettings"]["IPAddress"].as_str())
            .or_else(|| root["networkSettings"]["ipAddress"].as_str())
            .or_else(|| root["network"]["ip"].as_str())?;
        // Strip the `/prefix` suffix; vmnet hands out addresses as CIDR.
        let address = ip_str.split('/').next().unwrap_or(ip_str);
        address.parse::<Ipv4Addr>().ok()
    }

    /// Map an Apple `container inspect` JSON document to a [`ContainerState`].
    ///
    /// Apple's CLI reports the status at the top level of each inspect entry
    /// as a lowercase `status` (e.g. `running`, `stopped`). Older/other shapes
    /// (`State.Status`, `state.status`, `Status`) stay as fallbacks. Pulled out
    /// of `state()` so the adoption and status logic can be tested against
    /// fixture JSON without a running Apple Container.
    fn parse_state(
        inspect_json: &serde_json::Value,
        instance: &InstanceId,
    ) -> Result<ContainerState, GrillError> {
        let root = Self::inspect_root(inspect_json);
        let status = root["status"]
            .as_str()
            .or_else(|| root["State"]["Status"].as_str())
            .or_else(|| root["state"]["status"].as_str())
            .or_else(|| root["Status"].as_str())
            .unwrap_or("unknown");

        match status {
            "created" => Ok(ContainerState::Pending),
            "running" => Ok(ContainerState::Running),
            "exited" | "stopped" | "dead" => Ok(ContainerState::Stopped),
            "paused" => Ok(ContainerState::Stopping),
            other => Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("unknown container state: {other}"),
            }),
        }
    }

    /// Run a container CLI command and return its output.
    async fn container_command(
        args: &[&str],
        instance: &InstanceId,
    ) -> Result<std::process::Output, GrillError> {
        tokio::process::Command::new("container")
            .args(args)
            .output()
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to run container CLI: {e}"),
            })
    }
}

impl Default for AppleContainerGrill {
    fn default() -> Self {
        Self::new()
    }
}

impl super::Grill for AppleContainerGrill {
    fn runtime_kind(&self) -> super::records::RuntimeKind {
        super::records::RuntimeKind::Apple
    }

    /// Adopt a running Apple Container instance after a bun exec or restart
    /// (UPG2). Unlike runc/process, an Apple workload runs *inside a VM*
    /// managed by the `container` daemon, not as a child pid of bun, so the
    /// pid-based liveness check doesn't apply. The recoverable handle is the
    /// container itself: `container inspect <id>` reporting `running` means
    /// the VM survived our restart, so we re-track the entry (rebuilt from
    /// the record's OCI spec) and re-discover its IP instead of tearing the
    /// workload down and starting fresh.
    async fn adopt(
        &self,
        instance: &InstanceId,
        record: &super::records::InstanceRecord,
    ) -> Result<bool, GrillError> {
        // The container must still exist and be running for adoption to make
        // sense; otherwise the caller reschedules through the normal path.
        match self.state(instance).await {
            Ok(ContainerState::Running) => {}
            Ok(_) => return Ok(false),
            // NotFound: the VM is gone. Anything else is a real error, but
            // adoption is best-effort, so decline rather than wedge startup.
            Err(_) => return Ok(false),
        }

        let mut entries = self.entries.lock().await;
        entries.insert(
            instance.clone(),
            AppleEntry {
                spec: record.oci_spec.clone(),
                image: record.image.clone(),
                container_ip: None,
            },
        );
        drop(entries);

        // Re-discover the IP the same way `start()` does; a failure here is
        // non-fatal (the container is adopted, service discovery re-resolves
        // it on the next inspect).
        if let Ok(ip) = Self::discover_container_ip(instance).await {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(instance) {
                entry.container_ip = Some(ip);
            }
        }

        Ok(true)
    }

    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        // Extract image from OCI root path. The root.path holds the image
        // reference for Apple Container (it's the OCI image, not a rootfs path).
        let image = spec.root.path.clone();

        let mut args: Vec<String> = vec![
            "create".to_string(),
            "--name".to_string(),
            instance.0.clone(),
        ];

        // Environment variables
        for env_str in &spec.process.env {
            args.push("-e".to_string());
            args.push(env_str.clone());
        }

        // Memory limit
        if let Some(ref resources) = spec.linux.resources {
            if let Some(ref mem) = resources.memory {
                args.push("--memory".to_string());
                args.push(mem.limit.to_string());
            }
            if let Some(ref cpu) = resources.cpu {
                let cpus = cpu.quota as f64 / cpu.period as f64;
                args.push("--cpus".to_string());
                args.push(format!("{cpus:.1}"));
            }
        }

        // Image
        args.push(image.clone());

        // Command args
        for arg in &spec.process.args {
            args.push(arg.clone());
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = Self::container_command(&args_refs, instance).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("container create failed: {stderr}"),
            });
        }

        let mut entries = self.entries.lock().await;
        entries.insert(
            instance.clone(),
            AppleEntry {
                spec: spec.clone(),
                image,
                container_ip: None,
            },
        );

        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let output = Self::container_command(&["start", &instance.0], instance).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("container start failed: {stderr}"),
            });
        }

        // Discover the container's IP address from its VM's network interface
        if let Ok(ip) = Self::discover_container_ip(instance).await {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(instance) {
                entry.container_ip = Some(ip);
            }
        }

        Ok(())
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let output = Self::container_command(&["stop", &instance.0], instance).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("container stop failed: {stderr}"),
            });
        }

        Ok(())
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let output = Self::container_command(&["kill", &instance.0], instance).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("container kill failed: {stderr}"),
            });
        }

        Ok(())
    }

    async fn logs(&self, instance: &InstanceId) -> Result<String, GrillError> {
        let output = Self::container_command(&["logs", &instance.0], instance).await?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn exec(&self, instance: &InstanceId, command: &[String]) -> Result<String, GrillError> {
        if command.is_empty() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "no command specified".to_string(),
            });
        }

        let mut args = vec!["exec", &instance.0, "--"];
        let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();
        args.extend(&cmd_refs);

        let output = Self::container_command(&args, instance).await?;
        let mut result = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                if !result.is_empty() && !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push_str(&stderr);
            }
        }
        Ok(result)
    }

    async fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: tokio::sync::mpsc::Sender<String>,
    ) {
        let mut child = match tokio::process::Command::new("container")
            .args(["logs", "--follow", &instance.0])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return,
        };

        if let Some(stdout) = child.stdout.take() {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = tokio::io::AsyncBufReadExt::lines(reader);
            while let Ok(Some(line)) = lines.next_line().await {
                if lines_tx.send(line).await.is_err() {
                    break;
                }
            }
        }

        let _ = child.kill().await;
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        let output = Self::container_command(&["inspect", &instance.0], instance).await?;

        if !output.status.success() {
            return Err(GrillError::NotFound {
                instance: instance.clone(),
            });
        }

        let inspect_json: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to parse container inspect: {e}"),
            })?;

        Self::parse_state(&inspect_json, instance)
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        let output = Self::container_command(&["inspect", &instance.0], instance)
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let inspect: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
        // Apple's `container inspect` does not currently surface the process
        // exit code, so this normally yields None; we still probe the shapes we
        // might see if a future CLI adds it.
        let root = Self::inspect_root(&inspect);
        root["State"]["ExitCode"]
            .as_i64()
            .or_else(|| root["state"]["exitCode"].as_i64())
            .or_else(|| root["exitCode"].as_i64())
            .or_else(|| root["ExitCode"].as_i64())
            .map(|code| code as i32)
    }

    /// The container IP discovered during `start()` (see `discover_container_ip`).
    async fn container_ip(&self, instance: &InstanceId) -> Option<Ipv4Addr> {
        let entries = self.entries.lock().await;
        entries.get(instance).and_then(|e| e.container_ip)
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::grill::Grill;

    fn apple_tests_enabled() -> bool {
        std::env::var("RELIABURGER_APPLE_CONTAINER_TESTS").is_ok()
    }

    #[test]
    fn parse_state_recognises_running_for_adoption() {
        // The real `container inspect` shape adoption keys off: a JSON array
        // whose entry carries a top-level lowercase `status`. A running
        // container is adoptable, a stopped one is not.
        let id = InstanceId("apple-0".to_string());
        let running = serde_json::json!([{
            "id": "apple-0",
            "status": "running",
            "networks": [{ "ipv4Address": "192.168.64.3/24" }]
        }]);
        assert_eq!(
            AppleContainerGrill::parse_state(&running, &id).unwrap(),
            ContainerState::Running
        );
        let stopped = serde_json::json!([{ "id": "apple-0", "status": "stopped" }]);
        assert_eq!(
            AppleContainerGrill::parse_state(&stopped, &id).unwrap(),
            ContainerState::Stopped
        );
    }

    #[test]
    fn parse_state_accepts_alternate_json_shapes() {
        let id = InstanceId("apple-0".to_string());
        // Bare-object and nested variants kept as fallbacks in case the CLI
        // schema shifts again.
        let lower = serde_json::json!({ "state": { "status": "running" } });
        assert_eq!(
            AppleContainerGrill::parse_state(&lower, &id).unwrap(),
            ContainerState::Running
        );
        let flat = serde_json::json!({ "Status": "created" });
        assert_eq!(
            AppleContainerGrill::parse_state(&flat, &id).unwrap(),
            ContainerState::Pending
        );
    }

    #[test]
    fn parse_container_ip_reads_the_networks_cidr_address() {
        // Real inspect shape: a single-element array with the IPv4 address as
        // CIDR under `networks[0].ipv4Address`. The prefix must be stripped.
        let inspect = serde_json::json!([{
            "id": "apple-0",
            "status": "running",
            "networks": [{
                "ipv4Address": "192.168.64.3/24",
                "ipv4Gateway": "192.168.64.1"
            }]
        }]);
        assert_eq!(
            AppleContainerGrill::parse_container_ip(&inspect),
            Some("192.168.64.3".parse().unwrap())
        );
        // No network information yields None rather than a bogus address.
        let empty = serde_json::json!([{ "id": "apple-0", "status": "running" }]);
        assert_eq!(AppleContainerGrill::parse_container_ip(&empty), None);
    }

    #[test]
    fn parse_state_rejects_unknown_status() {
        let id = InstanceId("apple-0".to_string());
        let weird = serde_json::json!({ "State": { "Status": "banana" } });
        assert!(AppleContainerGrill::parse_state(&weird, &id).is_err());
    }

    #[tokio::test]
    #[ignore = "requires Apple Container and RELIABURGER_APPLE_CONTAINER_TESTS=1"]
    async fn apple_container_grill_creates_instance() {
        assert!(
            apple_tests_enabled(),
            "set RELIABURGER_APPLE_CONTAINER_TESTS=1 after installing and starting Apple Container"
        );

        // This test requires Apple's container CLI installed and running
        let grill = AppleContainerGrill::new();
        let id = InstanceId("apple-test-0".to_string());
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: "alpine:latest".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["echo".to_string(), "hello".to_string()],
                env: vec!["TEST=1".to_string()],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
            },
            mounts: vec![],
            linux: crate::grill::oci::OciLinux {
                namespaces: vec![],
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
            },
        };

        let result = grill.create(&id, &spec).await;
        if let Err(e) = &result {
            eprintln!(
                "Apple container create failed (expected if container CLI not installed): {e}"
            );
        }

        // Clean up
        let _ = AppleContainerGrill::container_command(&["rm", "-f", &id.0], &id).await;
    }

    #[tokio::test]
    #[ignore = "requires Apple Container and RELIABURGER_APPLE_CONTAINER_TESTS=1"]
    async fn adopt_re_tracks_a_running_apple_container() {
        assert!(
            apple_tests_enabled(),
            "set RELIABURGER_APPLE_CONTAINER_TESTS=1 after installing and starting Apple Container"
        );

        let id = InstanceId("apple-adopt-0".to_string());
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: "alpine:latest".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["sleep".to_string(), "300".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
            },
            mounts: vec![],
            linux: crate::grill::oci::OciLinux {
                namespaces: vec![],
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
            },
        };

        // Best-effort cleanup of any leftover from an earlier aborted run, so
        // the test is idempotent (Apple keeps stopped containers around).
        let _ = AppleContainerGrill::container_command(&["rm", "-f", &id.0], &id).await;

        // One grill starts the container; a FRESH grill (as after a bun
        // exec, with empty in-memory state) adopts it.
        let starter = AppleContainerGrill::new();
        starter.create(&id, &spec).await.expect("create");
        starter.start(&id).await.expect("start");

        let record = crate::grill::records::InstanceRecord {
            schema: 1,
            instance_id: id.0.clone(),
            namespace: "default".to_string(),
            app_name: "apple-adopt".to_string(),
            replica_index: 0,
            is_job: false,
            image: "alpine:latest".to_string(),
            runtime: crate::grill::records::RuntimeKind::Apple,
            pid: 0,
            pid_started_at: 0,
            runc_container_id: None,
            log_stem: None,
            host_port: None,
            app_spec: None,
            oci_spec: spec.clone(),
            rootless_network: None,
        };

        let fresh = AppleContainerGrill::new();
        let adopted = fresh.adopt(&id, &record).await.expect("adopt");
        assert!(adopted, "a running Apple container should be adopted");
        assert_eq!(
            fresh.state(&id).await.ok(),
            Some(ContainerState::Running),
            "adopted container should still be running"
        );

        // A vanished container declines adoption.
        let _ = AppleContainerGrill::container_command(&["rm", "-f", &id.0], &id).await;
        let after_removal = AppleContainerGrill::new()
            .adopt(&id, &record)
            .await
            .expect("adopt after removal");
        assert!(!after_removal, "a removed container must not be adopted");
    }
}
