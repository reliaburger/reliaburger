//! Bounded, parallel runtime state reads for the health tick (#351, stage 2).
//!
//! The tick used to ask the runtime for every running app's and job's state
//! one at a time, inline. With runc that is a fork and exec per instance, so
//! a hundred instances held the loop for seconds, and one read stuck behind
//! a slow create held it for as long as the create took. Now the tick plans
//! the reads and spawns them as one sweep: up to [`STATE_READ_CONCURRENCY`]
//! at a time, all under one [`STATE_SWEEP_DEADLINE`]. The loop applies what
//! the sweep saw when it reports back through its own `select!` branch.
//!
//! A result can land after the instance it describes has moved on, so each
//! read carries the instance's [`Incarnation`], and the loop applies an
//! exit only to the same incarnation, still `Running`.

use std::time::{Duration, Instant};

use futures_util::StreamExt;

use super::{ContainerState, Grill, InstanceId, WorkloadInstance};

/// At most this many runtime state reads run at once in one sweep.
pub(super) const STATE_READ_CONCURRENCY: usize = 8;

/// How long one sweep may read for. A read still running then is dropped,
/// and the next sweep asks again. A new sweep starts only when the last one
/// has reported, so this also bounds how stale a crash can get before the
/// loop sees it.
pub(super) const STATE_SWEEP_DEADLINE: Duration = Duration::from_secs(1);

/// Which run of an instance a read was about. A restart bumps
/// `restart_count`; a fresh deploy under the same id gets a new
/// `created_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Incarnation {
    created_at: Instant,
    restart_count: u32,
}

impl Incarnation {
    pub(super) fn of(instance: &WorkloadInstance) -> Self {
        Self {
            created_at: instance.created_at,
            restart_count: instance.restart_count,
        }
    }
}

/// One instance the sweep asks the runtime about.
#[derive(Debug, Clone)]
pub(super) struct StateRead {
    pub(super) id: InstanceId,
    pub(super) incarnation: Incarnation,
    pub(super) is_job: bool,
}

/// What the runtime said about one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Observed {
    /// The process is still there.
    Alive,
    /// The process has exited. A job's exit code comes with it, when the
    /// runtime kept one.
    Exited { exit_code: Option<i32> },
    /// The runtime didn't answer, or not before the deadline.
    Unknown,
}

/// Everything one sweep saw, in the order it was planned.
#[derive(Debug)]
pub(super) struct StateSweep {
    pub(super) observations: Vec<(StateRead, Observed)>,
}

/// Read every planned instance's state, [`STATE_READ_CONCURRENCY`] at a
/// time, giving up on whatever hasn't answered by [`STATE_SWEEP_DEADLINE`].
pub(super) async fn sweep_states<G: Grill>(grill: G, reads: Vec<StateRead>) -> StateSweep {
    let deadline = tokio::time::Instant::now() + STATE_SWEEP_DEADLINE;
    let grill = &grill;
    let observations = futures_util::stream::iter(reads)
        .map(|read| async move {
            // `timeout_at` polls once even past the deadline, so a runtime
            // that answers at once always counts.
            let observed = tokio::time::timeout_at(deadline, observe(grill, &read))
                .await
                .unwrap_or(Observed::Unknown);
            (read, observed)
        })
        .buffered(STATE_READ_CONCURRENCY)
        .collect()
        .await;
    StateSweep { observations }
}

async fn observe<G: Grill>(grill: &G, read: &StateRead) -> Observed {
    match grill.state(&read.id).await {
        Ok(ContainerState::Stopped) if read.is_job => Observed::Exited {
            exit_code: grill.exit_code(&read.id).await,
        },
        Ok(ContainerState::Stopped) => Observed::Exited { exit_code: None },
        Ok(_) => Observed::Alive,
        Err(_) => Observed::Unknown,
    }
}
