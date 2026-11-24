//! `relish apply --wait`: apply a manifest, then return once its apps run.
//!
//! On a cluster, `relish apply` returns as soon as the council has recorded
//! the change; the scheduler places the apps and each node starts its share
//! afterwards. `--wait` keeps watching until every app in the file is
//! ready, and reports what it saw on the way (#2240).
//!
//! An app is ready when its desired replica count runs, every one of those
//! instances has passed its health check (an instance reports `running`
//! only after that), and, on a cluster, its current spec has finished
//! rolling out: without that last check an update would look ready at once,
//! because the old version's instances still run. Jobs, namespaces,
//! permissions, policies and builds are done once the apply returns, so
//! there's nothing to wait for; a job's runs are followed with `relish jobs`.
//!
//! Everything it reads is what `relish apps` reads, the council's desired
//! apps (`GET /v1/diagnostics/apps`) and every node's instances, plus one
//! leader read, `GET /v1/diagnostics/rollouts`, which says whether each
//! app's current spec has rolled out. A node that predates that read
//! answers 404, and the wait then goes by the instance count alone.
//!
//! [`assess`] is pure, and [`wait_until_ready`] takes its observations from a
//! closure, so the unit tests drive both without a cluster.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::future::Future;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;

use super::agent::envelope::{AgentError, BuildInfo, CommandName, Envelope, NextStep};
use super::apps_cmd::{self, Instances};
use super::client::BunClient;
use super::{CommandOutcome, OutputFormat, RelishError};
use crate::bun::agent::ClusterInstanceStatus;
use crate::bun::diagnostics::{DesiredAppEvidence, RolloutEvidence};
use crate::config::Config;

/// How long `relish apply --wait` waits when `--timeout` isn't given. A
/// first image pull onto a small machine takes tens of seconds; five
/// minutes leaves room for a slow registry without hiding a stuck app for
/// long.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// How often the cluster is asked again.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The longest one look at the cluster may take before it counts as no
/// answer and the next one starts.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(10);

/// The exit code when the apply went through but an app wasn't ready in
/// time. Errors exit with the agent envelope's own codes (3, 4 or 5), so a
/// script can tell "applied, not ready yet" from "not applied".
pub const NOT_READY_EXIT_CODE: u8 = 1;

/// An app the manifest declares, which the wait watches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitTarget {
    /// The app's namespace.
    pub namespace: String,
    /// The app's name.
    pub app: String,
    /// The image it runs, if it runs one, to say what a node is pulling.
    pub image: Option<String>,
    /// Where its replica count comes from.
    pub count: Count,
}

/// Where an app's replica count comes from, which decides what "all its
/// replicas" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// The file's `replicas`.
    Fixed,
    /// `replicas = "*"`: one per node that can run it. The council's count
    /// is every live node, including ones its `placement` rules out, so the
    /// placements say how many there are.
    PerNode,
    /// An `[autoscale]` block: the autoscaler moves the count between `min`
    /// and `max`, so the placements say how many it wants now.
    Autoscaled,
}

/// The apps `config` declares, in namespace and name order.
pub fn targets(config: &Config) -> Vec<WaitTarget> {
    let mut targets: Vec<WaitTarget> = config
        .app
        .iter()
        .map(|(name, app)| WaitTarget {
            namespace: app.namespace.clone().unwrap_or_else(default_namespace),
            app: name.clone(),
            image: app.image.clone(),
            count: if app.replicas == crate::config::Replicas::DaemonSet {
                Count::PerNode
            } else if app.autoscale.is_some() {
                Count::Autoscaled
            } else {
                Count::Fixed
            },
        })
        .collect();
    targets.sort_by(|a, b| (&a.namespace, &a.app).cmp(&(&b.namespace, &b.app)));
    targets
}

fn default_namespace() -> String {
    "default".to_string()
}

/// How one app is doing, as the wait last saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AppReadiness {
    /// The app's namespace.
    pub namespace: String,
    /// The app's name.
    pub app: String,
    /// `true` when its replicas run, pass their health checks and run the
    /// spec just applied.
    pub ready: bool,
    /// Replicas it wants: the file's count, or for a daemon set or an
    /// autoscaled app, how many the scheduler placed.
    pub desired: u32,
    /// Replicas running and passing their health checks.
    pub running: u32,
    /// The nodes its instances are on.
    pub nodes: Vec<String>,
    /// Whether its current spec finished rolling out on every node; `null`
    /// from a standalone node, which doesn't track rollouts.
    pub rolled_out: Option<bool>,
    /// What it's doing, in one line: `1/3 running; 2 pulling nginx:1.27 on
    /// node-2, node-3`.
    pub phase: String,
    /// Why it isn't ready, when the cluster or a node says: a quota block,
    /// a failed rollout, no node with room, or an instance's failure.
    pub reason: Option<String>,
}

impl AppReadiness {
    /// The app as people read it: its name, with the namespace when it
    /// isn't `default`.
    pub fn label(&self) -> String {
        label(&self.namespace, &self.app)
    }

    /// Whether waiting longer can't help: the current spec's rollout
    /// failed, and the leader doesn't retry a failed spec until a different
    /// one is applied.
    fn gave_up(&self, evidence: Option<&DesiredAppEvidence>) -> bool {
        !self.ready && evidence.is_some_and(|app| app.rollout_failed.is_some())
    }
}

