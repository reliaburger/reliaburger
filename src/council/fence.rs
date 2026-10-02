//! The recovery-epoch fence (#424).
//!
//! `relish council recover --force` re-bootstraps one survivor as a fresh
//! single-voter council and bumps its recovery epoch. The voters it replaced
//! still hold the old membership in their data directories. Two of three old
//! voters are a majority of that membership, so if they come back they can
//! elect a leader and commit writes: a second council, a split brain.
//!
//! An equality check on the epoch stamped on each Raft RPC keeps the two
//! epochs from talking to each other, but it doesn't stop the old voters
//! talking among themselves. So the fence is a per-node state, not a pairwise
//! filter. A node that holds an epoch and learns of a newer one, from a Raft
//! reply or from gossip, fences itself for good: it stops sending and serving
//! Raft RPCs, refuses writes, stops claiming leadership, and says so in its
//! council status until an operator re-enrols it with a fresh data directory.
//!
//! A node with a fresh data directory holds no epoch at all. It adopts the
//! epoch of the first council that contacts it, which is how the recovered
//! council regrows onto new or wiped nodes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// File under `{data_dir}/raft/` holding this node's epoch claim and fence.
pub const FENCE_FILE: &str = "recovery-fence.json";

/// Where a node stands with respect to the recovery epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FenceState {
    /// A fresh data directory: no council yet, so no epoch to defend. The
    /// node adopts the epoch of the first council that contacts it.
    Unclaimed,
    /// A restarted member, holding its epoch but not serving Raft yet while
    /// gossip shows whether any peer has moved to a newer epoch.
    Probing,
    /// A member serving Raft at its epoch.
    Serving,
    /// This node's council was replaced by a recovery at `newer_epoch`. It
    /// must never serve Raft again until it is re-enrolled.
    Fenced {
        /// The newest epoch this node has seen.
        newer_epoch: u64,
    },
}

/// A point-in-time view of the fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceSnapshot {
    /// The epoch this node holds (meaningless while `Unclaimed`).
    pub epoch: u64,
    /// What the node may do with it.
    pub state: FenceState,
}

impl FenceSnapshot {
    /// The epoch this node claims, or `None` while it holds none.
    pub fn claimed_epoch(&self) -> Option<u64> {
        match self.state {
            FenceState::Unclaimed => None,
            _ => Some(self.epoch),
        }
    }

    /// The newer epoch that fenced this node, when it is fenced.
    pub fn fenced_by(&self) -> Option<u64> {
        match self.state {
            FenceState::Fenced { newer_epoch } => Some(newer_epoch),
            _ => None,
        }
    }

    /// The newest epoch this node knows exists.
    pub fn newest_known_epoch(&self) -> u64 {
        self.fenced_by().unwrap_or(self.epoch).max(self.epoch)
    }
}

/// What the accept side does with one inbound Raft RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Hand the RPC to Raft.
    Serve,
    /// Hand it to Raft once the adopted epoch is durable.
    Adopted,
    /// Refuse it, telling the sender the newest epoch this node knows.
    Refuse {
        /// The newest epoch known here; a sender below it fences itself.
        newest_epoch: u64,
    },
}

