//! The recovery-epoch fence (#424, #694).
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
//! Every recovery also draws a random recovery id (#694), carried next to the
//! epoch. Two recoveries that land on the same epoch (two survivors recovered
//! independently, or a stale backup restored twice) are told apart by it: a
//! node that meets its own epoch with a different id fences itself as a
//! split, and so does the other side.
//!
//! A node with a fresh data directory holds no epoch at all. It adopts the
//! epoch of the first council that contacts it, which is how the recovered
//! council regrows onto new or wiped nodes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::types::{RecoveryClaim, RecoveryId};

/// File under `{data_dir}/raft/` holding this node's epoch claim and fence.
pub const FENCE_FILE: &str = "recovery-fence.json";

/// Where a node stands with respect to the recovery epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FenceState {
    /// A fresh data directory: no council yet, so no epoch to defend. The
    /// node adopts the epoch of the first council that contacts it, unless
    /// gossip has already shown it a newer one.
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
    /// Another council claims this node's epoch from a different recovery
    /// (#694): two recoveries landed on one epoch. Neither side can tell
    /// which is the real one, so both fence, and an operator recovers one
    /// of them again (landing above this epoch) and re-enrols the other.
    Split {
        /// The other council's recovery id.
        other_id: RecoveryId,
    },
}

/// A point-in-time view of the fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceSnapshot {
    /// The epoch this node holds. While `Unclaimed` it holds none, and this
    /// is the newest epoch gossip has shown it: the floor for adoption.
    pub epoch: u64,
    /// The recovery that started `epoch`, when this node holds one and
    /// knows it (`None` at epoch 0).
    pub recovery_id: Option<RecoveryId>,
    /// What the node may do with it.
    pub state: FenceState,
}

impl FenceSnapshot {
    /// The epoch this node claims, or `None` while it holds none.
    pub fn claimed_epoch(&self) -> Option<u64> {
        self.claim().map(|claim| claim.epoch)
    }

    /// The epoch and recovery id this node claims, or `None` while it holds
    /// none.
    pub fn claim(&self) -> Option<RecoveryClaim> {
        match self.state {
            FenceState::Unclaimed => None,
            _ => Some(RecoveryClaim {
                epoch: self.epoch,
                id: self.recovery_id,
            }),
        }
    }

    /// The epoch that fenced this node, when it is fenced: the newer epoch
    /// that replaced it, or its own epoch when a split fenced it.
    pub fn fenced_by(&self) -> Option<u64> {
        match self.state {
            FenceState::Fenced { newer_epoch } => Some(newer_epoch),
            FenceState::Split { .. } => Some(self.epoch),
            _ => None,
        }
    }

    /// The other council's recovery id, when a split fenced this node.
    pub fn split_with(&self) -> Option<RecoveryId> {
        match self.state {
            FenceState::Split { other_id } => Some(other_id),
            _ => None,
        }
    }

    /// The newest epoch this node knows exists.
    pub fn newest_known_epoch(&self) -> u64 {
        self.fenced_by().unwrap_or(self.epoch).max(self.epoch)
    }

    /// What a refusal tells the sender: the newest epoch this node knows
    /// of, with its recovery id when that epoch is this node's own.
    fn refusal_claim(&self) -> RecoveryClaim {
        let newest = self.newest_known_epoch();
        let own = newest == self.epoch && self.state != FenceState::Unclaimed;
        RecoveryClaim {
            epoch: newest,
            id: if own { self.recovery_id } else { None },
        }
    }
}

/// What the accept side does with one inbound Raft RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Hand the RPC to Raft.
    Serve,
    /// Hand it to Raft once the adopted epoch is durable.
    Adopted,
    /// Refuse it, telling the sender the newest epoch this node knows (and
    /// its recovery id, when that is this node's own epoch).
    Refuse {
        /// A sender below this epoch fences itself; one at this epoch with
        /// a different recovery id is the other half of a split.
        newest: RecoveryClaim,
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
    /// Why the last write of the record failed, until one succeeds (#694).
    /// A fence that never reached the disk is forgotten by the next restart,
    /// so `relish council status` shows this.
    persist_failure: Arc<watch::Sender<Option<String>>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FenceRecord {
    epoch: u64,
    recovery_id: Option<RecoveryId>,
    fenced_by: Option<u64>,
    split_with: Option<RecoveryId>,
}

impl FenceRecord {
    /// The persisted fence as a snapshot. A record that isn't fenced gets
    /// `unfenced`: the caller decides between probing and serving.
    fn snapshot(&self, unfenced: FenceState) -> FenceSnapshot {
        let state = match (self.split_with, self.fenced_by) {
            (Some(other_id), _) => FenceState::Split { other_id },
            (None, Some(newer_epoch)) => FenceState::Fenced { newer_epoch },
            (None, None) => unfenced,
        };
        FenceSnapshot {
            epoch: self.epoch,
            recovery_id: self.recovery_id,
            state,
        }
    }

    fn decode(file: &Path, bytes: &[u8]) -> std::io::Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|e| std::io::Error::other(format!("{} is unreadable: {e}", file.display())))
    }
}

