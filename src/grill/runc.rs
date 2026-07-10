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
use super::netns::{self, ContainerNetwork, PortMapHandle};
use super::oci::OciSpec;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

/// Entry for a runc-managed container.
///
/// We drive containers with `runc run` (foreground: create + start + wait +
/// delete) rather than detached `runc create`/`start`, so the child process's
/// exit status *is* the container's exit code — the only way to get it from the
/// runc CLI. stdout/stderr are redirected to `log_path` for `logs`/`follow_logs`.
struct RuncEntry {
    bundle_dir: PathBuf,
    /// File the container's stdout+stderr are redirected to.
    log_path: PathBuf,
    /// The `runc run` child, once started.
    child: Option<tokio::process::Child>,
    /// Pid of an adopted `runc run` process (started by a previous bun).
    /// Mutually exclusive with `child`.
    adopted_pid: Option<u32>,
    state: ContainerState,
    exit_code: Option<i32>,
}

/// Runc-based Grill implementation.
///
/// Calls the `runc` binary for each operation. Requires `runc` to be
/// installed and in PATH. Supports rootless mode for non-root users.
#[derive(Clone)]
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
    /// Per-container network namespaces (root mode only).
    /// Rootless mode uses slirp4netns instead.
    networks: Arc<Mutex<HashMap<InstanceId, ContainerNetwork>>>,
    /// Active host-port publications, torn down with the container.
    port_handles: Arc<Mutex<HashMap<InstanceId, PortMapHandle>>>,
    /// Node index for IP address assignment (maps to a /23 subnet).
    node_index: u16,
    /// Counter for assigning container indices within the node's subnet.
    next_container_index: Arc<Mutex<u16>>,
    /// Nameserver written into each container's /etc/resolv.conf, so
    /// `.internal` names resolve inside containers. `None` leaves the
    /// image's resolv.conf untouched (host DNS).
    dns_nameserver: Option<std::net::Ipv4Addr>,
}

impl RuncGrill {
    /// Create a new RuncGrill with the given configuration.
    pub fn new(
        bundle_base: PathBuf,
        image_store: ImageStore,
        rootless: bool,
        state_dir: PathBuf,
    ) -> Self {
        // Default node index from hostname. Will be overridden when
        // the node joins a cluster and gets a proper node ID.
        let hostname = std::fs::read_to_string("/etc/hostname")
            .unwrap_or_else(|_| "localhost".to_string())
            .trim()
            .to_string();
        let node_index = netns::node_index_from_id(&hostname);

        Self {
            bundle_base,
            image_store,
            rootless,
            state_dir,
            entries: Arc::new(Mutex::new(HashMap::new())),
            networks: Arc::new(Mutex::new(HashMap::new())),
            port_handles: Arc::new(Mutex::new(HashMap::new())),
            node_index,
            next_container_index: Arc::new(Mutex::new(0)),
            dns_nameserver: None,
        }
    }

    /// Point containers' `/etc/resolv.conf` at this nameserver.
    ///
    /// The address must be reachable from inside container network
    /// namespaces (i.e. not a host-loopback address like 127.0.0.53 —
    /// use the node's bridge/gateway IP).
    pub fn with_dns_nameserver(mut self, nameserver: std::net::Ipv4Addr) -> Self {
        self.dns_nameserver = Some(nameserver);
        self
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

    /// Release a container's resources: force-delete any lingering runc state
    /// and tear down its network namespace + veth pair. Best-effort — safe to
    /// call more than once. `runc run` auto-deletes on exit, so the delete is a
    /// backstop; the netns teardown is the part that actually prevents leaks.
    async fn cleanup(&self, instance: &InstanceId) {
        let _ = self
            .runc_command(&["delete", "--force", &instance.0], instance)
            .await;
        if let Some(handle) = self.port_handles.lock().await.remove(instance)
            && let Err(e) = handle.shutdown().await
        {
            eprintln!("warning: port mapping teardown failed for {instance}: {e}");
        }
        if let Some(network) = self.networks.lock().await.remove(instance)
            && let Err(e) = netns::teardown_container_network(&network).await
        {
            eprintln!("warning: network teardown failed for {instance}: {e}");
        }
    }
}

/// Read the bytes of `path` from `offset` to end. Returns an empty vec if the
/// file is shorter than `offset` or doesn't exist yet.
async fn read_from_offset(path: &std::path::Path, offset: u64) -> std::io::Result<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = file.metadata().await?.len();
    if offset >= len {
        return Ok(Vec::new());
    }
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = Vec::with_capacity((len - offset) as usize);
    file.read_to_end(&mut buf).await?;
    Ok(buf)
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

