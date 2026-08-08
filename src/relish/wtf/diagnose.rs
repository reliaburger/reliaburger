use std::collections::BTreeMap;

use super::WTF_SCHEMA_VERSION;
use super::model::{
    ApplicationEvidence, ClusterEvidence, CorrelatedEvent, DeployObservation, Evidence,
    LogObservation, RestartObservation, WtfFinding, WtfInputs, WtfOk, WtfReport, WtfSummary,
    WtfUnknown,
};

const CRASHLOOP_WINDOW_SECONDS: u64 = 15 * 60;
const RECENT_DEPLOY_SECONDS: u64 = 30 * 60;
const STUCK_DEPLOY_SECONDS: u64 = 15 * 60;
const CERTIFICATE_WARNING_SECONDS: u64 = 14 * 24 * 60 * 60;

/// Diagnose a cluster from an immutable evidence snapshot.
///
/// The function performs no I/O and uses `collected_at` rather than wall-clock
/// time, so a captured input always produces the same report.
pub fn diagnose(inputs: &WtfInputs) -> WtfReport {
    let node_count = inputs.cluster.nodes.value().map_or(0, Vec::len);
    let mut report = WtfReport {
        schema_version: WTF_SCHEMA_VERSION,
        cluster_name: inputs.cluster_name.clone(),
        collected_at: inputs.collected_at,
        node_count,
        critical: Vec::new(),
        warnings: Vec::new(),
        unknown: Vec::new(),
        ok: Vec::new(),
        summary: WtfSummary::default(),
    };

    if inputs.app.is_none() {
        record_cluster_unknowns(&inputs.cluster, &mut report);
        check_nodes(inputs, &mut report);
        check_council(inputs, &mut report);
        check_faults(inputs, &mut report);
        check_disks(inputs, &mut report);
        check_certificates(inputs, &mut report);
        check_registry(inputs, &mut report);
    }
    record_application_unknowns(&inputs.applications, inputs.app.as_deref(), &mut report);
    check_crashloops(inputs, &mut report);
    check_services(inputs, &mut report);
    check_deploys(inputs, &mut report);
    check_alerts(inputs, &mut report);
    check_cpu_throttling(inputs, &mut report);

    report.critical.sort_by(finding_order);
    report.warnings.sort_by(finding_order);
    report
        .unknown
        .sort_by(|left, right| left.source.cmp(&right.source));
    report.ok.sort_by(|left, right| left.id.cmp(&right.id));
    report.summary = WtfSummary {
        critical_count: report.critical.len(),
        warning_count: report.warnings.len(),
        unknown_count: report.unknown.len(),
        ok_count: report.ok.len(),
    };
    report
}

fn finding_order(left: &WtfFinding, right: &WtfFinding) -> std::cmp::Ordering {
    (&left.id, &left.affected_resource, &left.title).cmp(&(
        &right.id,
        &right.affected_resource,
        &right.title,
    ))
}

fn record_cluster_unknowns(evidence: &ClusterEvidence, report: &mut WtfReport) {
    record_unknown("nodes", &evidence.nodes, "cluster", report);
    record_unknown("council", &evidence.council, "cluster", report);
    record_unknown("faults", &evidence.faults, "cluster", report);
    record_unknown("disks", &evidence.disks, "cluster", report);
    record_unknown("certificates", &evidence.certificates, "cluster", report);
    record_unknown("registry", &evidence.registry, "cluster", report);
}

fn record_application_unknowns(
    evidence: &ApplicationEvidence,
    app: Option<&str>,
    report: &mut WtfReport,
) {
    let resource = app.map_or_else(|| "cluster".to_string(), |name| format!("app.{name}"));
    record_unknown("restarts", &evidence.restarts, &resource, report);
    record_unknown("deploys", &evidence.deploys, &resource, report);
    record_unknown("services", &evidence.services, &resource, report);
    record_unknown("alerts", &evidence.alerts, &resource, report);
    record_unknown(
        "cpu_throttling",
        &evidence.cpu_throttling,
        &resource,
        report,
    );
    record_unknown("recent_logs", &evidence.recent_logs, &resource, report);
}

fn record_unknown<T>(source: &str, evidence: &Evidence<T>, resource: &str, report: &mut WtfReport) {
    if let Some(reason) = evidence.unknown_reason() {
        report.unknown.push(WtfUnknown {
            source: source.to_string(),
            reason: reason.to_string(),
            affected_resource: resource.to_string(),
        });
    }
}

