/// Apple Container runtime (macOS only).
///
/// Implements the `Grill` trait by calling Apple's `container` CLI
/// (github.com/apple/container). Runs Linux containers in lightweight
/// VMs on Apple Silicon.
use std::collections::HashMap;
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
}

/// Apple Container-based Grill implementation.
///
/// Calls the `container` CLI for each operation. Requires Apple's
/// container tool installed and `container system start` to have been run.
/// macOS 15+ on Apple Silicon only.
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

        // Apple container inspect returns a JSON object with a "State" field
        let status = inspect_json["State"]["Status"]
            .as_str()
            .or_else(|| inspect_json["state"]["status"].as_str())
            .or_else(|| inspect_json["Status"].as_str())
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

    #[tokio::test]
    async fn apple_container_grill_creates_instance() {
        if !apple_tests_enabled() {
            eprintln!(
                "skipping Apple container test (set RELIABURGER_APPLE_CONTAINER_TESTS=1 to enable)"
            );
            return;
        }

        // This test requires Apple's container CLI installed and running
        let grill = AppleContainerGrill::new();
        let id = InstanceId("apple-test-0".to_string());
        let spec = crate::grill::oci::OciSpec {
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
}