        // Set up per-container networking (root mode only, non-rootless).
        // For rootless, slirp4netns is set up after runc create (needs PID).
        if !self.rootless {
            let container_index = {
                let mut idx = self.next_container_index.lock().await;
                let current = *idx;
                *idx = idx.wrapping_add(1);
                current
            };

            match netns::setup_container_network(instance, self.node_index, container_index, false)
                .await
            {
                Ok(network) => {
                    // Update the OCI spec to join our pre-created network namespace
                    let ns_path_str = network.namespace_path.to_string_lossy().to_string();
                    for ns in &mut spec.linux.namespaces {
                        if ns.ns_type == "network" {
                            ns.path = Some(ns_path_str.clone());
                        }
                    }

                    // Publish the app's port: host_port on the node
                    // DNATs (root: map element) or proxies (rootless)
                    // to the container. Failure is logged, not fatal —
                    // matching the network-setup stance above.
                    if let Some(pm) = &spec.port_mapping {
                        match netns::add_port_mapping(&network, pm.host_port, pm.container_port)
                            .await
                        {
                            Ok(handle) => {
                                self.port_handles
                                    .lock()
                                    .await
                                    .insert(instance.clone(), handle);
                            }
                            Err(e) => eprintln!("warning: port mapping failed for {instance}: {e}"),
                        }
                    }

                    self.networks.lock().await.insert(instance.clone(), network);
                }
                Err(e) => {
                    // Network setup failed. Log and continue without isolation —
                    // the container will use an empty new network namespace.
                    eprintln!("warning: container network setup failed for {instance}: {e}");
                }
            }
        }

        // If root.path looks like an image reference, pull and unpack it
        if looks_like_image_ref(&spec.root.path) {
            let rootfs = self
                .image_store
                .pull_and_unpack(&spec.root.path)
                .await
                .map_err(GrillError::ImagePull)?;

            // Point the spec at the unpacked rootfs by its absolute path.
            // `runc run` rejects a relative or symlinked rootfs ("invalid
            // rootfs: not an absolute path, or a symlink"), so we must not
            // symlink it into the bundle as the old create/start path did.
            spec.root.path = std::fs::canonicalize(&rootfs)
                .unwrap_or(rootfs)
                .to_string_lossy()
                .to_string();
        } else {
            // No image to pull — create an empty rootfs directory and point the
            // spec at its absolute path (same rationale as above).
            let rootfs = bundle_dir.join("rootfs");
            tokio::fs::create_dir_all(&rootfs)
                .await
                .map_err(|e| GrillError::StartFailed {
                    instance: instance.clone(),
                    reason: format!("failed to create rootfs: {e}"),
                })?;
            spec.root.path = std::fs::canonicalize(&rootfs)
                .unwrap_or(rootfs)
                .to_string_lossy()
                .to_string();
        }