fn check_nodes(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(nodes) = inputs.cluster.nodes.value() else {
        return;
    };
    let mut problems = 0;
    for node in nodes {
        if !node.agent_reachable {
            problems += 1;
            report.critical.push(WtfFinding {
                id: "unreachable-nodes".to_string(),
                title: format!("node {} did not answer its Bun API", node.node_id),
                details: vec!["authenticated per-node evidence collection failed".to_string()],
                suggestion: "check the node's bun agent and network: `relish dev shell <node>` / `journalctl -u bun`".to_string(),
                correlated_events: Vec::new(),
                affected_resource: format!("node.{}", node.node_id),
            });
        }
        if matches!(
            node.membership_state.to_ascii_lowercase().as_str(),
            "dead" | "suspect"
        ) {
            problems += 1;
            report.critical.push(WtfFinding {
                id: "node-dead".to_string(),
                title: format!("node {} is {}", node.node_id, node.membership_state),
                details: vec!["Mustard membership does not consider this node alive".to_string()],
                suggestion: format!(
                    "node {} is {}; if expected, drain it; if not, check the host",
                    node.node_id, node.membership_state
                ),
                correlated_events: Vec::new(),
                affected_resource: format!("node.{}", node.node_id),
            });
        }
    }
    if problems == 0 {
        report.ok.push(WtfOk {
            id: "nodes".to_string(),
            description: format!("all {} nodes alive and answering", nodes.len()),
        });
    }
}

fn check_council(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(council) = inputs.cluster.council.value() else {
        return;
    };
    if !council.enabled {
        report.ok.push(WtfOk {
            id: "council".to_string(),
            description: "standalone mode does not require a council".to_string(),
        });
        return;
    }
    let mut healthy = true;
    if council.leader.is_none() {
        healthy = false;
        report.critical.push(WtfFinding {
            id: "no-leader".to_string(),
            title: "council has no elected leader".to_string(),
            details: vec![format!(
                "{} of {} members answered",
                council.reachable_members, council.member_count
            )],
            suggestion:
                "council has no leader; check quorum and recent elections: `relish council`"
                    .to_string(),
            correlated_events: Vec::new(),
            affected_resource: "council".to_string(),
        });
    }
    let quorum = council.member_count / 2 + 1;
    if council.reachable_members < quorum {
        healthy = false;
        report.critical.push(WtfFinding {
            id: "quorum-loss".to_string(),
            title: "council has lost quorum".to_string(),
            details: vec![format!(
                "{} reachable members; {} required from a council of {}",
                council.reachable_members, quorum, council.member_count
            )],
            suggestion: "restore failed council nodes or remove them from the council".to_string(),
            correlated_events: Vec::new(),
            affected_resource: "council".to_string(),
        });
    }
    if healthy {
        report.ok.push(WtfOk {
            id: "council".to_string(),
            description: format!(
                "council quorum healthy ({}/{})",
                council.reachable_members, council.member_count
            ),
        });
    }
}

fn check_crashloops(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(restarts) = inputs.applications.restarts.value() else {
        return;
    };
    let cutoff = inputs.collected_at.saturating_sub(CRASHLOOP_WINDOW_SECONDS);
    let mut grouped: BTreeMap<(&str, &str), Vec<&RestartObservation>> = BTreeMap::new();
    for restart in restarts.iter().filter(|restart| {
        restart.timestamp >= cutoff && app_matches(inputs.app.as_deref(), &restart.app)
    }) {
        grouped
            .entry((&restart.app, &restart.namespace))
            .or_default()
            .push(restart);
    }

    let mut found = false;
    for ((app, namespace), mut app_restarts) in grouped {
        if app_restarts.len() < 3 {
            continue;
        }
        found = true;
        app_restarts.sort_by_key(|restart| restart.timestamp);
        let mut correlated_events = app_restarts
            .iter()
            .map(|restart| CorrelatedEvent {
                timestamp: restart.timestamp,
                kind: "restart".to_string(),
                message: restart.reason.clone(),
            })
            .collect::<Vec<_>>();
        let deploy = recent_deploy(inputs, app, namespace);
        if let Some(deploy) = deploy {
            correlated_events.push(CorrelatedEvent {
                timestamp: deploy.started_at,
                kind: "deploy".to_string(),
                message: format!("deploy {} entered {}", deploy.operation_id, deploy.phase),
            });
        }
        let log = first_error_log(inputs, app, namespace);
        if let Some(log) = log {
            correlated_events.push(CorrelatedEvent {
                timestamp: log.timestamp,
                kind: "log".to_string(),
                message: log.line.clone(),
            });
        }
        correlated_events.sort_by_key(|event| event.timestamp);

        let suggestion = if let Some(deploy) = deploy {
            let version = deploy.version.as_deref().unwrap_or("the recent version");
            format!(
                "the restart window follows deploy {} ({version}); inspect it and run `relish rollback {app}` if it caused the failure",
                deploy.operation_id
            )
        } else {
            format!(
                "app {app} is crashlooping; inspect its recent logs and run `relish rollback {app}` if this started after a deploy"
            )
        };
        report.critical.push(WtfFinding {
            id: "crashloop".to_string(),
            title: format!("app {app}/{namespace} restarted repeatedly"),
            details: vec![format!(
                "{} timestamped restarts in the last 15 minutes{}",
                app_restarts.len(),
                log.map_or_else(String::new, |entry| format!(
                    "; first error: {}",
                    entry.line
                ))
            )],
            suggestion,
            correlated_events,
            affected_resource: app_resource(app, namespace),
        });
    }
    if !found {
        report.ok.push(WtfOk {
            id: "crashloops".to_string(),
            description: "no applications have three timestamped restarts in 15 minutes"
                .to_string(),
        });
    }
}

