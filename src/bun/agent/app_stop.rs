//! Operator stops and retirements whose exit wait runs off the command loop.
//!
//! A workload that ignores SIGTERM keeps its stop waiting for the whole grace.
//! The loop therefore only withdraws routing and marks the instances Stopping,
//! then hands the wait to a task in `stop_waits`. When the task finishes, the
//! loop records the exit and releases ownership, so every state transition
//! still happens here, one at a time.

use std::collections::HashMap;

use tokio::sync::oneshot;

use super::*;
use super::{BunAgent, BunError, Grill, InstanceId};

/// A stop that has withdrawn routing and marked its instances Stopping.
#[derive(Clone)]
pub(super) struct AppStop {
    pub(super) instances: Vec<InstanceId>,
    /// Whether any instance is a recorded job whose phase must be committed.
    pub(super) owns_job: bool,
    /// Restarts this stop took its instances back from. The exit wait lets
    /// their in-flight runtime steps finish before it signals anything.
    pub(super) taken_restarts: Vec<super::restarts::TakenRestart>,
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
    /// The stop itself has finished; only what its waiters asked for after
    /// it (a retirement's disk cleanup) is still running off the loop.
    stopped: bool,
    /// Why the stop failed, kept while the execution fence it set off is
    /// still running off the loop, so the waiters hear it once it holds.
    failure: Option<BunError>,
}

