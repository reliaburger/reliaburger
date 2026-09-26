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

use super::command::RuntimeCommandExecutor;
use super::image::{ImageStore, looks_like_image_ref};
use super::netns::{self, ContainerNetwork, PortMapHandle};
use super::oci::OciSpec;
use super::state::ContainerState;
use super::{GrillError, InstanceId};

mod owned;

/// Runc-based Grill implementation.
///
/// Calls the `runc` binary for each operation. Requires `runc` to be
/// installed and in PATH. Supports rootless mode for non-root users.
#[derive(Clone)]
pub struct RuncGrill {
    /// Base directory for OCI bundles.
    bundle_base: PathBuf,
    /// Independent owners that run every Runc command and outlive Bun.
    ownership: owned::Ownership,
    /// Image store for pulling and caching OCI images.
    image_store: ImageStore,
    /// Whether to run in rootless mode (user namespaces, no sudo).
    rootless: bool,
    /// Runc state directory (`--root` flag). Must be writable by the
    /// current user; in rootless mode this is under $XDG_RUNTIME_DIR.
    state_dir: PathBuf,
    /// Executable used for every invocation, including the foreground launcher.
    runc_program: PathBuf,
    /// Per-container network namespaces (root mode only).
    /// Rootless mode uses slirp4netns instead.
    networks: Arc<Mutex<HashMap<InstanceId, ContainerNetwork>>>,
    /// Active host-port publications, torn down with the container.
    port_handles: Arc<Mutex<HashMap<InstanceId, PortMapHandle>>>,
    /// Node index for IP address assignment (maps to a /23 subnet).
    node_index: u16,
    /// Durable reservations, including incomplete network setup.
    network_leases: super::network_leases::NetworkLeases,
    /// Per-instance lifecycle serialisation prevents stale cleanup deleting a successor.
    lifecycle: Arc<Mutex<HashMap<InstanceId, std::sync::Weak<Mutex<()>>>>>,
    /// Nameserver written into each container's /etc/resolv.conf, so
    /// `.internal` names resolve inside containers. `None` leaves the
    /// image's resolv.conf untouched (host DNS).
    dns_nameserver: Option<std::net::Ipv4Addr>,
    /// Source identities change with network ownership, before workloads start.
    dns_sources: tokio::sync::watch::Sender<crate::onion::dns::DnsSourceNamespaces>,
}

impl RuncGrill {
    /// Create a RuncGrill whose Runc commands run under durable owners.
    ///
    /// `owner_executable` is the Bun binary; it is re-run in a hidden helper
    /// mode as each command's owner. Relative directories are made absolute,
    /// because owners outlive this process and its working directory.
    pub fn new(
        bundle_base: PathBuf,
        image_store: ImageStore,
        rootless: bool,
        state_dir: PathBuf,
        owner_executable: PathBuf,
    ) -> std::io::Result<Self> {
        // Default node index from hostname. Will be overridden when
        // the node joins a cluster and gets a proper node ID.
        let hostname = std::fs::read_to_string("/etc/hostname")
            .unwrap_or_else(|_| "localhost".to_string())
            .trim()
            .to_string();
        let node_index = netns::node_index_from_id(&hostname);

        let bundle_base = std::path::absolute(&bundle_base)?;
        let state_dir = std::path::absolute(&state_dir)?;
        // Rootful containers run in the node's user namespace, so their
        // image files must be owned by ids that namespace maps (D1).
        let image_store = if rootless {
            image_store
        } else {
            image_store.with_owner_shift(super::userns::HOST_ID_BASE)
        };
        Ok(Self {
            network_leases: super::network_leases::NetworkLeases::new(bundle_base.clone()),
            lifecycle: Arc::new(Mutex::new(HashMap::new())),
            bundle_base,
            ownership: owned::Ownership::new(owner_executable),
            image_store,
            rootless,
            state_dir,
            runc_program: PathBuf::from("runc"),
            networks: Arc::new(Mutex::new(HashMap::new())),
            port_handles: Arc::new(Mutex::new(HashMap::new())),
            node_index,
            dns_nameserver: None,
            dns_sources: tokio::sync::watch::channel(
                crate::onion::dns::DnsSourceNamespaces::default(),
            )
            .0,
        })
    }