fn recent_deploy<'a>(
    inputs: &'a WtfInputs,
    app: &str,
    namespace: &str,
) -> Option<&'a DeployObservation> {
    let cutoff = inputs.collected_at.saturating_sub(RECENT_DEPLOY_SECONDS);
    inputs
        .applications
        .deploys
        .value()?
        .iter()
        .filter(|deploy| {
            deploy.app == app && deploy.namespace == namespace && deploy.started_at >= cutoff
        })
        .max_by_key(|deploy| deploy.started_at)
}

fn first_error_log<'a>(
    inputs: &'a WtfInputs,
    app: &str,
    namespace: &str,
) -> Option<&'a LogObservation> {
    inputs
        .applications
        .recent_logs
        .value()?
        .iter()
        .filter(|log| log.app == app && log.namespace == namespace && log.is_error)
        .min_by_key(|log| log.timestamp)
}

fn check_services(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(services) = inputs.applications.services.value() else {
        return;
    };
    let mut found = false;
    for service in services
        .iter()
        .filter(|service| app_matches(inputs.app.as_deref(), &service.app))
        .filter(|service| service.desired_replicas > 0 && service.healthy_backends == 0)
    {
        found = true;
        let related = has_crashloop(inputs, &service.app, &service.namespace)
            .then_some("the crashloop finding for this app")
            .or_else(|| {
                has_stuck_deploy(inputs, &service.app, &service.namespace)
                    .then_some("the deploy-stuck finding for this app")
            });
        let (details, suggestion) = if let Some(related) = related {
            (
                vec![format!(
                    "0 of {} backends are healthy; see {related}",
                    service.total_backends
                )],
                format!("resolve {related} first, then verify service discovery"),
            )
        } else {
            (
                vec![format!(
                    "0 of {} backends are healthy for {} desired replicas",
                    service.total_backends, service.desired_replicas
                )],
                format!(
                    "service {} has no healthy backends; check health-check configuration and instance logs",
                    service.app
                ),
            )
        };
        report.critical.push(WtfFinding {
            id: "no-backends".to_string(),
            title: format!(
                "service {}/{} has no healthy backends",
                service.app, service.namespace
            ),
            details,
            suggestion,
            correlated_events: Vec::new(),
            affected_resource: app_resource(&service.app, &service.namespace),
        });
    }
    if !found {
        report.ok.push(WtfOk {
            id: "services".to_string(),
            description: "all deployed services have a healthy backend".to_string(),
        });
    }
}

fn has_crashloop(inputs: &WtfInputs, app: &str, namespace: &str) -> bool {
    let cutoff = inputs.collected_at.saturating_sub(CRASHLOOP_WINDOW_SECONDS);
    inputs
        .applications
        .restarts
        .value()
        .map(|restarts| {
            restarts
                .iter()
                .filter(|restart| {
                    restart.app == app
                        && restart.namespace == namespace
                        && restart.timestamp >= cutoff
                })
                .count()
                >= 3
        })
        .unwrap_or(false)
}

