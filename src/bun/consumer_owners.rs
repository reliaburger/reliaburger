//! Original cluster publication attempts retained until confirmed withdrawal.

use std::{collections::BTreeMap, io};

use serde::{Deserialize, Serialize};

use crate::meat::NodeId;
use crate::onion::types::ServiceEntry;
use crate::onion::{catalog::EndpointCatalog, service_id::ServiceId, service_map::ServiceMap};

const MAX_PUBLICATIONS: usize = 1024;
const MAX_EXPOSURES: usize = 65_536;

/// Stable enrolment identity; a rotating leaf certificate is not a cluster identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerIdentity {
    /// Enrolled consumer responsible for these publication attempts.
    pub node_id: NodeId,
    /// Fingerprint of the cluster trust identity established during enrolment.
    pub cluster_identity: [u8; 32],
}

/// One attempted publication, including the original remote execution identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerPublication {
    /// Committed catalogue generation; zero can represent only an empty catalogue.
    pub generation: u64,
    /// Original committed catalogue, including runtime generation fingerprints.
    pub catalog: EndpointCatalog,
    /// Actual proposed merged local and remote service view.
    pub effective_services: Vec<ServiceEntry>,
    /// Original ingress configuration for this attempted view.
    #[serde(default)]
    pub ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
}

/// Durable permission for replacing a consumer's published view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsumerPhase {
    /// Publication may have partially reached kernel or userspace readers.
    Publishing,
    /// Publication completed; original request guards may still exist.
    Active,
    /// No replacement may publish until original exposures have drained.
    #[default]
    Withdrawing,
    /// Every original exposure has positively withdrawn; compaction is permitted.
    Withdrawn,
}

/// Receipt progress; only ready receipts may be sent or forgotten after acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptPhase {
    /// Original exposures still require local withdrawal proof.
    Pending,
    /// Original exposures are withdrawn; retry until the leader acknowledges.
    Ready,
}

/// Exact original instruction retained across lost receipt responses and restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerReceipt {
    /// Original committed withdrawal, including its allocated destinations.
    pub withdrawal: crate::onion::withdrawal::EndpointWithdrawalInstruction,
    /// Whether local withdrawal has been positively confirmed.
    pub phase: ReceiptPhase,
}

/// Bounded, append-only evidence. Saving this state does not prove publication or drainage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerOwnership {
    /// Identity that must perform or recover these obligations.
    pub identity: ConsumerIdentity,
    /// Original attempts, including attempts whose completion is uncertain.
    pub publications: Vec<ConsumerPublication>,
    /// Current publication/withdrawal permission. Missing evidence defaults to fenced.
    #[serde(default)]
    pub phase: ConsumerPhase,
    /// Original instructions awaiting local proof or a confirmed leader receipt.
    #[serde(default)]
    pub receipts: BTreeMap<u64, ConsumerReceipt>,
}

