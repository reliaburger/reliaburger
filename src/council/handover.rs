//! Moving leadership off a council voter (#755).
//!
//! openraft 0.9 has no leadership transfer. The only lever is
//! `trigger().elect()`, which makes one node campaign at a higher term. That
//! term deposes the leader, but it doesn't choose the next one. Raft's next
//! election favours the longest log, and the leader we want to leave usually
//! holds it: it wrote the newest entries. So the deposed leader often wins
//! again a second later.
//!
//! The fix is to take the old leader out of the race. A node that must give
//! up leadership *holds its elections* (openraft's
//! `runtime_config().elect(false)`): it still votes and replicates, but it
//! never campaigns on its own timer. Another voter campaigns, and once someone
//! else leads, the hold ends and the node campaigns normally again.
//!
//! A hold always ends, so a node that held its elections can't leave the
//! council leaderless: if nobody else takes over within [`HANDOVER_HOLD`], it
//! campaigns again.

use std::sync::Arc;
use std::time::{Duration, Instant};

use openraft::Raft;
use tokio::sync::watch;

use super::types::TypeConfig;

/// How long a node holds its elections while leadership moves off it.
///
/// A deposed leader's followers wait out the leader lease and an election
/// timeout before they campaign: about 4 seconds with the default timers, plus
/// another 4 when they have seen a longer log. 30 seconds leaves room for a
/// few failed rounds and still bounds how long a council can sit without a
/// leader if nobody else can win.
pub const HANDOVER_HOLD: Duration = Duration::from_secs(30);

/// Whether a node holding its elections since some earlier moment may
/// campaign again: another voter leads, or the hold ran out at `until`.
pub fn hold_ends(self_id: u64, current_leader: Option<u64>, now: Instant, until: Instant) -> bool {
    current_leader.is_some_and(|leader| leader != self_id) || now >= until
}

/// Whether a voter under disk pressure holds its elections this tick.
///
/// It holds while it leads or while nobody does, so that once a follower's
/// campaign deposes it, it can't win the election that follows. It holds only
/// when a healthy voter that isn't under pressure could lead instead;
/// otherwise holding would just leave the council without a leader.
pub fn pressured_voter_holds(
    self_id: u64,
    current_leader: Option<u64>,
    pressured: bool,
    successor_available: bool,
) -> bool {
    pressured && successor_available && current_leader.is_none_or(|leader| leader == self_id)
}

/// The leader that replaced `previous`, if leadership has moved.
pub fn leader_after_handover(previous: Option<u64>, current: Option<u64>) -> Option<u64> {
    current.filter(|leader| Some(*leader) != previous)
}

/// How long a voter taking over waits between campaigns.
///
/// Followers refuse every candidate while the leader lease runs, and openraft
/// sets the lease to the maximum election timeout. A campaign made inside the
/// lease still deposes the leader, but can't win; the next one, after the
/// lease, can.
pub fn campaign_round(election_timeout_max: Duration) -> Duration {
    election_timeout_max * 3 / 2
}

/// A node's hold on its own elections.
///
/// [`hold_until`](Self::hold_until) switches elections off and starts the
/// task that switches them back on. Repeated holds only push the deadline
/// out, so the reconciler can renew a hold every tick.
pub(crate) struct ElectionHold {
    raft: Raft<TypeConfig>,
    self_id: u64,
    until: Arc<watch::Sender<Option<Instant>>>,
}

impl ElectionHold {
    /// A hold for `raft`, not yet held.
    pub(crate) fn new(raft: Raft<TypeConfig>, self_id: u64) -> Self {
        Self {
            raft,
            self_id,
            until: Arc::new(watch::Sender::new(None)),
        }
    }