fn has_stuck_deploy(inputs: &WtfInputs, app: &str, namespace: &str) -> bool {
    inputs
        .applications
        .deploys
        .value()
        .map(|deploys| {
            deploys.iter().any(|deploy| {
                deploy.app == app
                    && deploy.namespace == namespace
                    && deploy.active
                    && inputs.collected_at.saturating_sub(deploy.started_at) > STUCK_DEPLOY_SECONDS
            })
        })
        .unwrap_or(false)
}

fn check_deploys(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(deploys) = inputs.applications.deploys.value() else {
        return;
    };
    let mut found = false;
    for deploy in deploys
        .iter()
        .filter(|deploy| app_matches(inputs.app.as_deref(), &deploy.app))
        .filter(|deploy| {
            deploy.active
                && inputs.collected_at.saturating_sub(deploy.started_at) > STUCK_DEPLOY_SECONDS
        })
    {
        found = true;
        let elapsed = inputs.collected_at.saturating_sub(deploy.started_at);
        report.warnings.push(WtfFinding {
            id: "deploy-stuck".to_string(),
            title: format!(
                "deploy of {}/{} appears stuck",
                deploy.app, deploy.namespace
            ),
            details: vec![format!(
                "operation {} has remained in {} for {} minutes",
                deploy.operation_id,
                deploy.phase,
                elapsed / 60
            )],
            suggestion: format!(
                "deploy of {} has been running for {} minutes; consider `relish rollback {}`",
                deploy.app,
                elapsed / 60,
                deploy.app
            ),
            correlated_events: vec![CorrelatedEvent {
                timestamp: deploy.started_at,
                kind: "deploy".to_string(),
                message: format!("operation {} started", deploy.operation_id),
            }],
            affected_resource: app_resource(&deploy.app, &deploy.namespace),
        });
    }
    if !found {
        report.ok.push(WtfOk {
            id: "deploys".to_string(),
            description: "no deploy has been active for more than 15 minutes".to_string(),
        });
    }
}

fn check_faults(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(faults) = inputs.cluster.faults.value() else {
        return;
    };
    if faults.is_empty() {
        report.ok.push(WtfOk {
            id: "faults".to_string(),
            description: "no active Smoker faults".to_string(),
        });
        return;
    }
    report.warnings.push(WtfFinding {
        id: "active-faults".to_string(),
        title: format!("{} Smoker fault(s) are active", faults.len()),
        details: faults
            .iter()
            .map(|fault| {
                format!(
                    "fault {}: {} on {} by {} ({}s remaining)",
                    fault.id,
                    fault.fault_type,
                    fault.target,
                    fault.injected_by,
                    fault.remaining_seconds
                )
            })
            .collect(),
        suggestion: format!(
            "{} active fault(s) injected by Smoker; clear them with `relish fault clear`",
            faults.len()
        ),
        correlated_events: Vec::new(),
        affected_resource: "cluster".to_string(),
    });
}

fn check_alerts(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(alerts) = inputs.applications.alerts.value() else {
        return;
    };
    let relevant = alerts
        .iter()
        .filter(|alert| {
            inputs
                .app
                .as_deref()
                .is_none_or(|app| alert.app.as_deref() == Some(app))
        })
        .collect::<Vec<_>>();
    if relevant.is_empty() {
        report.ok.push(WtfOk {
            id: "alerts".to_string(),
            description: "no relevant alerts are firing".to_string(),
        });
        return;
    }
    for alert in relevant {
        let resource = match (&alert.app, &alert.namespace) {
            (Some(app), Some(namespace)) => app_resource(app, namespace),
            (Some(app), None) => format!("app.{app}"),
            _ => "cluster".to_string(),
        };
        report.warnings.push(WtfFinding {
            id: "alerts-firing".to_string(),
            title: alert.message.clone(),
            details: Vec::new(),
            suggestion: "inspect the alert's current metrics and recent events".to_string(),
            correlated_events: Vec::new(),
            affected_resource: resource,
        });
    }
}

