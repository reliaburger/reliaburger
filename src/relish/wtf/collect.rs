use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::bun::agent::{CouncilStatus, NodeStatus};
use crate::bun::capabilities::{ClusterCapabilityReport, CollectedNodeCapability};
use crate::bun::deploy_operations::{
    DeployOperationPhase, DeployOperationSnapshot, DeployTargetKind,
};
use crate::bun::diagnostics::{DiagnosticSource, LocalDiagnosticSnapshot};
use crate::bun::events::EventKind;
use crate::ketchup::types::{LogQueryResult, LogStream};
use crate::onion::types::ResolveResponse;
use crate::relish::RelishError;
use crate::relish::client::BunClient;

use super::{
    AlertObservation, ApplicationEvidence, CertificateObservation, ClusterEvidence,
    CouncilObservation, CpuThrottleObservation, DeployObservation, DiskObservation, Evidence,
    FaultObservation, LogObservation, NodeObservation, RegistryObservation, RestartObservation,
    ServiceObservation, WtfInputs,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_LIMIT: usize = 1024;
const MAX_LOG_APPS: usize = 128;

struct NodeEndpoint {
    status: NodeStatus,
    client: Result<BunClient, String>,
}

struct NodeCollection {
    node_id: String,
    reachable: bool,
    diagnostics: Result<LocalDiagnosticSnapshot, String>,
    events: Result<Vec<crate::bun::events::ClusterEvent>, String>,
    deploys: Result<DeployOperationSnapshot, String>,
    alerts: Result<Vec<crate::mayo::alert::AlertStatus>, String>,
    faults: Result<Vec<crate::smoker::types::FaultSummary>, String>,
}

struct LocalEvidenceSet {
    disks: Evidence<Vec<DiskObservation>>,
    cpu_throttling: Evidence<Vec<CpuThrottleObservation>>,
    certificates: Evidence<Vec<CertificateObservation>>,
}

struct EvidenceBuilder<T> {
    values: Vec<T>,
    errors: Vec<String>,
    unsupported: usize,
}

impl<T> Default for EvidenceBuilder<T> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            errors: Vec::new(),
            unsupported: 0,
        }
    }
}

impl<T> EvidenceBuilder<T> {
    fn unavailable(&mut self, reason: impl Into<String>) {
        self.errors.push(reason.into());
    }

    fn unsupported(&mut self, reason: impl Into<String>) {
        self.unsupported += 1;
        self.errors.push(reason.into());
    }

    fn finish(self, observed_at: u64) -> Evidence<Vec<T>> {
        if self.errors.is_empty() {
            Evidence::available(observed_at, self.values)
        } else if self.values.is_empty() && self.unsupported == self.errors.len() {
            Evidence::Unsupported {
                reason: self.errors.join("; "),
            }
        } else if self.values.is_empty() {
            Evidence::Unavailable {
                reason: self.errors.join("; "),
            }
        } else {
            Evidence::Degraded {
                observed_at,
                value: self.values,
                reason: self.errors.join("; "),
            }
        }
    }

    /// Finish an evidence set that carries an inherent, non-error limitation.
    ///
    /// A clean collection becomes `AvailableWithCaveat`, so the caveat surfaces
    /// without forcing a `Degraded` verdict. A genuine collection error still
    /// wins: the caveat is dropped and the real failure is reported.
    fn finish_with_caveat(self, observed_at: u64, caveat: &str) -> Evidence<Vec<T>> {
        if self.errors.is_empty() {
            return Evidence::available_with_caveat(observed_at, self.values, caveat);
        }
        self.finish(observed_at)
    }
}

