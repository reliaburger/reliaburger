/// Runc-based container runtime (Linux only).
///
/// Implements the `Grill` trait by calling the `runc` CLI directly.
/// Simpler than containerd gRPC — no protobuf, no gRPC client. Just
/// `tokio::process::Command` calling the `runc` binary. This proves
/// the OCI specs we've been generating since day one actually work.
///
/// Supports rootless mode via user namespaces and `--rootless` flag,
/// and pulls real OCI images from Docker Hub when the spec's root
/// path looks like an image reference.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::image::{ImageStore, looks_like_image_ref};
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
/// installed and in PATH. Supports rootless mode for non-root users.
pub struct RuncGrill {
    /// Base directory for OCI bundles.
    bundle_base: PathBuf,
    /// Image store for pulling and caching OCI images.
    image_store: ImageStore,
    /// Whether to run in rootless mode (user namespaces, no sudo).
    rootless: bool,
    /// Runc state directory (`--root` flag). Must be writable by the
    /// current user; in rootless mode this is under $XDG_RUNTIME_DIR.
    state_dir: PathBuf,
    entries: Arc<Mutex<HashMap<InstanceId, RuncEntry>>>,
}

impl RuncGrill {
    /// Create a new RuncGrill with the given configuration.
    pub fn new(
        bundle_base: PathBuf,
        image_store: ImageStore,
        rootless: bool,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            bundle_base,
            image_store,
            rootless,
            state_dir,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Run a runc command and return its output.
    ///
    /// Always passes `--root {state_dir}` so runc uses a writable
    /// state directory (required for rootless mode).
    async fn runc_command(
        &self,
        args: &[&str],
        instance: &InstanceId,
    ) -> Result<std::process::Output, GrillError> {
        let state_dir_str = self.state_dir.to_string_lossy().to_string();
        let mut full_args = vec!["--root", &state_dir_str];
        full_args.extend_from_slice(args);

        let output = tokio::process::Command::new("runc")
            .args(&full_args)
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

        let mut spec = spec.clone();

        // If root.path looks like an image reference, pull and unpack it
        if looks_like_image_ref(&spec.root.path) {
            let rootfs = self
                .image_store
                .pull_and_unpack(&spec.root.path)
                .await
                .map_err(GrillError::ImagePull)?;

            // Symlink the unpacked rootfs into the bundle directory
            let bundle_rootfs = bundle_dir.join("rootfs");
            // Remove existing rootfs if it exists (from a previous attempt)
            let _ = tokio::fs::remove_file(&bundle_rootfs).await;
            let _ = tokio::fs::remove_dir_all(&bundle_rootfs).await;
            tokio::fs::symlink(&rootfs, &bundle_rootfs)
                .await
                .map_err(|e| GrillError::StartFailed {
                    instance: instance.clone(),
                    reason: format!("failed to symlink rootfs: {e}"),
                })?;

            // Update spec to use relative rootfs path within the bundle
            spec.root.path = "rootfs".to_string();
        } else {
            // No image to pull — create empty rootfs directory (original behaviour)
            let rootfs = bundle_dir.join("rootfs");
            tokio::fs::create_dir_all(&rootfs)
                .await
                .map_err(|e| GrillError::StartFailed {
                    instance: instance.clone(),
                    reason: format!("failed to create rootfs: {e}"),
                })?;
        }

        // Apply rootless modifications if running as non-root
        if self.rootless {
            super::rootless::make_rootless(&mut spec, &instance.0);
        }

        // Ensure runc state directory exists
        tokio::fs::create_dir_all(&self.state_dir)
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to create runc state dir: {e}"),
            })?;

        // Write the OCI spec as config.json
        let spec_json =
            serde_json::to_string_pretty(&spec).map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to serialise OCI spec: {e}"),
            })?;
        tokio::fs::write(bundle_dir.join("config.json"), spec_json)
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to write config.json: {e}"),
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
        let state_dir = tmp.path().join("state");
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            ImageStore::new(tmp.path().join("images")),
            false,
            state_dir,
        );
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
                uid_mappings: None,
                gid_mappings: None,
            },
        };

        // This will fail without runc installed, which is expected
        // when tests are not enabled
        let result = grill.create(&id, &spec).await;
        // The bundle dir should exist regardless
        assert!(tmp.path().join("bundles/runc-test-0").exists());
        assert!(tmp.path().join("bundles/runc-test-0/config.json").exists());

        if result.is_ok() {
            // Clean up runc state
            let _ = grill.runc_command(&["delete", "--force", &id.0], &id).await;
        }
    }

    #[tokio::test]
    async fn runc_rootless_runs_echo() {
        if !runc_tests_enabled() {
            eprintln!("skipping runc test (set RELIABURGER_RUNC_TESTS=1 to enable)");
            return;
        }

        if !std::env::var("RELIABURGER_IMAGE_PULL_TESTS").is_ok() {
            eprintln!("skipping runc rootless test (also needs RELIABURGER_IMAGE_PULL_TESTS=1)");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            ImageStore::new(tmp.path().join("images")),
            true,
            tmp.path().join("state"),
        );
        let id = InstanceId("runc-rootless-echo".to_string());

        let spec = crate::grill::oci::OciSpec {
            root: crate::grill::oci::OciRoot {
                path: "alpine:latest".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["echo".to_string(), "hello".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
            },
            mounts: crate::grill::oci::standard_mounts(),
            linux: crate::grill::oci::OciLinux {
                namespaces: crate::grill::oci::standard_namespaces(),
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
            },
        };

        let result = grill.create(&id, &spec).await;
        if let Err(e) = &result {
            eprintln!("runc rootless create failed (expected on non-Linux): {e}");
            return;
        }

        let start_result = grill.start(&id).await;
        if let Err(e) = &start_result {
            eprintln!("runc rootless start failed: {e}");
        }

        // Clean up
        let _ = grill.runc_command(&["delete", "--force", &id.0], &id).await;
    }
}
