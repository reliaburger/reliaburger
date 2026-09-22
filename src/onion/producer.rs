//! Permanent execution fences precede producer address release.

use super::{catalog::EndpointCatalog, withdrawal::EndpointWithdrawals};
use crate::grill::{InstanceId, RuntimeExecution};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Maximum permanently retired executions; reaching this limit never evicts history.
pub const MAX_PRODUCER_RETIREMENTS: usize = 65_536;

/// Permanent node-scoped generation fences, retained after all consumers acknowledge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerRetirements {
    executions: BTreeMap<String, BTreeMap<String, InstanceId>>,
}

/// An authenticated producer's request to fence one original runtime execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerRetirementRequest {
    /// Required wire and state format compatibility.
    pub compatibility: crate::compatibility::Compatibility,
    /// Original execution from the producer's durable runtime inventory.
    pub execution: RuntimeExecution,
}

impl ProducerRetirements {
    /// Prepare a fence without forgetting history or mutating the original state.
    pub fn plan_retirement(
        &self,
        node_id: &str,
        execution: &RuntimeExecution,
    ) -> Result<Self, String> {
        crate::cluster::retirement::validate_node_id(node_id).map_err(String::from)?;
        let identity = crate::grill::InstanceIdentity::parse(&execution.instance_id.0)
            .filter(|id| {
                id.instance_id() == execution.instance_id
                    && crate::config::valid_workload_label(&id.namespace)
                    && crate::config::valid_workload_label(&id.app)
            })
            .ok_or_else(|| "invalid producer execution identity".to_string())?;
        let _ = identity;
        if let Some(original) = self
            .executions
            .get(node_id)
            .and_then(|entries| entries.get(execution.generation.as_str()))
        {
            return if original == &execution.instance_id {
                Ok(self.clone())
            } else {
                Err("producer generation belongs to another instance".into())
            };
        }
        if self.executions.values().map(BTreeMap::len).sum::<usize>() >= MAX_PRODUCER_RETIREMENTS {
            return Err("producer retirement capacity exhausted".into());
        }
        let mut next = self.clone();
        next.executions.entry(node_id.into()).or_default().insert(
            execution.generation.as_str().into(),
            execution.instance_id.clone(),
        );
        Ok(next)
    }

    /// Missing generation evidence from a previously fenced producer cannot reintroduce an endpoint.
    pub fn blocks(&self, node_id: &str, execution: Option<&RuntimeExecution>) -> bool {
        let Some(entries) = self.executions.get(node_id) else {
            return false;
        };
        execution.is_none_or(|execution| entries.contains_key(execution.generation.as_str()))
    }

    /// Remove fenced and uncorrelated backends while preserving service allocations.
    pub fn withdraw(&self, catalog: &EndpointCatalog) -> EndpointCatalog {
        let mut next = catalog.clone();
        for service in next.services.values_mut() {
            service
                .backends
                .retain(|backend| !self.blocks(&backend.node_id, backend.execution.as_ref()));
        }
        next
    }

    /// Confirm that a fenced execution has no historical consumer obligations.
    pub fn release_confirmed(
        &self,
        node_id: &str,
        execution: &RuntimeExecution,
        withdrawals: &EndpointWithdrawals,
    ) -> bool {
        self.executions
            .get(node_id)
            .and_then(|entries| entries.get(execution.generation.as_str()))
            == Some(&execution.instance_id)
            && !withdrawals.pending.values().any(|withdrawal| {
                !withdrawal.consumers.is_empty()
                    && withdrawal.services.values().any(|service| {
                        service.service.backends.iter().any(|backend| {
                            backend.node_id == node_id
                                && backend.execution.as_ref().is_none_or(|original| {
                                    original.generation == execution.generation
                                })
                        })
                    })
            })
    }
}