/// Collect an authenticated, bounded cluster evidence snapshot.
///
/// Failure of one node or subsystem becomes typed evidence. The function only
/// returns an error when the entry Bun agent itself cannot be reached.
pub async fn collect(client: &BunClient, app: Option<&str>) -> Result<WtfInputs, RelishError> {
    bounded("entry health", client.health())
        .await
        .map_err(|_| RelishError::AgentUnreachable)?;

    let (nodes_result, capabilities_result) = tokio::join!(
        bounded("cluster nodes", client.nodes()),
        bounded("cluster capabilities", client.cluster_capabilities()),
    );
    let capabilities = capabilities_result.as_ref().ok();
    let (cluster_name, capability_cluster_enabled, local_node_id) =
        capability_identity(capabilities);

    let (membership, endpoints) = match nodes_result {
        Ok(nodes) if !nodes.is_empty() => {
            let endpoints = nodes
                .iter()
                .cloned()
                .map(|status| NodeEndpoint {
                    client: node_client(client, &status),
                    status,
                })
                .collect::<Vec<_>>();
            (Some(nodes), endpoints)
        }
        Ok(_) => {
            let status = NodeStatus {
                node_id: local_node_id,
                address: client.base_url().to_string(),
                api_address: None,
                state: "alive".to_string(),
                incarnation: 0,
                is_council: false,
                is_leader: false,
                labels: BTreeMap::new(),
            };
            (
                Some(Vec::new()),
                vec![NodeEndpoint {
                    status,
                    client: Ok(client.clone()),
                }],
            )
        }
        Err(_error) => {
            let status = NodeStatus {
                node_id: local_node_id,
                address: client.base_url().to_string(),
                api_address: None,
                state: "alive".to_string(),
                incarnation: 0,
                is_council: false,
                is_leader: false,
                labels: BTreeMap::new(),
            };
            (
                None,
                vec![NodeEndpoint {
                    status,
                    client: Ok(client.clone()),
                }],
            )
        }
    };
    // A failed capability request must not silently turn an observed cluster
    // into standalone mode and suppress its council checks.
    let cluster_enabled = capability_cluster_enabled
        || membership
            .as_deref()
            .is_some_and(|membership| !membership.is_empty());

    let collected = futures_util::future::join_all(endpoints.iter().map(collect_node)).await;
    let collected_at = unix_seconds();
    let nodes = collect_node_evidence(membership.as_deref(), &endpoints, &collected, collected_at);

    let leader_client = leader_client(client, &endpoints, cluster_enabled);
    let control_client = leader_client.as_ref().unwrap_or(client);
    let (council_result, desired_result, services_result) = tokio::join!(
        bounded("council status", control_client.council()),
        async {
            match leader_client.as_ref() {
                Some(leader) => bounded("desired apps", leader.desired_apps()).await,
                None if !cluster_enabled => bounded("desired apps", client.desired_apps()).await,
                None => Err("cluster has no reachable observed leader".to_string()),
            }
        },
        bounded("service resolution", client.resolve_all()),
    );

    let council =
        collect_council_evidence(cluster_enabled, &collected, council_result, collected_at);
    let restarts = collect_restarts(&collected, collected_at);
    let deploys = collect_deploys(&collected, collected_at);
    let services = collect_services(desired_result, services_result, app, collected_at);
    let faults = collect_faults(&collected, collected_at);
    let alerts = collect_alerts(&collected, app, collected_at);
    let local = collect_local_diagnostics(&collected, collected_at);
    let registry = collect_registry(capabilities_result, collected_at);
    let recent_logs = collect_logs(client, app, &restarts, collected_at).await;

    Ok(WtfInputs {
        cluster_name,
        collected_at,
        app: app.map(str::to_string),
        cluster: ClusterEvidence {
            nodes,
            council,
            faults,
            disks: local.disks,
            certificates: local.certificates,
            registry,
        },
        applications: ApplicationEvidence {
            restarts,
            deploys,
            services,
            alerts,
            cpu_throttling: local.cpu_throttling,
            recent_logs,
        },
    })
}

async fn collect_node(endpoint: &NodeEndpoint) -> NodeCollection {
    let node_id = endpoint.status.node_id.clone();
    let client = match &endpoint.client {
        Ok(client) => client,
        Err(reason) => {
            return NodeCollection {
                node_id,
                reachable: false,
                diagnostics: Err(reason.clone()),
                events: Err(reason.clone()),
                deploys: Err(reason.clone()),
                alerts: Err(reason.clone()),
                faults: Err(reason.clone()),
            };
        }
    };
    let (health, diagnostics, events, deploys, alerts, faults) = tokio::join!(
        bounded("health", client.health()),
        bounded("diagnostics", client.diagnostics(1)),
        bounded("events", client.events(EVENT_LIMIT)),
        bounded("deploy operations", client.deploy_operations()),
        bounded("alerts", client.alerts()),
        bounded("faults", client.list_faults()),
    );
    NodeCollection {
        node_id,
        reachable: health.is_ok(),
        diagnostics,
        events,
        deploys,
        alerts,
        faults,
    }
}

async fn bounded<T, F>(name: &str, future: F) -> Result<T, String>
where
    F: Future<Output = Result<T, RelishError>>,
{
    match tokio::time::timeout(REQUEST_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(format!("{name}: {error}")),
        Err(_) => Err(format!(
            "{name}: timed out after {}s",
            REQUEST_TIMEOUT.as_secs()
        )),
    }
}

fn capability_identity(report: Option<&ClusterCapabilityReport>) -> (String, bool, String) {
    let evidence = report.and_then(|report| {
        report.nodes.iter().find_map(|node| match node {
            CollectedNodeCapability::Evidence { report, .. } => Some(report.as_ref()),
            CollectedNodeCapability::Unknown { .. } => None,
        })
    });
    evidence.map_or_else(
        || ("unknown".to_string(), false, "local".to_string()),
        |report| {
            (
                report.cluster_name.clone(),
                report.cluster,
                report.node_id.clone(),
            )
        },
    )
}