    /// Stop campaigning until another voter leads or `until` passes,
    /// whichever comes first. A later `until` extends a running hold; an
    /// earlier one leaves it alone. Must be called on the tokio runtime.
    pub(crate) fn hold_until(&self, until: Instant) {
        let mut started = false;
        self.until.send_modify(|held| {
            started = held.is_none();
            *held = Some(held.map_or(until, |current| current.max(until)));
            // Inside the watch's write lock, so a release running at the
            // same moment can't switch elections back on after this.
            self.raft.runtime_config().elect(false);
        });
        if started {
            tokio::spawn(release_when_settled(
                self.raft.clone(),
                self.self_id,
                Arc::clone(&self.until),
            ));
        }
    }

    /// Whether this node is holding its elections.
    pub(crate) fn is_held(&self) -> bool {
        self.until.borrow().is_some()
    }
}

/// End the hold once another voter leads or the deadline passes.
async fn release_when_settled(
    raft: Raft<TypeConfig>,
    self_id: u64,
    until: Arc<watch::Sender<Option<Instant>>>,
) {
    let mut metrics = raft.metrics();
    loop {
        let Some(deadline) = *until.borrow() else {
            return;
        };
        let released = until.send_if_modified(|held| {
            let Some(deadline) = *held else {
                return false;
            };
            let leader = metrics.borrow().current_leader;
            if !hold_ends(self_id, leader, Instant::now(), deadline) {
                return false;
            }
            raft.runtime_config().elect(true);
            *held = None;
            true
        });
        if released {
            return;
        }
        tokio::select! {
            changed = metrics.changed() => {
                if changed.is_err() {
                    // Raft has shut down: there is nothing left to hold.
                    until.send_replace(None);
                    return;
                }
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELF: u64 = 1;
    const OTHER: u64 = 2;

    #[test]
    fn a_hold_ends_when_another_voter_leads() {
        let now = Instant::now();
        let until = now + Duration::from_secs(30);
        assert!(hold_ends(SELF, Some(OTHER), now, until));
    }

    #[test]
    fn a_hold_lasts_while_this_node_leads_or_nobody_does() {
        let now = Instant::now();
        let until = now + Duration::from_secs(30);
        assert!(!hold_ends(SELF, Some(SELF), now, until));
        assert!(!hold_ends(SELF, None, now, until));
    }

    #[test]
    fn a_hold_ends_at_its_deadline_even_without_a_new_leader() {
        let now = Instant::now();
        assert!(hold_ends(SELF, None, now, now));
        assert!(hold_ends(
            SELF,
            Some(SELF),
            now + Duration::from_secs(1),
            now
        ));
    }

    #[test]
    fn a_pressured_leader_holds_while_it_leads_and_after_it_is_deposed() {
        assert!(pressured_voter_holds(SELF, Some(SELF), true, true));
        assert!(pressured_voter_holds(SELF, None, true, true));
    }

    #[test]
    fn a_pressured_voter_stops_holding_once_another_voter_leads() {
        assert!(!pressured_voter_holds(SELF, Some(OTHER), true, true));
    }

    #[test]
    fn a_voter_without_pressure_never_holds() {
        assert!(!pressured_voter_holds(SELF, Some(SELF), false, true));
        assert!(!pressured_voter_holds(SELF, None, false, true));
    }

    #[test]
    fn a_pressured_voter_with_no_successor_keeps_campaigning() {
        assert!(!pressured_voter_holds(SELF, Some(SELF), true, false));
        assert!(!pressured_voter_holds(SELF, None, true, false));
    }

    #[test]
    fn the_old_leader_winning_again_is_not_a_handover() {
        assert_eq!(leader_after_handover(Some(SELF), Some(SELF)), None);
        assert_eq!(leader_after_handover(Some(SELF), None), None);
        assert_eq!(leader_after_handover(Some(SELF), Some(OTHER)), Some(OTHER));
        assert_eq!(leader_after_handover(None, Some(SELF)), Some(SELF));
    }

    #[test]
    fn campaigns_are_spaced_beyond_the_leader_lease() {
        let lease = Duration::from_secs(2);
        assert!(campaign_round(lease) > lease);
        assert_eq!(campaign_round(lease), Duration::from_secs(3));
    }
}
