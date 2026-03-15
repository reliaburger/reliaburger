/// Namespace resource quotas.
///
/// Enforced at scheduling time. Each namespace can have limits on
/// CPU, memory, GPUs, app count, and total replica count.
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
    if let Some(limit) = quota.max_cpu_millicores {
        let total = usage.cpu_millicores + requested.cpu_millicores * new_replicas as u64;
        if total > limit {
            return Err(QuotaError::CpuExceeded {
                namespace: quota.namespace.clone(),
                current: usage.cpu_millicores,
                requested: requested.cpu_millicores * new_replicas as u64,
                limit,
            });
        }
    }

    if let Some(limit) = quota.max_memory_bytes {
        let total = usage.memory_bytes + requested.memory_bytes * new_replicas as u64;
        if total > limit {
            return Err(QuotaError::MemoryExceeded {
                namespace: quota.namespace.clone(),
                current: usage.memory_bytes,
                requested: requested.memory_bytes * new_replicas as u64,
                limit,
            });
        }
    }

    if let Some(limit) = quota.max_gpus {
        let total = usage.gpus + requested.gpus * new_replicas;
        if total > limit {
            return Err(QuotaError::GpuExceeded {
                namespace: quota.namespace.clone(),
                current: usage.gpus,
                requested: requested.gpus * new_replicas,
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
        let total = usage.replica_count + new_replicas;
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
