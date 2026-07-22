/// State reconstruction controller.
///
/// Drives the learning period state machine after a new leader
/// election. Method-based design: callers invoke `on_leader_elected`,
/// `on_report_received`, and `check_timeout` at the appropriate times.
/// The controller is wired into the agent event loop in Step 9.
use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::config::node::ReconstructionSection;
use crate::council::types::DesiredState;
use crate::meat::types::NodeId;
use crate::reporting::aggregator::AggregatedState;

use super::diff::compute_diff;
use super::types::{Correction, LearningOutcome, ReconstructionPhase, ReconstructionResult};

/// Drives state reconstruction after leader election.
pub struct ReconstructionController {
    /// Current phase of the state machine.
    phase: ReconstructionPhase,
    /// Config for thresholds and timeouts.
    config: ReconstructionSection,
    /// When the learning period started.
    learning_started: Option<Instant>,
    /// IDs of nodes that have reported during this learning period.
    reported_nodes: HashSet<NodeId>,
    /// Total number of alive nodes at the start of the learning period.
    alive_count_at_start: usize,
    /// Result of the last completed reconstruction.
    last_result: Option<ReconstructionResult>,
}

impl ReconstructionController {
    /// Create a new controller in the Idle phase.
    pub fn new(config: ReconstructionSection) -> Self {
        Self {
            phase: ReconstructionPhase::Idle,
            config,
            learning_started: None,
            reported_nodes: HashSet::new(),
            alive_count_at_start: 0,
            last_result: None,
        }
    }

    /// Current phase.
    pub fn phase(&self) -> ReconstructionPhase {
        self.phase
    }

    /// Result of the last completed reconstruction.
    pub fn last_result(&self) -> Option<&ReconstructionResult> {
        self.last_result.as_ref()
    }

    /// Effective timeout for the current cluster size.
    pub fn effective_timeout(&self) -> Duration {
        if self.alive_count_at_start >= self.config.large_cluster_node_count {
            Duration::from_secs(self.config.large_cluster_timeout_secs)
        } else {
            Duration::from_secs(self.config.learning_period_timeout_secs)
        }
    }

    /// Called when this node becomes the Raft leader.
    ///
    /// Transitions from Idle to Learning and starts tracking reports.
    /// Returns immediately if zero nodes are alive (degenerate case).
    pub fn on_leader_elected(&mut self, alive_count: usize) -> Option<ReconstructionResult> {
        self.reported_nodes.clear();
        self.alive_count_at_start = alive_count;

        // Always enter Learning — even at zero alive (M16). A leader elected
        // during a gossip lull sees few or no peers; jumping straight to Active
        // resumed scheduling against an empty view and rescheduled workloads
        // still running elsewhere. Entering Learning lets `check_timeout`
        // provide a settling delay for membership to populate, and the alive
        // count is re-sampled as reports arrive so a transient lull can't let a
        // handful of reports clear the threshold against a tiny denominator.
        self.learning_started = Some(Instant::now());
        self.phase = ReconstructionPhase::Learning;
        None
    }

    /// Report keys that are current truth — reporters minus stale nodes (M15).
    ///
    /// A stale node lingers in `reports` with its last snapshot; counting it as
    /// a live reporter ends learning early and treats its (possibly dead) apps
    /// as running. Only fresh reporters count toward coverage.
    fn fresh_reporters(aggregated: &AggregatedState) -> impl Iterator<Item = &NodeId> {
        let stale: HashSet<&NodeId> = aggregated.stale_nodes.iter().collect();
        aggregated
            .reports
            .keys()
            .filter(move |node_id| !stale.contains(node_id))
    }

    /// Raise the high-water alive count so a transient gossip lull can't shrink
    /// the coverage denominator (M16).
    fn resample_alive(&mut self, alive_nodes: &[NodeId]) {
        self.alive_count_at_start = self.alive_count_at_start.max(alive_nodes.len());
    }