fn check_disks(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(disks) = inputs.cluster.disks.value() else {
        return;
    };
    let mut found = false;
    for disk in disks.iter().filter(|disk| disk.used_percent >= 85.0) {
        found = true;
        let finding = WtfFinding {
            id: "disk-high".to_string(),
            title: format!(
                "node {} filesystem for {} is {:.1}% full",
                disk.node_id,
                disk.storage_domains.join(", "),
                disk.used_percent
            ),
            details: vec![format!(
                "{} of {} bytes used; configured domains: {}",
                disk.used_bytes,
                disk.total_bytes,
                disk.storage_domains.join(", ")
            )],
            suggestion: format!(
                "node {} disk is at {:.1}%; prune images (`relish pickle gc`) or logs",
                disk.node_id, disk.used_percent
            ),
            correlated_events: Vec::new(),
            affected_resource: format!("node.{}", disk.node_id),
        };
        if disk.used_percent >= 95.0 {
            report.critical.push(finding);
        } else {
            report.warnings.push(finding);
        }
    }
    if !found {
        report.ok.push(WtfOk {
            id: "disks".to_string(),
            description: "all observed storage domains are below 85% usage".to_string(),
        });
    }
}

fn check_cpu_throttling(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(observations) = inputs.applications.cpu_throttling.value() else {
        return;
    };
    let mut found = false;
    for observation in observations
        .iter()
        .filter(|item| app_matches(inputs.app.as_deref(), &item.app))
        .filter(|item| item.throttled_seconds_delta > 0.0)
    {
        found = true;
        report.warnings.push(WtfFinding {
            id: "cpu-throttling".to_string(),
            title: format!(
                "app {}/{} was CPU throttled",
                observation.app, observation.namespace
            ),
            details: vec![format!(
                "{:.3}s of throttling over a {}s observation window",
                observation.throttled_seconds_delta, observation.window_seconds
            )],
            suggestion: format!(
                "app {} is being CPU throttled; raise the limit or scale out",
                observation.app
            ),
            correlated_events: Vec::new(),
            affected_resource: app_resource(&observation.app, &observation.namespace),
        });
    }
    if !found {
        report.ok.push(WtfOk {
            id: "cpu-throttling".to_string(),
            description: "no application accumulated throttled CPU time".to_string(),
        });
    }
}

fn check_certificates(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(certificates) = inputs.cluster.certificates.value() else {
        return;
    };
    let warning_at = inputs
        .collected_at
        .saturating_add(CERTIFICATE_WARNING_SECONDS);
    let mut found = false;
    let mut rotation_complete = true;
    for certificate in certificates {
        if !certificate.automatic_rotation {
            rotation_complete = false;
        }
        let rotation_unhealthy = matches!(
            certificate.rotation_state.as_str(),
            "needs_rotation" | "grace_period" | "expired"
        );
        // Short-lived workload leaves are expected to have less than 14 days
        // remaining. Warn only when their automatic rotation is unhealthy.
        if certificate.not_after > warning_at
            || (certificate.automatic_rotation && !rotation_unhealthy)
        {
            continue;
        }
        found = true;
        let remaining = certificate.not_after.saturating_sub(inputs.collected_at);
        report.warnings.push(WtfFinding {
            id: "cert-expiring".to_string(),
            title: format!("certificate for {} expires soon", certificate.identity),
            details: vec![format!(
                "{} certificate from {} (serial {}) expires in {} days; rotation state: {}",
                certificate.certificate_kind,
                certificate.issuer,
                certificate.serial,
                remaining / (24 * 60 * 60),
                certificate.rotation_state
            )],
            suggestion: format!(
                "certificate for {} expires soon; rotation should be automatic, so check the CA and rotation state",
                certificate.identity
            ),
            correlated_events: Vec::new(),
            affected_resource: format!("identity.{}", certificate.identity),
        });
    }
    if !rotation_complete
        && !report
            .unknown
            .iter()
            .any(|unknown| unknown.source == "certificates")
    {
        report.unknown.push(WtfUnknown {
            source: "certificate_rotation".to_string(),
            reason: "at least one certificate consumer cannot hot-rotate its leaf".to_string(),
            affected_resource: "cluster".to_string(),
        });
    }
    if !found && rotation_complete {
        report.ok.push(WtfOk {
            id: "certificates".to_string(),
            description: "all observed certificates are valid for at least 14 days".to_string(),
        });
    }
}