impl ConsumerOwnership {
    pub(crate) fn validate(&self) -> io::Result<()> {
        crate::cluster::retirement::validate_node_id(&self.identity.node_id.0)
            .map_err(io::Error::other)?;
        if self.publications.len() > MAX_PUBLICATIONS || self.receipts.len() > MAX_PUBLICATIONS {
            return Err(io::Error::other("consumer publication capacity exhausted"));
        }
        let mut exposures = 0usize;
        for (generation, receipt) in &self.receipts {
            if *generation == 0
                || *generation != receipt.withdrawal.generation
                || self
                    .publications
                    .last()
                    .is_none_or(|last| *generation >= last.generation)
                || receipt.withdrawal.services.is_empty()
            {
                return Err(io::Error::other("invalid consumer withdrawal generation"));
            }
            let catalog = EndpointCatalog {
                services: receipt
                    .withdrawal
                    .services
                    .iter()
                    .map(|(id, removed)| (id.clone(), removed.service.clone()))
                    .collect(),
            };
            catalog.validate_allocations().map_err(io::Error::other)?;
            for service in catalog.services.values() {
                exposures = exposures
                    .checked_add(1 + service.backends.len())
                    .filter(|count| *count <= MAX_EXPOSURES)
                    .ok_or_else(|| io::Error::other("consumer exposure capacity exhausted"))?;
            }
        }
        for publication in &self.publications {
            for backends in publication
                .catalog
                .services
                .values()
                .map(|entry| entry.backends.len())
                .chain(
                    publication
                        .effective_services
                        .iter()
                        .map(|entry| entry.backends.len()),
                )
            {
                exposures = exposures
                    .checked_add(1)
                    .and_then(|count| count.checked_add(backends))
                    .filter(|count| *count <= MAX_EXPOSURES)
                    .ok_or_else(|| io::Error::other("consumer exposure capacity exhausted"))?;
            }
            publication
                .catalog
                .validate_allocations()
                .map_err(io::Error::other)?;
            if publication.generation == 0 && !publication.catalog.services.is_empty() {
                return Err(io::Error::other(
                    "nonempty catalogue has no committed generation",
                ));
            }
            let effective = ServiceMap::from_snapshot(&publication.effective_services)
                .map_err(io::Error::other)?;
            let mut ingress = std::collections::HashMap::new();
            for route in &publication.ingress {
                if ingress
                    .insert(
                        (route.namespace.clone(), route.name.clone()),
                        route.config.clone(),
                    )
                    .is_some()
                {
                    return Err(io::Error::other("duplicate consumer ingress assignment"));
                }
            }
            crate::wrapper::routing::RoutingTable::new()
                .rebuild(&effective, &ingress)
                .map_err(io::Error::other)?;
            let remote = ServiceMap::new().with_cluster_catalog_excluding_node(
                &publication.catalog,
                Some(&self.identity.node_id.0),
            );
            for expected in remote.resolve_all() {
                let id = ServiceId::new(&expected.namespace, &expected.app_name);
                let actual = effective
                    .resolve(&id)
                    .ok_or_else(|| io::Error::other("effective view omits a catalogue service"))?;
                if actual.vip != expected.vip
                    || actual.port != expected.port
                    || expected
                        .backends
                        .iter()
                        .any(|backend| !actual.backends.contains(backend))
                {
                    return Err(io::Error::other(
                        "effective view conflicts with original catalogue",
                    ));
                }
            }
        }
        for pair in self.publications.windows(2) {
            if pair[1].generation < pair[0].generation
                || (pair[1].generation == pair[0].generation && pair[1].catalog != pair[0].catalog)
            {
                return Err(io::Error::other(
                    "consumer catalogue generation regressed or changed",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_transition(
    previous: Option<&ConsumerOwnership>,
    next: Option<&ConsumerOwnership>,
) -> io::Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let next = next.ok_or_else(|| io::Error::other("consumer ownership cannot be forgotten"))?;
    if next.identity != previous.identity {
        return Err(io::Error::other("consumer enrolment identity changed"));
    }
    use ConsumerPhase::*;
    let allowed = matches!(
        (previous.phase, next.phase),
        (Active, Active | Withdrawing)
            | (Publishing, Publishing | Active | Withdrawing)
            | (Withdrawing, Withdrawing | Withdrawn)
            | (Withdrawn, Withdrawn | Publishing | Withdrawing)
    );
    if !allowed {
        return Err(io::Error::other(
            "consumer withdrawal permission is missing",
        ));
    }
    if previous.phase == Withdrawn {
        if let Some(last) = previous.publications.last() {
            let next_last = next
                .publications
                .last()
                .ok_or_else(|| io::Error::other("consumer generation fence cannot be forgotten"))?;
            if next_last.generation < last.generation
                || (next_last.generation == last.generation && next_last.catalog != last.catalog)
            {
                return Err(io::Error::other(
                    "consumer generation fence regressed or changed",
                ));
            }
        }
    } else if !next.publications.starts_with(&previous.publications)
        || (next.phase != Withdrawing && next.publications != previous.publications)
    {
        return Err(io::Error::other(
            "original consumer publication history must be retained",
        ));
    }
    for (generation, receipt) in &previous.receipts {
        match next.receipts.get(generation) {
            None if receipt.phase == ReceiptPhase::Ready => {}
            Some(retained)
                if retained.withdrawal == receipt.withdrawal
                    && (retained.phase == receipt.phase
                        || (receipt.phase == ReceiptPhase::Pending
                            && previous.phase == Withdrawn
                            && next.phase == Withdrawn)) => {}
            _ => {
                return Err(io::Error::other(
                    "original consumer receipt has no withdrawal or acknowledgement permission",
                ));
            }
        }
    }
    for (generation, receipt) in &next.receipts {
        if !previous.receipts.contains_key(generation) && receipt.phase != ReceiptPhase::Pending {
            return Err(io::Error::other(
                "new consumer receipt has no withdrawal proof",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> ConsumerOwnership {
        serde_json::from_value(serde_json::json!({
            "identity": {"node_id": "consumer", "cluster_identity": vec![7; 32]},
            "publications": [
                {"generation": 1, "catalog": {"services": {}}, "effective_services": []},
                {"generation": 2, "catalog": {"services": {}}, "effective_services": []}
            ],
            "phase": "Active", "receipts": {}
        }))
        .unwrap()
    }

    #[test]
    fn consumer_compacts_only_after_confirmed_withdrawal_and_keeps_generation_fence() {
        let old = owner();
        let mut compact = old.clone();
        compact.publications.remove(0);
        assert!(validate_transition(Some(&old), Some(&compact)).is_err());
        let mut withdrawing = old.clone();
        withdrawing.phase = ConsumerPhase::Withdrawing;
        assert!(validate_transition(Some(&old), Some(&withdrawing)).is_ok());
        let mut withdrawn = withdrawing.clone();
        withdrawn.phase = ConsumerPhase::Withdrawn;
        assert!(validate_transition(Some(&withdrawing), Some(&withdrawn)).is_ok());
        compact.phase = ConsumerPhase::Publishing;
        assert!(validate_transition(Some(&withdrawn), Some(&compact)).is_ok());
        compact.publications[0].generation = 1;
        assert!(validate_transition(Some(&withdrawn), Some(&compact)).is_err());
        assert!(validate_transition(Some(&old), Some(&withdrawn)).is_err());
    }

    #[test]
    fn receipt_requires_withdrawal_then_confirmation_before_forgetting() {
        let mut old = owner();
        let receipt: ConsumerReceipt = serde_json::from_value(serde_json::json!({
            "withdrawal": {"generation": 1, "services": {}}, "phase": "Pending"
        }))
        .unwrap();
        old.receipts.insert(1, receipt);
        let mut next = old.clone();
        next.receipts.clear();
        assert!(validate_transition(Some(&old), Some(&next)).is_err());
        next = old.clone();
        next.receipts.get_mut(&1).unwrap().phase = ReceiptPhase::Ready;
        assert!(validate_transition(Some(&old), Some(&next)).is_err());
        old.phase = ConsumerPhase::Withdrawn;
        next.phase = ConsumerPhase::Withdrawn;
        assert!(validate_transition(Some(&old), Some(&next)).is_ok());
        let mut acknowledged = next.clone();
        acknowledged.receipts.clear();
        assert!(validate_transition(Some(&next), Some(&acknowledged)).is_ok());
    }
}
