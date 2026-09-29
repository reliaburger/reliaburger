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
    /// Fence the app's execution at once if the stop fails: the egress
    /// fence relies on this stop and must not wait for its next tick.
    fence_on_failure: bool,
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
                let waiters = vec![StopWaiter { purpose, response }];
                self.start_exit_wait(key, stop, waiters, false);
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

    /// Stop an app that lost its egress enforcement, without holding the
    /// loop for its grace. A stop already pending for the app is marked to
    /// fence the app if it fails, so the fallback runs as soon as it does.
    ///
    /// An error means the stop couldn't begin; the caller fences at once.
    #[cfg(any(test, all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn stop_app_unattended(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let key = (app_name.to_string(), namespace.to_string());
        if let Some(pending) = self.pending_stops.get_mut(&key) {
            pending.fence_on_failure = true;
            return Ok(());
        }
        let stop = self.begin_app_stop(app_name, namespace).await?;
        self.start_exit_wait(key, stop, Vec::new(), true);
        Ok(())
    }

    /// Hand a begun stop's exit wait to `stop_waits` and remember who waits.
    fn start_exit_wait(
        &mut self,
        key: (String, String),
        stop: AppStop,
        waiters: Vec<StopWaiter>,
        fence_on_failure: bool,
    ) {
        let wait = self.app_exit_wait(&stop);
        let task = self.stop_waits.spawn(wait).id();
        self.pending_stops.insert(
            key,
            PendingStop {
                stop,
                task,
                waiters,
                fence_on_failure,
            },
        );
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
        if let Err(error) = &finished
            && pending.fence_on_failure
        {
            eprintln!("bun: stop of {namespace}/{app_name} failed, fencing execution: {error}");
            self.fence_after_failed_stop(app_name, namespace).await;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::mock::MockGrill;
    use crate::grill::port::PortAllocator;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// A panicking exit wait still answers its callers, so none waits forever.
    #[tokio::test]
    async fn a_panicked_exit_wait_reports_the_stop_incomplete() {
        let (_tx, rx) = mpsc::channel(1);
        let mut agent = BunAgent::new(
            MockGrill::new(),
            PortAllocator::new(30000, 31000),
            rx,
            CancellationToken::new(),
        );
        let (first, first_reply) = oneshot::channel();
        let (second, second_reply) = oneshot::channel();
        let waiters = vec![
            StopWaiter {
                purpose: StopPurpose::Stop,
                response: first,
            },
            StopWaiter {
                purpose: StopPurpose::Retire,
                response: second,
            },
        ];
        let task = agent
            .stop_waits
            .spawn(async { panic!("injected exit-wait panic") })
            .id();
        agent.pending_stops.insert(
            ("web".into(), "default".into()),
            PendingStop {
                stop: AppStop {
                    instances: Vec::new(),
                    owns_job: false,
                },
                task,
                waiters,
                fence_on_failure: false,
            },
        );

        let outcome = agent.stop_waits.join_next_with_id().await.unwrap();
        agent.complete_app_stop(outcome).await;

        for reply in [first_reply, second_reply] {
            let result = reply.await.expect("a waiter was dropped unanswered");
            assert!(
                matches!(result, Err(BunError::StopIncomplete { .. })),
                "{result:?}"
            );
        }
        assert!(agent.pending_stops.is_empty());
    }
}