    async fn lock_lifecycle(&self, instance: &InstanceId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.lifecycle.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(instance).and_then(std::sync::Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(Mutex::new(()));
                    locks.insert(instance.clone(), Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }

    /// Subscribe to namespace identities for this runtime's isolated addresses.
    pub fn dns_source_namespaces(
        &self,
    ) -> tokio::sync::watch::Receiver<crate::onion::dns::DnsSourceNamespaces> {
        self.dns_sources.subscribe()
    }

    /// Publish while holding the network lock so concurrent lifecycle updates
    /// cannot overwrite a newer source snapshot with an older one.
    fn publish_dns_sources(&self, networks: &HashMap<InstanceId, ContainerNetwork>) {
        self.dns_sources
            .send_replace(crate::onion::dns::DnsSourceNamespaces::from_bindings(
                networks.iter().filter_map(|(id, network)| {
                    let identity = super::InstanceIdentity::parse(&id.0)?;
                    Some((network.container_ip.into(), identity.namespace))
                }),
            ));
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
    async fn prepare_with_commands(
        &self,
        instance: &InstanceId,
        spec: &OciSpec,
        container_index: Option<u16>,
        commands: &impl RuntimeCommandExecutor,
    ) -> Result<(), GrillError> {
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
            let container_index = container_index.ok_or_else(|| GrillError::StartFailed {
                instance: instance.clone(),
                reason: "missing durable network reservation".into(),
            })?;

            match netns::setup_container_network_with_commands(
                commands,
                instance,
                self.node_index,
                container_index,
                false,
            )
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

                    // Publish the app's port: host_port on the node DNATs
                    // (root: map element) or proxies (rootless) to the
                    // container. A failure here is fatal (M2): a container whose
                    // published port never listens must not be reported Running,
                    // so fail the prepare instead of just logging.
                    if let Some(pm) = &spec.port_mapping {
                        let handle = netns::add_port_mapping_with_commands(
                            commands,
                            &network,
                            pm.host_port,
                            pm.container_port,
                        )
                        .await
                        .map_err(|e| GrillError::StartFailed {
                            instance: instance.clone(),
                            reason: format!(
                                "port mapping {}->{} failed: {e}",
                                pm.host_port, pm.container_port
                            ),
                        })?;
                        self.port_handles
                            .lock()
                            .await
                            .insert(instance.clone(), handle);
                    }

                    let mut networks = self.networks.lock().await;
                    networks.insert(instance.clone(), network);
                    self.publish_dns_sources(&networks);
                }
                Err(e) => {
                    // The address is reserved and owned; a workload without
                    // its network would hold that reservation for nothing.
                    return Err(GrillError::StartFailed {
                        instance: instance.clone(),
                        reason: format!("failed to create container network: {e}"),
                    });
                }
            }
        }

        // If root.path looks like an image reference, pull and unpack it
        if looks_like_image_ref(&spec.root.path) {
            let pulled = self
                .image_store
                .pull_and_unpack(&spec.root.path)
                .await
                .map_err(GrillError::ImagePull)?;
            let lower = pulled.rootfs;
            self.apply_image_config(instance, &mut spec, &pulled.config, &lower)?;
            if !self.rootless {
                prepare_volumes(instance, &spec).await?;
            }

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
            // Without an image there is no config to resolve against: the
            // process is what the spec says.
            spec.process.overrides = None;
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
            let namespace = super::InstanceIdentity::parse(&instance.0).map(|id| id.namespace);
            let content =
                crate::onion::dns::container_resolv_conf(nameserver, namespace.as_deref());
            tokio::fs::write(&resolv_path, content)
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

        // Keep rollback armed until every fallible preparation step has
        // succeeded. Unwinding before here also drops the guard and releases
        // the host mount.
        if let Some(mounted) = rootfs_mount {
            mounted.commit();
        }

        Ok(())
    }
}

impl RuncGrill {
    /// Resolve the process against the image config, then (rootful) move it
    /// into the node's user namespace and hand it its identity directory.
    fn apply_image_config(
        &self,
        instance: &InstanceId,
        spec: &mut OciSpec,
        config: &super::image_config::ImageConfig,
        rootfs: &std::path::Path,
    ) -> Result<(), GrillError> {
        let failed = |reason: String| GrillError::StartFailed {
            instance: instance.clone(),
            reason,
        };
        super::image_config::resolve_process(&mut spec.process, config, rootfs)
            .map_err(|e| failed(e.to_string()))?;
        if self.rootless {
            return Ok(());
        }
        super::userns::apply(spec).map_err(|e| failed(e.to_string()))?;
        if let Some(identity) =
            super::userns::bind_source(spec, std::path::Path::new(IDENTITY_MOUNT)).cloned()
            && identity.is_dir()
        {
            super::userns::chown_to_container_user(&identity, spec).map_err(|e| {
                failed(format!(
                    "failed to hand identity directory {} to the container user: {e}",
                    identity.display()
                ))
            })?;
        }
        Ok(())
    }
}

/// Where the workload identity directory appears inside a container.
const IDENTITY_MOUNT: &str = "/run/reliaburger/identity";

/// Make every read-write volume usable by the user-namespaced process.
///
/// Managed volumes (the ones with a provisioning sidecar) are handed to
/// the container user. Host directories are the operator's and are never
/// chowned: when the mode bits say the container user can't write to one,
/// Bun warns and starts the container anyway, since it may only read.
/// Rootless containers need none of this: their single mapped id is the
/// user that created the volume.
async fn prepare_volumes(instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
    let user = &spec.process.user;
    let (Some(uid), Some(gid)) = (
        super::userns::host_id(user.uid),
        super::userns::host_id(user.gid),
    ) else {
        // userns::apply already refused ids outside the range.
        return Ok(());
    };
    let wanted = super::volume::VolumeOwner { uid, gid };
    let sources: Vec<PathBuf> = spec
        .mounts
        .iter()
        .filter(|mount| mount.options.iter().any(|option| option == "rw"))
        .filter(|mount| mount.options.iter().any(|option| option == "bind"))
        .filter_map(|mount| mount.source.clone())
        .collect();
    let container_user = format!("{}:{}", user.uid, user.gid);
    let prepared = tokio::task::spawn_blocking(move || {
        for source in sources {
            prepare_volume(&source, wanted, &container_user)?;
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|result| result);
    prepared.map_err(|reason| GrillError::StartFailed {
        instance: instance.clone(),
        reason,
    })
}

/// Hand one managed volume over, or check one host directory.
fn prepare_volume(
    source: &std::path::Path,
    wanted: super::volume::VolumeOwner,
    container_user: &str,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let handed = super::volume::hand_to_container_user(source, wanted).map_err(|e| {
        format!(
            "failed to hand volume {} to container user {container_user} (host {wanted}): {e}",
            source.display()
        )
    })?;
    if handed.is_some() {
        return Ok(());
    }
    // A host path that doesn't exist yet is runc's to report.
    let Ok(metadata) = std::fs::metadata(source) else {
        return Ok(());
    };
    let owner = super::volume::VolumeOwner {
        uid: metadata.uid(),
        gid: metadata.gid(),
    };
    if metadata.is_dir() && !super::volume::container_can_write(owner, metadata.mode(), wanted) {
        eprintln!(
            "warning: host path volume {} is owned by {owner} with mode {:o}; container user \
             {container_user} runs as host {wanted} and can't write to it. Bun never chowns \
             host directories: chown it to {wanted} or make it writable by that user",
            source.display(),
            metadata.mode() & 0o7777,
        );
    }
    Ok(())
}

impl super::Grill for RuncGrill {
    async fn create(&self, instance: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        self.owned_create(instance, spec).await
    }

    async fn start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.owned_start(instance).await
    }

    async fn stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.owned_stop(instance).await
    }

    async fn kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.owned_kill(instance).await
    }