fn label(namespace: &str, app: &str) -> String {
    if namespace == "default" {
        app.to_string()
    } else {
        format!("{app} (namespace {namespace})")
    }
}

/// How every target is doing, given what the council wants and what the
/// nodes run.
pub fn assess(targets: &[WaitTarget], observation: &Observation) -> Vec<AppReadiness> {
    targets
        .iter()
        .map(|target| {
            let evidence = observation
                .desired
                .iter()
                .find(|app| app.app == target.app && app.namespace == target.namespace);
            // An app the leader didn't list, or no answer at all from a
            // node that predates the read, is unknown, not "rolling out".
            let rolled_out = observation.rollouts.as_ref().and_then(|rollouts| {
                rollouts
                    .iter()
                    .find(|app| app.app == target.app && app.namespace == target.namespace)
                    .map(|app| app.rolled_out)
            });
            match evidence {
                Some(evidence) => assess_app(target, evidence, rolled_out, &observation.instances),
                None => AppReadiness {
                    namespace: target.namespace.clone(),
                    app: target.app.clone(),
                    ready: false,
                    desired: 0,
                    running: 0,
                    nodes: Vec::new(),
                    rolled_out: None,
                    phase: "the council doesn't list it yet".to_string(),
                    reason: None,
                },
            }
        })
        .collect()
}

fn assess_app(
    target: &WaitTarget,
    evidence: &DesiredAppEvidence,
    rolled_out: Option<bool>,
    instances: &Instances,
) -> AppReadiness {
    let own: Vec<&ClusterInstanceStatus> = instances
        .rows
        .iter()
        .filter(|row| {
            row.instance.app_name == target.app && row.instance.namespace == target.namespace
        })
        .collect();
    let running_count = own
        .iter()
        .filter(|row| row.instance.state == "running")
        .count();
    let running = u32::try_from(running_count).unwrap_or(u32::MAX);
    let wanted = wanted(target.count, evidence);
    // An app that wants no replicas never rolls out anywhere, so there's
    // nothing to wait for.
    let ready = evidence.desired_replicas == 0
        || (running >= wanted && rolled_out != Some(false) && evidence.rollout_failed.is_none());
    let nodes: BTreeSet<&str> = own.iter().map(|row| row.node.as_str()).collect();
    AppReadiness {
        namespace: target.namespace.clone(),
        app: target.app.clone(),
        ready,
        desired: wanted,
        running,
        nodes: nodes.into_iter().map(str::to_string).collect(),
        rolled_out,
        phase: phase(
            target, evidence, rolled_out, wanted, &own, running, ready, instances,
        ),
        reason: if ready {
            None
        } else {
            apps_cmd::reason(evidence, &own)
        },
    }
}

/// How many running replicas make the app whole. A daemon set or an
/// autoscaled app wants what the scheduler placed, once it placed every
/// replica it could; until then, and for a fixed count, the council's count.
fn wanted(count: Count, evidence: &DesiredAppEvidence) -> u32 {
    match count {
        Count::PerNode | Count::Autoscaled
            if evidence.scheduled_replicas > 0 && evidence.unschedulable.is_none() =>
        {
            evidence.scheduled_replicas
        }
        _ => evidence.desired_replicas,
    }
}

/// The lifecycle states an instance passes through before it runs, in
/// order, with what each means for a person waiting on it. `stopping` and
/// `stopped` are left out: those are the old version's instances leaving.
const ON_THE_WAY: &[(&str, &str)] = &[
    ("pending", "waiting to start"),
    ("preparing", "preparing"),
    ("initialising", "running init containers"),
    ("starting", "starting"),
    ("health-wait", "waiting for the health check"),
    ("unhealthy", "failing the health check"),
    ("failed", "failed"),
];

#[allow(clippy::too_many_arguments)]
fn phase(
    target: &WaitTarget,
    evidence: &DesiredAppEvidence,
    rolled_out: Option<bool>,
    desired: u32,
    own: &[&ClusterInstanceStatus],
    running: u32,
    ready: bool,
    instances: &Instances,
) -> String {
    if evidence.desired_replicas == 0 {
        return "wants no replicas".to_string();
    }
    let mut nodes: Vec<&str> = own
        .iter()
        .filter(|row| row.instance.state == "running")
        .map(|row| row.node.as_str())
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    if ready {
        return format!("ready, {running}/{desired} running on {}", nodes.join(", "));
    }
    let mut parts = vec![format!("{running}/{desired} running")];
    // Each state on the way, with the nodes its instances are on.
    let mut by_state: BTreeMap<usize, (u32, BTreeSet<&str>)> = BTreeMap::new();
    for row in own {
        if let Some(order) = ON_THE_WAY
            .iter()
            .position(|(state, _)| *state == row.instance.state)
        {
            let entry = by_state.entry(order).or_default();
            entry.0 += 1;
            entry.1.insert(row.node.as_str());
        }
    }
    for (order, (count, on)) in &by_state {
        let (state, words) = ON_THE_WAY[*order];
        let words = match (state, &target.image) {
            ("preparing", Some(image)) => format!("pulling {image}"),
            _ => words.to_string(),
        };
        let on: Vec<&str> = on.iter().copied().collect();
        parts.push(format!("{count} {words} on {}", on.join(", ")));
    }
    let restarts: u32 = own.iter().map(|row| row.instance.restart_count).sum();
    if restarts > 0 {
        parts.push(format!("restarted {restarts} time(s)"));
    }
    if by_state.is_empty() && running < desired {
        parts.push(if evidence.placements.is_empty() {
            if evidence.scheduled_replicas == 0 {
                "not placed yet".to_string()
            } else {
                format!("{} placed", evidence.scheduled_replicas)
            }
        } else {
            let on: Vec<&str> = evidence.placements.keys().map(String::as_str).collect();
            format!("placed on {}", on.join(", "))
        });
    }
    if running >= desired && rolled_out == Some(false) {
        parts.push("finishing the rollout".to_string());
    }
    if running < desired && !instances.unanswered.is_empty() {
        let quiet: Vec<&str> = instances
            .unanswered
            .iter()
            .map(|(node, _)| node.as_str())
            .collect();
        parts.push(format!("no answer from {}", quiet.join(", ")));
    }
    parts.join("; ")
}

