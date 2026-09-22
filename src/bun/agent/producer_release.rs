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
    }

    /// Call only after observed runtime exit and local request drainage.
    pub(super) async fn confirm_producer_release(
        &self,
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
        let launches = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            self.supervisor.grill().launch_inventory(),
        )
        .await
        .map_err(|_| refuse("producer runtime inventory timed out".into()))??
        .ok_or_else(|| refuse("producer runtime inventory is unavailable".into()))?;
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
        let confirmation = tokio::select! {
            _ = self.shutdown.cancelled() => return Err(refuse("producer release interrupted; ownership retained".into())),
            result = client.confirm(node_id, &execution) => result.map_err(|error| refuse(error.to_string()))?,
        };
        Ok(Some(confirmation))
    }
}
