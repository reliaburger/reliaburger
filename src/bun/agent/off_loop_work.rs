//! Disk and runtime work a turn starts and a later turn finishes (#351,
//! stage 3).
//!
//! Retiring an instance removes its identity directory (unmounting its tmpfs
//! first) and its adoption record; deploying an app with managed volumes
//! creates them; retiring a test lease removes its storage. Rerunning a job
//! kills its previous run first, and fencing an app whose stop failed kills
//! its instances. Each is milliseconds when the disk and the runtime are
//! healthy and seconds when they aren't (a kill waits for the runtime to
//! confirm the exit), and each sits in the middle of a sequence the loop
//! owns: retirement must not forget an owner whose key material is still on
//! disk, a launch must not start before its volumes exist, and a rerun must
//! not start before the run it replaces is gone.
//!
//! So the turn that reaches the work starts it in a task and waits for it
//! only until the turn's runtime budget runs out. If it finished, the turn
//! carries on as before. If not, the step fails with
//! [`BunError::StillRunning`] and whoever asked tries again: the tick on its
//! next pass, a deploy worker after a short sleep, a stop on its next check.
//! The next attempt finds the same task, so the work runs once however many
//! times it is asked about, and a step only goes past it once it is done.
//!
//! A task belongs to one incarnation of its instance. A result that was never
//! collected (the owner went another way) is thrown away rather than handed
//! to the next incarnation that happens to reuse the id.

use std::collections::HashMap;
use std::time::Instant;

use tokio::task::JoinHandle;

use super::{BunAgent, BunError, Grill, InstanceId};

/// Which work a task is doing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum WorkKey {
    /// Removing a retired instance's identity directory and adoption record.
    RetireArtifacts(InstanceId),
    /// Creating an app's managed volumes, or claiming its test storage, for
    /// the volumes a spec declares (`volumes` tells one spec from another).
    ProvisionStorage {
        namespace: String,
        app: String,
        volumes: String,
    },
    /// Removing a retired test lease's disposable storage.
    RetireTestStorage { namespace: String, app: String },
    /// Killing a job's previous run, and confirming its exit, before a
    /// rerun replaces it.
    ClearJobRun(InstanceId),
    /// Force-killing an instance whose graceful stop failed, and confirming
    /// its exit.
    #[cfg_attr(not(all(feature = "ebpf", target_os = "linux")), allow(dead_code))]
    FenceExecution(InstanceId),
}

impl std::fmt::Display for WorkKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkKey::RetireArtifacts(id) => {
                write!(f, "removing {id}'s identity directory and record")
            }
            WorkKey::ProvisionStorage { namespace, app, .. } => {
                write!(f, "provisioning {namespace}/{app}'s volumes")
            }
            WorkKey::RetireTestStorage { namespace, app } => {
                write!(f, "removing {namespace}/{app}'s test storage")
            }
            WorkKey::ClearJobRun(id) => write!(f, "killing {id}'s previous run"),
            WorkKey::FenceExecution(id) => write!(f, "force-killing {id}"),
        }
    }
}

/// One piece of work in flight, or finished and not yet collected.
struct InFlight {
    /// The instance's `created_at` when the work started, if it had one.
    incarnation: Option<Instant>,
    task: JoinHandle<Result<(), String>>,
}

/// Work in flight, by what it's doing.
#[derive(Default)]
pub(super) struct OffLoopWork {
    in_flight: HashMap<WorkKey, InFlight>,
}

impl OffLoopWork {
    /// Whether work for `key` is still running.
    #[cfg(test)]
    pub(super) fn is_running(&self, key: &WorkKey) -> bool {
        self.in_flight
            .get(key)
            .is_some_and(|work| !work.task.is_finished())
    }

    /// Whether `namespace/app`'s volumes are being provisioned, so a restore
    /// must not swap them meanwhile.
    pub(super) fn provisioning(&self, namespace: &str, app: &str) -> bool {
        self.in_flight.iter().any(|(key, work)| {
            matches!(key, WorkKey::ProvisionStorage { namespace: n, app: a, .. }
                if n == namespace && a == app)
                && !work.task.is_finished()
        })
    }

    /// Whether work for `key` was started for this incarnation, so the
    /// steps before it have already run.
    pub(super) fn started(&self, key: &WorkKey, incarnation: Option<Instant>) -> bool {
        self.in_flight
            .get(key)
            .is_some_and(|work| work.incarnation == incarnation)
    }

