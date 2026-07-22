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
    /// Start time of the adopted pid, to detect pid reuse (M23).
    adopted_pid_started_at: Option<u64>,
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

    /// Node-side address that rootful container namespaces use as their
    /// default gateway and DNS resolver endpoint.
    pub fn dns_gateway_address(&self) -> Option<std::net::Ipv4Addr> {
        (!self.rootless).then(|| netns::gateway_ip(self.node_index))
    }

    /// Whether this grill uses rootless runc.
    pub fn is_rootless(&self) -> bool {
        self.rootless
    }

    /// The image store this runtime pulls through. Clones share the
    /// cluster-source slot, so installing a source on the returned
    /// handle affects this grill's pulls.
    pub fn image_store(&self) -> &ImageStore {
        &self.image_store
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
        if let Err(error) = super::rootfs::unmount_bundle(self.bundle_base.join(&instance.0)).await
        {
            eprintln!("warning: rootfs teardown failed for {instance}: {error}");
        }
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

impl RuncGrill {
    async fn prepare(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        let bundle_dir = self.bundle_base.join(&instance.0);
        tokio::fs::create_dir_all(&bundle_dir)
            .await
            .map_err(|e| GrillError::StartFailed {
                instance: instance.clone(),
                reason: format!("failed to create bundle dir: {e}"),
            })?;

        let mut spec = spec.clone();
        let mut rootfs_mount = None;

        if self.rootless && looks_like_image_ref(&spec.root.path) && !spec.root.readonly {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "rootless runc cannot safely isolate a writable image rootfs; use a read-only rootfs or rootful runc"
                    .to_string(),
            });
        }

        if self.rootless && self.dns_nameserver.is_some() {
            return Err(GrillError::StartFailed {
                instance: instance.clone(),
                reason: "rootless runc has no supervised workload DNS path".to_string(),
            });
        }

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
                    // A configured resolver is only reachable through this
                    // namespace. Continuing would create a workload whose
                    // resolv.conf is guaranteed to be broken.
                    if self.dns_nameserver.is_some() {
                        return Err(GrillError::StartFailed {
                            instance: instance.clone(),
                            reason: format!("failed to create DNS-capable container network: {e}"),
                        });
                    }
                    // DNS-disabled workloads retain the historical best-effort
                    // networking behaviour.
                    eprintln!("warning: container network setup failed for {instance}: {e}");
                }
            }
        }

        // If root.path looks like an image reference, pull and unpack it
        if looks_like_image_ref(&spec.root.path) {
            let lower = self
                .image_store
                .pull_and_unpack(&spec.root.path)
                .await
                .map_err(GrillError::ImagePull)?;

            if spec.root.readonly {
                // A read-only OCI root cannot mutate the shared generation, so
                // it is safe to point runc at the immutable lower directly.
                spec.root.path = std::fs::canonicalize(&lower)
                    .unwrap_or(lower)
                    .to_string_lossy()
                    .to_string();
            } else {
                // Writable workloads get a private upper layer. The image
                // generation remains the shared, content-addressed lower.
                let mounted = super::rootfs::mount_private(lower, bundle_dir.clone())
                    .await
                    .map_err(|error| GrillError::StartFailed {
                        instance: instance.clone(),
                        reason: format!("failed to isolate writable image rootfs: {error}"),
                    })?;
                spec.root.path = mounted.path().to_string_lossy().to_string();
                rootfs_mount = Some(mounted);
            }
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

        // Point the container at the node's DNS responder with a per-instance
        // read-only bind mount. Image rootfs directories are shared by every
        // instance of an image, so mutating rootfs/etc/resolv.conf here races
        // concurrent creates and leaks node-local configuration into the
        // unpacked image cache.
        if let Some(nameserver) = self.dns_nameserver {
            let resolv_path = std::fs::canonicalize(&bundle_dir)
                .unwrap_or_else(|_| bundle_dir.clone())
                .join("resolv.conf");
            tokio::fs::write(&resolv_path, resolv_conf_content(nameserver))
                .await
                .map_err(|e| GrillError::StartFailed {
                    instance: instance.clone(),
                    reason: format!("failed to write per-instance resolv.conf: {e}"),
                })?;
            spec.mounts
                .retain(|mount| mount.destination != std::path::Path::new("/etc/resolv.conf"));
            spec.mounts.push(crate::grill::oci::OciMount {
                destination: std::path::PathBuf::from("/etc/resolv.conf"),
                source: Some(resolv_path),
                mount_type: Some("bind".to_string()),
                options: vec!["bind".to_string(), "ro".to_string()],
            });
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
                adopted_pid_started_at: None,
                state: ContainerState::Pending,
                exit_code: None,
            },
        );
        // Keep rollback armed until every fallible preparation step has
        // succeeded and the entry is visible. Unwinding before here also
        // drops the guard and releases the host mount.
        if let Some(mounted) = rootfs_mount {
            mounted.commit();
        }

        Ok(())
    }
}

