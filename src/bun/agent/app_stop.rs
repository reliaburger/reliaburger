//! Operator stops and retirements whose exit wait runs off the command loop.
//!
//! A workload that ignores SIGTERM keeps its stop waiting for the whole grace.
//! The loop therefore only withdraws routing and marks the instances Stopping,
//! then hands the wait to a task in `stop_waits`. When the task finishes, the
//! loop records the exit and releases ownership, so every state transition
//! still happens here, one at a time.

use std::collections::HashMap;

use tokio::sync::oneshot;

use super::{BunAgent, BunError, Grill, InstanceId};

/// A stop that has withdrawn routing and marked its instances Stopping.
pub(super) struct AppStop {
    pub(super) instances: Vec<InstanceId>,
    /// Whether any instance is a recorded job whose phase must be committed.
    pub(super) owns_job: bool,
}

/// What a caller wants done once a stop has confirmed every exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopPurpose {
    /// Only stop: the stopped instances stay owned.
    Stop,
    /// Stop, then forget the workload's ownership.
    Retire,
    /// Retire, then remove the lease's disposable managed storage.
    RetireTestResources,
}

/// One caller waiting on a pending stop.
struct StopWaiter {
    purpose: StopPurpose,
    response: oneshot::Sender<Result<(), BunError>>,
}

/// A stop whose exit wait is still running.
pub(super) struct PendingStop {
    stop: AppStop,
    task: tokio::task::Id,
    waiters: Vec<StopWaiter>,
}

/// Pending stops by (app, namespace).
pub(super) type PendingStops = HashMap<(String, String), PendingStop>;

/// The outcome of one exit wait, as `JoinSet::join_next_with_id` yields it.
pub(super) type StopWaitOutcome =
    Result<(tokio::task::Id, Result<(), BunError>), tokio::task::JoinError>;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Admit a stop or retirement and answer it once every exit is confirmed.
    ///
    /// A request for a workload that is already stopping joins that stop
    /// instead of signalling it again.
    pub(super) async fn request_app_stop(
        &mut self,
        app_name: String,
        namespace: String,
        purpose: StopPurpose,
        response: oneshot::Sender<Result<(), BunError>>,
    ) {
        let key = (app_name, namespace);
        if let Some(pending) = self.pending_stops.get_mut(&key) {
            pending.waiters.push(StopWaiter { purpose, response });
            return;
        }
        let (app_name, namespace) = (&key.0, &key.1);
        let begun = match self.refuse_while_deploying(app_name, namespace).await {
            Ok(()) => self.begin_app_stop(app_name, namespace).await,
            Err(error) => Err(error),
        };
        match begun {
            Ok(stop) => {
                let wait = self.app_exit_wait(&stop);
                let task = self.stop_waits.spawn(wait).id();
                let waiters = vec![StopWaiter { purpose, response }];
                self.pending_stops.insert(
                    key,
                    PendingStop {
                        stop,
                        task,
                        waiters,
                    },
                );
            }
            // Retirement is idempotent: nothing left to stop is still
            // ownership to forget.
            Err(BunError::AppNotFound { .. }) if purpose != StopPurpose::Stop => {
                let result = self.complete_purpose(app_name, namespace, purpose).await;
                let _ = response.send(result);
            }
            Err(error) => {
                let _ = response.send(Err(error));
            }
        }
    }

    /// Record a finished exit wait and answer everyone waiting on it.
    pub(super) async fn complete_app_stop(&mut self, outcome: StopWaitOutcome) {
        let (task, waited) = match outcome {
            Ok((task, waited)) => (task, waited),
            Err(error) => (error.id(), Err(stop_incomplete(error.to_string()))),
        };
        let Some(key) = self
            .pending_stops
            .iter()
            .find(|(_, pending)| pending.task == task)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        let Some(pending) = self.pending_stops.remove(&key) else {
            return;
        };
        let (app_name, namespace) = (&key.0, &key.1);
        let finished = match waited {
            Ok(()) => {
                self.finish_app_stop(app_name, namespace, pending.stop)
                    .await
            }
            Err(error) => Err(error),
        };
        // The first waiter gets the error itself; later ones get its text.
        let reason = finished.as_ref().err().map(ToString::to_string);
        let mut error = finished.err();
        for waiter in pending.waiters {
            let result = match &reason {
                Some(reason) => Err(error
                    .take()
                    .unwrap_or_else(|| stop_incomplete(reason.clone()))),
                None => {
                    self.complete_purpose(app_name, namespace, waiter.purpose)
                        .await
                }
            };
            let _ = waiter.response.send(result);
        }
    }

    /// Do what a caller asked for after a confirmed stop.
    async fn complete_purpose(
        &mut self,
        app_name: &str,
        namespace: &str,
        purpose: StopPurpose,
    ) -> Result<(), BunError> {
        match purpose {
            StopPurpose::Stop => Ok(()),
            StopPurpose::Retire => self.release_retired_workload(app_name, namespace).await,
            StopPurpose::RetireTestResources => {
                self.release_retired_workload(app_name, namespace).await?;
                self.retire_test_storage(app_name, namespace).await
            }
        }
    }

    /// Stop waiting on exits when the agent shuts down.
    ///
    /// Node shutdown SIGTERMs and force-kills every instance itself. Callers
    /// are told the stop is unconfirmed, so they keep what they own and retry
    /// after restart, when recovery finds the instances again.
    pub(super) fn abandon_pending_stops(&mut self) {
        self.stop_waits.abort_all();
        for (_, pending) in self.pending_stops.drain() {
            for waiter in pending.waiters {
                let _ = waiter.response.send(Err(stop_incomplete(
                    "the agent shut down before exit was confirmed".into(),
                )));
            }
        }
    }

    /// The first workload in `config` that is still stopping, if any.
    pub(super) fn stopping_target(
        &self,
        config: &crate::config::Config,
    ) -> Option<crate::bun::deploy_operations::DeployTarget> {
        crate::bun::deploy_operations::targets(config)
            .into_iter()
            .find(|target| {
                self.pending_stops
                    .contains_key(&(target.name.clone(), target.namespace.clone()))
            })
    }
}

fn stop_incomplete(reason: String) -> BunError {
    BunError::StopIncomplete { reason }
}
