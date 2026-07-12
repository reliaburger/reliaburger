/// Namespace resource quotas.
///
/// Enforced at scheduling time. Each namespace can have limits on
/// CPU, memory, GPUs, app count, and total replica count.
use std::collections::HashMap;

use super::types::Resources;

/// Resource quota for a namespace.
#[derive(Debug, Clone)]
pub struct NamespaceQuota {
    /// Namespace this quota applies to.
    pub namespace: String,
    /// Maximum CPU in millicores.
    pub max_cpu_millicores: Option<u64>,
    /// Maximum memory in bytes.
    pub max_memory_bytes: Option<u64>,
    /// Maximum GPU devices.
    pub max_gpus: Option<u32>,
    /// Maximum number of distinct apps.
    pub max_apps: Option<u32>,
    /// Maximum total replicas across all apps.
    pub max_replicas: Option<u32>,
}

/// Tracked usage for a namespace.
#[derive(Debug, Clone, Default)]
pub struct NamespaceUsage {
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
    pub gpus: u32,
    pub app_count: u32,
    pub replica_count: u32,
}

/// Quota check error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuotaError {
    #[error("namespace {namespace:?} would exceed CPU quota: {current}+{requested} > {limit}m")]
    CpuExceeded {
        namespace: String,
        current: u64,
        requested: u64,
        limit: u64,
    },
    #[error("namespace {namespace:?} would exceed memory quota: {current}+{requested} > {limit}")]
    MemoryExceeded {
        namespace: String,
        current: u64,
        requested: u64,
        limit: u64,
    },
    #[error("namespace {namespace:?} would exceed GPU quota: {current}+{requested} > {limit}")]
    GpuExceeded {
        namespace: String,
        current: u32,
        requested: u32,
        limit: u32,
    },
    #[error("namespace {namespace:?} would exceed max apps: {current}/{limit}")]
    MaxAppsExceeded {
        namespace: String,
        current: u32,
        limit: u32,
    },
    #[error("namespace {namespace:?} would exceed max replicas: {current}+{requested} > {limit}")]
    MaxReplicasExceeded {
        namespace: String,
        current: u32,
        requested: u32,
        limit: u32,
    },
}

/// Check whether scheduling `requested` resources for `new_replicas`
/// replicas would exceed the namespace quota.
///
/// `is_new_app` should be true if this is the first time this app is
/// being scheduled (counts against max_apps).
pub fn check_quota(
    quota: &NamespaceQuota,
    usage: &NamespaceUsage,
    requested: &Resources,
    new_replicas: u32,
    is_new_app: bool,
) -> Result<(), QuotaError> {
    // Saturating arithmetic throughout: a spec asking for an absurd
    // per-replica resource times a large replica count must reject
    // cleanly, never wrap (DEP9). A saturated total still exceeds any
    // real limit, so the check stays correct.
    if let Some(limit) = quota.max_cpu_millicores {
        let requested_total = requested
            .cpu_millicores
            .saturating_mul(u64::from(new_replicas));
        let total = usage.cpu_millicores.saturating_add(requested_total);
        if total > limit {
            return Err(QuotaError::CpuExceeded {
                namespace: quota.namespace.clone(),
                current: usage.cpu_millicores,
                requested: requested_total,
                limit,
            });
        }
    }

    if let Some(limit) = quota.max_memory_bytes {
        let requested_total = requested
            .memory_bytes
            .saturating_mul(u64::from(new_replicas));
        let total = usage.memory_bytes.saturating_add(requested_total);
        if total > limit {
            return Err(QuotaError::MemoryExceeded {
                namespace: quota.namespace.clone(),
                current: usage.memory_bytes,
                requested: requested_total,
                limit,
            });
        }
    }

    if let Some(limit) = quota.max_gpus {
        let requested_total = requested.gpus.saturating_mul(new_replicas);
        let total = usage.gpus.saturating_add(requested_total);
        if total > limit {
            return Err(QuotaError::GpuExceeded {
                namespace: quota.namespace.clone(),
                current: usage.gpus,
                requested: requested_total,
                limit,
            });
        }
    }

    if let Some(limit) = quota.max_apps
        && is_new_app
        && usage.app_count >= limit
    {
        return Err(QuotaError::MaxAppsExceeded {
            namespace: quota.namespace.clone(),
            current: usage.app_count,
            limit,
        });
    }

    if let Some(limit) = quota.max_replicas {
        let total = usage.replica_count.saturating_add(new_replicas);
        if total > limit {
            return Err(QuotaError::MaxReplicasExceeded {
                namespace: quota.namespace.clone(),
                current: usage.replica_count,
                requested: new_replicas,
                limit,
            });
        }
    }

    Ok(())
}

