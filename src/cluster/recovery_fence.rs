//! Wiring the recovery fence (#424, #694) into a running node.
//!
//! [`crate::council::fence`] holds the state machine. This module feeds it
//! from gossip and keeps it on disk: a restarted voter waits for its peers'
//! advertised epochs before it serves Raft, any node that sees a newer epoch
//! (or its own epoch from a different recovery) in gossip fences itself, and
//! every change is persisted, with retries, so a fence survives restarts. It
//! also keeps fenced nodes out of the reconciler's candidates, since a node
//! that refuses every Raft RPC can never catch up.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::council::fence::{FenceSnapshot, FenceState, RecoveryFence};
use crate::council::node::CouncilNode;
use crate::council::types::RecoveryClaim;
use crate::meat::types::NodeId;
use crate::mustard::directory::NodeDirectory;
use crate::mustard::membership::MembershipSnapshot;

/// How long a restarted voter keeps listening after gossip has joined and
/// synced, unless every persisted peer advertises sooner: two anti-entropy
/// intervals (`GossipConfig::push_pull_interval`), so a peer that missed
/// every piggybacked update still reaches this node in a full table
/// exchange. `[cluster] recovery_probe_window_secs` overrides it.
pub fn default_probe_window(gossip: &crate::mustard::GossipConfig) -> Duration {
    gossip.push_pull_interval * 2
}

/// How often a probing node looks again for its persisted membership while
/// openraft is still loading it.
const MEMBERSHIP_RECHECK: Duration = Duration::from_millis(200);

/// How long the supervisor waits before retrying a fence write that failed.
pub const FENCE_PERSIST_RETRY: Duration = Duration::from_secs(5);

/// The startup probe's window (#694). It opens when gossip has joined and
/// completed its first table exchange, not when the process starts: gossip
/// may only come up seconds into a restart, and a window that runs out
/// before gossip has had a fair chance proves nothing.
#[derive(Debug, Clone, Copy)]
pub struct ProbeWindow {
    length: Duration,
    opened_at: Option<Instant>,
}

/// What the probe decides at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Keep probing. `until` is when the window closes, or `None` while
    /// gossip hasn't synced (only gossip or every peer can end it then).
    Wait {
        /// When to look again.
        until: Option<Instant>,
    },
    /// Stop probing and serve Raft.
    Serve {
        /// Whether every persisted peer advertised an epoch. `false` means
        /// the window simply ran out, which the supervisor logs.
        heard_every_peer: bool,
    },
}

impl ProbeWindow {
    /// A window of `length`, not yet open.
    pub fn new(length: Duration) -> Self {
        Self {
            length,
            opened_at: None,
        }
    }

    /// Gossip joined and synced at `now`. Later calls keep the first time.
    pub fn open(&mut self, now: Instant) {
        self.opened_at.get_or_insert(now);
    }

    /// Whether gossip has synced, so the window is running.
    pub fn is_open(&self) -> bool {
        self.opened_at.is_some()
    }

    /// Decide at `now`, given whether every persisted peer has advertised
    /// an epoch over gossip.
    pub fn decide(&self, now: Instant, heard_every_peer: bool) -> ProbeVerdict {
        if heard_every_peer {
            return ProbeVerdict::Serve {
                heard_every_peer: true,
            };
        }
        match self.opened_at.map(|opened| opened + self.length) {
            Some(closes) if now >= closes => ProbeVerdict::Serve {
                heard_every_peer: false,
            },
            until => ProbeVerdict::Wait { until },
        }
    }
}

