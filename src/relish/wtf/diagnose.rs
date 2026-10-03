use std::collections::BTreeMap;

use super::WTF_SCHEMA_VERSION;
use super::model::{
    ApplicationEvidence, BuildObservation, ClusterEvidence, CorrelatedEvent, CouncilObservation,
    DeployObservation, Evidence, LogObservation, ReplicaObservation, RestartObservation,
    WtfFinding, WtfInputs, WtfOk, WtfReport, WtfSummary, WtfUnknown,
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
        check_builds(inputs, &mut report);
        check_council(inputs, &mut report);
        check_faults(inputs, &mut report);
        check_disks(inputs, &mut report);
        check_certificates(inputs, &mut report);
        check_registry(inputs, &mut report);
    }
    record_application_unknowns(&inputs.applications, inputs.app.as_deref(), &mut report);
    check_crashloops(inputs, &mut report);
    check_services(inputs, &mut report);
    check_replicas(inputs, &mut report);
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
    // `check_builds` reports its own unknowns, so a partly read cluster
    // gets one builds row rather than an UNKNOWN and an OK.
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
    record_unknown("replicas", &evidence.replicas, &resource, report);
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

/// How many hex digits of a binary's SHA-256 a report line shows.
const SHORT_SHA256_LEN: usize = 12;

/// What makes two nodes' builds the same.
///
/// The commit names the code. The binary's SHA-256 only decides when a build
/// doesn't know its commit, because one commit built for arm64 and for x86_64
/// hashes differently, and a mixed-architecture cluster isn't skewed.
fn build_identity(build: &BuildObservation) -> (&str, Option<&str>) {
    (
        build.version.as_str(),
        build.commit.as_deref().or(build.binary_sha256.as_deref()),
    )
}

fn describe_build(build: &BuildObservation) -> String {
    let mut line = format!(
        "bun {}",
        crate::upgrade::version::describe(&build.version, build.commit.as_deref())
    );
    if let Some(sha256) = &build.binary_sha256 {
        let short = sha256.get(..SHORT_SHA256_LEN).unwrap_or(sha256);
        line.push_str(&format!(", sha256 {short}"));
    }
    line
}