/// One look at the cluster: what the council wants and what the nodes run.
#[derive(Debug, Clone, Default)]
pub struct Observation {
    /// The council's desired apps.
    pub desired: Vec<DesiredAppEvidence>,
    /// Every node's instances.
    pub instances: Instances,
    /// Whether each app's current spec rolled out, from the leader; `None`
    /// from a node that predates `GET /v1/diagnostics/rollouts`.
    pub rollouts: Option<Vec<RolloutEvidence>>,
}

/// Look at the cluster once, the way `relish apps` does.
pub async fn observe(client: &BunClient) -> Result<Observation, RelishError> {
    let (desired, instances, rollouts) = tokio::join!(
        client.desired_apps(),
        apps_cmd::collect_instances(client),
        client.rollouts()
    );
    let rollouts = match rollouts {
        Ok(rollouts) => Some(rollouts),
        // A node from before the read: go by the instance count.
        Err(RelishError::ApiError { status: 404, .. }) => None,
        Err(error) => return Err(error),
    };
    Ok(Observation {
        desired: desired?,
        instances: instances?,
        rollouts,
    })
}

/// How the wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitReport {
    /// Every app was ready.
    pub ready: bool,
    /// How long the wait took.
    pub waited: Duration,
    /// Each app as last seen.
    pub apps: Vec<AppReadiness>,
    /// The wait stopped before the timeout because an app's rollout failed,
    /// which waiting can't fix.
    pub gave_up: bool,
    /// The last error asking the cluster, if the last look failed.
    pub last_error: Option<String>,
}

/// Watch `targets` until each is ready, a rollout fails, or `timeout`
/// passes, looking every `poll` with `observe`. `progress` hears each app
/// whose phase changed, with the time since the wait started.
///
/// A look that fails (a leader election, a node that didn't answer in time)
/// doesn't end the wait: the next one may succeed. Its error is kept for
/// the report.
pub async fn wait_until_ready<F, Fut>(
    targets: &[WaitTarget],
    timeout: Duration,
    poll: Duration,
    mut observe: F,
    mut progress: impl FnMut(Duration, &AppReadiness),
) -> WaitReport
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Observation, RelishError>>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    let mut seen: HashMap<(String, String), String> = HashMap::new();
    let mut apps: Vec<AppReadiness> = targets
        .iter()
        .map(|target| AppReadiness {
            namespace: target.namespace.clone(),
            app: target.app.clone(),
            ready: false,
            desired: 0,
            running: 0,
            nodes: Vec::new(),
            rolled_out: None,
            phase: "no answer from the cluster yet".to_string(),
            reason: None,
        })
        .collect();
    let mut last_error: Option<String>;
    loop {
        let budget = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(OBSERVE_TIMEOUT)
            .max(Duration::from_millis(100));
        match tokio::time::timeout(budget, observe()).await {
            Ok(Ok(observation)) => {
                last_error = None;
                apps = assess(targets, &observation);
                let elapsed = started.elapsed();
                for app in &apps {
                    let key = (app.namespace.clone(), app.app.clone());
                    if seen.get(&key) != Some(&app.phase) {
                        progress(elapsed, app);
                        seen.insert(key, app.phase.clone());
                    }
                }
                let ready = apps.iter().all(|app| app.ready);
                let gave_up = apps.iter().any(|app| {
                    app.gave_up(
                        observation
                            .desired
                            .iter()
                            .find(|d| d.app == app.app && d.namespace == app.namespace),
                    )
                });
                if ready || gave_up {
                    return WaitReport {
                        ready,
                        waited: elapsed,
                        apps,
                        gave_up,
                        last_error,
                    };
                }
            }
            Ok(Err(error)) => last_error = Some(error.to_string()),
            Err(_) => last_error = Some(format!("no answer in {} s", budget.as_secs())),
        }
        if tokio::time::Instant::now() + poll > deadline {
            return WaitReport {
                ready: false,
                waited: started.elapsed(),
                apps,
                gave_up: false,
                last_error,
            };
        }
        tokio::time::sleep(poll).await;
    }
}

/// A job the manifest declares: submitted by the apply, not waited for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct JobLine {
    /// The job's namespace.
    pub namespace: String,
    /// The job's name.
    pub job: String,
}

