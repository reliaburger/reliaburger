//! Council member selection algorithm.
//!
//! Ranks gossip-layer nodes as candidates for Raft council promotion.
//! Pure function: takes a membership table snapshot, returns ranked
//! candidates. The caller drives `CouncilNode::add_learner()` and
//! `change_membership()`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::meat::NodeId;
use crate::mustard::membership::MembershipSnapshot;
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
    /// How long a voter or learner must be continuously Dead (or missing
    /// from gossip) before the self-healing planner evicts it. Hysteresis:
    /// a flapping node must not churn membership.
    pub dead_window: Duration,
    /// How long a candidate must be continuously Alive before the planner
    /// adds or promotes it. The other half of the hysteresis.
    pub candidate_alive_window: Duration,
    /// Maximum number of log entries a learner may trail the leader by and
    /// still count as caught up for promotion.
    pub max_promotion_lag: u64,
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
            dead_window: Duration::from_secs(30),
            candidate_alive_window: Duration::from_secs(5),
            max_promotion_lag: 64,
        }
    }
}

// ---------------------------------------------------------------------------
// Selection algorithm
// ---------------------------------------------------------------------------

/// Select candidates for council membership from a gossip membership snapshot.
///
/// Returns an ordered list of `NodeId`s, most preferred first. Length
/// is at most `target_size - current_council.len()`.
///
/// The algorithm:
/// 1. Clamp `target_size` to `[min_council_size, max_council_size]`.
/// 2. Filter: Alive, not already on council, old enough, and — if resources are
///    reported — below the CPU/memory thresholds. A member with no reported
///    resources is treated as eligible (remote resources aren't gossiped yet).
/// 3. Sort by zone novelty (descending), then node age (oldest first),
///    then node ID (lexicographic) for a fully deterministic order.
/// 4. Take the first `needed` candidates.
///
/// Pass `now` explicitly so tests can control time.
pub fn select_council_candidates(
    members: &[MembershipSnapshot],
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
        .filter_map(|id| members.iter().find(|m| &m.node_id == id))
        .filter_map(|m| m.labels.get(&config.zone_label_key).map(|s| s.as_str()))
        .collect();

    // Filter eligible candidates.
    let mut candidates: Vec<&MembershipSnapshot> = members
        .iter()
        .filter(|m| {
            m.state == NodeState::Alive
                && !current_council.contains(&m.node_id)
                && now.duration_since(m.first_seen) >= config.min_node_age
                // Resources are optional: reported means enforce thresholds,
                // unreported means eligible (remote resources aren't gossiped).
                && m.resources.as_ref().is_none_or(|r| {
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
        .map(|m| m.node_id.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// Council self-healing: health tracking and the replacement planner
// ---------------------------------------------------------------------------

/// Gossip-derived health of a council member or candidate, carrying the
/// timestamp the hysteresis windows are measured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberHealth {
    /// Continuously alive since the given instant.
    Alive { since: Instant },
    /// Failed a probe and awaiting refutation: neither promotable nor
    /// evictable until SWIM makes up its mind.
    Suspect,
    /// Continuously dead, departed or missing from gossip since the given
    /// instant.
    Dead { since: Instant },
}

/// One gossip member as seen this tick: its SWIM state and when this node
/// first saw it. `first_seen` seeds the alive window for members that
/// predate the tracker, so a warm cluster does not wait out the window on
/// every leader change.
#[derive(Debug, Clone, Copy)]
pub struct ObservedMember {
    /// The member's SWIM state in the latest snapshot.
    pub state: NodeState,
    /// When the local node first saw the member.
    pub first_seen: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthKind {
    Alive,
    Suspect,
    Dead,
}

#[derive(Debug, Clone, Copy)]
struct TrackedHealth {
    kind: HealthKind,
    since: Instant,
}

/// Tracks the continuous health of Raft ids across reconciler ticks.
///
/// Gossip snapshots only say what a node's state *is*; the hysteresis
/// windows need to know how long it has *stayed* there. Feed the tracker one
/// snapshot per tick via [`HealthTracker::observe`] and it hands back a
/// [`MemberHealth`] per tracked id, with `since` reset on every observed
/// state transition — so a flapping node keeps restarting its clock.
#[derive(Debug, Default)]
pub struct HealthTracker {
    entries: HashMap<u64, TrackedHealth>,
}

impl HealthTracker {
    /// Fold one gossip snapshot into the tracker and return the health of
    /// every id in `tracked`. Ids absent from `present` are treated as dead
    /// (SWIM has already reaped them); ids no longer tracked are forgotten.
    pub fn observe(
        &mut self,
        present: &HashMap<u64, ObservedMember>,
        tracked: &BTreeSet<u64>,
        now: Instant,
    ) -> HashMap<u64, MemberHealth> {
        self.entries.retain(|id, _| tracked.contains(id));

        let mut health = HashMap::with_capacity(tracked.len());
        for id in tracked {
            let member = present.get(id);
            let kind = match member.map(|m| m.state) {
                Some(NodeState::Alive) => HealthKind::Alive,
                Some(NodeState::Suspect) => HealthKind::Suspect,
                // Dead, Left, or reaped from the table entirely.
                Some(NodeState::Dead) | Some(NodeState::Left) | None => HealthKind::Dead,
            };

            let entry = self
                .entries
                .entry(*id)
                .and_modify(|e| {
                    if e.kind != kind {
                        // An observed transition restarts the clock — this
                        // is what makes a flapping node keep waiting.
                        *e = TrackedHealth { kind, since: now };
                    }
                })
                .or_insert_with(|| {
                    // First sight. An already-alive member inherits its
                    // gossip age so a warm cluster is immediately eligible.
                    let since = match (kind, member) {
                        (HealthKind::Alive, Some(m)) => m.first_seen.min(now),
                        _ => now,
                    };
                    TrackedHealth { kind, since }
                });

            let observed = match entry.kind {
                HealthKind::Alive => MemberHealth::Alive { since: entry.since },
                HealthKind::Suspect => MemberHealth::Suspect,
                HealthKind::Dead => MemberHealth::Dead { since: entry.since },
            };
            health.insert(*id, observed);
        }
        health
    }
}

/// A single membership action for the council reconciler to execute this
/// tick. The planner returns at most one: membership changes are serialised
/// so a failure leaves the council in a state the next tick can re-plan
/// from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CouncilAction {
    /// Add the node as a non-voting learner so it starts replicating.
    AddLearner(u64),
    /// Promote a caught-up learner to voter via joint consensus.
    Promote(u64),
    /// Remove a dead voter from the voter set and drop it from the
    /// membership entirely.
    RemoveVoter(u64),
    /// Drop a dead learner from the membership.
    RemoveLearner(u64),
    /// Nothing to do this tick.
    Nothing,
}

/// What the reconciler observed this tick: the Raft membership, gossip
/// health and replication progress the planner decides from. Borrowed
/// fields (the `'a` lifetime) keep the planner allocation-free; the caller
/// owns the collections.
#[derive(Debug)]
pub struct CouncilObservation<'a> {
    /// Current voter ids.
    pub voters: &'a BTreeSet<u64>,
    /// Current learner ids (in the membership but not voting).
    pub learners: &'a BTreeSet<u64>,
    /// This node — the leader, since only the leader plans. Never planned
    /// for removal and always treated as alive.
    pub leader_id: u64,
    /// Health per Raft id, covering voters, learners and candidates.
    pub health: &'a HashMap<u64, MemberHealth>,
    /// Spare candidates ranked best-first by `select_council_candidates`.
    pub candidates: &'a [u64],
    /// The leader's last log index, for catch-up gating.
    pub leader_last_log_index: Option<u64>,
    /// Replicated (matched) log index per follower/learner, from the
    /// leader's Raft metrics.
    pub replication: &'a BTreeMap<u64, Option<u64>>,
    /// True while a joint-consensus membership change is still committing.
    pub change_in_flight: bool,
    /// Voter ids that have reported sustained disk pressure and want to
    /// resign (12b.2 T3). The planner treats a pressured *follower* voter as
    /// replaceable-but-not-yet-evictable: it grows a healthy replacement in
    /// first (add-before-remove) and only evicts the pressured seat once the
    /// replacement is a voter and quorum can spare it. A pressured *leader*
    /// deposes itself via `trigger().elect()` before it ever appears here as
    /// an ordinary voter, so the planner never has to remove a leader.
    pub disk_pressured: &'a BTreeSet<u64>,
}

/// Plan the single next membership action for the council.
///
/// Pure function: same observation, same answer. Invariants the caller can
/// rely on (and the proptest enforces):
///
/// - never proposes removing the leader;
/// - never proposes a change that live voters could not commit, or that
///   would drop the voter set below `min_council_size`;
/// - prefers add-before-remove: a dead voter's replacement is brought in as
///   a learner, caught up and promoted before the dead seat is evicted, so
///   quorum never depends on the dead node's vote returning;
/// - applies hysteresis: eviction requires `dead_window` of continuous
///   death, candidacy requires `candidate_alive_window` of continuous life.
///
/// When the leader itself must leave (disk pressure, T3), a leadership
/// transfer action will hang off this seam; openraft 0.9 has no graceful
/// transfer (see `upgrade::orchestrator`), so it is deliberately absent
/// here rather than stubbed.
pub fn plan_council_action(
    observation: &CouncilObservation<'_>,
    config: &CouncilSelectionConfig,
    now: Instant,
) -> CouncilAction {
    if observation.change_in_flight {
        return CouncilAction::Nothing;
    }

    let leader_id = observation.leader_id;
    let alive = |id: u64| {
        id == leader_id
            || matches!(
                observation.health.get(&id),
                Some(MemberHealth::Alive { .. })
            )
    };
    let stable_alive = |id: u64| {
        id == leader_id
            || matches!(
                observation.health.get(&id),
                Some(MemberHealth::Alive { since })
                    if now.duration_since(*since) >= config.candidate_alive_window
            )
    };
    let dead_past_window = |id: u64| {
        id != leader_id
            && matches!(
                observation.health.get(&id),
                Some(MemberHealth::Dead { since })
                    if now.duration_since(*since) >= config.dead_window
            )
    };
    let majority = |n: usize| n / 2 + 1;

    let voters = observation.voters;
    let alive_voters = voters.iter().filter(|id| alive(**id)).count();
    if alive_voters < majority(voters.len()) {
        // Quorum is already lost: no membership change can commit, so
        // planning one would only wedge the reconciler on timeouts.
        // Full-council-loss disaster recovery (T3) hangs off this seam.
        return CouncilAction::Nothing;
    }

    // A non-leader voter under sustained disk pressure wants to resign. It
    // still holds its seat and still votes, so it's treated as a soft-dead
    // voter: replace it add-before-remove, evict it last, and never below
    // quorum. An empty `disk_pressured` set leaves every rule below unchanged.
    let pressured = |id: u64| id != leader_id && observation.disk_pressured.contains(&id);

    let dead_voters: Vec<u64> = voters
        .iter()
        .copied()
        .filter(|id| dead_past_window(*id))
        .collect();
    // A voter that is merely flapping (dead inside the window) still holds
    // its seat: only confirmed-dead seats count as open.
    let seated = voters.len() - dead_voters.len();
    // A pressured voter that is not already counted as dead also frees a seat
    // for growth, so a healthy replacement is brought in before it resigns.
    let pressured_alive_voters = voters
        .iter()
        .filter(|id| pressured(**id) && !dead_past_window(**id))
        .count();
    let effective_seated = seated.saturating_sub(pressured_alive_voters);

    // 1. Promote the best caught-up, stably-alive learner while a healthy
    //    seat is free (growth below the cap, a dead voter, or a resigning
    //    pressured voter to replace).
    if effective_seated < config.max_council_size {
        for id in observation.candidates {
            if observation.learners.contains(id)
                && stable_alive(*id)
                && learner_caught_up(observation, *id, config)
            {
                return CouncilAction::Promote(*id);
            }
        }
    }

    // 2. Evict a confirmed-dead voter once the council can spare the seat:
    //    never below the minimum size, and only while live voters form a
    //    majority of both the old and the shrunken config.
    if let Some(id) = dead_voters.first() {
        let shrunk = voters.len() - 1;
        if shrunk >= config.min_council_size && alive_voters >= majority(shrunk) {
            return CouncilAction::RemoveVoter(*id);
        }
    }

    // 2b. Evict a resigning pressured voter, but only once a healthy
    //     replacement has landed so the council still has enough seats. It is
    //     evicted last (after any dead voter above) and only while quorum and
    //     the minimum size survive the shrink — hysteresis against churn is
    //     the sustained-pressure window the caller applies before flagging it.
    if let Some(id) = voters.iter().copied().find(|id| pressured(*id)) {
        let shrunk = voters.len() - 1;
        // "Healthy replacement present" == the surviving voters (minus the
        // pressured one) are still at least the minimum council size, so the
        // council doesn't shrink below its floor by letting it go.
        if shrunk >= config.min_council_size && alive_voters.saturating_sub(1) >= majority(shrunk) {
            return CouncilAction::RemoveVoter(id);
        }
    }

    // 3. Bring a replacement in as a learner (add-before-remove) when open
    //    seats are not already covered by a live learner catching up. A
    //    resigning pressured voter's seat counts as open here too.
    let live_learners = observation.learners.iter().filter(|id| alive(**id)).count();
    let open_seats = config
        .max_council_size
        .saturating_sub(effective_seated + live_learners);
    if open_seats > 0 {
        for id in observation.candidates {
            if !observation.learners.contains(id) && !voters.contains(id) && stable_alive(*id) {
                return CouncilAction::AddLearner(*id);
            }
        }
    }

    // 4. Drop a learner that has been dead past the window.
    for id in observation.learners {
        if dead_past_window(*id) {
            return CouncilAction::RemoveLearner(*id);
        }
    }

    CouncilAction::Nothing
}

/// Is the learner's replicated log within `max_promotion_lag` entries of
/// the leader's last log index? A learner the leader has no replication
/// metrics for counts as caught up only when the log is empty.
fn learner_caught_up(
    observation: &CouncilObservation<'_>,
    id: u64,
    config: &CouncilSelectionConfig,
) -> bool {
    let last = observation.leader_last_log_index.unwrap_or(0);
    match observation.replication.get(&id).copied().flatten() {
        Some(matched) => last.saturating_sub(matched) <= config.max_promotion_lag,
        None => last == 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::SocketAddr;

    use crate::mustard::membership::{MembershipSnapshot, ResourceSummary};
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

    /// Push an Alive member snapshot with a backdated `first_seen`.
    fn add_node(
        members: &mut Vec<MembershipSnapshot>,
        name: &str,
        port: u16,
        now: Instant,
        age: Duration,
        zone: Option<&str>,
        resources: Option<ResourceSummary>,
    ) -> NodeId {
        let id = NodeId::new(name);
        let mut labels = BTreeMap::new();
        if let Some(z) = zone {
            labels.insert("zone".to_string(), z.to_string());
        }
        members.push(MembershipSnapshot {
            node_id: id.clone(),
            address: addr(port),
            state: NodeState::Alive,
            incarnation: 1,
            is_council: false,
            is_leader: false,
            labels,
            first_seen: now - age,
            resources,
        });
        id
    }

    /// Set a member's state in the snapshot list.
    fn set_state(members: &mut [MembershipSnapshot], id: &NodeId, state: NodeState) {
        if let Some(m) = members.iter_mut().find(|m| &m.node_id == id) {
            m.state = state;
        }
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
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        let alive_id = add_node(
            &mut members,
            "alive",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let suspect_id = add_node(
            &mut members,
            "suspect",
            2,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let dead_id = add_node(
            &mut members,
            "dead",
            3,
            now,
            old,
            None,
            Some(healthy_resources()),
        );

        // Transition suspect and dead.
        set_state(&mut members, &suspect_id, NodeState::Suspect);
        set_state(&mut members, &dead_id, NodeState::Dead);

        let result = select_council_candidates(&members, &[], 3, &default_config(), now);
        assert_eq!(result, vec![alive_id]);
    }

    #[test]
    fn excludes_nodes_below_min_age() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();

        let old_id = add_node(
            &mut members,
            "old",
            1,
            now,
            Duration::from_secs(700),
            None,
            Some(healthy_resources()),
        );
        let _young_id = add_node(
            &mut members,
            "young",
            2,
            now,
            Duration::from_secs(300),
            None,
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&members, &[], 3, &default_config(), now);
        assert_eq!(result, vec![old_id]);
    }

    #[test]
    fn excludes_current_council_members() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        let council_id = add_node(
            &mut members,
            "council",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let candidate_id = add_node(
            &mut members,
            "candidate",
            2,
            now,
            old,
            None,
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&members, &[council_id], 3, &default_config(), now);
        assert_eq!(result, vec![candidate_id]);
    }

    #[test]
    fn excludes_overloaded_cpu() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        let healthy_id = add_node(
            &mut members,
            "healthy",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let _overloaded_id = add_node(
            &mut members,
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

        let result = select_council_candidates(&members, &[], 3, &default_config(), now);
        assert_eq!(result, vec![healthy_id]);
    }

    #[test]
    fn excludes_overloaded_memory() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        let healthy_id = add_node(
            &mut members,
            "healthy",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let _overloaded_id = add_node(
            &mut members,
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

        let result = select_council_candidates(&members, &[], 3, &default_config(), now);
        assert_eq!(result, vec![healthy_id]);
    }

    #[test]
    fn includes_nodes_without_resources() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        let with_resources = add_node(
            &mut members,
            "reported",
            1,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        // Remote resources aren't gossiped, so unreported nodes must remain
        // eligible — otherwise no peer would ever be selected.
        let no_resources = add_node(&mut members, "unreported", 2, now, old, None, None);

        let result = select_council_candidates(&members, &[], 3, &default_config(), now);
        assert!(result.contains(&with_resources));
        assert!(result.contains(&no_resources));
    }

    // -------------------------------------------------------------------
    // Scoring tests
    // -------------------------------------------------------------------

    #[test]
    fn prefers_novel_zones() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        // Council member in zone-a.
        let council_id = add_node(
            &mut members,
            "council-1",
            1,
            now,
            old,
            Some("zone-a"),
            Some(healthy_resources()),
        );

        // Two candidates: one in zone-a (same), one in zone-b (novel).
        let _same_zone = add_node(
            &mut members,
            "candidate-a",
            2,
            now,
            old,
            Some("zone-a"),
            Some(healthy_resources()),
        );
        let novel_zone = add_node(
            &mut members,
            "candidate-b",
            3,
            now,
            old,
            Some("zone-b"),
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&members, &[council_id], 3, &default_config(), now);
        // Novel zone should be first.
        assert_eq!(result[0], novel_zone);
    }

    #[test]
    fn no_zone_label_treated_as_not_novel() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        // Council member in zone-a.
        let council_id = add_node(
            &mut members,
            "council-1",
            1,
            now,
            old,
            Some("zone-a"),
            Some(healthy_resources()),
        );

        // Candidate with no zone label, candidate with novel zone.
        let _no_zone = add_node(
            &mut members,
            "no-zone",
            2,
            now,
            old,
            None,
            Some(healthy_resources()),
        );
        let novel = add_node(
            &mut members,
            "zone-b",
            3,
            now,
            old,
            Some("zone-b"),
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&members, &[council_id], 3, &default_config(), now);
        // Novel zone ranks above no-zone.
        assert_eq!(result[0], novel);
    }

    #[test]
    fn older_nodes_preferred_within_same_zone_novelty() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();

        // Both in same zone, different ages.
        let older = add_node(
            &mut members,
            "older",
            1,
            now,
            Duration::from_secs(2000),
            Some("zone-a"),
            Some(healthy_resources()),
        );
        let _younger = add_node(
            &mut members,
            "younger",
            2,
            now,
            Duration::from_secs(700),
            Some("zone-a"),
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&members, &[], 3, &default_config(), now);
        assert_eq!(result[0], older);
    }

    // -------------------------------------------------------------------
    // Determinism tests
    // -------------------------------------------------------------------

    #[test]
    fn deterministic_same_inputs_same_output() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        for i in 0..10 {
            add_node(
                &mut members,
                &format!("node-{:02}", i),
                9000 + i,
                now,
                old,
                Some(&format!("zone-{}", i % 3)),
                Some(healthy_resources()),
            );
        }

        let config = default_config();
        let r1 = select_council_candidates(&members, &[], 5, &config, now);
        let r2 = select_council_candidates(&members, &[], 5, &config, now);
        assert_eq!(r1, r2);
    }

    #[test]
    fn lexicographic_tiebreak_for_same_age_and_zone() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let age = Duration::from_secs(700);

        // Same zone, same age — node ID breaks the tie.
        let alpha = add_node(
            &mut members,
            "alpha",
            1,
            now,
            age,
            Some("zone-a"),
            Some(healthy_resources()),
        );
        let beta = add_node(
            &mut members,
            "beta",
            2,
            now,
            age,
            Some("zone-a"),
            Some(healthy_resources()),
        );
        let gamma = add_node(
            &mut members,
            "gamma",
            3,
            now,
            age,
            Some("zone-a"),
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&members, &[], 5, &default_config(), now);
        assert_eq!(result, vec![alpha, beta, gamma]);
    }

    // -------------------------------------------------------------------
    // Bounds tests
    // -------------------------------------------------------------------

    #[test]
    fn target_size_clamped_to_bounds() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        for i in 0..10 {
            add_node(
                &mut members,
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
        let r = select_council_candidates(&members, &[], 1, &config, now);
        assert_eq!(r.len(), 3);

        // Target 20 should clamp to max (7).
        let r = select_council_candidates(&members, &[], 20, &config, now);
        assert_eq!(r.len(), 7);
    }

    #[test]
    fn returns_empty_when_council_at_target_size() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();
        let old = Duration::from_secs(700);

        let mut council = Vec::new();
        for i in 0..5 {
            let id = add_node(
                &mut members,
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
            &mut members,
            "extra",
            9100,
            now,
            old,
            None,
            Some(healthy_resources()),
        );

        let result = select_council_candidates(&members, &council, 5, &default_config(), now);
        assert!(result.is_empty());
    }

    #[test]
    fn returns_empty_when_no_eligible_candidates() {
        let now = Instant::now();
        let mut members: Vec<MembershipSnapshot> = Vec::new();

        // All nodes too young.
        for i in 0..5 {
            add_node(
                &mut members,
                &format!("young-{}", i),
                9000 + i,
                now,
                Duration::from_secs(60),
                None,
                Some(healthy_resources()),
            );
        }

        let result = select_council_candidates(&members, &[], 3, &default_config(), now);
        assert!(result.is_empty());
    }

    // -------------------------------------------------------------------
    // Self-healing planner tests
    // -------------------------------------------------------------------

    /// A small planner config with second-scale windows the tests can
    /// step over by backdating `since` timestamps.
    fn heal_config() -> CouncilSelectionConfig {
        CouncilSelectionConfig {
            min_council_size: 3,
            max_council_size: 3,
            dead_window: Duration::from_secs(30),
            candidate_alive_window: Duration::from_secs(5),
            max_promotion_lag: 8,
            ..CouncilSelectionConfig::default()
        }
    }

    fn alive_for(now: Instant, secs: u64) -> MemberHealth {
        MemberHealth::Alive {
            since: now - Duration::from_secs(secs),
        }
    }

    fn dead_for(now: Instant, secs: u64) -> MemberHealth {
        MemberHealth::Dead {
            since: now - Duration::from_secs(secs),
        }
    }

    /// Inputs for a planner call, with owned collections so tests can build
    /// an observation without lifetime gymnastics.
    struct PlannerFixture {
        voters: BTreeSet<u64>,
        learners: BTreeSet<u64>,
        leader_id: u64,
        health: HashMap<u64, MemberHealth>,
        candidates: Vec<u64>,
        leader_last_log_index: Option<u64>,
        replication: BTreeMap<u64, Option<u64>>,
        change_in_flight: bool,
        disk_pressured: BTreeSet<u64>,
    }

    impl PlannerFixture {
        fn plan(&self, config: &CouncilSelectionConfig, now: Instant) -> CouncilAction {
            plan_council_action(
                &CouncilObservation {
                    voters: &self.voters,
                    learners: &self.learners,
                    leader_id: self.leader_id,
                    health: &self.health,
                    candidates: &self.candidates,
                    leader_last_log_index: self.leader_last_log_index,
                    replication: &self.replication,
                    change_in_flight: self.change_in_flight,
                    disk_pressured: &self.disk_pressured,
                },
                config,
                now,
            )
        }
    }

    /// Three healthy voters (1 = leader), no learners, spares 4 and 5.
    fn healthy_three(now: Instant) -> PlannerFixture {
        PlannerFixture {
            voters: BTreeSet::from([1, 2, 3]),
            learners: BTreeSet::new(),
            leader_id: 1,
            health: HashMap::from([
                (2, alive_for(now, 600)),
                (3, alive_for(now, 600)),
                (4, alive_for(now, 600)),
                (5, alive_for(now, 600)),
            ]),
            candidates: vec![4, 5],
            leader_last_log_index: Some(100),
            replication: BTreeMap::from([(2, Some(100)), (3, Some(100))]),
            change_in_flight: false,
            disk_pressured: BTreeSet::new(),
        }
    }

    #[test]
    fn planner_does_nothing_when_council_healthy_and_at_target() {
        let now = Instant::now();
        let fixture = healthy_three(now);
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_adds_learner_to_replace_dead_voter() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        // Candidate 4 ranks first: it becomes the replacement learner.
        assert_eq!(
            fixture.plan(&heal_config(), now),
            CouncilAction::AddLearner(4)
        );
    }

    #[test]
    fn planner_waits_out_dead_window_before_acting() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // Dead for 10s of a 30s window: still hysteresis territory.
        fixture.health.insert(3, dead_for(now, 10));
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_waits_out_candidate_alive_window() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        // Both spares only just came alive: neither is stable yet.
        fixture.health.insert(4, alive_for(now, 1));
        fixture.health.insert(5, alive_for(now, 2));
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_promotes_caught_up_learner() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        fixture.learners.insert(4);
        fixture.replication.insert(4, Some(100));
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Promote(4));
    }

    #[test]
    fn planner_holds_promotion_for_lagging_learner() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        fixture.learners.insert(4);
        // 60 entries behind with a lag bound of 8: not caught up. No other
        // learner is needed (4 is alive), so the planner holds.
        fixture.replication.insert(4, Some(40));
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_promotes_learner_within_lag_bound() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        fixture.learners.insert(4);
        fixture.replication.insert(4, Some(95));
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Promote(4));
    }

    #[test]
    fn planner_treats_unreported_replication_as_behind() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        fixture.learners.insert(4);
        // No replication entry for the learner at all: not caught up.
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_removes_dead_voter_after_replacement_promoted() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // Voter 3 died, spare 4 was already promoted: four voters, three
        // alive. Eviction keeps the set at the minimum size.
        fixture.voters.insert(4);
        fixture.health.insert(3, dead_for(now, 60));
        fixture.replication.insert(4, Some(100));
        fixture.candidates = vec![5];
        assert_eq!(
            fixture.plan(&heal_config(), now),
            CouncilAction::RemoveVoter(3)
        );
    }

    #[test]
    fn planner_holds_removal_below_min_council_size() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        // No spares at all: removal would shrink 3 -> 2, below the minimum.
        fixture.candidates = Vec::new();
        fixture.health.remove(&4);
        fixture.health.remove(&5);
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_never_removes_the_leader() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // Gossip claims the leader itself is dead (a stale or lying view).
        // The planner must treat the node it runs on as alive.
        fixture.health.insert(1, dead_for(now, 600));
        let action = fixture.plan(&heal_config(), now);
        assert_ne!(action, CouncilAction::RemoveVoter(1));
        assert_ne!(action, CouncilAction::RemoveLearner(1));
        assert_eq!(action, CouncilAction::Nothing);
    }

    #[test]
    fn planner_does_nothing_while_change_in_flight() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        fixture.change_in_flight = true;
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_holds_when_quorum_already_lost() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // Two of three voters dead: no change can commit. T3 disaster
        // recovery owns this state; the planner must not thrash.
        fixture.health.insert(2, dead_for(now, 60));
        fixture.health.insert(3, dead_for(now, 60));
        fixture.learners.insert(4);
        fixture.replication.insert(4, Some(100));
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_adds_second_learner_when_first_dies_mid_catch_up() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        fixture.health.insert(3, dead_for(now, 60));
        // Spare 4 was added as a learner, then died before catching up.
        fixture.learners.insert(4);
        fixture.health.insert(4, dead_for(now, 60));
        fixture.candidates = vec![5];
        assert_eq!(
            fixture.plan(&heal_config(), now),
            CouncilAction::AddLearner(5)
        );
    }

    #[test]
    fn planner_evicts_dead_learner_once_council_is_whole() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // Council healthy at target; a leftover learner died.
        fixture.learners.insert(4);
        fixture.health.insert(4, dead_for(now, 60));
        fixture.candidates = vec![5];
        assert_eq!(
            fixture.plan(&heal_config(), now),
            CouncilAction::RemoveLearner(4)
        );
    }

    #[test]
    fn planner_ignores_briefly_dead_voter_and_adds_no_learner() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // A flapping voter: dead for 2s of a 30s window. Its seat is still
        // counted, so no replacement learner gets added either.
        fixture.health.insert(3, dead_for(now, 2));
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_grows_council_below_target_one_action_per_tick() {
        let now = Instant::now();
        // Fresh single-node cluster: leader only, two stable spares.
        let fixture = PlannerFixture {
            voters: BTreeSet::from([1]),
            learners: BTreeSet::new(),
            leader_id: 1,
            health: HashMap::from([(4, alive_for(now, 600)), (5, alive_for(now, 600))]),
            candidates: vec![4, 5],
            leader_last_log_index: Some(10),
            replication: BTreeMap::new(),
            change_in_flight: false,
            disk_pressured: BTreeSet::new(),
        };
        assert_eq!(
            fixture.plan(&heal_config(), now),
            CouncilAction::AddLearner(4)
        );
    }

    // -------------------------------------------------------------------
    // Disk-pressure resignation tests (12b.2 T3)
    // -------------------------------------------------------------------

    #[test]
    fn planner_grows_replacement_for_a_disk_pressured_voter() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // Voter 3 is a healthy follower under sustained disk pressure. It's
        // still alive, so the planner brings in a replacement first rather
        // than evicting it outright.
        fixture.disk_pressured.insert(3);
        assert_eq!(
            fixture.plan(&heal_config(), now),
            CouncilAction::AddLearner(4)
        );
    }

    #[test]
    fn planner_evicts_a_pressured_voter_once_replacement_seated() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // A four-voter council with one pressured seat: the replacement (4) is
        // already a voter, so the pressured voter (3) can resign without
        // dropping below the minimum size.
        fixture.voters.insert(4);
        fixture.health.insert(4, alive_for(now, 600));
        fixture.replication.insert(4, Some(100));
        fixture.disk_pressured.insert(3);
        fixture.candidates = vec![5];
        assert_eq!(
            fixture.plan(&heal_config(), now),
            CouncilAction::RemoveVoter(3)
        );
    }

    #[test]
    fn planner_holds_pressured_eviction_below_min_size() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // Three voters, one pressured, but no spare to grow into: evicting
        // would shrink 3 -> 2, below the minimum. The planner must wait.
        fixture.disk_pressured.insert(3);
        fixture.candidates = Vec::new();
        fixture.health.remove(&4);
        fixture.health.remove(&5);
        assert_eq!(fixture.plan(&heal_config(), now), CouncilAction::Nothing);
    }

    #[test]
    fn planner_never_resigns_the_leader_for_disk_pressure() {
        let now = Instant::now();
        let mut fixture = healthy_three(now);
        // Even if the leader itself is flagged (it shouldn't be — a pressured
        // leader deposes itself first), the planner never removes it.
        fixture.disk_pressured.insert(1);
        let action = fixture.plan(&heal_config(), now);
        assert_ne!(action, CouncilAction::RemoveVoter(1));
    }

    // -------------------------------------------------------------------
    // Health tracker tests
    // -------------------------------------------------------------------

    fn observed(state: NodeState, first_seen: Instant) -> ObservedMember {
        ObservedMember { state, first_seen }
    }

    #[test]
    fn tracker_seeds_alive_since_from_first_seen() {
        let now = Instant::now();
        let mut tracker = HealthTracker::default();
        let first_seen = now - Duration::from_secs(120);
        let present = HashMap::from([(7, observed(NodeState::Alive, first_seen))]);
        let health = tracker.observe(&present, &BTreeSet::from([7]), now);
        // A member that predates the tracker inherits its gossip age, so a
        // warm cluster does not wait out the window after a leader change.
        assert_eq!(
            health.get(&7),
            Some(&MemberHealth::Alive { since: first_seen })
        );
    }

    #[test]
    fn tracker_marks_missing_member_dead_since_first_absence() {
        let t0 = Instant::now();
        let mut tracker = HealthTracker::default();
        let present = HashMap::from([(7, observed(NodeState::Alive, t0))]);
        tracker.observe(&present, &BTreeSet::from([7]), t0);

        // The member vanishes from gossip (SWIM reaped it).
        let t1 = t0 + Duration::from_secs(5);
        let health = tracker.observe(&HashMap::new(), &BTreeSet::from([7]), t1);
        assert_eq!(health.get(&7), Some(&MemberHealth::Dead { since: t1 }));

        // Still missing later: the dead clock keeps its original start.
        let t2 = t0 + Duration::from_secs(20);
        let health = tracker.observe(&HashMap::new(), &BTreeSet::from([7]), t2);
        assert_eq!(health.get(&7), Some(&MemberHealth::Dead { since: t1 }));
    }

    #[test]
    fn tracker_resets_clocks_when_member_flaps() {
        let t0 = Instant::now();
        let mut tracker = HealthTracker::default();
        let present = HashMap::from([(7, observed(NodeState::Alive, t0))]);
        tracker.observe(&present, &BTreeSet::from([7]), t0);

        // Dies, then returns. The alive clock must restart at the return,
        // not inherit the pre-flap age — that is the anti-churn hysteresis.
        let t1 = t0 + Duration::from_secs(5);
        tracker.observe(&HashMap::new(), &BTreeSet::from([7]), t1);
        let t2 = t0 + Duration::from_secs(10);
        let health = tracker.observe(&present, &BTreeSet::from([7]), t2);
        assert_eq!(health.get(&7), Some(&MemberHealth::Alive { since: t2 }));
    }

    #[test]
    fn tracker_treats_suspect_as_neither_alive_nor_dead() {
        let t0 = Instant::now();
        let mut tracker = HealthTracker::default();
        let present = HashMap::from([(7, observed(NodeState::Suspect, t0))]);
        let health = tracker.observe(&present, &BTreeSet::from([7]), t0);
        assert_eq!(health.get(&7), Some(&MemberHealth::Suspect));

        // Refuted back to alive: the alive clock starts now, not at
        // first_seen — we watched it transition.
        let t1 = t0 + Duration::from_secs(3);
        let present = HashMap::from([(7, observed(NodeState::Alive, t0))]);
        let health = tracker.observe(&present, &BTreeSet::from([7]), t1);
        assert_eq!(health.get(&7), Some(&MemberHealth::Alive { since: t1 }));
    }

    #[test]
    fn tracker_forgets_untracked_ids() {
        let t0 = Instant::now();
        let mut tracker = HealthTracker::default();
        let present = HashMap::from([(7, observed(NodeState::Alive, t0))]);
        tracker.observe(&present, &BTreeSet::from([7]), t0);

        // Drop 7 from the tracked set, then track it again: its history is
        // gone, so it re-seeds instead of resuming a stale clock.
        let t1 = t0 + Duration::from_secs(5);
        tracker.observe(&HashMap::new(), &BTreeSet::new(), t1);
        let t2 = t0 + Duration::from_secs(10);
        let health = tracker.observe(&present, &BTreeSet::from([7]), t2);
        assert_eq!(health.get(&7), Some(&MemberHealth::Alive { since: t0 }));
    }

    // -------------------------------------------------------------------
    // Planner property tests
    // -------------------------------------------------------------------

    mod planner_properties {
        use proptest::prelude::*;

        use super::*;

        /// Health as a generatable value: (kind, seconds in that state).
        fn health_from_seed(seed: (u8, u64), now: Instant) -> MemberHealth {
            match seed.0 % 3 {
                0 => MemberHealth::Alive {
                    since: now - Duration::from_secs(seed.1),
                },
                1 => MemberHealth::Suspect,
                _ => MemberHealth::Dead {
                    since: now - Duration::from_secs(seed.1),
                },
            }
        }

        fn majority(n: usize) -> usize {
            n / 2 + 1
        }

        proptest! {
            /// For arbitrary observations the planner never removes the
            /// leader, never plans a change live voters cannot commit or
            /// that drops below the minimum size, only evicts nodes that
            /// are dead past the window, only promotes caught-up stable
            /// learners, and is deterministic (at most one action, and the
            /// same one every time).
            #[test]
            fn planner_is_safe_for_arbitrary_observations(
                voter_count in 1usize..7,
                learner_count in 0usize..4,
                extra_count in 0usize..4,
                health_seeds in proptest::collection::vec((0u8..3, 0u64..120), 16),
                matched_seeds in proptest::collection::vec(proptest::option::of(0u64..1000), 16),
                leader_last in proptest::option::of(0u64..1000),
                change_in_flight in proptest::bool::ANY,
                pressure_seeds in proptest::collection::vec(proptest::bool::ANY, 16),
            ) {
                let now = Instant::now();
                let config = heal_config();

                let voters: BTreeSet<u64> = (1..=voter_count as u64).collect();
                let leader_id = 1u64;
                let learners: BTreeSet<u64> =
                    (voter_count as u64 + 1..=(voter_count + learner_count) as u64).collect();
                let candidates: Vec<u64> = (voter_count as u64 + 1
                    ..=(voter_count + learner_count + extra_count) as u64)
                    .collect();

                let mut health = HashMap::new();
                let mut replication = BTreeMap::new();
                let mut disk_pressured = BTreeSet::new();
                for (i, id) in voters.iter().chain(learners.iter()).chain(candidates.iter()).enumerate() {
                    health.insert(*id, health_from_seed(health_seeds[i % health_seeds.len()], now));
                    replication.insert(*id, matched_seeds[i % matched_seeds.len()]);
                    if pressure_seeds[i % pressure_seeds.len()] {
                        disk_pressured.insert(*id);
                    }
                }

                let observation = CouncilObservation {
                    voters: &voters,
                    learners: &learners,
                    leader_id,
                    health: &health,
                    candidates: &candidates,
                    leader_last_log_index: leader_last,
                    replication: &replication,
                    change_in_flight,
                    disk_pressured: &disk_pressured,
                };

                let action = plan_council_action(&observation, &config, now);

                // Deterministic: replanning the same observation gives the
                // same single action.
                prop_assert_eq!(action, plan_council_action(&observation, &config, now));

                if change_in_flight {
                    prop_assert_eq!(action, CouncilAction::Nothing);
                }

                let alive = |id: u64| {
                    id == leader_id
                        || matches!(health.get(&id), Some(MemberHealth::Alive { .. }))
                };
                let alive_voters = voters.iter().filter(|id| alive(**id)).count();

                let dead_past_window = |id: u64| {
                    matches!(
                        health.get(&id),
                        Some(MemberHealth::Dead { since })
                            if now.duration_since(*since) >= config.dead_window
                    )
                };
                let stable_alive = |id: u64| {
                    matches!(
                        health.get(&id),
                        Some(MemberHealth::Alive { since })
                            if now.duration_since(*since) >= config.candidate_alive_window
                    )
                };

                match action {
                    CouncilAction::RemoveVoter(id) => {
                        prop_assert_ne!(id, leader_id, "planner removed the leader");
                        prop_assert!(voters.contains(&id));
                        // Minimum-size safety holds for every eviction.
                        prop_assert!(voters.len() > config.min_council_size);
                        prop_assert!(alive_voters >= majority(voters.len()));

                        if dead_past_window(id) {
                            // A dead voter is not counted in `alive_voters`, so
                            // the survivors already form the shrunken majority.
                            prop_assert!(alive_voters >= majority(voters.len() - 1));
                        } else {
                            // The only other reason to evict a live voter is a
                            // disk-pressure resignation (12b.2 T3). It *is*
                            // counted in `alive_voters`, so removing it drops
                            // the count by one — quorum must survive that.
                            prop_assert!(
                                disk_pressured.contains(&id),
                                "evicted a voter that was neither dead nor pressured"
                            );
                            prop_assert!(
                                alive_voters.saturating_sub(1) >= majority(voters.len() - 1)
                            );
                        }
                    }
                    CouncilAction::RemoveLearner(id) => {
                        prop_assert_ne!(id, leader_id);
                        prop_assert!(learners.contains(&id));
                        prop_assert!(dead_past_window(id), "evicted a learner inside the window");
                    }
                    CouncilAction::Promote(id) => {
                        prop_assert!(learners.contains(&id));
                        prop_assert!(stable_alive(id), "promoted an unstable learner");
                        // Caught up within the lag bound.
                        let last = leader_last.unwrap_or(0);
                        let matched = replication.get(&id).copied().flatten();
                        let caught_up = match matched {
                            Some(m) => last.saturating_sub(m) <= config.max_promotion_lag,
                            None => last == 0,
                        };
                        prop_assert!(caught_up, "promoted a lagging learner");
                        // The change must be committable by live voters.
                        prop_assert!(alive_voters >= majority(voters.len()));
                    }
                    CouncilAction::AddLearner(id) => {
                        prop_assert!(!voters.contains(&id));
                        prop_assert!(!learners.contains(&id));
                        prop_assert!(candidates.contains(&id));
                        prop_assert!(stable_alive(id), "added an unstable learner");
                    }
                    CouncilAction::Nothing => {}
                }
            }
        }
    }
}
