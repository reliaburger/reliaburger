//! Retain runtime addresses, host ports and original records until committed remote release.

use super::{BunAgent, BunError, DiscoveryOwnership, Grill, InstanceId};
use crate::onion::producer::ProducerReleaseConfirmation;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Configure enrolled leader transport. Durable discovery enables the release gate.
    pub fn set_producer_release_client(
        &mut self,
        client: crate::cluster::producer::ProducerReleaseClient,
    ) {
        self.producer_release_client = Some(client);
        // Requests in flight belong to the previous transport.
        self.producer_releases.clear();
    }

    /// Call only after observed runtime exit and local request drainage.
    pub(super) async fn confirm_producer_release(
        &mut self,
        id: &InstanceId,
    ) -> Result<Option<ProducerReleaseConfirmation>, BunError> {
        if matches!(self.discovery_ownership, DiscoveryOwnership::Disabled)
            || self.cluster.is_none()
        {
            return Ok(None);
        }
        let refuse = |reason: String| BunError::RetirementState {
            instance_id: id.clone(),
            reason,
        };
        if matches!(self.discovery_ownership, DiscoveryOwnership::Uncertain) {
            return Err(refuse(
                "discovery ownership is uncertain; producer release refused".into(),
            ));
        }
        let instance = self.supervisor.get_instance(id);
        if instance.is_some_and(|instance| instance.host_port.is_none())
            && !self.network_references.contains_key(id)
        {
            return Ok(None);
        }
        let launches = self
            .complete_runtime_inventory(super::LOOP_RUNTIME_INVENTORY_TIMEOUT, |reason| {
                refuse(format!("producer {reason}"))
            })
            .await?;
        let mut originals = launches.iter().filter(|launch| launch.instance_id == *id);
        let original = originals
            .next()
            .ok_or_else(|| refuse("original producer execution is missing".into()))?;
        if originals.next().is_some()
            || instance.is_some_and(|instance| {
                instance.host_port
                    != original
                        .spec
                        .port_mapping
                        .as_ref()
                        .map(|mapping| mapping.host_port)
                    || instance
                        .oci_spec
                        .as_ref()
                        .is_some_and(|spec| spec != &original.spec)
            })
        {
            return Err(refuse(
                "original producer execution conflicts with owned allocation".into(),
            ));
        }
        if original.spec.port_mapping.is_none() && !self.network_references.contains_key(id) {
            return Ok(None);
        }
        if matches!(original.network_reference,
            Some(crate::grill::runc_intent::NetworkReferenceState::Released(ref reference))
                if reference.instance_id == *id
                    && original.generation == crate::grill::RuntimeGeneration::runc(reference.generation.as_str()))
        {
            return Ok(None);
        }
        let execution = crate::grill::RuntimeExecution {
            instance_id: id.clone(),
            generation: original.generation.clone(),
        };
        let client = self
            .producer_release_client
            .as_ref()
            .ok_or_else(|| refuse("producer release transport is unavailable".into()))?;
        let node_id = &self
            .cluster
            .as_ref()
            .ok_or_else(|| refuse("producer cluster identity is unavailable".into()))?
            .local_node_id
            .0;
        // The request runs as its own task, so a slow or unreachable leader
        // can't hold the agent loop for the client's whole timeout. Callers
        // treat the refusal as "retry later"; the next attempt collects the
        // answer instead of asking again.
        let requested = self.producer_releases.contains_key(&execution);
        let pending = self
            .producer_releases
            .entry(execution.clone())
            .or_insert_with(|| {
                let client = client.clone();
                let node_id = node_id.clone();
                let execution = execution.clone();
                tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                    client
                        .confirm(&node_id, &execution)
                        .await
                        .map_err(|error| error.to_string())
                }))
            });
        // Only a fresh request waits; a retry just collects a finished answer.
        if requested && !pending.is_finished() {
            return Err(refuse("producer release awaits leader confirmation".into()));
        }
        let outcome = tokio::select! {
            _ = self.shutdown.cancelled() => {
                return Err(refuse("producer release interrupted; ownership retained".into()));
            }
            outcome = tokio::time::timeout(PRODUCER_RELEASE_WAIT, &mut *pending) => outcome,
        };
        let Ok(joined) = outcome else {
            return Err(refuse("producer release awaits leader confirmation".into()));
        };
        self.producer_releases.remove(&execution);
        let confirmation = joined
            .map_err(|error| refuse(error.to_string()))?
            .map_err(refuse)?;
        Ok(Some(confirmation))
    }
}

/// How long a caller waits for the leader before retiring on a later attempt.
/// A healthy leader answers well within it; a slow one no longer stalls the
/// single agent loop for every pending retirement.
const PRODUCER_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(1);