/// What the node answered to the apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Applied {
    /// Instances a standalone node started before it answered; 0 on a
    /// cluster, where the scheduler places them afterwards.
    pub created: usize,
    /// Their ids.
    pub instances: Vec<String>,
}

/// `data` for `relish apply --wait -o json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ApplyWaitData {
    /// `true` when every app in the file was ready before the timeout. The
    /// apply itself went through either way.
    pub ready: bool,
    /// The wait stopped early because an app's rollout failed, which the
    /// cluster doesn't retry until a different spec is applied.
    pub gave_up: bool,
    /// What the node answered to the apply.
    pub applied: Applied,
    /// How long the wait took, in seconds.
    pub waited_seconds: u64,
    /// How long it was allowed, in seconds.
    pub timeout_seconds: u64,
    /// Each app in the file, as last seen.
    pub apps: Vec<AppReadiness>,
    /// The jobs in the file. Applying submits them; `relish jobs` follows
    /// their runs.
    pub jobs: Vec<JobLine>,
    /// The last error asking the cluster, when the last look failed.
    pub last_error: Option<String>,
}

/// The `data` for `report`.
pub fn wait_data(
    report: WaitReport,
    applied: Applied,
    timeout: Duration,
    config: &Config,
) -> ApplyWaitData {
    ApplyWaitData {
        ready: report.ready,
        gave_up: report.gave_up,
        applied,
        waited_seconds: report.waited.as_secs(),
        timeout_seconds: timeout.as_secs(),
        apps: report.apps,
        jobs: config
            .job
            .iter()
            .map(|(name, job)| JobLine {
                namespace: job.namespace.clone().unwrap_or_else(default_namespace),
                job: name.clone(),
            })
            .collect(),
        last_error: report.last_error,
    }
}

/// The exit code for `data`: 0 when every app is ready, 1 when one isn't.
pub fn exit_code(data: &ApplyWaitData) -> u8 {
    if data.ready { 0 } else { NOT_READY_EXIT_CODE }
}

/// A duration as people write it: `45s`, `5m`, `1h30m`.
pub fn human_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let mut out = String::new();
    if hours > 0 {
        let _ = write!(out, "{hours}h");
    }
    if minutes > 0 {
        let _ = write!(out, "{minutes}m");
    }
    if seconds > 0 || out.is_empty() {
        let _ = write!(out, "{seconds}s");
    }
    out
}

/// One progress line, as `relish apply --wait` prints it to stderr.
pub fn progress_line(elapsed: Duration, app: &AppReadiness) -> String {
    format!("[{:>4}s] {}: {}", elapsed.as_secs(), app.label(), app.phase)
}

/// The closing lines `relish apply --wait` prints for people.
pub fn render(data: &ApplyWaitData) -> String {
    let mut out = String::new();
    for job in &data.jobs {
        let _ = writeln!(
            out,
            "job {}: submitted; `relish jobs` follows its runs",
            label(&job.namespace, &job.job)
        );
    }
    let total = data.apps.len();
    if data.ready {
        let _ = writeln!(
            out,
            "{total} app(s) ready in {}",
            human_duration(Duration::from_secs(data.waited_seconds))
        );
        return out;
    }
    let stuck: Vec<&AppReadiness> = data.apps.iter().filter(|app| !app.ready).collect();
    if data.gave_up {
        let _ = writeln!(
            out,
            "stopped waiting after {}: a rollout failed, and the cluster doesn't retry it",
            human_duration(Duration::from_secs(data.waited_seconds))
        );
    } else {
        let _ = writeln!(
            out,
            "not ready after {}: {} of {total} app(s)",
            human_duration(Duration::from_secs(data.timeout_seconds)),
            stuck.len()
        );
    }
    for app in &stuck {
        let _ = writeln!(out, "  {}: {}", app.label(), app.phase);
        if let Some(reason) = &app.reason {
            let _ = writeln!(out, "    {reason}");
        }
    }
    if let Some(error) = &data.last_error {
        let _ = writeln!(out, "the last look at the cluster failed: {error}");
    }
    let _ = writeln!(
        out,
        "The change is applied. `relish apps{}` shows where it stands.",
        match stuck.as_slice() {
            [only] => apps_command_arguments(only),
            _ => String::new(),
        }
    );
    out
}

/// ` NAME`, or ` NAME --namespace NS` outside `default`.
fn apps_command_arguments(app: &AppReadiness) -> String {
    if app.namespace == "default" {
        format!(" {}", app.app)
    } else {
        format!(" {} --namespace {}", app.app, app.namespace)
    }
}

/// The commands worth running next: each app that isn't ready, or the app
/// list when every one is.
pub fn next_steps(data: &ApplyWaitData) -> Vec<NextStep> {
    let mut steps: Vec<NextStep> = data
        .apps
        .iter()
        .filter(|app| !app.ready)
        .map(|app| NextStep {
            run: format!("relish apps{}", apps_command_arguments(app)),
            why: format!("{}'s instances, and why it isn't ready", app.label()),
            read_only: true,
        })
        .collect();
    if steps.is_empty() {
        steps.push(NextStep {
            run: "relish apps".to_string(),
            why: "list every app with its state".to_string(),
            read_only: true,
        });
    }
    steps
}

