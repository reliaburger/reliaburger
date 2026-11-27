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

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::app::SidecarSpec;
use crate::grill::oci::OciSpec;

/// The longest a crashed sidecar waits to be started again.
pub(super) const SIDECAR_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long a restarted sidecar must run before its next crash counts as
/// the first again: the same ten minutes that forgive an app's restarts.
pub(super) const SIDECAR_STABLE_RUN: Duration = crate::bun::restart::RESTART_FORGIVEN_AFTER;

/// How long a sidecar waits before restart number `restarts + 1`: nothing
/// for the first, then 1 s, doubling to [`SIDECAR_MAX_BACKOFF`].
pub(super) fn sidecar_backoff(restarts: u32) -> Duration {
    if restarts == 0 {
        return Duration::ZERO;
    }
    let seconds = 1u64.checked_shl(restarts - 1).unwrap_or(u64::MAX);
    Duration::from_secs(seconds).min(SIDECAR_MAX_BACKOFF)
}

/// `at` on the wall clock, in milliseconds since the Unix epoch, given that
/// the monotonic `now` is `wall_now`. A record keeps times this way because
/// an `Instant` means nothing to the next Bun process (#2336).
fn to_unix_ms(at: Instant, now: Instant, wall_now: SystemTime) -> Option<u64> {
    let wall = if at >= now {
        wall_now.checked_add(at - now)?
    } else {
        wall_now.checked_sub(now - at)?
    };
    let since_epoch = wall.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    u64::try_from(since_epoch.as_millis()).ok()
}

/// The `Instant` of a wall-clock time a record kept, the inverse of
/// [`to_unix_ms`]. `None` for one the monotonic clock can't express.
fn from_unix_ms(unix_ms: u64, now: Instant, wall_now: SystemTime) -> Option<Instant> {
    let wall = SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(unix_ms))?;
    match wall.duration_since(wall_now) {
        Ok(ahead) => now.checked_add(ahead),
        Err(behind) => now.checked_sub(behind.duration()),
    }
}

/// What a sidecar's status says while its instance is in `state` (#2336).
/// Sidecars start before their app, so while it's still being prepared or
/// started they're `starting`; only beside a started instance does each
/// say what it's doing itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SidecarPhase {
    /// The instance is being prepared or started, and its sidecars with it.
    Starting,
    /// The instance has started and not begun to stop.
    Beside,
    /// The instance hasn't started, is stopping, or has stopped.
    Stopped,
}

impl SidecarPhase {
    /// The phase of the sidecars of an instance in `state`.
    pub(super) fn of(state: ContainerState) -> Self {
        match state {
            ContainerState::Preparing | ContainerState::Initialising | ContainerState::Starting => {
                Self::Starting
            }
            ContainerState::HealthWait | ContainerState::Running | ContainerState::Unhealthy => {
                Self::Beside
            }
            ContainerState::Pending
            | ContainerState::Stopping
            | ContainerState::Stopped
            | ContainerState::Failed => Self::Stopped,
        }
    }
}

/// Whether an instance in `state` has started and not begun to stop, so
/// its sidecars should be running beside it. An unhealthy app keeps them:
/// a sidecar that's down may be why it's unhealthy.
pub(super) fn runs_beside(state: ContainerState) -> bool {
    SidecarPhase::of(state) == SidecarPhase::Beside
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

/// Kill sidecar `id` and confirm its exit, where the runtime still knows
/// it. A runtime that has forgotten it (runc, after its retirement
/// deleted the launch's intent) answers `NotFound`: nothing of that launch
/// is left to kill, so its identity is free (review of #2269).
pub(super) async fn kill_unless_forgotten<G: Grill>(
    grill: &G,
    id: &InstanceId,
    confirmation_timeout: Duration,
) -> Result<(), BunError> {
    match kill_runtime_instance(grill, id, confirmation_timeout).await {
        Err(BunError::Grill(crate::grill::GrillError::NotFound { .. })) => Ok(()),
        other => other,
    }
}

/// Why a sidecar's restart alone didn't start it.
#[derive(Debug)]
pub(super) enum RelaunchFailure {
    /// The node gate refused it before the runtime saw it: the host paths
    /// it shares with its app are no longer allowed (#2335). Asking again
    /// fails the same way, so it's parked until its app next restarts.
    Refused(String),
    /// Anything else, which a later attempt may get past.
    Failed(String),
}

impl std::fmt::Display for RelaunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelaunchFailure::Refused(reason) | RelaunchFailure::Failed(reason) => {
                f.write_str(reason)
            }
        }
    }
}

/// What a sidecar's restart alone needs besides the runtime and the launch.
struct RelaunchContext {
    secrets: crate::sesame::secret_provider::SecretProviders,
    app_name: String,
    namespace: String,
    /// The node's host-path policy, to approve the launch's host paths again
    /// off the loop (#2335).
    host_paths: crate::bun::host_paths::HostPathPolicy,
    cancelled: CancellationToken,
    confirmation_timeout: Duration,
}

