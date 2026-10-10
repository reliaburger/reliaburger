//! Workload-selected runtimes with one durable, exclusive routing authority.
use super::records::{InstanceRecord, RootlessNetworkRecord, RuntimeKind};
use super::runc_intent::{NetworkReference, NetworkReferenceState};
use super::{ContainerState, Grill, GrillError, InstanceId, OciSpec, RuntimeLaunch};
use crate::durable::{self, Access};
use crate::file_lock::FileLock;
use std::future::Future;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    schema: u32,
    instance: InstanceId,
    runtime: RuntimeKind,
    committed: bool,
    retired: bool,
}

/// Both backends retain their original ownership journals. This journal only
/// chooses between them; it never authorises a guessed PID or a runtime fallback.
#[derive(Clone)]
pub struct MixedGrill<C, H = super::ProcessGrill> {
    pub(crate) container: C,
    host: H,
    directory: PathBuf,
}
impl<C: Grill + Clone + 'static, H: Grill + Clone + 'static> MixedGrill<C, H> {
    pub fn new(container: C, host: H, directory: PathBuf) -> Self {
        Self {
            container,
            host,
            directory,
        }
    }
    pub fn container(&self) -> &C {
        &self.container
    }
    pub fn with_container(mut self, container: C) -> Self {
        self.container = container;
        self
    }
    fn error(id: &InstanceId, reason: impl std::fmt::Display) -> GrillError {
        GrillError::StateUnavailable {
            instance: id.clone(),
            reason: reason.to_string(),
        }
    }
    fn path(&self, id: &InstanceId) -> io::Result<PathBuf> {
        if id.0.is_empty()
            || id.0.len() > 200
            || !id
                .0
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(io::Error::other("invalid mixed-runtime instance identity"));
        }
        Ok(self.directory.join(format!("{}.json", id.0)))
    }
    /// Route files are tiny, but reading one is still blocking file I/O, so
    /// async callers use [`Self::load`], which runs this on the blocking pool.
    fn load_blocking(&self, id: &InstanceId) -> Result<Option<Route>, GrillError> {
        let path = self.path(id).map_err(|e| Self::error(id, e))?;
        durable::validate_directory(&self.directory).map_err(|e| Self::error(id, e))?;
        let route: Option<Route> = durable::read_json_if_exists(&path, 65536, Access::Exclusive)
            .map_err(|e| Self::error(id, e))?;
        if let Some(route) = &route
            && (route.schema != 1
                || route.instance != *id
                || (route.runtime != RuntimeKind::Process
                    && route.runtime != self.container.runtime_kind()))
        {
            return Err(Self::error(
                id,
                "mixed-runtime route conflicts with its identity or backend",
            ));
        }
        Ok(route)
    }
    async fn load(&self, id: &InstanceId) -> Result<Option<Route>, GrillError> {
        let this = self.clone();
        let owned = id.clone();
        tokio::task::spawn_blocking(move || this.load_blocking(&owned))
            .await
            .map_err(|e| Self::error(id, e))?
    }
    async fn store(&self, route: &Route) -> Result<(), GrillError> {
        let id = &route.instance;
        let bytes = serde_json::to_vec(route).map_err(|e| Self::error(id, e))?;
        let path = self.path(id).map_err(|e| Self::error(id, e))?;
        tokio::task::spawn_blocking(move || {
            crate::sesame::identity::atomic_write_mode(&path, &bytes, Some(0o600))
        })
        .await
        .map_err(|e| Self::error(id, e))?
        .map_err(|e| Self::error(id, e))
    }
    async fn operation<
        T: Send + 'static,
        F: Future<Output = Result<T, GrillError>> + Send + 'static,
    >(
        &self,
        id: &InstanceId,
        operation: impl FnOnce(Self, InstanceId) -> F + Send + 'static,
    ) -> Result<T, GrillError> {
        let this = self.clone();
        let id = id.clone();
        let error_id = id.clone();
        // Detachment preserves this cross-backend claim until the underlying
        // owner operation finishes, even if its caller cancels the future.
        tokio::spawn(async move {
            let directory = this.directory.clone();
            let path = this
                .path(&id)
                .map_err(|e| Self::error(&id, e))?
                .with_extension("lock");
            let lock = tokio::task::spawn_blocking(move || lock_route(&directory, &path))
                .await
                .map_err(|e| Self::error(&id, e))?
                .map_err(|e| Self::error(&id, e))?;
            let result = operation(this, id).await;
            drop(lock);
            result
        })
        .await
        .map_err(|e| Self::error(&error_id, e))?
    }
    /// Delete the route and lock files of an instance whose original backend
    /// has positively retired, after the backend forgets its own journal.
    async fn forget_route(&self, id: &InstanceId) -> Result<(), GrillError> {
        let this = self.clone();
        let id = id.clone();
        let error_id = id.clone();
        // Detached for the same reason as `operation`: the claim must outlive
        // a cancelled caller until the deletion it authorises has finished.
        tokio::spawn(async move {
            let directory = this.directory.clone();
            let route_path = this.path(&id).map_err(|e| Self::error(&id, e))?;
            let path = route_path.with_extension("lock");
            let lock_path = path.clone();
            let lock = tokio::task::spawn_blocking(move || lock_route(&directory, &lock_path))
                .await
                .map_err(|e| Self::error(&id, e))?
                .map_err(|e| Self::error(&id, e))?;
            if let Some(route) = this.load(&id).await? {
                let host = route.runtime == RuntimeKind::Process;
                // Absent from the original backend's inventory, or stopped
                // without a published address: nothing it owns can remain.
                this.prove_retired(&id, host, true).await?;
                if host {
                    this.host.forget_retired(&id).await?;
                } else {
                    this.container.forget_retired(&id).await?;
                }
            }
            let directory = this.directory.clone();
            tokio::task::spawn_blocking(move || {
                match std::fs::remove_file(&route_path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                lock.remove(&path)?;
                std::fs::File::open(&directory)?.sync_all()
            })
            .await
            .map_err(|e| Self::error(&id, e))?
            .map_err(|e| Self::error(&id, e))
        })
        .await
        .map_err(|e| Self::error(&error_id, e))?
    }
    /// Route identities with no launch in either backend: an interrupted
    /// forget, or an instance whose journal its backend already removed.
    async fn orphan_routes(&self) -> Result<Vec<InstanceId>, GrillError> {
        let directory = self.directory.clone();
        let inventory = InstanceId("inventory".into());
        let mut ids = tokio::task::spawn_blocking(move || {
            let mut ids = Vec::new();
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(ids),
                Err(e) => return Err(e),
            };
            for entry in entries {
                let name = entry?.file_name();
                let Some(name) = name.to_str() else { continue };
                if let Some(stem) = name.strip_suffix(".json") {
                    ids.push(InstanceId(stem.to_string()));
                }
            }
            Ok(ids)
        })
        .await
        .map_err(|e| Self::error(&inventory, e))?
        .map_err(|e| Self::error(&inventory, e))?;
        let mut launched = std::collections::HashSet::new();
        for backend in [
            self.container.launch_inventory().await?,
            self.host.launch_inventory().await?,
        ] {
            let backend = backend
                .ok_or_else(|| Self::error(&inventory, "backend inventory is unavailable"))?;
            launched.extend(backend.into_iter().map(|launch| launch.instance_id));
        }
        ids.retain(|id| !launched.contains(id));
        Ok(ids)
    }
    fn selected_blocking(&self, id: &InstanceId) -> Result<Route, GrillError> {
        self.load_blocking(id)?
            .ok_or_else(|| Self::error(id, "mixed-runtime route is absent"))
    }
    async fn selected(&self, id: &InstanceId) -> Result<Route, GrillError> {
        self.load(id)
            .await?
            .ok_or_else(|| Self::error(id, "mixed-runtime route is absent"))
    }
    async fn backend_state(
        &self,
        id: &InstanceId,
        host: bool,
    ) -> Result<ContainerState, GrillError> {
        if host {
            self.host.state(id).await
        } else {
            self.container.state(id).await
        }
    }
    async fn backend_absent(&self, id: &InstanceId, host: bool) -> Result<bool, GrillError> {
        let inventory = if host {
            self.host.launch_inventory().await?
        } else {
            self.container.launch_inventory().await?
        };
        let inventory = inventory
            .ok_or_else(|| Self::error(id, "original backend inventory is unavailable"))?;
        Ok(!inventory.iter().any(|launch| launch.instance_id == *id))
    }
    async fn absent_error(&self, id: &InstanceId) -> GrillError {
        match (
            self.backend_absent(id, true).await,
            self.backend_absent(id, false).await,
        ) {
            (Ok(true), Ok(true)) => GrillError::NotFound {
                instance: id.clone(),
            },
            (Err(e), _) | (_, Err(e)) => e,
            _ => Self::error(
                id,
                "runtime ownership exists without its original backend route",
            ),
        }
    }
    async fn prove_retired(
        &self,
        id: &InstanceId,
        host: bool,
        preparing: bool,
    ) -> Result<(), GrillError> {
        if preparing && self.backend_absent(id, host).await? {
            return Ok(());
        }
        match self.backend_state(id, host).await {
            Ok(ContainerState::Stopped) => {}
            Ok(_) => {
                return Err(Self::error(
                    id,
                    "previous backend has not positively retired",
                ));
            }
            Err(e) => return Err(e),
        }
        if !host && self.container.network_reference(id).await?.is_some() {
            return Err(Self::error(
                id,
                "previous container still owns a published address",
            ));
        }
        Ok(())
    }
}
/// Take the exclusive claim on one route. Only `forget_route` unlinks a lock
/// file, and only while holding it, so a file unlinked after we opened it
/// authorises nothing: lock the current one instead.
fn lock_route(directory: &std::path::Path, path: &std::path::Path) -> io::Result<FileLock> {
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
    {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    durable::validate_directory(directory)?;
    loop {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)?;
        durable::validate_file(&file, Access::Exclusive)?;
        let lock = FileLock::lock_within(file, Duration::from_secs(30)).map_err(io::Error::from)?;
        if lock.still_names(path)? {
            return Ok(lock);
        }
    }
}
impl<C: Grill + Clone + 'static, H: Grill + Clone + 'static> Grill for MixedGrill<C, H> {
    async fn create(&self, id: &InstanceId, spec: &OciSpec) -> Result<(), GrillError> {
        let spec = spec.clone();
        self.operation(id, move |this, id| async move {
            let host = spec.host_process;
            if host && (spec.reusable_executor || spec.linux.resources.is_some()) {
                return Err(Self::error(
                    &id,
                    "host commands cannot enforce container isolation or cpu/memory limits",
                ));
            }
            let runtime = if host {
                RuntimeKind::Process
            } else {
                this.container.runtime_kind()
            };
            match this.load(&id).await? {
                Some(old) if old.runtime != runtime => {
                    this.prove_retired(
                        &id,
                        old.runtime == RuntimeKind::Process,
                        !old.committed || old.retired,
                    )
                    .await?;
                }
                None => {
                    // A lost routing journal must not erase either original
                    // runtime's ownership. Consult both complete inventories
                    // before creating another owner, even on the same backend.
                    this.prove_retired(&id, true, true).await?;
                    this.prove_retired(&id, false, true).await?;
                }
                Some(_) => {}
            }
            let mut route = Route {
                schema: 1,
                instance: id.clone(),
                runtime,
                committed: false,
                retired: false,
            };
            this.store(&route).await?;
            if host {
                this.host.create(&id, &spec).await?;
            } else {
                this.container.create(&id, &spec).await?;
            }
            route.committed = true;
            this.store(&route).await
        })
        .await
    }
    async fn start(&self, id: &InstanceId) -> Result<(), GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if !route.committed {
                return Err(Self::error(&id, "backend creation is incomplete"));
            }
            if route.runtime == RuntimeKind::Process {
                this.host.start(&id).await
            } else {
                this.container.start(&id).await
            }
        })
        .await
    }
    async fn stop(&self, id: &InstanceId) -> Result<(), GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.stop(&id).await
            } else {
                this.container.stop(&id).await
            }
        })
        .await
    }
    async fn kill(&self, id: &InstanceId) -> Result<(), GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.kill(&id).await
            } else {
                this.container.kill(&id).await
            }
        })
        .await
    }
    async fn state(&self, id: &InstanceId) -> Result<ContainerState, GrillError> {
        self.operation(id, |this, id| async move {
            let Some(mut route) = this.load(&id).await? else {
                return Err(this.absent_error(&id).await);
            };
            let host = route.runtime == RuntimeKind::Process;
            let state = this.backend_state(&id, host).await?;
            if state == ContainerState::Stopped
                && !route.retired
                && (host || this.container.network_reference(&id).await?.is_none())
            {
                route.retired = true;
                this.store(&route).await?;
            }
            Ok(state)
        })
        .await
    }
    async fn has_exited(&self, id: &InstanceId) -> Result<bool, GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.has_exited(&id).await
            } else {
                this.container.has_exited(&id).await
            }
        })
        .await
    }
    async fn pid(&self, id: &InstanceId) -> Result<Option<u32>, GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.pid(&id).await
            } else {
                this.container.pid(&id).await
            }
        })
        .await
    }
    async fn exit_code(&self, id: &InstanceId) -> Result<Option<i32>, GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.exit_code(&id).await
            } else {
                this.container.exit_code(&id).await
            }
        })
        .await
    }
    async fn logs(&self, id: &InstanceId) -> Result<String, GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.logs(&id).await
            } else {
                this.container.logs(&id).await
            }
        })
        .await
    }
    async fn workload_cgroup(&self, id: &InstanceId) -> Result<Option<u64>, GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.workload_cgroup(&id).await
            } else {
                this.container.workload_cgroup(&id).await
            }
        })
        .await
    }
    async fn retain_network_reference(
        &self,
        id: &InstanceId,
    ) -> Result<Option<NetworkReference>, GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.retain_network_reference(&id).await
            } else {
                this.container.retain_network_reference(&id).await
            }
        })
        .await
    }
    async fn network_reference(
        &self,
        id: &InstanceId,
    ) -> Result<Option<NetworkReference>, GrillError> {
        self.operation(id, |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime == RuntimeKind::Process {
                this.host.network_reference(&id).await
            } else {
                this.container.network_reference(&id).await
            }
        })
        .await
    }

    async fn adopt(&self, id: &InstanceId, record: &InstanceRecord) -> Result<bool, GrillError> {
        let record = record.clone();
        self.operation(id, move |this, id| async move {
            let route = this.selected(&id).await?;
            if route.runtime != record.runtime
                || record.runtime != this.runtime_kind_for(&record.oci_spec)
            {
                return Err(Self::error(
                    &id,
                    "adoption record conflicts with its original backend",
                ));
            }
            if route.runtime == RuntimeKind::Process {
                this.host.adopt(&id, &record).await
            } else {
                this.container.adopt(&id, &record).await
            }
        })
        .await
    }
    async fn release_network_reference(
        &self,
        reference: &NetworkReference,
    ) -> Result<(), GrillError> {
        let reference = reference.clone();
        self.operation(&reference.instance_id.clone(), move |this, id| async move {
            if this.selected(&id).await?.runtime == RuntimeKind::Process {
                return Err(Self::error(
                    &id,
                    "host commands do not own container addresses",
                ));
            }
            this.container.release_network_reference(&reference).await
        })
        .await
    }
    async fn forget_retired(&self, id: &InstanceId) -> Result<(), GrillError> {
        self.forget_route(id).await
    }
    async fn unlaunched_metadata(&self) -> Result<Vec<InstanceId>, GrillError> {
        self.orphan_routes().await
    }
    async fn launch_inventory(&self) -> Result<Option<Vec<RuntimeLaunch>>, GrillError> {
        let container = self.container.launch_inventory().await?.ok_or_else(|| {
            Self::error(
                &InstanceId("inventory".into()),
                "container inventory is unavailable",
            )
        })?;
        let host = self.host.launch_inventory().await?.ok_or_else(|| {
            Self::error(
                &InstanceId("inventory".into()),
                "host inventory is unavailable",
            )
        })?;
        // A matching inventory entry is observation, not a lifecycle mutation.
        // Holding its claim would queue detached readers behind create/cleanup
        // when the caller's snapshot deadline expires. Atomic route files and
        // backend generation receipts remain authoritative; actual mutations
        // still revalidate under the exclusive claim.
        let launches: Vec<_> = container
            .into_iter()
            .map(|launch| (false, launch))
            .chain(host.into_iter().map(|launch| (true, launch)))
            .collect();
        let this = self.clone();
        let snapshots = tokio::task::spawn_blocking(move || {
            launches
                .into_iter()
                .map(|(is_host, launch)| {
                    let route = this.selected_blocking(&launch.instance_id)?;
                    Ok((is_host, launch, route))
                })
                .collect::<Result<Vec<_>, GrillError>>()
        })
        .await
        .map_err(|error| Self::error(&InstanceId("inventory".into()), error))??;
        let mut result = Vec::new();
        for (is_host, launch, route) in snapshots {
            let id = launch.instance_id.clone();
            let selected = if (route.runtime == RuntimeKind::Process) != is_host {
                // This is still an observation of the original backend, not
                // permission to mutate its replacement. Check positive original
                // retirement without waiting on the other backend's lifecycle
                // claim. Generation receipts are revalidated before recovery
                // mutates anything, just as for matching-route snapshots.
                self.prove_retired(&id, is_host, false).await?;
                None
            } else {
                Some(launch)
            };
            if let Some(launch) = selected {
                if launch.spec.host_process != is_host
                    || launch.network_reference.as_ref().is_some_and(|reference| {
                        matches!(reference, NetworkReferenceState::Held(_)) && is_host
                    })
                {
                    return Err(Self::error(
                        &id,
                        "runtime launch conflicts with its selected backend",
                    ));
                }
                result.push(launch);
            }
        }
        Ok(Some(result))
    }
    fn runtime_kind(&self) -> RuntimeKind {
        self.container.runtime_kind()
    }
    fn runtime_kind_for_host(&self, host: bool) -> RuntimeKind {
        if host {
            RuntimeKind::Process
        } else {
            self.container.runtime_kind()
        }
    }
    fn supports_runtime(&self, kind: RuntimeKind) -> bool {
        kind == RuntimeKind::Process || self.container.supports_runtime(kind)
    }
    fn honours_cgroup_path(&self) -> bool {
        self.container.honours_cgroup_path()
    }
    fn honours_cgroup_path_for(&self, spec: &OciSpec) -> bool {
        !spec.host_process && self.container.honours_cgroup_path_for(spec)
    }
    #[cfg(target_os = "linux")]
    fn host_executor_runtime(&self) -> Option<super::ProcessGrill> {
        self.host.host_executor_runtime()
    }
    #[cfg(target_os = "linux")]
    fn reusable_runtime(&self) -> Option<super::runc::RuncGrill> {
        self.container.reusable_runtime()
    }
    async fn log_stem(&self, id: &InstanceId) -> Option<PathBuf> {
        let route = self.selected(id).await.ok()?;
        if route.runtime == RuntimeKind::Process {
            self.host.log_stem(id).await
        } else {
            self.container.log_stem(id).await
        }
    }
    async fn rootless_network_record(&self, id: &InstanceId) -> Option<RootlessNetworkRecord> {
        if self.selected(id).await.ok()?.runtime == RuntimeKind::Process {
            None
        } else {
            self.container.rootless_network_record(id).await
        }
    }
    async fn container_ip(&self, id: &InstanceId) -> Option<std::net::Ipv4Addr> {
        if self.selected(id).await.ok()?.runtime == RuntimeKind::Process {
            None
        } else {
            self.container.container_ip(id).await
        }
    }
    async fn follow_logs(
        &self,
        id: &InstanceId,
        tx: tokio::sync::mpsc::Sender<crate::ketchup::types::CapturedLine>,
        resume: &crate::ketchup::types::CaptureOffsets,
    ) {
        // A follower must not retain the lifecycle claim while waiting for output:
        // stop/kill need that claim to retire its original producer.
        if let Ok(route) = self.selected(id).await {
            if route.runtime == RuntimeKind::Process {
                self.host.follow_logs(id, tx, resume).await;
            } else {
                self.container.follow_logs(id, tx, resume).await;
            }
        }
    }
    async fn exec(&self, id: &InstanceId, command: &[String]) -> Result<String, GrillError> {
        // Like follow_logs, exec doesn't take the lifecycle claim: it may run
        // for minutes, and stop/kill need the claim. It also stays in the
        // caller's future, so the agent's exec timeout drops the backend call.
        if self.selected(id).await?.runtime == RuntimeKind::Process {
            self.host.exec(id, command).await
        } else {
            self.container.exec(id, command).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::mock::MockGrill;
    use std::path::Path;
    use std::sync::Arc;
    async fn pair(root: &Path) -> MixedGrill<MockGrill, MockGrill> {
        let image = MockGrill::new();
        image.set_runtime_kind(RuntimeKind::Runc);
        let host = MockGrill::new();
        image.set_launch_inventory(vec![]).await;
        host.set_launch_inventory(vec![]).await;
        MixedGrill::new(image, host, root.join("routes"))
    }
    fn spec(host: bool) -> OciSpec {
        let mut result = crate::grill::oci::generate_job_oci_spec(
            "test",
            "default",
            &crate::config::Config::parse("[job.worker]\nruntime='process'\nscript='true'")
                .unwrap()
                .job["worker"],
            "/sys/fs/cgroup/reliaburger/test",
            None,
        );
        result.host_process = host;
        result
    }
    #[tokio::test]
    async fn inventory_reads_do_not_wait_for_a_running_lifecycle_operation() {
        let root = tempfile::tempdir().unwrap();
        let runtime = pair(root.path()).await;
        let id = InstanceId("default__worker-0".into());
        runtime.create(&id, &spec(false)).await.unwrap();
        runtime
            .container
            .set_launch_inventory(vec![RuntimeLaunch {
                generation: super::super::RuntimeGeneration::try_from("a".repeat(64)).unwrap(),
                instance_id: id.clone(),
                spec: spec(false),
                network_reference: None,
            }])
            .await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let held = tokio::spawn({
            let runtime = runtime.clone();
            let id = id.clone();
            let entered = entered.clone();
            let release = release.clone();
            async move {
                runtime
                    .operation(&id, move |_, _| async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok(())
                    })
                    .await
            }
        });
        entered.notified().await;
        let snapshot =
            tokio::time::timeout(Duration::from_millis(200), runtime.launch_inventory()).await;
        release.notify_one();
        held.await.unwrap().unwrap();
        let snapshot = snapshot
            .expect("read-only inventory waited for runtime mutation")
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].instance_id, id);
    }

    #[tokio::test]
    async fn inventory_omits_a_retired_backend_without_waiting_for_its_replacement() {
        for original_host in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let runtime = pair(root.path()).await;
            let id = InstanceId("default__worker-0".into());
            runtime.create(&id, &spec(original_host)).await.unwrap();
            runtime.start(&id).await.unwrap();
            runtime.kill(&id).await.unwrap();
            runtime.create(&id, &spec(!original_host)).await.unwrap();
            runtime.start(&id).await.unwrap();
            for (host, generation) in [(false, "a"), (true, "b")] {
                let launches = vec![RuntimeLaunch {
                    generation: super::super::RuntimeGeneration::try_from(generation.repeat(64))
                        .unwrap(),
                    instance_id: id.clone(),
                    spec: spec(host),
                    network_reference: None,
                }];
                if host {
                    runtime.host.set_launch_inventory(launches).await;
                } else {
                    runtime.container.set_launch_inventory(launches).await;
                }
            }
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let held = tokio::spawn({
                let runtime = runtime.clone();
                let id = id.clone();
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    runtime
                        .operation(&id, move |_, _| async move {
                            entered.notify_one();
                            release.notified().await;
                            Ok(())
                        })
                        .await
                }
            });
            entered.notified().await;
            let snapshot =
                tokio::time::timeout(Duration::from_millis(200), runtime.launch_inventory()).await;
            release.notify_one();
            held.await.unwrap().unwrap();
            let launches = snapshot
                .expect("retired inventory waited for the other backend's lifecycle claim")
                .unwrap()
                .unwrap();
            assert_eq!(launches.len(), 1);
            assert_eq!(launches[0].instance_id, id);
            assert_eq!(launches[0].spec.host_process, !original_host);
            // A route alone cannot hide a backend which still owns work.
            if original_host {
                runtime.host.set_state(&id, ContainerState::Running);
            } else {
                runtime.container.set_state(&id, ContainerState::Running);
            }
            assert!(runtime.launch_inventory().await.is_err());
            if !original_host {
                runtime.container.set_state(&id, ContainerState::Stopped);
                runtime
                    .container
                    .set_network_reference(NetworkReference {
                        instance_id: id.clone(),
                        generation: serde_json::from_value(serde_json::json!("a".repeat(32)))
                            .unwrap(),
                        container_index: 1,
                    })
                    .await;
                assert!(
                    runtime.launch_inventory().await.is_err(),
                    "stopped work with a held address is not positively retired"
                );
            }
        }
    }

    #[tokio::test]
    async fn missing_route_cannot_replace_an_original_backend_owner() {
        for (original, replacement) in [(false, true), (true, false), (false, false), (true, true)]
        {
            let root = tempfile::tempdir().unwrap();
            let runtime = pair(root.path()).await;
            let id = InstanceId("default__worker-0".into());
            let original_spec = spec(original);
            runtime.create(&id, &original_spec).await.unwrap();
            runtime.start(&id).await.unwrap();
            let inventory = vec![RuntimeLaunch {
                generation: super::super::RuntimeGeneration::try_from("a".repeat(64)).unwrap(),
                instance_id: id.clone(),
                spec: original_spec,
                network_reference: None,
            }];
            if original {
                runtime.host.set_launch_inventory(inventory).await;
            } else {
                runtime.container.set_launch_inventory(inventory).await;
            }
            std::fs::remove_file(runtime.path(&id).unwrap()).unwrap();
            assert!(runtime.state(&id).await.is_err());
            assert!(
                runtime.create(&id, &spec(replacement)).await.is_err(),
                "missing route authorised a replacement"
            );
            assert_eq!(
                runtime.backend_state(&id, original).await.unwrap(),
                ContainerState::Running
            );
            if original {
                runtime.host.kill(&id).await.unwrap();
            } else {
                runtime.container.kill(&id).await.unwrap();
            }
            runtime.create(&id, &spec(replacement)).await.unwrap();
            runtime.start(&id).await.unwrap();
        }
    }

    #[tokio::test]
    async fn missing_route_requires_complete_original_inventories() {
        let root = tempfile::tempdir().unwrap();
        let runtime = MixedGrill::new(
            MockGrill::new(),
            MockGrill::new(),
            root.path().join("routes"),
        );
        let id = InstanceId("default__worker-0".into());
        assert!(runtime.create(&id, &spec(true)).await.is_err());
        assert!(
            !runtime
                .host
                .calls()
                .iter()
                .any(|(call, _)| call == "create")
        );
        runtime
            .host
            .set_launch_inventory(vec![RuntimeLaunch {
                generation: super::super::RuntimeGeneration::try_from("a".repeat(64)).unwrap(),
                instance_id: id.clone(),
                spec: spec(true),
                network_reference: None,
            }])
            .await;
        runtime.container.set_launch_inventory(vec![]).await;
        // An inventory entry with unavailable state is not proof of absence.
        assert!(runtime.create(&id, &spec(true)).await.is_err());
        assert!(
            !runtime
                .host
                .calls()
                .iter()
                .any(|(call, _)| call == "create")
        );
    }

    #[tokio::test]
    async fn simultaneous_backends_recover_the_original_selection() {
        let root = tempfile::tempdir().unwrap();
        let runtime = pair(root.path()).await;
        let host = InstanceId("default__host-0".into());
        let image = InstanceId("default__image-0".into());
        runtime.create(&host, &spec(true)).await.unwrap();
        runtime.create(&image, &spec(false)).await.unwrap();
        runtime.start(&host).await.unwrap();
        runtime.start(&image).await.unwrap();
        assert_eq!(
            runtime
                .host
                .calls()
                .iter()
                .filter(|(call, _)| call == "start")
                .count(),
            1
        );
        assert_eq!(
            runtime
                .container
                .calls()
                .iter()
                .filter(|(call, _)| call == "start")
                .count(),
            1
        );
        let restarted = MixedGrill::new(
            runtime.container.clone(),
            runtime.host.clone(),
            root.path().join("routes"),
        );
        restarted.kill(&host).await.unwrap();
        assert_eq!(
            restarted.state(&image).await.unwrap(),
            ContainerState::Running
        );
        assert_eq!(
            restarted.state(&host).await.unwrap(),
            ContainerState::Stopped
        );
    }
    #[tokio::test]
    async fn active_backend_changes_refuse_then_retired_changes_work_both_ways() {
        let root = tempfile::tempdir().unwrap();
        let runtime = pair(root.path()).await;
        let id = InstanceId("default__worker-0".into());
        runtime.create(&id, &spec(true)).await.unwrap();
        runtime.start(&id).await.unwrap();
        assert!(runtime.create(&id, &spec(false)).await.is_err());
        runtime.kill(&id).await.unwrap();
        runtime.create(&id, &spec(false)).await.unwrap();
        runtime.start(&id).await.unwrap();
        assert!(runtime.create(&id, &spec(true)).await.is_err());
        runtime.kill(&id).await.unwrap();
        runtime.create(&id, &spec(true)).await.unwrap();
        runtime.start(&id).await.unwrap();
        runtime.host.set_state(&id, ContainerState::Running);
        assert_eq!(runtime.state(&id).await.unwrap(), ContainerState::Running);
    }
    #[tokio::test]
    async fn failed_image_creation_never_runs_a_host_command() {
        let root = tempfile::tempdir().unwrap();
        let runtime = pair(root.path()).await;
        runtime.container.set_fail_create(true);
        let id = InstanceId("default__image-0".into());
        assert!(runtime.create(&id, &spec(false)).await.is_err());
        assert!(runtime.start(&id).await.is_err());
        assert!(runtime.host.calls().is_empty());
    }
    #[tokio::test]
    async fn corrupt_route_and_unsupported_host_limits_refuse_before_start() {
        let root = tempfile::tempdir().unwrap();
        let runtime = pair(root.path()).await;
        let id = InstanceId("default__worker-0".into());
        let mut limited = spec(true);
        limited.linux.resources = Some(crate::grill::oci::OciResources {
            cpu: None,
            memory: None,
            unified: Default::default(),
        });
        assert!(runtime.create(&id, &limited).await.is_err());
        assert!(runtime.host.calls().is_empty());
        runtime.create(&id, &spec(true)).await.unwrap();
        std::fs::write(runtime.path(&id).unwrap(), b"{}").unwrap();
        assert!(runtime.start(&id).await.is_err());
        assert!(!runtime.host.calls().iter().any(|(call, _)| call == "start"));
    }
    #[tokio::test]
    async fn cancelled_create_keeps_the_cross_backend_claim_until_it_finishes() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(pair(root.path()).await);
        runtime.host.block_creates();
        let id = InstanceId("default__worker-0".into());
        let launch = {
            let runtime = runtime.clone();
            let id = id.clone();
            tokio::spawn(async move { runtime.create(&id, &spec(true)).await })
        };
        runtime.host.wait_for_creates(1).await;
        launch.abort();
        let replacement = {
            let runtime = runtime.clone();
            let id = id.clone();
            tokio::spawn(async move { runtime.create(&id, &spec(false)).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !runtime
                .container
                .calls()
                .iter()
                .any(|(call, _)| call == "create")
        );
        runtime.host.release_creates(1);
        assert!(replacement.await.unwrap().is_err());
        // Created is still owned and must be killed before switching.
        runtime.kill(&id).await.unwrap();
        runtime.create(&id, &spec(false)).await.unwrap();
    }

    #[tokio::test]
    async fn exec_stays_in_the_callers_future_and_never_blocks_lifecycle_operations() {
        let root = tempfile::tempdir().unwrap();
        let runtime = pair(root.path()).await;
        let id = InstanceId("default__worker-0".into());
        runtime.create(&id, &spec(true)).await.unwrap();
        runtime.host.set_exec_outputs(["first".to_string()]);
        runtime.host.block_execs();
        let command = ["sleep".to_string(), "3600".to_string()];
        let mut exec = Box::pin(runtime.exec(&id, &command));
        tokio::select! {
            _ = &mut exec => panic!("blocked exec returned"),
            () = runtime.host.wait_for_execs(1) => {}
        }
        // A long exec must not hold the claim that stop/kill need.
        tokio::time::timeout(Duration::from_secs(2), runtime.kill(&id))
            .await
            .expect("kill waited for a running exec")
            .unwrap();
        // Dropping the caller's future drops the backend exec too, so the
        // agent's exec timeout reaches the process.
        drop(exec);
        runtime.host.release_execs(1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(runtime.exec(&id, &command).await.unwrap(), "first");
    }

    fn launch(id: &InstanceId, host: bool) -> RuntimeLaunch {
        RuntimeLaunch {
            generation: super::super::RuntimeGeneration::try_from("a".repeat(64)).unwrap(),
            instance_id: id.clone(),
            spec: spec(host),
            network_reference: None,
        }
    }

    #[tokio::test]
    async fn route_and_lock_files_are_removed_after_retirement() {
        for host in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let runtime = pair(root.path()).await;
            let id = InstanceId("tenant-a__executor-0".into());
            runtime.create(&id, &spec(host)).await.unwrap();
            runtime.start(&id).await.unwrap();
            let backend = if host {
                &runtime.host
            } else {
                &runtime.container
            };
            backend.set_launch_inventory(vec![launch(&id, host)]).await;
            let route = root.path().join("routes").join(format!("{}.json", id.0));
            let lock = route.with_extension("lock");
            assert!(route.exists() && lock.exists());

            backend.set_state(&id, ContainerState::Running);
            assert!(
                runtime.forget_retired(&id).await.is_err(),
                "a running instance was forgotten"
            );
            assert!(route.exists(), "a refused forget deleted the route");

            backend.set_state(&id, ContainerState::Stopped);
            runtime.forget_retired(&id).await.unwrap();
            assert!(!route.exists(), "route kept after retirement");
            assert!(!lock.exists(), "lock kept after retirement");
            assert!(
                backend
                    .calls()
                    .contains(&("forget_retired".to_string(), id.clone())),
                "the backend journal was not forgotten"
            );
            // The identity starts afresh, exactly as on a new node.
            backend.set_launch_inventory(vec![]).await;
            runtime.create(&id, &spec(host)).await.unwrap();
            assert!(route.exists());
        }
    }

    #[tokio::test]
    async fn a_route_without_a_launch_is_unlaunched_metadata() {
        let root = tempfile::tempdir().unwrap();
        let runtime = pair(root.path()).await;
        let live = InstanceId("default__web-0".into());
        let orphan = InstanceId("tenant-b__executor-3".into());
        runtime.create(&live, &spec(false)).await.unwrap();
        runtime.create(&orphan, &spec(false)).await.unwrap();
        runtime
            .container
            .set_launch_inventory(vec![launch(&live, false)])
            .await;
        assert_eq!(
            runtime.unlaunched_metadata().await.unwrap(),
            vec![orphan.clone()]
        );
        runtime.forget_retired(&orphan).await.unwrap();
        assert!(runtime.unlaunched_metadata().await.unwrap().is_empty());
        let routes: Vec<_> = std::fs::read_dir(root.path().join("routes"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(routes.len(), 2, "{routes:?}");
        assert!(routes.iter().all(|name| name.starts_with("default__web-0")));
    }
}