impl super::Grill for RuncGrill {
    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        let result = self.prepare(instance, spec).await;
        if result.is_err() {
            self.cleanup(instance).await;
        }
        result
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        let result = {
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

            (|| {
                // Redirect stdout and stderr to the per-instance log file.
                let log_file = std::fs::File::create(&entry.log_path).map_err(|e| {
                    GrillError::StartFailed {
                        instance: instance.clone(),
                        reason: format!("failed to create log file: {e}"),
                    }
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
            })()
        };

        if result.is_err() {
            self.cleanup(instance).await;
        }
        result
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
                let (running, exit_code) =
                    super::records::poll_adopted_process(pid, entry.adopted_pid_started_at);
                if !running {
                    just_exited = true;
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = exit_code;
                }
            }
            (entry.state, just_exited)
        };

        // Release all host resources once `runc run` has exited (lock released).
        if just_exited {
            self.cleanup(instance).await;
        }
        Ok(result_state)
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        let (exit_code, just_exited) = {
            let mut entries = self.entries.lock().await;
            let entry = entries.get_mut(instance)?;
            let mut just_exited = false;
            // Reap the child so the exit code is captured even if state() wasn't
            // polled since exit.
            if let Some(ref mut child) = entry.child {
                if let Ok(Some(status)) = child.try_wait() {
                    just_exited = entry.state != ContainerState::Stopped;
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = status.code();
                }
            } else if let Some(pid) = entry.adopted_pid
                && entry.state != ContainerState::Stopped
            {
                let (running, exit_code) =
                    super::records::poll_adopted_process(pid, entry.adopted_pid_started_at);
                if !running {
                    just_exited = true;
                    entry.state = ContainerState::Stopped;
                    entry.exit_code = exit_code;
                }
            }
            (entry.exit_code, just_exited)
        };
        if just_exited {
            self.cleanup(instance).await;
        }
        exit_code
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

    /// Root-mode runc joins the exact cgroup v2 path from the OCI spec's
    /// `cgroupsPath`, so the agent can program egress before `start`.
    /// Rootless runc may not own the cgroup tree — decline there.
    fn honours_cgroup_path(&self) -> bool {
        !self.rootless
    }