/// Start a crashed sidecar again, alone: ask the node gate about the host
/// paths it mounts, kill what's left of its last launch, so its identity is
/// free, then create and start it. A `cancelled` token (its instance
/// stopped or crashed meanwhile) stops it between steps, and a launch it
/// already made is killed again.
async fn relaunch<G: Grill>(
    grill: G,
    launch: SidecarLaunch,
    context: RelaunchContext,
) -> Result<(), RelaunchFailure> {
    let RelaunchContext {
        secrets,
        app_name,
        namespace,
        host_paths,
        cancelled,
        confirmation_timeout,
    } = context;
    // The sidecar's spec carries its app's mounts as the deploy approved
    // them, and `node.toml` may have dropped one since (#2335). The check
    // is bounded as the kill after it is.
    super::restarts::recheck_host_paths(&host_paths, &launch.spec, confirmation_timeout)
        .await
        .map_err(|denial| match denial {
            super::restarts::HostPathDenial::Refused(refusal) => {
                RelaunchFailure::Refused(super::restarts::restart_refusal(
                    &format!("restart of sidecar {}", launch.id),
                    &refusal,
                ))
            }
            super::restarts::HostPathDenial::Unanswered(reason) => {
                RelaunchFailure::Failed(format!("the host path check didn't finish: {reason}"))
            }
        })?;
    let failed = |reason: String| RelaunchFailure::Failed(reason);
    // A sidecar startup retired is one runc has forgotten.
    kill_unless_forgotten(&grill, &launch.id, confirmation_timeout)
        .await
        .map_err(|error| failed(error.to_string()))?;
    if cancelled.is_cancelled() {
        return Err(failed("its instance stopped first".into()));
    }
    let started = launch_sidecars(
        &grill,
        &secrets,
        &app_name,
        &namespace,
        std::slice::from_ref(&launch),
    )
    .await
    .map_err(|failure| failed(failure.error.to_string()));
    if cancelled.is_cancelled() {
        // Whatever it started belongs to an instance that's going away.
        let _ = kill_runtime_instance(&grill, &launch.id, confirmation_timeout).await;
        return Err(failed("its instance stopped first".into()));
    }
    started
}

/// Whose sidecars a batch of adoption records describes: the instance's
/// own identity fields, which each sidecar's record repeats.
#[derive(Debug, Clone)]
pub(super) struct SidecarOwner {
    pub(super) namespace: String,
    pub(super) app_name: String,
    /// The instance's image, for a sidecar that runs the app's.
    pub(super) image: String,
}

/// How long recording one sidecar waits for the runtime to name its
/// process and logs.
const SIDECAR_RECORD_TIMEOUT: Duration = Duration::from_secs(10);