/// A client for one node's per-node reads, routed through the entry node.
///
/// The CLI often can't dial a node's advertised address: on a laptop the
/// guests sit behind Lima's user-mode network and only node 1's API is
/// forwarded to the host. The entry node can reach every peer, so every
/// per-node read goes through its relay, which carries the caller's own
/// credential. One path for every topology beats guessing which addresses
/// happen to be reachable.
pub(crate) fn node_client(entry: &BunClient, node: &NodeStatus) -> Result<BunClient, String> {
    entry
        .via_node(&node.node_id)
        .map_err(|error| error.to_string())
}

fn leader_client<'a>(
    entry: &'a BunClient,
    endpoints: &'a [NodeEndpoint],
    cluster_enabled: bool,
) -> Option<BunClient> {
    if !cluster_enabled {
        return Some(entry.clone());
    }
    endpoints
        .iter()
        .find(|endpoint| endpoint.status.is_leader)
        .and_then(|endpoint| endpoint.client.clone().ok())
}

fn collect_node_evidence(
    membership: Option<&[NodeStatus]>,
    endpoints: &[NodeEndpoint],
    collected: &[NodeCollection],
    observed_at: u64,
) -> Evidence<Vec<NodeObservation>> {
    let Some(membership) = membership else {
        return Evidence::Unavailable {
            reason: "cluster membership could not be collected".to_string(),
        };
    };
    if membership.is_empty() {
        return Evidence::available(
            observed_at,
            vec![NodeObservation {
                node_id: endpoints[0].status.node_id.clone(),
                membership_state: "alive".to_string(),
                agent_reachable: collected[0].reachable,
            }],
        );
    }
    let health = collected
        .iter()
        .map(|node| (node.node_id.as_str(), node.reachable))
        .collect::<BTreeMap<_, _>>();
    Evidence::available(
        observed_at,
        membership
            .iter()
            .map(|node| NodeObservation {
                node_id: node.node_id.clone(),
                membership_state: node.state.clone(),
                agent_reachable: health.get(node.node_id.as_str()).copied().unwrap_or(false),
            })
            .collect(),
    )
}

fn collect_council_evidence(
    cluster_enabled: bool,
    collected: &[NodeCollection],
    council: Result<CouncilStatus, String>,
    observed_at: u64,
) -> Evidence<CouncilObservation> {
    if !cluster_enabled {
        return Evidence::available(
            observed_at,
            CouncilObservation {
                enabled: false,
                member_count: 0,
                reachable_members: 0,
                leader: None,
            },
        );
    }
    let health = collected
        .iter()
        .map(|node| (node.node_id.as_str(), node.reachable))
        .collect::<BTreeMap<_, _>>();
    match council {
        Ok(council) if !council.members.is_empty() => {
            let reachable_members = council
                .members
                .iter()
                .filter(|member| health.get(member.name.as_str()).copied().unwrap_or(false))
                .count();
            Evidence::available(
                observed_at,
                CouncilObservation {
                    enabled: true,
                    member_count: council.members.len(),
                    reachable_members,
                    leader: council.leader,
                },
            )
        }
        result => Evidence::Unavailable {
            // Gossip role flags can lag Raft membership changes. They cannot
            // establish the voter denominator for a quorum diagnosis.
            reason: result
                .err()
                .unwrap_or_else(|| "council endpoint returned no configured members".to_string()),
        },
    }
}

fn collect_restarts(
    collected: &[NodeCollection],
    observed_at: u64,
) -> Evidence<Vec<RestartObservation>> {
    let mut builder = EvidenceBuilder::default();
    for node in collected {
        match &node.events {
            Ok(events) => {
                if events.len() == EVENT_LIMIT {
                    builder.unavailable(format!(
                        "node {} event ring reached its {}-entry response limit",
                        node.node_id, EVENT_LIMIT
                    ));
                }
                builder.values.extend(events.iter().filter_map(|event| {
                    if event.kind != EventKind::Restart {
                        return None;
                    }
                    Some(RestartObservation {
                        app: event.app.clone()?,
                        namespace: event
                            .namespace
                            .clone()
                            .unwrap_or_else(|| "default".to_string()),
                        instance: event.details.get("instance_id").cloned(),
                        timestamp: event.timestamp,
                        reason: event.message.clone(),
                    })
                }));
            }
            Err(error) => builder.unavailable(format!("node {}: {error}", node.node_id)),
        }
    }
    builder.finish_with_caveat(
        observed_at,
        "restart history is held in node-local, bounded, non-durable event rings",
    )
}

