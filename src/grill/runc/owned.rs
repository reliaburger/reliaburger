//! OCI integration of generation-bound command and resource ownership.

#[path = "owned_rootless.rs"]
mod rootless;

use std::collections::BTreeMap;
use std::future::Future;
use std::io;
use std::path::Path;
use std::time::Duration;

use super::*;
use crate::grill::capture::CaptureReader;
use crate::grill::command::{
    ClaimedCommandExecutor, CommandOutput, CommandState, RuntimeCommandExecutor,
};
use crate::grill::runc_intent::{
    IntentConfiguration, IntentGeneration, IntentJournal, IntentPhase, RuntimeRole,
};
use crate::ketchup::types::{CapturedLine, LogStream};

/// Shared exclusive claims and the executable that starts independent owners.
#[derive(Clone)]
pub(super) struct Ownership {
    executable: PathBuf,
    contexts: Arc<Mutex<HashMap<InstanceId, ClaimedCommandExecutor>>>,
    inventory_reader: crate::grill::inventory::InventoryReader,
}

impl Ownership {
    pub(super) fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            contexts: Arc::new(Mutex::new(HashMap::new())),
            inventory_reader: Default::default(),
        }
    }
}

fn failure(instance: &InstanceId, error: impl std::fmt::Display) -> GrillError {
    GrillError::StateUnavailable {
        instance: instance.clone(),
        reason: error.to_string(),
    }
}

impl RuncGrill {
    /// Retain the original rootful address before a publisher exposes it.
    pub(super) async fn owned_retain_network_reference(
        &self,
        instance: &InstanceId,
    ) -> Result<super::super::runc_intent::NetworkReference, GrillError> {
        self.owned_operation(instance, |runtime, id, context| async move {
            let index = runtime
                .network_leases
                .lookup(&id, runtime.node_index)
                .await?
                .ok_or_else(|| io::Error::other("runtime network reservation is absent"))?;
            context.retain_network(index).await?;
            match context.intent().await?.network_reference {
                Some(super::super::runc_intent::NetworkReferenceState::Held(reference)) => {
                    Ok(reference)
                }
                _ => Err(io::Error::other(
                    "runtime network reference was not retained",
                )),
            }
        })
        .await
    }

