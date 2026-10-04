//! Volume leases: reserving an app's volumes for a snapshot or restore, and
//! preparing storage for a launch.

use super::*;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Reserve an app's volumes for a snapshot operation.
    pub(super) fn reserve_volumes(
        &mut self,
        namespace: &str,
        app: &str,
        operation: crate::bun::volume_maintenance::VolumeOperation,
    ) -> Option<crate::bun::volume_maintenance::VolumeLease> {
        self.volume_maintenance.reserve(namespace, app, operation)
    }

    /// Hand a snapshot operation's volumes back, then answer it. The order
    /// matters: once the caller has the answer it may send its next
    /// snapshot request straight away, and that request must not find this
    /// finished operation still holding the volumes (#340). The work is done
    /// by now, so releasing first can't let anything overlap it.
    pub(super) fn release_then_answer<T>(
        lease: crate::bun::volume_maintenance::VolumeLease,
        response: oneshot::Sender<Result<T, BunError>>,
        result: Result<T, BunError>,
    ) {
        drop(lease);
        let _ = response.send(result);
    }

    /// Test hook: park a snapshot task that has already answered until
    /// the test releases its write lock on `hold`.
    #[cfg(test)]
    pub(super) fn hold_after_answer(hold: Option<&tokio::sync::RwLock<()>>) {
        if let Some(hold) = hold {
            let _parked = hold.blocking_read();
        }
    }

    pub(super) fn volumes_busy(namespace: &str, app: &str) -> BunError {
        crate::grill::snapshot::SnapshotError::Busy {
            namespace: namespace.to_string(),
            app: app.to_string(),
        }
        .into()
    }

    /// The first app in `config` whose volumes a restore owns.
    pub(super) fn restoring_target(&self, config: &Config) -> Option<(String, String)> {
        config.app.iter().find_map(|(name, spec)| {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            self.volume_maintenance
                .restoring(namespace, name)
                .then(|| (namespace.to_string(), name.clone()))
        })
    }

    /// Claim test storage and create managed volumes before launch. The disk
    /// work runs in a task ([`off_loop_work`]); `StillRunning` means ask again,
    /// and a snapshot restore of the app waits until it has finished.
    pub(super) async fn prepare_storage(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        // A deploy accepted before the restore still can't mount the volume
        // while the restore is swapping it.
        if self.volume_maintenance.restoring(namespace, app_name) {
            return Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!(
                    "volumes of {namespace}/{app_name} are being restored from a snapshot"
                ),
            });
        }
        let manager = crate::grill::volume::VolumeManager::new(self.volumes_dir.clone());
        let key = off_loop_work::WorkKey::ProvisionStorage {
            namespace: namespace.to_string(),
            app: app_name.to_string(),
            volumes: format!("{:?}", spec.volumes),
        };
        let (namespace, app) = (namespace.to_string(), app_name.to_string());
        let spec = spec.clone();
        let provisioning = async move {
            tokio::task::spawn_blocking(move || {
                crate::grill::volume::validate_managed_volume_layout(&spec.volumes).map_err(
                    |reason| crate::grill::volume::VolumeError::CreateFailed {
                        path: app.clone(),
                        reason,
                    },
                )?;
                if crate::testkit::lease::valid_test_namespace(&namespace) {
                    manager.prepare_test_storage(&namespace, &app, &spec)?;
                } else {
                    for volume in spec.volumes.iter().filter(|volume| volume.source.is_none()) {
                        manager.create_managed_volume(
                            &namespace,
                            &app,
                            &volume.path,
                            volume.size.as_deref(),
                        )?;
                    }
                }
                Ok::<(), crate::grill::volume::VolumeError>(())
            })
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
        };
        self.finish_off_loop_work(key, None, provisioning)
            .await?
            .map_err(|reason| BunError::DeployFailed {
                app_name: app_name.into(),
                reason,
            })
    }
}