/// The answer `relish apply --wait -o json` prints.
pub fn envelope(data: &ApplyWaitData) -> Envelope<&ApplyWaitData> {
    let mut envelope = Envelope::answered(CommandName::Apply, BuildInfo::current(), data);
    for step in next_steps(data) {
        envelope = envelope.with_next(step);
    }
    envelope
}

fn print_structured<T: Serialize>(value: &T, output: OutputFormat) -> Result<(), RelishError> {
    match output {
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(value).map_err(RelishError::SerialiseYaml)?
        ),
        _ => println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(RelishError::SerialiseJson)?
        ),
    }
    Ok(())
}

/// The exit code for an apply that failed before or during the wait: the
/// agent envelope's own, so `3` for a cluster it couldn't reach or
/// authenticate to, `4` for a refusal and `5` for anything else. Never `1`,
/// which means "applied, not ready".
pub fn failure_exit_code(error: &RelishError) -> u8 {
    AgentError::from(error).code.exit_status().code()
}

/// Run `relish apply --wait` against the configured endpoint.
pub async fn run(
    source: &super::manifest::ManifestSource,
    output: OutputFormat,
    break_lease: bool,
    timeout: Duration,
) -> Result<CommandOutcome, RelishError> {
    let client = BunClient::default_local().breaking_leases(break_lease);
    run_with_client(source, output, timeout, POLL_INTERVAL, &client).await
}

/// Apply `source` with `client`, then wait up to `timeout` for its apps.
///
/// With `-o json` or `yaml` every outcome is an agent envelope, a failure
/// too, and the outcome carries the exit code. For people a failure is an
/// error, which the caller prints and exits with [`failure_exit_code`].
pub async fn run_with_client(
    source: &super::manifest::ManifestSource,
    output: OutputFormat,
    timeout: Duration,
    poll: Duration,
    client: &BunClient,
) -> Result<CommandOutcome, RelishError> {
    match apply_and_wait(source, output, timeout, poll, client).await {
        Ok(data) => {
            match output {
                OutputFormat::Human => print!("{}", render(&data)),
                _ => print_structured(&envelope(&data), output)?,
            }
            Ok(CommandOutcome::Exited(exit_code(&data)))
        }
        Err(error) if output == OutputFormat::Human => Err(error),
        Err(error) => {
            let envelope: Envelope<()> = Envelope::failed(
                CommandName::Apply,
                BuildInfo::current(),
                AgentError::from(&error),
            );
            print_structured(&envelope, output)?;
            Ok(CommandOutcome::Exited(envelope.exit_status().code()))
        }
    }
}