    /// Read a retained reference without inferring withdrawal from runtime exit.
    pub(super) async fn owned_network_reference(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<super::super::runc_intent::NetworkReference>, GrillError> {
        self.owned_operation(instance, |_runtime, _id, context| async move {
            Ok(match context.intent().await?.network_reference {
                Some(super::super::runc_intent::NetworkReferenceState::Held(reference)) => {
                    Some(reference)
                }
                _ => None,
            })
        })
        .await
    }

    /// Release only the matching original reference after discovery withdrawal.
    pub(super) async fn owned_release_network_reference(
        &self,
        reference: &super::super::runc_intent::NetworkReference,
    ) -> Result<(), GrillError> {
        let original = reference.clone();
        self.owned_operation(&reference.instance_id, |runtime, id, context| async move {
            context.release_network(original).await?;
            if context.intent().await?.phase == IntentPhase::Retiring {
                runtime.owned_cleanup(&id, &context).await?;
            }
            Ok(())
        })
        .await
    }

    fn ownership(&self) -> io::Result<&Ownership> {
        Ok(&self.ownership)
    }

    fn intent_configuration(&self) -> io::Result<IntentConfiguration> {
        Ok(IntentConfiguration {
            bundle_directory: self.bundle_base.clone(),
            state_directory: self.state_dir.clone(),
            image_directory: std::path::absolute(self.image_store.storage_directory())?,
            runc_program: self.runc_program.clone(),
            rootless: self.rootless,
            node_index: self.node_index,
            dns_nameserver: self.dns_nameserver,
        })
    }

    fn intent_journal(&self) -> io::Result<IntentJournal> {
        Ok(IntentJournal::new(
            self.bundle_base.join(".intents"),
            self.intent_configuration()?,
        ))
    }

    async fn owned_context(
        &self,
        instance: &InstanceId,
        expected: Option<IntentGeneration>,
    ) -> io::Result<ClaimedCommandExecutor> {
        let ownership = self.ownership()?;
        let cached = ownership.contexts.lock().await.get(instance).cloned();
        if let Some(context) = cached {
            if let Ok(record) = context.intent().await {
                if Some(record.generation) != expected
                    || record.configuration != self.intent_configuration()?
                {
                    return Err(io::Error::other(
                        "runtime generation or configuration changed",
                    ));
                }
                return Ok(context);
            }
            ownership.contexts.lock().await.remove(instance);
        }
        let claim = self.intent_journal()?.claim(instance, expected).await?;
        let retired = claim
            .record()
            .is_some_and(|record| matches!(record.phase, IntentPhase::Retired { .. }));
        let context =
            ClaimedCommandExecutor::new(claim.supervise_commands(ownership.executable.clone())?);
        if !retired {
            ownership
                .contexts
                .lock()
                .await
                .insert(instance.clone(), context.clone());
        }
        Ok(context)
    }

    async fn owned_operation<T, F, Fut>(
        &self,
        instance: &InstanceId,
        operation: F,
    ) -> Result<T, GrillError>
    where
        T: Send + 'static,
        F: FnOnce(Self, InstanceId, ClaimedCommandExecutor) -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<T>> + Send,
    {
        let expected = self
            .intent_journal()
            .map_err(|error| failure(instance, error))?
            .observe(instance)
            .await
            .map_err(|error| failure(instance, error))?;
        if expected.is_none() {
            return Err(GrillError::NotFound {
                instance: instance.clone(),
            });
        }
        let runtime = self.clone();
        let id = instance.clone();
        tokio::spawn(async move {
            let _lifecycle = runtime.lock_lifecycle(&id).await;
            let context = runtime.owned_context(&id, expected).await?;
            operation(runtime, id, context).await
        })
        .await
        .map_err(|error| failure(instance, error))?
        .map_err(|error| failure(instance, error))
    }

    /// Publish original intent and retain preparation through caller cancellation.
    pub(super) async fn owned_create(
        &self,
        instance: &InstanceId,
        spec: &OciSpec,
    ) -> Result<(), GrillError> {
        tokio::fs::create_dir_all(&self.bundle_base)
            .await
            .map_err(|error| failure(instance, error))?;
        let expected = self
            .intent_journal()
            .map_err(|error| failure(instance, error))?
            .observe(instance)
            .await
            .map_err(|error| failure(instance, error))?;
        let runtime = self.clone();
        let id = instance.clone();
        let spec = spec.clone();
        tokio::spawn(async move {
            let _lifecycle = runtime.lock_lifecycle(&id).await;
            let ownership = runtime.ownership()?;
            if ownership.contexts.lock().await.contains_key(&id) {
                return Err(io::Error::other("runtime generation still owns resources"));
            }
            let claim = runtime.intent_journal()?.claim(&id, expected).await?;
            let mut paths = vec![runtime.state_dir.join(&id.0)];
            if !runtime.rootless {
                paths.extend([
                    netns::namespace_path(&id),
                    PathBuf::from("/sys/class/net").join(netns::host_veth_name(&id)),
                ]);
            }
            for path in paths {
                if tokio::fs::try_exists(path).await? {
                    return Err(io::Error::other(
                        "unretired OCI or network resources already exist",
                    ));
                }
            }
            let claim = claim.publish(&spec).await?;
            let context = ClaimedCommandExecutor::new(
                claim.supervise_commands(ownership.executable.clone())?,
            );
            ownership
                .contexts
                .lock()
                .await
                .insert(id.clone(), context.clone());
            let result = async {
                let index = if runtime.rootless {
                    None
                } else {
                    Some(
                        runtime
                            .network_leases
                            .reserve(&id, runtime.node_index)
                            .await?,
                    )
                };
                runtime
                    .prepare_with_commands(&id, &spec, index, &context)
                    .await
                    .map_err(io::Error::other)?;
                if runtime.rootless {
                    runtime.owned_prepare_rootless(&id, &context).await?;
                }
                Ok(())
            }
            .await;
            if result.is_err() {
                runtime.owned_cleanup_recovering(&id).await?;
            }
            result
        })
        .await
        .map_err(|error| failure(instance, error))?
        .map_err(|error| failure(instance, error))
    }

    async fn owned_runc_command(
        &self,
        context: &ClaimedCommandExecutor,
        arguments: &[&str],
    ) -> io::Result<CommandOutput> {
        let program = self
            .runc_program
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF-8 runc executable"))?;
        let state = self
            .state_dir
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF-8 runc state directory"))?;
        let mut full = vec!["--root", state];
        full.extend_from_slice(arguments);
        context.output(program, &full).await
    }

    async fn owned_running_pid(
        &self,
        id: &InstanceId,
        context: &ClaimedCommandExecutor,
    ) -> io::Result<Option<u32>> {
        let output = self.owned_runc_command(context, &["state", &id.0]).await?;
        if output.exit_code != Some(0) {
            return Ok(None);
        }
        let state: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        if state["id"].as_str() != Some(&id.0) {
            return Err(io::Error::other(
                "runc returned a different instance identity",
            ));
        }
        if state["status"].as_str() != Some("running") {
            return Ok(None);
        }
        let pid = state["pid"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|pid| *pid > 0)
            .ok_or_else(|| io::Error::other("running OCI state omitted its process identity"))?;
        Ok(Some(pid))
    }

    /// Bind and activate the launcher, retaining ownership through startup inspection.
    pub(super) async fn owned_start(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.owned_operation(instance, |runtime, id, context| async move {
            let intent = context.intent().await?;
            if intent.from_previous_boot().await? {
                return Err(io::Error::other(
                    "runtime generation belongs to a previous kernel boot",
                ));
            }
            if intent.phase != IntentPhase::Owned || intent.roles.launcher.is_some() {
                return Err(io::Error::other(
                    "runtime launcher is already bound or admission is sealed",
                ));
            }
            let args = vec![
                "--root".into(),
                runtime
                    .state_dir
                    .to_str()
                    .ok_or_else(|| io::Error::other("non-UTF-8 runc state directory"))?
                    .into(),
                "run".into(),
                "--bundle".into(),
                runtime
                    .bundle_base
                    .join(&id.0)
                    .to_str()
                    .ok_or_else(|| io::Error::other("non-UTF-8 bundle directory"))?
                    .into(),
                id.0.clone(),
            ];
            let result = async {
                context
                    .start_role(
                        RuntimeRole::Launcher,
                        &runtime.runc_program,
                        &args,
                        &BTreeMap::new(),
                    )
                    .await?;
                if runtime.rootless {
                    runtime.owned_open_rootless_gate(&context).await?;
                }
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                loop {
                    if runtime.owned_running_pid(&id, &context).await?.is_some() {
                        return Ok(());
                    }
                    if matches!(
                        context.role_state(RuntimeRole::Launcher).await?,
                        Some(CommandState::Retired { .. })
                    ) {
                        return Ok(());
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "runc startup timed out",
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            .await;
            if result.is_err() {
                runtime.owned_cleanup_recovering(&id).await?;
            }
            result
        })
        .await
    }

    async fn owned_cleanup_recovering(&self, id: &InstanceId) -> io::Result<()> {
        let expected = self.intent_journal()?.observe(id).await?;
        let context = self.owned_context(id, expected).await?;
        self.owned_cleanup(id, &context).await
    }

    async fn owned_cleanup(
        &self,
        id: &InstanceId,
        context: &ClaimedCommandExecutor,
    ) -> io::Result<()> {
        let record = context.intent().await?;
        if matches!(record.phase, IntentPhase::Retired { .. }) {
            return Ok(());
        }
        let previous_boot = record.from_previous_boot().await?;
        if previous_boot {
            // Old PIDs and namespace names are not authority over new kernel
            // objects. Refuse conflicts before running any cleanup command.
            let cgroup = record
                .spec
                .linux
                .host_cgroup_path()
                .unwrap_or_else(|| Path::new("/sys/fs/cgroup").join(&id.0));
            for path in [
                cgroup,
                netns::namespace_path(id),
                Path::new("/sys/class/net").join(netns::host_veth_name(id)),
            ] {
                if tokio::fs::try_exists(path).await? {
                    return Err(io::Error::other(
                        "prior-boot runtime has conflicting current kernel resources",
                    ));
                }
            }
        }
        let exit_code = match context.role_state(RuntimeRole::Launcher).await? {
            Some(CommandState::Retired { exit_code }) => exit_code,
            _ => None,
        };
        let cleanup = context.seal(Duration::from_secs(15)).await?;
        let state = self.state_dir.join(&id.0);
        if tokio::fs::try_exists(&state).await? {
            if previous_boot {
                // Under the exclusive original claim, remove only stale private
                // metadata. Never feed prior-boot PIDs to `runc delete --force`.
                tokio::fs::remove_dir_all(&state).await?;
                let parent = self.state_dir.clone();
                tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
                    .await
                    .map_err(io::Error::other)??;
            } else {
                self.owned_runc_command(&cleanup, &["delete", "--force", &id.0])
                    .await?;
            }
            if tokio::fs::try_exists(&state).await? {
                return Err(io::Error::other("runc deletion left OCI state"));
            }
        }
        if !self.rootless {
            super::super::rootfs::unmount_bundle(self.bundle_base.join(&id.0))
                .await
                .map_err(io::Error::other)?;
        }
        if self.rootless {
            self.owned_remove_rootless(&record).await?;
        }
        let index = if self.rootless {
            None
        } else {
            self.network_leases.lookup(id, self.node_index).await?
        };
        let held = match &record.network_reference {
            Some(super::super::runc_intent::NetworkReferenceState::Held(reference)) => {
                Some(reference)
            }
            _ => None,
        };
        if held.is_some_and(|reference| index != Some(reference.container_index)) {
            return Err(io::Error::other(
                "retained discovery address has no matching reservation",
            ));
        }
        if let Some(index) = index {
            let network = netns::planned_container_network(id, self.node_index, index)
                .map_err(io::Error::other)?;
            netns::retire_address_forwarding_with_commands(&cleanup, network.container_ip)
                .await
                .map_err(io::Error::other)?;
            netns::teardown_container_network_with_commands(&cleanup, &network)
                .await
                .map_err(io::Error::other)?;
        } else if !self.rootless
            && (tokio::fs::try_exists(netns::namespace_path(id)).await?
                || tokio::fs::try_exists(
                    Path::new("/sys/class/net").join(netns::host_veth_name(id)),
                )
                .await?)
        {
            return Err(io::Error::other(
                "network resources have no owned address reservation",
            ));
        }
        self.port_handles.lock().await.remove(id);
        {
            let mut networks = self.networks.lock().await;
            networks.remove(id);
            self.publish_dns_sources(&networks);
        }
        if held.is_some() {
            // Commands and host resources are gone. Keep the allocation and
            // sealed original intent until discovery confirms its own retirement.
            return Ok(());
        }
        if let Some(index) = index {
            self.network_leases
                .retire(id, self.node_index, index)
                .await?;
        }
        cleanup.finish(exit_code).await?;
        self.ownership()?.contexts.lock().await.remove(id);
        Ok(())
    }

    /// Retire all admitted runtime work before releasing kernel resources.
    pub(super) async fn owned_kill(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.owned_operation(instance, |runtime, id, context| async move {
            runtime.owned_cleanup(&id, &context).await
        })
        .await
    }

    /// Signal a running container through an owned Runc command.
    pub(super) async fn owned_stop(&self, instance: &InstanceId) -> Result<(), GrillError> {
        self.owned_operation(instance, |runtime, id, context| async move {
            if !matches!(
                context.role_state(RuntimeRole::Launcher).await?,
                Some(CommandState::Running { .. })
            ) {
                return runtime.owned_cleanup(&id, &context).await;
            }
            let output = runtime
                .owned_runc_command(&context, &["kill", &id.0, "SIGTERM"])
                .await?;
            if output.exit_code != Some(0) {
                if matches!(
                    context.role_state(RuntimeRole::Launcher).await?,
                    Some(CommandState::Retired { .. })
                ) {
                    return runtime.owned_cleanup(&id, &context).await;
                }
                return Err(io::Error::other(format!(
                    "runc signal failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(())
        })
        .await
    }

    /// Report positive owner evidence and complete any observed retirement.
    pub(super) async fn owned_state(
        &self,
        instance: &InstanceId,
    ) -> Result<ContainerState, GrillError> {
        self.owned_operation(instance, |runtime, id, context| async move {
            let record = context.intent().await?;
            if matches!(record.phase, IntentPhase::Retired { .. }) {
                return Ok(ContainerState::Stopped);
            }
            if record.phase == IntentPhase::Retiring || record.from_previous_boot().await? {
                runtime.owned_cleanup(&id, &context).await?;
                return Ok(ContainerState::Stopped);
            }
            match context.role_state(RuntimeRole::Launcher).await? {
                None | Some(CommandState::Prepared) => Ok(ContainerState::Pending),
                Some(CommandState::Running { .. }) => {
                    if runtime.rootless {
                        let Some(pid) = runtime.owned_running_pid(&id, &context).await? else {
                            if matches!(
                                context.role_state(RuntimeRole::Launcher).await?,
                                Some(CommandState::Retired { .. })
                            ) {
                                runtime.owned_cleanup(&id, &context).await?;
                                return Ok(ContainerState::Stopped);
                            }
                            return Ok(ContainerState::Pending);
                        };
                        runtime.owned_rootless_network(&context, pid).await?;
                    }
                    Ok(ContainerState::Running)
                }
                Some(CommandState::Cancelled | CommandState::Retired { .. }) => {
                    runtime.owned_cleanup(&id, &context).await?;
                    Ok(ContainerState::Stopped)
                }
            }
        })
        .await
    }

    /// Read the actual workload outcome without inventing a cleanup result.
    pub(super) async fn owned_exit_code(&self, instance: &InstanceId) -> Option<i32> {
        self.owned_operation(instance, |_runtime, _id, context| async move {
            if let IntentPhase::Retired { exit_code } = context.intent().await?.phase {
                return Ok(exit_code);
            }
            Ok(match context.role_state(RuntimeRole::Launcher).await? {
                Some(CommandState::Retired { exit_code }) => exit_code,
                _ => None,
            })
        })
        .await
        .ok()
        .flatten()
    }

    /// Return informational identity from the bound launcher owner.
    pub(super) async fn owned_pid(&self, instance: &InstanceId) -> Option<u32> {
        self.owned_operation(instance, |_runtime, _id, context| async move {
            Ok(match context.role_state(RuntimeRole::Launcher).await? {
                Some(CommandState::Running { pid }) => Some(pid),
                _ => None,
            })
        })
        .await
        .ok()
        .flatten()
    }

    /// Verify source attribution against the original launch and retained owner.
    pub(super) async fn owned_workload_cgroup(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<u64>, GrillError> {
        self.owned_operation(instance, |runtime, id, context| async move {
            let intent = context.intent().await?;
            if intent.phase != IntentPhase::Owned || runtime.rootless {
                return Ok(None);
            }
            let Some(CommandState::Running { pid: launcher }) =
                context.role_state(RuntimeRole::Launcher).await?
            else {
                return Ok(None);
            };
            if intent.spec.linux.cgroups_path.is_none() {
                return Ok(None);
            }
            let path = intent
                .spec
                .linux
                .host_cgroup_path()
                .ok_or_else(|| io::Error::other("original workload cgroup path is invalid"))?;
            let pid = runtime
                .owned_running_pid(&id, &context)
                .await?
                .ok_or_else(|| io::Error::other("owned launcher has no verified live container"))?;
            // Retain both kernel objects until the owner confirms the same
            // launcher after inspection. Numeric PID reuse cannot change an
            // already-open /proc directory underneath these reads.
            let (_process, _cgroup, cgroup_id) = tokio::task::spawn_blocking(move || {
                use std::os::fd::AsRawFd;
                use std::os::unix::fs::MetadataExt;
                let process = std::fs::File::open(format!("/proc/{pid}"))?;
                let base = format!("/proc/self/fd/{}", process.as_raw_fd());
                let status = std::fs::read_to_string(format!("{base}/status"))?;
                let parent = status
                    .lines()
                    .find_map(|line| line.strip_prefix("PPid:"))
                    .and_then(|value| value.trim().parse::<u32>().ok());
                let nested_init = status
                    .lines()
                    .find_map(|line| line.strip_prefix("NSpid:"))
                    .is_some_and(|value| {
                        let ids: Vec<_> = value.split_whitespace().collect();
                        ids.len() >= 2 && ids.last() == Some(&"1")
                    });
                let membership = std::fs::read_to_string(format!("{base}/cgroup"))?;
                let hierarchy = membership.lines().find_map(|line| line.strip_prefix("0::"));
                let expected = path
                    .strip_prefix("/sys/fs/cgroup")
                    .map_err(io::Error::other)?;
                let expected = format!("/{}", expected.display());
                if parent != Some(launcher) || !nested_init || hierarchy != Some(expected.as_str())
                {
                    return Err(io::Error::other(
                        "container source identity conflicts with its original owner",
                    ));
                }
                let cgroup = std::fs::File::open(path)?;
                let metadata = cgroup.metadata()?;
                if !metadata.is_dir() {
                    return Err(io::Error::other("workload cgroup is not a directory"));
                }
                Ok::<_, io::Error>((process, cgroup, metadata.ino()))
            })
            .await
            .map_err(io::Error::other)??;
            if context.role_state(RuntimeRole::Launcher).await?
                != Some(CommandState::Running { pid: launcher })
            {
                return Err(io::Error::other(
                    "workload owner retired during source inspection",
                ));
            }
            Ok(Some(cgroup_id))
        })
        .await
    }

    /// Return original requests independently of agent adoption records.
    pub(super) async fn owned_inventory(
        &self,
    ) -> Result<Option<Vec<crate::grill::RuntimeLaunch>>, GrillError> {
        let unavailable = |error: io::Error| GrillError::InventoryUnavailable {
            reason: error.to_string(),
        };
        let ownership = self.ownership().map_err(unavailable)?;
        let journal = self.intent_journal().map_err(unavailable)?;
        let records = ownership
            .inventory_reader
            .read(async move { journal.inventory().await })
            .await
            .map_err(unavailable)?;
        Ok(Some(
            records
                .into_iter()
                .map(|record| crate::grill::RuntimeLaunch {
                    generation: crate::grill::RuntimeGeneration::runc(record.generation.as_str()),
                    instance_id: record.instance_id,
                    spec: record.spec,
                    network_reference: record.network_reference,
                })
                .collect(),
        ))
    }

    /// Validate adoption against this generation before restoring network bindings.
    pub(super) async fn owned_adopt(
        &self,
        instance: &InstanceId,
        adoption: &crate::grill::records::InstanceRecord,
    ) -> Result<bool, GrillError> {
        let adoption = adoption.clone();
        self.owned_operation(instance, move |runtime, id, context| async move {
            let intent = context.intent().await?;
            if intent.spec != adoption.oci_spec
                || adoption.instance_id != id.0
                || adoption.runtime != crate::grill::records::RuntimeKind::Runc
                || adoption.runc_container_id.as_deref() != Some(&id.0)
            {
                return Err(io::Error::other(
                    "runtime intent conflicts with adoption record",
                ));
            }
            let stem = context.role_log_stem(RuntimeRole::Launcher).await?;
            if stem.is_none() || adoption.log_stem != stem {
                return Err(io::Error::other(
                    "adoption log identity conflicts with runtime generation",
                ));
            }
            if matches!(intent.phase, IntentPhase::Retired { .. }) {
                return Ok(false);
            }
            if intent.phase == IntentPhase::Retiring {
                // A sealed generation never runs again. Its host resources are
                // gone or go now; a retained discovery address stays held until
                // the caller's confirmed withdrawal releases it. Refusing here
                // left the agent unable to start at all.
                runtime.owned_cleanup(&id, &context).await?;
                return Ok(false);
            }
            let Some(CommandState::Running { pid: launcher_pid }) =
                context.role_state(RuntimeRole::Launcher).await?
            else {
                runtime.owned_cleanup(&id, &context).await?;
                return Ok(false);
            };
            // Same ±2 s tolerance as every other adoption: /proc start times
            // are derived from boot time, which moves with NTP steps.
            if adoption.pid != launcher_pid
                || !crate::grill::records::process_matches(launcher_pid, adoption.pid_started_at)
            {
                return Err(io::Error::other(
                    "adoption process identity conflicts with runtime owner",
                ));
            }
            let pid = runtime
                .owned_running_pid(&id, &context)
                .await?
                .ok_or_else(|| io::Error::other("owned launcher has no running OCI state"))?;
            if runtime.rootless {
                runtime.owned_rootless_network(&context, pid).await?;
                return Ok(true);
            }
            let index = runtime
                .network_leases
                .lookup(&id, runtime.node_index)
                .await?
                .ok_or_else(|| io::Error::other("owned container has no address reservation"))?;
            let expected = netns::planned_container_network(&id, runtime.node_index, index)
                .map_err(io::Error::other)?;
            let network = netns::adopt_container_network(&id, pid)
                .await
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::other("owned container network is absent"))?;
            if network.container_ip != expected.container_ip
                || network.host_veth != expected.host_veth
                || network.namespace_path != expected.namespace_path
            {
                return Err(io::Error::other(
                    "container network conflicts with its address reservation",
                ));
            }
            let mut networks = runtime.networks.lock().await;
            networks.insert(id, network);
            runtime.publish_dns_sources(&networks);
            Ok(true)
        })
        .await
    }

    /// Locate this generation’s original launcher logs.
    pub(super) async fn owned_log_stem(
        &self,
        instance: &InstanceId,
    ) -> Result<Option<PathBuf>, GrillError> {
        self.owned_operation(instance, |_runtime, _id, context| async move {
            context.role_log_stem(RuntimeRole::Launcher).await
        })
        .await
    }

    /// Read captured output from the bound launcher.
    pub(super) async fn owned_logs(&self, instance: &InstanceId) -> Result<String, GrillError> {
        let Some(stem) = self.owned_log_stem(instance).await? else {
            return Ok(String::new());
        };
        let mut output = String::new();
        for extension in ["stdout", "stderr"] {
            let bytes = tokio::fs::read(stem.with_extension(extension))
                .await
                .map_err(|error| failure(instance, error))?;
            output.push_str(&String::from_utf8_lossy(&bytes));
        }
        Ok(output)
    }

    /// Run container exec under the launcher owner with caller cancellation.
    pub(super) async fn owned_exec(
        &self,
        instance: &InstanceId,
        command: &[String],
    ) -> Result<String, GrillError> {
        if command.is_empty() {
            return Err(failure(instance, "no command specified"));
        }
        let context = self
            .owned_operation(
                instance,
                |_runtime, _id, context| async move { Ok(context) },
            )
            .await?;
        let mut arguments = vec![
            self.runc_program
                .to_str()
                .ok_or_else(|| failure(instance, "non-UTF-8 runc executable"))?
                .into(),
            "--root".into(),
            self.state_dir
                .to_str()
                .ok_or_else(|| failure(instance, "non-UTF-8 runc state directory"))?
                .into(),
            "exec".into(),
            "--".into(),
            instance.0.clone(),
        ];
        arguments.extend_from_slice(command);
        context
            .exec_role(RuntimeRole::Launcher, &arguments)
            .await
            .map_err(|error| failure(instance, error))
    }

    /// Follow the captured generation and drain its final output.
    pub(super) async fn owned_follow_logs(
        &self,
        instance: &InstanceId,
        sender: tokio::sync::mpsc::Sender<CapturedLine>,
    ) {
        let source = self
            .owned_operation(instance, |_runtime, _id, context| async move {
                let stem = context.role_log_stem(RuntimeRole::Launcher).await?;
                let retired = matches!(context.intent().await?.phase, IntentPhase::Retired { .. });
                // Immutable retired logs need no lifecycle claim. A slow reader
                // must not retain the lock required to prepare the next generation.
                Ok((stem, if retired { None } else { Some(context) }))
            })
            .await;
        let Ok((Some(stem), context)) = source else {
            return;
        };
        let mut terminal = context.is_none();
        let mut readers = [(LogStream::Stdout, "stdout"), (LogStream::Stderr, "stderr")].map(
            |(stream, extension)| CaptureReader::new(stream, Some(stem.with_extension(extension))),
        );
        loop {
            for reader in &mut readers {
                let Some(file) = reader.file().map(std::path::Path::to_path_buf) else {
                    continue;
                };
                let Ok(bytes) = read_from_offset(&file, reader.read_offset()).await else {
                    continue;
                };
                for line in reader.push(&bytes) {
                    if sender.send(line).await.is_err() {
                        return;
                    }
                }
            }
            if terminal || sender.is_closed() {
                for reader in &mut readers {
                    if let Some(line) = reader.finish() {
                        let _ = sender.send(line).await;
                    }
                }
                return;
            }
            // Follow the captured generation, never a successor using its name.
            // A lost source ends this log stream without asserting runtime absence.
            terminal = match &context {
                Some(context) => !matches!(
                    context.role_state(RuntimeRole::Launcher).await,
                    Ok(Some(CommandState::Running { .. }))
                ),
                None => true,
            };
            if !terminal {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}
