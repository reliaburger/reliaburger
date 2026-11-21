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
//! exit only to the same incarnation, still in a state whose exit it
//! watches ([`watches_exit`]).
//!
//! Each read runs in a task of its own. One still running at the deadline
//! is handed back as unfinished, and the next sweep waits on it again
//! rather than asking the runtime a second time (#2085): on runc a read the
//! sweep stopped waiting for kept running and held the instance's lifecycle
//! lock, so a fresh read queued behind it and missed its deadline as well,
//! and an exit on a runtime slower than the deadline was never seen.

use std::time::{Duration, Instant};

use futures_util::StreamExt;

use super::*;
use super::{ContainerState, Grill, InstanceId, WorkloadInstance};

/// At most this many runtime state reads run at once in one sweep.
pub(super) const STATE_READ_CONCURRENCY: usize = 8;

/// How long one sweep may read for. A read still running then comes back
/// `Unknown`, and the next sweep collects it. A new sweep starts only when
/// the last one has reported, so this also bounds how long the loop waits
/// between looks at a crash the runtime reports promptly.
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

/// Whether the sweep watches an app in `state` for its process exiting.
/// A `HealthWait` app's process is started, so a crash before its first
/// healthy probe is an exit like any other: no probe would notice, because
/// a startup probe that fails only counts towards giving up.
pub(super) fn watches_exit(state: ContainerState) -> bool {
    matches!(state, ContainerState::HealthWait | ContainerState::Running)
}

/// One instance the sweep asks the runtime about.
#[derive(Debug, Clone)]
pub(super) struct StateRead {
    pub(super) id: InstanceId,
    pub(super) incarnation: Incarnation,
}

/// What the runtime said about one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Observed {
    /// The process is still there.
    Alive,
    /// The process has exited.
    Exited,
    /// The runtime didn't answer, or not before the deadline.
    Unknown,
}

/// Reads a sweep was still waiting for at its deadline, by instance, each
/// with the incarnation it reads.
pub(super) type UnfinishedReads =
    std::collections::HashMap<InstanceId, (Incarnation, tokio::task::JoinHandle<Observed>)>;

/// Everything one sweep saw, in the order it was planned.
#[derive(Debug)]
pub(super) struct StateSweep {
    pub(super) observations: Vec<(StateRead, Observed)>,
    /// The reads still running at the deadline, for the next sweep.
    pub(super) unfinished: UnfinishedReads,
}

/// Read every planned instance's state, [`STATE_READ_CONCURRENCY`] at a
/// time, waiting for whatever hasn't answered only until
/// [`STATE_SWEEP_DEADLINE`]. A read of the same incarnation that an earlier
/// sweep left `unfinished` is waited on again instead of started afresh;
/// one for an instance no longer planned, or since restarted, is cancelled.
pub(super) async fn sweep_states<G: Grill + Clone + 'static>(
    grill: G,
    reads: Vec<StateRead>,
    mut unfinished: UnfinishedReads,
) -> StateSweep {
    let deadline = tokio::time::Instant::now() + STATE_SWEEP_DEADLINE;
    let reads: Vec<_> = reads
        .into_iter()
        .map(|read| {
            let earlier = unfinished
                .remove(&read.id)
                .filter(|(incarnation, _)| *incarnation == read.incarnation)
                .map(|(_, task)| task);
            (read, earlier)
        })
        .collect();
    for (_, task) in unfinished.into_values() {
        task.abort();
    }
    let results: Vec<_> = futures_util::stream::iter(reads)
        .map(|(read, earlier)| {
            let grill = grill.clone();
            async move {
                let mut task = earlier.unwrap_or_else(|| {
                    let id = read.id.clone();
                    tokio::spawn(async move { observe(&grill, &id).await })
                });
                match tokio::time::timeout_at(deadline, &mut task).await {
                    Ok(observed) => (read, observed.unwrap_or(Observed::Unknown), None),
                    Err(_) => (read, Observed::Unknown, Some(task)),
                }
            }
        })
        .buffered(STATE_READ_CONCURRENCY)
        .collect()
        .await;
    let mut sweep = StateSweep {
        observations: Vec::with_capacity(results.len()),
        unfinished: UnfinishedReads::new(),
    };
    for (read, observed, task) in results {
        if let Some(task) = task {
            sweep
                .unfinished
                .insert(read.id.clone(), (read.incarnation, task));
        }
        sweep.observations.push((read, observed));
    }
    sweep
}