        // Point the container at the node's DNS responder. The rootfs
        // may be shared between containers of the same image; the
        // content is node-constant, so repeated writes are idempotent.
        if let Some(nameserver) = self.dns_nameserver {
            let resolv_path = std::path::Path::new(&spec.root.path)
                .join("etc")
                .join("resolv.conf");
            if let Some(parent) = resolv_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let content = resolv_conf_content(nameserver);
            if let Err(e) = tokio::fs::write(&resolv_path, content).await {
                eprintln!("warning: failed to write resolv.conf for {instance}: {e}");
            }
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

        // Bundle is prepared; the container is created and started together by
        // `runc run` in `start()`. Record the entry as Pending.
        let log_path = bundle_dir.join("output.log");
        let mut entries = self.entries.lock().await;
        entries.insert(
            instance.clone(),
            RuncEntry {
                bundle_dir,
                log_path,
                child: None,
                adopted_pid: None,
                state: ContainerState::Pending,
                exit_code: None,
            },
        );

        Ok(())
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(instance)
            .ok_or_else(|| GrillError::NotFound {
                instance: instance.clone(),
            })?;

        if entry.child.is_some() || entry.adopted_pid.is_some() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "already started".to_string(),
            });
        }

        // Redirect the container's stdout+stderr to the per-instance log file.
        let log_file =
            std::fs::File::create(&entry.log_path).map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to create log file: {e}"),
            })?;
        let log_file_err = log_file.try_clone().map_err(|e| GrillError::StartFailed {
            instance: instance.clone(),
            reason: format!("failed to clone log file handle: {e}"),
        })?;

        // `runc run` = create + start + wait; its exit status is the
        // container's exit code.
        let state_dir_str = self.state_dir.to_string_lossy().to_string();
        let bundle_str = entry.bundle_dir.to_string_lossy().to_string();
        let child = tokio::process::Command::new("runc")
            .args([
                "--root",
                &state_dir_str,
                "run",
                "--bundle",
                &bundle_str,
                &instance.0,
            ])
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_file_err))
            .spawn()
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to spawn runc run: {e}"),
            })?;

        entry.child = Some(child);
        entry.state = ContainerState::Running;
        Ok(())
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        // Best-effort graceful signal; the container exits and `runc run`
        // returns, which `state()` observes.
        let _ = self
            .runc_command(&["kill", &instance.0, "SIGTERM"], instance)
            .await;
        if let Some(entry) = self.entries.lock().await.get_mut(instance) {
            entry.state = ContainerState::Stopping;
        }
        Ok(())
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let _ = self
            .runc_command(&["kill", &instance.0, "SIGKILL"], instance)
            .await;
        // Also kill the `runc run` process in case the container is unresponsive.
        if let Some(entry) = self.entries.lock().await.get_mut(instance) {
            if let Some(ref mut child) = entry.child {
                let _ = child.kill().await;
            } else if let Some(pid) = entry.adopted_pid {
                let pid = nix::unistd::Pid::from_raw(pid as i32);
                let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            }
        }
        self.cleanup(instance).await;
        if let Some(entry) = self.entries.lock().await.get_mut(instance) {
            entry.state = ContainerState::Stopped;
        }
        Ok(())
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        let (result_state, just_exited) = {
            let mut entries = self.entries.lock().await;
            let entry = entries
                .get_mut(instance)
                .ok_or_else(|| GrillError::NotFound {
                    instance: instance.clone(),
                })?;

            let mut just_exited = false;
            if let Some(ref mut child) = entry.child {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        just_exited = entry.state != ContainerState::Stopped;
                        entry.state = ContainerState::Stopped;
                        entry.exit_code = status.code();
                    }
                    Ok(None) => {}
                    Err(_) => {
                        just_exited = entry.state != ContainerState::Stopped;
                        entry.state = ContainerState::Stopped;
                    }
                }
            } else if let Some(pid) = entry.adopted_pid
                && entry.state != ContainerState::Stopped
            {
                // Adopted `runc run` process: no handle, poll (and reap)
                // by pid. See records::poll_adopted_process.
                let (running, exit_code) = super::records::poll_adopted_process(pid);
                if !running {
                    just_exited = true;
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = exit_code;
                }
            }
            (entry.state, just_exited)
        };

        // Tear down the port mapping and netns for a naturally-exited
        // container (lock released).
        if just_exited {
            if let Some(handle) = self.port_handles.lock().await.remove(instance)
                && let Err(e) = handle.shutdown().await
            {
                eprintln!("warning: port mapping teardown failed for {instance}: {e}");
            }
            if let Some(network) = self.networks.lock().await.remove(instance)
                && let Err(e) = netns::teardown_container_network(&network).await
            {
                eprintln!("warning: network teardown failed for {instance}: {e}");
            }
        }
        Ok(result_state)
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        let mut entries = self.entries.lock().await;
        let entry = entries.get_mut(instance)?;
        // Reap the child so the exit code is captured even if state() wasn't
        // polled since exit.
        if let Some(ref mut child) = entry.child {
            if let Ok(Some(status)) = child.try_wait() {
                entry.state = ContainerState::Stopped;
                entry.exit_code = status.code();
            }
        } else if let Some(pid) = entry.adopted_pid
            && entry.state != ContainerState::Stopped
        {
            let (running, exit_code) = super::records::poll_adopted_process(pid);
            if !running {
                entry.state = ContainerState::Stopped;
                entry.exit_code = exit_code;
            }
        }
        entry.exit_code
    }

    async fn pid(&self, instance: &InstanceId) -> Option<u32> {
        let entries = self.entries.lock().await;
        let entry = entries.get(instance)?;
        entry
            .child
            .as_ref()
            .and_then(|c| c.id())
            .or(entry.adopted_pid)
    }

    async fn container_ip(&self, instance: &InstanceId) -> Option<std::net::Ipv4Addr> {
        // Populated only when a per-container netns was set up (a mapped
        // port on a rootful runc container); rootless/no-port containers
        // have no isolated address and fall back to loopback.
        let networks = self.networks.lock().await;
        networks.get(instance).map(|n| n.container_ip)
    }

    fn runtime_kind(&self) -> super::records::RuntimeKind {
        super::records::RuntimeKind::Runc
    }

    async fn adopt(
        &self,
        instance: &InstanceId,
        record: &super::records::InstanceRecord,
    ) -> Result<bool, GrillError> {
        // The recorded `runc run` process must still be the one we started...
        if !super::records::is_live(record) {
            return Ok(false);
        }
        // ...and runc itself must agree the container is running.
        let container_id = record.runc_container_id.as_deref().unwrap_or(&instance.0);
        let output = self
            .runc_command(&["state", container_id], instance)
            .await?;
        if !output.status.success() {
            return Ok(false);
        }
        let running = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .ok()
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(String::from))
            .is_some_and(|status| status == "running");
        if !running {
            return Ok(false);
        }

        let bundle_dir = self.bundle_base.join(&instance.0);
        let log_path = bundle_dir.join("output.log");
        self.entries.lock().await.insert(
            instance.clone(),
            RuncEntry {
                bundle_dir,
                log_path,
                child: None,
                adopted_pid: Some(record.pid),
                state: ContainerState::Running,
                exit_code: None,
            },
        );

        // Re-track the root-mode port mapping: the map element survived
        // the agent restart in the kernel, so only the handle (whose
        // shutdown deletes the element) needs rebuilding.
        if let Some(pm) = &record.oci_spec.port_mapping {
            self.port_handles.lock().await.insert(
                instance.clone(),
                PortMapHandle::for_adopted(pm.host_port, pm.container_port),
            );
        }

        Ok(true)
    }

    async fn logs(&self, instance: &InstanceId) -> Result<String, GrillError> {
        let log_path = {
            let entries = self.entries.lock().await;
            entries
                .get(instance)
                .ok_or_else(|| GrillError::NotFound {
                    instance: instance.clone(),
                })?
                .log_path
                .clone()
        };
        match tokio::fs::read(&log_path).await {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            // No output yet is not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to read logs: {e}"),
            }),
        }
    }

    async fn exec(&self, instance: &InstanceId, command: &[String]) -> Result<String, GrillError> {
        if command.is_empty() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "no command specified".to_string(),
            });
        }
        let mut args = vec!["exec", &instance.0, "--"];
        for part in command {
            args.push(part.as_str());
        }
        let output = self.runc_command(&args, instance).await?;
        let mut result = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            if !result.is_empty() && !result.ends_with('\n') {
                result.push('\n');
            }
            result.push_str(&stderr);
        }
        Ok(result)
    }

    async fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: tokio::sync::mpsc::Sender<String>,
    ) {
        let log_path = {
            let entries = self.entries.lock().await;
            match entries.get(instance) {
                Some(entry) => entry.log_path.clone(),
                None => return,
            }
        };

        let mut offset = 0u64;
        let mut partial_line = String::new();
        loop {
            if let Ok(bytes) = read_from_offset(&log_path, offset).await
                && !bytes.is_empty()
            {
                offset += bytes.len() as u64;
                partial_line.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(pos) = partial_line.find('\n') {
                    let line = partial_line[..pos].to_string();
                    partial_line = partial_line[pos + 1..].to_string();
                    if lines_tx.send(line).await.is_err() {
                        return;
                    }
                }
            }

            // Stop once the container has exited and the file is fully read.
            let exited = {
                let entries = self.entries.lock().await;
                match entries.get(instance) {
                    Some(entry) => matches!(
                        entry.state,
                        ContainerState::Stopped | ContainerState::Stopping
                    ),
                    None => return,
                }
            };
            if exited
                && read_from_offset(&log_path, offset)
                    .await
                    .map(|b| b.is_empty())
                    .unwrap_or(true)
            {
                if !partial_line.is_empty() {
                    let _ = lines_tx.send(std::mem::take(&mut partial_line)).await;
                }
                return;
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

/// Render the resolv.conf pointing containers at the node's resolver.
///
/// `ndots:0` keeps single-label lookups (like `redis.internal`) from
/// being expanded through search domains first.
fn resolv_conf_content(nameserver: std::net::Ipv4Addr) -> String {
    format!("nameserver {nameserver}\noptions ndots:0\n")
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

    // Runtime-agnostic: exercises the log-tailing primitive used by
    // `logs`/`follow_logs` without needing runc.
    #[tokio::test]
    async fn read_from_offset_tails_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("output.log");

        // Missing file reads as empty, not an error.
        assert!(read_from_offset(&path, 0).await.unwrap().is_empty());

        std::fs::write(&path, b"line one\n").unwrap();
        let first = read_from_offset(&path, 0).await.unwrap();
        assert_eq!(first, b"line one\n");

        // Reading from the end yields nothing until more is appended.
        let offset = first.len() as u64;
        assert!(read_from_offset(&path, offset).await.unwrap().is_empty());

        std::fs::write(&path, b"line one\nline two\n").unwrap();
        let second = read_from_offset(&path, offset).await.unwrap();
        assert_eq!(second, b"line two\n");
    }

    #[tokio::test]
    async fn runc_grill_creates_bundle_dir() {
        if !runc_tests_enabled() {
            eprintln!("skipping runc test (set RELIABURGER_RUNC_TESTS=1 to enable)");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        // Use rootless=true to skip per-container networking setup,
        // which needs real network namespace permissions. This test
        // is about verifying the runc CLI interaction, not networking.
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            ImageStore::new(tmp.path().join("images")),
            true,
            state_dir,
        );
        let id = InstanceId("runc-test-0".to_string());
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                // Use a path (not an image ref) to skip the image pull step
                path: "./rootfs".to_string(),
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

        // runc create will fail (no real rootfs), but the bundle dir
        // and config.json should still be written before the runc call.
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
            port_mapping: None,
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
                namespaces: crate::grill::oci::standard_namespaces(None),
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

    // Validates the run-and-capture model end-to-end: the container's exit code
    // is captured (so jobs don't get retried) and its stdout is readable.
    #[tokio::test]
    async fn runc_captures_exit_code_and_logs() {
        if !runc_tests_enabled() || std::env::var("RELIABURGER_IMAGE_PULL_TESTS").is_err() {
            eprintln!(
                "skipping runc capture test (needs RELIABURGER_RUNC_TESTS=1 + RELIABURGER_IMAGE_PULL_TESTS=1)"
            );
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            ImageStore::new(tmp.path().join("images")),
            true,
            tmp.path().join("state"),
        );
        let id = InstanceId("runc-capture".to_string());
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: "alpine:latest".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                // Absolute path: a bare "sh" needs $PATH, which a hand-built
                // spec doesn't set (real images set PATH via their image config).
                args: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo captured-stdout; exit 7".to_string(),
                ],
                env: vec![],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
            },
            mounts: crate::grill::oci::standard_mounts(),
            linux: crate::grill::oci::OciLinux {
                namespaces: crate::grill::oci::standard_namespaces(None),
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
            },
        };

        if let Err(e) = grill.create(&id, &spec).await {
            eprintln!("runc create failed (expected off-Linux): {e}");
            return;
        }
        grill.start(&id).await.expect("runc run should spawn");

        // Wait for the container to exit.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if matches!(grill.state(&id).await, Ok(ContainerState::Stopped)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "container never exited"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // The log file holds the container's stdout+stderr (or runc's error if
        // the run failed — surfaced in the assert messages).
        let logs = grill.logs(&id).await.unwrap();
        let code = grill.exit_code(&id).await;

        // Exit code is captured (7), and stdout was written to the log file.
        assert_eq!(code, Some(7), "exit code not captured (logs: {logs:?})");
        assert!(
            logs.contains("captured-stdout"),
            "container stdout not captured, got: {logs:?}"
        );

        grill.kill(&id).await.ok();
    }
}
