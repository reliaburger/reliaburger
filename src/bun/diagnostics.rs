//! Bounded, public telemetry used by `relish wtf`.
//!
//! The wire types deliberately omit certificate bodies, private material and
//! host filesystem paths. Collection can return degraded evidence alongside
//! the facts it did observe, rather than hiding a partial failure.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sysinfo::Disks;

use crate::bun::agent::InstanceStatus;
use crate::config::Replicas;

const MAX_DIAGNOSTIC_INSTANCES: usize = 4096;

/// One diagnostic source and its collection status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DiagnosticSource<T> {
    /// Every required fact was observed.
    Available {
        /// Unix timestamp, in seconds, when collection completed.
        observed_at: u64,
        /// Collected value.
        value: T,
    },
    /// Some facts were observed but the inventory is incomplete.
    Degraded {
        /// Unix timestamp, in seconds, when collection completed.
        observed_at: u64,
        /// Facts which were still safe to use.
        value: T,
        /// Why the source is incomplete.
        reason: String,
    },
    /// The source should have worked but could not be read.
    Unavailable {
        /// Concrete collection failure.
        reason: String,
    },
    /// This runtime or configuration cannot expose the fact.
    Unsupported {
        /// Missing capability or telemetry contract.
        reason: String,
    },
}

impl<T> DiagnosticSource<T> {
    /// Borrow all usable evidence, including evidence from a degraded source.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Available { value, .. } | Self::Degraded { value, .. } => Some(value),
            Self::Unavailable { .. } | Self::Unsupported { .. } => None,
        }
    }

    fn into_parts(self) -> (Option<T>, Vec<String>, Option<u64>) {
        match self {
            Self::Available { observed_at, value } => (Some(value), Vec::new(), Some(observed_at)),
            Self::Degraded {
                observed_at,
                value,
                reason,
            } => (Some(value), vec![reason], Some(observed_at)),
            Self::Unavailable { reason } | Self::Unsupported { reason } => {
                (None, vec![reason], None)
            }
        }
    }
}

/// A configured storage domain. The path remains server-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticStoragePath {
    /// Stable storage-domain name.
    pub domain: String,
    /// Configured filesystem path.
    pub path: PathBuf,
}

/// Startup facts needed only by the local diagnostics endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticStaticEvidence {
    /// Configured data, image, log, metric and volume paths.
    pub storage_paths: Vec<DiagnosticStoragePath>,
    /// Public node leaf metadata, when cluster mTLS is configured.
    pub node_certificate: Option<PublicCertificateMetadata>,
}

/// Capacity evidence for one configured storage domain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskUsageEvidence {
    /// Stable domain such as `images`, `logs` or `volumes`.
    pub storage_domain: String,
    /// Bytes used on the filesystem containing that domain.
    pub used_bytes: u64,
    /// Total bytes on that filesystem.
    pub total_bytes: u64,
    /// Derived percentage, retained so clients use one calculation.
    pub used_percent: f64,
}

/// Cgroup CPU throttling observed over a bounded window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CpuThrottleEvidence {
    /// Application name.
    pub app: String,
    /// Application namespace.
    pub namespace: String,
    /// Instance identifier.
    pub instance: String,
    /// Increase in `cpu.stat` throttled time over the window.
    pub throttled_seconds_delta: f64,
    /// Width of the observation window.
    pub window_seconds: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CpuThrottleTotal {
    app: String,
    namespace: String,
    instance: String,
    throttled_microseconds: u64,
}

/// Certificate metadata safe to return from an authenticated read endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicCertificateMetadata {
    /// `node` or `workload`.
    pub certificate_kind: String,
    /// Node name or SPIFFE URI.
    pub identity: String,
    /// X.509 issuer distinguished name.
    pub issuer: String,
    /// X.509 serial number in hexadecimal.
    pub serial: String,
    /// Unix timestamp, in seconds, when the leaf expires.
    pub not_after: u64,
    /// Current lifecycle state such as `valid`, `grace_period` or
    /// `restart_required`.
    pub rotation_state: String,
    /// Whether Bun can rotate this leaf without a process restart.
    pub automatic_rotation: bool,
}

/// Complete local payload for the authenticated diagnostics API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalDiagnosticSnapshot {
    /// Wire schema version.
    pub schema_version: u32,
    /// Node which made the observations.
    pub node_id: String,
    /// Unix timestamp, in seconds, when collection completed.
    pub observed_at: u64,
    /// Per-storage-domain capacity.
    pub disks: DiagnosticSource<Vec<DiskUsageEvidence>>,
    /// Actual cgroup throttled-time deltas.
    pub cpu_throttling: DiagnosticSource<Vec<CpuThrottleEvidence>>,
    /// Public certificate lifecycle metadata.
    pub certificates: DiagnosticSource<Vec<PublicCertificateMetadata>>,
}

