//! Council member selection algorithm.
//!
//! Ranks gossip-layer nodes as candidates for Raft council promotion.
//! Pure function: takes a membership table snapshot, returns ranked
//! candidates. The caller drives `CouncilNode::add_learner()` and
//! `change_membership()`.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::meat::NodeId;
use crate::mustard::membership::MembershipTable;
use crate::mustard::state::NodeState;

// ---------------------------------------------------------------------------
// CouncilSelectionConfig
// ---------------------------------------------------------------------------

/// Configuration for the council selection algorithm.
///
/// Controls eligibility thresholds and council size bounds.
/// Separate from `CouncilConfig` (Raft timers) — different concern.
#[derive(Debug, Clone)]
pub struct CouncilSelectionConfig {
    /// Minimum time a node must be alive before council eligibility.
    pub min_node_age: Duration,
    /// Maximum CPU usage fraction (0.0–1.0) for eligibility.
    pub max_cpu_usage_fraction: f64,
    /// Maximum memory usage fraction (0.0–1.0) for eligibility.
    pub max_memory_usage_fraction: f64,
    /// Label key used for zone diversity scoring.
    pub zone_label_key: String,
    /// Minimum council size (target is clamped to this).
    pub min_council_size: usize,
    /// Maximum council size (target is clamped to this).
    pub max_council_size: usize,
}

impl Default for CouncilSelectionConfig {
    fn default() -> Self {
        Self {
            min_node_age: Duration::from_secs(600),
            max_cpu_usage_fraction: 0.90,
            max_memory_usage_fraction: 0.85,
            zone_label_key: "zone".to_string(),
            min_council_size: 3,
            max_council_size: 7,
        }
    }
}

// ---------------------------------------------------------------------------
// Selection algorithm
// ---------------------------------------------------------------------------