async fn observe<G: Grill>(grill: &G, id: &InstanceId) -> Observed {
    match grill.state(id).await {
        Ok(ContainerState::Stopped) => Observed::Exited,
        Ok(_) => Observed::Alive,
        Err(_) => Observed::Unknown,
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// The instances whose runtime state a sweep should read: started apps
    /// (to catch a crash that no health check would), filtered by `include`,
    /// and the sidecars that should be running beside a started instance
    /// (#1120), each read under its instance's incarnation.
    pub(super) fn plan_state_reads(
        &self,
        include: impl Fn(&WorkloadInstance) -> bool,
    ) -> Vec<state_sweep::StateRead> {
        let instances: Vec<&WorkloadInstance> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| include(instance))
            .collect();
        let apps = instances
            .iter()
            .filter(|instance| state_sweep::watches_exit(instance.state))
            .map(|instance| state_sweep::StateRead {
                id: instance.id.clone(),
                incarnation: state_sweep::Incarnation::of(instance),
            });
        let sidecars = instances
            .iter()
            .filter(|instance| sidecars::runs_beside(instance.state))
            .flat_map(|instance| {
                self.sidecars
                    .running_beside(&instance.id)
                    .into_iter()
                    .map(|id| state_sweep::StateRead {
                        id,
                        incarnation: state_sweep::Incarnation::of(instance),
                    })
            });
        apps.chain(sidecars).collect()
    }

    /// Start a sweep of every started app's runtime state, unless
    /// the last one hasn't reported yet. The reads run off the loop; the
    /// sweep's `select!` branch applies what they saw.
    pub(super) fn begin_state_sweep(&mut self) {
        if !self.state_sweeps.is_empty() {
            return;
        }
        let reads = self.plan_state_reads(|_| true);
        if reads.is_empty() {
            for (_, task) in std::mem::take(&mut self.unfinished_state_reads).into_values() {
                task.abort();
            }
            return;
        }
        let grill = self.supervisor.grill().clone();
        let unfinished = std::mem::take(&mut self.unfinished_state_reads);
        self.state_sweeps
            .spawn(state_sweep::sweep_states(grill, reads, unfinished));
    }

    /// Apply a finished sweep: every app it saw exit goes through the
    /// restart path, if it's still the incarnation the sweep read and still
    /// in a state whose exit the sweep watches.
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
        self.unfinished_state_reads = sweep.unfinished;
        for (read, observed) in sweep.observations {
            let state_sweep::Observed::Exited = observed else {
                continue;
            };
            // A sidecar that exited restarts on its own, while its instance
            // still runs as the read saw it (#1120).
            if let Some(parent) = self.sidecars.parent_of(&read.id).cloned() {
                let current = self
                    .supervisor
                    .get_instance(&parent)
                    .is_some_and(|instance| {
                        sidecars::runs_beside(instance.state)
                            && state_sweep::Incarnation::of(instance) == read.incarnation
                    });
                if current {
                    self.sidecars.mark_exited(&read.id, Instant::now());
                }
                continue;
            }
            let current = self
                .supervisor
                .get_instance(&read.id)
                .is_some_and(|instance| {
                    state_sweep::watches_exit(instance.state)
                        && state_sweep::Incarnation::of(instance) == read.incarnation
                });
            if !current {
                continue;
            }
            self.observe_app_exit(&read.id).await;
        }
    }
}
