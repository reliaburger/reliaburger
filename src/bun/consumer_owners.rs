//! Original cluster publication attempts retained until confirmed withdrawal.

use std::io;

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
}

/// Bounded, append-only evidence. Saving this state does not prove publication or drainage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerOwnership {
    /// Identity that must perform or recover these obligations.
    pub identity: ConsumerIdentity,
    /// Original attempts, including attempts whose completion is uncertain.
    pub publications: Vec<ConsumerPublication>,
}

impl ConsumerOwnership {
    pub(crate) fn validate(&self) -> io::Result<()> {
        crate::cluster::retirement::validate_node_id(&self.identity.node_id.0)
            .map_err(io::Error::other)?;
        if self.publications.len() > MAX_PUBLICATIONS {
            return Err(io::Error::other("consumer publication capacity exhausted"));
        }
        let mut exposures = 0usize;
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
    if let Some(previous) = previous {
        let next =
            next.ok_or_else(|| io::Error::other("consumer ownership cannot be forgotten"))?;
        if next.identity != previous.identity
            || !next.publications.starts_with(&previous.publications)
        {
            return Err(io::Error::other(
                "original consumer publication history must be retained",
            ));
        }
    }
    Ok(())
}