    /// The task doing `key`'s work for this incarnation, started with `work`
    /// if there is none yet, and whether it was started just now. `None`
    /// while an earlier incarnation's work is still running: the new one
    /// starts once that has finished.
    fn task<F>(
        &mut self,
        key: &WorkKey,
        incarnation: Option<Instant>,
        work: F,
    ) -> Option<(&mut JoinHandle<Result<(), String>>, bool)>
    where
        F: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        if let Some(stale) = self.in_flight.get(key)
            && stale.incarnation != incarnation
        {
            if !stale.task.is_finished() {
                return None;
            }
            self.in_flight.remove(key);
        }
        let started_now = !self.in_flight.contains_key(key);
        let work = self
            .in_flight
            .entry(key.clone())
            .or_insert_with(|| InFlight {
                incarnation,
                task: tokio::spawn(work),
            });
        Some((&mut work.task, started_now))
    }

    /// Stop every task's result from mattering, for shutdown. Blocking
    /// disk work can't be interrupted; it finishes on its own.
    pub(super) fn abandon_all(&mut self) {
        self.in_flight.clear();
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Run `key`'s work, starting it with `work` if this incarnation hasn't
    /// yet. A turn that starts the work waits for it until the turn's
    /// runtime budget runs out, so with a healthy disk and runtime the step
    /// finishes in the same turn; a later turn only collects a result that
    /// is already there. `Err(BunError::StillRunning)` means ask again later.
    pub(super) async fn finish_off_loop_work<F>(
        &mut self,
        key: WorkKey,
        incarnation: Option<Instant>,
        work: F,
    ) -> Result<Result<(), String>, BunError>
    where
        F: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let turn_deadline = self.turn_deadline();
        let still_running = || BunError::StillRunning {
            work: key.to_string(),
        };
        let Some((task, started_now)) = self.off_loop_work.task(&key, incarnation, work) else {
            return Err(still_running());
        };
        // `timeout_at` polls the task once even with the deadline passed, so
        // a finished result is always collected.
        let deadline = if started_now {
            turn_deadline
        } else {
            tokio::time::Instant::now()
        };
        let joined = tokio::time::timeout_at(deadline, task)
            .await
            .map_err(|_| still_running())?;
        self.off_loop_work.in_flight.remove(&key);
        Ok(joined.unwrap_or_else(|error| Err(format!("off-loop task failed: {error}"))))
    }

    /// The incarnation of `id` that work started now belongs to.
    pub(super) fn incarnation_of(&self, id: &InstanceId) -> Option<Instant> {
        self.supervisor
            .get_instance(id)
            .map(|instance| instance.created_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> WorkKey {
        WorkKey::RetireArtifacts(InstanceId("default__web-0".into()))
    }

    #[tokio::test]
    async fn the_same_incarnation_finds_the_task_it_started() {
        let mut work = OffLoopWork::default();
        let incarnation = Some(Instant::now());
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        assert!(
            work.task(&key(), incarnation, async move {
                let _ = wait.await;
                Ok(())
            })
            .is_some()
        );
        assert!(work.is_running(&key()));
        assert!(work.started(&key(), incarnation));
        // A second ask doesn't start the work again.
        assert!(matches!(
            work.task(&key(), incarnation, async { Err("ran twice".into()) }),
            Some((_, false))
        ));
        release.send(()).unwrap();
        let (task, started_now) = work
            .task(&key(), incarnation, async { Err("ran twice".into()) })
            .unwrap();
        assert!(!started_now);
        assert_eq!(task.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn a_new_incarnation_waits_for_the_old_work_then_starts_its_own() {
        let mut work = OffLoopWork::default();
        let old = Some(Instant::now());
        let new = Some(Instant::now() + std::time::Duration::from_secs(1));
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        work.task(&key(), old, async move {
            let _ = wait.await;
            Ok(())
        });
        assert!(
            work.task(&key(), new, async { Ok(()) }).is_none(),
            "the old incarnation's work is still running"
        );
        release.send(()).unwrap();
        while work.is_running(&key()) {
            tokio::task::yield_now().await;
        }
        let (task, started_now) = work
            .task(&key(), new, async { Err("the new incarnation's".into()) })
            .unwrap();
        assert!(started_now);
        assert_eq!(
            task.await.unwrap(),
            Err("the new incarnation's".into()),
            "the old result was handed to the new incarnation"
        );
        assert!(work.started(&key(), new));
        assert!(!work.started(&key(), old));
    }
}