    async fn state(&self, instance: &InstanceId) -> Result<ContainerState, GrillError> {
        self.owned_state(instance).await
    }

    async fn exit_code(&self, instance: &InstanceId) -> Option<i32> {
        self.owned_exit_code(instance).await
    }

    async fn pid(&self, instance: &InstanceId) -> Option<u32> {
        self.owned_pid(instance).await
    }

    async fn workload_cgroup(&self, instance: &InstanceId) -> Result<Option<u64>, GrillError> {
        // Rootless runc may not own its cgroup tree.
        if self.rootless {
            return Ok(None);
        }
        self.owned_workload_cgroup(instance).await
    }

    async fn container_ip(&self, instance: &InstanceId) -> Option<std::net::Ipv4Addr> {
        // Rootful containers publish their isolated address even without a
        // mapped port. Rootless networking has no address in this node pool.
        let networks = self.networks.lock().await;
        networks.get(instance).map(|n| n.container_ip)
    }

    async fn launch_inventory(&self) -> Result<Option<Vec<super::RuntimeLaunch>>, GrillError> {
        self.owned_inventory().await
    }

    async fn log_stem(&self, instance: &InstanceId) -> Option<PathBuf> {
        self.owned_log_stem(instance).await.ok().flatten()
    }

    async fn retain_network_reference(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<super::runc_intent::NetworkReference>, GrillError> {
        if self.rootless {
            return Ok(None);
        }
        self.owned_retain_network_reference(instance)
            .await
            .map(Some)
    }

    async fn network_reference(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<super::runc_intent::NetworkReference>, GrillError> {
        if self.rootless {
            return Ok(None);
        }
        match self.owned_network_reference(instance).await {
            Err(GrillError::NotFound { .. }) => Ok(None),
            result => result,
        }
    }

    async fn release_network_reference(
        &self,
        reference: &super::runc_intent::NetworkReference,
    ) -> Result<(), GrillError> {
        self.owned_release_network_reference(reference).await
    }

    fn runtime_kind(&self) -> super::records::RuntimeKind {
        super::records::RuntimeKind::Runc
    }

    async fn rootless_network_record(
        &self,
        instance: &InstanceId,
    ) -> Option<super::records::RootlessNetworkRecord> {
        self.owned_network_record(instance).await
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
        self.owned_adopt(instance, record).await
    }

    async fn logs(&self, instance: &InstanceId) -> Result<String, GrillError> {
        self.owned_logs(instance).await
    }

    async fn exec(&self, instance: &InstanceId, command: &[String]) -> Result<String, GrillError> {
        self.owned_exec(instance, command).await
    }

    async fn follow_logs(
        &self,
        instance: &InstanceId,
        lines_tx: tokio::sync::mpsc::Sender<crate::ketchup::types::CapturedLine>,
    ) {
        self.owned_follow_logs(instance, lines_tx).await;
    }
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
    use crate::testkit::pinned_images::{ALPINE_IMAGE, NGINX_IMAGE, REDIS_IMAGE};

    /// The Bun binary that owns runtime commands in these tests.
    fn test_owner() -> PathBuf {
        std::env::var_os("RELIABURGER_BUN_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                // Unit tests run from target/<profile>/deps; Bun is built beside it.
                let executable = std::env::current_exe().unwrap();
                executable.parent().unwrap().parent().unwrap().join("bun")
            })
    }

    fn preparation_spec(command: &str) -> OciSpec {
        serde_json::from_value(serde_json::json!({
            "root": {"path": "/", "readonly": true},
            "process": {"args": [command], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
            "mounts": [], "linux": {"namespaces": []}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn duplicate_rootless_create_preserves_preparation_until_retirement() {
        let root = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(
            root.path().join("bundles"),
            ImageStore::new(root.path().join("images")),
            true,
            root.path().join("state"),
            test_owner(),
        )
        .unwrap();
        let id = InstanceId("default__duplicate-0".into());
        let bundle = root.path().join("bundles").join(&id.0).join("config.json");
        grill.create(&id, &preparation_spec("first")).await.unwrap();
        let original = std::fs::read(&bundle).unwrap();
        let replacement = grill.create(&id, &preparation_spec("second")).await;
        let preserved = std::fs::read(&bundle).unwrap() == original;
        grill.kill(&id).await.unwrap();
        grill
            .create(&id, &preparation_spec("second"))
            .await
            .unwrap();
        let replaced = std::fs::read(&bundle).unwrap() != original;
        grill.kill(&id).await.unwrap();
        assert!(
            replacement.is_err(),
            "duplicate create discarded the existing runtime owner"
        );
        assert!(preserved, "duplicate create rewrote the owned bundle");
        assert!(replaced, "confirmed retirement must allow a replacement");
    }

    #[tokio::test]
    async fn create_refuses_existing_oci_state_before_touching_its_bundle() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let id = InstanceId("default__unadopted-0".into());
        std::fs::create_dir_all(state.join(&id.0)).unwrap();
        let bundle = root.path().join("bundles").join(&id.0);
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("config.json"), "original owner").unwrap();
        let grill = RuncGrill::new(
            root.path().join("bundles"),
            ImageStore::new(root.path().join("images")),
            true,
            state.clone(),
            test_owner(),
        )
        .unwrap();
        let result = grill.create(&id, &preparation_spec("replacement")).await;
        assert!(
            result.is_err(),
            "unadopted OCI state must fence a new create"
        );
        assert_eq!(
            std::fs::read_to_string(bundle.join("config.json")).unwrap(),
            "original owner"
        );
        assert!(state.join(&id.0).is_dir());
    }

    /// An image store that pulls the pinned test images from the local test
    /// mirror when the harness provides one (see `testkit::pinned_images`).
    fn pinned_image_store(root: &std::path::Path) -> ImageStore {
        ImageStore::new(root.join("images"))
            .with_mirrors(crate::testkit::pinned_images::local_test_mirrors().unwrap())
    }

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
            pinned_image_store(tmp.path()),
            true,
            tmp.path().join("state"),
            test_owner(),
        )
        .unwrap();
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
                capabilities: None,
                overrides: None,
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
    async fn exhausted_address_pool_refuses_before_creating_a_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle_base = tmp.path().join("bundles");
        std::fs::create_dir_all(&bundle_base).unwrap();
        let grill = RuncGrill::new(
            bundle_base.clone(),
            pinned_image_store(tmp.path()),
            false,
            tmp.path().join("state"),
            test_owner(),
        )
        .unwrap();
        let allocations: std::collections::BTreeMap<String, u16> = (0
            ..netns::MAX_CONTAINERS_PER_NODE)
            .map(|index| (format!("existing-{index}"), index))
            .collect();
        std::fs::write(bundle_base.join(".network-leases.json"), serde_json::to_vec(
            &serde_json::json!({"version": 1, "node_index": grill.node_index, "allocations": allocations})
        ).unwrap()).unwrap();
        let id = InstanceId(format!("capacity-{}", std::process::id()));
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: "./rootfs".into(),
                readonly: true,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["sh".into()],
                env: vec![],
                cwd: "/".into(),
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
        let result = grill.create(&id, &spec).await;
        if result.is_ok() {
            grill.kill(&id).await.unwrap();
        }
        assert!(
            result.is_err(),
            "an exhausted subnet admitted another container"
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("address pool exhausted")
        );
        assert!(!bundle_base.join(&id.0).exists());
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
            pinned_image_store(tmp.path()),
            true,
            state_dir,
            test_owner(),
        )
        .unwrap();
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

        // runc create will fail (no real rootfs), but the bundle dir
        // and config.json should still be written before the runc call.
        let result = grill.create(&id, &spec).await;
        // The bundle dir should exist regardless
        assert!(tmp.path().join("bundles/runc-test-0").exists());
        assert!(tmp.path().join("bundles/runc-test-0/config.json").exists());

        if result.is_ok() {
            grill.kill(&id).await.unwrap();
        }
    }

    // Validates the run-and-capture model end-to-end: the container's exit code
    // is captured (so jobs don't get retried) and its stdout is readable.
    #[tokio::test]
    #[ignore = "requires runc, a pinned OCI image (local test mirror or registry access), and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_netns_resolves_internal_name_through_mounted_resolv_conf() {
        assert!(
            runc_tests_enabled(),
            "set RELIABURGER_RUNC_TESTS=1 after provisioning runc"
        );

        let tmp = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            pinned_image_store(tmp.path()),
            false,
            tmp.path().join("state"),
            test_owner(),
        )
        .unwrap();
        let nameserver = grill.dns_gateway_address().unwrap();
        let grill = grill.with_dns_nameserver(nameserver);