/// How long a stop waits before it checks again on disk work still running
/// off the loop (#351, stage 3). Each check is one short turn.
const DISK_WORK_RECHECK: std::time::Duration = std::time::Duration::from_millis(100);

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
                stopped: false,
                failure: None,
            },
        );
    }

    /// Keep a stop pending while disk work it needs runs off the loop, and
    /// look again shortly. The exits are confirmed, so the wait is a timer,
    /// not another exit wait.
    fn recheck_stop_later(&mut self, key: (String, String), mut pending: PendingStop) {
        pending.task = self
            .stop_waits
            .spawn(async {
                tokio::time::sleep(DISK_WORK_RECHECK).await;
                Ok(())
            })
            .id();
        self.pending_stops.insert(key, pending);
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
        let Some(mut pending) = self.pending_stops.remove(&key) else {
            return;
        };
        let (app_name, namespace) = (key.0.clone(), key.1.clone());
        let (app_name, namespace) = (app_name.as_str(), namespace.as_str());
        let finished = match waited {
            Ok(()) if pending.stopped => pending.failure.take().map_or(Ok(()), Err),
            Ok(()) => {
                self.finish_app_stop(app_name, namespace, pending.stop.clone())
                    .await
            }
            Err(error) => Err(error),
        };
        // Retirement's disk cleanup runs off the loop; look again shortly,
        // rather than answer anyone before it has finished.
        if matches!(finished, Err(BunError::StillRunning { .. })) {
            self.recheck_stop_later(key, pending);
            return;
        }
        let first_attempt = !pending.stopped;
        pending.stopped = true;
        if let Err(error) = &finished
            && pending.fence_on_failure
        {
            if first_attempt {
                eprintln!("bun: stop of {namespace}/{app_name} failed, fencing execution: {error}");
            }
            // The fence's runtime work runs off the loop (#393). Nobody hears
            // about the stop until the fence holds, so a caller that sees
            // the failure never races an app that is still running.
            if let Err(BunError::StillRunning { .. }) =
                self.fence_after_failed_stop(app_name, namespace).await
            {
                pending.failure = finished.err();
                self.recheck_stop_later(key, pending);
                return;
            }
        }
        // The first waiter gets the error itself; later ones get its text.
        let reason = finished.as_ref().err().map(ToString::to_string);
        let mut error = finished.err();
        let mut still_waiting = Vec::new();
        for waiter in std::mem::take(&mut pending.waiters) {
            let result = match &reason {
                Some(reason) => Err(error
                    .take()
                    .unwrap_or_else(|| stop_incomplete(reason.clone()))),
                None => {
                    self.complete_purpose(app_name, namespace, waiter.purpose)
                        .await
                }
            };
            if matches!(result, Err(BunError::StillRunning { .. })) {
                still_waiting.push(waiter);
                continue;
            }
            let _ = waiter.response.send(result);
        }
        if !still_waiting.is_empty() {
            pending.waiters = still_waiting;
            self.recheck_stop_later(key, pending);
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

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Stop an app's instances, waiting for their exit inline.
    ///
    /// Operator stops, retirements and the egress fence all await the exit
    /// off the command loop instead (`request_app_stop`,
    /// `stop_app_unattended`). This inline form lets tests drive a whole stop
    /// without running the loop.
    #[cfg(test)]
    pub(super) async fn stop_app(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let stop = self.begin_app_stop(app_name, namespace).await?;
        self.app_exit_wait(&stop).await?;
        self.finish_app_stop(app_name, namespace, stop).await
    }

    /// Withdraw an app's routing and move its instances to Stopping.
    ///
    /// Nothing is signalled yet: `app_exit_wait` sends SIGTERM, waits out
    /// the grace and escalates, and `finish_app_stop` releases ownership only
    /// after that wait has confirmed every exit.
    pub(super) async fn begin_app_stop(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<AppStop, BunError> {
        // A schedule exists before its first instance. Retire future firings
        // even when there is no running process (or runtime cleanup fails).
        let mut next = self.scheduled_jobs.clone();
        let had_schedule = next
            .remove(&(app_name.to_string(), namespace.to_string()))
            .is_some();
        if had_schedule {
            self.commit_scheduled_jobs(next).await?;
        }
        // Get instance IDs for this app
        let instances: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.app_name == app_name && i.namespace == namespace)
            .map(|i| i.id.clone())
            .collect();

        if instances.is_empty() && !had_schedule {
            return Err(BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            });
        }

        let owns_job = instances
            .iter()
            .any(|id| self.recorded_jobs.contains_key(&id.0));
        let mut jobs = self.recorded_jobs.clone();
        for id in &instances {
            if let Some(job) = jobs.get_mut(&id.0)
                && job.phase != crate::bun::jobs::JobPhase::Unknown
            {
                job.phase = crate::bun::jobs::JobPhase::Stopping;
            }
        }
        if owns_job {
            self.commit_jobs(jobs).await?;
        }

        // Runtime retirement can release a reusable container address. Refuse
        // before that happens if an old VIP can still route to the address.
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        if !self
            .withdraw_backends_from_view(&service_id, &instances)
            .await?
        {
            self.withdraw_service_ebpf(&service_id).await?;
        }
        for id in &instances {
            let _ = self.service_map.remove_backend(&service_id, &id.0);
        }
        self.rebuild_routing_table().await;

        // Stop via supervisor (moves the tracked state to Stopping).
        if !instances.is_empty() {
            // LOOP-INLINE: in-memory lock, no I/O
            self.supervisor.stop_app(app_name, namespace).await?;
        }
        // A stop wins over a restart in flight: the restart won't touch the
        // runtime again, and the exit wait lets its current step finish.
        let taken_restarts = instances
            .iter()
            .filter_map(|id| self.take_back_from_restart(id))
            .collect();

        Ok(AppStop {
            instances,
            owns_job,
            taken_restarts,
        })
    }

    /// Take this node's `instances` out of the installed consumer view of
    /// `id`, kernel entry first, and keep every other backend. Returns false
    /// when this node has no consumer view of the service; the caller then
    /// withdraws the whole entry, which names only local backends.
    ///
    /// In a cluster the kernel entry for a VIP is the whole view, other
    /// nodes' backends included. Deleting it on a local stop refused every
    /// local client with `EPERM` until the next catalogue arrived (#481).
    /// A local rollout retires its old instances the same way.
    pub(super) async fn withdraw_backends_from_view(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
        instances: &[InstanceId],
    ) -> Result<bool, BunError> {
        if self.consumer_owner().is_none() {
            return Ok(false);
        }
        let mut view: Vec<_> = self
            .service_map_tx
            .borrow()
            .resolve_all()
            .into_iter()
            .cloned()
            .collect();
        let Some(entry) = view
            .iter_mut()
            .find(|entry| entry.namespace == id.namespace && entry.app_name == id.name)
        else {
            return Ok(false);
        };
        // Remote backends are never this stop's to remove, whatever their names.
        entry.backends.retain(|backend| {
            !(backend.local
                && instances
                    .iter()
                    .any(|stopping| stopping.0 == backend.instance_id))
        });
        let view =
            crate::onion::service_map::ServiceMap::from_snapshot(&view).map_err(|error| {
                BunError::BackendRetirement {
                    service: id.clone(),
                    reason: error.to_string(),
                }
            })?;
        // Only this service's entry changes, so only it is rewritten. Wrapper
        // keeps its routes until the next refresh rebuilds the whole view.
        self.publish_backend_kernel(id, &view).await?;
        self.service_map_tx.send_replace(view);
        self.mark_consumer_view_stale()?;
        Ok(true)
    }

    /// The exit wait for a begun stop, detached from `self` so it can run on
    /// a spawned task while the command loop keeps serving.
    ///
    /// DEP6: SIGTERM, wait for the runtime to confirm exit, escalate to
    /// SIGKILL on timeout. Only then may the caller record Stopped. Recording
    /// it before the process exits let container and supervisor state
    /// diverge — a "stopped" app whose process was still serving traffic.
    /// Every replica waits at once, so a stop costs one grace, not one each.
    pub(super) fn app_exit_wait(
        &self,
        stop: &AppStop,
    ) -> impl std::future::Future<Output = Result<(), BunError>> + Send + 'static {
        let ids: Vec<InstanceId> = stop
            .instances
            .iter()
            .filter(|id| {
                !self
                    .recorded_jobs
                    .get(&id.0)
                    .is_some_and(|job| job.runtime_absent)
            })
            .cloned()
            .collect();
        let grill = self.supervisor.grill().clone();
        let drains = self.drains.clone();
        let grace = self.stop_grace;
        let confirmation_timeout = self.stop_confirmation_timeout;
        let taken_restarts = stop.taken_restarts.clone();
        async move {
            restarts::settle_all(&taken_restarts, confirmation_timeout).await?;
            let waits = ids.iter().map(|id| {
                drain_and_stop_instance(&drains, &grill, id, grace, confirmation_timeout)
            });
            // Try every replica, but report the first failure: ownership and
            // enforcement stay until all exits are confirmed, and a later stop
            // can retry the incomplete cleanup.
            futures_util::future::join_all(waits)
                .await
                .into_iter()
                .find_map(Result::err)
                .map_or(Ok(()), Err)
        }
    }

    /// Record a stop whose exits are confirmed and release what it owned.
    pub(super) async fn finish_app_stop(
        &mut self,
        app_name: &str,
        namespace: &str,
        stop: AppStop,
    ) -> Result<(), BunError> {
        let AppStop {
            instances,
            owns_job,
            ..
        } = stop;
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);

        // Transition Stopping → Stopped now the exit is confirmed.
        for id in &instances {
            if let Some(instance) = self.supervisor.get_instance_mut(id)
                && instance.state == ContainerState::Stopping
            {
                let _ = instance
                    .state
                    .transition_to(ContainerState::Stopped)
                    .map(|s| {
                        instance.state = s;
                    });
            }
        }

        let mut jobs = self.recorded_jobs.clone();
        for id in &instances {
            if let Some(job) = jobs.get_mut(&id.0) {
                if job.phase != crate::bun::jobs::JobPhase::Unknown {
                    job.phase = crate::bun::jobs::JobPhase::Stopped;
                }
                job.runtime_absent = true;
            }
        }
        if owns_job {
            self.commit_jobs(jobs).await?;
        }

        // A failed artifact cleanup retains the empty service's key for retry.
        for id in &instances {
            self.retire_instance_artifacts(id).await?;
        }

        self.retire_discovery_service(&service_id).await?;
        let _ = self.service_map.unregister(&service_id);
        // NET5: prune this app's cgroup-namespace + firewall entries now it's
        // gone, so a reused cgroup inode can't inherit its isolation identity.
        self.sync_firewall_ebpf().await;
        self.ingress_configs
            .remove(&(namespace.to_string(), app_name.to_string()));
        self.rebuild_routing_table().await;

        self.record_event(
            crate::bun::events::EventKind::Stop,
            crate::bun::events::EventSeverity::Info,
            Some(app_name.to_string()),
            Some(namespace.to_string()),
            format!("stopped app {app_name}"),
        )
        .await;

        Ok(())
    }

    /// Restore what `finish_app_stop` released for an app whose stopped
    /// replicas are still owned, so a redeploy can publish into it again.
    ///
    /// Leaves a registered service and a stored route untouched: only a
    /// completed stop removes them while the replicas stay owned. The VIP is
    /// derived from the app's name, so the service comes back under the
    /// address it had before the stop, as the cluster catalogue keeps it.
    pub(super) async fn restore_stopped_routing(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        if let Some(port) = spec.port
            && self.service_map.resolve(&service_id).is_none()
        {
            let firewall = spec
                .firewall
                .as_ref()
                .filter(|firewall| !firewall.allow_from.is_empty())
                .map(|firewall| firewall.allow_from.clone());
            self.register_local_service(&service_id, port, firewall)?;
            self.publish_backend_ebpf(&service_id).await?;
            self.sync_firewall_ebpf().await;
        }
        if let Some(ingress) = &spec.ingress {
            self.ingress_configs
                .entry((namespace.to_string(), app_name.to_string()))
                .or_insert_with(|| ingress.clone());
        }
        Ok(())
    }
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
                    taken_restarts: Vec::new(),
                },
                task,
                waiters,
                fence_on_failure: false,
                stopped: false,
                failure: None,
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