    /// Called when this node loses leadership.
    ///
    /// Resets everything back to Idle.
    pub fn on_leader_lost(&mut self) {
        self.phase = ReconstructionPhase::Idle;
        self.learning_started = None;
        self.reported_nodes.clear();
        self.alive_count_at_start = 0;
    }

    /// Called when new reports arrive during the learning period.
    ///
    /// Updates the set of reported nodes from the aggregated state.
    /// Returns `Some(result)` if the coverage threshold is met.
    pub fn on_report_received(
        &mut self,
        aggregated: &AggregatedState,
        desired: &DesiredState,
        alive_nodes: &[NodeId],
    ) -> Option<ReconstructionResult> {
        if self.phase != ReconstructionPhase::Learning {
            return None;
        }

        // Re-sample the alive count and count only fresh reporters (M15/M16).
        self.resample_alive(alive_nodes);
        for node_id in Self::fresh_reporters(aggregated)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.reported_nodes.insert(node_id);
        }

        // Check if coverage threshold is met.
        if self.coverage_met() {
            let result = self.finish_reconstruction(
                LearningOutcome::ThresholdMet {
                    reported: self.reported_nodes.len(),
                    total: self.alive_count_at_start,
                },
                aggregated,
                desired,
                alive_nodes,
            );
            return Some(result);
        }