fn check_registry(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(registries) = inputs.cluster.registry.value() else {
        return;
    };
    let mut found = false;
    for registry in registries.iter().filter(|registry| {
        !registry.ready
            || (registry.clustered && !registry.peer_reachable)
            || (registry.clustered && !registry.redundancy_possible)
            || registry.under_replicated_layers > 0
    }) {
        found = true;
        report.warnings.push(WtfFinding {
            id: "registry-redundancy".to_string(),
            title: format!("Pickle is degraded on node {}", registry.node_id),
            details: vec![format!(
                "ready={}, peer_reachable={}, redundancy_possible={}, under_replicated_layers={}",
                registry.ready,
                registry.peer_reachable,
                registry.redundancy_possible,
                registry.under_replicated_layers
            )],
            suggestion: "restore registry peer reachability and allow Pickle to heal under-replicated layers".to_string(),
            correlated_events: Vec::new(),
            affected_resource: format!("node.{}", registry.node_id),
        });
    }
    if !found {
        report.ok.push(WtfOk {
            id: "registry".to_string(),
            description: "Pickle is reachable and its known layers meet the redundancy target"
                .to_string(),
        });
    }
}

fn app_matches(scope: Option<&str>, app: &str) -> bool {
    scope.is_none_or(|scope| scope == app)
}