/// Desired service state used to distinguish an intentionally empty service
/// from a service which lost all of its backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredAppEvidence {
    /// Application name.
    pub app: String,
    /// Application namespace.
    pub namespace: String,
    /// Desired replicas after resolving fixed versus daemon mode.
    pub desired_replicas: u32,
    /// Placements currently recorded by the scheduler.
    pub scheduled_replicas: u32,
    /// Declared service port. `None` means this app is not a service.
    pub service_port: Option<u16>,
}

/// Current diagnostics wire schema.
pub const LOCAL_DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;

/// Resolve a fixed or daemon-set replica declaration for current membership.
pub fn desired_replica_count(replicas: Replicas, live_nodes: usize) -> u32 {
    match replicas {
        Replicas::Fixed(count) => count,
        Replicas::DaemonSet => u32::try_from(live_nodes.max(1)).unwrap_or(u32::MAX),
    }
}

/// Extract safe public metadata from a DER certificate.
pub fn public_certificate_metadata(
    certificate_kind: &str,
    identity: &str,
    certificate_der: &[u8],
    rotation_state: &str,
    automatic_rotation: bool,
) -> Result<PublicCertificateMetadata, String> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate_der)
        .map_err(|error| format!("could not parse {certificate_kind} certificate: {error}"))?;
    let not_after = certificate.validity().not_after.timestamp();
    let not_after = u64::try_from(not_after)
        .map_err(|_| format!("{certificate_kind} certificate expires before Unix epoch"))?;
    Ok(PublicCertificateMetadata {
        certificate_kind: certificate_kind.to_string(),
        identity: identity.to_string(),
        issuer: certificate.issuer().to_string(),
        serial: certificate.raw_serial_as_string(),
        not_after,
        rotation_state: rotation_state.to_string(),
        automatic_rotation,
    })
}

/// Collect attributed capacity for configured storage paths.
pub fn collect_disk_usage(
    storage_paths: &[DiagnosticStoragePath],
    observed_at: u64,
) -> DiagnosticSource<Vec<DiskUsageEvidence>> {
    if storage_paths.is_empty() {
        return DiagnosticSource::Unsupported {
            reason: "configured storage paths were not supplied to diagnostics".to_string(),
        };
    }
    let disks = Disks::new_with_refreshed_list();
    let mounts = disks
        .list()
        .iter()
        .map(|disk| MountCapacity {
            mount: disk.mount_point().to_path_buf(),
            total_bytes: disk.total_space(),
            available_bytes: disk.available_space(),
        })
        .collect::<Vec<_>>();
    disk_usage_from_mounts(storage_paths, &mounts, observed_at)
}

#[derive(Debug, Clone)]
struct MountCapacity {
    mount: PathBuf,
    total_bytes: u64,
    available_bytes: u64,
}

fn disk_usage_from_mounts(
    storage_paths: &[DiagnosticStoragePath],
    mounts: &[MountCapacity],
    observed_at: u64,
) -> DiagnosticSource<Vec<DiskUsageEvidence>> {
    if mounts.is_empty() {
        return DiagnosticSource::Unavailable {
            reason: "the operating system returned no mounted disks".to_string(),
        };
    }
    let mut values = Vec::new();
    let mut errors = Vec::new();
    for storage in storage_paths {
        let mount = mounts
            .iter()
            .filter(|mount| storage.path.starts_with(&mount.mount))
            .max_by_key(|mount| mount.mount.components().count());
        let Some(mount) = mount else {
            errors.push(format!(
                "no mounted filesystem contains the {} storage domain",
                storage.domain
            ));
            continue;
        };
        if mount.total_bytes == 0 || mount.available_bytes > mount.total_bytes {
            errors.push(format!(
                "invalid capacity returned for the {} storage domain",
                storage.domain
            ));
            continue;
        }
        let used_bytes = mount.total_bytes - mount.available_bytes;
        values.push(DiskUsageEvidence {
            storage_domain: storage.domain.clone(),
            used_bytes,
            total_bytes: mount.total_bytes,
            used_percent: used_bytes as f64 * 100.0 / mount.total_bytes as f64,
        });
    }
    values.sort_by(|left, right| left.storage_domain.cmp(&right.storage_domain));
    finish_collection(values, errors, observed_at)
}

