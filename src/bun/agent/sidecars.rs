//! Sidecars (#1120): containers that run beside an instance for its whole
//! life, in its network namespace.
//!
//! The ordering is the whole feature, so it lives here, in portable code the
//! mock runtime can drive:
//!
//! ```text
//!   create app ─▶ init containers ─▶ create + start sidecars ─▶ start app
//!   stop app ─▶ app exits ─▶ stop sidecars ─▶ retirement kills what's left
//! ```
//!
//! An instance's sidecars are registered before the first is created, so a
//! launch that fails half-way leaves nothing retirement can't find: the
//! retirement of an instance kills every sidecar it registered, after the
//! instance itself, the way it kills an instance's init containers.
//!
//! Two crash policies:
//!
//! - **A sidecar that exits is restarted on its own**, as Kubernetes does,
//!   with a backoff that doubles from nothing to a minute. The app keeps
//!   running and keeps its address meanwhile.
//! - **An app that exits restarts with its sidecars.** On runc the network
//!   namespace belongs to the app's instance and goes when it does, so a
//!   sidecar left running would be cut off from its app's successor. The
//!   restart stops the sidecars after the app, as any stop does, and starts
//!   them again before it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::app::SidecarSpec;
use crate::grill::oci::OciSpec;

/// The longest a crashed sidecar waits to be started again.
pub(super) const SIDECAR_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long a restarted sidecar must run before its next crash counts as
/// the first again: ten minutes, as a kubelet resets a container's back-off.
pub(super) const SIDECAR_STABLE_RUN: Duration = Duration::from_secs(600);

/// How long a sidecar waits before restart number `restarts + 1`: nothing
/// for the first, then 1 s, doubling to [`SIDECAR_MAX_BACKOFF`].
pub(super) fn sidecar_backoff(restarts: u32) -> Duration {
    if restarts == 0 {
        return Duration::ZERO;
    }
    let seconds = 1u64.checked_shl(restarts - 1).unwrap_or(u64::MAX);
    Duration::from_secs(seconds).min(SIDECAR_MAX_BACKOFF)
}

/// Whether an instance in `state` has started and not begun to stop, so
/// its sidecars should be running beside it. An unhealthy app keeps them:
/// a sidecar that's down may be why it's unhealthy.
pub(super) fn runs_beside(state: ContainerState) -> bool {
    matches!(
        state,
        ContainerState::HealthWait | ContainerState::Running | ContainerState::Unhealthy
    )
}

/// One sidecar's runtime launch: its identity and the spec it's created
/// from, with its `PROVIDER[...]` references still unresolved.
#[derive(Debug, Clone)]
pub(super) struct SidecarLaunch {
    pub(super) id: InstanceId,
    pub(super) spec: OciSpec,
}

/// The launches of `sidecars` beside instance `parent`, built from the
/// parent's stored spec, in the order the app declares them.
pub(super) fn sidecar_launches(
    sidecars: &[SidecarSpec],
    parent: &InstanceId,
    parent_spec: &OciSpec,
) -> Vec<SidecarLaunch> {
    sidecars
        .iter()
        .map(|sidecar| SidecarLaunch {
            id: crate::grill::oci::sidecar_instance_id(parent, &sidecar.name),
            spec: crate::grill::oci::generate_sidecar_oci_spec(sidecar, parent_spec, parent),
        })
        .collect()
}

/// Why launching a sidecar failed, and which one.
#[derive(Debug)]
pub(super) struct SidecarLaunchFailure {
    pub(super) sidecar: InstanceId,
    pub(super) error: Box<crate::grill::GrillError>,
}