fn collect_deploys(
    collected: &[NodeCollection],
    observed_at: u64,
) -> Evidence<Vec<DeployObservation>> {
    let mut builder = EvidenceBuilder::default();
    let mut seen = BTreeSet::new();
    for node in collected {
        match &node.deploys {
            Ok(snapshot) => {
                for (active, operation) in snapshot
                    .active_deploys
                    .iter()
                    .map(|operation| (true, operation))
                    .chain(snapshot.history.iter().map(|operation| (false, operation)))
                {
                    for target in operation
                        .targets
                        .iter()
                        .filter(|target| target.kind == DeployTargetKind::App)
                    {
                        let key = (
                            operation.id.to_string(),
                            target.name.clone(),
                            target.namespace.clone(),
                        );
                        if !seen.insert(key.clone()) {
                            continue;
                        }
                        builder.values.push(DeployObservation {
                            operation_id: key.0,
                            app: key.1,
                            namespace: key.2,
                            version: None,
                            started_at: operation.started_at,
                            phase: deploy_phase(operation.phase).to_string(),
                            active,
                        });
                    }
                }
            }
            Err(error) => builder.unavailable(format!("node {}: {error}", node.node_id)),
        }
    }
    builder.finish_with_caveat(
        observed_at,
        "terminal deploy history is bounded and process-local; operation targets do not yet carry image versions",
    )
}

fn deploy_phase(phase: DeployOperationPhase) -> &'static str {
    match phase {
        DeployOperationPhase::Accepted => "accepted",
        DeployOperationPhase::DeployingApps => "deploying_apps",
        DeployOperationPhase::DeployingJobs => "deploying_jobs",
        DeployOperationPhase::RebuildingRoutes => "rebuilding_routes",
        DeployOperationPhase::Finished => "finished",
    }
}

