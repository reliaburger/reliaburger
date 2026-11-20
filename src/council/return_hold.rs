//! Holding a returning voter's elections (#1524).
//!
//! A voter replaced while it was down comes back with its old Raft state: a
//! log, a vote and a membership that still names it a voter, because it
//! missed its own removal. On its election timer it campaigns. The voters
//! refuse it while the leader's lease holds, but each failed campaign raises
//! its term, and when the leader adds it back as a learner the leader's
//! first replication meets that higher term and steps down. A few seconds
//! without writes, once per return.
//!
//! openraft 0.9 has no pre-vote, which would stop the term rising, and its
//! metrics can't say whether a node has heard from a leader since it
//! started: a restarted node restores its committed vote, so
//! `current_leader` names the old leader at once. So the transport records
//! it: every AppendEntries a leader sends, over TCP or the in-memory test
//! router, notes a [`LeaderContact`].
//!
//! A node that starts on a log store holding a vote starts with its
//! elections held. The hold ends at the first of:
//!
//! - an AppendEntries from a leader: a voter that is still a member hears
//!   one within a heartbeat, and a replaced one once it's added back as a
//!   learner;
//! - the startup grace passing while gossip names no live leader, as after
//!   a whole-cluster restart, where somebody has to campaign;
//! - the cap, so a stale gossip hint about a dead leader can't hold a node
//!   for longer than a few election timeouts.
//!
//! A normal leader failure is unaffected: the followers have heard from the
//! leader since they started, so they hold nothing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// How long a returning node holds its elections before gossip's view
/// counts: long enough for gossip to learn the members and the leader's
/// hint after a restart.
pub fn return_hold_grace(election_timeout_max: Duration) -> Duration {
    election_timeout_max * 3
}

/// The longest a returning node holds its elections, whatever gossip says.
pub fn return_hold_cap(election_timeout_max: Duration) -> Duration {
    election_timeout_max * 10
}

/// Whether a returning node's hold on its elections ends now.
pub fn return_hold_ends(
    heard_from_leader: bool,
    gossip_names_live_leader: bool,
    now: Instant,
    grace_until: Instant,
    cap_until: Instant,
) -> bool {
    heard_from_leader || now >= cap_until || (now >= grace_until && !gossip_names_live_leader)
}

/// Whether this node has had an AppendEntries from a leader since it
/// started. Cloning shares the signal: the transport notes it, the node's
/// hold watches it.
#[derive(Debug, Clone)]
pub struct LeaderContact(Arc<watch::Sender<bool>>);

impl LeaderContact {
    /// A signal nobody has noted yet.
    pub fn new() -> Self {
        Self(Arc::new(watch::Sender::new(false)))
    }

    /// Record that a leader's AppendEntries arrived.
    pub fn note(&self) {
        self.0
            .send_if_modified(|heard| !std::mem::replace(heard, true));
    }

    /// Whether a leader's AppendEntries has arrived.
    pub fn heard(&self) -> bool {
        *self.0.borrow()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

impl Default for LeaderContact {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether an AppendEntries response shows the request came from a leader
/// this node follows. `HigherVote` means this node refused it.
pub fn counts_as_leader_contact(response: &openraft::raft::AppendEntriesResponse<u64>) -> bool {
    !matches!(
        response,
        openraft::raft::AppendEntriesResponse::HigherVote(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, seconds: u64) -> Instant {
        start + Duration::from_secs(seconds)
    }

    #[test]
    fn a_leaders_append_entries_ends_the_hold_at_once() {
        let start = Instant::now();
        assert!(return_hold_ends(
            true,
            true,
            start,
            at(start, 6),
            at(start, 20)
        ));
    }

    #[test]
    fn the_hold_lasts_through_the_grace_whatever_gossip_says() {
        let start = Instant::now();
        let now = at(start, 5);
        assert!(!return_hold_ends(
            false,
            false,
            now,
            at(start, 6),
            at(start, 20)
        ));
        assert!(!return_hold_ends(
            false,
            true,
            now,
            at(start, 6),
            at(start, 20)
        ));
    }

    #[test]
    fn after_the_grace_the_hold_lasts_while_gossip_names_a_live_leader() {
        let start = Instant::now();
        let now = at(start, 10);
        assert!(!return_hold_ends(
            false,
            true,
            now,
            at(start, 6),
            at(start, 20)
        ));
        assert!(return_hold_ends(
            false,
            false,
            now,
            at(start, 6),
            at(start, 20)
        ));
    }

    #[test]
    fn the_cap_ends_a_hold_a_stale_hint_would_keep() {
        let start = Instant::now();
        assert!(return_hold_ends(
            false,
            true,
            at(start, 20),
            at(start, 6),
            at(start, 20)
        ));
    }

    #[test]
    fn grace_and_cap_scale_with_the_election_timeout() {
        let timeout = Duration::from_secs(2);
        assert_eq!(return_hold_grace(timeout), Duration::from_secs(6));
        assert_eq!(return_hold_cap(timeout), Duration::from_secs(20));
    }

    #[test]
    fn a_refused_append_entries_is_not_contact() {
        use openraft::raft::AppendEntriesResponse;
        assert!(counts_as_leader_contact(&AppendEntriesResponse::Success));
        assert!(counts_as_leader_contact(&AppendEntriesResponse::Conflict));
        assert!(!counts_as_leader_contact(
            &AppendEntriesResponse::HigherVote(openraft::Vote::new(3, 1))
        ));
    }

    #[test]
    fn contact_is_shared_by_clones() {
        let contact = LeaderContact::new();
        let transport = contact.clone();
        assert!(!contact.heard());
        transport.note();
        assert!(contact.heard());
    }
}