/// Create and start each sidecar in turn, resolving its secrets first, as
/// any launch does (#1123). Stops at the first that fails.
///
/// Runs on a deploy worker or a restart step, never on the agent loop: a
/// create can pull an image.
pub(super) async fn launch_sidecars<G: Grill>(
    grill: &G,
    secrets: &crate::sesame::secret_provider::SecretProviders,
    app_name: &str,
    namespace: &str,
    launches: &[SidecarLaunch],
) -> Result<(), SidecarLaunchFailure> {
    for launch in launches {
        let target = super::launch::LaunchTarget {
            instance_id: &launch.id,
            app_name,
            namespace,
        };
        let failed = |error| SidecarLaunchFailure {
            sidecar: launch.id.clone(),
            error: Box::new(error),
        };
        super::launch::create_with_secrets(grill, secrets, target, &launch.spec)
            .await
            .map_err(failed)?;
        grill.start(&launch.id).await.map_err(failed)?;
    }
    Ok(())
}

/// Stop each sidecar of an instance whose own exit is confirmed, in the
/// order the app declares them: its stop signal, `grace` to exit, then the
/// force-kill.
///
/// Best effort, and only reported: a sidecar the runtime never launched or
/// already saw exit is skipped, and one whose stop fails is killed by the
/// instance's retirement, which never lets an instance go while a sidecar
/// it registered still runs.
pub(super) async fn stop_sidecars<G: Grill>(
    grill: &G,
    sidecars: &[InstanceId],
    grace: Duration,
    confirmation_timeout: Duration,
) {
    let stop = InstanceStop {
        grace,
        pre_stop: None,
        sidecars: Vec::new(),
        sidecar_grace: grace,
    };
    for id in sidecars {
        match tokio::time::timeout(confirmation_timeout, grill.state(id)).await {
            Ok(Ok(ContainerState::Stopped))
            | Ok(Err(crate::grill::GrillError::NotFound { .. })) => {
                continue;
            }
            _ => {}
        }
        if let Err(error) = stop_instance_alone(grill, id, &stop, confirmation_timeout).await {
            eprintln!("bun: {id}: sidecar stop not confirmed, retirement kills it: {error}");
        }
    }
}

/// Kill each sidecar and confirm its exit, after its instance's own. A
/// restart's Clear and Cleanup steps use it, and so does retirement.
pub(super) async fn kill_sidecars<G: Grill>(
    grill: &G,
    sidecars: &[InstanceId],
    confirmation_timeout: Duration,
) -> Result<(), BunError> {
    for id in sidecars {
        kill_runtime_instance(grill, id, confirmation_timeout).await?;
    }
    Ok(())
}

/// Start a crashed sidecar again, alone: kill what's left of its last
/// launch, so its identity is free, then create and start it. A
/// `cancelled` token (its instance stopped or crashed meanwhile) stops it
/// between steps, and a launch it already made is killed again.
async fn relaunch<G: Grill>(
    grill: G,
    secrets: crate::sesame::secret_provider::SecretProviders,
    app_name: String,
    namespace: String,
    launch: SidecarLaunch,
    cancelled: CancellationToken,
    confirmation_timeout: Duration,
) -> Result<(), String> {
    kill_runtime_instance(&grill, &launch.id, confirmation_timeout)
        .await
        .map_err(|error| error.to_string())?;
    if cancelled.is_cancelled() {
        return Err("its instance stopped first".into());
    }
    let started = launch_sidecars(
        &grill,
        &secrets,
        &app_name,
        &namespace,
        std::slice::from_ref(&launch),
    )
    .await
    .map_err(|failure| failure.error.to_string());
    if cancelled.is_cancelled() {
        // Whatever it started belongs to an instance that's going away.
        let _ = kill_runtime_instance(&grill, &launch.id, confirmation_timeout).await;
        return Err("its instance stopped first".into());
    }
    started
}

/// A sidecar's restart, finished, for the loop to record.
#[derive(Debug)]
pub(super) struct Relaunched {
    parent: InstanceId,
    sidecar: InstanceId,
    result: Result<(), String>,
}