/// Whether gossip has shown an advertised epoch (or the lack of one), in
/// `claims` (the directory's `recovery_claims`), for every peer in `peers`,
/// the other voters of this node's persisted membership. `None` (a membership not loaded yet) is never complete. A
/// single-voter council has no peers to hear from, so it is complete once
/// gossip has synced: a node with no seeds at all counts as synced at once,
/// which keeps a single-node cluster's restart as quick as before.
pub fn probe_complete(
    claims: &HashMap<NodeId, Option<RecoveryClaim>>,
    peers: Option<&BTreeSet<NodeId>>,
    gossip_synced: bool,
) -> bool {
    match peers {
        None => false,
        Some(peers) if peers.is_empty() => gossip_synced,
        Some(peers) => peers.iter().all(|peer| claims.contains_key(peer)),
    }
}

/// Nodes advertising an epoch older than `own`: fenced voters of a replaced
/// council. A node advertising no epoch is fresh and may join.
pub fn stale_epoch_members(directory: &NodeDirectory, own: Option<u64>) -> HashSet<NodeId> {
    let Some(own) = own else {
        return HashSet::new();
    };
    directory
        .recovery_claims
        .iter()
        .filter(|(_, claim)| claim.is_some_and(|claim| claim.epoch < own))
        .map(|(node_id, _)| node_id.clone())
        .collect()
}

/// The members this node's persisted membership names, other than itself,
/// or `None` while openraft hasn't loaded a membership yet.
fn persisted_peers(council: &CouncilNode, self_name: &str) -> Option<BTreeSet<NodeId>> {
    let metrics = council.metrics();
    let metrics = metrics.borrow();
    let membership = metrics.membership_config.membership();
    membership.nodes().next()?;
    Some(
        membership
            .nodes()
            .filter(|(_, info)| info.name != self_name)
            .map(|(_, info)| NodeId::new(&info.name))
            .collect(),
    )
}

fn log_transition(before: FenceSnapshot, after: FenceSnapshot) {
    match after.state {
        FenceState::Fenced { newer_epoch } => eprintln!(
            "council: FENCED: a recovery replaced this node's council (epoch {}) with epoch \
             {newer_epoch}; this node no longer serves Raft or accepts writes. Re-enrol it with \
             a fresh data directory",
            after.epoch
        ),
        FenceState::Split { other_id } => eprintln!(
            "council: FENCED (split): another council claims this node's recovery epoch {} \
             from a different recovery ({other_id}); this node no longer serves Raft or accepts \
             writes. Recover one side again and re-enrol the other",
            after.epoch
        ),
        FenceState::Serving if before.state == FenceState::Probing => eprintln!(
            "council: no peer advertises a newer recovery epoch than {}; serving raft",
            after.epoch
        ),
        FenceState::Serving if before.state == FenceState::Unclaimed => eprintln!(
            "council: joined a council at recovery epoch {}",
            after.epoch
        ),
        _ => {}
    }
}

/// The inputs the fence supervisor watches.
pub struct FenceSupervisorInputs {
    /// The gossip directory, carrying every peer's advertised claim.
    pub directory_rx: watch::Receiver<NodeDirectory>,
    /// Flips to `true` once gossip has joined and completed a table
    /// exchange with a peer.
    pub gossip_synced_rx: watch::Receiver<bool>,
    /// The probe window's length once gossip has synced.
    pub probe_window: Duration,
}