fn collect_services(
    desired: Result<Vec<crate::bun::diagnostics::DesiredAppEvidence>, String>,
    resolved: Result<Vec<ResolveResponse>, String>,
    app_scope: Option<&str>,
    observed_at: u64,
) -> Evidence<Vec<ServiceObservation>> {
    let (desired, resolved) = match (desired, resolved) {
        (Ok(desired), Ok(resolved)) => (desired, resolved),
        (desired, resolved) => {
            let mut reasons = Vec::new();
            if let Err(error) = desired {
                reasons.push(error);
            }
            if let Err(error) = resolved {
                reasons.push(error);
            }
            return Evidence::Unavailable {
                reason: reasons.join("; "),
            };
        }
    };
    let resolved = resolved
        .into_iter()
        .map(|service| {
            (
                (service.app_name.clone(), service.namespace.clone()),
                service,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let services = desired
        .into_iter()
        .filter(|desired| desired.service_port.is_some())
        .filter(|desired| app_scope.is_none_or(|app| desired.app == app))
        .map(|desired| {
            let resolved = resolved.get(&(desired.app.clone(), desired.namespace.clone()));
            ServiceObservation {
                app: desired.app,
                namespace: desired.namespace,
                desired_replicas: desired.desired_replicas,
                healthy_backends: resolved.map_or(0, |service| service.healthy_backends),
                total_backends: resolved.map_or(0, |service| service.total_backends),
            }
        })
        .collect();
    Evidence::available(observed_at, services)
}

fn collect_faults(
    collected: &[NodeCollection],
    observed_at: u64,
) -> Evidence<Vec<FaultObservation>> {
    let mut builder = EvidenceBuilder::default();
    for node in collected {
        match &node.faults {
            Ok(faults) => builder.values.extend(faults.iter().map(|fault| {
                let target = fault.target_node.as_ref().map_or_else(
                    || fault.target_service.clone(),
                    |target| format!("node {target}"),
                );
                FaultObservation {
                    id: fault.id,
                    fault_type: fault.fault_type.clone(),
                    target: format!("{target} (reported by {})", node.node_id),
                    injected_by: fault.injected_by.clone(),
                    remaining_seconds: fault.remaining_secs,
                }
            })),
            Err(error) => builder.unavailable(format!("node {}: {error}", node.node_id)),
        }
    }
    builder.finish(observed_at)
}

fn collect_alerts(
    collected: &[NodeCollection],
    app_scope: Option<&str>,
    observed_at: u64,
) -> Evidence<Vec<AlertObservation>> {
    if app_scope.is_some() {
        return Evidence::Unsupported {
            reason: "application-scoped alert collection is not supported; inspect cluster alerts and their series labels".to_string(),
        };
    }
    let mut builder = EvidenceBuilder::default();
    let mut seen = BTreeSet::new();
    for node in collected {
        match &node.alerts {
            Ok(alerts) => {
                for alert in alerts {
                    if alert.state != crate::mayo::alert::AlertPhase::Firing {
                        continue;
                    }
                    let rule = &alert.rule_name;
                    let description = &alert.description;
                    let labels = &alert.labels;
                    let message = if labels.is_empty() {
                        format!("{rule}: {description}")
                    } else {
                        format!("{rule}: {description} {labels:?}")
                    };
                    if seen.insert(message.clone()) {
                        builder.values.push(AlertObservation {
                            app: None,
                            namespace: None,
                            message,
                        });
                    }
                }
            }
            Err(error) => builder.unavailable(format!("node {}: {error}", node.node_id)),
        }
    }
    builder.finish(observed_at)
}

fn collect_local_diagnostics(collected: &[NodeCollection], observed_at: u64) -> LocalEvidenceSet {
    let mut disks = EvidenceBuilder::default();
    let mut cpu = EvidenceBuilder::default();
    let mut certificates = EvidenceBuilder::default();
    for node in collected {
        let snapshot = match &node.diagnostics {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let reason = format!("node {}: {error}", node.node_id);
                disks.unavailable(reason.clone());
                cpu.unavailable(reason.clone());
                certificates.unavailable(reason);
                continue;
            }
        };
        append_diagnostic_source(&snapshot.disks, &node.node_id, &mut disks, |disk| {
            DiskObservation {
                filesystem_id: disk.filesystem_id.clone(),
                node_id: node.node_id.clone(),
                storage_domains: vec![disk.storage_domain.clone()],
                used_bytes: disk.used_bytes,
                total_bytes: disk.total_bytes,
                used_percent: disk.used_percent,
            }
        });
        append_diagnostic_source(&snapshot.cpu_throttling, &node.node_id, &mut cpu, |item| {
            CpuThrottleObservation {
                app: item.app.clone(),
                namespace: item.namespace.clone(),
                throttled_seconds_delta: item.throttled_seconds_delta,
                window_seconds: item.window_seconds,
            }
        });
        append_diagnostic_source(
            &snapshot.certificates,
            &node.node_id,
            &mut certificates,
            |item| CertificateObservation {
                certificate_kind: item.certificate_kind.clone(),
                identity: item.identity.clone(),
                issuer: item.issuer.clone(),
                serial: item.serial.clone(),
                not_after: item.not_after,
                rotation_state: item.rotation_state.clone(),
                automatic_rotation: item.automatic_rotation,
            },
        );
    }
    coalesce_disk_filesystems(&mut disks.values);
    LocalEvidenceSet {
        disks: disks.finish(observed_at),
        cpu_throttling: cpu.finish(observed_at),
        certificates: certificates.finish(observed_at),
    }
}

fn coalesce_disk_filesystems(disks: &mut Vec<DiskObservation>) {
    let mut grouped = BTreeMap::<(String, String), DiskObservation>::new();
    for disk in std::mem::take(disks) {
        let Some(identity) = disk.filesystem_id.as_ref().filter(|id| !id.is_empty()) else {
            // Matching capacities do not prove two observations share a device.
            disks.push(disk);
            continue;
        };
        let key = (disk.node_id.clone(), identity.clone());
        grouped
            .entry(key)
            .and_modify(|existing| {
                existing
                    .storage_domains
                    .extend(disk.storage_domains.clone());
                // Readings can change during collection. Preserve the busiest
                // observed reading as a whole, rather than mixing its counters.
                if disk.used_percent > existing.used_percent {
                    existing.used_bytes = disk.used_bytes;
                    existing.total_bytes = disk.total_bytes;
                    existing.used_percent = disk.used_percent;
                }
            })
            .or_insert(disk);
    }
    for disk in grouped.values_mut() {
        disk.storage_domains.sort();
        disk.storage_domains.dedup();
    }
    disks.extend(grouped.into_values());
    disks.sort_by(|left, right| {
        (&left.node_id, &left.storage_domains).cmp(&(&right.node_id, &right.storage_domains))
    });
}

fn append_diagnostic_source<T, U, F>(
    source: &DiagnosticSource<Vec<T>>,
    node_id: &str,
    builder: &mut EvidenceBuilder<U>,
    map: F,
) where
    F: Fn(&T) -> U,
{
    match source {
        DiagnosticSource::Available { value, .. } => {
            builder.values.extend(value.iter().map(map));
        }
        DiagnosticSource::Degraded { value, reason, .. } => {
            builder.values.extend(value.iter().map(map));
            builder.unavailable(format!("node {node_id}: {reason}"));
        }
        DiagnosticSource::Unavailable { reason } => {
            builder.unavailable(format!("node {node_id}: {reason}"));
        }
        DiagnosticSource::Unsupported { reason } => {
            builder.unsupported(format!("node {node_id}: {reason}"));
        }
    }
}

fn collect_registry(
    report: Result<ClusterCapabilityReport, String>,
    observed_at: u64,
) -> Evidence<Vec<RegistryObservation>> {
    let report = match report {
        Ok(report) => report,
        Err(reason) => return Evidence::Unavailable { reason },
    };
    let mut builder = EvidenceBuilder::default();
    for node in report.nodes {
        match node {
            CollectedNodeCapability::Unknown {
                node_id, reason, ..
            } => builder.unavailable(format!("node {node_id}: {reason}")),
            CollectedNodeCapability::Evidence {
                node_id, report, ..
            } => {
                if !report.registry {
                    builder.unsupported(format!("node {node_id}: Pickle registry is disabled"));
                    continue;
                }
                let Some(placement) = report.placement else {
                    builder.unavailable(format!(
                        "node {node_id}: live registry placement evidence is absent"
                    ));
                    continue;
                };
                let registry = placement.registry;
                builder.values.push(RegistryObservation {
                    node_id,
                    clustered: report.cluster,
                    ready: registry.ready,
                    peer_reachable: registry.peer_reachable,
                    redundancy_possible: registry.redundancy_possible,
                    under_replicated_layers: registry.under_replicated_layers,
                });
            }
        }
    }
    builder.finish(observed_at)
}

async fn collect_logs(
    client: &BunClient,
    app_scope: Option<&str>,
    restarts: &Evidence<Vec<RestartObservation>>,
    observed_at: u64,
) -> Evidence<Vec<LogObservation>> {
    let Some(restarts) = restarts.value() else {
        return Evidence::Unavailable {
            reason: "restart evidence is unavailable, so unhealthy applications are unknown"
                .to_string(),
        };
    };
    let cutoff = observed_at.saturating_sub(15 * 60);
    let mut counts = BTreeMap::<(String, String), usize>::new();
    for restart in restarts
        .iter()
        .filter(|restart| restart.timestamp >= cutoff)
    {
        *counts
            .entry((restart.app.clone(), restart.namespace.clone()))
            .or_default() += 1;
    }
    let mut apps = counts
        .into_iter()
        .filter(|(_, count)| *count >= 3)
        .map(|(resource, _)| resource)
        .filter(|(app, _)| app_scope.is_none_or(|scope| scope == app))
        .collect::<Vec<_>>();
    apps.sort();
    if apps.len() > MAX_LOG_APPS {
        return Evidence::Unavailable {
            reason: format!(
                "{} crashlooping applications exceeds the log correlation limit of {}",
                apps.len(),
                MAX_LOG_APPS
            ),
        };
    }
    let tail = if app_scope.is_some() { 200 } else { 50 };
    let start = observed_at.saturating_sub(30 * 60);
    let results = futures_util::future::join_all(apps.into_iter().map(|(app, namespace)| {
        let client = client.clone();
        async move {
            let result = bounded(
                "recent logs",
                client.log_entries(&app, &namespace, tail, start),
            )
            .await;
            (app, namespace, result)
        }
    }))
    .await;
    let mut builder = EvidenceBuilder::default();
    for (app, namespace, result) in results {
        match result {
            Ok(result) => append_logs(&app, &namespace, result, &mut builder),
            Err(error) => builder.unavailable(format!("{app}/{namespace}: {error}")),
        }
    }
    builder.finish(observed_at)
}

fn append_logs(
    app: &str,
    namespace: &str,
    result: LogQueryResult,
    builder: &mut EvidenceBuilder<LogObservation>,
) {
    for warning in result.warnings {
        match warning {
            crate::ketchup::types::LogQueryWarning::NodeUnresponsive { node_id } => builder
                .unavailable(format!(
                    "{app}/{namespace}: log source node {node_id} did not answer"
                )),
        }
    }
    builder
        .values
        .extend(result.entries.into_iter().map(|entry| {
            let is_error = entry.stream == LogStream::Stderr || line_is_error(&entry.line);
            LogObservation {
                app: app.to_string(),
                namespace: namespace.to_string(),
                timestamp: entry.timestamp,
                is_error,
                line: entry.line,
            }
        }));
}

fn line_is_error(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("level")
                .or_else(|| value.get("severity"))
                .and_then(serde_json::Value::as_str)
                .map(|level| level.eq_ignore_ascii_case("error"))
        })
        .unwrap_or_else(|| line.trim_start().to_ascii_lowercase().starts_with("error"))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::diagnostics::{
        CpuThrottleEvidence, DiagnosticSource, DiskUsageEvidence, PublicCertificateMetadata,
    };

    fn node(address: &str) -> NodeStatus {
        NodeStatus {
            node_id: "node-1".to_string(),
            address: address.to_string(),
            api_address: None,
            state: "alive".to_string(),
            incarnation: 1,
            is_council: true,
            is_leader: true,
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn node_clients_go_through_the_entry_node_never_the_advertised_address() {
        let entry = BunClient::new_with_token("https://127.0.0.1:19117", Some("secret"));

        let mut guest = node("192.168.104.3:7946");
        guest.node_id = "rb-0123456789ab-2".to_string();
        guest.api_address = Some("192.168.104.3:9117".parse().unwrap());
        let relayed = node_client(&entry, &guest).unwrap();
        assert_eq!(
            relayed.base_url(),
            "https://127.0.0.1:19117/v1/nodes/rb-0123456789ab-2/relay"
        );
        // A node with no advertised API address is still reachable.
        assert!(node_client(&entry, &node("10.0.0.8:7946")).is_ok());

        let mut hostile = node("10.0.0.9:7946");
        hostile.node_id = "../v1/token/create?".to_string();
        assert!(node_client(&entry, &hostile).is_err());
    }

    #[test]
    fn service_evidence_uses_desire_even_when_resolver_has_no_entry() {
        let evidence = collect_services(
            Ok(vec![crate::bun::diagnostics::DesiredAppEvidence {
                app: "api".to_string(),
                namespace: "default".to_string(),
                desired_replicas: 2,
                scheduled_replicas: 0,
                service_port: Some(8080),
            }]),
            Ok(Vec::new()),
            None,
            10,
        );

        let services = evidence.value().unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].desired_replicas, 2);
        assert_eq!(services[0].healthy_backends, 0);
    }

    #[test]
    fn degraded_node_diagnostics_keep_values_and_unknown_reason() {
        let snapshot = LocalDiagnosticSnapshot {
            schema_version: 1,
            node_id: "node-1".to_string(),
            observed_at: 10,
            disks: DiagnosticSource::Degraded {
                observed_at: 10,
                value: vec![
                    DiskUsageEvidence {
                        filesystem_id: Some("device:a".into()),
                        storage_domain: "images".to_string(),
                        used_bytes: 95,
                        total_bytes: 100,
                        used_percent: 95.0,
                    },
                    DiskUsageEvidence {
                        filesystem_id: Some("device:a".into()),
                        storage_domain: "logs".to_string(),
                        used_bytes: 95,
                        total_bytes: 100,
                        used_percent: 95.0,
                    },
                ],
                reason: "logs filesystem unavailable".to_string(),
            },
            cpu_throttling: DiagnosticSource::Available {
                observed_at: 10,
                value: vec![CpuThrottleEvidence {
                    app: "api".to_string(),
                    namespace: "default".to_string(),
                    instance: "default__api-0".to_string(),
                    throttled_seconds_delta: 0.5,
                    window_seconds: 1,
                }],
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
                reason: "workload inventory unavailable".to_string(),
            },
        };
        let collected = vec![NodeCollection {
            node_id: "node-1".to_string(),
            reachable: true,
            diagnostics: Ok(snapshot),
            events: Ok(Vec::new()),
            deploys: Err("unused".to_string()),
            alerts: Err("unused".to_string()),
            faults: Err("unused".to_string()),
        }];

        let evidence = collect_local_diagnostics(&collected, 10);

        assert!(matches!(evidence.disks, Evidence::Degraded { .. }));
        assert_eq!(evidence.disks.value().unwrap().len(), 1);
        assert_eq!(evidence.disks.value().unwrap()[0].used_percent, 95.0);
        assert_eq!(
            evidence.disks.value().unwrap()[0].storage_domains,
            ["images", "logs"]
        );
        assert_eq!(
            evidence.cpu_throttling.value().unwrap()[0].throttled_seconds_delta,
            0.5
        );
        assert!(matches!(evidence.certificates, Evidence::Degraded { .. }));
    }

    #[test]
    fn labelled_alerts_remain_distinct_across_collected_nodes() {
        let alerts = ["hot-a", "hot-b"].map(|node| crate::mayo::alert::AlertStatus {
            rule_name: "cpu".into(),
            description: "CPU high".into(),
            state: crate::mayo::alert::AlertPhase::Firing,
            severity: crate::mayo::alert::AlertSeverity::Critical,
            labels: BTreeMap::from([("node".into(), node.into())]),
            since: Some(1),
        });
        let collected = ["reporter-a", "reporter-b"].map(|node| NodeCollection {
            node_id: node.into(),
            reachable: true,
            diagnostics: Err("unused".into()),
            events: Ok(Vec::new()),
            deploys: Err("unused".into()),
            alerts: Ok(alerts.to_vec()),
            faults: Err("unused".into()),
        });
        let evidence = collect_alerts(&collected, None, 10);
        let observed = evidence.value().unwrap();
        assert_eq!(observed.len(), 2);
        assert!(observed[0].message.contains("hot-a"));
        assert!(observed[1].message.contains("hot-b"));
    }

    #[test]
    fn clean_restart_collection_is_a_caveat_not_a_degrade() {
        let collected = vec![NodeCollection {
            node_id: "node-1".to_string(),
            reachable: true,
            diagnostics: Err("unused".to_string()),
            events: Ok(Vec::new()),
            deploys: Err("unused".to_string()),
            alerts: Err("unused".to_string()),
            faults: Err("unused".to_string()),
        }];

        let evidence = collect_restarts(&collected, 10);

        assert!(matches!(evidence, Evidence::AvailableWithCaveat { .. }));
        assert!(evidence.unknown_reason().is_none());
        assert!(evidence.caveat().is_some());
    }

    #[test]
    fn failed_restart_collection_stays_unknown() {
        let collected = vec![NodeCollection {
            node_id: "node-1".to_string(),
            reachable: false,
            diagnostics: Err("unused".to_string()),
            events: Err("event ring unreachable".to_string()),
            deploys: Err("unused".to_string()),
            alerts: Err("unused".to_string()),
            faults: Err("unused".to_string()),
        }];

        let evidence = collect_restarts(&collected, 10);

        assert!(evidence.caveat().is_none());
        assert!(evidence.unknown_reason().is_some());
    }

    #[test]
    fn structured_and_plain_error_logs_are_classified() {
        assert!(line_is_error(r#"{"level":"ERROR","message":"boom"}"#));
        assert!(line_is_error("ERROR connection refused"));
        assert!(!line_is_error("request completed"));
    }

    #[tokio::test]
    async fn disk_collection_groups_by_identity_not_matching_capacity() {
        use axum::{Json, Router, routing::get};
        for (identities, usages, expected_count) in [
            ([None, None], [50, 50], 2),
            ([Some("device-a"), Some("device-b")], [50, 50], 2),
            ([Some("device-a"), Some("device-a")], [50, 60], 1),
        ] {
            let mut disks = Vec::new();
            for (index, domain) in ["images", "logs"].into_iter().enumerate() {
                let mut disk = serde_json::json!({"storage_domain":domain, "used_bytes":usages[index], "total_bytes":100, "used_percent":usages[index] as f64});
                if let Some(identity) = identities[index] {
                    disk["filesystem_id"] = identity.into();
                }
                disks.push(disk);
            }
            let snapshot = serde_json::json!({
                "schema_version":1, "node_id":"node-a", "observed_at":10,
                "disks":{"state":"available","observed_at":10,"value":disks},
                "cpu_throttling":{"state":"unsupported","reason":"fixture"},
                "certificates":{"state":"unsupported","reason":"fixture"},
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = Router::new()
                .route(
                    "/v1/health",
                    get(|| async { Json(serde_json::json!({"status":"ok"})) }),
                )
                .route(
                    "/v1/cluster/nodes",
                    get(|| async { Json(Vec::<NodeStatus>::new()) }),
                )
                .route(
                    "/v1/diagnostics",
                    get(move || {
                        let snapshot = snapshot.clone();
                        async move { Json(snapshot) }
                    }),
                );
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let inputs = collect(&BunClient::new(&format!("http://{address}")), None)
                .await
                .unwrap();
            server.abort();
            let _ = server.await;
            let disks = inputs.cluster.disks.value().expect("disk evidence");
            assert_eq!(disks.len(), expected_count);
            if expected_count == 1 {
                assert_eq!(disks[0].storage_domains, ["images", "logs"]);
                assert_eq!(disks[0].used_percent, 60.0);
            }
        }
    }

    #[tokio::test]
    async fn missing_council_membership_never_invents_quorum_loss() {
        use axum::{Json, Router, routing::get};
        for (stale_council_flag, empty_answer) in [(false, false), (true, false), (true, true)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = Router::new()
                .route(
                    "/v1/cluster/council",
                    get(move || async move {
                        (
                            if empty_answer {
                                axum::http::StatusCode::OK
                            } else {
                                axum::http::StatusCode::SERVICE_UNAVAILABLE
                            },
                            Json(CouncilStatus::default()),
                        )
                    }),
                )
                .route(
                    "/v1/health",
                    get(|| async { Json(serde_json::json!({"status":"ok"})) }),
                )
                .route(
                    "/v1/cluster/nodes",
                    get(move || async move {
                        Json(vec![NodeStatus {
                            node_id: "node-a".into(),
                            address: address.to_string(),
                            api_address: Some(address),
                            state: "alive".into(),
                            incarnation: 0,
                            is_council: stale_council_flag,
                            is_leader: false,
                            labels: BTreeMap::new(),
                        }])
                    }),
                );
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let inputs = collect(&BunClient::new(&format!("http://{address}")), None)
                .await
                .unwrap();
            server.abort();
            let _ = server.await;
            assert!(
                inputs.cluster.council.value().is_none(),
                "gossip flags cannot establish configured voters"
            );
            let report = super::super::diagnose(&inputs);
            assert!(
                report
                    .unknown
                    .iter()
                    .any(|finding| finding.source == "council")
            );
            assert!(
                !report
                    .critical
                    .iter()
                    .any(|finding| ["quorum-loss", "no-leader"].contains(&finding.id.as_str()))
            );
        }
    }

    #[test]
    fn standalone_council_is_an_observed_non_requirement() {
        let evidence = collect_council_evidence(false, &[], Ok(CouncilStatus::default()), 10);
        let council = evidence.value().unwrap();

        assert!(!council.enabled);
    }
}