/// One registered sidecar.
#[derive(Debug, Clone)]
struct SidecarRun {
    id: InstanceId,
    /// The definition its instance launched it from. A restart uses this,
    /// not the app's current spec, which a rollout replaces while old
    /// instances still run (#1434).
    spec: SidecarSpec,
    /// Restarts since it last started with its instance, or since it last
    /// ran for [`SIDECAR_STABLE_RUN`].
    restarts: u32,
    /// When its last restart alone succeeded.
    restarted_at: Option<Instant>,
    /// Set while it's down: when it may be started again.
    due: Option<Instant>,
    /// Set while a restart is in flight.
    relaunching: Option<CancellationToken>,
}

/// Every instance's sidecars on this node, and the restarts in flight.
#[derive(Debug, Default)]
pub(super) struct Sidecars {
    by_parent: HashMap<InstanceId, Vec<SidecarRun>>,
}

impl Sidecars {
    /// Record `specs` as `parent`'s sidecars, running from now on, and
    /// return their identities. Refused while a restart of one it had
    /// before is still in flight: that restart could still create it.
    pub(super) fn register(
        &mut self,
        parent: &InstanceId,
        specs: &[SidecarSpec],
    ) -> Result<Vec<InstanceId>, BunError> {
        self.register_with(parent, specs, None)
    }

    /// Record `specs` as `parent`'s sidecars, all down and due at `now`: an
    /// instance adopted after a Bun restart, whose old sidecars startup
    /// adoption retired because no record names them.
    pub(super) fn register_down(
        &mut self,
        parent: &InstanceId,
        specs: &[SidecarSpec],
        now: Instant,
    ) -> Result<Vec<InstanceId>, BunError> {
        self.register_with(parent, specs, Some(now))
    }

    fn register_with(
        &mut self,
        parent: &InstanceId,
        specs: &[SidecarSpec],
        due: Option<Instant>,
    ) -> Result<Vec<InstanceId>, BunError> {
        if self.relaunching(parent) {
            return Err(BunError::RetirementState {
                instance_id: parent.clone(),
                reason: "a sidecar's restart still owns its previous runtime".into(),
            });
        }
        if specs.is_empty() {
            self.by_parent.remove(parent);
            return Ok(Vec::new());
        }
        let runs: Vec<SidecarRun> = specs
            .iter()
            .map(|spec| SidecarRun {
                id: crate::grill::oci::sidecar_instance_id(parent, &spec.name),
                spec: spec.clone(),
                restarts: 0,
                restarted_at: None,
                due,
                relaunching: None,
            })
            .collect();
        let ids = runs.iter().map(|run| run.id.clone()).collect();
        self.by_parent.insert(parent.clone(), runs);
        Ok(ids)
    }

    /// The definitions `parent`'s sidecars were launched from, in the
    /// order the app declares them.
    pub(super) fn specs_of(&self, parent: &InstanceId) -> Vec<SidecarSpec> {
        self.by_parent
            .get(parent)
            .map(|runs| runs.iter().map(|run| run.spec.clone()).collect())
            .unwrap_or_default()
    }

    /// `parent`'s sidecars, in the order the app declares them.
    pub(super) fn of(&self, parent: &InstanceId) -> Vec<InstanceId> {
        self.by_parent
            .get(parent)
            .map(|runs| runs.iter().map(|run| run.id.clone()).collect())
            .unwrap_or_default()
    }

    /// The instance `sidecar` runs beside, if it's a registered sidecar.
    pub(super) fn parent_of(&self, sidecar: &InstanceId) -> Option<&InstanceId> {
        self.by_parent
            .iter()
            .find(|(_, runs)| runs.iter().any(|run| &run.id == sidecar))
            .map(|(parent, _)| parent)
    }

    /// Whether a restart of one of `parent`'s sidecars is in flight.
    pub(super) fn relaunching(&self, parent: &InstanceId) -> bool {
        self.by_parent
            .get(parent)
            .is_some_and(|runs| runs.iter().any(|run| run.relaunching.is_some()))
    }