/// Write the adoption record of `launch`, a sidecar that has just started
/// (#1315), into `directory`. It names the sidecar's process the way an
/// instance's record names the instance's, so a restarted Bun can prove the
/// process it finds is this launch's before adopting it.
async fn record_sidecar<G: Grill>(
    grill: &G,
    directory: std::path::PathBuf,
    owner: SidecarOwner,
    replica_index: u32,
    launch: SidecarLaunch,
    image: String,
    restarts: Option<crate::grill::records::SidecarRestarts>,
) -> Result<(), String> {
    let (pid, log_stem) = tokio::time::timeout(SIDECAR_RECORD_TIMEOUT, async {
        let pid = grill.pid(&launch.id).await;
        (pid, grill.log_stem(&launch.id).await)
    })
    .await
    .map_err(|_| "the runtime didn't name its process in time".to_string())?;
    let pid = pid
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the runtime names no process for it".to_string())?;
    let runtime = grill.runtime_kind_for(&launch.spec);
    tokio::task::spawn_blocking(move || {
        use crate::grill::records;
        let pid_started_at = records::process_start_time(pid)
            .ok_or_else(|| "its process exited before it was recorded".to_string())?;
        let record = records::InstanceRecord {
            schema: records::RECORD_SCHEMA,
            runc_container_id: matches!(runtime, records::RuntimeKind::Runc)
                .then(|| launch.id.0.clone()),
            instance_id: launch.id.0,
            namespace: owner.namespace,
            app_name: owner.app_name,
            replica_index,
            image,
            runtime,
            pid,
            pid_started_at,
            boot_id: records::current_boot(),
            log_stem,
            host_port: None,
            // The instance's own record carries the app spec, and with it
            // this sidecar's definition.
            app_spec: None,
            oci_spec: launch.spec,
            rootless_network: None,
            sidecar_restarts: restarts,
        };
        records::write_record(&directory, &record).map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

/// One write to the sidecars' adoption records, queued by the agent loop.
#[derive(Debug)]
enum SidecarRecordWrite {
    /// A sidecar started: its whole record. Boxed, because a launch's spec
    /// dwarfs the other writes.
    Launch(Box<LaunchRecordWrite>),
    /// A sidecar exited or failed to start again: only its crash-loop state.
    Restarts {
        directory: std::path::PathBuf,
        sidecar: InstanceId,
        restarts: crate::grill::records::SidecarRestarts,
    },
    /// Answers once every write queued before it has landed.
    #[cfg(test)]
    Flush(tokio::sync::oneshot::Sender<()>),
}

/// The whole record of a sidecar that started, queued for the writer.
#[derive(Debug)]
struct LaunchRecordWrite {
    directory: std::path::PathBuf,
    owner: SidecarOwner,
    replica_index: u32,
    launch: SidecarLaunch,
    image: String,
    restarts: Option<crate::grill::records::SidecarRestarts>,
}

/// Writes the sidecars' adoption records one at a time, in the order the
/// agent loop queued them (#2336).
///
/// Each write carries the loop's state at the moment it was queued, so the
/// last one queued must be the last to land. Separate tasks raced: a
/// crash's write could read the record, a restart's whole record land, and
/// the crash's write put back its older count over it. One queue and one
/// writer make the order on disk the loop's order. The queue is unbounded
/// because the loop never waits on a record, and a sidecar's backoff bounds
/// how fast it fills.
#[derive(Debug, Default)]
pub(super) struct SidecarRecordWriter {
    queue: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<SidecarRecordWrite>>,
}

impl SidecarRecordWriter {
    /// Queue `write`, starting the writer task on first use.
    fn queue<G: Grill + Clone + 'static>(&self, grill: &G, write: SidecarRecordWrite) {
        let queue = self.queue.get_or_init(|| {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(write_sidecar_records(grill.clone(), receiver));
            sender
        });
        if queue.send(write).is_err() {
            eprintln!("bun: sidecar record writer has stopped; a Bun restart restarts sidecars");
        }
    }
}

/// The writer task behind [`SidecarRecordWriter`]: each write in turn,
/// until the agent drops the queue and what it queued has landed.
async fn write_sidecar_records<G: Grill>(
    grill: G,
    mut queue: tokio::sync::mpsc::UnboundedReceiver<SidecarRecordWrite>,
) {
    while let Some(write) = queue.recv().await {
        match write {
            SidecarRecordWrite::Launch(write) => {
                let LaunchRecordWrite {
                    directory,
                    owner,
                    replica_index,
                    launch,
                    image,
                    restarts,
                } = *write;
                let sidecar = launch.id.clone();
                let recorded = record_sidecar(
                    &grill,
                    directory,
                    owner,
                    replica_index,
                    launch,
                    image,
                    restarts,
                )
                .await;
                if let Err(reason) = recorded {
                    eprintln!(
                        "bun: {sidecar}: sidecar not recorded for adoption, so a Bun restart restarts it: {reason}"
                    );
                }
            }
            SidecarRecordWrite::Restarts {
                directory,
                sidecar,
                restarts,
            } => {
                let id = sidecar.0.clone();
                let updated = tokio::task::spawn_blocking(move || {
                    crate::grill::records::update_sidecar_restarts(&directory, &id, restarts)
                })
                .await;
                if let Ok(Err(error)) = updated {
                    eprintln!("bun: {sidecar}: sidecar restarts not recorded: {error}");
                }
            }
            #[cfg(test)]
            SidecarRecordWrite::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// A sidecar's restart, finished, for the loop to record.
#[derive(Debug)]
pub(super) struct Relaunched {
    parent: InstanceId,
    sidecar: InstanceId,
    result: Result<(), RelaunchFailure>,
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
    /// ran for [`SIDECAR_STABLE_RUN`]. Its record keeps it across a Bun
    /// restart (#2336).
    restarts: u32,
    /// When its last restart alone succeeded.
    restarted_at: Option<Instant>,
    /// Set while it's down: when it may be started again.
    due: Option<Instant>,
    /// Set while a restart is in flight.
    relaunching: Option<CancellationToken>,
    /// Why the node gate refused its restart, once it did (#2335). It's
    /// parked: never due again until its instance restarts with it, and its
    /// status says why. The app keeps running.
    refused: Option<String>,
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
        self.register_with(parent, specs, |_| None)
    }

    /// Record `specs` as the sidecars of `parent`, an instance adopted after
    /// a Bun restart (#1315). Those in `adopted` run on, adopted with it;
    /// every other one is down, because startup retired it.
    ///
    /// Each keeps the restart count and backoff its record `restored`
    /// (#2336), read with `now` on the monotonic clock being `wall_now`. A
    /// retired one whose record says it was waiting out a backoff waits out
    /// the rest of it, at most [`SIDECAR_MAX_BACKOFF`] in case the wall clock
    /// stepped. One that was running when the last Bun stopped exited
    /// meanwhile, so it's due after its next backoff, as if it exited `now`.
    pub(super) fn register_adopted(
        &mut self,
        parent: &InstanceId,
        specs: &[SidecarSpec],
        adopted: &HashSet<InstanceId>,
        restored: &HashMap<InstanceId, crate::grill::records::SidecarRestarts>,
        now: Instant,
        wall_now: SystemTime,
    ) -> Result<Vec<InstanceId>, BunError> {
        let ids = self.register_with(parent, specs, |_| None)?;
        for id in &ids {
            let saved = restored.get(id).copied().unwrap_or_default();
            let Some(run) = self.run_mut(id) else {
                continue;
            };
            run.restarts = saved.count;
            run.restarted_at = saved
                .restarted_at_unix_ms
                .and_then(|unix_ms| from_unix_ms(unix_ms, now, wall_now));
            if adopted.contains(id) {
                continue;
            }
            match saved.due_at_unix_ms {
                Some(unix_ms) => {
                    let due = from_unix_ms(unix_ms, now, wall_now).unwrap_or(now);
                    run.due = Some(due.clamp(now, now + SIDECAR_MAX_BACKOFF));
                }
                None => {
                    self.mark_exited(id, now);
                }
            }
        }
        Ok(ids)
    }

    /// Register `specs`, each due when `due_of` its identity says: `None`
    /// for one that's running.
    fn register_with(
        &mut self,
        parent: &InstanceId,
        specs: &[SidecarSpec],
        due_of: impl Fn(&InstanceId) -> Option<Instant>,
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
            .map(|spec| {
                let id = crate::grill::oci::sidecar_instance_id(parent, &spec.name);
                SidecarRun {
                    due: due_of(&id),
                    id,
                    spec: spec.clone(),
                    restarts: 0,
                    restarted_at: None,
                    relaunching: None,
                    refused: None,
                }
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
            run.refused = None;
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
                    .filter(|run| {
                        run.due.is_none() && run.relaunching.is_none() && run.refused.is_none()
                    })
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
    /// after a longer backoff; either way it counts. Returns whether its
    /// instance cancelled it meanwhile: then the instance is stopping or
    /// restarting, and what the restart found is no longer its to act on.
    pub(super) fn finish_relaunch(
        &mut self,
        sidecar: &InstanceId,
        succeeded: bool,
        now: Instant,
    ) -> bool {
        let Some(run) = self.run_mut(sidecar) else {
            return false;
        };
        let cancelled = run
            .relaunching
            .take()
            .is_some_and(|token| token.is_cancelled());
        run.restarts = run.restarts.saturating_add(1);
        run.restarted_at = succeeded.then_some(now);
        run.due = (!succeeded && !cancelled).then(|| now + sidecar_backoff(run.restarts));
        cancelled
    }

    /// The node gate refused `sidecar`'s restart (#2335), for `reason`: it
    /// won't be started again until its instance restarts with it, and a
    /// sweep no longer reads it.
    pub(super) fn park(&mut self, sidecar: &InstanceId, reason: &str) {
        if let Some(run) = self.run_mut(sidecar) {
            run.due = None;
            run.refused = Some(reason.to_string());
        }
    }

    /// How many times `sidecar` has been restarted alone.
    pub(super) fn restarts(&self, sidecar: &InstanceId) -> u32 {
        self.by_parent
            .values()
            .flatten()
            .find(|run| &run.id == sidecar)
            .map_or(0, |run| run.restarts)
    }

    /// Cut `sidecar`'s backoff short: it's due at `at`. For tests that
    /// can't wait a backoff out.
    #[cfg(test)]
    pub(super) fn make_due(&mut self, sidecar: &InstanceId, at: Instant) {
        if let Some(run) = self.run_mut(sidecar) {
            run.due = Some(at);
        }
    }

    /// `sidecar`'s crash-loop state as its record keeps it (#2336), with
    /// `now` on the monotonic clock being `wall_now`.
    pub(super) fn restart_state(
        &self,
        sidecar: &InstanceId,
        now: Instant,
        wall_now: SystemTime,
    ) -> Option<crate::grill::records::SidecarRestarts> {
        let run = self
            .by_parent
            .values()
            .flatten()
            .find(|run| &run.id == sidecar)?;
        Some(crate::grill::records::SidecarRestarts {
            count: run.restarts,
            restarted_at_unix_ms: run
                .restarted_at
                .and_then(|at| to_unix_ms(at, now, wall_now)),
            due_at_unix_ms: run.due.and_then(|due| to_unix_ms(due, now, wall_now)),
        })
    }

    /// What `parent`'s sidecars are doing, for its status (#1315), in the
    /// `phase` its instance's state puts them in (#2336).
    pub(super) fn statuses(
        &self,
        parent: &InstanceId,
        phase: SidecarPhase,
    ) -> Vec<super::status::SidecarStatus> {
        use super::status::{SidecarState, SidecarStatus};
        self.by_parent
            .get(parent)
            .into_iter()
            .flatten()
            .map(|run| SidecarStatus {
                name: run.spec.name.clone(),
                id: run.id.0.clone(),
                // A refused sidecar stays stopped, with its reason, even
                // beside an instance still starting (#2335, #2336).
                state: if phase == SidecarPhase::Stopped || run.refused.is_some() {
                    SidecarState::Stopped
                } else if phase == SidecarPhase::Starting {
                    SidecarState::Starting
                } else if run.relaunching.is_some() {
                    SidecarState::Restarting
                } else if run.due.is_some() {
                    SidecarState::Backoff
                } else {
                    SidecarState::Running
                },
                restart_count: run.restarts,
                refused: run.refused.clone(),
            })
            .collect()
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

    /// Record `parent`'s sidecars for adoption, through the record writer
    /// (#1315, #2336): all of
    /// them after the instance's own launch was recorded, or only `only`
    /// after it restarted alone. `parent_spec` is the spec the instance was
    /// launched from.
    ///
    /// Best effort. A sidecar with no record, or a stale one, is what every
    /// sidecar was before #1315: a Bun restart retires it and starts it
    /// again. A write that lands after retirement removed the record leaves
    /// one nothing adopts, which the next startup removes.
    pub(super) fn record_sidecars(
        &self,
        parent: &InstanceId,
        owner: SidecarOwner,
        parent_spec: &OciSpec,
        only: Option<&InstanceId>,
    ) {
        let Some(directory) = self
            .records_dir
            .as_deref()
            .map(crate::grill::records::sidecar_records_dir)
        else {
            return;
        };
        let specs = self.sidecars.specs_of(parent);
        let (now, wall_now) = (Instant::now(), SystemTime::now());
        type Recorded = (
            SidecarLaunch,
            String,
            Option<crate::grill::records::SidecarRestarts>,
        );
        let launches: Vec<Recorded> = sidecar_launches(&specs, parent, parent_spec)
            .into_iter()
            .zip(&specs)
            .filter(|(launch, _)| only.is_none_or(|id| &launch.id == id))
            .map(|(launch, spec)| {
                let image = spec.image.clone().unwrap_or_else(|| owner.image.clone());
                let restarts = self.sidecars.restart_state(&launch.id, now, wall_now);
                (launch, image, restarts)
            })
            .collect();
        if launches.is_empty() {
            return;
        }
        let replica_index = crate::grill::InstanceIdentity::parse(&parent.0)
            .map(|identity| identity.ordinal)
            .unwrap_or(0);
        for (launch, image, restarts) in launches {
            self.sidecar_records.queue(
                self.supervisor.grill(),
                SidecarRecordWrite::Launch(Box::new(LaunchRecordWrite {
                    directory: directory.clone(),
                    owner: owner.clone(),
                    replica_index,
                    launch,
                    image,
                    restarts,
                })),
            );
        }
    }

    /// Who `parent`'s sidecar records name as their owner: the instance as
    /// the supervisor keeps it, with the image it runs, pinned to its digest
    /// where the trust policy pinned it. Every writer of a sidecar record
    /// asks here, so the records agree with the instance's own (#2336).
    pub(super) fn sidecar_owner(&self, parent: &InstanceId) -> Option<SidecarOwner> {
        let instance = self.supervisor.get_instance(parent)?;
        Some(SidecarOwner {
            namespace: instance.namespace.clone(),
            app_name: instance.app_name.clone(),
            image: instance.image.clone(),
        })
    }

    /// Write `sidecar`'s crash-loop state into its record, through the queue,
    /// after it exited or failed to start again (#2336), so a Bun restart
    /// keeps its count and the rest of its backoff. Best effort, as the
    /// record is: a lost write means the next Bun starts its backoff over.
    pub(super) fn record_sidecar_restarts(&self, sidecar: &InstanceId) {
        let Some(directory) = self
            .records_dir
            .as_deref()
            .map(crate::grill::records::sidecar_records_dir)
        else {
            return;
        };
        let Some(restarts) =
            self.sidecars
                .restart_state(sidecar, Instant::now(), SystemTime::now())
        else {
            return;
        };
        self.sidecar_records.queue(
            self.supervisor.grill(),
            SidecarRecordWrite::Restarts {
                directory,
                sidecar: sidecar.clone(),
                restarts,
            },
        );
    }

    /// Wait until every sidecar record write queued so far has landed.
    #[cfg(test)]
    pub(super) async fn flush_sidecar_records(&self) {
        let (done, landed) = tokio::sync::oneshot::channel();
        self.sidecar_records
            .queue(self.supervisor.grill(), SidecarRecordWrite::Flush(done));
        let _ = landed.await;
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
            let context = RelaunchContext {
                secrets: self.secret_providers.clone(),
                app_name,
                namespace,
                host_paths: self.supervisor.host_path_policy(),
                cancelled: token,
                confirmation_timeout: self.stop_confirmation_timeout,
            };
            self.follow_ups.spawn(async move {
                let result = relaunch(grill, launch, context).await;
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
        let cancelled = self
            .sidecars
            .finish_relaunch(&sidecar, result.is_ok(), Instant::now());
        // A refusal that comes back after the instance cancelled the
        // relaunch (it crashed, and restarts with its sidecars) belongs to
        // a launch that's gone: the instance's own restart asks the gate.
        if let Err(RelaunchFailure::Refused(reason)) = &result
            && !cancelled
        {
            self.park_refused_sidecar(&parent, &sidecar, reason).await;
            return;
        }
        // Its new cgroup is bound to its app's namespace on the next tick
        // (#1426).
        if result.is_ok() {
            self.namespace_firewall_stale = true;
        }
        let Some(instance) = self.supervisor.get_instance(&parent) else {
            return;
        };
        let (app, namespace) = (instance.app_name.clone(), instance.namespace.clone());
        // Its new process is what a restarted Bun must find (#1315), and a
        // failed start leaves a longer backoff it must keep (#2336).
        if result.is_ok()
            && let Some(parent_spec) = instance.oci_spec.clone()
            && let Some(owner) = self.sidecar_owner(&parent)
        {
            self.record_sidecars(&parent, owner, &parent_spec, Some(&sidecar));
        } else if result.is_err() {
            self.record_sidecar_restarts(&sidecar);
        }
        let severity = crate::bun::events::EventSeverity::Warning;
        match result {
            Ok(()) => {
                let attempt = self.sidecars.restarts(&sidecar);
                let decision = crate::bun::decisions::Decision::new("restart.sidecar_exited")
                    .with("instance", &parent.0)
                    .with("sidecar", &sidecar)
                    .with("attempt", attempt);
                self.record_decision(
                    crate::bun::events::EventKind::Restart,
                    severity,
                    Some(app),
                    Some(namespace),
                    format!("sidecar {sidecar} of {parent} restarted (restart {attempt})"),
                    decision,
                )
                .await;
            }
            Err(reason) => {
                self.record_event(
                    crate::bun::events::EventKind::Restart,
                    severity,
                    Some(app),
                    Some(namespace),
                    format!("sidecar {sidecar} of {parent} failed to restart: {reason}"),
                )
                .await;
            }
        }
    }

    /// Park `sidecar`, whose restart the node gate refused for `reason`
    /// (#2335), and say so. Its app keeps running, with the mount the gate
    /// refused, until its own next restart, which the same check refuses:
    /// a node never kills a running instance over a policy change (#1316).
    async fn park_refused_sidecar(
        &mut self,
        parent: &InstanceId,
        sidecar: &InstanceId,
        reason: &str,
    ) {
        eprintln!("bun: {sidecar} is parked, {parent} runs on: {reason}");
        self.sidecars.park(sidecar, reason);
        // Its count, which the refused restart raised, outlives a Bun
        // restart, and its record no longer says it's in backoff (#2336).
        self.record_sidecar_restarts(sidecar);
        let Some(instance) = self.supervisor.get_instance(parent) else {
            return;
        };
        let (app, namespace) = (instance.app_name.clone(), instance.namespace.clone());
        self.record_event(
            crate::bun::events::EventKind::Restart,
            crate::bun::events::EventSeverity::Warning,
            Some(app),
            Some(namespace),
            format!(
                "sidecar {sidecar} of {parent} is parked: {reason}; {parent} keeps running \
                 until its next restart"
            ),
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
        // Their runtime metadata and adoption records go too; the runtime
        // refuses while a successor under the same identity owns anything.
        // A record left behind names a retired launch, which the next
        // startup won't adopt and removes.
        let grill = self.supervisor.grill().clone();
        let records = self
            .records_dir
            .as_deref()
            .map(crate::grill::records::sidecar_records_dir);
        tokio::spawn(async move {
            for sidecar in sidecars {
                if let Err(error) = grill.forget_retired(&sidecar).await {
                    eprintln!("bun: {sidecar}: runtime metadata kept: {error}");
                }
                let Some(directory) = records.clone() else {
                    continue;
                };
                let id = sidecar.0.clone();
                let removed = tokio::task::spawn_blocking(move || {
                    crate::grill::records::remove_record(&directory, &id)
                })
                .await;
                if let Ok(Err(error)) = removed {
                    eprintln!("bun: {sidecar}: adoption record kept: {error}");
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
        sidecars
            .register_adopted(
                &parent,
                &proxy_spec(),
                &HashSet::new(),
                &HashMap::new(),
                now,
                SystemTime::now(),
            )
            .unwrap();
        assert_eq!(sidecars.due(now), vec![(parent.clone(), proxy.clone())]);
        assert_eq!(sidecars.parent_of(&proxy), Some(&parent));
        assert_eq!(sidecars.forget(&parent), vec![proxy.clone()]);
        assert_eq!(sidecars.parent_of(&proxy), None);
    }

    /// #1315: the status says which sidecars run, which wait out a
    /// backoff, which are being started, and how often each restarted.
    #[test]
    fn statuses_say_what_each_sidecar_is_doing() {
        use crate::bun::agent::status::SidecarState;
        let mut sidecars = Sidecars::default();
        let app: AppSpec = toml::from_str(
            "image = \"a:1\"\n[[sidecar]]\nname = \"proxy\"\n[[sidecar]]\nname = \"logs\"\n",
        )
        .unwrap();
        let (parent, proxy, logs) = (
            id("web-0"),
            id("web-0__sidecar-proxy"),
            id("web-0__sidecar-logs"),
        );
        sidecars.register(&parent, &app.sidecar).unwrap();
        let now = Instant::now();
        let states = |sidecars: &Sidecars, running: bool| -> Vec<(String, SidecarState, u32)> {
            let phase = if running {
                SidecarPhase::Beside
            } else {
                SidecarPhase::Stopped
            };
            sidecars
                .statuses(&parent, phase)
                .into_iter()
                .map(|status| (status.name, status.state, status.restart_count))
                .collect()
        };
        assert_eq!(
            states(&sidecars, true),
            [
                ("proxy".into(), SidecarState::Running, 0),
                ("logs".into(), SidecarState::Running, 0)
            ]
        );

        sidecars.mark_exited(&proxy, now);
        sidecars.begin_relaunch(&proxy, CancellationToken::new());
        sidecars.finish_relaunch(&proxy, true, now);
        sidecars.mark_exited(&proxy, now);
        sidecars.mark_exited(&logs, now);
        sidecars.begin_relaunch(&logs, CancellationToken::new());
        assert_eq!(
            states(&sidecars, true),
            [
                ("proxy".into(), SidecarState::Backoff, 1),
                ("logs".into(), SidecarState::Restarting, 0)
            ]
        );
        assert!(
            states(&sidecars, false)
                .iter()
                .all(|(_, state, _)| *state == SidecarState::Stopped)
        );
        assert!(
            sidecars
                .statuses(&id("api-0"), SidecarPhase::Beside)
                .is_empty()
        );
    }

    /// #2336: sidecars start before their app, so while it's prepared or
    /// started they're `starting`, not `stopped`.
    #[test]
    fn sidecars_beside_an_instance_still_starting_say_starting() {
        use crate::bun::agent::status::SidecarState;
        let mut sidecars = Sidecars::default();
        let parent = id("web-0");
        sidecars.register(&parent, &proxy_spec()).unwrap();
        for state in [
            ContainerState::Preparing,
            ContainerState::Initialising,
            ContainerState::Starting,
        ] {
            let statuses = sidecars.statuses(&parent, SidecarPhase::of(state));
            assert_eq!(statuses[0].state, SidecarState::Starting, "{state:?}");
        }
        for (state, expected) in [
            (ContainerState::HealthWait, SidecarState::Running),
            (ContainerState::Running, SidecarState::Running),
            (ContainerState::Unhealthy, SidecarState::Running),
            (ContainerState::Pending, SidecarState::Stopped),
            (ContainerState::Stopping, SidecarState::Stopped),
            (ContainerState::Stopped, SidecarState::Stopped),
            (ContainerState::Failed, SidecarState::Stopped),
        ] {
            let statuses = sidecars.statuses(&parent, SidecarPhase::of(state));
            assert_eq!(statuses[0].state, expected, "{state:?}");
        }
    }

    /// #2336 with #2335: a sidecar the node gate refused keeps its restart
    /// count and its reason, and says `stopped`, not `starting`, beside an
    /// instance still starting.
    #[test]
    fn a_refused_sidecar_keeps_its_count_and_reason_and_never_says_starting() {
        use crate::bun::agent::status::SidecarState;
        let mut sidecars = Sidecars::default();
        let (parent, proxy) = (id("web-0"), id("web-0__sidecar-proxy"));
        sidecars.register(&parent, &proxy_spec()).unwrap();
        let now = Instant::now();
        sidecars.mark_exited(&proxy, now);
        sidecars.begin_relaunch(&proxy, CancellationToken::new());
        sidecars.finish_relaunch(&proxy, false, now);
        sidecars.park(&proxy, "host path refused");
        for phase in [
            SidecarPhase::Starting,
            SidecarPhase::Beside,
            SidecarPhase::Stopped,
        ] {
            let status = &sidecars.statuses(&parent, phase)[0];
            assert_eq!(status.state, SidecarState::Stopped, "{phase:?}");
            assert_eq!(status.refused.as_deref(), Some("host path refused"));
            assert_eq!(status.restart_count, 1, "{phase:?}");
        }
        let kept = sidecars
            .restart_state(&proxy, now, SystemTime::now())
            .unwrap();
        assert_eq!(kept.count, 1);
        assert_eq!(
            kept.due_at_unix_ms, None,
            "a parked sidecar isn't in backoff"
        );
    }

    #[test]
    fn wall_clock_times_survive_the_trip_through_a_record() {
        let (now, wall_now) = (Instant::now(), SystemTime::now());
        for offset in [Duration::ZERO, Duration::from_millis(1500)] {
            let later = now + offset;
            let unix_ms = to_unix_ms(later, now, wall_now).unwrap();
            let back = from_unix_ms(unix_ms, now, wall_now).unwrap();
            assert!(back.max(later) - back.min(later) < Duration::from_millis(1));
            if let Some(earlier) = now.checked_sub(offset) {
                let unix_ms = to_unix_ms(earlier, now, wall_now).unwrap();
                let back = from_unix_ms(unix_ms, now, wall_now).unwrap();
                assert!(back.max(earlier) - back.min(earlier) < Duration::from_millis(1));
            }
        }
    }

    /// #2336: an adopted instance's sidecars keep the restart count their
    /// records kept. One the runtime adopted runs on; one it retired while
    /// it waited out a backoff waits out the rest of it, and one retired
    /// with no backoff pending exited while Bun was down, so it's due after
    /// its next backoff.
    #[test]
    fn adopted_sidecars_keep_their_restart_count_and_backoff() {
        use crate::grill::records::SidecarRestarts;
        let mut sidecars = Sidecars::default();
        let app: AppSpec = toml::from_str(
            "image = \"a:1\"\n[[sidecar]]\nname = \"proxy\"\n[[sidecar]]\nname = \"logs\"\n[[sidecar]]\nname = \"metrics\"\n",
        )
        .unwrap();
        let (parent, proxy, logs, metrics) = (
            id("web-0"),
            id("web-0__sidecar-proxy"),
            id("web-0__sidecar-logs"),
            id("web-0__sidecar-metrics"),
        );
        let (now, wall_now) = (Instant::now(), SystemTime::now());
        let in_ms = |after: Duration| to_unix_ms(now + after, now, wall_now);
        let restored = HashMap::from([
            (
                proxy.clone(),
                SidecarRestarts {
                    count: 2,
                    restarted_at_unix_ms: in_ms(Duration::ZERO),
                    due_at_unix_ms: None,
                },
            ),
            (
                logs.clone(),
                SidecarRestarts {
                    count: 5,
                    restarted_at_unix_ms: None,
                    due_at_unix_ms: in_ms(Duration::from_secs(20)),
                },
            ),
            (
                metrics.clone(),
                SidecarRestarts {
                    count: 3,
                    restarted_at_unix_ms: None,
                    due_at_unix_ms: None,
                },
            ),
        ]);
        let adopted = HashSet::from([proxy.clone()]);
        sidecars
            .register_adopted(&parent, &app.sidecar, &adopted, &restored, now, wall_now)
            .unwrap();

        assert_eq!(sidecars.restarts(&proxy), 2);
        assert_eq!(sidecars.restarts(&logs), 5);
        assert_eq!(sidecars.restarts(&metrics), 3);
        assert_eq!(
            sidecars.running_beside(&parent),
            std::slice::from_ref(&proxy)
        );
        assert!(sidecars.due(now).is_empty(), "nothing relaunches at once");
        // metrics: the backoff after three restarts, four seconds.
        assert!(sidecars.due(now + Duration::from_millis(3900)).is_empty());
        assert_eq!(
            sidecars.due(now + Duration::from_secs(4)),
            [(parent.clone(), metrics.clone())]
        );
        // logs: the rest of the backoff its record kept.
        assert_eq!(sidecars.due(now + Duration::from_secs(20)).len(), 2);
        // The adopted one's next crash backs off by its kept count.
        let later = now + Duration::from_secs(1);
        assert!(sidecars.mark_exited(&proxy, later));
        assert!(
            sidecars
                .due(later + Duration::from_millis(1900))
                .iter()
                .all(|(_, id)| id != &proxy)
        );
        assert!(
            sidecars
                .due(later + Duration::from_secs(2))
                .iter()
                .any(|(_, id)| id == &proxy)
        );
    }

    /// #2336: a backoff a record kept never waits longer than the longest
    /// backoff, even when the wall clock stepped back meanwhile.
    #[test]
    fn a_kept_backoff_never_waits_longer_than_a_minute() {
        use crate::grill::records::SidecarRestarts;
        let mut sidecars = Sidecars::default();
        let (parent, proxy) = (id("web-0"), id("web-0__sidecar-proxy"));
        let (now, wall_now) = (Instant::now(), SystemTime::now());
        let restored = HashMap::from([(
            proxy.clone(),
            SidecarRestarts {
                count: 9,
                restarted_at_unix_ms: None,
                due_at_unix_ms: to_unix_ms(now + Duration::from_secs(3600), now, wall_now),
            },
        )]);
        sidecars
            .register_adopted(
                &parent,
                &proxy_spec(),
                &HashSet::new(),
                &restored,
                now,
                wall_now,
            )
            .unwrap();
        assert_eq!(sidecars.due(now + SIDECAR_MAX_BACKOFF), [(parent, proxy)]);
    }

    /// #1315: a sidecar adopted with its instance runs on; only the ones
    /// startup retired are due.
    #[test]
    fn an_adopted_sidecar_runs_on_and_only_the_others_are_due() {
        let mut sidecars = Sidecars::default();
        let app: AppSpec = toml::from_str(
            "image = \"a:1\"\n[[sidecar]]\nname = \"proxy\"\n[[sidecar]]\nname = \"logs\"\n",
        )
        .unwrap();
        let (parent, proxy, logs) = (
            id("web-0"),
            id("web-0__sidecar-proxy"),
            id("web-0__sidecar-logs"),
        );
        let now = Instant::now();
        let adopted = HashSet::from([proxy.clone()]);
        sidecars
            .register_adopted(
                &parent,
                &app.sidecar,
                &adopted,
                &HashMap::new(),
                now,
                SystemTime::now(),
            )
            .unwrap();
        assert_eq!(sidecars.running_beside(&parent), vec![proxy]);
        assert_eq!(sidecars.due(now), vec![(parent, logs)]);
    }
}
