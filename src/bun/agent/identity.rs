//! Workload identities on disk: where an instance's identity lives, who
//! owns it, rotation and the sweep of orphaned identity directories.

use super::*;

/// Construct the identity the agent requests for an app or job.
///
/// Keeping this in one function prevents certificate SANs and JWT claims from
/// drifting onto different trust domains.
pub fn workload_spiffe_uri(
    trust_domain: &str,
    namespace: &str,
    name: &str,
    workload_type: crate::sesame::types::WorkloadType,
) -> crate::sesame::types::SpiffeUri {
    crate::sesame::types::SpiffeUri {
        trust_domain: trust_domain.to_string(),
        namespace: namespace.to_string(),
        workload_type,
        name: name.to_string(),
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// The per-instance identity directory (PKI7): keyed by instance id so
    /// replicas never share (or clobber) key material.
    pub(super) fn instance_identity_dir(&self, instance_id: &InstanceId) -> std::path::PathBuf {
        crate::sesame::identity::instance_identity_dir(&self.volumes_dir, &instance_id.0)
    }

    /// The uid/gid identity files should be owned by, so the container
    /// process can read its owner-only key. Only when we're root and can
    /// actually chown: in rootless mode the files stay owned by the bun
    /// user, the same user namespace the workload runs in.
    ///
    /// Runc hands the directory to the container's (user-namespaced) host
    /// uid when it creates the container, so files follow the directory's
    /// owner. A directory still owned by root belongs to a runtime without
    /// that step, whose workloads run as nobody (65534).
    pub(super) fn workload_identity_owner(dir: &std::path::Path) -> Option<(u32, u32)> {
        use std::os::unix::fs::MetadataExt;
        if !nix::unistd::geteuid().is_root() {
            return None;
        }
        match std::fs::metadata(dir) {
            Ok(metadata) if metadata.uid() != 0 => Some((metadata.uid(), metadata.gid())),
            _ => Some((65534, 65534)),
        }
    }

    /// Prepare an instance's identity directory before its container is
    /// created — the bind-mount source must exist, and on Linux root mode
    /// this is where the backing tmpfs gets mounted (PKI7).
    pub(super) fn prepare_instance_identity(
        &self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        let dir = self.instance_identity_dir(instance_id);
        crate::sesame::identity::prepare_identity_dir(&dir).map_err(|e| BunError::SecurityError {
            reason: format!("failed to prepare identity dir for {instance_id}: {e}"),
        })
    }

    /// Remove identity directories that don't belong to any tracked
    /// instance. Runs once after adoption, so the key material of instances
    /// that died while bun was down never lingers (PKI7).
    pub(super) async fn sweep_orphaned_identity_dirs(&self) {
        let root = self.volumes_dir.join(".identity");
        // Decide what to keep here, then leave the directory walk and file
        // removal to a blocking worker.
        let keep: std::collections::HashSet<String> = self
            .supervisor
            .list_instances()
            .iter()
            .map(|instance| instance.id.0.clone())
            .chain(
                self.startup_retirements
                    .iter()
                    .map(|pending| pending.instance_id.0.clone()),
            )
            .collect();
        let swept = tokio::task::spawn_blocking(move || {
            let Ok(entries) = std::fs::read_dir(&root) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if keep.contains(&name) {
                    continue;
                }
                if let Err(e) = crate::sesame::identity::cleanup_identity_dir(&entry.path()) {
                    eprintln!("bun: warning: failed to sweep stale identity dir {name}: {e}");
                }
            }
        })
        .await;
        if let Err(error) = swept {
            eprintln!("bun: warning: identity sweep worker failed: {error}");
        }
    }

    /// Check identity rotation for all instances, and (rate-limited)
    /// provision identities for running instances that don't have one —
    /// a failed CSR at deploy time, or an adopted instance whose
    /// directory predates the per-instance layout, heals here (D9).
    pub(super) fn check_identity_rotation(&mut self) {
        let now = std::time::SystemTime::now();
        let mut needs_rotation = Vec::new();

        self.identity_retry_ticks += 1;
        let retry_missing = self.identity_retry_ticks >= IDENTITY_RETRY_TICKS;
        if retry_missing {
            self.identity_retry_ticks = 0;
        }

        for inst in self.supervisor.list_instances() {
            let Some(ref identity) = inst.identity else {
                // Apps only: job containers don't mount an identity dir.
                if retry_missing
                    && !inst.is_job
                    && inst.state == crate::grill::state::ContainerState::Running
                {
                    needs_rotation.push((
                        inst.id.clone(),
                        inst.app_name.clone(),
                        inst.namespace.clone(),
                        inst.is_job,
                    ));
                }
                continue;
            };
            let state = crate::sesame::identity::rotation_state(identity, now);
            match state {
                crate::sesame::identity::RotationState::NeedsRotation => {
                    needs_rotation.push((
                        inst.id.clone(),
                        inst.app_name.clone(),
                        inst.namespace.clone(),
                        inst.is_job,
                    ));
                }
                crate::sesame::identity::RotationState::Expired => {
                    eprintln!(
                        "warning: identity expired for {} ({})",
                        inst.id.0, inst.app_name
                    );
                }
                crate::sesame::identity::RotationState::GracePeriod => {
                    eprintln!(
                        "warning: identity in grace period for {} ({})",
                        inst.id.0, inst.app_name
                    );
                }
                crate::sesame::identity::RotationState::Valid => {}
            }
        }

        // Only start the signings here: they finish on the loop when their
        // tasks report back, and one already running for an instance is joined.
        for (id, app, ns, is_job) in needs_rotation {
            self.begin_identity_provision(&app, &ns, &id, is_job, None);
        }
    }
}