/// Collect current cumulative cgroup CPU throttling counters.
pub(crate) async fn collect_cpu_throttle_totals(
    instances: &[InstanceStatus],
    observed_at: u64,
) -> DiagnosticSource<Vec<CpuThrottleTotal>> {
    let instances = instances
        .iter()
        .filter(|instance| instance.pid.is_some())
        .collect::<Vec<_>>();
    if instances.len() > MAX_DIAGNOSTIC_INSTANCES {
        return DiagnosticSource::Unavailable {
            reason: format!(
                "{} instances exceeds the diagnostic collection limit of {}",
                instances.len(),
                MAX_DIAGNOSTIC_INSTANCES
            ),
        };
    }
    let reads = instances.into_iter().map(|instance| async move {
        let identity = crate::grill::InstanceIdentity::parse(&instance.id)
            .ok_or_else(|| format!("{} has an invalid instance id", instance.id))?;
        let path = crate::grill::cgroup::cgroup_path(
            &instance.namespace,
            &instance.app_name,
            identity.ordinal,
        )
        .join("cpu.stat");
        let contents = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| format!("could not read cpu.stat for {}: {error}", instance.id))?;
        let throttled_microseconds = parse_throttled_microseconds(&contents)
            .map_err(|error| format!("invalid cpu.stat for {}: {error}", instance.id))?;
        Ok::<_, String>(CpuThrottleTotal {
            app: instance.app_name.clone(),
            namespace: instance.namespace.clone(),
            instance: instance.id.clone(),
            throttled_microseconds,
        })
    });
    let mut values = Vec::new();
    let mut errors = Vec::new();
    for result in futures_util::future::join_all(reads).await {
        match result {
            Ok(value) => values.push(value),
            Err(error) => errors.push(error),
        }
    }
    values.sort_by(|left, right| left.instance.cmp(&right.instance));
    finish_collection(values, errors, observed_at)
}

fn parse_throttled_microseconds(cpu_stat: &str) -> Result<u64, String> {
    let mut found = None;
    for line in cpu_stat.lines() {
        let mut fields = line.split_ascii_whitespace();
        if fields.next() != Some("throttled_usec") {
            continue;
        }
        let value = fields
            .next()
            .ok_or_else(|| "throttled_usec has no value".to_string())?;
        if fields.next().is_some() {
            return Err("throttled_usec has extra fields".to_string());
        }
        if found.is_some() {
            return Err("throttled_usec appears more than once".to_string());
        }
        found = Some(
            value
                .parse::<u64>()
                .map_err(|_| "throttled_usec is not an unsigned integer".to_string())?,
        );
    }
    found.ok_or_else(|| "throttled_usec is absent".to_string())
}

/// Calculate bounded deltas between two cumulative cgroup samples.
pub(crate) fn cpu_throttle_window(
    first: DiagnosticSource<Vec<CpuThrottleTotal>>,
    second: DiagnosticSource<Vec<CpuThrottleTotal>>,
    window_seconds: u64,
    observed_at: u64,
) -> DiagnosticSource<Vec<CpuThrottleEvidence>> {
    let (first, mut errors, _) = first.into_parts();
    let (second, second_errors, _) = second.into_parts();
    errors.extend(second_errors);
    let (Some(first), Some(second)) = (first, second) else {
        return DiagnosticSource::Unavailable {
            reason: errors.join("; "),
        };
    };
    let first = first
        .into_iter()
        .map(|sample| (sample.instance.clone(), sample))
        .collect::<BTreeMap<_, _>>();
    let mut values = Vec::new();
    for sample in second {
        let Some(previous) = first.get(&sample.instance) else {
            errors.push(format!(
                "instance {} appeared during the CPU observation window",
                sample.instance
            ));
            continue;
        };
        let Some(delta) = sample
            .throttled_microseconds
            .checked_sub(previous.throttled_microseconds)
        else {
            errors.push(format!(
                "CPU throttle counter reset for instance {}",
                sample.instance
            ));
            continue;
        };
        values.push(CpuThrottleEvidence {
            app: sample.app,
            namespace: sample.namespace,
            instance: sample.instance,
            throttled_seconds_delta: delta as f64 / 1_000_000.0,
            window_seconds,
        });
    }
    if first.len() != values.len() {
        for instance in first.keys() {
            if !values.iter().any(|sample| &sample.instance == instance)
                && !errors.iter().any(|error| error.contains(instance))
            {
                errors.push(format!(
                    "instance {instance} disappeared during the CPU observation window"
                ));
            }
        }
    }
    values.sort_by(|left, right| left.instance.cmp(&right.instance));
    finish_collection(values, errors, observed_at)
}