    async fn adopt(
        &self,
        instance: &InstanceId,
        record: &super::records::InstanceRecord,
    ) -> Result<bool, GrillError> {
        // The recorded `runc run` process must still be the one we started...
        if !super::records::is_live(record) {
            self.cleanup(instance).await;
            return Ok(false);
        }
        // ...and runc itself must agree the container is running.
        let container_id = record.runc_container_id.as_deref().unwrap_or(&instance.0);
        let output = match self.runc_command(&["state", container_id], instance).await {
            Ok(output) => output,
            Err(error) => {
                self.cleanup(instance).await;
                return Err(error);
            }
        };
        if !output.status.success() {
            self.cleanup(instance).await;
            return Ok(false);
        }
        let running = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .ok()
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(String::from))
            .is_some_and(|status| status == "running");
        if !running {
            self.cleanup(instance).await;
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
                adopted_pid_started_at: Some(record.pid_started_at),
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
        // `--` terminates runc's own option parsing. It must precede the
        // container id; after the id it becomes the command to execute.
        let mut args = vec!["exec", "--", &instance.0];
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
        // Intentionally leave runc and its rootfs mounts alive. Bun records the
        // foreground runc pid and the replacement process adopts it. Runtime
        // lifecycle methods own cleanup; doing it here would turn a Bun restart
        // or self-upgrade into a workload restart.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::Grill;

    fn runc_tests_enabled() -> bool {
        std::env::var("RELIABURGER_RUNC_TESTS").is_ok()
    }

    fn remove_test_network(instance: &InstanceId) {
        let _ = std::process::Command::new("ip")
            .args(["link", "delete", &netns::host_veth_name(instance)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("ip")
            .args(["netns", "delete", &format!("rb-{}", instance.0)])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    struct TestNetworkCleanup(Vec<InstanceId>);

    impl Drop for TestNetworkCleanup {
        fn drop(&mut self) {
            for instance in &self.0 {
                remove_test_network(instance);
            }
        }
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
    async fn rootless_writable_image_fails_before_pull() {
        let tmp = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            ImageStore::new(tmp.path().join("images")),
            true,
            tmp.path().join("state"),
        );
        let id = InstanceId("rootless-writable".to_string());
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: "invalid.example/reliaburger/no-pull:latest".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["/bin/true".to_string()],
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

        let error = grill.create(&id, &spec).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("rootless runc cannot safely isolate a writable image rootfs")
        );
        assert!(!tmp.path().join("images/rootfs").exists());
        assert!(
            !tmp.path()
                .join("bundles/rootless-writable/rootfs-lower")
                .exists()
        );
    }

    #[tokio::test]
    #[ignore = "requires runc and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_grill_creates_bundle_dir() {
        assert!(
            runc_tests_enabled(),
            "set RELIABURGER_RUNC_TESTS=1 after installing runc"
        );

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

    // Validates the run-and-capture model end-to-end: the container's exit code
    // is captured (so jobs don't get retried) and its stdout is readable.
    #[tokio::test]
    #[ignore = "requires runc, network access to a runnable OCI image, and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_netns_resolves_internal_name_through_mounted_resolv_conf() {
        assert!(
            runc_tests_enabled(),
            "set RELIABURGER_RUNC_TESTS=1 after provisioning runc"
        );

        let tmp = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            ImageStore::new(tmp.path().join("images")),
            false,
            tmp.path().join("state"),
        );
        let nameserver = grill.dns_gateway_address().unwrap();
        let grill = grill.with_dns_nameserver(nameserver);

        let mut map = crate::onion::service_map::ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let vip = map
            .resolve(&crate::onion::service_id::ServiceId::new(
                "default", "redis",
            ))
            .unwrap()
            .vip;
        let (_map_tx, map_rx) = tokio::sync::watch::channel(map);
        let (_fault_tx, fault_rx) =
            tokio::sync::watch::channel(crate::onion::dns::DnsFaultState::default());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let responder =
            crate::onion::dns::BoundDnsResponder::bind_freebind(crate::onion::dns::DnsConfig {
                listen_addr: std::net::SocketAddr::new(nameserver.into(), 53),
                upstream: "192.0.2.1:53".parse().unwrap(),
                ..Default::default()
            })
            .expect("bind the future runc gateway before its veth exists");
        let responder_shutdown = shutdown.clone();
        let responder_task = tokio::spawn(responder.run(map_rx, fault_rx, responder_shutdown));

        let ids = [
            InstanceId("runc-dns-netns-0".to_string()),
            InstanceId("runc-dns-netns-1".to_string()),
        ];
        for id in &ids {
            remove_test_network(id);
        }
        let _network_cleanup = TestNetworkCleanup(ids.to_vec());
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: "alpine:latest".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    // Keep both namespaces alive while they query. This catches
                    // duplicate-gateway routing bugs that a single-container
                    // proof cannot see.
                    "sleep 1; cat /etc/resolv.conf; nslookup redis.internal; sleep 2".to_string(),
                ],
                env: vec!["PATH=/usr/sbin:/usr/bin:/sbin:/bin".to_string()],
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

        for id in &ids {
            grill
                .create(id, &spec)
                .await
                .expect("create a rootful runc workload and netns");
            grill.start(id).await.expect("start a DNS client");
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let mut all_stopped = true;
            for id in &ids {
                all_stopped &= matches!(grill.state(id).await, Ok(ContainerState::Stopped));
            }
            if all_stopped {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "DNS client did not exit"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        for id in &ids {
            let logs = grill.logs(id).await.unwrap();
            assert!(
                logs.contains(&format!("nameserver {nameserver}")),
                "{id} did not receive the derived resolver: {logs}"
            );
            assert!(
                logs.contains(&vip.0.to_string()),
                "{id} did not resolve redis.internal to {vip:?}: {logs}"
            );
            assert_eq!(grill.exit_code(id).await, Some(0), "{id} logs: {logs}");
        }

        let config: crate::grill::oci::OciSpec = serde_json::from_slice(
            &std::fs::read(tmp.path().join("bundles/runc-dns-netns-0/config.json")).unwrap(),
        )
        .unwrap();
        let resolver_mount = config
            .mounts
            .iter()
            .find(|mount| mount.destination == std::path::Path::new("/etc/resolv.conf"))
            .expect("per-instance resolver bind mount");
        assert_eq!(
            resolver_mount.source.as_deref(),
            Some(
                tmp.path()
                    .join("bundles/runc-dns-netns-0/resolv.conf")
                    .as_path()
            )
        );

        for id in &ids {
            grill.kill(id).await.ok();
        }
        shutdown.cancel();
        responder_task.await.ok();
    }

    #[tokio::test]
    #[ignore = "requires rootful runc, overlayfs, network access to a runnable OCI image, and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_replicas_cannot_observe_each_others_rootfs_writes() {
        assert!(
            runc_tests_enabled(),
            "set RELIABURGER_RUNC_TESTS=1 after provisioning rootful runc"
        );
        assert_eq!(
            nix::unistd::Uid::effective().as_raw(),
            0,
            "the rootfs overlay acceptance must run as root"
        );

        let tmp = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            ImageStore::new(tmp.path().join("images")),
            false,
            tmp.path().join("state"),
        );
        let ids = [
            InstanceId("runc-rootfs-isolation-0".to_string()),
            InstanceId("runc-rootfs-isolation-1".to_string()),
        ];
        for id in &ids {
            remove_test_network(id);
        }
        let _network_cleanup = TestNetworkCleanup(ids.to_vec());

        let spec_for =
            |value: &str, initial_delay: u64, final_delay: u64| crate::grill::oci::OciSpec {
                port_mapping: None,
                root: crate::grill::oci::OciRoot {
                    path: "alpine:latest".to_string(),
                    readonly: false,
                },
                process: crate::grill::oci::OciProcess {
                    args: vec![
                        "/bin/sh".to_string(),
                        "-c".to_string(),
                        format!(
                            "sleep {initial_delay}; echo {value} > /reliaburger-isolation; \
                             sleep {final_delay}; test \"$(cat /reliaburger-isolation)\" = {value}"
                        ),
                    ],
                    env: vec!["PATH=/usr/sbin:/usr/bin:/sbin:/bin".to_string()],
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
        let specs = [spec_for("alpha", 0, 3), spec_for("beta", 1, 1)];

        for (id, spec) in ids.iter().zip(&specs) {
            grill
                .create(id, spec)
                .await
                .expect("prepare isolated rootfs");
            grill.start(id).await.expect("start rootfs writer");
        }

        let configs: Vec<crate::grill::oci::OciSpec> = ids
            .iter()
            .map(|id| {
                serde_json::from_slice(
                    &std::fs::read(tmp.path().join("bundles").join(&id.0).join("config.json"))
                        .unwrap(),
                )
                .unwrap()
            })
            .collect();
        assert_ne!(
            configs[0].root.path, configs[1].root.path,
            "replicas must not point runc at one writable rootfs"
        );
        let rootfs_paths: Vec<PathBuf> = configs
            .iter()
            .map(|config| PathBuf::from(&config.root.path))
            .collect();
        for rootfs in &rootfs_paths {
            assert!(
                crate::grill::rootfs::is_mountpoint(rootfs),
                "private rootfs must remain mounted while runc uses it"
            );
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let mut all_stopped = true;
            for id in &ids {
                all_stopped &= matches!(grill.state(id).await, Ok(ContainerState::Stopped));
            }
            if all_stopped {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "writers did not exit");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        for id in &ids {
            assert_eq!(
                grill.exit_code(id).await,
                Some(0),
                "{id} observed another replica's write: {}",
                grill.logs(id).await.unwrap()
            );
        }
        for rootfs in &rootfs_paths {
            assert!(
                !crate::grill::rootfs::is_mountpoint(rootfs),
                "natural exit must not leak an overlay mount"
            );
        }

        // Recreating the same instance on the same image generation remounts
        // its private upper. Each replica keeps its own file across restart.
        for ((id, spec), value) in ids.iter().zip(&specs).zip(["alpha", "beta"]) {
            let mut restart = spec.clone();
            restart.process.args = vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                format!("test \"$(cat /reliaburger-isolation)\" = {value}"),
            ];
            grill
                .create(id, &restart)
                .await
                .expect("remount private rootfs");
            grill.start(id).await.expect("restart rootfs reader");
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let mut all_stopped = true;
            for id in &ids {
                all_stopped &= matches!(grill.state(id).await, Ok(ContainerState::Stopped));
            }
            if all_stopped {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "restarted readers did not exit"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        for (id, rootfs) in ids.iter().zip(&rootfs_paths) {
            assert_eq!(grill.exit_code(id).await, Some(0));
            assert!(!crate::grill::rootfs::is_mountpoint(rootfs));
        }
    }

    #[tokio::test]
    #[ignore = "requires rootful runc, overlayfs, network access to an OCI image, and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_create_failure_rolls_back_private_rootfs_mount() {
        assert!(runc_tests_enabled());
        assert_eq!(nix::unistd::Uid::effective().as_raw(), 0);

        let tmp = tempfile::tempdir().unwrap();
        let id = InstanceId("runc-rootfs-create-failure-0".to_string());
        remove_test_network(&id);
        let _network_cleanup = TestNetworkCleanup(vec![id.clone()]);
        let bundle = tmp.path().join("bundles").join(&id.0);
        std::fs::create_dir_all(bundle.join("config.json")).unwrap();
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            ImageStore::new(tmp.path().join("images")),
            false,
            tmp.path().join("state"),
        );
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: "alpine:latest".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["/bin/true".to_string()],
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

        let error = grill.create(&id, &spec).await.unwrap_err();
        assert!(error.to_string().contains("failed to write config.json"));
        assert!(
            !crate::grill::rootfs::is_mountpoint(&bundle.join("rootfs")),
            "a failed create must release its overlay mount"
        );
        assert!(grill.networks.lock().await.get(&id).is_none());
    }

    #[tokio::test]
    #[ignore = "requires rootful runc, overlayfs, network access to a runnable OCI image, and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_adoption_keeps_private_rootfs_and_releases_mount() {
        assert!(
            runc_tests_enabled(),
            "set RELIABURGER_RUNC_TESTS=1 after provisioning rootful runc"
        );
        assert_eq!(nix::unistd::Uid::effective().as_raw(), 0);

        let tmp = tempfile::tempdir().unwrap();
        let bundle_base = tmp.path().join("bundles");
        let image_store = ImageStore::new(tmp.path().join("images"));
        let state_dir = tmp.path().join("state");
        let id = InstanceId("runc-rootfs-adoption-0".to_string());
        remove_test_network(&id);
        let _network_cleanup = TestNetworkCleanup(vec![id.clone()]);
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: "alpine:latest".to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo adopted > /reliaburger-isolation; sleep 30".to_string(),
                ],
                env: vec!["PATH=/usr/sbin:/usr/bin:/sbin:/bin".to_string()],
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

        let original = RuncGrill::new(
            bundle_base.clone(),
            image_store.clone(),
            false,
            state_dir.clone(),
        );
        original.create(&id, &spec).await.unwrap();
        original.start(&id).await.unwrap();
        let pid = original.pid(&id).await.unwrap();
        let started_at = crate::grill::records::process_start_time(pid).unwrap();
        let rootfs = bundle_base.join(&id.0).join("rootfs");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if original
                .exec(
                    &id,
                    &["cat".to_string(), "/reliaburger-isolation".to_string()],
                )
                .await
                .is_ok_and(|output| output.trim() == "adopted")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "workload did not write its private file"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let record = crate::grill::records::InstanceRecord {
            schema: 1,
            instance_id: id.0.clone(),
            namespace: "default".to_string(),
            app_name: "rootfs-adoption".to_string(),
            replica_index: 0,
            is_job: false,
            image: "alpine:latest".to_string(),
            runtime: crate::grill::records::RuntimeKind::Runc,
            pid,
            pid_started_at: started_at,
            runc_container_id: Some(id.0.clone()),
            log_stem: None,
            host_port: None,
            app_spec: None,
            oci_spec: spec,
        };
        drop(original); // A Bun exec/process death drops the child handle only.

        let adopter = RuncGrill::new(bundle_base, image_store, false, state_dir);
        assert!(adopter.adopt(&id, &record).await.unwrap());
        assert!(crate::grill::rootfs::is_mountpoint(&rootfs));
        let contents = adopter
            .exec(
                &id,
                &["cat".to_string(), "/reliaburger-isolation".to_string()],
            )
            .await
            .unwrap();
        assert_eq!(contents.trim(), "adopted");

        adopter.kill(&id).await.unwrap();
        assert!(!crate::grill::rootfs::is_mountpoint(&rootfs));
    }

    #[tokio::test]
    #[ignore = "requires runc, network access to a runnable OCI image, and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_captures_exit_code_and_logs() {
        assert!(
            runc_tests_enabled(),
            "set RELIABURGER_RUNC_TESTS=1 after provisioning runc"
        );

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
                readonly: true,
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

        grill
            .create(&id, &spec)
            .await
            .expect("runc create should succeed in the provisioned Linux suite");
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
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
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
