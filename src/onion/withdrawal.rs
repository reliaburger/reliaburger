//! Replicated obligations for removing original discovery publications.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::catalog::{CatalogService, EndpointCatalog};

/// Maximum retained generations; reaching the bound never evicts an obligation.
pub const MAX_WITHDRAWAL_GENERATIONS: usize = 1_024;
/// Maximum retained service and backend exposures across all generations.
pub const MAX_WITHDRAWAL_EXPOSURES: usize = 65_536;
/// Maximum outstanding consumer confirmations across all generations.
pub const MAX_WITHDRAWAL_CONFIRMATIONS: usize = 262_144;

/// Removed destinations and ownership of their original virtual address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceWithdrawal {
    /// Original allocation and only the backends that must be withdrawn.
    pub service: CatalogService,
    /// Whether the virtual address itself has also left the active catalogue.
    pub retire_vip: bool,
}

/// Original routes a registered consumer must remove before acknowledging cleanup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointWithdrawalInstruction {
    /// Original publication generation whose routes may still be owned locally.
    pub generation: u64,
    /// Original service allocations and only their removed destinations.
    pub services: BTreeMap<String, ServiceWithdrawal>,
}

/// A node's confirmation that one original publication has been withdrawn locally.
/// The HTTP listener supplies the consumer identity from its TLS certificate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointWithdrawalReceipt {
    /// Explicit protocol and durable-state compatibility for this receipt.
    pub compatibility: crate::compatibility::Compatibility,
    /// Original withdrawn publication, never the current catalogue generation.
    pub generation: u64,
}

/// A publication's withdrawn exposures and the consumers still owing confirmation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointWithdrawal {
    /// Qualified service identities with their original destinations.
    pub services: BTreeMap<String, ServiceWithdrawal>,
    /// Durable node identities; absence from gossip does not discharge them.
    pub consumers: BTreeSet<String>,
}

/// Committed publication sequence and outstanding remote withdrawal obligations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointWithdrawals {
    /// Generation of the active catalogue, starting at zero before publication.
    pub generation: u64,
    /// Original publication generations, retained until every consumer confirms.
    pub pending: BTreeMap<u64, EndpointWithdrawal>,
}

/// Refusal to advance publication without losing retained cleanup evidence.
#[derive(Debug, thiserror::Error)]
pub enum WithdrawalError {
    /// The sequence cannot advance without repeating an earlier identity.
    #[error("endpoint publication generation exhausted")]
    GenerationExhausted,
    /// Retained generations, exposures or confirmation obligations reached a bound.
    #[error("endpoint withdrawal capacity reached; outstanding confirmations must finish")]
    CapacityReached,
    /// Stored history conflicts with the active publication sequence.
    #[error("endpoint withdrawal generation conflicts with publication history")]
    GenerationConflict,
    /// The candidate contains an invalid service or conflicting virtual addresses.
    #[error("invalid endpoint catalogue: {0}")]
    InvalidCatalogue(#[from] super::types::OnionError),
    /// Another publication still owns the requested virtual address remotely.
    #[error("endpoint catalogue reuses virtual address {vip} awaiting withdrawal")]
    RetiredVipInUse { vip: std::net::Ipv4Addr },
}

impl EndpointWithdrawals {
    /// Virtual addresses retained until all original consumers confirm withdrawal.
    pub fn reserved_vips(&self) -> impl Iterator<Item = super::vip::VirtualIP> + '_ {
        self.pending
            .values()
            .flat_map(|withdrawal| withdrawal.services.values())
            .filter_map(|removed| removed.retire_vip.then_some(removed.service.vip))
    }