/// Apply, then wait: the answer, or why there's none.
pub async fn apply_and_wait(
    source: &super::manifest::ManifestSource,
    output: OutputFormat,
    timeout: Duration,
    poll: Duration,
    client: &BunClient,
) -> Result<ApplyWaitData, RelishError> {
    let config = super::commands::load_manifest(source).await?;
    client.health().await?;
    let result = client.apply(&config).await?;
    let targets = targets(&config);
    let human = output == OutputFormat::Human;
    if human {
        eprintln!(
            "applied {} app(s); waiting up to {} for them to run",
            targets.len(),
            human_duration(timeout)
        );
    }
    let report = wait_until_ready(
        &targets,
        timeout,
        poll,
        || observe(client),
        |elapsed, app| {
            if human {
                eprintln!("{}", progress_line(elapsed, app));
            }
        },
    )
    .await;
    let applied = Applied {
        created: result.created,
        instances: result.instances,
    };
    Ok(wait_data(report, applied, timeout, &config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::agent::InstanceStatus;

    fn target(app: &str) -> WaitTarget {
        WaitTarget {
            namespace: "default".to_string(),
            app: app.to_string(),
            image: Some("nginx:1.27".to_string()),
            count: Count::Fixed,
        }
    }

    /// What the council says about one app, and whether the leader says it
    /// rolled out (`None`: the leader didn't say).
    #[derive(Clone)]
    struct Evidence {
        desired: DesiredAppEvidence,
        rolled_out: Option<bool>,
    }

    fn evidence(app: &str, replicas: u32, rolled_out: Option<bool>) -> Evidence {
        let desired = DesiredAppEvidence {
            app: app.to_string(),
            namespace: "default".to_string(),
            desired_replicas: replicas,
            scheduled_replicas: replicas,
            placements: Default::default(),
            service_port: None,
            blocked: None,
            volume_home_away: None,
            unschedulable: None,
            volume_homes: Vec::new(),
            rollout_failed: None,
            owner: None,
        };
        Evidence {
            desired,
            rolled_out,
        }
    }

    /// One look at the cluster. With no app's rollout known, the answer is
    /// an old node's: no rollouts at all.
    fn look(apps: Vec<Evidence>, rows: Vec<ClusterInstanceStatus>) -> Observation {
        let rollouts = apps.iter().any(|app| app.rolled_out.is_some()).then(|| {
            apps.iter()
                .filter_map(|app| {
                    app.rolled_out.map(|rolled_out| RolloutEvidence {
                        app: app.desired.app.clone(),
                        namespace: app.desired.namespace.clone(),
                        fingerprint: "f".to_string(),
                        rolled_out,
                    })
                })
                .collect()
        });
        Observation {
            desired: apps.into_iter().map(|app| app.desired).collect(),
            instances: instances(rows),
            rollouts,
        }
    }

    fn assessed(
        targets: &[WaitTarget],
        apps: &[Evidence],
        instances: &Instances,
    ) -> Vec<AppReadiness> {
        assess(targets, &look(apps.to_vec(), instances.rows.clone()))
    }

    fn instance(node: &str, app: &str, id: &str, state: &str) -> ClusterInstanceStatus {
        ClusterInstanceStatus {
            node: node.to_string(),
            instance: InstanceStatus {
                id: id.to_string(),
                app_name: app.to_string(),
                namespace: "default".to_string(),
                state: state.to_string(),
                restart_count: 0,
                host_port: None,
                exit_code: None,
                pid: None,
                runtime_unknown: false,
                status_age_ms: None,
                age_seconds: Some(1),
                reason: None,
                image: None,
                data_on_node: false,
            },
        }
    }

    fn instances(rows: Vec<ClusterInstanceStatus>) -> Instances {
        Instances {
            rows,
            unanswered: Vec::new(),
        }
    }

    #[test]
    fn an_app_is_ready_when_every_replica_runs_its_rolled_out_spec() {
        let seen = assessed(
            &[target("web")],
            &[evidence("web", 2, Some(true))],
            &instances(vec![
                instance("node-1", "web", "web-0", "running"),
                instance("node-2", "web", "web-1", "running"),
            ]),
        );
        assert!(seen[0].ready, "{seen:?}");
        assert_eq!(seen[0].phase, "ready, 2/2 running on node-1, node-2");
        assert_eq!(seen[0].nodes, ["node-1", "node-2"]);
        assert_eq!(seen[0].reason, None);
    }

    #[test]
    fn an_app_short_of_replicas_says_what_the_rest_are_doing() {
        let seen = assessed(
            &[target("web")],
            &[evidence("web", 3, Some(false))],
            &instances(vec![
                instance("node-1", "web", "web-0", "running"),
                instance("node-2", "web", "web-1", "preparing"),
                instance("node-3", "web", "web-2", "health-wait"),
            ]),
        );
        assert!(!seen[0].ready);
        assert_eq!(
            seen[0].phase,
            "1/3 running; 1 pulling nginx:1.27 on node-2; \
             1 waiting for the health check on node-3"
        );
    }

    #[test]
    fn an_update_is_not_ready_while_the_old_version_still_serves() {
        // The old instances run every replica, but the new spec hasn't
        // rolled out yet.
        let seen = assessed(
            &[target("web")],
            &[evidence("web", 1, Some(false))],
            &instances(vec![instance("node-1", "web", "web-0", "running")]),
        );
        assert!(!seen[0].ready);
        assert_eq!(seen[0].phase, "1/1 running; finishing the rollout");
    }

    #[test]
    fn a_standalone_node_is_ready_on_its_instance_count_alone() {
        let seen = assessed(
            &[target("web")],
            &[evidence("web", 1, None)],
            &instances(vec![instance("local", "web", "web-0", "running")]),
        );
        assert!(seen[0].ready);
    }

    #[test]
    fn an_unplaced_app_names_the_schedulers_reason() {
        let mut short = evidence("web", 2, Some(false));
        short.desired.scheduled_replicas = 0;
        short.desired.unschedulable = Some(crate::meat::types::Unschedulable {
            placed: 0,
            wanted: 2,
            reason: crate::meat::types::UnplacedReason::NoLabelMatch,
        });
        let seen = assessed(&[target("web")], &[short], &instances(Vec::new()));
        assert!(!seen[0].ready);
        assert_eq!(seen[0].phase, "0/2 running; not placed yet");
        let reason = seen[0].reason.as_deref().unwrap();
        assert!(reason.starts_with("unschedulable: "), "{reason}");
    }

    #[test]
    fn a_crash_loop_counts_its_restarts_and_names_the_failure() {
        let mut crashing = instance("node-1", "web", "web-0", "starting");
        crashing.instance.restart_count = 4;
        crashing.instance.reason = Some("exited with status 1".to_string());
        let seen = assessed(
            &[target("web")],
            &[evidence("web", 1, Some(false))],
            &instances(vec![crashing]),
        );
        assert_eq!(
            seen[0].phase,
            "0/1 running; 1 starting on node-1; restarted 4 time(s)"
        );
        assert_eq!(
            seen[0].reason.as_deref(),
            Some("web-0 failed on node-1: exited with status 1")
        );
    }

    /// Review of #2253: the council counts every live node for a daemon
    /// set, but a node its `placement` rules out never gets a replica.
    #[test]
    fn a_daemon_set_is_ready_on_every_node_it_was_placed_on() {
        let mut daemon = target("agent");
        daemon.count = Count::PerNode;
        let mut placed = evidence("agent", 3, Some(true));
        placed.desired.scheduled_replicas = 2;
        let rows = instances(vec![
            instance("node-1", "agent", "agent-0", "running"),
            instance("node-2", "agent", "agent-1", "running"),
        ]);
        let seen = assessed(&[daemon], &[placed.clone()], &rows);
        assert!(seen[0].ready, "{seen:?}");
        assert_eq!(seen[0].desired, 2);
        assert_eq!(seen[0].phase, "ready, 2/2 running on node-1, node-2");
        // A fixed count still wants every replica the council asks for.
        let seen = assessed(&[target("agent")], &[placed], &rows);
        assert!(!seen[0].ready, "{seen:?}");
    }

    #[test]
    fn an_autoscaled_app_is_ready_at_the_count_the_autoscaler_holds() {
        let mut scaled = target("api");
        scaled.count = Count::Autoscaled;
        let mut held = evidence("api", 4, Some(true));
        held.desired.scheduled_replicas = 2;
        let rows = instances(vec![
            instance("node-1", "api", "api-0", "running"),
            instance("node-2", "api", "api-1", "running"),
        ]);
        assert!(assessed(&[scaled.clone()], &[held.clone()], &rows)[0].ready);
        // Not while some of its replicas found no node.
        held.desired.unschedulable = Some(crate::meat::types::Unschedulable {
            placed: 2,
            wanted: 4,
            reason: crate::meat::types::UnplacedReason::NoRoom,
        });
        assert!(!assessed(&[scaled], &[held], &rows)[0].ready);
    }

    #[test]
    fn targets_know_where_their_count_comes_from() {
        let config = Config::parse(
            r#"
            [app.agent]
            image = "agent:1"
            replicas = "*"
            [app.api]
            image = "api:1"
            [app.api.autoscale]
            metric = "cpu"
            target = "70%"
            min = 2
            max = 10
        "#,
        )
        .unwrap();
        let counts: Vec<(String, Count)> = targets(&config)
            .into_iter()
            .map(|target| (target.app, target.count))
            .collect();
        assert_eq!(
            counts,
            [
                ("agent".to_string(), Count::PerNode),
                ("api".to_string(), Count::Autoscaled),
            ]
        );
    }

    #[test]
    fn an_app_the_council_doesnt_list_yet_isnt_ready() {
        let seen = assessed(&[target("web")], &[], &instances(Vec::new()));
        assert!(!seen[0].ready);
        assert_eq!(seen[0].phase, "the council doesn't list it yet");
    }

    #[test]
    fn an_app_that_wants_no_replicas_is_ready_at_once() {
        let seen = assessed(
            &[target("web")],
            &[evidence("web", 0, Some(false))],
            &instances(Vec::new()),
        );
        assert!(seen[0].ready);
    }

    #[test]
    fn targets_are_the_files_apps_with_their_namespaces() {
        let config = Config::parse(
            r#"
            [app.web]
            image = "nginx:1.27"
            [app.api]
            exec = "/bin/api"
            namespace = "shop"
            [job.nightly]
            image = "busybox:1"
        "#,
        )
        .unwrap();
        assert_eq!(
            targets(&config),
            [
                WaitTarget {
                    namespace: "default".into(),
                    app: "web".into(),
                    image: Some("nginx:1.27".into()),
                    count: Count::Fixed,
                },
                WaitTarget {
                    namespace: "shop".into(),
                    app: "api".into(),
                    image: None,
                    count: Count::Fixed,
                },
            ]
        );
    }

    /// Observations handed out one per look, the last one repeating.
    fn script(
        looks: Vec<Result<Observation, RelishError>>,
    ) -> impl FnMut() -> std::future::Ready<Result<Observation, RelishError>> {
        let mut looks = std::collections::VecDeque::from(looks);
        move || {
            let look = if looks.len() > 1 {
                looks.pop_front().unwrap()
            } else {
                match looks.front().unwrap() {
                    Ok(observation) => Ok(observation.clone()),
                    Err(_) => Err(RelishError::RequestTimeout),
                }
            };
            std::future::ready(look)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_returns_once_every_app_is_ready() {
        let looks = vec![
            Err(RelishError::RequestTimeout),
            Ok(look(
                vec![
                    evidence("web", 1, Some(false)),
                    evidence("db", 1, Some(false)),
                ],
                vec![instance("node-1", "web", "web-0", "preparing")],
            )),
            Ok(look(
                vec![
                    evidence("web", 1, Some(true)),
                    evidence("db", 1, Some(false)),
                ],
                vec![
                    instance("node-1", "web", "web-0", "running"),
                    instance("node-2", "db", "db-0", "running"),
                ],
            )),
            Ok(look(
                vec![
                    evidence("web", 1, Some(true)),
                    evidence("db", 1, Some(true)),
                ],
                vec![
                    instance("node-1", "web", "web-0", "running"),
                    instance("node-2", "db", "db-0", "running"),
                ],
            )),
        ];
        let mut lines = Vec::new();
        let report = wait_until_ready(
            &[target("db"), target("web")],
            Duration::from_secs(60),
            Duration::from_secs(1),
            script(looks),
            |elapsed, app| lines.push(progress_line(elapsed, app)),
        )
        .await;
        assert!(report.ready, "{report:?}");
        assert!(!report.gave_up);
        assert_eq!(report.waited, Duration::from_secs(3));
        assert_eq!(report.last_error, None);
        assert_eq!(
            lines,
            [
                "[   1s] db: 0/1 running; 1 placed",
                "[   1s] web: 0/1 running; 1 pulling nginx:1.27 on node-1",
                "[   2s] db: 1/1 running; finishing the rollout",
                "[   2s] web: ready, 1/1 running on node-1",
                "[   3s] db: ready, 1/1 running on node-2",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_times_out_with_the_last_look_and_error() {
        let looks = vec![
            Ok(look(vec![evidence("web", 1, Some(false))], Vec::new())),
            Err(RelishError::RequestTimeout),
        ];
        let report = wait_until_ready(
            &[target("web")],
            Duration::from_secs(5),
            Duration::from_secs(1),
            script(looks),
            |_, _| {},
        )
        .await;
        assert!(!report.ready);
        assert!(!report.gave_up);
        assert!(report.waited <= Duration::from_secs(5), "{report:?}");
        assert_eq!(report.apps[0].phase, "0/1 running; 1 placed");
        assert!(report.last_error.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_rollout_ends_the_wait_at_once() {
        let mut failed = evidence("web", 1, Some(false));
        failed.desired.rollout_failed = Some(crate::meat::deploy_types::RolloutFailure {
            fingerprint: "f".into(),
            node: "node-2".into(),
            reason: "web-g2-0 failed its health check".into(),
            at_unix_ms: 1,
        });
        let looks = vec![Ok(look(
            vec![failed],
            vec![instance("node-2", "web", "web-0", "running")],
        ))];
        let report = wait_until_ready(
            &[target("web")],
            Duration::from_secs(300),
            Duration::from_secs(1),
            script(looks),
            |_, _| {},
        )
        .await;
        assert!(!report.ready);
        assert!(report.gave_up);
        assert_eq!(report.waited, Duration::ZERO);
        assert_eq!(
            report.apps[0].reason.as_deref(),
            Some("RolloutFailed on node-2: web-g2-0 failed its health check")
        );
    }

    fn data(apps: Vec<AppReadiness>, ready: bool) -> ApplyWaitData {
        ApplyWaitData {
            ready,
            gave_up: false,
            applied: Applied {
                created: 0,
                instances: Vec::new(),
            },
            waited_seconds: 14,
            timeout_seconds: 300,
            apps,
            jobs: vec![JobLine {
                namespace: "default".into(),
                job: "nightly".into(),
            }],
            last_error: None,
        }
    }

    fn readiness(app: &str, namespace: &str, ready: bool) -> AppReadiness {
        AppReadiness {
            namespace: namespace.to_string(),
            app: app.to_string(),
            ready,
            desired: 2,
            running: if ready { 2 } else { 0 },
            nodes: Vec::new(),
            rolled_out: Some(ready),
            phase: if ready {
                "ready, 2/2 running on node-1".to_string()
            } else {
                "0/2 running; not placed yet".to_string()
            },
            reason: (!ready).then(|| "unschedulable: no node has the labels".to_string()),
        }
    }

    #[test]
    fn a_ready_apply_says_how_long_it_took() {
        let data = data(vec![readiness("web", "default", true)], true);
        assert_eq!(
            render(&data),
            "job nightly: submitted; `relish jobs` follows its runs\n1 app(s) ready in 14s\n"
        );
        assert_eq!(exit_code(&data), 0);
    }

    #[test]
    fn a_timed_out_apply_names_each_app_that_isnt_ready_and_why() {
        let data = data(
            vec![
                readiness("web", "default", true),
                readiness("big", "shop", false),
            ],
            false,
        );
        assert_eq!(
            render(&data),
            "job nightly: submitted; `relish jobs` follows its runs\n\
             not ready after 5m: 1 of 2 app(s)\n  \
             big (namespace shop): 0/2 running; not placed yet\n    \
             unschedulable: no node has the labels\n\
             The change is applied. `relish apps big --namespace shop` shows where it stands.\n"
        );
        assert_eq!(exit_code(&data), NOT_READY_EXIT_CODE);
    }

    #[test]
    fn the_json_answer_is_an_agent_envelope_pointing_at_the_stuck_app() {
        let data = data(vec![readiness("web", "default", false)], false);
        let value = serde_json::to_value(envelope(&data)).unwrap();
        assert_eq!(value["schema"], "relish.agent/v1");
        assert_eq!(value["command"], "apply");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["ready"], false);
        assert_eq!(value["data"]["apps"][0]["app"], "web");
        assert_eq!(value["data"]["jobs"][0]["job"], "nightly");
        assert_eq!(value["next"][0]["run"], "relish apps web");
        assert_eq!(value["next"][0]["read_only"], true);
    }

    #[test]
    fn agent_schema_offers_the_apply_data() {
        let schema = crate::relish::agent::data_schema(CommandName::Apply).unwrap();
        assert!(schema["properties"]["apps"].is_object(), "{schema}");
        assert!(CommandName::ANSWERING.contains(&CommandName::Apply));
    }

    #[test]
    fn failures_exit_with_the_envelopes_codes_never_the_not_ready_one() {
        let unreachable = RelishError::AgentUnreachable {
            endpoint: "https://localhost:9117".into(),
            cause: crate::relish::UnreachableCause::Refused,
        };
        assert_eq!(failure_exit_code(&unreachable), 3);
        let refused = RelishError::ApiError {
            status: 403,
            body: "no".into(),
        };
        assert_eq!(failure_exit_code(&refused), 4);
        let broken = RelishError::ApiError {
            status: 500,
            body: "boom".into(),
        };
        assert_eq!(failure_exit_code(&broken), 5);
    }

    #[test]
    fn durations_read_the_way_people_write_them() {
        assert_eq!(human_duration(Duration::from_secs(0)), "0s");
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::from_secs(300)), "5m");
        assert_eq!(human_duration(Duration::from_secs(5430)), "1h30m30s");
    }
}
