//! Replicated ownership for deploy-time migrations and ordinary job dispatch.
//!
//! Claims retain the approved manifest, original leadership term and recovery
//! epoch. A later leader cannot infer an abandoned worker's runtime outcome.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::{Error, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::config::Config;

pub(crate) const MAX_CLAIMS: usize = 64;
pub(crate) const MAX_TARGETS: usize = 1024;
pub(crate) const MAX_CLAIM_BYTES: usize = 8 * 1024 * 1024;

/// Approved manifest held until migration and ordinary-job outcomes settle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrerequisiteClaim {
    /// Leadership term that authorised the worker, never refreshed on retry.
    pub term: u64,
    /// Recovery generation in which the worker was authorised.
    pub recovery_epoch: u64,
    /// Desired writes were published; remaining ordinary jobs still own work.
    pub apps_committed: bool,
    /// Original validated specs; settlement cannot substitute another manifest.
    pub config: Config,
}

impl PrerequisiteClaim {
    /// Before commit, every target is fenced. Afterwards only ordinary jobs
    /// retain runtime ownership; committed apps and migration jobs are released.
    pub fn blocks(&self, name: &str, namespace: &str) -> bool {
        (!self.apps_committed
            && self
                .config
                .app
                .get(name)
                .is_some_and(|spec| spec.namespace.as_deref().unwrap_or("default") == namespace))
            || self.config.job.get(name).is_some_and(|spec| {
                spec.namespace.as_deref().unwrap_or("default") == namespace
                    && (!self.apps_committed || spec.run_before.is_empty())
            })
    }

    /// The original manifest includes ordinary one-shot jobs.
    pub fn has_ordinary_jobs(&self) -> bool {
        self.config
            .job
            .values()
            .any(|job| job.run_before.is_empty())
    }
}

/// Count every declared resource, including writes published after migration.
pub(crate) fn target_count(config: &Config) -> usize {
    config
        .app
        .len()
        .saturating_add(config.job.len())
        .saturating_add(config.namespace.len())
        .saturating_add(config.permission.len())
        .saturating_add(config.build.len())
}

fn validate_claim(operation: &str, claim: &PrerequisiteClaim) -> Result<(), String> {
    if operation.len() != 32 || !operation.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid prerequisite operation identity".into());
    }
    if claim.term == 0 || claim.config.job.is_empty() || target_count(&claim.config) > MAX_TARGETS {
        return Err("invalid prerequisite ownership term or resource inventory".into());
    }
    if claim.config.job.values().any(|job| job.schedule.is_some()) {
        return Err("cluster ownership does not support recurring schedules".into());
    }
    claim
        .config
        .validate_intrinsic()
        .map_err(|error| error.to_string())?;
    if claim.apps_committed && !claim.has_ordinary_jobs() {
        return Err("completed prerequisite claim has no ordinary jobs to retain".into());
    }
    Ok(())
}

/// Admission and snapshot decoding enforce the same ownership bounds.
pub(crate) fn validate_claims(claims: &BTreeMap<String, PrerequisiteClaim>) -> Result<(), String> {
    if claims.len() > MAX_CLAIMS {
        return Err("prerequisite ownership count limit exceeded".into());
    }
    let mut identities = BTreeSet::new();
    for (operation, claim) in claims {
        validate_claim(operation, claim)?;
        for (name, namespace) in claim
            .config
            .app
            .iter()
            .map(|(name, spec)| (name, spec.namespace.as_deref().unwrap_or("default")))
            .chain(
                claim
                    .config
                    .job
                    .iter()
                    .map(|(name, spec)| (name, spec.namespace.as_deref().unwrap_or("default"))),
            )
        {
            if claim.blocks(name, namespace) && !identities.insert((namespace, name)) {
                return Err("overlapping active prerequisite ownership".into());
            }
        }
    }
    let encoded = serde_json::to_vec(claims).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_CLAIM_BYTES {
        return Err("prerequisite ownership inventory is too large".into());
    }
    Ok(())
}

/// Refuse malformed, duplicate or oversized durable ownership on reopening.
pub(crate) fn deserialize_claims<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, PrerequisiteClaim>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ClaimsVisitor;

    impl<'de> Visitor<'de> for ClaimsVisitor {
        type Value = BTreeMap<String, PrerequisiteClaim>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded map of approved job ownership claims")
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut claims = BTreeMap::new();
            while let Some((operation, claim)) = map.next_entry::<String, PrerequisiteClaim>()? {
                if claims.len() >= MAX_CLAIMS || claims.contains_key(&operation) {
                    return Err(A::Error::custom(
                        "duplicate or excessive prerequisite ownership",
                    ));
                }
                validate_claim(&operation, &claim).map_err(A::Error::custom)?;
                claims.insert(operation, claim);
            }
            validate_claims(&claims).map_err(A::Error::custom)?;
            Ok(claims)
        }
    }

    deserializer.deserialize_map(ClaimsVisitor)
}

/// Check every target, including a job-only or app-only changed submission.
pub(crate) fn conflict<'a>(
    claims: &'a BTreeMap<String, PrerequisiteClaim>,
    config: &Config,
) -> Option<&'a str> {
    claims.iter().find_map(|(operation, claim)| {
        let app = config
            .app
            .iter()
            .any(|(name, spec)| claim.blocks(name, spec.namespace.as_deref().unwrap_or("default")));
        let job = config
            .job
            .iter()
            .any(|(name, spec)| claim.blocks(name, spec.namespace.as_deref().unwrap_or("default")));
        (app || job).then_some(operation.as_str())
    })
}