    /// Prepare an atomic publication transition without changing the original ledger.
    pub fn plan_publication(
        &self,
        previous: &EndpointCatalog,
        next: &EndpointCatalog,
        consumers: &BTreeSet<String>,
    ) -> Result<Self, WithdrawalError> {
        previous.validate_allocations()?;
        next.validate_allocations()?;
        self.check_reservations(next)?;
        if self
            .pending
            .keys()
            .any(|generation| *generation >= self.generation)
        {
            return Err(WithdrawalError::GenerationConflict);
        }
        if previous == next {
            return Ok(self.clone());
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(WithdrawalError::GenerationExhausted)?;
        let mut planned = self.clone();
        if !consumers.is_empty() {
            let services = removed_exposures(previous, next);
            if !services.is_empty() {
                planned.pending.insert(
                    self.generation,
                    EndpointWithdrawal {
                        services,
                        consumers: consumers.clone(),
                    },
                );
            }
        }
        planned.check_capacity()?;
        // Also protect allocations withdrawn by this very publication.
        planned.check_reservations(next)?;
        planned.generation = generation;
        Ok(planned)
    }

    /// Discharge only a permanently fenced consumer's outstanding obligations.
    pub fn retire_consumer(&mut self, node_id: &str) {
        self.pending.retain(|_, withdrawal| {
            withdrawal.consumers.remove(node_id);
            !withdrawal.consumers.is_empty()
        });
    }

    fn check_reservations(&self, next: &EndpointCatalog) -> Result<(), WithdrawalError> {
        let reserved: std::collections::HashSet<_> = self.reserved_vips().collect();
        for service in next.services.values() {
            if reserved.contains(&service.vip) {
                return Err(WithdrawalError::RetiredVipInUse { vip: service.vip.0 });
            }
        }
        Ok(())
    }

    /// Largest share of any ledger bound in use, from 0.0 upwards. Publication
    /// is refused once this passes 1.0.
    pub fn occupancy(&self) -> f64 {
        let (exposures, confirmations) = self.usage();
        [
            self.pending.len() as f64 / MAX_WITHDRAWAL_GENERATIONS as f64,
            exposures as f64 / MAX_WITHDRAWAL_EXPOSURES as f64,
            confirmations as f64 / MAX_WITHDRAWAL_CONFIRMATIONS as f64,
        ]
        .into_iter()
        .fold(0.0, f64::max)
    }

    /// How many retained generations each consumer still has to confirm.
    pub fn owed_by_consumer(&self) -> BTreeMap<&str, usize> {
        let mut owed = BTreeMap::new();
        for consumer in self.pending.values().flat_map(|w| &w.consumers) {
            *owed.entry(consumer.as_str()).or_insert(0) += 1;
        }
        owed
    }

    fn usage(&self) -> (usize, usize) {
        let mut exposures = 0usize;
        let mut confirmations = 0usize;
        for withdrawal in self.pending.values() {
            confirmations = confirmations.saturating_add(withdrawal.consumers.len());
            for removed in withdrawal.services.values() {
                exposures = exposures
                    .saturating_add(1)
                    .saturating_add(removed.service.backends.len());
            }
        }
        (exposures, confirmations)
    }

    fn check_capacity(&self) -> Result<(), WithdrawalError> {
        let (exposures, confirmations) = self.usage();
        if self.pending.len() > MAX_WITHDRAWAL_GENERATIONS
            || exposures > MAX_WITHDRAWAL_EXPOSURES
            || confirmations > MAX_WITHDRAWAL_CONFIRMATIONS
        {
            return Err(WithdrawalError::CapacityReached);
        }
        Ok(())
    }
}

fn removed_exposures(
    previous: &EndpointCatalog,
    next: &EndpointCatalog,
) -> BTreeMap<String, ServiceWithdrawal> {
    let mut removed = BTreeMap::new();
    for (id, original) in &previous.services {
        let replacement = next.services.get(id);
        let retire_vip = replacement.is_none_or(|service| service.vip != original.vip);
        let same_route = replacement
            .filter(|service| service.vip == original.vip && service.port == original.port);
        let backends = match same_route {
            None => original.backends.clone(),
            Some(service) => {
                // Health and ordering change availability, not the original destination owner.
                let retained: std::collections::HashSet<_> = service
                    .backends
                    .iter()
                    .map(|backend| {
                        (
                            &backend.node_id,
                            backend.node_ip,
                            backend.host_port,
                            &backend.execution,
                        )
                    })
                    .collect();
                // A backend first published before its node knew the runtime
                // execution, then again with it, is the same destination
                // learning who owns it, not a withdrawal. A withdrawal with no
                // execution matches every execution at that address, so no
                // consumer could ever confirm it while the address stays in
                // the catalogue, and the leader would refuse every producer
                // release on that node for good.
                let refined = |backend: &crate::onion::catalog::CatalogBackend| {
                    backend.execution.is_none()
                        && service.backends.iter().any(|candidate| {
                            candidate.node_id == backend.node_id
                                && candidate.node_ip == backend.node_ip
                                && candidate.host_port == backend.host_port
                        })
                };
                original
                    .backends
                    .iter()
                    .filter(|backend| {
                        !retained.contains(&(
                            &backend.node_id,
                            backend.node_ip,
                            backend.host_port,
                            &backend.execution,
                        )) && !refined(backend)
                    })
                    .cloned()
                    .collect()
            }
        };
        if same_route.is_some() && backends.is_empty() {
            continue;
        }
        removed.insert(
            id.clone(),
            ServiceWithdrawal {
                service: CatalogService {
                    vip: original.vip,
                    port: original.port,
                    backends,
                },
                retire_vip,
            },
        );
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::{InstanceId, RuntimeExecution};
    use crate::onion::catalog::CatalogBackend;
    use crate::onion::service_id::ServiceId;

    fn catalogue(generation: char) -> EndpointCatalog {
        EndpointCatalog::rebuild([(
            ServiceId::new("default", "api"),
            80,
            vec![CatalogBackend {
                node_id: "producer".into(),
                node_ip: "10.0.0.1".parse().unwrap(),
                host_port: 30001,
                healthy: true,
                execution: Some(RuntimeExecution {
                    instance_id: InstanceId("default__api-0".into()),
                    generation: generation.to_string().repeat(64).try_into().unwrap(),
                }),
            }],
        )])
        .unwrap()
    }

    #[test]
    fn replacement_retains_original_execution_and_consumer_obligations() {
        let first = catalogue('a');
        let second = catalogue('b');
        let consumers = BTreeSet::from(["offline".into(), "online".into()]);
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &first, &consumers)
            .unwrap();
        assert_eq!(ledger.generation, 1);
        assert!(ledger.pending.is_empty());
        let replacement = ledger
            .plan_publication(&first, &second, &consumers)
            .unwrap();
        assert_eq!(replacement.generation, 2);
        let original = &replacement.pending[&1];
        assert_eq!(original.consumers, consumers);
        assert_eq!(
            original.services["default__api"].service,
            first.services["default__api"]
        );
        assert!(!original.services["default__api"].retire_vip);
        assert!(
            ledger.pending.is_empty(),
            "planning must leave committed state intact"
        );
    }

    #[test]
    fn unchanged_backends_and_health_changes_do_not_create_drain_obligations() {
        let first = catalogue('a');
        let consumers = BTreeSet::from(["reader".into()]);
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &first, &consumers)
            .unwrap();
        assert_eq!(
            ledger.plan_publication(&first, &first, &consumers).unwrap(),
            ledger
        );
        let mut unhealthy = first.clone();
        unhealthy.services.get_mut("default__api").unwrap().backends[0].healthy = false;
        let updated = ledger
            .plan_publication(&first, &unhealthy, &consumers)
            .unwrap();
        assert_eq!(updated.generation, 2);
        assert!(updated.pending.is_empty());
        let mut added = unhealthy.clone();
        let mut extra = catalogue('b').services["default__api"].backends[0].clone();
        extra.host_port = 30002;
        extra.execution.as_mut().unwrap().instance_id = InstanceId("default__api-1".into());
        added
            .services
            .get_mut("default__api")
            .unwrap()
            .backends
            .push(extra);
        assert!(
            updated
                .plan_publication(&unhealthy, &added, &consumers)
                .unwrap()
                .pending
                .is_empty()
        );
    }

