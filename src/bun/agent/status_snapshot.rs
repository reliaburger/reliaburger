//! The status snapshot the agent loop publishes (#351, stage 2).
//!
//! `relish status` and `/v1/status` used to queue a `Status` command and wait
//! for the loop to answer it, so every status poll waited for the turn in
//! progress and then spent up to 500 ms of loop time on runtime reads. Now
//! the loop publishes what it knows after every turn through a
//! `tokio::sync::watch` channel, and a [`StatusReader`] answers from the
//! latest snapshot without asking the loop at all.
//!
//! Two rules keep the answer honest:
//!
//! 1. **Freshness is bounded.** A snapshot older than
//!    [`STATUS_SNAPSHOT_MAX_AGE`] doesn't answer anything. The reader waits
//!    for the next publication instead, and if none comes within
//!    [`STATUS_FRESHNESS_WAIT`] it fails the request the way a timed-out
//!    `Status` command used to. Every answer carries its snapshot's age in
//!    `status_age_ms`.
//! 2. **The runtime has the last word on liveness.** The pid and exit-code
//!    reads that used to run on the loop now run in the reader, and for an
//!    instance the loop last saw alive they also ask the runtime for its
//!    state. A container that has exited is reported `stopped` even before
//!    the loop's next health tick notices, so a dead instance is never
//!    reported `running` for longer than it was when the loop answered.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use tokio::sync::watch;

use super::{
    ContainerState, Grill, InstanceId, InstanceStatus, STATUS_RUNTIME_READ_CONCURRENCY,
    STATUS_RUNTIME_READ_TIMEOUT,
};

/// The oldest snapshot that may answer a status request. The loop publishes
/// after every turn, and an idle loop still turns once a second for its
/// health tick, so a live loop's snapshot is younger than this unless a turn
/// runs over the 1 s budget.
pub(crate) const STATUS_SNAPSHOT_MAX_AGE: Duration = Duration::from_secs(2);

/// How long a request waits for a fresh snapshot when the latest one is too
/// old. It plus the runtime reads stays under the 5 s that callers (the
/// cluster fan-out, `relish`) give a node to answer.
pub(crate) const STATUS_FRESHNESS_WAIT: Duration = Duration::from_secs(4);

/// What the loop knew about every instance at the end of one turn.
#[derive(Debug)]
pub(crate) struct StatusSnapshot {
    /// `None` until the loop has run: the agent is still adopting what it
    /// finds on disk, so it has nothing true to say yet.
    published_at: Option<tokio::time::Instant>,
    entries: Vec<StatusEntry>,
}

/// One instance, as the loop saw it, plus how to complete it at read time.
#[derive(Debug)]
pub(super) struct StatusEntry {
    /// Everything but the runtime evidence (`pid`, `exit_code`,
    /// `runtime_unknown`, `status_age_ms`), which the reader fills in.
    pub(super) status: InstanceStatus,
    pub(super) evidence: EvidenceSource,
}

/// Where a status answer's runtime evidence for one instance comes from.
#[derive(Debug, Clone, Copy)]
pub(super) enum EvidenceSource {
    /// Still being created: it has no process yet, and asking the runtime
    /// would wait for the create, image pull included (Z6.7).
    Creating { exit_code: Option<i32> },
    /// Ask the runtime. `recorded_exit` is a job outcome already on record,
    /// which needs no exit-code read. `alive` is set when the loop last saw
    /// the instance with a process, so the reader confirms that it still
    /// has one.
    Runtime {
        recorded_exit: Option<Option<i32>>,
        alive: bool,
    },
}

impl StatusSnapshot {
    /// A snapshot of `entries`, published now.
    pub(super) fn new(entries: Vec<StatusEntry>) -> Self {
        Self {
            published_at: Some(tokio::time::Instant::now()),
            entries,
        }
    }

    /// The placeholder an agent holds before its loop first publishes. It
    /// is never fresh, so readers wait for the real thing.
    pub(super) fn unpublished() -> Self {
        Self {
            published_at: None,
            entries: Vec::new(),
        }
    }

    /// How long ago the loop published this; `Duration::MAX` if it hasn't.
    pub(crate) fn age(&self) -> Duration {
        self.published_at
            .map_or(Duration::MAX, |published| published.elapsed())
    }
}

/// Why a status request got no answer.
#[derive(Debug, thiserror::Error)]
pub enum StatusUnavailable {
    /// The agent loop has stopped; nothing will publish again.
    #[error("agent unavailable")]
    AgentStopped,
    /// The loop hasn't published for too long: it is stuck in a turn, or
    /// hasn't started yet.
    #[error("agent status timed out: the last published status is {} ms old", age.as_millis())]
    Stale { age: Duration },
}

/// Reads the runtime evidence for a snapshot. It's a boxed closure so that
/// the reader doesn't carry the agent's `Grill` type parameter into the API.
type EvidenceReader =
    Arc<dyn Fn(Arc<StatusSnapshot>) -> BoxFuture<'static, Vec<InstanceStatus>> + Send + Sync>;

/// Answers status requests from the snapshot the agent loop publishes.
/// Cheap to clone; every clone watches the same loop.
#[derive(Clone)]
pub struct StatusReader {
    snapshots: watch::Receiver<Arc<StatusSnapshot>>,
    read_evidence: EvidenceReader,
}