/// Per-namespace usage ledger for one scheduling pass.
///
/// The leader plans every app in a single pass. Quotas are *cumulative*
/// within a namespace, so a ledger accumulates usage as each app is
/// admitted: the second app in `prod` is checked against the first's
/// footprint, not a clean slate. Without this a namespace could hold two
/// apps that each fit alone but together bust the budget.
///
/// The quota table is *injected*. Namespace resources are not yet
/// desired state (that lands with T6 declarative resources), so in
/// production the table is empty today and every app is admitted — the
/// enforcement seam is wired and tested here, ready for T6 to feed it.
#[derive(Debug, Default)]
pub struct QuotaLedger {
    quotas: HashMap<String, NamespaceQuota>,
    usage: HashMap<String, NamespaceUsage>,
}

impl QuotaLedger {
    /// Build a ledger from a namespace → quota table.
    pub fn new(quotas: HashMap<String, NamespaceQuota>) -> Self {
        Self {
            quotas,
            usage: HashMap::new(),
        }
    }

    /// Whether any quota is configured. An empty ledger admits everything
    /// without allocating usage maps.
    pub fn is_empty(&self) -> bool {
        self.quotas.is_empty()
    }

    /// Check whether admitting `replicas` of an app requesting `per_replica`
    /// resources in `namespace` fits its quota; if so, record the usage.
    ///
    /// Returns `Ok(())` and mutates the ledger on admission, or the quota
    /// error without mutating on rejection. A namespace with no configured
    /// quota is always admitted (its usage is still tracked, harmlessly).
    pub fn try_admit(
        &mut self,
        namespace: &str,
        per_replica: &Resources,
        replicas: u32,
        is_new_app: bool,
    ) -> Result<(), QuotaError> {
        let entry = self.usage.entry(namespace.to_string()).or_default();
        if let Some(quota) = self.quotas.get(namespace) {
            check_quota(quota, entry, per_replica, replicas, is_new_app)?;
        }
        entry.cpu_millicores = entry.cpu_millicores.saturating_add(
            per_replica
                .cpu_millicores
                .saturating_mul(u64::from(replicas)),
        );
        entry.memory_bytes = entry
            .memory_bytes
            .saturating_add(per_replica.memory_bytes.saturating_mul(u64::from(replicas)));
        entry.gpus = entry
            .gpus
            .saturating_add(per_replica.gpus.saturating_mul(replicas));
        entry.replica_count = entry.replica_count.saturating_add(replicas);
        if is_new_app {
            entry.app_count = entry.app_count.saturating_add(1);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(ns: &str) -> NamespaceQuota {
        NamespaceQuota {
            namespace: ns.to_string(),
            max_cpu_millicores: Some(2000),
            max_memory_bytes: Some(4 * 1024 * 1024 * 1024),
            max_gpus: Some(2),
            max_apps: Some(5),
            max_replicas: Some(10),
        }
    }

    fn empty_usage() -> NamespaceUsage {
        NamespaceUsage::default()
    }

    #[test]
    fn allows_within_limits() {
        let res = Resources::new(500, 1024 * 1024 * 1024, 0);
        assert!(check_quota(&quota("prod"), &empty_usage(), &res, 2, true).is_ok());
    }

    #[test]
    fn rejects_cpu_exceeded() {
        let res = Resources::new(1500, 1024, 0);
        let err = check_quota(&quota("prod"), &empty_usage(), &res, 2, false);
        assert!(matches!(err, Err(QuotaError::CpuExceeded { .. })));
    }

    #[test]
    fn rejects_memory_exceeded() {
        let res = Resources::new(100, 3 * 1024 * 1024 * 1024, 0);
        let err = check_quota(&quota("prod"), &empty_usage(), &res, 2, false);
        assert!(matches!(err, Err(QuotaError::MemoryExceeded { .. })));
    }

    #[test]
    fn rejects_max_apps_exceeded() {
        let usage = NamespaceUsage {
            app_count: 5,
            ..Default::default()
        };
        let res = Resources::new(100, 1024, 0);
        let err = check_quota(&quota("prod"), &usage, &res, 1, true);
        assert!(matches!(err, Err(QuotaError::MaxAppsExceeded { .. })));
    }

    #[test]
    fn rejects_max_replicas_exceeded() {
        let usage = NamespaceUsage {
            replica_count: 9,
            ..Default::default()
        };
        let res = Resources::new(100, 1024, 0);
        let err = check_quota(&quota("prod"), &usage, &res, 2, false);
        assert!(matches!(err, Err(QuotaError::MaxReplicasExceeded { .. })));
    }

    #[test]
    fn quota_does_not_overflow_on_absurd_replica_product() {
        // A spec asking for a huge per-replica memory times a large replica
        // count must reject cleanly (saturate), not wrap around to a small
        // total that spuriously passes (DEP9).
        let q = NamespaceQuota {
            namespace: "prod".to_string(),
            max_cpu_millicores: None,
            max_memory_bytes: Some(1024),
            max_gpus: None,
            max_apps: None,
            max_replicas: None,
        };
        let res = Resources::new(0, u64::MAX, 0);
        let err = check_quota(&q, &empty_usage(), &res, 1_000_000, false);
        assert!(matches!(err, Err(QuotaError::MemoryExceeded { .. })));
    }

    #[test]
    fn ledger_accumulates_across_apps_in_a_namespace() {
        let q = NamespaceQuota {
            namespace: "prod".to_string(),
            max_cpu_millicores: Some(1000),
            max_memory_bytes: None,
            max_gpus: None,
            max_apps: None,
            max_replicas: None,
        };
        let mut ledger = QuotaLedger::new(HashMap::from([("prod".to_string(), q)]));
        // First app: 2 × 300m = 600m — fits under 1000m.
        assert!(
            ledger
                .try_admit("prod", &Resources::new(300, 0, 0), 2, true)
                .is_ok()
        );
        // Second app: 2 × 300m = 600m more → 1200m total > 1000m — rejected
        // because the first app already consumed the namespace budget.
        let err = ledger.try_admit("prod", &Resources::new(300, 0, 0), 2, true);
        assert!(
            matches!(err, Err(QuotaError::CpuExceeded { .. })),
            "cumulative usage should bust the quota: {err:?}"
        );
    }

    #[test]
    fn ledger_without_quota_admits_everything() {
        let mut ledger = QuotaLedger::default();
        assert!(ledger.is_empty());
        assert!(
            ledger
                .try_admit("anything", &Resources::new(9_999_999, 0, 0), 100, true)
                .is_ok()
        );
    }

    #[test]
    fn no_quota_allows_everything() {
        let unlimited = NamespaceQuota {
            namespace: "free".to_string(),
            max_cpu_millicores: None,
            max_memory_bytes: None,
            max_gpus: None,
            max_apps: None,
            max_replicas: None,
        };
        let res = Resources::new(999_999, 999_999_999, 100);
        assert!(check_quota(&unlimited, &empty_usage(), &res, 100, true).is_ok());
    }
}