fn finish_collection<T>(
    values: Vec<T>,
    errors: Vec<String>,
    observed_at: u64,
) -> DiagnosticSource<Vec<T>> {
    if errors.is_empty() {
        DiagnosticSource::Available {
            observed_at,
            value: values,
        }
    } else if values.is_empty() {
        DiagnosticSource::Unavailable {
            reason: errors.join("; "),
        }
    } else {
        DiagnosticSource::Degraded {
            observed_at,
            value: values,
            reason: errors.join("; "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_stat_parser_requires_one_bounded_counter() {
        assert_eq!(
            parse_throttled_microseconds(
                "usage_usec 100\nuser_usec 60\nsystem_usec 40\nnr_throttled 2\nthrottled_usec 345\n"
            )
            .unwrap(),
            345
        );
        assert!(parse_throttled_microseconds("usage_usec 1\n").is_err());
        assert!(parse_throttled_microseconds("throttled_usec nope\n").is_err());
        assert!(parse_throttled_microseconds("throttled_usec 1 2\n").is_err());
    }

    #[test]
    fn cpu_window_uses_counter_delta_not_cpu_usage() {
        let first = DiagnosticSource::Available {
            observed_at: 10,
            value: vec![cpu_total("default__api-0", 500_000)],
        };
        let second = DiagnosticSource::Available {
            observed_at: 12,
            value: vec![cpu_total("default__api-0", 1_750_000)],
        };

        let result = cpu_throttle_window(first, second, 2, 12);
        let values = result.value().unwrap();

        assert_eq!(values.len(), 1);
        assert_eq!(values[0].throttled_seconds_delta, 1.25);
        assert_eq!(values[0].window_seconds, 2);
    }

    #[test]
    fn cpu_window_marks_counter_reset_unavailable() {
        let first = DiagnosticSource::Available {
            observed_at: 10,
            value: vec![cpu_total("default__api-0", 500)],
        };
        let second = DiagnosticSource::Available {
            observed_at: 11,
            value: vec![cpu_total("default__api-0", 1)],
        };

        let result = cpu_throttle_window(first, second, 1, 11);

        assert!(
            matches!(result, DiagnosticSource::Unavailable { reason } if reason.contains("reset"))
        );
    }

    #[test]
    fn disk_capacity_is_attributed_to_longest_matching_mount() {
        let paths = vec![
            DiagnosticStoragePath {
                domain: "images".to_string(),
                path: PathBuf::from("/srv/reliaburger/images"),
            },
            DiagnosticStoragePath {
                domain: "logs".to_string(),
                path: PathBuf::from("/var/log/reliaburger"),
            },
        ];
        let mounts = vec![
            MountCapacity {
                mount: PathBuf::from("/"),
                total_bytes: 1000,
                available_bytes: 500,
            },
            MountCapacity {
                mount: PathBuf::from("/srv"),
                total_bytes: 2000,
                available_bytes: 500,
            },
        ];

        let result = disk_usage_from_mounts(&paths, &mounts, 10);
        let values = result.value().unwrap();

        assert_eq!(values[0].storage_domain, "images");
        assert_eq!(values[0].used_percent, 75.0);
        assert_eq!(values[1].storage_domain, "logs");
        assert_eq!(values[1].used_percent, 50.0);
    }

    #[test]
    fn public_snapshot_never_serialises_paths_or_certificate_material() {
        let snapshot = LocalDiagnosticSnapshot {
            schema_version: LOCAL_DIAGNOSTIC_SCHEMA_VERSION,
            node_id: "node-1".to_string(),
            observed_at: 10,
            disks: DiagnosticSource::Available {
                observed_at: 10,
                value: vec![DiskUsageEvidence {
                    storage_domain: "images".to_string(),
                    used_bytes: 1,
                    total_bytes: 2,
                    used_percent: 50.0,
                }],
            },
            cpu_throttling: DiagnosticSource::Available {
                observed_at: 10,
                value: Vec::new(),
            },
            certificates: DiagnosticSource::Degraded {
                observed_at: 10,
                value: vec![PublicCertificateMetadata {
                    certificate_kind: "node".to_string(),
                    identity: "node-1".to_string(),
                    issuer: "CN=node-ca".to_string(),
                    serial: "01".to_string(),
                    not_after: 20,
                    rotation_state: "restart_required".to_string(),
                    automatic_rotation: false,
                }],
                reason: "workload inventory is not exposed".to_string(),
            },
        };

        let json = serde_json::to_string(&snapshot).unwrap();

        assert!(!json.contains("/var/lib"));
        assert!(!json.contains("certificate_der"));
        assert!(!json.contains("private"));
        assert!(json.contains("not_after"));
    }

    #[test]
    fn daemon_desire_tracks_live_membership_without_becoming_zero() {
        assert_eq!(desired_replica_count(Replicas::Fixed(3), 9), 3);
        assert_eq!(desired_replica_count(Replicas::DaemonSet, 4), 4);
        assert_eq!(desired_replica_count(Replicas::DaemonSet, 0), 1);
    }

    fn cpu_total(instance: &str, throttled_microseconds: u64) -> CpuThrottleTotal {
        CpuThrottleTotal {
            app: "api".to_string(),
            namespace: "default".to_string(),
            instance: instance.to_string(),
            throttled_microseconds,
        }
    }
}
