//! Keep startup cleanup owned while cluster receipt delivery becomes available.

use super::{BunAgent, BunError, Grill, InstanceId};

fn startup_refusal(reason: String) -> BunError {
    BunError::AdoptionState(format!("startup cleanup {reason}"))
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    pub(super) async fn defer_startup_retirement(
        &mut self,
        id: &InstanceId,
    ) -> Result<bool, BunError> {
        if self.consumer_owner().is_none() {
            return Ok(false);
        }
        let launches = self
            .complete_runtime_inventory(super::RUNTIME_INVENTORY_TIMEOUT, startup_refusal)
            .await?;
        let launch = launches
            .into_iter()
            .find(|launch| launch.instance_id == *id)
            .ok_or_else(|| {
                BunError::AdoptionState("startup cleanup lost original runtime".into())
            })?;
        if matches!(
            launch.network_reference,
            Some(crate::grill::runc_intent::NetworkReferenceState::Released(
                _
            ))
        ) || (launch.spec.port_mapping.is_none() && !self.network_references.contains_key(id))
        {
            return Ok(false);
        }
        if self
            .startup_retirements
            .iter()
            .any(|pending| pending.instance_id == *id)
        {
            return Ok(true);
        }
        if let Some(port) = launch.spec.port_mapping {
            self.supervisor
                .port_allocator
                .reserve(port.host_port)
                .await?;
        }
        self.startup_retirements.push_back(launch);
        self.startup_cleanup_pending = true;
        if let Some(readiness) = &self.readiness {
            readiness.register("discovery:startup-cleanup", true).await;
        }
        Ok(true)
    }

    pub(super) async fn drive_startup_retirements(&mut self) {
        let Some(original) = self.startup_retirements.front().cloned() else {
            self.finish_startup_retirements().await;
            return;
        };
        let cleanup = async {
            let launches = self
                .complete_runtime_inventory(super::RUNTIME_INVENTORY_TIMEOUT, startup_refusal)
                .await?;
            if !launches.iter().any(|current| {
                current.instance_id == original.instance_id
                    && current.generation == original.generation
                    && current.launched(&original.spec)
            }) {
                return Err(BunError::AdoptionState(
                    "startup cleanup execution changed".into(),
                ));
            }
            self.retire_instance_artifacts(&original.instance_id)
                .await?;
            if let Some(port) = original.spec.port_mapping {
                // LOOP-INLINE: in-memory lock, no I/O
                self.supervisor
                    .port_allocator
                    .release(port.host_port)
                    .await?;
            }
            Ok::<(), BunError>(())
        }
        .await;
        match cleanup {
            Ok(()) => {
                // Journalling the release took two writes. The discovery
                // recovery that follows the last retirement takes two more,
                // so it waits for the next tick: four in one turn come near
                // two runtime budgets on a slow disk (#422).
                self.startup_retirements.pop_front();
            }
            // Its disk cleanup is running off the loop; the next tick
            // collects it. Nothing is wrong, so readiness stays as it is.
            Err(BunError::StillRunning { .. }) => {}
            Err(error) => {
                // Rotate ownership so one unavailable producer does not starve others.
                if let Some(pending) = self.startup_retirements.pop_front() {
                    self.startup_retirements.push_back(pending);
                }
                if let Some(readiness) = &self.readiness {
                    // LOOP-INLINE: in-memory lock, no I/O
                    readiness
                        .degraded("discovery:startup-cleanup", error.to_string())
                        .await;
                }
            }
        }
    }

    async fn finish_startup_retirements(&mut self) {
        if self.startup_cleanup_pending && self.startup_retirements.is_empty() {
            let result = self.finish_discovery_recovery().await;
            if result.is_ok() {
                self.startup_cleanup_pending = false;
            }
            if let Some(readiness) = &self.readiness {
                // LOOP-INLINE: in-memory lock, no I/O
                match result {
                    Ok(()) => readiness.ready("discovery:startup-cleanup").await,
                    Err(error) => {
                        // LOOP-INLINE: in-memory lock, no I/O
                        readiness
                            .degraded("discovery:startup-cleanup", error.to_string())
                            .await
                    }
                }
            }
        }
    }
}
