/// Diff engine for state reconstruction.
///
/// Compares the desired state (from the Raft log) against the actual
/// runtime state (from aggregated StateReports) and produces a list
/// of corrections. This is a pure function with no I/O.
use std::collections::HashSet;

use crate::council::types::DesiredState;
use crate::meat::types::{AppId, NodeId};
use crate::reporting::aggregator::AggregatedState;

use super::types::Correction;

/// Compute the diff between desired placements and actual running state.
///
/// For each `(AppId, Vec<Placement>)` in `desired.scheduling`, checks
/// whether the app is actually running on each assigned node. Produces
/// `MissingApp` corrections for apps that should be running but aren't,
/// `ExtraApp` for apps that are running but shouldn't be, and
/// `UnknownNode` for alive nodes that didn't report.
///
/// Only checks against nodes that reported. Unknown nodes get a
/// separate `UnknownNode` correction, not `MissingApp`.
pub fn compute_diff(
    desired: &DesiredState,
    actual: &AggregatedState,
    alive_nodes: &[NodeId],
    reported_nodes: &HashSet<NodeId>,
) -> Vec<Correction> {
    let mut corrections = Vec::new();

    // Build the set of desired (app_id, node_id) placements.
    let mut desired_placements: HashSet<(AppId, NodeId)> = HashSet::new();
    for (app_id, placements) in &desired.scheduling {
        for placement in placements {
            desired_placements.insert((app_id.clone(), placement.node_id.clone()));
        }
    }

    // Build the set of actual (app_id, node_id) placements from reports.
    let mut actual_placements: HashSet<(AppId, NodeId)> = HashSet::new();
    for (node_id, report) in &actual.reports {
        for app in &report.running_apps {
            let app_id = AppId::new(&app.app_name, &app.namespace);
            actual_placements.insert((app_id, node_id.clone()));
        }
    }

    // Missing = desired − actual (only for nodes that reported).
    for (app_id, node_id) in &desired_placements {
        if reported_nodes.contains(node_id)
            && !actual_placements.contains(&(app_id.clone(), node_id.clone()))
        {
            corrections.push(Correction::MissingApp {
                app_id: app_id.clone(),
                node_id: node_id.clone(),
            });
        }
    }

    // Extra = actual − desired (for reported nodes).
    for (app_id, node_id) in &actual_placements {
        if !desired_placements.contains(&(app_id.clone(), node_id.clone())) {
            corrections.push(Correction::ExtraApp {
                app_id: app_id.clone(),
                node_id: node_id.clone(),
            });
        }
    }

    // Unknown = alive nodes that didn't report.
    let alive_set: HashSet<&NodeId> = alive_nodes.iter().collect();
    for node_id in &alive_set {
        if !reported_nodes.contains(node_id) {
            corrections.push(Correction::UnknownNode {
                node_id: (*node_id).clone(),
            });
        }
    }

    corrections
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::SystemTime;

    use super::*;
    use crate::council::types::DesiredState;
    use crate::meat::types::{Placement, Resources};
    use crate::reporting::types::{
        AppResourceUsage, ReportHealthStatus, ResourceUsage, RunningApp, StateReport,
    };

    fn node(name: &str) -> NodeId {
        NodeId::new(name)
    }

    fn app(name: &str, ns: &str) -> AppId {
        AppId::new(name, ns)
    }

    fn empty_desired() -> DesiredState {
        DesiredState::default()
    }

    fn empty_actual() -> AggregatedState {
        AggregatedState::default()
    }

    fn desired_with_placements(placements: Vec<(AppId, NodeId)>) -> DesiredState {
        let mut scheduling: HashMap<AppId, Vec<Placement>> = HashMap::new();
        for (app_id, node_id) in placements {
            scheduling.entry(app_id).or_default().push(Placement {
                node_id,
                resources: Resources::new(100, 128 * 1024 * 1024, 0),
            });
        }
        DesiredState {
            scheduling,
            ..Default::default()
        }
    }

    fn actual_with_apps(entries: Vec<(NodeId, Vec<(String, String)>)>) -> AggregatedState {
        let mut reports = HashMap::new();
        for (node_id, apps) in entries {
            let running_apps = apps
                .into_iter()
                .map(|(name, namespace)| RunningApp {
                    app_name: name,
                    namespace,
                    instance_id: 0,
                    image: String::new(),
                    port: None,
                    health_status: ReportHealthStatus::Healthy,
                    uptime: std::time::Duration::from_secs(60),
                    resource_usage: AppResourceUsage::default(),
                })
                .collect();
            reports.insert(
                node_id.clone(),
                StateReport {
                    node_id,
                    timestamp: SystemTime::now(),
                    running_apps,
                    cached_specs: vec![],
                    resource_usage: ResourceUsage::default(),
                    event_log: vec![],
                },
            );
        }
        AggregatedState {
            reports,
            stale_nodes: vec![],
        }
    }

    #[test]
    fn empty_desired_empty_actual_no_corrections() {
        let corrections = compute_diff(&empty_desired(), &empty_actual(), &[], &HashSet::new());
        assert!(corrections.is_empty());
    }

    #[test]
    fn missing_app_detected() {
        let desired = desired_with_placements(vec![(app("web", "prod"), node("n1"))]);
        let actual = actual_with_apps(vec![
            (node("n1"), vec![]), // node reported but no apps running
        ]);
        let reported: HashSet<NodeId> = [node("n1")].into();
        let corrections = compute_diff(&desired, &actual, &[node("n1")], &reported);

        assert_eq!(corrections.len(), 1);
        assert!(matches!(
            &corrections[0],
            Correction::MissingApp { app_id, node_id }
            if *app_id == app("web", "prod") && *node_id == node("n1")
        ));
    }

    #[test]
    fn extra_app_detected() {
        let desired = empty_desired();
        let actual = actual_with_apps(vec![(
            node("n1"),
            vec![("rogue".to_string(), "default".to_string())],
        )]);
        let reported: HashSet<NodeId> = [node("n1")].into();
        let corrections = compute_diff(&desired, &actual, &[node("n1")], &reported);

        assert_eq!(corrections.len(), 1);
        assert!(matches!(
            &corrections[0],
            Correction::ExtraApp { app_id, node_id }
            if *app_id == app("rogue", "default") && *node_id == node("n1")
        ));
    }

    #[test]
    fn matching_state_no_corrections() {
        let desired = desired_with_placements(vec![
            (app("web", "prod"), node("n1")),
            (app("api", "prod"), node("n2")),
        ]);
        let actual = actual_with_apps(vec![
            (node("n1"), vec![("web".to_string(), "prod".to_string())]),
            (node("n2"), vec![("api".to_string(), "prod".to_string())]),
        ]);
        let reported: HashSet<NodeId> = [node("n1"), node("n2")].into();
        let corrections = compute_diff(&desired, &actual, &[node("n1"), node("n2")], &reported);

        assert!(corrections.is_empty(), "got: {corrections:?}");
    }

    #[test]
    fn multiple_placements_per_app() {
        let desired = desired_with_placements(vec![
            (app("web", "prod"), node("n1")),
            (app("web", "prod"), node("n2")),
            (app("web", "prod"), node("n3")),
        ]);
        let actual = actual_with_apps(vec![
            (node("n1"), vec![("web".to_string(), "prod".to_string())]),
            (node("n2"), vec![("web".to_string(), "prod".to_string())]),
            (node("n3"), vec![]), // missing on n3
        ]);
        let reported: HashSet<NodeId> = [node("n1"), node("n2"), node("n3")].into();
        let corrections = compute_diff(
            &desired,
            &actual,
            &[node("n1"), node("n2"), node("n3")],
            &reported,
        );

        assert_eq!(corrections.len(), 1);
        assert!(matches!(
            &corrections[0],
            Correction::MissingApp { app_id, node_id }
            if *app_id == app("web", "prod") && *node_id == node("n3")
        ));
    }

    #[test]
    fn skips_unknown_nodes_for_missing_check() {
        // App should run on n1 but n1 didn't report.
        // We should NOT emit MissingApp for n1 — only UnknownNode.
        let desired = desired_with_placements(vec![(app("web", "prod"), node("n1"))]);
        let actual = empty_actual();
        let reported: HashSet<NodeId> = HashSet::new(); // n1 didn't report
        let corrections = compute_diff(&desired, &actual, &[node("n1")], &reported);

        // Should have UnknownNode but NOT MissingApp
        assert!(
            corrections.iter().any(
                |c| matches!(c, Correction::UnknownNode { node_id } if *node_id == node("n1"))
            )
        );
        assert!(
            !corrections
                .iter()
                .any(|c| matches!(c, Correction::MissingApp { .. }))
        );
    }

    #[test]
    fn unknown_node_emitted() {
        let corrections = compute_diff(
            &empty_desired(),
            &empty_actual(),
            &[node("n1"), node("n2")],
            &HashSet::new(),
        );
        assert_eq!(corrections.len(), 2);
        assert!(
            corrections
                .iter()
                .all(|c| matches!(c, Correction::UnknownNode { .. }))
        );
    }

    #[test]
    fn mixed_corrections() {
        let desired = desired_with_placements(vec![
            (app("web", "prod"), node("n1")),
            (app("api", "prod"), node("n2")),
        ]);
        let actual = actual_with_apps(vec![
            (
                node("n1"),
                vec![
                    ("web".to_string(), "prod".to_string()),
                    ("rogue".to_string(), "default".to_string()), // extra
                ],
            ),
            // n2 reported but api is missing
            (node("n2"), vec![]),
        ]);
        // n3 is alive but didn't report
        let reported: HashSet<NodeId> = [node("n1"), node("n2")].into();
        let alive = [node("n1"), node("n2"), node("n3")];
        let corrections = compute_diff(&desired, &actual, &alive, &reported);

        let missing: Vec<_> = corrections
            .iter()
            .filter(|c| matches!(c, Correction::MissingApp { .. }))
            .collect();
        let extra: Vec<_> = corrections
            .iter()
            .filter(|c| matches!(c, Correction::ExtraApp { .. }))
            .collect();
        let unknown: Vec<_> = corrections
            .iter()
            .filter(|c| matches!(c, Correction::UnknownNode { .. }))
            .collect();

        assert_eq!(missing.len(), 1, "api/prod missing on n2");
        assert_eq!(extra.len(), 1, "rogue/default extra on n1");
        assert_eq!(unknown.len(), 1, "n3 unknown");
    }
}