        None
    }

    /// Check if the learning period has timed out.
    ///
    /// Should be called periodically. Returns `Some(result)` if the
    /// timeout has fired.
    pub fn check_timeout(
        &mut self,
        desired: &DesiredState,
        alive_nodes: &[NodeId],
        aggregated: &AggregatedState,
    ) -> Option<ReconstructionResult> {
        if self.phase != ReconstructionPhase::Learning {
            return None;
        }

        let started = self.learning_started?;
        if started.elapsed() >= self.effective_timeout() {
            // Update reported nodes one last time (fresh reporters only, M15).
            self.resample_alive(alive_nodes);
            for node_id in Self::fresh_reporters(aggregated)
                .cloned()
                .collect::<Vec<_>>()
            {
                self.reported_nodes.insert(node_id);
            }

            let result = self.finish_reconstruction(
                LearningOutcome::TimedOut {
                    reported: self.reported_nodes.len(),
                    total: self.alive_count_at_start,
                },
                aggregated,
                desired,
                alive_nodes,
            );
            return Some(result);
        }

        None
    }

    /// Whether the coverage threshold has been met.
    fn coverage_met(&self) -> bool {
        if self.alive_count_at_start == 0 {
            return true;
        }
        let coverage =
            (self.reported_nodes.len() as f64 / self.alive_count_at_start as f64) * 100.0;
        coverage >= self.config.report_threshold_percent as f64
    }

    /// Complete reconstruction: run diff, build result, transition to Active.
    fn finish_reconstruction(
        &mut self,
        outcome: LearningOutcome,
        aggregated: &AggregatedState,
        desired: &DesiredState,
        alive_nodes: &[NodeId],
    ) -> ReconstructionResult {
        self.phase = ReconstructionPhase::Reconciling;

        let corrections = compute_diff(desired, aggregated, alive_nodes, &self.reported_nodes);

        let unknown_nodes: Vec<NodeId> = corrections
            .iter()
            .filter_map(|c| match c {
                Correction::UnknownNode { node_id } => Some(node_id.clone()),
                _ => None,
            })
            .collect();

        let reported_nodes: Vec<NodeId> = self.reported_nodes.iter().cloned().collect();

        let result = ReconstructionResult {
            outcome,
            corrections,
            unknown_nodes,
            reported_nodes,
        };

        self.last_result = Some(result.clone());
        self.phase = ReconstructionPhase::Active;
        result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::SystemTime;

    use super::*;
    use crate::meat::types::{Placement, Resources};
    use crate::reporting::types::{ResourceUsage, StateReport};

    fn node(name: &str) -> NodeId {
        NodeId::new(name)
    }

    fn default_config() -> ReconstructionSection {
        ReconstructionSection::default()
    }

    fn fast_config() -> ReconstructionSection {
        ReconstructionSection {
            report_threshold_percent: 95,
            learning_period_timeout_secs: 1,
            large_cluster_timeout_secs: 2,
            large_cluster_node_count: 5,
        }
    }

    fn empty_desired() -> DesiredState {
        DesiredState::default()
    }

    fn aggregated_with_nodes(nodes: &[&str]) -> AggregatedState {
        let mut reports = HashMap::new();
        for name in nodes {
            let node_id = node(name);
            reports.insert(
                node_id.clone(),
                StateReport {
                    has_buildah: false,
                    node_id,
                    timestamp: SystemTime::now(),
                    running_apps: vec![],
                    cached_specs: vec![],
                    resource_usage: ResourceUsage::default(),
                    event_log: vec![],
                },
            );
        }
        AggregatedState {
            reports,
            stale_nodes: vec![],
            capabilities: HashMap::new(),
        }
    }

    #[test]
    fn starts_in_idle() {
        let ctrl = ReconstructionController::new(default_config());
        assert_eq!(ctrl.phase(), ReconstructionPhase::Idle);
        assert!(ctrl.last_result().is_none());
    }

    #[test]
    fn leader_elected_transitions_to_learning() {
        let mut ctrl = ReconstructionController::new(default_config());
        let result = ctrl.on_leader_elected(10);
        assert!(result.is_none());
        assert_eq!(ctrl.phase(), ReconstructionPhase::Learning);
    }

    #[test]
    fn threshold_met_transitions_to_active() {
        let mut ctrl = ReconstructionController::new(default_config());
        ctrl.on_leader_elected(2);

        let alive = [node("n1"), node("n2")];
        let aggregated = aggregated_with_nodes(&["n1", "n2"]);

        let result = ctrl
            .on_report_received(&aggregated, &empty_desired(), &alive)
            .expect("threshold should be met with 100% coverage");

        assert_eq!(ctrl.phase(), ReconstructionPhase::Active);
        assert!(matches!(
            result.outcome,
            LearningOutcome::ThresholdMet {
                reported: 2,
                total: 2
            }
        ));
    }

    #[test]
    fn timeout_triggers_with_unknown_nodes() {
        let mut ctrl = ReconstructionController::new(fast_config());
        ctrl.on_leader_elected(3);

        // Only 1 of 3 nodes reported — below 95%
        let alive = [node("n1"), node("n2"), node("n3")];
        let aggregated = aggregated_with_nodes(&["n1"]);
        ctrl.on_report_received(&aggregated, &empty_desired(), &alive);

        // Not yet timed out
        assert_eq!(ctrl.phase(), ReconstructionPhase::Learning);

        // Simulate timeout by setting learning_started to the past
        ctrl.learning_started = Some(Instant::now() - Duration::from_secs(10));

        let result = ctrl
            .check_timeout(&empty_desired(), &alive, &aggregated)
            .expect("should time out");

        assert_eq!(ctrl.phase(), ReconstructionPhase::Active);
        assert!(matches!(
            result.outcome,
            LearningOutcome::TimedOut {
                reported: 1,
                total: 3
            }
        ));
        assert_eq!(result.unknown_nodes.len(), 2);
    }

    #[test]
    fn leader_lost_resets_to_idle() {
        let mut ctrl = ReconstructionController::new(default_config());
        ctrl.on_leader_elected(5);
        assert_eq!(ctrl.phase(), ReconstructionPhase::Learning);

        ctrl.on_leader_lost();
        assert_eq!(ctrl.phase(), ReconstructionPhase::Idle);
    }

    #[test]
    fn coverage_calculation() {
        let mut ctrl = ReconstructionController::new(ReconstructionSection {
            report_threshold_percent: 50,
            ..default_config()
        });
        ctrl.on_leader_elected(4);

        // 1/4 = 25% — not enough
        let alive = [node("n1"), node("n2"), node("n3"), node("n4")];
        let agg1 = aggregated_with_nodes(&["n1"]);
        let result = ctrl.on_report_received(&agg1, &empty_desired(), &alive);
        assert!(result.is_none());

        // 2/4 = 50% — exactly enough
        let agg2 = aggregated_with_nodes(&["n1", "n2"]);
        let result = ctrl.on_report_received(&agg2, &empty_desired(), &alive);
        assert!(result.is_some());
    }

    #[test]
    fn effective_timeout_large_cluster() {
        let config = ReconstructionSection {
            learning_period_timeout_secs: 15,
            large_cluster_timeout_secs: 30,
            large_cluster_node_count: 100,
            ..default_config()
        };
        let mut ctrl = ReconstructionController::new(config);

        ctrl.on_leader_elected(50);
        assert_eq!(ctrl.effective_timeout(), Duration::from_secs(15));

        ctrl.on_leader_lost();
        ctrl.on_leader_elected(200);
        assert_eq!(ctrl.effective_timeout(), Duration::from_secs(30));
    }

    #[test]
    fn repeated_reports_same_node_counted_once() {
        let mut ctrl = ReconstructionController::new(default_config());
        ctrl.on_leader_elected(2);

        let alive = [node("n1"), node("n2")];

        // n1 reports twice
        let agg = aggregated_with_nodes(&["n1"]);
        ctrl.on_report_received(&agg, &empty_desired(), &alive);
        ctrl.on_report_received(&agg, &empty_desired(), &alive);

        // Still in Learning because only 1 of 2 nodes reported (50%)
        assert_eq!(ctrl.phase(), ReconstructionPhase::Learning);
    }

    #[test]
    fn zero_alive_nodes_enter_learning_then_settle_on_timeout() {
        // M16: a leader elected during a gossip lull (0 alive seen) no longer
        // jumps straight to Active against an empty view — it enters Learning
        // so membership has a settling window to populate. Only the learning
        // timeout completes it.
        let mut ctrl = ReconstructionController::new(default_config());
        assert!(ctrl.on_leader_elected(0).is_none());
        assert_eq!(ctrl.phase(), ReconstructionPhase::Learning);
    }

    /// M16: the alive count is a high-water mark. A leader that saw 0 alive at
    /// election but then hears from real nodes must not treat those reports as
    /// 100% coverage against a zero denominator — the denominator grows.
    #[test]
    fn alive_count_is_resampled_upward_after_a_lull() {
        let mut ctrl = ReconstructionController::new(default_config());
        ctrl.on_leader_elected(0);

        // A report arrives from n1 while alive membership now shows two nodes.
        let agg = aggregated_with_nodes(&["n1"]);
        let alive = [node("n1"), node("n2")];
        ctrl.on_report_received(&agg, &empty_desired(), &alive);

        // 1 of 2 reported = 50%, below threshold — still Learning, not a
        // trivially-met 1-of-0.
        assert_eq!(ctrl.phase(), ReconstructionPhase::Learning);
    }

    #[test]
    fn result_contains_corrections_from_diff() {
        let mut ctrl = ReconstructionController::new(default_config());
        ctrl.on_leader_elected(2);

        // Desired: web/prod on n1
        let mut desired = DesiredState::default();
        desired.scheduling.insert(
            crate::meat::types::AppId::new("web", "prod"),
            vec![Placement {
                node_id: node("n1"),
                resources: Resources::new(100, 128 * 1024 * 1024, 0),
            }],
        );

        // Actual: n1 and n2 reported, but web/prod not running on n1
        let alive = [node("n1"), node("n2")];
        let aggregated = aggregated_with_nodes(&["n1", "n2"]);

        let result = ctrl
            .on_report_received(&aggregated, &desired, &alive)
            .expect("threshold met at 100%");

        // Should have a MissingApp correction
        assert!(result.corrections.iter().any(|c| matches!(
            c,
            Correction::MissingApp { app_id, node_id }
            if app_id.name == "web" && *node_id == node("n1")
        )));
    }
}