    /// Z6.7: every new instance's first report reached the catalogue without
    /// its runtime execution. The next catalogue named the execution, the
    /// ledger recorded a withdrawal of the execution-less entry, and on the
    /// laptop cluster no producer release on that node ever succeeded again.
    #[test]
    fn learning_a_backends_execution_is_not_a_withdrawal() {
        let consumers = BTreeSet::from(["reader".into()]);
        let known = catalogue('a');
        let mut unknown = known.clone();
        let backend = &mut unknown.services.get_mut("default__api").unwrap().backends[0];
        backend.execution = None;
        backend.healthy = false;
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &unknown, &consumers)
            .unwrap();
        let learned = ledger
            .plan_publication(&unknown, &known, &consumers)
            .unwrap();
        assert_eq!(learned.generation, 2);
        assert!(learned.pending.is_empty(), "{:?}", learned.pending);

        // The same address gone from the catalogue is still a withdrawal.
        let removed = ledger
            .plan_publication(&unknown, &EndpointCatalog::default(), &consumers)
            .unwrap();
        assert_eq!(removed.pending.len(), 1);
        // So is an execution-less entry replaced at a different port.
        let mut moved = known.clone();
        moved.services.get_mut("default__api").unwrap().backends[0].host_port = 30002;
        assert_eq!(
            ledger
                .plan_publication(&unknown, &moved, &consumers)
                .unwrap()
                .pending
                .len(),
            1
        );
    }

    #[test]
    fn later_publications_and_registrations_preserve_older_withdrawals() {
        let first = catalogue('a');
        let second = catalogue('b');
        let original_consumers = BTreeSet::from(["offline".into()]);
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &first, &original_consumers)
            .unwrap()
            .plan_publication(&first, &second, &original_consumers)
            .unwrap();
        let consumers = BTreeSet::from(["offline".into(), "new-reader".into()]);
        let removed = ledger
            .plan_publication(&second, &EndpointCatalog::default(), &consumers)
            .unwrap();
        assert_eq!(removed.pending[&1], ledger.pending[&1]);
        assert_eq!(removed.pending[&1].consumers, original_consumers);
        assert_eq!(removed.pending[&2].consumers, consumers);
        assert!(removed.pending[&2].services["default__api"].retire_vip);
        let mut restored: EndpointWithdrawals =
            serde_json::from_slice(&serde_json::to_vec(&removed).unwrap()).unwrap();
        assert_eq!(restored, removed);
        restored.retire_consumer("offline");
        assert!(!restored.pending.contains_key(&1));
        assert_eq!(
            restored.pending[&2].consumers,
            BTreeSet::from(["new-reader".into()])
        );
        restored.retire_consumer("new-reader");
        assert!(restored.pending.is_empty());
        assert_eq!(restored.generation, 3);
    }

    #[test]
    fn removed_empty_services_retain_their_allocation_but_no_readers_need_no_receipt() {
        let mut first = catalogue('a');
        first
            .services
            .get_mut("default__api")
            .unwrap()
            .backends
            .clear();
        let consumers = BTreeSet::from(["reader".into()]);
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &first, &consumers)
            .unwrap();
        let removed = ledger
            .plan_publication(&first, &EndpointCatalog::default(), &consumers)
            .unwrap();
        assert!(removed.pending[&1].services["default__api"].retire_vip);
        assert!(
            removed.pending[&1].services["default__api"]
                .service
                .backends
                .is_empty()
        );
        let unread = ledger
            .plan_publication(&first, &EndpointCatalog::default(), &BTreeSet::new())
            .unwrap();
        assert_eq!(unread.generation, 2);
        assert!(unread.pending.is_empty());
    }

    #[test]
    fn exhausted_generation_or_capacity_refuses_without_losing_evidence() {
        let first = catalogue('a');
        let consumers = BTreeSet::from(["reader".into()]);
        let exhausted = EndpointWithdrawals {
            generation: u64::MAX,
            ..Default::default()
        };
        assert!(matches!(
            exhausted.plan_publication(&first, &EndpointCatalog::default(), &consumers),
            Err(WithdrawalError::GenerationExhausted)
        ));
        assert_eq!(
            exhausted
                .plan_publication(&first, &first, &consumers)
                .unwrap(),
            exhausted
        );
        let obligation = EndpointWithdrawal {
            consumers: consumers.clone(),
            services: BTreeMap::from([(
                "default__api".into(),
                ServiceWithdrawal {
                    service: first.services["default__api"].clone(),
                    retire_vip: true,
                },
            )]),
        };
        let full = EndpointWithdrawals {
            generation: MAX_WITHDRAWAL_GENERATIONS as u64 + 1,
            pending: (0..MAX_WITHDRAWAL_GENERATIONS as u64)
                .map(|g| (g, obligation.clone()))
                .collect(),
        };
        assert!(matches!(
            full.plan_publication(&first, &EndpointCatalog::default(), &consumers),
            Err(WithdrawalError::CapacityReached)
        ));
        assert_eq!(full.pending.len(), MAX_WITHDRAWAL_GENERATIONS);
    }
    #[test]
    fn partial_backend_removal_keeps_only_the_removed_original_destination() {
        let mut first = catalogue('a');
        let mut retained = catalogue('b').services["default__api"].backends[0].clone();
        retained.host_port = 30002;
        retained.execution.as_mut().unwrap().instance_id = InstanceId("default__api-1".into());
        first
            .services
            .get_mut("default__api")
            .unwrap()
            .backends
            .push(retained.clone());
        let mut next = first.clone();
        next.services.get_mut("default__api").unwrap().backends = vec![retained];
        let consumers = BTreeSet::from(["reader".into()]);
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &first, &consumers)
            .unwrap()
            .plan_publication(&first, &next, &consumers)
            .unwrap();
        let removed = &ledger.pending[&1].services["default__api"];
        assert_eq!(
            removed.service.backends,
            vec![first.services["default__api"].backends[0].clone()]
        );
        assert!(!removed.retire_vip);
        let mut conflicting = ledger.clone();
        conflicting.generation = 1;
        assert!(matches!(
            conflicting.plan_publication(&next, &EndpointCatalog::default(), &consumers),
            Err(WithdrawalError::GenerationConflict)
        ));
        assert_eq!(conflicting.pending, ledger.pending);
    }

    #[test]
    fn changed_service_routes_retain_original_ports_and_allocation_ownership() {
        let first = catalogue('a');
        let consumers = BTreeSet::from(["reader".into()]);
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &first, &consumers)
            .unwrap();
        let mut next = first.clone();
        next.services.get_mut("default__api").unwrap().port = 8080;
        let changed = ledger.plan_publication(&first, &next, &consumers).unwrap();
        let removed = &changed.pending[&1].services["default__api"];
        assert_eq!(removed.service, first.services["default__api"]);
        assert!(!removed.retire_vip);
        next.services.get_mut("default__api").unwrap().vip =
            super::super::vip::VirtualIP("127.128.1.1".parse().unwrap());
        let moved = ledger.plan_publication(&first, &next, &consumers).unwrap();
        assert!(moved.pending[&1].services["default__api"].retire_vip);
        assert_eq!(
            moved.pending[&1].services["default__api"].service,
            first.services["default__api"]
        );
    }

    #[test]
    fn exposure_and_confirmation_budgets_refuse_without_eviction() {
        let first = catalogue('a');
        let consumers = BTreeSet::from(["reader".into()]);
        let mut service = first.services["default__api"].clone();
        service.backends = vec![service.backends[0].clone(); MAX_WITHDRAWAL_EXPOSURES - 1];
        let full = EndpointWithdrawals {
            generation: 2,
            pending: BTreeMap::from([(
                1,
                EndpointWithdrawal {
                    services: BTreeMap::from([(
                        "default__api".into(),
                        ServiceWithdrawal {
                            service,
                            retire_vip: true,
                        },
                    )]),
                    consumers: consumers.clone(),
                },
            )]),
        };
        assert!(matches!(
            full.plan_publication(&first, &EndpointCatalog::default(), &consumers),
            Err(WithdrawalError::CapacityReached)
        ));
        assert_eq!(
            full.pending[&1].services["default__api"]
                .service
                .backends
                .len(),
            MAX_WITHDRAWAL_EXPOSURES - 1
        );
        let readers: BTreeSet<String> = (0..super::super::catalog::MAX_ENDPOINT_CONSUMERS)
            .map(|n| format!("reader-{n}"))
            .collect();
        let obligation = EndpointWithdrawal {
            services: BTreeMap::from([(
                "default__api".into(),
                ServiceWithdrawal {
                    service: first.services["default__api"].clone(),
                    retire_vip: true,
                },
            )]),
            consumers: readers,
        };
        let full = EndpointWithdrawals {
            generation: 5,
            pending: (1..5).map(|g| (g, obligation.clone())).collect(),
        };
        assert!(matches!(
            full.plan_publication(&first, &EndpointCatalog::default(), &consumers),
            Err(WithdrawalError::CapacityReached)
        ));
        assert_eq!(full.pending.len(), 4);
    }
    #[test]
    fn withdrawn_vip_requires_confirmation_before_a_later_publication_reuses_it() {
        let first = catalogue('a');
        let consumers = BTreeSet::from(["reader".into()]);
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &first, &consumers)
            .unwrap()
            .plan_publication(&first, &EndpointCatalog::default(), &consumers)
            .unwrap();
        assert_eq!(
            ledger.reserved_vips().collect::<Vec<_>>(),
            vec![first.services["default__api"].vip]
        );
        assert!(
            ledger
                .plan_publication(&EndpointCatalog::default(), &first, &consumers)
                .is_err()
        );
        let mut retired = ledger.clone();
        retired.retire_consumer("reader");
        assert!(
            retired
                .plan_publication(&EndpointCatalog::default(), &first, &BTreeSet::new())
                .is_ok()
        );
        assert_eq!(ledger.pending.len(), 1);
    }

    #[test]
    fn withdrawn_vip_cannot_change_owners_in_the_same_publication() {
        let first = catalogue('a');
        let consumers = BTreeSet::from(["reader".into()]);
        let ledger = EndpointWithdrawals::default()
            .plan_publication(&EndpointCatalog::default(), &first, &consumers)
            .unwrap();
        let replacement = EndpointCatalog {
            services: BTreeMap::from([(
                "default__different".into(),
                first.services["default__api"].clone(),
            )]),
        };
        assert!(
            ledger
                .plan_publication(&first, &replacement, &consumers)
                .is_err()
        );
        assert!(ledger.pending.is_empty());
        assert_eq!(ledger.generation, 1);
    }

    #[test]
    fn catalogue_publication_rejects_invalid_or_aliased_virtual_allocations() {
        let first = catalogue('a');
        let mut invalid = first.clone();
        invalid.services.insert(
            "default__different".into(),
            first.services["default__api"].clone(),
        );
        assert!(
            EndpointWithdrawals::default()
                .plan_publication(&EndpointCatalog::default(), &invalid, &BTreeSet::new())
                .is_err()
        );
        let mut invalid = first.clone();
        invalid.services.get_mut("default__api").unwrap().port = 0;
        assert!(
            EndpointWithdrawals::default()
                .plan_publication(&EndpointCatalog::default(), &invalid, &BTreeSet::new())
                .is_err()
        );
        let mut invalid = first.clone();
        invalid.services.get_mut("default__api").unwrap().vip =
            super::super::vip::VirtualIP("10.0.0.1".parse().unwrap());
        assert!(
            EndpointWithdrawals::default()
                .plan_publication(&EndpointCatalog::default(), &invalid, &BTreeSet::new())
                .is_err()
        );
        let invalid = EndpointCatalog {
            services: BTreeMap::from([(
                "bad/service".into(),
                first.services["default__api"].clone(),
            )]),
        };
        assert!(
            EndpointWithdrawals::default()
                .plan_publication(&EndpointCatalog::default(), &invalid, &BTreeSet::new())
                .is_err()
        );
    }
}
