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

impl ConsumerPublication {
    /// Whether this view still exposes anything the withdrawal removes.
    pub(crate) fn intersects(
        &self,
        withdrawal: &crate::onion::withdrawal::EndpointWithdrawalInstruction,
    ) -> bool {
        withdrawal.services.values().any(|removed| {
            self.effective_services
                .iter()
                .any(|entry| removed.retire_vip && entry.vip == removed.service.vip)
                || self.catalog.services.values().any(|service| {
                    service.backends.iter().any(|candidate| {
                        removed.service.backends.iter().any(|original| {
                            candidate.node_id == original.node_id
                                && candidate.node_ip == original.node_ip
                                && candidate.host_port == original.host_port
                                && (original.execution.is_none()
                                    || candidate.execution == original.execution)
                        })
                    })
                })
        })
    }
}

/// Durable permission for replacing a consumer's published view.
///
/// `publications` holds every view that may still be exposed: the newest one in
/// the kernel and userspace, earlier ones through requests that captured their
/// backends. A view is forgotten only once those requests have released.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsumerPhase {
    /// The newest publication may have partially reached kernel or userspace readers.
    Publishing,
    /// The newest publication is complete; earlier ones may still hold captured requests.
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
        (Active, Active | Publishing | Withdrawing)
            | (Publishing, Publishing | Active | Withdrawing)
            | (Withdrawing, Withdrawing | Withdrawn)
            | (Withdrawn, Withdrawn | Publishing | Withdrawing)
    );
    if !allowed {
        return Err(io::Error::other(
            "consumer withdrawal permission is missing",
        ));
    }
    let history = &previous.publications;
    if (previous.phase, next.phase) == (Active, Publishing) {
        // Replacing in place appends exactly one view and keeps every earlier
        // one, because requests may still hold their backends.
        if next.publications.len() != history.len() + 1 || !next.publications.starts_with(history) {
            return Err(io::Error::other(
                "original consumer publication history must be retained",
            ));
        }
    } else if (previous.phase, next.phase) == (Active, Active) {
        // Compaction keeps a suffix that ends with the published view. The
        // caller forgets earlier views only after their requests released.
        if next.publications != *history
            && (next.publications.is_empty() || !history.ends_with(&next.publications))
        {
            return Err(io::Error::other(
                "consumer compaction must keep the published view",
            ));
        }
    } else if previous.phase == Withdrawn {
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
                            && receipt_proven(previous, next, &receipt.withdrawal))) => {}
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

/// A pending receipt becomes ready after a full withdrawal, or once no view
/// that may still be exposed contains anything the withdrawal removes.
fn receipt_proven(
    previous: &ConsumerOwnership,
    next: &ConsumerOwnership,
    withdrawal: &crate::onion::withdrawal::EndpointWithdrawalInstruction,
) -> bool {
    use ConsumerPhase::*;
    match (previous.phase, next.phase) {
        (Withdrawn, Withdrawn) => true,
        (Active, Active) => !next
            .publications
            .iter()
            .any(|publication| publication.intersects(withdrawal)),
        _ => false,
    }
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

    fn web_catalog() -> serde_json::Value {
        serde_json::json!({"services": {"default__web": {
            "vip": "127.128.0.9", "port": 8080,
            "backends": [{"execution": null, "node_id": "producer",
                "node_ip": "10.0.0.1", "host_port": 30001, "healthy": true}]
        }}})
    }

    fn withdrawal(generation: u64) -> crate::onion::withdrawal::EndpointWithdrawalInstruction {
        let service = web_catalog()["services"]["default__web"].clone();
        serde_json::from_value(serde_json::json!({
            "generation": generation,
            "services": {"default__web": {"service": service, "retire_vip": false}}
        }))
        .unwrap()
    }

    #[test]
    fn consumer_compaction_keeps_the_published_view_and_generation_fence() {
        let old = owner();
        let mut compact = old.clone();
        compact.publications.remove(0);
        assert!(validate_transition(Some(&old), Some(&compact)).is_ok());
        let mut dropped_current = old.clone();
        dropped_current.publications.pop();
        assert!(
            validate_transition(Some(&old), Some(&dropped_current)).is_err(),
            "compaction forgot the published view"
        );
        let mut emptied = old.clone();
        emptied.publications.clear();
        assert!(validate_transition(Some(&old), Some(&emptied)).is_err());
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
    fn in_place_publication_appends_exactly_one_view() {
        let old = owner();
        let mut next = old.clone();
        next.phase = ConsumerPhase::Publishing;
        let mut third = next.publications[1].clone();
        third.generation = 3;
        next.publications.push(third.clone());
        assert!(validate_transition(Some(&old), Some(&next)).is_ok());
        let mut replaced = next.clone();
        replaced.publications.remove(0);
        assert!(
            validate_transition(Some(&old), Some(&replaced)).is_err(),
            "publishing in place forgot a view that may hold requests"
        );
        let mut two = next.clone();
        two.publications.push(third);
        assert!(validate_transition(Some(&old), Some(&two)).is_err());
    }

    #[test]
    fn receipt_waits_until_no_retained_view_exposes_the_withdrawal() {
        let mut old = owner();
        old.publications[0].catalog = serde_json::from_value(web_catalog()).unwrap();
        old.publications[1].generation = 3;
        old.receipts.insert(
            2,
            ConsumerReceipt {
                withdrawal: withdrawal(2),
                phase: ReceiptPhase::Pending,
            },
        );
        let mut cleared = old.clone();
        cleared.receipts.clear();
        assert!(validate_transition(Some(&old), Some(&cleared)).is_err());
        let mut early = old.clone();
        early.receipts.get_mut(&2).unwrap().phase = ReceiptPhase::Ready;
        assert!(
            validate_transition(Some(&old), Some(&early)).is_err(),
            "a retained view still exposes the withdrawn backend"
        );
        let mut compacted = early.clone();
        compacted.publications.remove(0);
        assert!(validate_transition(Some(&old), Some(&compacted)).is_ok());
        let mut acknowledged = compacted.clone();
        acknowledged.receipts.clear();
        assert!(validate_transition(Some(&compacted), Some(&acknowledged)).is_ok());
    }

    #[test]
    fn full_withdrawal_still_proves_receipts() {
        let mut old = owner();
        old.receipts.insert(
            1,
            ConsumerReceipt {
                withdrawal: withdrawal(1),
                phase: ReceiptPhase::Pending,
            },
        );
        old.phase = ConsumerPhase::Withdrawn;
        let mut next = old.clone();
        next.receipts.get_mut(&1).unwrap().phase = ReceiptPhase::Ready;
        assert!(validate_transition(Some(&old), Some(&next)).is_ok());
    }
}
