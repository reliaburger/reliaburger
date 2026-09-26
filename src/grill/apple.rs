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
    container_program: std::path::PathBuf,
    /// Deadline for one `container inspect`, after which the CLI is reaped.
    inspection_timeout: std::time::Duration,
}

impl AppleContainerGrill {
    /// Create a new AppleContainerGrill.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            container_program: "container".into(),
            inspection_timeout: std::time::Duration::from_secs(10),
        }
    }

    /// Discover a container's IP address from `container inspect`.
    ///
    /// Apple Container VMs get their IP via vmnet. The inspect output
    /// contains the IP in various possible JSON paths.
    async fn discover_container_ip(&self, instance: &InstanceId) -> Result<Ipv4Addr, GrillError> {
        let inspect = self.inspect_container(instance).await?;

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

    /// Inspect one named container, retaining errors unless absence is explicit.
    async fn inspect_container(
        &self,
        instance: &InstanceId,
    ) -> Result<serde_json::Value, GrillError> {
        let unavailable = |reason: String| GrillError::StateUnavailable {
            instance: instance.clone(),
            reason,
        };
        let output = tokio::time::timeout(
            self.inspection_timeout,
            self.container_command(&["inspect", &instance.0], instance),
        )
        .await
        .map_err(|_| unavailable("container inspection timed out".into()))?
        .map_err(|error| unavailable(error.to_string()))?;
        if !output.status.success() {
            return Err(unavailable(format!(
                "container inspect failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let document: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| unavailable(format!("invalid container inspection: {error}")))?;
        // Apple's CLI returns an empty array for a name absent from a successful
        // daemon inventory. A command failure never establishes that absence.
        let root = match document {
            serde_json::Value::Array(mut entries) if entries.len() == 1 => entries.remove(0),
            serde_json::Value::Array(entries) if entries.is_empty() => {
                return Err(GrillError::NotFound {
                    instance: instance.clone(),
                });
            }
            serde_json::Value::Object(_) => document,
            _ => {
                return Err(unavailable(
                    "expected exactly one inspected container".into(),
                ));
            }
        };
        if root["configuration"]["id"].as_str() != Some(instance.0.as_str()) {
            return Err(unavailable(
                "inspected container identity does not match".into(),
            ));
        }
        Ok(root)
    }

    /// Run a container CLI command and return its output.
    async fn container_command(
        &self,
        args: &[&str],
        instance: &InstanceId,
    ) -> Result<std::process::Output, GrillError> {
        tokio::process::Command::new(&self.container_program)
            .args(args)
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to run container CLI: {e}"),
            })
    }

    fn create_command_args(
        instance: &InstanceId,
        spec: &OciSpec,
    ) -> Result<Vec<String>, GrillError> {
        let invalid = |reason: &str| GrillError::StartFailed {
            instance: instance.clone(),
            reason: reason.to_string(),
        };
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
                if cpu.period == 0
                    || cpu.quota <= 0
                    || !(cpu.quota as u64).is_multiple_of(cpu.period)
                {
                    return Err(invalid(
                        "Apple Container requires a positive whole number of CPUs",
                    ));
                }
                args.push("--cpus".to_string());
                args.push((cpu.quota as u64 / cpu.period).to_string());
            }
        }

        args.extend([
            "--workdir".into(),
            spec.process.cwd.clone(),
            "--user".into(),
            format!("{}:{}", spec.process.user.uid, spec.process.user.gid),
        ]);
        if spec.root.readonly {
            args.push("--read-only".into());
        }
        if let Some(mapping) = spec.port_mapping {
            args.extend([
                "--publish".into(),
                format!(
                    "0.0.0.0:{}:{}/tcp",
                    mapping.host_port, mapping.container_port
                ),
            ]);
        }
        let standard_mounts = super::oci::standard_mounts();
        for mount in &spec.mounts {
            // Apple supplies /proc, /dev and /sys inside its Linux VM.
            if standard_mounts.contains(mount) {
                continue;
            }
            if mount.mount_type.as_deref() != Some("bind")
                || mount
                    .options
                    .iter()
                    .any(|option| !matches!(option.as_str(), "bind" | "rbind" | "ro" | "rw"))
            {
                return Err(invalid(
                    "Apple Container supports bind mounts with ro/rw options only",
                ));
            }
            let Some(source) = mount.source.as_ref().and_then(|path| path.to_str()) else {
                return Err(invalid("Apple Container bind source must be a UTF-8 path"));
            };
            let Some(target) = mount.destination.to_str() else {
                return Err(invalid("Apple Container bind target must be a UTF-8 path"));
            };
            for path in [source, target] {
                if !std::path::Path::new(path).is_absolute() || path.contains([',', '\n', '\r']) {
                    return Err(invalid(
                        "Apple Container mount paths must be absolute and contain no commas or newlines",
                    ));
                }
            }
            let mut mount_arg = format!("type=bind,source={source},target={target}");
            if mount.options.iter().any(|option| option == "ro") {
                mount_arg.push_str(",readonly");
            }
            args.extend(["--mount".into(), mount_arg]);
        }

        // Image
        args.push(spec.root.path.clone());

        // Command args
        for arg in &spec.process.args {
            args.push(arg.clone());
        }

        Ok(args)
    }

    /// Build `container exec` arguments.
    ///
    /// Apple's CLI treats a Docker-style `--` after the container name as the
    /// executable itself. The command therefore follows the name directly.
    fn exec_command_args(instance: &InstanceId, command: &[String]) -> Vec<String> {
        let mut args = vec!["exec".to_string(), instance.0.clone()];
        args.extend(command.iter().cloned());
        args
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
        match self.state(instance).await {
            Ok(ContainerState::Running) => {}
            Ok(ContainerState::Stopped) | Err(GrillError::NotFound { .. }) => return Ok(false),
            Ok(state) => {
                return Err(GrillError::StateUnavailable {
                    instance: instance.clone(),
                    reason: format!("container cannot be adopted while {state:?}"),
                });
            }
            Err(error) => return Err(error),
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
        if let Ok(ip) = self.discover_container_ip(instance).await {
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

        let args = Self::create_command_args(instance, spec)?;

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let output = self.container_command(&args_refs, instance).await?;

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
        let output = self
            .container_command(&["start", &instance.0], instance)
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("container start failed: {stderr}"),
            });
        }

        // Discover the container's IP address from its VM's network interface
        if let Ok(ip) = self.discover_container_ip(instance).await {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(instance) {
                entry.container_ip = Some(ip);
            }
        }

        Ok(())
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let output = self
            .container_command(&["stop", &instance.0], instance)
            .await?;

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
        let output = self
            .container_command(&["kill", &instance.0], instance)
            .await?;

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
        let output = self
            .container_command(&["logs", &instance.0], instance)
            .await?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn exec(&self, instance: &InstanceId, command: &[String]) -> Result<String, GrillError> {
        if command.is_empty() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "no command specified".to_string(),
            });
        }

        let args = Self::exec_command_args(instance, command);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();

        let output = self.container_command(&args, instance).await?;
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
        lines_tx: tokio::sync::mpsc::Sender<crate::ketchup::types::CapturedLine>,
    ) {
        let mut child = match tokio::process::Command::new(&self.container_program)
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
                // `container logs` gives no byte offsets, so a restarted agent
                // re-ingests an adopted Apple container's earlier output.
                let captured = crate::ketchup::types::CapturedLine {
                    stream: crate::ketchup::types::LogStream::Stdout,
                    line,
                    position: None,
                };
                if lines_tx.send(captured).await.is_err() {
                    break;
                }
            }
        }

        let _ = child.kill().await;
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        let inspect_json = self.inspect_container(instance).await?;
        Self::parse_state(&inspect_json, instance)
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        let output = self
            .container_command(&["inspect", &instance.0], instance)
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

    #[tokio::test]
    async fn adoption_requires_positive_runtime_evidence() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("container-fixture");
        std::fs::write(
            &program,
            "#!/bin/sh\ndir=${0%/*}\ncat \"$dir/output\"\nexit \"$(cat \"$dir/status\")\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let id = InstanceId("apple-evidence".into());
        let record = serde_json::from_value(serde_json::json!({
            "schema": 2, "instance_id": id.0, "namespace": "default",
            "app_name": "apple-evidence", "replica_index": 0,
            "is_job": false, "image": "unused", "runtime": "Apple",
            "pid": 0, "pid_started_at": 0,
            "oci_spec": {
                "root": {"path": "unused", "readonly": true},
                "process": {"args": [], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
                "mounts": [], "linux": {"namespaces": []}
            }
        }))
        .unwrap();
        for (status, output) in [
            (1, "daemon unavailable"),
            (0, "invalid json"),
            (0, r#"[{"status":"running"}]"#),
            (
                0,
                r#"[{"configuration":{"id":"apple-evidence"},"status":"unknown"}]"#,
            ),
            (
                0,
                r#"[{"configuration":{"id":"apple-evidence"},"status":"paused"}]"#,
            ),
            (
                0,
                r#"[{"configuration":{"id":"apple-evidence"},"status":"created"}]"#,
            ),
            (
                0,
                r#"[{"configuration":{"id":"someone-else"},"status":"running"}]"#,
            ),
            (0, r#"[{"status":"running"},{"status":"stopped"}]"#),
        ] {
            std::fs::write(directory.path().join("output"), output).unwrap();
            std::fs::write(directory.path().join("status"), status.to_string()).unwrap();
            let mut grill = AppleContainerGrill::new();
            grill.container_program = program.clone();
            assert!(
                grill.adopt(&id, &record).await.is_err(),
                "uncertain inspection was accepted: {status}: {output}"
            );
            assert!(grill.entries.lock().await.is_empty());
        }
        for (output, expected) in [
            ("[]", false),
            (
                r#"[{"configuration":{"id":"apple-evidence"},"status":"stopped"}]"#,
                false,
            ),
            (
                r#"[{"configuration":{"id":"apple-evidence"},"status":"running"}]"#,
                true,
            ),
        ] {
            std::fs::write(directory.path().join("output"), output).unwrap();
            std::fs::write(directory.path().join("status"), "0").unwrap();
            let mut grill = AppleContainerGrill::new();
            grill.container_program = program.clone();
            assert_eq!(
                grill.adopt(&id, &record).await.unwrap(),
                expected,
                "{output}"
            );
            assert_eq!(grill.entries.lock().await.contains_key(&id), expected);
        }
    }

    #[tokio::test]
    async fn stalled_inspection_times_out_and_reaps_the_cli() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("container-fixture");
        std::fs::write(
            &program,
            "#!/bin/sh\ndir=${0%/*}\necho $$ > \"$dir/pid\"\nexec sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut grill = AppleContainerGrill::new();
        grill.container_program = program;
        // The fixture never answers, so two seconds proves the same bound as
        // the production ten while leaving the shell time to record its pid.
        grill.inspection_timeout = std::time::Duration::from_secs(2);
        let started = tokio::time::Instant::now();
        let error = grill
            .state(&InstanceId("stalled-inspection".into()))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("inspection timed out"),
            "{error}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
        let pid = std::fs::read_to_string(directory.path().join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while super::super::records::process_start_time(pid).is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed-out container CLI was left alive");
    }

    #[test]
    fn create_preserves_mounts_identity_ports_and_process_settings() {
        let spec: OciSpec = serde_json::from_value(serde_json::json!({
            "root": {"path":"example:v1", "readonly":true},
            "process":{"args":["/bin/sleep","30"],"env":["KEY=value"],"cwd":"/work","user":{"uid":123,"gid":456}},
            "mounts":[{"destination":"/work","source":"/tmp/source with spaces","type":"bind","options":["bind","ro"]}],
            "linux":{"namespaces":[]},
            "port_mapping":{"host_port":30000,"container_port":8080}
        })).unwrap();
        let args =
            AppleContainerGrill::create_command_args(&InstanceId("apple-settings".into()), &spec)
                .unwrap();
        for pair in [
            ["--workdir", "/work"],
            ["--user", "123:456"],
            ["--publish", "0.0.0.0:30000:8080/tcp"],
            [
                "--mount",
                "type=bind,source=/tmp/source with spaces,target=/work,readonly",
            ],
        ] {
            assert!(
                args.windows(2).any(|args| args == pair),
                "missing {pair:?}: {args:?}"
            );
        }
        assert!(args.iter().any(|arg| arg == "--read-only"));
        assert!(args.ends_with(&["example:v1".into(), "/bin/sleep".into(), "30".into()]));
        for (quota, period) in [(50_000, 100_000), (100_000, 0), (0, 100_000)] {
            let mut fractional = spec.clone();
            fractional.linux.resources = Some(
                serde_json::from_value(serde_json::json!({"cpu":{"quota":quota,"period":period}}))
                    .unwrap(),
            );
            assert!(
                AppleContainerGrill::create_command_args(
                    &InstanceId("apple-cpu".into()),
                    &fractional
                )
                .is_err()
            );
        }
        let mut unsupported = spec.clone();
        unsupported.mounts[0].options.push("nosuid".into());
        assert!(
            AppleContainerGrill::create_command_args(
                &InstanceId("apple-settings".into()),
                &unsupported
            )
            .is_err()
        );
        unsupported = spec;
        unsupported.mounts[0].source = Some("/tmp/source,target=/escape".into());
        assert!(
            AppleContainerGrill::create_command_args(
                &InstanceId("apple-settings".into()),
                &unsupported
            )
            .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "requires Apple Container and RELIABURGER_APPLE_CONTAINER_TESTS=1"]
    async fn apple_serves_a_readonly_bind_mount_with_requested_identity_and_port() {
        assert!(apple_tests_enabled());
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        tokio::fs::write(directory.path().join("index.html"), "apple-settings-work")
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let id = InstanceId(format!("rb-apple-settings-{}", std::process::id()));
        let spec: OciSpec = serde_json::from_value(serde_json::json!({
            "root": {"path":crate::testkit::PINNED_TEST_WORKLOAD_IMAGE.replacen("docker.io/library/", "public.ecr.aws/docker/library/", 1), "readonly":true},
            "process":{"args":["/bin/httpd","-f","-p","8080","-h","/work"],"env":[],"cwd":"/work","user":{"uid":123,"gid":456}},
            "mounts":[{"destination":"/work","source":directory.path(),"type":"bind","options":["bind","ro"]}],
            "linux":{"namespaces":[]},
            "port_mapping":{"host_port":port,"container_port":8080}
        })).unwrap();
        let grill = AppleContainerGrill::new();
        let result: Result<(), String> = async {
            grill.create(&id, &spec).await.map_err(|e| e.to_string())?;
            grill.start(&id).await.map_err(|e| e.to_string())?;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                loop {
                    if let Ok(response) =
                        client.get(format!("http://127.0.0.1:{port}/")).send().await
                        && response.status().is_success()
                        && response
                            .text()
                            .await
                            .is_ok_and(|body| body == "apple-settings-work")
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            })
            .await
            .map_err(|_| "published port did not serve the bind mount".to_string())?;
            let output = grill
                .exec(
                    &id,
                    &[
                        "/bin/sh".into(),
                        "-c".into(),
                        "id -u; id -g; pwd; awk '$2 == \"/\" {print $4}' /proc/mounts".into(),
                    ],
                )
                .await
                .map_err(|e| e.to_string())?;
            let lines: Vec<_> = output.lines().collect();
            if lines.len() < 4
                || lines[..3] != ["123", "456", "/work"]
                || !lines[3].split(',').any(|option| option == "ro")
            {
                return Err(format!("process settings were not applied: {output}"));
            }
            Ok(())
        }
        .await;
        let _ = grill.kill(&id).await;
        let _ = AppleContainerGrill::new()
            .container_command(&["rm", "-f", &id.0], &id)
            .await;
        result.unwrap();
    }

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

    #[test]
    fn exec_command_does_not_insert_a_docker_style_separator() {
        let id = InstanceId("apple-0".to_string());
        let args = AppleContainerGrill::exec_command_args(
            &id,
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf ok".to_string(),
            ],
        );
        assert_eq!(args, ["exec", "apple-0", "/bin/sh", "-c", "printf ok"]);
        assert!(!args.iter().any(|argument| argument == "--"));
    }

    #[tokio::test]
    #[ignore = "requires Apple Container and RELIABURGER_APPLE_CONTAINER_TESTS=1"]
    async fn pinned_test_workload_runs_under_apple_container() {
        assert!(
            apple_tests_enabled(),
            "set RELIABURGER_APPLE_CONTAINER_TESTS=1 after installing and starting Apple Container"
        );

        let grill = AppleContainerGrill::new();
        let id = InstanceId("apple-pinned-workload-0".to_string());
        let _ = AppleContainerGrill::new()
            .container_command(&["rm", "-f", &id.0], &id)
            .await;
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: crate::testkit::PINNED_TEST_WORKLOAD_IMAGE.to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["/bin/sleep".to_string(), "30".to_string()],
                env: vec!["TEST=1".to_string()],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
                capabilities: None,
                overrides: None,
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

        let result: Result<(), String> = async {
            grill
                .create(&id, &spec)
                .await
                .map_err(|error| format!("pinned image create failed: {error}"))?;
            grill
                .start(&id)
                .await
                .map_err(|error| format!("pinned image start failed: {error}"))?;
            if grill.state(&id).await.ok() != Some(ContainerState::Running) {
                return Err("pinned image did not reach running".to_string());
            }
            let output = grill
                .exec(
                    &id,
                    &[
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        "printf reliaburger-pinned-workload".to_string(),
                    ],
                )
                .await
                .map_err(|error| format!("pinned image exec failed: {error}"))?;
            if output != "reliaburger-pinned-workload" {
                return Err(format!("unexpected pinned image output: {output:?}"));
            }
            Ok(())
        }
        .await;

        let _ = grill.kill(&id).await;
        let _ = AppleContainerGrill::new()
            .container_command(&["rm", "-f", &id.0], &id)
            .await;
        result.unwrap_or_else(|reason| panic!("{reason}"));
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
                path: crate::testkit::PINNED_TEST_WORKLOAD_IMAGE.to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["sleep".to_string(), "300".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
                capabilities: None,
                overrides: None,
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
        let _ = AppleContainerGrill::new()
            .container_command(&["rm", "-f", &id.0], &id)
            .await;

        // One grill starts the container; a FRESH grill (as after a bun
        // exec, with empty in-memory state) adopts it.
        let starter = AppleContainerGrill::new();
        starter.create(&id, &spec).await.expect("create");
        starter.start(&id).await.expect("start");

        let record = crate::grill::records::InstanceRecord {
            schema: 2,
            instance_id: id.0.clone(),
            namespace: "default".to_string(),
            app_name: "apple-adopt".to_string(),
            replica_index: 0,
            is_job: false,
            image: crate::testkit::PINNED_TEST_WORKLOAD_IMAGE.to_string(),
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
        let _ = AppleContainerGrill::new()
            .container_command(&["rm", "-f", &id.0], &id)
            .await;
        let after_removal = AppleContainerGrill::new()
            .adopt(&id, &record)
            .await
            .expect("adopt after removal");
        assert!(!after_removal, "a removed container must not be adopted");
    }
}