/// Compare the bun builds the nodes run, and report them as one row.
///
/// When some nodes didn't answer (the evidence is degraded), the row names
/// them inside it: a uniform build becomes one UNKNOWN that says what the
/// others run, and skew stays one warning with the silent nodes in its
/// details. A missing node's build can't vouch for an OK.
fn check_builds(inputs: &WtfInputs, report: &mut WtfReport) {
    let evidence = &inputs.cluster.builds;
    let Some(builds) = evidence.value() else {
        record_unknown("builds", evidence, "cluster", report);
        return;
    };
    let unread = evidence.unknown_reason();
    let mut groups: BTreeMap<(&str, Option<&str>), Vec<&BuildObservation>> = BTreeMap::new();
    for build in builds {
        groups.entry(build_identity(build)).or_default().push(build);
    }
    let mut details: Vec<String> = builds
        .iter()
        .map(|build| format!("{}: {}", build.node_id, describe_build(build)))
        .collect();
    let Some(largest) = groups.values().map(Vec::len).max() else {
        record_unknown("builds", evidence, "cluster", report);
        return;
    };
    if groups.len() == 1 {
        let description = match builds.as_slice() {
            [only] => format!("{} runs {}", only.node_id, describe_build(only)),
            [first, ..] if unread.is_none() => {
                format!("all {} nodes run {}", builds.len(), describe_build(first))
            }
            [first, ..] => format!(
                "the {} nodes that answered run {}",
                builds.len(),
                describe_build(first)
            ),
            [] => return,
        };
        match unread {
            None => report.ok.push(WtfOk {
                id: "builds".to_string(),
                description,
            }),
            Some(reason) => report.unknown.push(WtfUnknown {
                source: "builds".to_string(),
                reason: format!("{description}; {reason}"),
                affected_resource: "cluster".to_string(),
            }),
        }
        return;
    }
    if let Some(reason) = unread {
        details.push(format!("not read: {reason}"));
    }

    let mut majorities = groups
        .iter()
        .filter(|(_, group)| group.len() == largest)
        .map(|(identity, _)| *identity);
    let title = match (majorities.next(), majorities.next()) {
        // One build clearly wins, so the rest are the odd ones out.
        (Some(majority), None) => {
            let odd: Vec<&str> = builds
                .iter()
                .filter(|build| build_identity(build) != majority)
                .map(|build| build.node_id.as_str())
                .collect();
            let verb = if odd.len() == 1 { "runs" } else { "run" };
            format!(
                "{} {verb} a different bun build from the other {largest} nodes",
                odd.join(", ")
            )
        }
        // A tie: no build is "the" build, so name them all.
        _ => format!("nodes run {} different bun builds", groups.len()),
    };
    report.warnings.push(WtfFinding {
        id: "version-skew".to_string(),
        title,
        details,
        suggestion: "bring every node to one build with `relish upgrade`, then re-run `relish wtf`"
            .to_string(),
        correlated_events: Vec::new(),
        affected_resource: "cluster".to_string(),
    });
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
    if council.member_count == 0 || inputs.cluster.council.unknown_reason().is_some() {
        if !report.unknown.iter().any(|item| item.source == "council") {
            report.unknown.push(WtfUnknown {
                source: "council".into(),
                reason: "configured council membership is not known".into(),
                affected_resource: "council".into(),
            });
        }
        return;
    }
    let mut healthy = check_council_split(council, report);
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
    } else if council.reachable_members < council.member_count {
        // Quorum holds, but a lost member leaves the membership list, so the
        // node check alone would call the smaller cluster healthy.
        healthy = false;
        let missing = council.member_count - council.reachable_members;
        report.warnings.push(WtfFinding {
            id: "council-member-down".to_string(),
            title: format!(
                "{missing} of {} council members did not answer",
                council.member_count
            ),
            details: vec![
                format!(
                    "quorum holds with {} of {}; {} more failure(s) would lose it",
                    council.reachable_members,
                    council.member_count,
                    council.reachable_members + 1 - quorum
                ),
                format!("not answering: {}", council.missing_members.join(", ")),
            ],
            suggestion: "bring the missing node back, or replace it: `relish nodes`, `relish local start <node>` on a laptop cluster".to_string(),
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

/// Compare every node's own view of the council (#424): a fenced node, two
/// serving recovery epochs, or two leaders. Returns `false` if it found any.
fn check_council_split(council: &CouncilObservation, report: &mut WtfReport) -> bool {
    if council.nodes.is_empty() {
        return true;
    }
    let summary = crate::relish::council_view::summarise(&council.nodes);
    let mut healthy = true;
    if !summary.fenced.is_empty() {
        healthy = false;
        report.critical.push(WtfFinding {
            id: "council-fenced".to_string(),
            title: format!(
                "{} node(s) fenced out of a council that `relish council recover` replaced",
                summary.fenced.len()
            ),
            details: summary
                .fenced
                .iter()
                .map(|node| {
                    format!(
                        "{}: belonged to recovery epoch {}, replaced by epoch {}; it serves no \
                         Raft and refuses writes",
                        node.node_id, node.epoch, node.fenced_by
                    )
                })
                .collect(),
            suggestion: "re-enrol each fenced node: stop it, run `relish council re-enrol \
                         --data-dir <its data directory>`, then start it so the current council \
                         admits it afresh"
                .to_string(),
            correlated_events: Vec::new(),
            affected_resource: "council".to_string(),
        });
    }
    if summary.epochs.len() > 1 {
        healthy = false;
        report.critical.push(WtfFinding {
            id: "council-epoch-split".to_string(),
            title: "nodes serve different recovery epochs: two councils".to_string(),
            details: summary
                .epochs
                .iter()
                .map(|(epoch, nodes)| format!("epoch {epoch}: {}", nodes.join(", ")))
                .collect(),
            suggestion: format!(
                "the newest epoch ({}) is the recovered council; stop the nodes on older \
                 epochs, then `relish council re-enrol --data-dir <data directory>` each one",
                summary.epoch.unwrap_or_default()
            ),
            correlated_events: Vec::new(),
            affected_resource: "council".to_string(),
        });
    }
    if !summary.lagging.is_empty() {
        healthy = false;
        report.warnings.push(WtfFinding {
            id: "council-voter-lagging".to_string(),
            title: format!(
                "{} council voter(s) answer but are not applying the log",
                summary.lagging.len()
            ),
            details: vec![crate::relish::council_view::describe_lagging(
                &summary.lagging,
            )],
            suggestion: "the voter counts towards quorum but can't commit writes; read its \
                         journal for a storage error (a full disk stops Raft), free space, \
                         then restart bun on it"
                .to_string(),
            correlated_events: Vec::new(),
            affected_resource: "council".to_string(),
        });
    }
    if summary.leaders.len() > 1 {
        healthy = false;
        report.critical.push(WtfFinding {
            id: "council-multiple-leaders".to_string(),
            title: format!("{} nodes are reported as leader", summary.leaders.len()),
            details: vec![format!("leaders: {}", summary.leaders.join(", "))],
            suggestion: "compare every node's view with `relish council status`; a lasting \
                         second leader is a split brain, so stop the side on the older epoch"
                .to_string(),
            correlated_events: Vec::new(),
            affected_resource: "council".to_string(),
        });
    }
    healthy
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
            description: with_caveat(
                "no applications have three timestamped restarts in 15 minutes",
                inputs.applications.restarts.caveat(),
            ),
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

/// An app running fewer replicas than it wants is degraded even when every
/// replica that does run is healthy, so `check_services` can't see it.
fn check_replicas(inputs: &WtfInputs, report: &mut WtfReport) {
    let Some(apps) = inputs.applications.replicas.value() else {
        return;
    };
    let mut found = false;
    for app in apps
        .iter()
        .filter(|app| app_matches(inputs.app.as_deref(), &app.app))
    {
        let running: u32 = app.running.values().sum();
        if running >= app.desired_replicas {
            continue;
        }
        found = true;
        // The scheduler isn't failing to find room: the namespace quota
        // forbids it, and only the operator can change that.
        if let Some(reason) = &app.blocked {
            report.warnings.push(WtfFinding {
                id: "quota-blocked".to_string(),
                title: format!(
                    "app {}/{} is not placed: its namespace quota has no room",
                    app.app, app.namespace
                ),
                details: vec![reason.clone()],
                suggestion: format!(
                    "raise the quota in the [namespace.{}] block and apply it, or shrink or \
                     delete other apps in {}; the app is placed on the next scheduling pass",
                    app.namespace, app.namespace
                ),
                correlated_events: Vec::new(),
                affected_resource: app_resource(&app.app, &app.namespace),
            });
            continue;
        }
        // Not a lost replica the scheduler will replace: it holds the app
        // back on purpose, because anywhere else the volume would be empty.
        if let Some(home) = &app.volume_home_away {
            report
                .critical
                .push(volume_home_away_finding(app, home, running));
            continue;
        }
        let replicas = |count: u32| {
            if count == 1 {
                "1 replica".to_string()
            } else {
                format!("{count} replicas")
            }
        };
        let mut details = Vec::new();
        for (node, placed) in &app.placed {
            let here = app.running.get(node).copied().unwrap_or(0);
            if here >= *placed {
                continue;
            }
            let missing = placed - here;
            let verb = if missing == 1 { "is" } else { "are" };
            let mut line = format!("{} placed on {node} {verb} not running", replicas(missing));
            let member = inputs
                .cluster
                .nodes
                .value()
                .map(|nodes| nodes.iter().any(|known| known.node_id == *node));
            if app.unanswered.contains(node) {
                line.push_str(&format!(" ({node} did not answer)"));
            } else if member == Some(false) {
                line.push_str(&format!(" ({node} is not a live member)"));
            }
            details.push(line);
        }
        let placed: u32 = app.placed.values().sum();
        if placed < app.desired_replicas {
            let unplaced = app.desired_replicas - placed;
            let verb = if unplaced == 1 { "has" } else { "have" };
            details.push(format!("{} {verb} no placement yet", replicas(unplaced)));
        }
        report.warnings.push(WtfFinding {
            id: "under-replicated".to_string(),
            title: format!(
                "app {}/{} runs {running} of {} replicas",
                app.app, app.namespace, app.desired_replicas
            ),
            details,
            suggestion: format!(
                "the scheduler replaces replicas on a lost node within a minute or two; \
                 if this persists, check `relish status` and `relish logs {}`, and bring a \
                 stopped node back with `relish local start <node>` on a laptop cluster",
                app.app
            ),
            correlated_events: Vec::new(),
            affected_resource: app_resource(&app.app, &app.namespace),
        });
    }
    if !found {
        report.ok.push(WtfOk {
            id: "replicas".to_string(),
            description: "every app runs its desired replicas".to_string(),
        });
    }
}

/// #423: a managed-volume app whose home node is out of the cluster waits
/// for it. That's an outage until someone brings the node back or writes
/// its data off, so it's critical, and it says which of the two to do.
fn volume_home_away_finding(app: &ReplicaObservation, home: &str, running: u32) -> WtfFinding {
    WtfFinding {
        id: "volume-home-away".to_string(),
        title: format!(
            "app {}/{} waits for {home}, which holds its volume ({running} of {} replicas running)",
            app.app, app.namespace, app.desired_replicas
        ),
        details: vec![format!(
            "{home} is out of the cluster; the scheduler won't start the app on another \
             node, where its volume would be empty"
        )],
        suggestion: format!(
            "bring {home} back (start bun, or the machine) and the app starts there with its \
             data; if {home} is gone for good, `relish decommission-node {home} \
             --workloads-stopped --reason <why>` writes its volume off and lets the app start \
             on another node with an empty volume"
        ),
        correlated_events: Vec::new(),
        affected_resource: app_resource(&app.app, &app.namespace),
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
            description: with_caveat(
                "no deploy has been active for more than 15 minutes",
                inputs.applications.deploys.caveat(),
            ),
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
        // Encoded expiry takes precedence over a potentially stale rotation label.
        if certificate.not_after <= inputs.collected_at {
            found = true;
            report.critical.push(WtfFinding {
                id: "cert-expired".to_string(),
                title: format!("certificate for {} has expired", certificate.identity),
                details: vec![format!(
                    "{} certificate from {} (serial {}) expired at {}; rotation state: {}",
                    certificate.certificate_kind,
                    certificate.issuer,
                    certificate.serial,
                    certificate.not_after,
                    certificate.rotation_state
                )],
                suggestion: format!(
                    "renew the certificate for {} and verify its consumer serves the replacement",
                    certificate.identity
                ),
                correlated_events: Vec::new(),
                affected_resource: format!("identity.{}", certificate.identity),
            });
            continue;
        }
        // Short-lived leaves are normal only with positive rotation evidence.
        if certificate.not_after > warning_at
            || (certificate.automatic_rotation && certificate.rotation_state == "valid")
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
                "renew the certificate for {} and check its CA and consumer rotation state",
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
            description:
                "all observed certificates are currently valid with healthy automatic rotation"
                    .to_string(),
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

/// Append an inherent evidence caveat to an OK description.
///
/// A caveat is not a failure, so it never becomes an unknown; it rides along
/// with the successful check it qualifies.
fn with_caveat(description: &str, caveat: Option<&str>) -> String {
    match caveat {
        Some(caveat) => format!("{description} (caveat: {caveat})"),
        None => description.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::wtf::{
        AlertObservation, CertificateObservation, CouncilObservation, CpuThrottleObservation,
        DeployObservation, DiskObservation, FaultObservation, LogObservation, NodeObservation,
        RegistryObservation, ReplicaObservation, RestartObservation, ServiceObservation,
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
                builds: available(vec![build(
                    "node-1",
                    "v0.1.1",
                    Some(COMMIT_A),
                    Some("aaaa"),
                )]),
                council: available(CouncilObservation {
                    enabled: true,
                    member_count: 1,
                    reachable_members: 1,
                    leader: Some("node-1".to_string()),
                    missing_members: Vec::new(),
                    nodes: Vec::new(),
                }),
                faults: available(Vec::new()),
                disks: available(vec![DiskObservation {
                    filesystem_id: None,
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
                replicas: available(vec![ReplicaObservation {
                    app: "api".to_string(),
                    namespace: "default".to_string(),
                    desired_replicas: 1,
                    placed: BTreeMap::from([("node-1".to_string(), 1)]),
                    running: BTreeMap::from([("node-1".to_string(), 1)]),
                    unanswered: Vec::new(),
                    blocked: None,
                    volume_home_away: None,
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
    fn expired_certificates_are_critical_even_with_a_stale_valid_rotation_label() {
        for automatic_rotation in [false, true] {
            let mut inputs = healthy_inputs();
            inputs.cluster.certificates = available(vec![CertificateObservation {
                certificate_kind: "workload".into(),
                identity: "api".into(),
                issuer: "workload-ca".into(),
                serial: "01".into(),
                not_after: NOW,
                rotation_state: "valid".into(),
                automatic_rotation,
            }]);
            let report = diagnose(&inputs);
            assert!(
                report
                    .critical
                    .iter()
                    .any(|finding| finding.id == "cert-expired")
            );
            assert!(!report.ok.iter().any(|finding| finding.id == "certificates"));
        }
    }

    #[test]
    fn healthy_short_lived_certificates_do_not_claim_fourteen_days_of_validity() {
        let mut inputs = healthy_inputs();
        inputs.cluster.certificates = available(vec![CertificateObservation {
            certificate_kind: "workload".into(),
            identity: "api".into(),
            issuer: "workload-ca".into(),
            serial: "01".into(),
            not_after: NOW + 90,
            rotation_state: "valid".into(),
            automatic_rotation: true,
        }]);
        let report = diagnose(&inputs);
        assert!(report.critical.is_empty());
        assert!(report.warnings.is_empty());
        let evidence = report
            .ok
            .iter()
            .find(|finding| finding.id == "certificates")
            .unwrap();
        assert!(!evidence.description.contains("14 days"));
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
            missing_members: vec!["node-2".to_string(), "node-3".to_string()],
            nodes: Vec::new(),
        });

        let report = diagnose(&inputs);

        assert!(report.critical.iter().any(|item| item.id == "no-leader"));
        assert!(report.critical.iter().any(|item| item.id == "quorum-loss"));
        assert!(!report.ok.iter().any(|ok| ok.id == "council"));
    }

    /// Z6.7: after `relish local stop node-3` the stopped node left the
    /// membership list, and wtf reported "all 2 nodes alive" and a healthy
    /// council, all OK.
    #[test]
    fn a_missing_council_member_is_a_warning_while_quorum_holds() {
        let mut inputs = healthy_inputs();
        inputs.cluster.council = available(CouncilObservation {
            enabled: true,
            member_count: 3,
            reachable_members: 2,
            leader: Some("node-1".to_string()),
            missing_members: vec!["node-3".to_string()],
            nodes: Vec::new(),
        });

        let report = diagnose(&inputs);

        let finding = report
            .warnings
            .iter()
            .find(|item| item.id == "council-member-down")
            .unwrap();
        assert_eq!(finding.title, "1 of 3 council members did not answer");
        assert!(finding.details[0].contains("1 more failure(s)"));
        assert_eq!(finding.details[1], "not answering: node-3");
        assert!(report.critical.is_empty());
        assert!(!report.ok.iter().any(|ok| ok.id == "council"));
    }

    fn council_node(
        name: &str,
        role: crate::bun::agent::CouncilRole,
        epoch: u64,
        fenced_by: Option<u64>,
        leader: Option<&str>,
    ) -> crate::relish::council_view::CouncilNodeObservation {
        use crate::relish::council_view::{CouncilMemberObservation, CouncilNodeObservation};
        CouncilNodeObservation {
            node_id: name.to_string(),
            error: None,
            role,
            recovery_epoch: Some(epoch),
            fenced_by,
            leader: leader.map(str::to_string),
            term: 3,
            last_applied: Some(10),
            last_log_index: Some(10),
            members: vec![CouncilMemberObservation {
                name: "node-1".to_string(),
                voter: true,
            }],
        }
    }

    fn council_with_nodes(
        nodes: Vec<crate::relish::council_view::CouncilNodeObservation>,
    ) -> Evidence<CouncilObservation> {
        available(CouncilObservation {
            enabled: true,
            member_count: 1,
            reachable_members: 1,
            leader: Some("node-1".to_string()),
            missing_members: Vec::new(),
            nodes,
        })
    }

    /// #424, after the fence: the recovered council is healthy, but the old
    /// voters must not hide. Each is named, with the way back.
    #[test]
    fn a_fenced_node_is_critical_and_names_the_way_back() {
        use crate::bun::agent::CouncilRole;
        let mut inputs = healthy_inputs();
        inputs.cluster.council = council_with_nodes(vec![
            council_node("node-1", CouncilRole::Leader, 1, None, Some("node-1")),
            council_node("node-2", CouncilRole::Fenced, 0, Some(1), None),
            council_node("node-3", CouncilRole::Fenced, 0, Some(1), None),
        ]);

        let report = diagnose(&inputs);

        let finding = report
            .critical
            .iter()
            .find(|item| item.id == "council-fenced")
            .unwrap();
        assert!(finding.title.starts_with("2 node(s) fenced"));
        assert!(finding.details[0].starts_with("node-2: belonged to recovery epoch 0"));
        assert!(
            finding
                .suggestion
                .contains("relish council re-enrol --data-dir")
        );
        // The fence working is not a second council.
        assert!(
            !report
                .critical
                .iter()
                .any(|item| item.id == "council-epoch-split")
        );
        assert!(!report.ok.iter().any(|ok| ok.id == "council"));
    }

    /// #480: a voter whose Raft core stopped still answers its API, so the
    /// member count alone called the council healthy.
    #[test]
    fn a_voter_that_stopped_applying_is_a_warning() {
        use crate::bun::agent::CouncilRole;
        let mut inputs = healthy_inputs();
        let mut stuck = council_node("node-3", CouncilRole::Follower, 0, None, Some("node-1"));
        stuck.last_applied = Some(2827);
        let mut leader = council_node("node-1", CouncilRole::Leader, 0, None, Some("node-1"));
        leader.last_applied = Some(3976);
        leader
            .members
            .push(crate::relish::council_view::CouncilMemberObservation {
                name: "node-3".to_string(),
                voter: true,
            });
        inputs.cluster.council = council_with_nodes(vec![leader, stuck]);

        let report = diagnose(&inputs);

        let finding = report
            .warnings
            .iter()
            .find(|item| item.id == "council-voter-lagging")
            .unwrap();
        assert_eq!(finding.details, vec!["node-3 (applied 2827, 1149 behind)"]);
        assert!(!report.ok.iter().any(|ok| ok.id == "council"));
    }

    /// #424, before the fence: both halves serve and both report healthy.
    #[test]
    fn two_serving_epochs_and_two_leaders_are_critical() {
        use crate::bun::agent::CouncilRole;
        let mut inputs = healthy_inputs();
        inputs.cluster.council = council_with_nodes(vec![
            council_node("node-1", CouncilRole::Leader, 1, None, Some("node-1")),
            council_node("node-2", CouncilRole::Leader, 0, None, Some("node-2")),
            council_node("node-3", CouncilRole::Follower, 0, None, Some("node-2")),
        ]);

        let report = diagnose(&inputs);

        let split = report
            .critical
            .iter()
            .find(|item| item.id == "council-epoch-split")
            .unwrap();
        assert_eq!(
            split.details,
            vec!["epoch 0: node-2, node-3", "epoch 1: node-1"]
        );
        let leaders = report
            .critical
            .iter()
            .find(|item| item.id == "council-multiple-leaders")
            .unwrap();
        assert_eq!(leaders.details, vec!["leaders: node-1, node-2"]);
        assert!(!report.ok.iter().any(|ok| ok.id == "council"));
    }

    #[test]
    fn one_council_on_one_epoch_stays_ok() {
        use crate::bun::agent::CouncilRole;
        let mut inputs = healthy_inputs();
        inputs.cluster.council = council_with_nodes(vec![council_node(
            "node-1",
            CouncilRole::Leader,
            1,
            None,
            Some("node-1"),
        )]);

        let report = diagnose(&inputs);

        assert!(report.critical.is_empty());
        assert!(report.ok.iter().any(|ok| ok.id == "council"));
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
            filesystem_id: None,
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

    #[test]
    fn inherent_collector_limitations_are_caveats_not_unknowns() {
        let mut inputs = healthy_inputs();
        // The restart and deploy collectors only ever observe bounded,
        // process-local history. That is an inherent limitation, not a
        // collection failure, so it must not force a non-zero exit.
        inputs.applications.restarts =
            Evidence::available_with_caveat(NOW, Vec::new(), "restart history is bounded");
        inputs.applications.deploys =
            Evidence::available_with_caveat(NOW, Vec::new(), "deploy history is bounded");

        let report = diagnose(&inputs);

        // A healthy cluster whose only "Degraded" came from these collectors now
        // diagnoses clean: nothing critical, no warnings, and crucially no
        // unknowns, so `relish wtf` would exit 0.
        assert!(report.critical.is_empty());
        assert!(report.warnings.is_empty());
        assert!(report.unknown.is_empty());
        // The caveat still surfaces, attached to the passing check.
        let crashloops = report.ok.iter().find(|ok| ok.id == "crashloops").unwrap();
        assert!(
            crashloops
                .description
                .contains("restart history is bounded")
        );
        let deploys = report.ok.iter().find(|ok| ok.id == "deploys").unwrap();
        assert!(deploys.description.contains("deploy history is bounded"));
    }

    #[test]
    fn genuine_collection_errors_remain_unknown() {
        let mut inputs = healthy_inputs();
        // A real collection failure (not an inherent caveat) must still block a
        // clean verdict and become an unknown.
        inputs.applications.restarts = Evidence::Unavailable {
            reason: "every node event ring was unreachable".to_string(),
        };

        let report = diagnose(&inputs);

        assert!(report.unknown.iter().any(|item| item.source == "restarts"));
    }

    /// Z6.7: with one of three frontends gone, `wtf` said every service had a
    /// healthy backend. Under-replication is its own finding, and it names
    /// the nodes whose placements aren't running.
    #[test]
    fn an_app_running_fewer_replicas_than_desired_is_flagged_with_its_placements() {
        let mut inputs = healthy_inputs();
        inputs.applications.replicas = available(vec![ReplicaObservation {
            app: "frontend".to_string(),
            namespace: "default".to_string(),
            desired_replicas: 3,
            placed: BTreeMap::from([
                ("node-1".to_string(), 1),
                ("node-2".to_string(), 1),
                ("node-3".to_string(), 1),
            ]),
            running: BTreeMap::from([("node-1".to_string(), 1), ("node-2".to_string(), 1)]),
            unanswered: vec!["node-3".to_string()],
            blocked: None,
            volume_home_away: None,
        }]);

        let report = diagnose(&inputs);

        let finding = report
            .warnings
            .iter()
            .find(|finding| finding.id == "under-replicated")
            .expect("under-replication is a warning");
        assert_eq!(finding.title, "app frontend/default runs 2 of 3 replicas");
        assert_eq!(finding.affected_resource, "app.frontend/default");
        assert_eq!(
            finding.details,
            ["1 replica placed on node-3 is not running (node-3 did not answer)"]
        );
        assert!(!report.ok.iter().any(|ok| ok.id == "replicas"));

        // Placed nowhere yet: the scheduler has no room for them.
        let mut inputs = healthy_inputs();
        inputs.applications.replicas = available(vec![ReplicaObservation {
            app: "frontend".to_string(),
            namespace: "default".to_string(),
            desired_replicas: 3,
            placed: BTreeMap::from([("node-1".to_string(), 2)]),
            running: BTreeMap::from([("node-1".to_string(), 1)]),
            unanswered: Vec::new(),
            blocked: None,
            volume_home_away: None,
        }]);
        let report = diagnose(&inputs);
        let finding = report
            .warnings
            .iter()
            .find(|finding| finding.id == "under-replicated")
            .unwrap();
        assert_eq!(
            finding.details,
            [
                "1 replica placed on node-1 is not running",
                "1 replica has no placement yet"
            ]
        );

        // A node that has left the membership isn't asked at all.
        let mut inputs = healthy_inputs();
        inputs.applications.replicas = available(vec![ReplicaObservation {
            app: "frontend".to_string(),
            namespace: "default".to_string(),
            desired_replicas: 2,
            placed: BTreeMap::from([("node-1".to_string(), 1), ("node-3".to_string(), 1)]),
            running: BTreeMap::from([("node-1".to_string(), 1)]),
            unanswered: Vec::new(),
            blocked: None,
            volume_home_away: None,
        }]);
        let report = diagnose(&inputs);
        let finding = report
            .warnings
            .iter()
            .find(|finding| finding.id == "under-replicated")
            .unwrap();
        assert_eq!(
            finding.details,
            ["1 replica placed on node-3 is not running (node-3 is not a live member)"]
        );
    }

    /// #326: an app its namespace quota keeps unplaced is its own finding,
    /// naming the quota, not a vague under-replication.
    #[test]
    fn an_over_quota_app_is_flagged_with_the_quota_that_blocks_it() {
        let mut inputs = healthy_inputs();
        let reason = "namespace \"prod\" would exceed CPU quota: 0+1600 > 1000m";
        inputs.applications.replicas = available(vec![ReplicaObservation {
            app: "greedy".to_string(),
            namespace: "prod".to_string(),
            desired_replicas: 2,
            placed: BTreeMap::new(),
            running: BTreeMap::new(),
            unanswered: Vec::new(),
            blocked: Some(reason.to_string()),
            volume_home_away: None,
        }]);

        let report = diagnose(&inputs);

        let finding = report
            .warnings
            .iter()
            .find(|finding| finding.id == "quota-blocked")
            .expect("a quota-blocked app is a warning");
        assert_eq!(
            finding.title,
            "app greedy/prod is not placed: its namespace quota has no room"
        );
        assert_eq!(finding.details, [reason]);
        assert_eq!(finding.affected_resource, "app.greedy/prod");
        assert!(finding.suggestion.contains("[namespace.prod]"));
        assert!(
            !report
                .warnings
                .iter()
                .any(|finding| finding.id == "under-replicated"),
            "the quota finding replaces the generic one"
        );
        assert!(!report.ok.iter().any(|ok| ok.id == "replicas"));
    }

    /// #423: a volume app waiting for its home node is an outage, so it is
    /// critical, and it names the node and both ways out.
    #[test]
    fn a_volume_app_waiting_for_its_home_node_is_critical() {
        let mut inputs = healthy_inputs();
        inputs.applications.replicas = available(vec![ReplicaObservation {
            app: "db".to_string(),
            namespace: "prod".to_string(),
            desired_replicas: 1,
            placed: BTreeMap::from([("node-2".to_string(), 1)]),
            running: BTreeMap::new(),
            unanswered: Vec::new(),
            blocked: None,
            volume_home_away: Some("node-2".to_string()),
        }]);

        let report = diagnose(&inputs);

        let finding = report
            .critical
            .iter()
            .find(|finding| finding.id == "volume-home-away")
            .expect("a volume app waiting for its home is critical");
        assert_eq!(
            finding.title,
            "app db/prod waits for node-2, which holds its volume (0 of 1 replicas running)"
        );
        assert_eq!(finding.affected_resource, "app.db/prod");
        assert!(
            finding
                .suggestion
                .contains("relish decommission-node node-2"),
            "{}",
            finding.suggestion
        );
        assert!(
            !report
                .warnings
                .iter()
                .any(|finding| finding.id == "under-replicated"),
            "the volume finding replaces the generic one"
        );
        assert!(!report.ok.iter().any(|ok| ok.id == "replicas"));

        // Once it runs again, there's nothing to report.
        let mut inputs = healthy_inputs();
        inputs.applications.replicas = available(vec![ReplicaObservation {
            app: "db".to_string(),
            namespace: "prod".to_string(),
            desired_replicas: 1,
            placed: BTreeMap::from([("node-2".to_string(), 1)]),
            running: BTreeMap::from([("node-2".to_string(), 1)]),
            unanswered: Vec::new(),
            blocked: None,
            volume_home_away: None,
        }]);
        let report = diagnose(&inputs);
        assert!(report.critical.is_empty(), "{:?}", report.critical);
    }

    #[test]
    fn apps_at_or_above_their_desired_replicas_are_ok() {
        let mut inputs = healthy_inputs();
        // A rolling deploy briefly runs one more than desired.
        inputs.applications.replicas = available(vec![ReplicaObservation {
            app: "api".to_string(),
            namespace: "default".to_string(),
            desired_replicas: 1,
            placed: BTreeMap::from([("node-1".to_string(), 1)]),
            running: BTreeMap::from([("node-1".to_string(), 2)]),
            unanswered: Vec::new(),
            blocked: None,
            volume_home_away: None,
        }]);
        let report = diagnose(&inputs);
        assert!(report.warnings.is_empty());
        assert!(report.ok.iter().any(|ok| ok.id == "replicas"));
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

    const COMMIT_A: &str = "3fcb1fd0000000000000000000000000000000aa";
    const COMMIT_B: &str = "9e1d2c30000000000000000000000000000000bb";

    fn build(
        node_id: &str,
        version: &str,
        commit: Option<&str>,
        binary_sha256: Option<&str>,
    ) -> BuildObservation {
        BuildObservation {
            node_id: node_id.to_string(),
            version: version.to_string(),
            commit: commit.map(str::to_string),
            binary_sha256: binary_sha256.map(str::to_string),
        }
    }

    fn with_builds(builds: Vec<BuildObservation>) -> WtfReport {
        let mut inputs = healthy_inputs();
        inputs.cluster.builds = available(builds);
        diagnose(&inputs)
    }

    #[test]
    fn a_uniform_cluster_reports_its_one_build_as_ok() {
        let report = with_builds(vec![
            build("node-1", "v0.1.1", Some(COMMIT_A), Some("aaaa1111bbbb2222")),
            build("node-2", "v0.1.1", Some(COMMIT_A), Some("aaaa1111bbbb2222")),
            build("node-3", "v0.1.1", Some(COMMIT_A), Some("aaaa1111bbbb2222")),
        ]);

        assert!(report.warnings.iter().all(|w| w.id != "version-skew"));
        let ok = report.ok.iter().find(|ok| ok.id == "builds").unwrap();
        assert_eq!(
            ok.description,
            "all 3 nodes run bun v0.1.1 (3fcb1fd), sha256 aaaa1111bbbb"
        );
    }

    #[test]
    fn a_node_on_an_older_build_is_named_in_a_skew_warning() {
        let report = with_builds(vec![
            build("node-1", "v0.1.1", Some(COMMIT_A), Some("aaaa")),
            build("node-2", "v0.1.1", Some(COMMIT_A), Some("aaaa")),
            build("node-3", "v0.1.1", Some(COMMIT_B), Some("bbbb")),
        ]);

        let skew = report
            .warnings
            .iter()
            .find(|w| w.id == "version-skew")
            .expect("skew warning");
        assert_eq!(
            skew.title,
            "node-3 runs a different bun build from the other 2 nodes"
        );
        assert_eq!(
            skew.details,
            [
                "node-1: bun v0.1.1 (3fcb1fd), sha256 aaaa",
                "node-2: bun v0.1.1 (3fcb1fd), sha256 aaaa",
                "node-3: bun v0.1.1 (9e1d2c3), sha256 bbbb",
            ]
        );
        assert!(report.ok.iter().all(|ok| ok.id != "builds"));
    }

    #[test]
    fn a_different_version_is_skew_even_without_commits() {
        let report = with_builds(vec![
            build("node-1", "v0.1.1", None, None),
            build("node-2", "v0.1.0", None, None),
            build("node-3", "v0.1.1", None, None),
        ]);

        let skew = report.warnings.iter().find(|w| w.id == "version-skew");
        assert_eq!(
            skew.unwrap().title,
            "node-2 runs a different bun build from the other 2 nodes"
        );
    }

    #[test]
    fn one_commit_built_for_two_architectures_is_not_skew() {
        let report = with_builds(vec![
            build("node-1", "v0.1.1", Some(COMMIT_A), Some("arm64-hash")),
            build("node-2", "v0.1.1", Some(COMMIT_A), Some("x86_64-hash")),
        ]);

        assert!(report.warnings.iter().all(|w| w.id != "version-skew"));
        assert!(report.ok.iter().any(|ok| ok.id == "builds"));
    }

    #[test]
    fn builds_without_a_commit_are_told_apart_by_their_hash() {
        let report = with_builds(vec![
            build("node-1", "v0.1.1", None, Some("aaaa")),
            build("node-2", "v0.1.1", None, Some("bbbb")),
        ]);

        let skew = report.warnings.iter().find(|w| w.id == "version-skew");
        assert_eq!(skew.unwrap().title, "nodes run 2 different bun builds");
    }

    /// Every row of a report, one line each, headed by its section.
    fn report_rows(report: &WtfReport) -> String {
        let mut rows = Vec::new();
        for finding in &report.critical {
            rows.push(format!("CRITICAL [{}] {}", finding.id, finding.title));
        }
        for finding in &report.warnings {
            rows.push(format!("WARNING [{}] {}", finding.id, finding.title));
            for detail in &finding.details {
                rows.push(format!("    {detail}"));
            }
        }
        for unknown in &report.unknown {
            rows.push(format!("UNKNOWN [{}] {}", unknown.source, unknown.reason));
        }
        for ok in &report.ok {
            rows.push(format!("OK [{}] {}", ok.id, ok.description));
        }
        rows.join("\n")
    }

    /// Three nodes, `node-3` dead, so its build couldn't be read.
    fn inputs_with_a_dead_node(builds: Vec<BuildObservation>) -> WtfInputs {
        let mut inputs = healthy_inputs();
        let node = |id: &str, state: &str, reachable| NodeObservation {
            node_id: id.to_string(),
            membership_state: state.to_string(),
            agent_reachable: reachable,
        };
        inputs.cluster.nodes = available(vec![
            node("node-1", "Alive", true),
            node("node-2", "Alive", true),
            node("node-3", "Dead", false),
        ]);
        inputs.cluster.builds = Evidence::Degraded {
            observed_at: NOW,
            value: builds,
            reason: "node node-3: version: timed out after 10s".to_string(),
        };
        inputs
    }

    #[test]
    fn a_dead_node_leaves_one_builds_row_that_names_it() {
        let report = diagnose(&inputs_with_a_dead_node(vec![
            build("node-1", "v0.1.2", Some(COMMIT_A), Some("aaaa1111bbbb2222")),
            build("node-2", "v0.1.2", Some(COMMIT_A), Some("aaaa1111bbbb2222")),
        ]));
        insta::assert_snapshot!(report_rows(&report));
    }

    #[test]
    fn skew_beside_a_dead_node_is_one_warning_that_names_the_dead_node() {
        let report = diagnose(&inputs_with_a_dead_node(vec![
            build("node-1", "v0.1.2", Some(COMMIT_A), Some("aaaa")),
            build("node-2", "v0.1.2", Some(COMMIT_B), Some("bbbb")),
        ]));
        insta::assert_snapshot!(report_rows(&report));
    }

    #[test]
    fn unreadable_builds_are_unknown_not_ok() {
        let mut inputs = healthy_inputs();
        inputs.cluster.builds = Evidence::Unavailable {
            reason: "node node-1: version: timed out".to_string(),
        };
        let report = diagnose(&inputs);

        assert!(report.unknown.iter().any(|u| u.source == "builds"));
        assert!(report.ok.iter().all(|ok| ok.id != "builds"));
    }
}