fn app_resource(app: &str, namespace: &str) -> String {
    format!("app.{app}/{namespace}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::wtf::{
        AlertObservation, CertificateObservation, CouncilObservation, CpuThrottleObservation,
        DeployObservation, DiskObservation, FaultObservation, LogObservation, NodeObservation,
        RegistryObservation, RestartObservation, ServiceObservation,
    };

    const NOW: u64 = 2_000_000;

    fn healthy_inputs() -> WtfInputs {
        WtfInputs {
            cluster_name: "test".to_string(),
            collected_at: NOW,
            app: None,
            cluster: ClusterEvidence {
                nodes: available(vec![NodeObservation {
                    node_id: "node-1".to_string(),
                    membership_state: "Alive".to_string(),
                    agent_reachable: true,
                }]),
                council: available(CouncilObservation {
                    enabled: true,
                    member_count: 1,
                    reachable_members: 1,
                    leader: Some("node-1".to_string()),
                }),
                faults: available(Vec::new()),
                disks: available(vec![DiskObservation {
                    node_id: "node-1".to_string(),
                    storage_domains: vec!["images".to_string()],
                    used_bytes: 20,
                    total_bytes: 100,
                    used_percent: 20.0,
                }]),
                certificates: available(vec![CertificateObservation {
                    certificate_kind: "node".to_string(),
                    identity: "node-1".to_string(),
                    issuer: "CN=node-ca".to_string(),
                    serial: "01".to_string(),
                    not_after: NOW + 30 * 24 * 60 * 60,
                    rotation_state: "valid".to_string(),
                    automatic_rotation: true,
                }]),
                registry: available(vec![RegistryObservation {
                    node_id: "node-1".to_string(),
                    clustered: true,
                    ready: true,
                    peer_reachable: true,
                    redundancy_possible: true,
                    under_replicated_layers: 0,
                }]),
            },
            applications: ApplicationEvidence {
                restarts: available(Vec::new()),
                deploys: available(Vec::new()),
                services: available(vec![ServiceObservation {
                    app: "api".to_string(),
                    namespace: "default".to_string(),
                    desired_replicas: 1,
                    healthy_backends: 1,
                    total_backends: 1,
                }]),
                alerts: available(Vec::new()),
                cpu_throttling: available(Vec::new()),
                recent_logs: available(Vec::new()),
            },
        }
    }

    fn available<T>(value: T) -> Evidence<T> {
        Evidence::available(NOW, value)
    }

    #[test]
    fn healthy_evidence_produces_only_ok_results() {
        let report = diagnose(&healthy_inputs());

        assert!(report.critical.is_empty());
        assert!(report.warnings.is_empty());
        assert!(report.unknown.is_empty());
        assert_eq!(report.node_count, 1);
        assert_eq!(report.summary.ok_count, report.ok.len());
        assert!(report.ok.iter().any(|ok| ok.id == "nodes"));
    }

    #[test]
    fn unavailable_source_is_unknown_and_never_ok() {
        let mut inputs = healthy_inputs();
        inputs.cluster.nodes = Evidence::Unavailable {
            reason: "node request timed out".to_string(),
        };

        let report = diagnose(&inputs);

        assert!(report.unknown.iter().any(|item| item.source == "nodes"));
        assert!(!report.ok.iter().any(|ok| ok.id == "nodes"));
        assert_eq!(report.node_count, 0);
    }

    #[test]
    fn unreachable_and_suspect_nodes_are_critical() {
        let mut inputs = healthy_inputs();
        inputs.cluster.nodes = available(vec![NodeObservation {
            node_id: "node-2".to_string(),
            membership_state: "Suspect".to_string(),
            agent_reachable: false,
        }]);

        let report = diagnose(&inputs);

        assert!(
            report
                .critical
                .iter()
                .any(|finding| finding.id == "unreachable-nodes")
        );
        assert!(
            report
                .critical
                .iter()
                .any(|finding| finding.id == "node-dead")
        );
        assert!(!report.ok.iter().any(|ok| ok.id == "nodes"));
    }

    #[test]
    fn missing_leader_and_quorum_are_separate_critical_findings() {
        let mut inputs = healthy_inputs();
        inputs.cluster.council = available(CouncilObservation {
            enabled: true,
            member_count: 3,
            reachable_members: 1,
            leader: None,
        });

        let report = diagnose(&inputs);

        assert!(report.critical.iter().any(|item| item.id == "no-leader"));
        assert!(report.critical.iter().any(|item| item.id == "quorum-loss"));
        assert!(!report.ok.iter().any(|ok| ok.id == "council"));
    }

    #[test]
    fn crashloop_requires_three_restarts_inside_fifteen_minutes() {
        let mut inputs = healthy_inputs();
        inputs.applications.restarts = available(vec![
            restart(NOW - 800),
            restart(NOW - 400),
            restart(NOW - 10),
            restart(NOW - 901),
        ]);

        let report = diagnose(&inputs);

        let finding = report
            .critical
            .iter()
            .find(|item| item.id == "crashloop")
            .unwrap();
        assert!(finding.details[0].contains("3 timestamped restarts"));
    }

    #[test]
    fn lifetime_restart_count_is_not_an_input_to_crashloop_detection() {
        let mut inputs = healthy_inputs();
        inputs.applications.restarts = available(vec![restart(NOW - 901), restart(NOW - 50)]);

        let report = diagnose(&inputs);

        assert!(!report.critical.iter().any(|item| item.id == "crashloop"));
        assert!(report.ok.iter().any(|ok| ok.id == "crashloops"));
    }

    #[test]
    fn crashloop_correlates_recent_deploy_and_first_error_log() {
        let mut inputs = healthy_inputs();
        inputs.applications.restarts = available(vec![
            restart(NOW - 30),
            restart(NOW - 20),
            restart(NOW - 10),
        ]);
        inputs.applications.deploys = available(vec![DeployObservation {
            operation_id: "deploy-7".to_string(),
            app: "api".to_string(),
            namespace: "default".to_string(),
            version: Some("v2".to_string()),
            started_at: NOW - 600,
            phase: "completed".to_string(),
            active: false,
        }]);
        inputs.applications.recent_logs = available(vec![
            LogObservation {
                app: "api".to_string(),
                namespace: "default".to_string(),
                timestamp: NOW - 25,
                is_error: true,
                line: "database refused connection".to_string(),
            },
            LogObservation {
                app: "api".to_string(),
                namespace: "default".to_string(),
                timestamp: NOW - 5,
                is_error: true,
                line: "later error".to_string(),
            },
        ]);

        let report = diagnose(&inputs);
        let finding = report
            .critical
            .iter()
            .find(|item| item.id == "crashloop")
            .unwrap();

        assert!(finding.suggestion.contains("deploy-7 (v2)"));
        assert!(
            finding
                .correlated_events
                .iter()
                .any(|event| event.kind == "deploy")
        );
        assert!(
            finding
                .correlated_events
                .iter()
                .any(|event| event.message == "database refused connection")
        );
    }

    #[test]
    fn no_backends_references_crashloop_instead_of_repeating_advice() {
        let mut inputs = healthy_inputs();
        inputs.applications.restarts = available(vec![
            restart(NOW - 30),
            restart(NOW - 20),
            restart(NOW - 10),
        ]);
        inputs.applications.services = available(vec![ServiceObservation {
            app: "api".to_string(),
            namespace: "default".to_string(),
            desired_replicas: 2,
            healthy_backends: 0,
            total_backends: 2,
        }]);

        let report = diagnose(&inputs);
        let finding = report
            .critical
            .iter()
            .find(|item| item.id == "no-backends")
            .unwrap();

        assert!(finding.details[0].contains("see the crashloop finding"));
        assert!(!finding.suggestion.contains("health-check configuration"));
    }

    #[test]
    fn stuck_deploy_uses_active_operation_age() {
        let mut inputs = healthy_inputs();
        inputs.applications.deploys = available(vec![DeployObservation {
            operation_id: "deploy-9".to_string(),
            app: "api".to_string(),
            namespace: "default".to_string(),
            version: None,
            started_at: NOW - 16 * 60,
            phase: "waiting-for-health".to_string(),
            active: true,
        }]);

        let report = diagnose(&inputs);

        assert!(report.warnings.iter().any(|item| item.id == "deploy-stuck"));
    }

    #[test]
    fn faults_alerts_disk_throttling_certificates_and_registry_are_reported() {
        let mut inputs = healthy_inputs();
        inputs.cluster.faults = available(vec![FaultObservation {
            id: 3,
            fault_type: "drop 100%".to_string(),
            target: "api".to_string(),
            injected_by: "operator".to_string(),
            remaining_seconds: 10,
        }]);
        inputs.applications.alerts = available(vec![AlertObservation {
            app: Some("api".to_string()),
            namespace: Some("default".to_string()),
            message: "latency budget exhausted".to_string(),
        }]);
        inputs.cluster.disks = available(vec![DiskObservation {
            node_id: "node-1".to_string(),
            storage_domains: vec!["logs".to_string()],
            used_bytes: 96,
            total_bytes: 100,
            used_percent: 96.0,
        }]);
        inputs.applications.cpu_throttling = available(vec![CpuThrottleObservation {
            app: "api".to_string(),
            namespace: "default".to_string(),
            throttled_seconds_delta: 1.25,
            window_seconds: 60,
        }]);
        inputs.cluster.certificates = available(vec![CertificateObservation {
            certificate_kind: "node".to_string(),
            identity: "node-1".to_string(),
            issuer: "CN=node-ca".to_string(),
            serial: "01".to_string(),
            not_after: NOW + 2 * 24 * 60 * 60,
            rotation_state: "retrying".to_string(),
            automatic_rotation: false,
        }]);
        inputs.cluster.registry = available(vec![RegistryObservation {
            node_id: "node-1".to_string(),
            clustered: true,
            ready: true,
            peer_reachable: false,
            redundancy_possible: false,
            under_replicated_layers: 2,
        }]);

        let report = diagnose(&inputs);

        assert!(report.critical.iter().any(|item| item.id == "disk-high"));
        for id in [
            "active-faults",
            "alerts-firing",
            "cpu-throttling",
            "cert-expiring",
            "registry-redundancy",
        ] {
            assert!(report.warnings.iter().any(|item| item.id == id), "{id}");
        }
    }

    #[test]
    fn app_scope_omits_cluster_checks_and_filters_other_apps() {
        let mut inputs = healthy_inputs();
        inputs.app = Some("api".to_string());
        inputs.cluster.nodes = Evidence::Unavailable {
            reason: "not collected in app scope".to_string(),
        };
        inputs.applications.alerts = available(vec![
            AlertObservation {
                app: Some("worker".to_string()),
                namespace: Some("default".to_string()),
                message: "worker alert".to_string(),
            },
            AlertObservation {
                app: Some("api".to_string()),
                namespace: Some("default".to_string()),
                message: "api alert".to_string(),
            },
        ]);

        let report = diagnose(&inputs);

        assert!(!report.unknown.iter().any(|item| item.source == "nodes"));
        assert_eq!(
            report
                .warnings
                .iter()
                .filter(|item| item.id == "alerts-firing")
                .count(),
            1
        );
        assert!(report.warnings.iter().any(|item| item.title == "api alert"));
    }

    #[test]
    fn standalone_registry_does_not_require_peer_reachability() {
        let mut inputs = healthy_inputs();
        inputs.cluster.registry = available(vec![RegistryObservation {
            node_id: "node-1".to_string(),
            clustered: false,
            ready: true,
            peer_reachable: false,
            redundancy_possible: false,
            under_replicated_layers: 0,
        }]);

        let report = diagnose(&inputs);

        assert!(report.ok.iter().any(|ok| ok.id == "registry"));
        assert!(
            !report
                .warnings
                .iter()
                .any(|finding| finding.id == "registry-redundancy")
        );
    }

    #[test]
    fn report_json_rejects_unknown_contract_fields() {
        let report = diagnose(&healthy_inputs());
        let mut value = serde_json::to_value(report).unwrap();
        value["surprise"] = serde_json::json!(true);

        let error = serde_json::from_value::<WtfReport>(value).unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    fn restart(timestamp: u64) -> RestartObservation {
        RestartObservation {
            app: "api".to_string(),
            namespace: "default".to_string(),
            instance: Some("api-0".to_string()),
            timestamp,
            reason: "process exited".to_string(),
        }
    }
}