/// Feed `fence` from gossip, end the startup probe, and persist every
/// change, retrying failed writes every [`FENCE_PERSIST_RETRY`]. Runs until
/// `shutdown`; the caller spawns it. `peers` names the members of this
/// node's persisted membership other than itself (`None` until it loads).
pub async fn supervise_recovery_fence(
    fence: RecoveryFence,
    peers: impl Fn() -> Option<BTreeSet<NodeId>>,
    inputs: FenceSupervisorInputs,
    shutdown: CancellationToken,
) {
    let FenceSupervisorInputs {
        mut directory_rx,
        mut gossip_synced_rx,
        probe_window,
    } = inputs;
    let mut probe = ProbeWindow::new(probe_window);
    let mut fence_rx = fence.subscribe();
    // What the disk holds is unknown until the first write: the fence may
    // have moved (a Raft RPC adopting or fencing) before this task ran.
    let mut recorded: Option<FenceSnapshot> = None;
    let mut last_seen = fence.snapshot();
    let mut retry_at: Option<Instant> = None;
    let mut gossip_live = true;
    loop {
        let now = Instant::now();
        if *gossip_synced_rx.borrow_and_update() {
            probe.open(now);
        }
        let mut wake = None;
        // Copy the claims out and drop the directory borrow in this one
        // statement. A `watch` borrow holds the channel's read lock, and
        // `peers()` reads openraft's metrics channel, which the leader-target
        // maintainer holds while it reads the directory. Holding both in
        // opposite orders deadlocked four worker threads behind the two
        // channels' queued writers (#1273).
        let claims = directory_rx.borrow_and_update().recovery_claims.clone();
        for claim in claims.values().flatten() {
            fence.observe_peer(*claim);
        }
        if fence.snapshot().state == FenceState::Probing {
            let peers = peers();
            let heard = probe_complete(&claims, peers.as_ref(), probe.is_open());
            match probe.decide(now, heard) {
                ProbeVerdict::Serve { heard_every_peer } => {
                    if !heard_every_peer {
                        let missing: Vec<&str> = peers
                            .iter()
                            .flatten()
                            .filter(|peer| !claims.contains_key(peer))
                            .map(|peer| peer.0.as_str())
                            .collect();
                        eprintln!(
                            "council: recovery probe window ({}s after gossip synced) ended \
                             without hearing from every persisted peer (missing: {})",
                            probe_window.as_secs(),
                            if missing.is_empty() {
                                "none known".to_string()
                            } else {
                                missing.join(", ")
                            }
                        );
                    }
                    fence.finish_probe();
                }
                // Openraft loads the persisted membership in the
                // background and says nothing when it lands: look again
                // shortly rather than wait for an unrelated change.
                ProbeVerdict::Wait { until } if peers.is_none() => {
                    let soon = now + MEMBERSHIP_RECHECK;
                    wake = Some(until.map_or(soon, |until| until.min(soon)));
                }
                ProbeVerdict::Wait { until } => wake = until,
            }
        }

        let current = *fence_rx.borrow_and_update();
        let changed = current != last_seen;
        if changed {
            log_transition(last_seen, current);
            last_seen = current;
        }
        let retry_due = retry_at.is_some_and(|at| now >= at);
        let first_write = recorded.is_none() && retry_at.is_none();
        if recorded != Some(current) && (first_write || changed || retry_due) {
            match fence.persist().await {
                Ok(()) => {
                    recorded = Some(current);
                    retry_at = None;
                }
                Err(error) => {
                    eprintln!(
                        "council: could not persist the recovery fence ({error}); retrying in \
                         {}s",
                        FENCE_PERSIST_RETRY.as_secs()
                    );
                    retry_at = Some(Instant::now() + FENCE_PERSIST_RETRY);
                }
            }
        }

        tokio::select! {
            _ = shutdown.cancelled() => break,
            changed = directory_rx.changed() => if changed.is_err() { break },
            changed = fence_rx.changed() => if changed.is_err() { break },
            changed = gossip_synced_rx.changed(), if gossip_live => {
                gossip_live = changed.is_ok();
            }
            _ = sleep_until_some(wake), if wake.is_some() => {}
            _ = sleep_until_some(retry_at), if retry_at.is_some() => {}
        }
    }
}

async fn sleep_until_some(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Spawn [`supervise_recovery_fence`] for a running node.
pub fn spawn_recovery_fence_supervisor(
    fence: RecoveryFence,
    council: Arc<CouncilNode>,
    inputs: FenceSupervisorInputs,
    self_name: String,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let peers = || persisted_peers(&council, &self_name);
        supervise_recovery_fence(fence, peers, inputs, shutdown).await;
    });
}

