/// Runc-based container runtime (Linux only).
///
/// Implements the `Grill` trait by calling the `runc` CLI directly.
/// Simpler than containerd gRPC — no protobuf, no gRPC client. Just
/// `tokio::process::Command` calling the `runc` binary. This proves
/// the OCI specs we've been generating since day one actually work.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::oci::OciSpec;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// Entry for a runc-managed container.
struct RuncEntry {
    #[allow(dead_code)]
    bundle_dir: PathBuf,
    #[allow(dead_code)]
    spec: OciSpec,
}

/// Runc-based Grill implementation.
///
/// Calls the `runc` binary for each operation. Requires `runc` to be
/// installed and in PATH. Typically requires root privileges.
pub struct RuncGrill {
    /// Base directory for OCI bundles.
    bundle_base: PathBuf,
    entries: Arc<Mutex<HashMap<InstanceId, RuncEntry>>>,
}

impl RuncGrill {
    /// Create a new RuncGrill with the given base directory for bundles.
    pub fn new(bundle_base: PathBuf) -> Self {
        Self {
            bundle_base,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Run a runc command and return its output.
    async fn runc_command(
        &self,
        args: &[&str],
        instance: &InstanceId,
    ) -> Result<std::process::Output, GrillError> {
        let output = tokio::process::Command::new("runc")
            .args(args)
            .output()
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to run runc: {e}"),
            })?;

        Ok(output)
    }
}

impl super::Grill for RuncGrill {
    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        let bundle_dir = self.bundle_base.join(&instance.0);
        tokio::fs::create_dir_all(&bundle_dir)
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to create bundle dir: {e}"),
            })?;

        // Write the OCI spec as config.json
        let spec_json =
            serde_json::to_string_pretty(spec).map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to serialise OCI spec: {e}"),
            })?;
        tokio::fs::write(bundle_dir.join("config.json"), spec_json)
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to write config.json: {e}"),
            })?;

        // Create rootfs directory
        let rootfs = bundle_dir.join("rootfs");
        tokio::fs::create_dir_all(&rootfs)
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to create rootfs: {e}"),
            })?;

        // Call runc create
        let bundle_str = bundle_dir.to_string_lossy().to_string();
        let output = self
            .runc_command(&["create", "--bundle", &bundle_str, &instance.0], instance)
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("runc create failed: {stderr}"),
            });
        }

        let mut entries = self.entries.lock().await;
        entries.insert(
            instance.clone(),
            RuncEntry {
                bundle_dir,
                spec: spec.clone(),
            },
        );

        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let output = self.runc_command(&["start", &instance.0], instance).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("runc start failed: {stderr}"),
            });
        }

        Ok(())
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let output = self
            .runc_command(&["kill", &instance.0, "SIGTERM"], instance)
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("runc kill SIGTERM failed: {stderr}"),
            });
        }

        Ok(())
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let output = self
            .runc_command(&["kill", &instance.0, "SIGKILL"], instance)
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("runc kill SIGKILL failed: {stderr}"),
            });
        }

        Ok(())
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        let output = self.runc_command(&["state", &instance.0], instance).await?;

        if !output.status.success() {
            return Err(GrillError::NotFound {
                instance: instance.clone(),
            });
        }

        let state_json: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to parse runc state: {e}"),
            })?;

        let status = state_json["status"].as_str().unwrap_or("unknown");

        match status {
            "created" => Ok(ContainerState::Pending),
            "running" => Ok(ContainerState::Running),
            "stopped" => Ok(ContainerState::Stopped),
            other => Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("unknown runc state: {other}"),
            }),
        }
    }
}

impl Drop for RuncGrill {
    fn drop(&mut self) {
        // Clean up bundle directories. Best-effort, ignore errors.
        let entries = self.entries.clone();
        // We can't do async cleanup in Drop, so we just log the intent.
        // In production the Bun agent handles cleanup before exit.
        let _ = entries;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::Grill;

    fn runc_tests_enabled() -> bool {
        std::env::var("RELIABURGER_RUNC_TESTS").is_ok()
    }

    #[tokio::test]
    async fn runc_grill_creates_bundle_dir() {
        if !runc_tests_enabled() {
            eprintln!("skipping runc test (set RELIABURGER_RUNC_TESTS=1 to enable)");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(tmp.path().to_path_buf());
        let id = InstanceId("runc-test-0".to_string());
        let spec = crate::grill::oci::OciSpec {
            root: crate::grill::oci::OciRoot {
                path: "rootfs".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["sh".to_string(), "-c".to_string(), "echo hello".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
            },
            mounts: vec![],
            linux: crate::grill::oci::OciLinux {
                namespaces: vec![],
                resources: None,
                cgroups_path: None,
            },
        };

        // This will fail without runc installed, which is expected
        // when tests are not enabled
        let result = grill.create(&id, &spec).await;
        // The bundle dir should exist regardless
        assert!(tmp.path().join("runc-test-0").exists());
        assert!(tmp.path().join("runc-test-0/config.json").exists());

        if result.is_ok() {
            // Clean up runc state
            let _ = grill.runc_command(&["delete", "--force", &id.0], &id).await;
        }
    }
}