/// The shared fence handle: the Raft transport, the council node and the
/// cluster runtime all hold a clone.
#[derive(Debug, Clone)]
pub struct RecoveryFence {
    state: Arc<watch::Sender<FenceSnapshot>>,
    /// Where the claim is persisted; `None` keeps it in memory (tests).
    file: Option<PathBuf>,
    /// Serialises writers: the Raft handler and the supervisor both persist,
    /// and they share one staging file.
    persisting: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FenceRecord {
    epoch: u64,
    fenced_by: Option<u64>,
}

impl RecoveryFence {
    /// A fence starting from `snapshot`, persisted at `file` when given.
    pub fn new(snapshot: FenceSnapshot, file: Option<PathBuf>) -> Self {
        let (state, _) = watch::channel(snapshot);
        Self {
            state: Arc::new(state),
            file,
            persisting: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// An in-memory fence serving `epoch`, for a transport built outside the
    /// cluster runtime (tests, tools).
    pub fn serving(epoch: u64) -> Self {
        Self::new(
            FenceSnapshot {
                epoch,
                state: FenceState::Serving,
            },
            None,
        )
    }

    /// Work out a node's starting fence from its storage.
    ///
    /// A persisted record wins: it holds an adopted epoch the state machine
    /// may not show yet, and a fence that must survive restarts. Otherwise a
    /// node that holds Raft state (or is about to bootstrap one) claims the
    /// epoch in its snapshot, and a fresh joiner claims nothing.
    pub async fn load(
        raft_dir: &Path,
        snapshot_epoch: u64,
        holds_council_state: bool,
        bootstrapping: bool,
    ) -> std::io::Result<Self> {
        let file = raft_dir.join(FENCE_FILE);
        let record = match tokio::fs::read(&file).await {
            Ok(bytes) => Some(serde_json::from_slice::<FenceRecord>(&bytes).map_err(|e| {
                std::io::Error::other(format!("{} is unreadable: {e}", file.display()))
            })?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let snapshot = match record {
            Some(FenceRecord {
                epoch,
                fenced_by: Some(newer_epoch),
            }) => FenceSnapshot {
                epoch,
                state: FenceState::Fenced { newer_epoch },
            },
            Some(FenceRecord { epoch, .. }) if holds_council_state => FenceSnapshot {
                epoch,
                state: FenceState::Probing,
            },
            Some(FenceRecord { epoch, .. }) => FenceSnapshot {
                epoch,
                state: FenceState::Serving,
            },
            None if holds_council_state => FenceSnapshot {
                epoch: snapshot_epoch,
                state: FenceState::Probing,
            },
            None if bootstrapping => FenceSnapshot {
                epoch: snapshot_epoch,
                state: FenceState::Serving,
            },
            None => FenceSnapshot {
                epoch: snapshot_epoch,
                state: FenceState::Unclaimed,
            },
        };
        Ok(Self::new(snapshot, Some(file)))
    }

    /// The current fence.
    pub fn snapshot(&self) -> FenceSnapshot {
        *self.state.borrow()
    }

    /// Follow fence changes.
    pub fn subscribe(&self) -> watch::Receiver<FenceSnapshot> {
        self.state.subscribe()
    }

    /// Whether this node is fenced.
    pub fn is_fenced(&self) -> bool {
        self.snapshot().fenced_by().is_some()
    }

    /// The epoch to stamp on an outgoing RPC, or `None` when this node must
    /// not send Raft RPCs at all (probing, fenced, or holding no epoch).
    pub fn outbound_epoch(&self) -> Option<u64> {
        let snapshot = self.snapshot();
        matches!(snapshot.state, FenceState::Serving).then_some(snapshot.epoch)
    }

    /// Decide what to do with an inbound RPC stamped `sender_epoch`, moving
    /// the fence if the sender reveals something new.
    pub fn admit(&self, sender_epoch: u64) -> Admission {
        let mut admission = Admission::Serve;
        self.state.send_if_modified(|current| {
            let before = *current;
            admission = match current.state {
                FenceState::Unclaimed => {
                    *current = FenceSnapshot {
                        epoch: sender_epoch,
                        state: FenceState::Serving,
                    };
                    Admission::Adopted
                }
                FenceState::Serving if sender_epoch == current.epoch => Admission::Serve,
                FenceState::Serving | FenceState::Probing | FenceState::Fenced { .. } => {
                    fence_if_newer(current, sender_epoch);
                    Admission::Refuse {
                        newest_epoch: current.newest_known_epoch(),
                    }
                }
            };
            *current != before
        });
        admission
    }

    /// A peer (in a Raft reply or over gossip) holds `peer_epoch`. A node
    /// holding an older epoch fences itself. Returns `true` if this call
    /// fenced the node.
    pub fn observe_peer_epoch(&self, peer_epoch: u64) -> bool {
        let was_fenced = self.is_fenced();
        self.state
            .send_if_modified(|current| fence_if_newer(current, peer_epoch));
        !was_fenced && self.is_fenced()
    }

    /// End the startup probe: a probing node starts serving.
    pub fn finish_probe(&self) {
        self.state.send_if_modified(|current| {
            if current.state == FenceState::Probing {
                current.state = FenceState::Serving;
                true
            } else {
                false
            }
        });
    }

    /// Persist the current claim and fence, atomically. A no-op for an
    /// in-memory fence or a node that holds no epoch.
    pub async fn persist(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let _writing = self.persisting.lock().await;
        let snapshot = self.snapshot();
        if snapshot.claimed_epoch().is_none() {
            return Ok(());
        }
        let record = FenceRecord {
            epoch: snapshot.epoch,
            fenced_by: snapshot.fenced_by(),
        };
        let bytes = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
        let staging = file.with_extension("json.tmp");
        tokio::fs::write(&staging, bytes).await?;
        let staged = tokio::fs::File::open(&staging).await?;
        staged.sync_all().await?;
        tokio::fs::rename(&staging, file).await
    }
}

/// Fence `current` if `peer_epoch` is newer than anything it knows. Returns
/// `true` if the fence moved.
fn fence_if_newer(current: &mut FenceSnapshot, peer_epoch: u64) -> bool {
    if current.state == FenceState::Unclaimed || peer_epoch <= current.newest_known_epoch() {
        return false;
    }
    current.state = FenceState::Fenced {
        newer_epoch: peer_epoch,
    };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence(epoch: u64, state: FenceState) -> RecoveryFence {
        RecoveryFence::new(FenceSnapshot { epoch, state }, None)
    }

    #[test]
    fn serving_node_serves_its_own_epoch() {
        let fence = fence(1, FenceState::Serving);
        assert_eq!(fence.admit(1), Admission::Serve);
        assert_eq!(fence.outbound_epoch(), Some(1));
    }

    #[test]
    fn serving_node_refuses_an_older_sender_and_names_its_epoch() {
        let fence = fence(1, FenceState::Serving);
        assert_eq!(fence.admit(0), Admission::Refuse { newest_epoch: 1 });
        assert!(!fence.is_fenced(), "an older sender must not fence us");
    }

    #[test]
    fn serving_node_fences_itself_on_a_newer_sender() {
        let fence = fence(0, FenceState::Serving);
        assert_eq!(fence.admit(1), Admission::Refuse { newest_epoch: 1 });
        assert_eq!(fence.snapshot().fenced_by(), Some(1));
        assert_eq!(fence.outbound_epoch(), None);
    }

    #[test]
    fn fenced_node_refuses_even_its_own_epoch_and_passes_the_news_on() {
        let fence = fence(0, FenceState::Fenced { newer_epoch: 1 });
        // Another old voter at epoch 0 learns that epoch 1 exists.
        assert_eq!(fence.admit(0), Admission::Refuse { newest_epoch: 1 });
        assert_eq!(fence.outbound_epoch(), None);
    }

    #[test]
    fn fenced_node_tracks_the_newest_epoch_it_hears_of() {
        let fence = fence(0, FenceState::Fenced { newer_epoch: 1 });
        fence.observe_peer_epoch(3);
        assert_eq!(fence.snapshot().fenced_by(), Some(3));
        fence.observe_peer_epoch(2);
        assert_eq!(fence.snapshot().fenced_by(), Some(3));
    }

    #[test]
    fn probing_node_neither_sends_nor_serves() {
        let fence = fence(0, FenceState::Probing);
        assert_eq!(fence.outbound_epoch(), None);
        assert_eq!(fence.admit(0), Admission::Refuse { newest_epoch: 0 });
        assert!(!fence.is_fenced());
        fence.finish_probe();
        assert_eq!(fence.outbound_epoch(), Some(0));
    }

    #[test]
    fn probing_node_fenced_by_gossip_stays_fenced_after_the_probe() {
        let fence = fence(0, FenceState::Probing);
        assert!(fence.observe_peer_epoch(1));
        fence.finish_probe();
        assert_eq!(fence.snapshot().fenced_by(), Some(1));
        assert_eq!(fence.outbound_epoch(), None);
    }

    #[test]
    fn unclaimed_node_adopts_the_first_council_that_contacts_it() {
        let fence = fence(0, FenceState::Unclaimed);
        assert_eq!(fence.outbound_epoch(), None);
        // Gossip about epochs means nothing to a node with no council.
        assert!(!fence.observe_peer_epoch(5));
        assert_eq!(fence.admit(2), Admission::Adopted);
        assert_eq!(
            fence.snapshot(),
            FenceSnapshot {
                epoch: 2,
                state: FenceState::Serving
            }
        );
        assert_eq!(fence.admit(2), Admission::Serve);
    }

    #[test]
    fn an_older_or_equal_peer_never_fences() {
        let fence = fence(2, FenceState::Serving);
        assert!(!fence.observe_peer_epoch(2));
        assert!(!fence.observe_peer_epoch(1));
        assert!(!fence.is_fenced());
    }

    #[tokio::test]
    async fn fence_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let fence = RecoveryFence::load(dir.path(), 0, true, false)
            .await
            .unwrap();
        assert_eq!(fence.snapshot().state, FenceState::Probing);
        fence.observe_peer_epoch(1);
        fence.persist().await.unwrap();

        // Even with a snapshot claiming epoch 0 and Raft state on disk, the
        // persisted fence comes back.
        let restarted = RecoveryFence::load(dir.path(), 0, true, false)
            .await
            .unwrap();
        assert_eq!(restarted.snapshot().fenced_by(), Some(1));
    }

    #[tokio::test]
    async fn an_adopted_epoch_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let fence = RecoveryFence::load(dir.path(), 0, false, false)
            .await
            .unwrap();
        assert_eq!(fence.snapshot().state, FenceState::Unclaimed);
        assert_eq!(fence.admit(1), Admission::Adopted);
        fence.persist().await.unwrap();

        // The state machine may never have seen the epoch (it only arrives
        // in a snapshot), so the record is what restores it.
        let restarted = RecoveryFence::load(dir.path(), 0, true, false)
            .await
            .unwrap();
        assert_eq!(
            restarted.snapshot(),
            FenceSnapshot {
                epoch: 1,
                state: FenceState::Probing
            }
        );
    }

    #[tokio::test]
    async fn a_fresh_store_claims_nothing_unless_it_bootstraps() {
        let dir = tempfile::tempdir().unwrap();
        let joiner = RecoveryFence::load(dir.path(), 0, false, false)
            .await
            .unwrap();
        assert_eq!(joiner.snapshot().claimed_epoch(), None);
        let recovered = RecoveryFence::load(dir.path(), 1, false, true)
            .await
            .unwrap();
        assert_eq!(
            recovered.snapshot(),
            FenceSnapshot {
                epoch: 1,
                state: FenceState::Serving
            }
        );
        // Nothing to persist for an unclaimed node.
        joiner.persist().await.unwrap();
        assert!(!dir.path().join(FENCE_FILE).exists());
    }
}