/// The membership the council reconciler plans from: gossip's live members
/// minus fenced voters of a replaced council.
pub fn spawn_reconciler_membership_filter(
    mut membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    mut directory_rx: watch::Receiver<NodeDirectory>,
    fence: RecoveryFence,
    shutdown: CancellationToken,
) -> watch::Receiver<Vec<MembershipSnapshot>> {
    // Each channel is borrowed in its own statement: two `watch` borrows
    // held at once can deadlock (#1273, #1433).
    let filter = move |membership_rx: &watch::Receiver<Vec<MembershipSnapshot>>,
                       directory_rx: &watch::Receiver<NodeDirectory>| {
        let own_epoch = fence.snapshot().claimed_epoch();
        let stale = stale_epoch_members(&directory_rx.borrow(), own_epoch);
        membership_rx
            .borrow()
            .iter()
            .filter(|member| !stale.contains(&member.node_id))
            .cloned()
            .collect::<Vec<_>>()
    };
    let (tx, rx) = watch::channel(filter(&membership_rx, &directory_rx));
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                changed = membership_rx.changed() => if changed.is_err() { break },
                changed = directory_rx.changed() => if changed.is_err() { break },
            }
            let filtered = filter(&membership_rx, &directory_rx);
            if tx.send(filtered).is_err() {
                break;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use super::*;

    fn directory(epochs: &[(&str, Option<u64>)]) -> NodeDirectory {
        NodeDirectory {
            recovery_claims: epochs
                .iter()
                .map(|(name, epoch)| {
                    let claim = epoch.map(|epoch| RecoveryClaim { epoch, id: None });
                    (NodeId::new(*name), claim)
                })
                .collect(),
            ..NodeDirectory::default()
        }
    }

    fn peers(names: &[&str]) -> BTreeSet<NodeId> {
        names.iter().map(|name| NodeId::new(*name)).collect()
    }

    #[test]
    fn probe_completes_once_every_persisted_peer_has_advertised() {
        let partial = directory(&[("n2", Some(0))]);
        assert!(!probe_complete(
            &partial.recovery_claims,
            Some(&peers(&["n2", "n3"])),
            true
        ));
        // A wiped peer advertises no epoch; that still answers the question.
        let full = directory(&[("n2", Some(0)), ("n3", None)]);
        assert!(probe_complete(
            &full.recovery_claims,
            Some(&peers(&["n2", "n3"])),
            false
        ));
    }

    #[test]
    fn a_membership_not_loaded_yet_never_completes_the_probe() {
        assert!(!probe_complete(
            &directory(&[("n2", Some(0))]).recovery_claims,
            None,
            true
        ));
    }

    #[test]
    fn a_sole_voter_completes_the_probe_once_gossip_has_synced() {
        let alone = peers(&[]);
        assert!(!probe_complete(
            &directory(&[]).recovery_claims,
            Some(&alone),
            false
        ));
        assert!(probe_complete(
            &directory(&[]).recovery_claims,
            Some(&alone),
            true
        ));
    }

    #[test]
    fn older_epoch_members_are_stale_and_fresh_ones_are_not() {
        let dir = directory(&[("old", Some(0)), ("current", Some(1)), ("fresh", None)]);
        assert_eq!(
            stale_epoch_members(&dir, Some(1)),
            HashSet::from([NodeId::new("old")])
        );
        // A node with no epoch of its own judges no one.
        assert!(stale_epoch_members(&dir, None).is_empty());
    }

    #[test]
    fn the_default_window_is_two_anti_entropy_intervals() {
        let gossip = crate::mustard::GossipConfig::default();
        assert_eq!(default_probe_window(&gossip), gossip.push_pull_interval * 2);
    }

    // -- the supervisor, driven by hand under paused time ------------------

    const WINDOW: Duration = Duration::from_secs(20);

    struct Rig {
        fence: RecoveryFence,
        directory_tx: watch::Sender<NodeDirectory>,
        synced_tx: watch::Sender<bool>,
        shutdown: CancellationToken,
    }

    fn probing_fence(file: Option<std::path::PathBuf>) -> RecoveryFence {
        RecoveryFence::new(
            FenceSnapshot {
                epoch: 0,
                recovery_id: None,
                state: FenceState::Probing,
            },
            file,
        )
    }

    /// The supervisor future for `fence`, whose persisted peers are `n2` and
    /// `n3`, plus the senders that drive it.
    fn rig(fence: RecoveryFence) -> (Rig, impl Future<Output = ()>) {
        let (directory_tx, directory_rx) = watch::channel(NodeDirectory::default());
        let (synced_tx, gossip_synced_rx) = watch::channel(false);
        let shutdown = CancellationToken::new();
        let supervisor = supervise_recovery_fence(
            fence.clone(),
            || Some(peers(&["n2", "n3"])),
            FenceSupervisorInputs {
                directory_rx,
                gossip_synced_rx,
                probe_window: WINDOW,
            },
            shutdown.clone(),
        );
        let rig = Rig {
            fence,
            directory_tx,
            synced_tx,
            shutdown,
        };
        (rig, supervisor)
    }

    /// Poll `supervisor` for `duration` of (paused) time, without spawning.
    async fn run_for(supervisor: &mut Pin<&mut impl Future<Output = ()>>, duration: Duration) {
        tokio::select! {
            _ = supervisor.as_mut() => panic!("the supervisor stopped early"),
            _ = tokio::time::sleep(duration) => {}
        }
    }

    /// Poll `supervisor` in small steps until `done` holds or `limit` of
    /// (paused) time has passed. File writes run on the blocking pool,
    /// which paused time doesn't wait for, so a test that writes the fence
    /// polls for the outcome rather than assuming it landed in one step.
    async fn run_until(
        supervisor: &mut Pin<&mut impl Future<Output = ()>>,
        limit: Duration,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        let step = Duration::from_millis(10);
        let mut waited = Duration::ZERO;
        while !done() {
            if waited >= limit {
                return false;
            }
            run_for(supervisor, step).await;
            waited += step;
        }
        true
    }

    #[tokio::test(start_paused = true)]
    async fn probe_window_starts_after_gossip_joins() {
        let (rig, supervisor) = rig(probing_fence(None));
        tokio::pin!(supervisor);

        // Gossip hasn't synced: no amount of waiting ends the probe.
        run_for(&mut supervisor, Duration::from_secs(120)).await;
        assert_eq!(rig.fence.snapshot().state, FenceState::Probing);

        // Once it syncs, the window runs from that moment.
        rig.synced_tx.send_replace(true);
        run_for(&mut supervisor, WINDOW - Duration::from_secs(1)).await;
        assert_eq!(rig.fence.snapshot().state, FenceState::Probing);
        run_for(&mut supervisor, Duration::from_secs(2)).await;
        assert_eq!(rig.fence.snapshot().state, FenceState::Serving);
        rig.shutdown.cancel();
        supervisor.await;
    }

    #[tokio::test(start_paused = true)]
    async fn probe_ends_early_only_when_every_peer_advertised() {
        let (rig, supervisor) = rig(probing_fence(None));
        tokio::pin!(supervisor);
        rig.synced_tx.send_replace(true);

        // One of two peers heard from: the window still governs.
        rig.directory_tx.send_replace(directory(&[("n2", Some(0))]));
        run_for(&mut supervisor, Duration::from_secs(5)).await;
        assert_eq!(rig.fence.snapshot().state, FenceState::Probing);

        // Both heard from, neither newer: serve now, well inside the window.
        rig.directory_tx
            .send_replace(directory(&[("n2", Some(0)), ("n3", Some(0))]));
        run_for(&mut supervisor, Duration::from_millis(10)).await;
        assert_eq!(rig.fence.snapshot().state, FenceState::Serving);
        rig.shutdown.cancel();
        supervisor.await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_epoch_heard_during_the_probe_fences_the_node() {
        let (rig, supervisor) = rig(probing_fence(None));
        tokio::pin!(supervisor);
        rig.directory_tx.send_replace(directory(&[("n2", Some(1))]));
        run_for(&mut supervisor, Duration::from_millis(10)).await;
        assert_eq!(rig.fence.snapshot().fenced_by(), Some(1));
        rig.shutdown.cancel();
        supervisor.await;
    }

    // #1273: the supervisor read the persisted membership (openraft's
    // metrics watch) while it still held its borrow of the gossip directory.
    // The leader-target maintainer takes the same two locks the other way
    // round, metrics first. Watch locks make a reader wait for a queued
    // writer, so with openraft and gossip each waiting to publish, the four
    // tasks blocked one another for good. On a four-core runner that was
    // every worker thread, and the upgraded node never answered again.
    // The membership read must find the directory free to write.
    #[tokio::test]
    async fn the_probe_reads_the_membership_without_holding_the_gossip_directory() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let (directory_tx, directory_rx) = watch::channel(NodeDirectory::default());
        let directory_tx = Arc::new(directory_tx);
        let (_synced_tx, gossip_synced_rx) = watch::channel(false);
        let shutdown = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let blocked = Arc::new(AtomicBool::new(false));

        let writer = Arc::clone(&directory_tx);
        let probe_calls = Arc::clone(&calls);
        let probe_blocked = Arc::clone(&blocked);
        let peers = move || {
            if probe_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                // Gossip publishes from another thread while the membership
                // is being read. A held directory borrow stalls it.
                let (done_tx, done_rx) = std::sync::mpsc::channel();
                let writer = Arc::clone(&writer);
                std::thread::spawn(move || {
                    writer.send_replace(NodeDirectory::default());
                    let _ = done_tx.send(());
                });
                if done_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                    probe_blocked.store(true, Ordering::SeqCst);
                }
            }
            Some(peers(&["n2", "n3"]))
        };
        let supervisor = supervise_recovery_fence(
            probing_fence(None),
            peers,
            FenceSupervisorInputs {
                directory_rx,
                gossip_synced_rx,
                probe_window: WINDOW,
            },
            shutdown.clone(),
        );
        tokio::pin!(supervisor);
        let probed = run_until(&mut supervisor, Duration::from_secs(5), || {
            calls.load(Ordering::SeqCst) > 0
        })
        .await;
        assert!(probed, "the probe never read the membership");
        assert!(
            !blocked.load(Ordering::SeqCst),
            "a gossip directory write waited for the membership read"
        );
        shutdown.cancel();
        supervisor.await;
    }

    #[tokio::test(start_paused = true)]
    async fn failed_fence_write_is_retried() {
        let root = tempfile::tempdir().unwrap();
        let raft = root.path().join("raft");
        let file = raft.join(crate::council::fence::FENCE_FILE);
        let (rig, supervisor) = rig(probing_fence(Some(file.clone())));
        tokio::pin!(supervisor);

        // The fence moves while its directory is missing: the write fails,
        // and the failure is visible.
        rig.fence.observe_peer(RecoveryClaim { epoch: 1, id: None });
        let failed = run_until(&mut supervisor, Duration::from_secs(1), || {
            rig.fence.persist_failure().is_some()
        })
        .await;
        assert!(failed, "the failed write was never reported");
        assert!(!file.exists());

        // The disk recovers; the next retry writes the fence.
        std::fs::create_dir_all(&raft).unwrap();
        let retried = run_until(
            &mut supervisor,
            FENCE_PERSIST_RETRY + Duration::from_secs(5),
            || rig.fence.persist_failure().is_none(),
        )
        .await;
        assert!(retried, "the fence was never retried");
        assert!(file.exists());
        assert_eq!(rig.fence.persist_failure(), None);
        assert_eq!(
            RecoveryFence::read_fenced_by(&raft).unwrap(),
            Some(1),
            "the retried record must hold the fence"
        );
        rig.shutdown.cancel();
        supervisor.await;
    }
}
