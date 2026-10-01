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

use super::*;
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
        // A job's exit code the runtime couldn't read is unknown, not "none":
        // the next sweep asks again rather than settling the outcome (#389).
        Ok(ContainerState::Stopped) if read.is_job => match grill.exit_code(&read.id).await {
            Ok(exit_code) => Observed::Exited { exit_code },
            Err(_) => Observed::Unknown,
        },
        Ok(ContainerState::Stopped) => Observed::Exited { exit_code: None },
        Ok(_) => Observed::Alive,
        Err(_) => Observed::Unknown,
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// The instances whose runtime state a sweep should read: running apps
    /// (to catch a crash that no health check would) and running jobs that
    /// haven't recorded an exit yet, filtered by `include`.
    pub(super) fn plan_state_reads(
        &self,
        include: impl Fn(&WorkloadInstance) -> bool,
    ) -> Vec<state_sweep::StateRead> {
        self.supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| {
                instance.state == ContainerState::Running
                    && (!instance.is_job
                        || self
                            .recorded_jobs
                            .get(&instance.id.0)
                            .is_some_and(|job| job.phase == crate::bun::jobs::JobPhase::Launching))
                    && include(instance)
            })
            .map(|instance| state_sweep::StateRead {
                id: instance.id.clone(),
                incarnation: state_sweep::Incarnation::of(instance),
                is_job: instance.is_job,
            })
            .collect()
    }

    /// Start a sweep of every running app's and job's runtime state, unless
    /// the last one hasn't reported yet. The reads run off the loop; the
    /// sweep's `select!` branch applies what they saw.
    pub(super) fn begin_state_sweep(&mut self) {
        if !self.state_sweeps.is_empty() {
            return;
        }
        let reads = self.plan_state_reads(|_| true);
        if reads.is_empty() {
            return;
        }
        let grill = self.supervisor.grill().clone();
        self.state_sweeps
            .spawn(state_sweep::sweep_states(grill, reads));
    }

    /// Apply a finished sweep: every app or job it saw exit goes through
    /// the restart or job-outcome path, if it's still the incarnation the
    /// sweep read and still Running.
    pub(super) async fn apply_state_sweep(
        &mut self,
        sweep: Result<state_sweep::StateSweep, tokio::task::JoinError>,
    ) {
        let sweep = match sweep {
            Ok(sweep) => sweep,
            Err(error) => {
                eprintln!("bun: runtime state sweep failed: {error}");
                return;
            }
        };
        for (read, observed) in sweep.observations {
            let state_sweep::Observed::Exited { exit_code } = observed else {
                continue;
            };
            let current = self
                .supervisor
                .get_instance(&read.id)
                .is_some_and(|instance| {
                    instance.state == ContainerState::Running
                        && state_sweep::Incarnation::of(instance) == read.incarnation
                });
            if !current {
                continue;
            }
            if read.is_job {
                self.observe_job_exit(&read.id, exit_code).await;
            } else {
                self.observe_app_exit(&read.id).await;
            }
        }
    }
}