/// Select candidates for council membership from the membership table.
///
/// Returns an ordered list of `NodeId`s, most preferred first. Length
/// is at most `target_size - current_council.len()`.
///
/// The algorithm:
/// 1. Clamp `target_size` to `[min_council_size, max_council_size]`.
/// 2. Filter: Alive, not already on council, old enough, resources
///    reported and below thresholds.
/// 3. Sort by zone novelty (descending), then node age (oldest first),
///    then node ID (lexicographic) for a fully deterministic order.
/// 4. Take the first `needed` candidates.
///
/// Pass `now` explicitly so tests can control time.
pub fn select_council_candidates(
    membership: &MembershipTable,
    current_council: &[NodeId],
    target_size: usize,
    config: &CouncilSelectionConfig,
    now: Instant,
) -> Vec<NodeId> {
    let clamped = target_size.clamp(config.min_council_size, config.max_council_size);
    let needed = clamped.saturating_sub(current_council.len());
    if needed == 0 {
        return Vec::new();
    }

    // Zones already represented in the current council.
    let council_zones: HashSet<&str> = current_council
        .iter()
        .filter_map(|id| membership.get(id))
        .filter_map(|n| n.labels.get(&config.zone_label_key).map(|s| s.as_str()))
        .collect();

    // Filter eligible candidates.
    let mut candidates: Vec<_> = membership
        .iter()
        .filter(|n| {
            n.state == NodeState::Alive
                && !current_council.contains(&n.node_id)
                && now.duration_since(n.first_seen) >= config.min_node_age
                && n.resources.as_ref().is_some_and(|r| {
                    r.cpu_capacity_millicores > 0
                        && r.memory_capacity_mb > 0
                        && (r.cpu_used_millicores as f64 / r.cpu_capacity_millicores as f64)
                            < config.max_cpu_usage_fraction
                        && (r.memory_used_mb as f64 / r.memory_capacity_mb as f64)
                            < config.max_memory_usage_fraction
                })
        })
        .collect();

    // Sort: zone novelty desc, age desc (oldest first_seen first), node_id asc.
    candidates.sort_by(|a, b| {
        let a_novel = a
            .labels
            .get(&config.zone_label_key)
            .is_some_and(|z| !council_zones.contains(z.as_str()));
        let b_novel = b
            .labels
            .get(&config.zone_label_key)
            .is_some_and(|z| !council_zones.contains(z.as_str()));

        b_novel
            .cmp(&a_novel)
            .then_with(|| a.first_seen.cmp(&b.first_seen))
            .then_with(|| a.node_id.cmp(&b.node_id))
    });

    candidates
        .into_iter()
        .take(needed)
        .map(|n| n.node_id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use crate::mustard::membership::{MembershipTable, ResourceSummary};
    use crate::mustard::message::MembershipUpdate;
    use crate::mustard::state::NodeState;

    use super::*;

    /// Default resource summary: 50% CPU, 50% memory.
    fn healthy_resources() -> ResourceSummary {
        ResourceSummary {
            cpu_capacity_millicores: 4000,
            cpu_used_millicores: 2000,
            memory_capacity_mb: 8192,
            memory_used_mb: 4096,
            gpu_count: 0,
            gpu_used: 0,
            running_app_count: 5,
            running_job_count: 1,
        }
    }

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{}", port).parse().unwrap()
    }

    /// Add a node to the table and then customise it via get_mut.
    fn add_node(
        table: &mut MembershipTable,
        name: &str,
        port: u16,
        now: Instant,
        age: Duration,
        zone: Option<&str>,
        resources: Option<ResourceSummary>,
    ) -> NodeId {
        let id = NodeId::new(name);
        let update = MembershipUpdate {
            node_id: id.clone(),
            address: addr(port),
            state: NodeState::Alive,
            incarnation: 1,
            lamport: 1,
        };
        // Use a backdated `now` so first_seen is in the past.
        let join_time = now - age;
        table.apply_update(&update, join_time);

        if let Some(n) = table.get_mut(&id) {
            n.resources = resources;
            if let Some(z) = zone {
                n.labels.insert("zone".to_string(), z.to_string());
            }
        }
        id
    }

    fn default_config() -> CouncilSelectionConfig {
        CouncilSelectionConfig {
            min_node_age: Duration::from_secs(600),
            ..CouncilSelectionConfig::default()
        }
    }

    // -------------------------------------------------------------------
    // Filtering tests
    // -------------------------------------------------------------------

    #[test]
    fn excludes_non_alive_nodes() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        let alive_id = add_node(
            &mut table,
            "alive",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let suspect_id = add_node(
            &mut table,
            "suspect",
            2,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let dead_id = add_node(
            &mut table,
            "dead",
            3,
            now,
            old,
            None,
            Some(healthy_resources()),
        );

        // Transition suspect and dead.
        if let Some(n) = table.get_mut(&suspect_id) {
            n.state = NodeState::Suspect;
        }
        if let Some(n) = table.get_mut(&dead_id) {
            n.state = NodeState::Dead;
        }

        let result = select_council_candidates(&table, &[], 3, &default_config(), now);
        assert_eq!(result, vec![alive_id]);
    }

    #[test]
    fn excludes_nodes_below_min_age() {
        let now = Instant::now();
        let mut table = MembershipTable::new();

        let old_id = add_node(
            &mut table,
            "old",
            1,
            now,
            Duration::from_secs(700),
            None,
            Some(healthy_resources()),
        );
        let _young_id = add_node(
            &mut table,
            "young",
            2,
            now,
            Duration::from_secs(300),
            None,
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&table, &[], 3, &default_config(), now);
        assert_eq!(result, vec![old_id]);
    }

    #[test]
    fn excludes_current_council_members() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        let council_id = add_node(
            &mut table,
            "council",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let candidate_id = add_node(
            &mut table,
            "candidate",
            2,
            now,
            old,
            None,
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&table, &[council_id], 3, &default_config(), now);
        assert_eq!(result, vec![candidate_id]);
    }

    #[test]
    fn excludes_overloaded_cpu() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        let healthy_id = add_node(
            &mut table,
            "healthy",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let _overloaded_id = add_node(
            &mut table,
            "overloaded",
            2,
            now,
            old,
            None,
            Some(ResourceSummary {
                cpu_capacity_millicores: 4000,
                cpu_used_millicores: 3700, // 92.5% > 90%
                ..healthy_resources()
            }),
        );

        let result = select_council_candidates(&table, &[], 3, &default_config(), now);
        assert_eq!(result, vec![healthy_id]);
    }

    #[test]
    fn excludes_overloaded_memory() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        let healthy_id = add_node(
            &mut table,
            "healthy",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let _overloaded_id = add_node(
            &mut table,
            "overloaded",
            2,
            now,
            old,
            None,
            Some(ResourceSummary {
                memory_capacity_mb: 8192,
                memory_used_mb: 7200, // 87.9% > 85%
                ..healthy_resources()
            }),
        );

        let result = select_council_candidates(&table, &[], 3, &default_config(), now);
        assert_eq!(result, vec![healthy_id]);
    }

    #[test]
    fn excludes_nodes_without_resources() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        let with_resources = add_node(
            &mut table,
            "reported",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let _no_resources = add_node(&mut table, "unreported", 2, now, old, None, None);

        let result = select_council_candidates(&table, &[], 3, &default_config(), now);
        assert_eq!(result, vec![with_resources]);
    }

    // -------------------------------------------------------------------
    // Scoring tests
    // -------------------------------------------------------------------

    #[test]
    fn prefers_novel_zones() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        // Council member in zone-a.
        let council_id = add_node(
            &mut table,
            "council-1",
            1,
            now,
            old,
            Some("zone-a"),
            Some(healthy_resources()),
        );

        // Two candidates: one in zone-a (same), one in zone-b (novel).
        let _same_zone = add_node(
            &mut table,
            "candidate-a",
            2,
            now,
            old,
            Some("zone-a"),
            Some(healthy_resources()),
        );
        let novel_zone = add_node(
            &mut table,
            "candidate-b",
            3,
            now,
            old,
            Some("zone-b"),
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&table, &[council_id], 3, &default_config(), now);
        // Novel zone should be first.
        assert_eq!(result[0], novel_zone);
    }

    #[test]
    fn no_zone_label_treated_as_not_novel() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        // Council member in zone-a.
        let council_id = add_node(
            &mut table,
            "council-1",
            1,
            now,
            old,
            Some("zone-a"),
            Some(healthy_resources()),
        );

        // Candidate with no zone label, candidate with novel zone.
        let _no_zone = add_node(
            &mut table,
            "no-zone",
            2,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let novel = add_node(
            &mut table,
            "zone-b",
            3,
            now,
            old,
            Some("zone-b"),
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&table, &[council_id], 3, &default_config(), now);
        // Novel zone ranks above no-zone.
        assert_eq!(result[0], novel);
    }

    #[test]
    fn older_nodes_preferred_within_same_zone_novelty() {
        let now = Instant::now();
        let mut table = MembershipTable::new();

        // Both in same zone, different ages.
        let older = add_node(
            &mut table,
            "older",
            1,
            now,
            Duration::from_secs(2000),
            Some("zone-a"),
            Some(healthy_resources()),
        );
        let _younger = add_node(
            &mut table,
            "younger",
            2,
            now,
            Duration::from_secs(700),
            Some("zone-a"),
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&table, &[], 3, &default_config(), now);
        assert_eq!(result[0], older);
    }

    // -------------------------------------------------------------------
    // Determinism tests
    // -------------------------------------------------------------------

    #[test]
    fn deterministic_same_inputs_same_output() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        for i in 0..10 {
            add_node(
                &mut table,
                &format!("node-{:02}", i),
                9000 + i,
                now,
                old,
                Some(&format!("zone-{}", i % 3)),
                Some(healthy_resources()),
            );
        }

        let config = default_config();
        let r1 = select_council_candidates(&table, &[], 5, &config, now);
        let r2 = select_council_candidates(&table, &[], 5, &config, now);
        assert_eq!(r1, r2);
    }

    #[test]
    fn lexicographic_tiebreak_for_same_age_and_zone() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let age = Duration::from_secs(700);

        // Same zone, same age — node ID breaks the tie.
        let alpha = add_node(
            &mut table,
            "alpha",
            1,
            now,
            age,
            Some("zone-a"),
            Some(healthy_resources()),
        );
        let beta = add_node(
            &mut table,
            "beta",
            2,
            now,
            age,
            Some("zone-a"),
            Some(healthy_resources()),
        );
        let gamma = add_node(
            &mut table,
            "gamma",
            3,
            now,
            age,
            Some("zone-a"),
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&table, &[], 5, &default_config(), now);
        assert_eq!(result, vec![alpha, beta, gamma]);
    }

    // -------------------------------------------------------------------
    // Bounds tests
    // -------------------------------------------------------------------

    #[test]
    fn target_size_clamped_to_bounds() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        for i in 0..10 {
            add_node(
                &mut table,
                &format!("node-{}", i),
                9000 + i,
                now,
                old,
                None,
                Some(healthy_resources()),
            );
        }

        let config = default_config();

        // Target 1 should clamp to min (3).
        let r = select_council_candidates(&table, &[], 1, &config, now);
        assert_eq!(r.len(), 3);

        // Target 20 should clamp to max (7).
        let r = select_council_candidates(&table, &[], 20, &config, now);
        assert_eq!(r.len(), 7);
    }

    #[test]
    fn returns_empty_when_council_at_target_size() {
        let now = Instant::now();
        let mut table = MembershipTable::new();
        let old = Duration::from_secs(700);

        let mut council = Vec::new();
        for i in 0..5 {
            let id = add_node(
                &mut table,
                &format!("council-{}", i),
                9000 + i,
                now,
                old,
                None,
                Some(healthy_resources()),
            );
            council.push(id);
        }
        // Extra non-council node.
        add_node(
            &mut table,
            "extra",
            9100,
            now,
            old,
            None,
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&table, &council, 5, &default_config(), now);
        assert!(result.is_empty());
    }

    #[test]
    fn returns_empty_when_no_eligible_candidates() {
        let now = Instant::now();
        let mut table = MembershipTable::new();

        // All nodes too young.
        for i in 0..5 {
            add_node(
                &mut table,
                &format!("young-{}", i),
                9000 + i,
                now,
                Duration::from_secs(60),
                None,
                Some(healthy_resources()),
            );
        }

        let result = select_council_candidates(&table, &[], 3, &default_config(), now);
        assert!(result.is_empty());
    }
}