    /// Ask every restart of `parent`'s sidecars to give up: the instance is
    /// stopping or has crashed.
    pub(super) fn cancel_relaunches(&mut self, parent: &InstanceId) {
        for run in self.by_parent.get_mut(parent).into_iter().flatten() {
            if let Some(token) = &run.relaunching {
                token.cancel();
            }
            run.due = None;
        }
    }

    /// `parent`'s sidecars started again with it (a restart of the whole
    /// instance): none is down any more.
    pub(super) fn started_with(&mut self, parent: &InstanceId) {
        for run in self.by_parent.get_mut(parent).into_iter().flatten() {
            run.due = None;
        }
    }

    /// Forget `parent`'s sidecars once retirement has confirmed they've
    /// exited, returning them.
    pub(super) fn forget(&mut self, parent: &InstanceId) -> Vec<InstanceId> {
        self.by_parent
            .remove(parent)
            .map(|runs| runs.into_iter().map(|run| run.id).collect())
            .unwrap_or_default()
    }

    /// Sidecars whose state a sweep should read: those that should be
    /// running beside a running instance, in a stable order.
    pub(super) fn running_beside(&self, parent: &InstanceId) -> Vec<InstanceId> {
        self.by_parent
            .get(parent)
            .map(|runs| {
                runs.iter()
                    .filter(|run| run.due.is_none() && run.relaunching.is_none())
                    .map(|run| run.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A sweep saw `sidecar` exit at `now`: it's due again after its backoff.
    /// Returns whether it was running until now.
    pub(super) fn mark_exited(&mut self, sidecar: &InstanceId, now: Instant) -> bool {
        let Some(run) = self.run_mut(sidecar) else {
            return false;
        };
        if run.due.is_some() || run.relaunching.is_some() {
            return false;
        }
        // Crashes far apart aren't a crash loop: the backoff starts over.
        if run
            .restarted_at
            .is_some_and(|at| now.saturating_duration_since(at) >= SIDECAR_STABLE_RUN)
        {
            run.restarts = 0;
        }
        run.due = Some(now + sidecar_backoff(run.restarts));
        true
    }

    /// Sidecars due a restart at `now`, with their instances.
    pub(super) fn due(&self, now: Instant) -> Vec<(InstanceId, InstanceId)> {
        let mut due: Vec<_> = self
            .by_parent
            .iter()
            .flat_map(|(parent, runs)| {
                runs.iter()
                    .filter(move |run| {
                        run.relaunching.is_none() && run.due.is_some_and(|due| due <= now)
                    })
                    .map(move |run| (parent.clone(), run.id.clone()))
            })
            .collect();
        due.sort_by(|a, b| a.1.0.cmp(&b.1.0));
        due
    }

    /// A restart of `sidecar` has begun, cancellable through `token`.
    pub(super) fn begin_relaunch(&mut self, sidecar: &InstanceId, token: CancellationToken) {
        if let Some(run) = self.run_mut(sidecar) {
            run.relaunching = Some(token);
        }
    }

    /// A restart of `sidecar` has ended at `now`. A failed one is due again
    /// after a longer backoff; either way it counts.
    pub(super) fn finish_relaunch(&mut self, sidecar: &InstanceId, succeeded: bool, now: Instant) {
        let Some(run) = self.run_mut(sidecar) else {
            return;
        };
        let cancelled = run
            .relaunching
            .take()
            .is_some_and(|token| token.is_cancelled());
        run.restarts = run.restarts.saturating_add(1);
        run.restarted_at = succeeded.then_some(now);
        run.due = (!succeeded && !cancelled).then(|| now + sidecar_backoff(run.restarts));
    }

    /// How many times `sidecar` has been restarted alone.
    pub(super) fn restarts(&self, sidecar: &InstanceId) -> u32 {
        self.by_parent
            .values()
            .flatten()
            .find(|run| &run.id == sidecar)
            .map_or(0, |run| run.restarts)
    }

    /// Every registered sidecar, for shutdown.
    pub(super) fn all(&self) -> Vec<InstanceId> {
        self.by_parent
            .values()
            .flatten()
            .map(|run| run.id.clone())
            .collect()
    }

    fn run_mut(&mut self, sidecar: &InstanceId) -> Option<&mut SidecarRun> {
        self.by_parent
            .values_mut()
            .flatten()
            .find(|run| &run.id == sidecar)
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Register `parent`'s sidecars, with the definitions it launches them
    /// from, before the first is created. Answers the deploy worker's
    /// `RegisterSidecars`.
    pub(super) fn register_sidecars(
        &mut self,
        parent: &InstanceId,
        specs: &[SidecarSpec],
    ) -> Result<Vec<InstanceId>, BunError> {
        if self.supervisor.get_instance(parent).is_none() {
            return Err(BunError::InstanceNotFound {
                instance_id: parent.clone(),
            });
        }
        self.sidecars.register(parent, specs)
    }

    /// The launches of `parent`'s sidecars from the definitions it
    /// registered and its stored launch spec. Never from the app's current
    /// spec: mid-rollout that's the new one, while `parent` is old (#1434).
    pub(super) fn sidecar_launches_of(
        &self,
        parent: &InstanceId,
        parent_spec: &OciSpec,
    ) -> Vec<SidecarLaunch> {
        sidecar_launches(&self.sidecars.specs_of(parent), parent, parent_spec)
    }

    /// Start every crashed sidecar whose backoff is over and whose instance
    /// is still running, each in its own task; the task reports back as a
    /// [`follow_ups::FollowUp::SidecarRelaunched`].
    pub(super) fn relaunch_due_sidecars(&mut self) {
        for (parent, sidecar) in self.sidecars.due(Instant::now()) {
            let Some(instance) = self.supervisor.get_instance(&parent) else {
                continue;
            };
            if !runs_beside(instance.state) {
                continue;
            }
            let Some(parent_spec) = instance.oci_spec.clone() else {
                continue;
            };
            let (app_name, namespace) = (instance.app_name.clone(), instance.namespace.clone());
            let Some(launch) = self
                .sidecar_launches_of(&parent, &parent_spec)
                .into_iter()
                .find(|launch| launch.id == sidecar)
            else {
                continue;
            };
            let token = CancellationToken::new();
            self.sidecars.begin_relaunch(&sidecar, token.clone());
            let grill = self.supervisor.grill().clone();
            let secrets = self.secret_providers.clone();
            let confirmation = self.stop_confirmation_timeout;
            self.follow_ups.spawn(async move {
                let result = relaunch(
                    grill,
                    secrets,
                    app_name,
                    namespace,
                    launch,
                    token,
                    confirmation,
                )
                .await;
                follow_ups::FollowUp::SidecarRelaunched(Relaunched {
                    parent,
                    sidecar,
                    result,
                })
            });
        }
    }

    /// Record a sidecar's restart: the count, and an event either way.
    pub(super) async fn finish_sidecar_relaunch(&mut self, relaunched: Relaunched) {
        let Relaunched {
            parent,
            sidecar,
            result,
        } = relaunched;
        self.sidecars
            .finish_relaunch(&sidecar, result.is_ok(), Instant::now());
        // Its new cgroup is bound to its app's namespace on the next tick
        // (#1426).
        if result.is_ok() {
            self.namespace_firewall_stale = true;
        }
        let Some(instance) = self.supervisor.get_instance(&parent) else {
            return;
        };
        let (app, namespace) = (instance.app_name.clone(), instance.namespace.clone());
        let (severity, message) = match result {
            Ok(()) => (
                crate::bun::events::EventSeverity::Warning,
                format!(
                    "sidecar {sidecar} of {parent} restarted (restart {})",
                    self.sidecars.restarts(&sidecar)
                ),
            ),
            Err(reason) => (
                crate::bun::events::EventSeverity::Warning,
                format!("sidecar {sidecar} of {parent} failed to restart: {reason}"),
            ),
        };
        self.record_event(
            crate::bun::events::EventKind::Restart,
            severity,
            Some(app),
            Some(namespace),
            message,
        )
        .await;
    }

    /// Kill `parent`'s sidecars and forget them, after the instance itself
    /// has exited: the last step that can find one. A restart of one still
    /// in flight is cancelled, and the retirement asks again once it has
    /// ended, so a late create can't outlive it.
    pub(super) async fn retire_sidecars(&mut self, parent: &InstanceId) -> Result<(), BunError> {
        if self.sidecars.relaunching(parent) {
            self.sidecars.cancel_relaunches(parent);
            return Err(BunError::StillRunning {
                work: format!("a restart of a sidecar of {parent}"),
            });
        }
        let sidecars = self.sidecars.of(parent);
        if sidecars.is_empty() {
            return Ok(());
        }
        let deadline = self.turn_deadline();
        tokio::time::timeout_at(
            deadline,
            kill_sidecars(
                self.supervisor.grill(),
                &sidecars,
                self.stop_confirmation_timeout,
            ),
        )
        .await
        .map_err(|_| BunError::StopUnconfirmed {
            instance_id: parent.clone(),
            reason: "sidecar exit was not confirmed within the turn",
        })??;
        self.sidecars.forget(parent);
        // Their runtime metadata goes too; the runtime refuses while a
        // successor under the same identity owns anything.
        let grill = self.supervisor.grill().clone();
        tokio::spawn(async move {
            for sidecar in sidecars {
                if let Err(error) = grill.forget_retired(&sidecar).await {
                    eprintln!("bun: {sidecar}: runtime metadata kept: {error}");
                }
            }
        });
        Ok(())
    }

    /// Run every due sidecar restart to its end, the way the loop's
    /// follow-up branch would. For tests that drive the agent without
    /// running its loop.
    #[cfg(test)]
    pub(super) async fn relaunch_due_sidecars_to_completion(&mut self) {
        self.relaunch_due_sidecars();
        while let Some(done) = self.follow_ups.join_next_with_id().await {
            self.apply_follow_up(done).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> InstanceId {
        InstanceId(value.to_string())
    }

    /// The proxy sidecar's definition; its identity beside `web-0` is
    /// `web-0__sidecar-proxy`.
    fn proxy_spec() -> Vec<SidecarSpec> {
        let app: AppSpec =
            toml::from_str("image = \"a:1\"\n[[sidecar]]\nname = \"proxy\"\n").unwrap();
        app.sidecar
    }

    #[test]
    fn the_first_restart_is_at_once_and_later_ones_back_off_to_a_minute() {
        assert_eq!(sidecar_backoff(0), Duration::ZERO);
        assert_eq!(sidecar_backoff(1), Duration::from_secs(1));
        assert_eq!(sidecar_backoff(2), Duration::from_secs(2));
        assert_eq!(sidecar_backoff(5), Duration::from_secs(16));
        assert_eq!(sidecar_backoff(7), SIDECAR_MAX_BACKOFF);
        assert_eq!(sidecar_backoff(200), SIDECAR_MAX_BACKOFF);
    }

    #[test]
    fn a_crashing_sidecar_is_due_again_after_a_growing_backoff() {
        let mut sidecars = Sidecars::default();
        let (parent, proxy) = (id("web-0"), id("web-0__sidecar-proxy"));
        sidecars.register(&parent, &proxy_spec()).unwrap();
        let now = Instant::now();

        assert!(sidecars.mark_exited(&proxy, now));
        assert!(!sidecars.mark_exited(&proxy, now), "already down");
        assert_eq!(sidecars.due(now), vec![(parent.clone(), proxy.clone())]);
        assert!(sidecars.running_beside(&parent).is_empty());

        sidecars.begin_relaunch(&proxy, CancellationToken::new());
        assert!(sidecars.due(now).is_empty(), "one restart at a time");
        assert!(sidecars.relaunching(&parent));
        sidecars.finish_relaunch(&proxy, true, now);
        assert_eq!(sidecars.restarts(&proxy), 1);
        assert_eq!(sidecars.running_beside(&parent), vec![proxy.clone()]);

        // The second crash waits a second, not at once.
        assert!(sidecars.mark_exited(&proxy, now));
        assert!(sidecars.due(now).is_empty());
        assert_eq!(sidecars.due(now + Duration::from_secs(1)).len(), 1);

        // A failed restart is due again after the next, longer backoff.
        sidecars.begin_relaunch(&proxy, CancellationToken::new());
        sidecars.finish_relaunch(&proxy, false, now);
        assert!(sidecars.due(now + Duration::from_secs(1)).is_empty());
        assert_eq!(sidecars.due(now + Duration::from_secs(2)).len(), 1);
    }

    /// #1442: the backoff counts crashes close together. A sidecar that has
    /// then run for ten minutes since its last restart starts at once again,
    /// as a kubelet resets its back-off.
    #[test]
    fn a_sidecar_that_ran_stably_restarts_at_once_again() {
        let mut sidecars = Sidecars::default();
        let (parent, proxy) = (id("web-0"), id("web-0__sidecar-proxy"));
        sidecars.register(&parent, &proxy_spec()).unwrap();
        let mut now = Instant::now();
        for _ in 0..7 {
            assert!(sidecars.mark_exited(&proxy, now));
            now += SIDECAR_MAX_BACKOFF;
            sidecars.begin_relaunch(&proxy, CancellationToken::new());
            sidecars.finish_relaunch(&proxy, true, now);
        }

        // A crash soon after still waits the longest backoff...
        let soon = now + Duration::from_secs(60);
        assert!(sidecars.mark_exited(&proxy, soon));
        assert!(sidecars.due(soon).is_empty());
        sidecars.begin_relaunch(&proxy, CancellationToken::new());
        sidecars.finish_relaunch(&proxy, true, soon);

        // ...but one after ten quiet minutes doesn't.
        let later = soon + Duration::from_secs(600);
        assert!(sidecars.mark_exited(&proxy, later));
        assert_eq!(sidecars.due(later), vec![(parent.clone(), proxy.clone())]);
    }

    #[test]
    fn a_cancelled_restart_is_not_due_again() {
        let mut sidecars = Sidecars::default();
        let (parent, proxy) = (id("web-0"), id("web-0__sidecar-proxy"));
        sidecars.register(&parent, &proxy_spec()).unwrap();
        let now = Instant::now();
        sidecars.mark_exited(&proxy, now);
        let token = CancellationToken::new();
        sidecars.begin_relaunch(&proxy, token.clone());

        sidecars.cancel_relaunches(&parent);
        assert!(token.is_cancelled());
        assert!(
            sidecars.register(&parent, &proxy_spec()).is_err(),
            "a restart in flight could still create the old sidecar"
        );
        sidecars.finish_relaunch(&proxy, false, now);
        assert!(sidecars.due(now + SIDECAR_MAX_BACKOFF).is_empty());
        sidecars.register(&parent, &proxy_spec()).unwrap();
    }

    #[test]
    fn an_adopted_instances_sidecars_are_due_at_once() {
        let mut sidecars = Sidecars::default();
        let (parent, proxy) = (id("web-0"), id("web-0__sidecar-proxy"));
        let now = Instant::now();
        sidecars.register_down(&parent, &proxy_spec(), now).unwrap();
        assert_eq!(sidecars.due(now), vec![(parent.clone(), proxy.clone())]);
        assert_eq!(sidecars.parent_of(&proxy), Some(&parent));
        assert_eq!(sidecars.forget(&parent), vec![proxy.clone()]);
        assert_eq!(sidecars.parent_of(&proxy), None);
    }
}