impl RecoveryFence {
    /// A fence starting from `snapshot`, persisted at `file` when given.
    pub fn new(snapshot: FenceSnapshot, file: Option<PathBuf>) -> Self {
        let (state, _) = watch::channel(snapshot);
        let (persist_failure, _) = watch::channel(None);
        Self {
            state: Arc::new(state),
            file,
            persisting: Arc::new(tokio::sync::Mutex::new(())),
            persist_failure: Arc::new(persist_failure),
        }
    }

    /// An in-memory fence serving `epoch`, for a transport built outside the
    /// cluster runtime (tests, tools).
    pub fn serving(epoch: u64) -> Self {
        Self::new(
            FenceSnapshot {
                epoch,
                recovery_id: None,
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
    /// epoch and recovery id in its snapshot, and a fresh joiner claims
    /// nothing.
    pub async fn load(
        raft_dir: &Path,
        snapshot_claim: RecoveryClaim,
        holds_council_state: bool,
        bootstrapping: bool,
    ) -> std::io::Result<Self> {
        let file = raft_dir.join(FENCE_FILE);
        let record = match tokio::fs::read(&file).await {
            Ok(bytes) => Some(FenceRecord::decode(&file, &bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let from_snapshot = |state| FenceSnapshot {
            epoch: snapshot_claim.epoch,
            recovery_id: snapshot_claim.id,
            state,
        };
        let snapshot = match record {
            Some(record) if holds_council_state => record.snapshot(FenceState::Probing),
            Some(record) => record.snapshot(FenceState::Serving),
            None if holds_council_state => from_snapshot(FenceState::Probing),
            None if bootstrapping => from_snapshot(FenceState::Serving),
            None => from_snapshot(FenceState::Unclaimed),
        };
        Ok(Self::new(snapshot, Some(file)))
    }

    /// Read a stopped node's persisted fence, if it has a record. A record
    /// that isn't fenced reads as `Serving`. Blocking: for offline tools
    /// such as `relish council recover` and `re-enrol`, never the runtime.
    pub fn read_persisted(raft_dir: &Path) -> std::io::Result<Option<FenceSnapshot>> {
        let file = raft_dir.join(FENCE_FILE);
        match std::fs::read(&file) {
            Ok(bytes) => FenceRecord::decode(&file, &bytes)
                .map(|record| Some(record.snapshot(FenceState::Serving))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Read the epoch a stopped node's persisted record says fenced it, if
    /// any. Blocking, like [`Self::read_persisted`].
    pub fn read_fenced_by(raft_dir: &Path) -> std::io::Result<Option<u64>> {
        Ok(Self::read_persisted(raft_dir)?.and_then(|snapshot| snapshot.fenced_by()))
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

    /// Why the last attempt to persist the fence failed, while no later
    /// attempt has succeeded.
    pub fn persist_failure(&self) -> Option<String> {
        self.persist_failure.borrow().clone()
    }

    /// The claim to stamp on an outgoing RPC, or `None` when this node must
    /// not send Raft RPCs at all (probing, fenced, or holding no epoch).
    pub fn outbound_claim(&self) -> Option<RecoveryClaim> {
        let snapshot = self.snapshot();
        if snapshot.state == FenceState::Serving {
            snapshot.claim()
        } else {
            None
        }
    }

    /// Decide what to do with an inbound RPC from a sender claiming
    /// `sender`, moving the fence if the sender reveals something new.
    pub fn admit(&self, sender: RecoveryClaim) -> Admission {
        let mut admission = Admission::Serve;
        self.state.send_if_modified(|current| {
            let before = *current;
            admission = match current.state {
                // A council older than one gossip has already shown us is a
                // replaced one: joining it would hand it a vote (#428).
                FenceState::Unclaimed if sender.epoch < current.epoch => Admission::Refuse {
                    newest: current.refusal_claim(),
                },
                FenceState::Unclaimed => {
                    *current = FenceSnapshot {
                        epoch: sender.epoch,
                        recovery_id: sender.id,
                        state: FenceState::Serving,
                    };
                    Admission::Adopted
                }
                FenceState::Serving
                    if current.claim().is_some_and(|own| {
                        own.epoch == sender.epoch && !own.conflicts_with(&sender)
                    }) =>
                {
                    Admission::Serve
                }
                FenceState::Serving
                | FenceState::Probing
                | FenceState::Fenced { .. }
                | FenceState::Split { .. } => {
                    fence_if_newer(current, sender.epoch);
                    split_if_conflicting(current, sender);
                    Admission::Refuse {
                        newest: current.refusal_claim(),
                    }
                }
            };
            *current != before
        });
        admission
    }

    /// A peer (in a Raft reply or over gossip) holds `peer`. A node holding
    /// an older epoch fences itself, and so does one holding the same epoch
    /// from a different recovery. Returns `true` if this call fenced the
    /// node.
    pub fn observe_peer(&self, peer: RecoveryClaim) -> bool {
        let was_fenced = self.is_fenced();
        self.state.send_if_modified(|current| {
            fence_if_newer(current, peer.epoch) || split_if_conflicting(current, peer)
        });
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
    /// in-memory fence or a node that holds no epoch. A failure is kept for
    /// [`Self::persist_failure`] until a later write succeeds.
    pub async fn persist(&self) -> std::io::Result<()> {
        let result = self.write_record().await;
        let failure = result.as_ref().err().map(ToString::to_string);
        self.persist_failure.send_if_modified(|current| {
            let changed = *current != failure;
            *current = failure;
            changed
        });
        result
    }

    async fn write_record(&self) -> std::io::Result<()> {
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
            recovery_id: snapshot.recovery_id,
            fenced_by: snapshot.fenced_by(),
            split_with: snapshot.split_with(),
        };
        let bytes = serde_json::to_vec(&record).map_err(std::io::Error::other)?;
        let staging = file.with_extension("json.tmp");
        tokio::fs::write(&staging, bytes).await?;
        let staged = tokio::fs::File::open(&staging).await?;
        staged.sync_all().await?;
        tokio::fs::rename(&staging, file).await?;
        // The rename is only durable once the directory entry is: without
        // this a power cut can bring back a node that forgot its fence.
        if let Some(directory) = file.parent() {
            tokio::fs::File::open(directory).await?.sync_all().await?;
        }
        Ok(())
    }
}

/// Fence `current` if `peer_epoch` is newer than anything it knows. Returns
/// `true` if the fence moved.
fn fence_if_newer(current: &mut FenceSnapshot, peer_epoch: u64) -> bool {
    if peer_epoch <= current.newest_known_epoch() {
        return false;
    }
    if current.state == FenceState::Unclaimed {
        // Nothing to fence: remember it as the floor for adoption.
        current.epoch = peer_epoch;
        return true;
    }
    current.state = FenceState::Fenced {
        newer_epoch: peer_epoch,
    };
    true
}

/// Fence `current` as a split if it serves (or is about to serve) the epoch
/// `peer` claims, but from a different recovery (#694). Returns `true` if
/// the fence moved.
fn split_if_conflicting(current: &mut FenceSnapshot, peer: RecoveryClaim) -> bool {
    if !matches!(current.state, FenceState::Serving | FenceState::Probing) {
        return false;
    }
    let (Some(own), Some(other_id)) = (current.claim(), peer.id) else {
        return false;
    };
    if !own.conflicts_with(&peer) {
        return false;
    }
    current.state = FenceState::Split { other_id };
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence(epoch: u64, state: FenceState) -> RecoveryFence {
        RecoveryFence::new(
            FenceSnapshot {
                epoch,
                recovery_id: None,
                state,
            },
            None,
        )
    }

    /// A claim with no recovery id, as epoch 0 and every pre-#694 test uses.
    fn at(epoch: u64) -> RecoveryClaim {
        RecoveryClaim { epoch, id: None }
    }

    fn claim(epoch: u64, id: u128) -> RecoveryClaim {
        RecoveryClaim {
            epoch,
            id: Some(RecoveryId(id)),
        }
    }

    fn recovered(epoch: u64, id: u128) -> RecoveryFence {
        RecoveryFence::new(
            FenceSnapshot {
                epoch,
                recovery_id: Some(RecoveryId(id)),
                state: FenceState::Serving,
            },
            None,
        )
    }

    #[test]
    fn serving_node_serves_its_own_epoch() {
        let fence = fence(1, FenceState::Serving);
        assert_eq!(fence.admit(at(1)), Admission::Serve);
        assert_eq!(fence.outbound_claim(), Some(at(1)));
    }

    #[test]
    fn serving_node_refuses_an_older_sender_and_names_its_epoch() {
        let fence = fence(1, FenceState::Serving);
        assert_eq!(fence.admit(at(0)), Admission::Refuse { newest: at(1) });
        assert!(!fence.is_fenced(), "an older sender must not fence us");
    }

    #[test]
    fn serving_node_fences_itself_on_a_newer_sender() {
        let fence = fence(0, FenceState::Serving);
        assert_eq!(fence.admit(at(1)), Admission::Refuse { newest: at(1) });
        assert_eq!(fence.snapshot().fenced_by(), Some(1));
        assert_eq!(fence.outbound_claim(), None);
    }

    #[test]
    fn fenced_node_refuses_even_its_own_epoch_and_passes_the_news_on() {
        let fence = fence(0, FenceState::Fenced { newer_epoch: 1 });
        // Another old voter at epoch 0 learns that epoch 1 exists.
        assert_eq!(fence.admit(at(0)), Admission::Refuse { newest: at(1) });
        assert_eq!(fence.outbound_claim(), None);
    }

    #[test]
    fn fenced_node_tracks_the_newest_epoch_it_hears_of() {
        let fence = fence(0, FenceState::Fenced { newer_epoch: 1 });
        fence.observe_peer(at(3));
        assert_eq!(fence.snapshot().fenced_by(), Some(3));
        fence.observe_peer(at(2));
        assert_eq!(fence.snapshot().fenced_by(), Some(3));
    }

    #[test]
    fn probing_node_neither_sends_nor_serves() {
        let fence = fence(0, FenceState::Probing);
        assert_eq!(fence.outbound_claim(), None);
        assert_eq!(fence.admit(at(0)), Admission::Refuse { newest: at(0) });
        assert!(!fence.is_fenced());
        fence.finish_probe();
        assert_eq!(fence.outbound_claim(), Some(at(0)));
    }

    #[test]
    fn probing_node_fenced_by_gossip_stays_fenced_after_the_probe() {
        let fence = fence(0, FenceState::Probing);
        assert!(fence.observe_peer(at(1)));
        fence.finish_probe();
        assert_eq!(fence.snapshot().fenced_by(), Some(1));
        assert_eq!(fence.outbound_claim(), None);
    }

    #[test]
    fn unclaimed_node_adopts_the_first_council_that_contacts_it() {
        let fence = fence(0, FenceState::Unclaimed);
        assert_eq!(fence.outbound_claim(), None);
        // Gossip can't fence a node with no council.
        assert!(!fence.observe_peer(at(1)));
        assert!(!fence.is_fenced());
        assert_eq!(fence.admit(claim(2, 7)), Admission::Adopted);
        assert_eq!(
            fence.snapshot(),
            FenceSnapshot {
                epoch: 2,
                recovery_id: Some(RecoveryId(7)),
                state: FenceState::Serving
            }
        );
        assert_eq!(fence.admit(claim(2, 7)), Admission::Serve);
    }

    /// #428: a re-enrolled old voter must not hand a vote to an unfenced
    /// old voter once gossip has shown it the recovered council's epoch.
    #[test]
    fn unclaimed_node_refuses_a_council_older_than_one_gossip_showed_it() {
        let fence = fence(0, FenceState::Unclaimed);
        fence.observe_peer(at(1));
        assert_eq!(fence.admit(at(0)), Admission::Refuse { newest: at(1) });
        assert_eq!(fence.snapshot().claimed_epoch(), None);
        assert_eq!(fence.admit(at(1)), Admission::Adopted);
        assert_eq!(fence.snapshot().claimed_epoch(), Some(1));
    }

    #[test]
    fn an_older_or_equal_peer_never_fences() {
        let fence = fence(2, FenceState::Serving);
        assert!(!fence.observe_peer(at(2)));
        assert!(!fence.observe_peer(at(1)));
        assert!(!fence.is_fenced());
    }

    /// #694: two recoveries landed on epoch 1. The receiver fences itself as
    /// a split and tells the sender its own id, so the sender fences too.
    #[test]
    fn same_epoch_with_a_different_recovery_id_is_refused() {
        let receiver = recovered(1, 0xaaaa);
        let refusal = receiver.admit(claim(1, 0xbbbb));
        assert_eq!(
            refusal,
            Admission::Refuse {
                newest: claim(1, 0xaaaa)
            }
        );
        assert_eq!(receiver.snapshot().split_with(), Some(RecoveryId(0xbbbb)));
        assert_eq!(receiver.snapshot().fenced_by(), Some(1));
        assert_eq!(receiver.outbound_claim(), None);

        let sender = recovered(1, 0xbbbb);
        let Admission::Refuse { newest } = refusal else {
            unreachable!()
        };
        assert!(sender.observe_peer(newest), "the sender must fence too");
        assert_eq!(sender.snapshot().split_with(), Some(RecoveryId(0xaaaa)));

        // The same recovery's own members keep talking.
        let peer = recovered(1, 0xaaaa);
        assert_eq!(peer.admit(claim(1, 0xaaaa)), Admission::Serve);
    }

    #[test]
    fn a_peer_whose_recovery_id_is_unknown_is_not_a_split() {
        let fence = recovered(1, 0xaaaa);
        assert!(!fence.observe_peer(at(1)));
        assert_eq!(fence.admit(at(1)), Admission::Serve);
    }

    #[test]
    fn a_split_node_is_fenced_by_a_newer_recovery() {
        let fence = recovered(1, 0xaaaa);
        fence.observe_peer(claim(1, 0xbbbb));
        fence.observe_peer(at(2));
        assert_eq!(fence.snapshot().fenced_by(), Some(2));
        assert_eq!(fence.snapshot().split_with(), None);
    }

    #[tokio::test]
    async fn fence_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let fence = RecoveryFence::load(dir.path(), at(0), true, false)
            .await
            .unwrap();
        assert_eq!(fence.snapshot().state, FenceState::Probing);
        fence.observe_peer(at(1));
        fence.persist().await.unwrap();

        // Even with a snapshot claiming epoch 0 and Raft state on disk, the
        // persisted fence comes back.
        let restarted = RecoveryFence::load(dir.path(), at(0), true, false)
            .await
            .unwrap();
        assert_eq!(restarted.snapshot().fenced_by(), Some(1));
    }

    #[tokio::test]
    async fn a_split_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let fence = RecoveryFence::load(dir.path(), claim(1, 0xaaaa), true, false)
            .await
            .unwrap();
        fence.observe_peer(claim(1, 0xbbbb));
        fence.persist().await.unwrap();
        let restarted = RecoveryFence::load(dir.path(), claim(1, 0xaaaa), true, false)
            .await
            .unwrap();
        assert_eq!(restarted.snapshot().split_with(), Some(RecoveryId(0xbbbb)));
        assert_eq!(
            RecoveryFence::read_persisted(dir.path())
                .unwrap()
                .and_then(|snapshot| snapshot.fenced_by()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn an_adopted_epoch_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let fence = RecoveryFence::load(dir.path(), at(0), false, false)
            .await
            .unwrap();
        assert_eq!(fence.snapshot().state, FenceState::Unclaimed);
        assert_eq!(fence.admit(claim(1, 9)), Admission::Adopted);
        fence.persist().await.unwrap();

        // The state machine may never have seen the epoch (it only arrives
        // in a snapshot), so the record is what restores it.
        let restarted = RecoveryFence::load(dir.path(), at(0), true, false)
            .await
            .unwrap();
        assert_eq!(
            restarted.snapshot(),
            FenceSnapshot {
                epoch: 1,
                recovery_id: Some(RecoveryId(9)),
                state: FenceState::Probing
            }
        );
    }

    #[tokio::test]
    async fn a_fresh_store_claims_nothing_unless_it_bootstraps() {
        let dir = tempfile::tempdir().unwrap();
        let joiner = RecoveryFence::load(dir.path(), at(0), false, false)
            .await
            .unwrap();
        assert_eq!(joiner.snapshot().claimed_epoch(), None);
        let recovered = RecoveryFence::load(dir.path(), claim(1, 5), false, true)
            .await
            .unwrap();
        assert_eq!(
            recovered.snapshot(),
            FenceSnapshot {
                epoch: 1,
                recovery_id: Some(RecoveryId(5)),
                state: FenceState::Serving
            }
        );
        // Nothing to persist for an unclaimed node.
        joiner.persist().await.unwrap();
        assert!(!dir.path().join(FENCE_FILE).exists());
    }

    #[tokio::test]
    async fn a_failed_write_is_reported_until_one_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let raft = dir.path().join("raft");
        let fence = RecoveryFence::load(&raft, at(0), true, false)
            .await
            .unwrap();
        fence.observe_peer(at(1));
        // The directory doesn't exist yet: the write fails.
        assert!(fence.persist().await.is_err());
        assert!(fence.persist_failure().is_some());
        std::fs::create_dir_all(&raft).unwrap();
        fence.persist().await.unwrap();
        assert_eq!(fence.persist_failure(), None);
    }
}