impl std::fmt::Debug for StatusReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusReader")
            .field("age", &self.snapshots.borrow().age())
            .finish_non_exhaustive()
    }
}

impl StatusReader {
    /// A reader of `snapshots` that asks `grill` for the runtime evidence.
    pub(super) fn new<G: Grill + Clone + 'static>(
        snapshots: watch::Receiver<Arc<StatusSnapshot>>,
        grill: G,
    ) -> Self {
        let read_evidence: EvidenceReader = Arc::new(move |snapshot| {
            let grill = grill.clone();
            Box::pin(async move { read_status(&grill, &snapshot).await })
        });
        Self {
            snapshots,
            read_evidence,
        }
    }

    /// Every instance's status, from a snapshot no older than
    /// [`STATUS_SNAPSHOT_MAX_AGE`], completed with the runtime's evidence.
    ///
    /// Waits up to [`STATUS_FRESHNESS_WAIT`] for a fresh snapshot, and fails
    /// rather than answer from an older one.
    pub async fn read(&self) -> Result<Vec<InstanceStatus>, StatusUnavailable> {
        let deadline = tokio::time::Instant::now() + STATUS_FRESHNESS_WAIT;
        let mut snapshots = self.snapshots.clone();
        loop {
            let snapshot = Arc::clone(&snapshots.borrow_and_update());
            if snapshot.age() <= STATUS_SNAPSHOT_MAX_AGE {
                return Ok((self.read_evidence)(snapshot).await);
            }
            match tokio::time::timeout_at(deadline, snapshots.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(StatusUnavailable::AgentStopped),
                Err(_) => {
                    return Err(StatusUnavailable::Stale {
                        age: snapshot.age(),
                    });
                }
            }
        }
    }
}

/// The published statuses, with every instance's runtime evidence read under
/// one shared deadline, [`STATUS_RUNTIME_READ_TIMEOUT`], at most
/// [`STATUS_RUNTIME_READ_CONCURRENCY`] at a time. A read that hasn't
/// answered by then leaves its part to what the loop knew and sets
/// `runtime_unknown`; the reads that did answer are still reported.
pub(super) async fn read_status<G: Grill>(
    grill: &G,
    snapshot: &StatusSnapshot,
) -> Vec<InstanceStatus> {
    let deadline = tokio::time::Instant::now() + STATUS_RUNTIME_READ_TIMEOUT;
    let age_ms = u64::try_from(snapshot.age().as_millis()).unwrap_or(u64::MAX);
    // Built up front: a lazy `map` over borrowed entries leaves the compiler
    // unable to prove the stream `Send` for every lifetime it could see.
    let reads: Vec<_> = snapshot
        .entries
        .iter()
        .map(|entry| complete_entry(grill, entry, deadline))
        .collect();
    // `buffered` keeps the snapshot's order while running reads side by side.
    futures_util::stream::iter(reads)
        .buffered(STATUS_RUNTIME_READ_CONCURRENCY)
        .map(|mut status| {
            status.status_age_ms = Some(age_ms);
            status
        })
        .collect()
        .await
}

/// One instance's published status with its runtime evidence filled in.
async fn complete_entry<G: Grill>(
    grill: &G,
    entry: &StatusEntry,
    deadline: tokio::time::Instant,
) -> InstanceStatus {
    let mut status = entry.status.clone();
    let (recorded_exit, alive) = match entry.evidence {
        EvidenceSource::Creating { exit_code } => {
            status.exit_code = exit_code;
            return status;
        }
        EvidenceSource::Runtime {
            recorded_exit,
            alive,
        } => (recorded_exit, alive),
    };
    let id = InstanceId(status.id.clone());
    // The three reads run side by side, so the liveness check doesn't eat
    // into the deadline the pid and exit code had before it existed. Each
    // has its own timeout, so one that misses the deadline doesn't take the
    // others' answers with it: runc serialises an instance's calls, and a
    // liveness check stuck behind the health sweep's must not hide a pid
    // that answered (#358). `timeout_at` polls once even past the deadline,
    // so a read that answers at once is never marked unknown.
    let exited = async {
        if !alive {
            return Ok(false);
        }
        tokio::time::timeout_at(deadline, grill.state(&id))
            .await
            .map(|state| matches!(state, Ok(ContainerState::Stopped)))
    };
    let pid = tokio::time::timeout_at(deadline, grill.pid(&id));
    let exit_code = async {
        match recorded_exit {
            Some(code) => Ok(code),
            None => tokio::time::timeout_at(deadline, grill.exit_code(&id)).await,
        }
    };
    let (exited, pid, exit_code) = tokio::join!(exited, pid, exit_code);
    status.runtime_unknown = exited.is_err() || pid.is_err() || exit_code.is_err();
    // A missing liveness verdict leaves the loop's view of the state.
    let exited = exited.unwrap_or(false);
    if exited {
        // The loop hasn't noticed yet; its next tick will.
        status.state = ContainerState::Stopped.to_string();
    }
    // An exited instance has no process, whatever the pid read saw first.
    status.pid = pid.ok().flatten().filter(|_| !exited);
    status.exit_code = exit_code.unwrap_or(recorded_exit.flatten());
    status
}