        let mut map = crate::onion::service_map::ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let payments_vip = map.register_app("redis", "payments", 6379, None).unwrap();
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
                source_namespaces: grill.dns_source_namespaces(),
                ..Default::default()
            })
            .expect("bind the future runc gateway before its veth exists");
        let responder_shutdown = shutdown.clone();
        let responder_task = tokio::spawn(responder.run(map_rx, fault_rx, responder_shutdown));

        let ids = [
            InstanceId("default__runc-dns-netns-0".to_string()),
            InstanceId("payments__runc-dns-netns-0".to_string()),
        ];
        for id in &ids {
            remove_test_network(id);
        }
        let _network_cleanup = TestNetworkCleanup(ids.to_vec());
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: ALPINE_IMAGE.to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec![
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    // Keep both namespaces alive while they query. This catches
                    // duplicate-gateway routing bugs that a single-container
                    // proof cannot see.
                    // `getent` resolves through musl's getaddrinfo, which
                    // walks the search list the way applications do (Z1.2).
                    "sleep 1; cat /etc/resolv.conf; nslookup redis.internal; \
                     echo short=$(getent hosts redis | cut -d' ' -f1); \
                     echo qualified=$(getent hosts redis.default | cut -d' ' -f1); \
                     sleep 2"
                        .to_string(),
                ],
                env: vec!["PATH=/usr/sbin:/usr/bin:/sbin:/bin".to_string()],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
                capabilities: None,
                overrides: None,
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

        for (id, expected_vip) in ids.iter().zip([vip, payments_vip]) {
            let logs = grill.logs(id).await.unwrap();
            assert!(
                logs.contains(&format!("nameserver {nameserver}")),
                "{id} did not receive the derived resolver: {logs}"
            );
            assert!(
                logs.contains(&expected_vip.0.to_string()),
                "{id} did not resolve redis.internal to {expected_vip:?}: {logs}"
            );
            // Kubernetes-style names: `redis` stays in the caller's own
            // namespace, `redis.default` names the default one explicitly.
            assert!(
                logs.contains(&format!("short={}", expected_vip.0)),
                "{id} did not resolve the short name redis to {expected_vip:?}: {logs}"
            );
            assert!(
                logs.contains(&format!("qualified={}", vip.0)),
                "{id} did not resolve redis.default to {vip:?}: {logs}"
            );
            assert_eq!(grill.exit_code(id).await, Some(0), "{id} logs: {logs}");
        }

        let config: crate::grill::oci::OciSpec = serde_json::from_slice(
            &std::fs::read(
                tmp.path()
                    .join("bundles")
                    .join(&ids[0].0)
                    .join("config.json"),
            )
            .unwrap(),
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
                    .join("bundles")
                    .join(&ids[0].0)
                    .join("resolv.conf")
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
    #[ignore = "requires rootful runc, overlayfs, a pinned OCI image (local test mirror or registry access), and RELIABURGER_RUNC_TESTS=1"]
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
            pinned_image_store(tmp.path()),
            false,
            tmp.path().join("state"),
            test_owner(),
        )
        .unwrap();
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
                    path: ALPINE_IMAGE.to_string(),
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
                    capabilities: None,
                    overrides: None,
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
    #[ignore = "requires rootful runc, overlayfs, a pinned OCI image (local test mirror or registry access), and RELIABURGER_RUNC_TESTS=1"]
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
            pinned_image_store(tmp.path()),
            false,
            tmp.path().join("state"),
            test_owner(),
        )
        .unwrap();
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: ALPINE_IMAGE.to_string(),
                readonly: false,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["/bin/true".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
                capabilities: None,
                overrides: None,
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
        assert_eq!(
            grill
                .network_leases
                .lookup(&id, grill.node_index)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    #[ignore = "requires rootful runc, overlayfs, a pinned OCI image (local test mirror or registry access), and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_adoption_keeps_private_rootfs_and_releases_mount() {
        assert!(
            runc_tests_enabled(),
            "set RELIABURGER_RUNC_TESTS=1 after provisioning rootful runc"
        );
        assert_eq!(nix::unistd::Uid::effective().as_raw(), 0);

        let tmp = tempfile::tempdir().unwrap();
        let bundle_base = tmp.path().join("bundles");
        let image_store = pinned_image_store(tmp.path());
        let state_dir = tmp.path().join("state");
        let id = InstanceId("payments__runc-rootfs-adoption-0".to_string());
        remove_test_network(&id);
        let _network_cleanup = TestNetworkCleanup(vec![id.clone()]);
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: ALPINE_IMAGE.to_string(),
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
                capabilities: None,
                overrides: None,
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
            test_owner(),
        )
        .unwrap();
        let gateway = original.dns_gateway_address().unwrap();
        let original = original.with_dns_nameserver(gateway);
        let sources = original.dns_source_namespaces();
        original.create(&id, &spec).await.unwrap();
        let container_ip = original.container_ip(&id).await.unwrap();
        assert_eq!(
            sources.borrow().namespace(container_ip.into()),
            Some("payments")
        );
        original.start(&id).await.unwrap();
        assert!(
            original.create(&id, &spec).await.is_err(),
            "duplicate create must refuse before cleanup"
        );
        assert_eq!(original.container_ip(&id).await, Some(container_ip));
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
            schema: 2,
            instance_id: id.0.clone(),
            namespace: "payments".to_string(),
            app_name: "rootfs-adoption".to_string(),
            replica_index: 0,
            is_job: false,
            image: ALPINE_IMAGE.to_string(),
            runtime: crate::grill::records::RuntimeKind::Runc,
            pid,
            pid_started_at: started_at,
            runc_container_id: Some(id.0.clone()),
            // Owned adoption checks the launcher's log identity too.
            log_stem: original.log_stem(&id).await,
            host_port: None,
            app_spec: None,
            oci_spec: spec,
            rootless_network: None,
        };
        drop(original); // A Bun exec/process death drops the child handle only.

        let adopter = RuncGrill::new(bundle_base, image_store, false, state_dir, test_owner())
            .unwrap()
            .with_dns_nameserver(gateway);
        let adopted_sources = adopter.dns_source_namespaces();
        assert!(adopter.adopt(&id, &record).await.unwrap());
        assert_eq!(adopter.container_ip(&id).await, Some(container_ip));
        assert_eq!(
            adopted_sources.borrow().namespace(container_ip.into()),
            Some("payments")
        );
        assert!(crate::grill::rootfs::is_mountpoint(&rootfs));
        let contents = adopter
            .exec(
                &id,
                &["cat".to_string(), "/reliaburger-isolation".to_string()],
            )
            .await
            .unwrap();
        assert_eq!(contents.trim(), "adopted");

        let next_id = InstanceId("payments__runc-rootfs-adoption-1".into());
        let _next_cleanup = TestNetworkCleanup(vec![next_id.clone()]);
        adopter.create(&next_id, &record.oci_spec).await.unwrap();
        assert_ne!(
            adopter.container_ip(&next_id).await,
            Some(container_ip),
            "adoption reused a live source address"
        );
        adopter.start(&next_id).await.unwrap();
        adopter.kill(&next_id).await.unwrap();
        let busy_mount = std::fs::File::open(&rootfs).unwrap();
        let error = adopter.kill(&id).await.unwrap_err();
        assert!(error.to_string().contains("rootfs"), "{error}");
        assert!(crate::grill::rootfs::is_mountpoint(&rootfs));
        // An unfinished teardown may report its error, but never Stopped.
        assert!(!matches!(
            adopter.state(&id).await,
            Ok(ContainerState::Stopped)
        ));
        drop(busy_mount);
        adopter.kill(&id).await.unwrap();
        assert!(
            adopted_sources
                .borrow()
                .namespace(container_ip.into())
                .is_none()
        );
        assert!(!netns::namespace_path(&id).exists());
        assert!(!crate::grill::rootfs::is_mountpoint(&rootfs));
        adopter.create(&next_id, &record.oci_spec).await.unwrap();
        assert_eq!(
            adopter.container_ip(&next_id).await,
            Some(container_ip),
            "confirmed teardown should make the retired address reusable"
        );
        adopter.kill(&next_id).await.unwrap();

        // Batch commands may finish before startup's first OCI state query.
        // Both successful and failed jobs must retain their actual exit status.
        for code in [0, 7] {
            let job_id = InstanceId(format!("payments__runc-short-job-{code}"));
            let _job_cleanup = TestNetworkCleanup(vec![job_id.clone()]);
            let mut job_spec = record.oci_spec.clone();
            job_spec.process.args = vec!["/bin/sh".into(), "-c".into(), format!("exit {code}")];
            adopter.create(&job_id, &job_spec).await.unwrap();
            adopter.start(&job_id).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while adopter.state(&job_id).await.unwrap() != ContainerState::Stopped {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(adopter.exit_code(&job_id).await, Some(code));
            adopter.kill(&job_id).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires runc, a pinned OCI image (local test mirror or registry access), and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_runs_pinned_multiarchitecture_test_workload() {
        assert!(
            runc_tests_enabled(),
            "set RELIABURGER_RUNC_TESTS=1 after provisioning runc"
        );

        let tmp = tempfile::tempdir().unwrap();
        let grill = RuncGrill::new(
            tmp.path().join("bundles"),
            pinned_image_store(tmp.path()),
            true,
            tmp.path().join("state"),
            test_owner(),
        )
        .unwrap();
        let id = InstanceId("runc-pinned-workload".to_string());
        let spec = crate::grill::oci::OciSpec {
            port_mapping: None,
            root: crate::grill::oci::OciRoot {
                path: crate::testkit::PINNED_TEST_WORKLOAD_IMAGE.to_string(),
                readonly: true,
            },
            process: crate::grill::oci::OciProcess {
                args: vec!["/bin/sleep".to_string(), "30".to_string()],
                env: vec![],
                cwd: "/".to_string(),
                user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
                capabilities: None,
                overrides: None,
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
        assert_eq!(grill.state(&id).await.ok(), Some(ContainerState::Running));
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
            .expect("the pinned workload should support exec");
        assert_eq!(
            output, "reliaburger-pinned-workload",
            "unexpected output from the selected platform manifest"
        );

        grill.kill(&id).await.unwrap();
    }

    /// A rootful grill and the spec Bun would generate for `app_toml`.
    fn image_app(tmp: &std::path::Path, id: &InstanceId, app_toml: &str) -> (RuncGrill, OciSpec) {
        let grill = RuncGrill::new(
            tmp.join("bundles"),
            pinned_image_store(tmp),
            false,
            tmp.join("state"),
            test_owner(),
        )
        .unwrap();
        let app: crate::config::app::AppSpec = toml::from_str(app_toml).unwrap();
        let identity = super::super::InstanceIdentity::parse(&id.0).unwrap();
        let volumes = tmp.join("volumes");
        let mut spec = crate::grill::oci::generate_oci_spec(
            &identity.app,
            &identity.namespace,
            &app,
            &id.0,
            None,
            "/unused",
            Some(&volumes),
            None,
        );
        spec.linux.cgroups_path = None;
        (grill, spec)
    }

    async fn wait_for_log(grill: &RuncGrill, id: &InstanceId, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let logs = grill.logs(id).await.unwrap_or_default();
            if logs.contains(needle) {
                return logs;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{id} never logged {needle:?}: {logs}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Z1.1: an app that sets nothing but its image runs the image's own
    /// entrypoint, env and working directory, as the image's user, and
    /// that user is root only inside the container's user namespace.
    #[tokio::test]
    #[ignore = "requires rootful runc, a pinned OCI image (local test mirror or registry access), and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_runs_an_image_by_its_own_entrypoint_env_and_working_dir() {
        assert!(runc_tests_enabled(), "set RELIABURGER_RUNC_TESTS=1");
        assert!(nix::unistd::geteuid().is_root(), "rootful runc needs root");
        let tmp = tempfile::tempdir().unwrap();
        let id = InstanceId("default__image-config-0".to_string());
        remove_test_network(&id);
        let _network_cleanup = TestNetworkCleanup(vec![id.clone()]);
        let (grill, spec) = image_app(
            tmp.path(),
            &id,
            &format!("image = \"{REDIS_IMAGE}\"\nenv = {{ GREETING = \"hello\" }}"),
        );

        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();
        // docker-entrypoint.sh chowns /data and drops to `redis` with gosu:
        // both need capabilities that only exist inside the user namespace.
        wait_for_log(&grill, &id, "Ready to accept connections").await;

        let probe = grill
            .exec(
                &id,
                &[
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "echo \"$REDIS_VERSION|$GREETING|$(pwd)|$(id -u)|$(stat -c %U /data)\"; \
                     grep ^Uid: /proc/1/status; cat /proc/self/uid_map"
                        .to_string(),
                ],
            )
            .await
            .unwrap();
        assert!(
            probe.contains("8.8.0|hello|/data|0|redis"),
            "image env, app env, working dir, image user and image-owned files: {probe}"
        );
        assert!(
            probe.contains("Uid:\t999\t999"),
            "the entrypoint should have dropped redis-server to the redis user: {probe}"
        );
        let mapping: Vec<&str> = probe.lines().last().unwrap().split_whitespace().collect();
        assert_eq!(
            mapping,
            [
                "0",
                crate::grill::userns::HOST_ID_BASE.to_string().as_str(),
                "65536"
            ],
            "container root must map to the node's unprivileged range"
        );

        let bundle: OciSpec = serde_json::from_slice(
            &std::fs::read(tmp.path().join("bundles").join(&id.0).join("config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            bundle.process.args,
            ["docker-entrypoint.sh", "redis-server"]
        );
        assert!(
            bundle.process.overrides.is_none(),
            "config.json is pure OCI"
        );

        grill.kill(&id).await.unwrap();
    }

    /// Z1.1/D1: image root binds port 80 and nginx's workers drop to their
    /// own user, yet no container process is root on the node.
    #[tokio::test]
    #[ignore = "requires rootful runc, a pinned OCI image (local test mirror or registry access), and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_image_root_binds_port_80_without_being_host_root() {
        use std::os::unix::fs::MetadataExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        assert!(runc_tests_enabled(), "set RELIABURGER_RUNC_TESTS=1");
        assert!(nix::unistd::geteuid().is_root(), "rootful runc needs root");
        let tmp = tempfile::tempdir().unwrap();
        let id = InstanceId("default__image-root-0".to_string());
        remove_test_network(&id);
        let _network_cleanup = TestNetworkCleanup(vec![id.clone()]);
        let (grill, spec) = image_app(tmp.path(), &id, &format!("image = \"{NGINX_IMAGE}\""));

        grill.create(&id, &spec).await.unwrap();
        grill.start(&id).await.unwrap();
        wait_for_log(&grill, &id, "start worker process").await;

        let address = grill.container_ip(&id).await.expect("rootful address");
        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::net::TcpStream::connect((address, 80)),
        )
        .await
        .unwrap()
        .expect("nginx listens on port 80");
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: test\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");

        // Find the container's processes by their user namespace, which
        // differs from ours, and check their host uids.
        let own_namespace = std::fs::read_link("/proc/self/ns/user").unwrap();
        let mut container_uids = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(namespace) = std::fs::read_link(entry.path().join("ns/user")) else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            if namespace != own_namespace && cmdline.starts_with(b"nginx") {
                container_uids.push(entry.metadata().unwrap().uid());
            }
        }
        assert!(
            container_uids.len() >= 2,
            "expected an nginx master and worker: {container_uids:?}"
        );
        for uid in container_uids {
            assert!(
                uid >= crate::grill::userns::HOST_ID_BASE,
                "an nginx process runs as host uid {uid}"
            );
        }

        grill.kill(&id).await.unwrap();
    }

    /// Run redis (append-only persistence) over the managed `/data` volume
    /// as instance `id` and wait until it serves.
    async fn start_redis_on_volume(
        tmp: &std::path::Path,
        id: &InstanceId,
        user: &str,
    ) -> RuncGrill {
        let (grill, spec) = image_app(
            tmp,
            id,
            &format!(
                "image = \"{REDIS_IMAGE}\"\nargs = [\"--appendonly\", \"yes\"]\n{user}\n\
                 [[volumes]]\npath = \"/data\"\n"
            ),
        );
        grill.create(id, &spec).await.unwrap();
        grill.start(id).await.unwrap();
        wait_for_log(&grill, id, "Ready to accept connections").await;
        grill
    }

    async fn redis_cli(grill: &RuncGrill, id: &InstanceId, command: &[&str]) -> String {
        let mut argv = vec!["redis-cli".to_string()];
        argv.extend(command.iter().map(|part| part.to_string()));
        grill.exec(id, &argv).await.unwrap().trim().to_string()
    }

    /// Z1.1: user-namespaced redis writes to a managed volume and finds its
    /// data again after a restart, first as the image's `redis` user
    /// directly (no entrypoint chown to help), then as image root after a
    /// user change.
    #[tokio::test]
    #[ignore = "requires rootful runc, a pinned OCI image (local test mirror or registry access), and RELIABURGER_RUNC_TESTS=1"]
    async fn runc_redis_persists_to_a_managed_volume_across_restarts() {
        use std::os::unix::fs::MetadataExt;

        assert!(runc_tests_enabled(), "set RELIABURGER_RUNC_TESTS=1");
        assert!(nix::unistd::geteuid().is_root(), "rootful runc needs root");
        let tmp = tempfile::tempdir().unwrap();
        let ids: Vec<InstanceId> = (0..3)
            .map(|index| InstanceId(format!("default__redis-data-{index}")))
            .collect();
        for id in &ids {
            remove_test_network(id);
        }
        let _network_cleanup = TestNetworkCleanup(ids.clone());
        // Bun provisions the volume before creating the container.
        let volume = crate::grill::volume::VolumeManager::new(tmp.path().join("volumes"))
            .create_managed_volume("default", "redis-data", std::path::Path::new("/data"), None)
            .unwrap();
        assert_eq!(std::fs::metadata(&volume).unwrap().uid(), 0);
        let redis_host_uid = crate::grill::userns::host_id(999).unwrap();
        let as_redis = "run_as_user = 999\nrun_as_group = 999";

        // First start: redis-server runs as 999 from the first instruction,
        // so only the hand-over can make /data writable.
        let grill = start_redis_on_volume(tmp.path(), &ids[0], as_redis).await;
        assert_eq!(
            redis_cli(&grill, &ids[0], &["SET", "burger", "cheese"]).await,
            "OK"
        );
        let root = std::fs::metadata(&volume).unwrap();
        assert_eq!((root.uid(), root.gid()), (redis_host_uid, redis_host_uid));
        assert!(
            volume.join("appendonlydir").is_dir(),
            "redis should persist into the volume"
        );
        grill.kill(&ids[0]).await.unwrap();

        // Restart as the same user: the data is back, ownership untouched.
        let grill = start_redis_on_volume(tmp.path(), &ids[1], as_redis).await;
        assert_eq!(
            redis_cli(&grill, &ids[1], &["GET", "burger"]).await,
            "cheese"
        );
        assert_eq!(
            redis_cli(&grill, &ids[1], &["SET", "bun", "sesame"]).await,
            "OK"
        );
        grill.kill(&ids[1]).await.unwrap();

        // Restart as image root: the volume moves to container root, and
        // docker-entrypoint.sh chowns /data to redis and drops to it.
        let grill = start_redis_on_volume(tmp.path(), &ids[2], "").await;
        assert_eq!(
            redis_cli(&grill, &ids[2], &["GET", "burger"]).await,
            "cheese"
        );
        assert_eq!(redis_cli(&grill, &ids[2], &["GET", "bun"]).await, "sesame");
        assert_eq!(
            redis_cli(&grill, &ids[2], &["SET", "pickle", "yes"]).await,
            "OK"
        );
        let sidecar: serde_json::Value = serde_json::from_slice(
            &std::fs::read(volume.with_file_name("data.volume.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            sidecar["owner"]["uid"],
            crate::grill::userns::HOST_ID_BASE,
            "the volume was last handed to container root: {sidecar}"
        );
        grill.kill(&ids[2]).await.unwrap();
    }
}
