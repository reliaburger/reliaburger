//! Building a proposal on the leader, from its own desired state: the
//! `before` and `after` of each touched app, the plain-language summary,
//! the field diff, the blast radius and the undo plan.
//!
//! The author supplies only the request and its reason. Everything a human
//! reads before approving is computed here, from what the cluster holds, so
//! an agent can't describe a smaller change than the one it makes.

use std::collections::BTreeSet;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::revision::current_app;
use super::types::*;
use crate::config::app::AppSpec;
use crate::config::{Config, Replicas};
use crate::council::types::DesiredState;
use crate::meat::AppId;

/// What an author asks for, as `POST /v1/proposals` takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposalRequest {
    /// The kind of change.
    pub kind: RequestKind,
    /// The namespace; `default` when absent. For `undo`, the original's.
    #[serde(default)]
    pub namespace: Option<String>,
    /// The app, for `scale`, `rollback`, `stop`, `delete`, `restart` and
    /// `cancel_deploy`.
    #[serde(default)]
    pub app: Option<String>,
    /// The new replica count, for `scale`.
    #[serde(default)]
    pub replicas: Option<u32>,
    /// The TOML whose `[app.*]` tables to apply, for `apply`.
    #[serde(default)]
    pub config: Option<String>,
    /// The proposal to undo, for `undo`.
    #[serde(default)]
    pub of: Option<u64>,
    /// The fault to clear, for `clear_fault`: its id, as `relish fault
    /// list` shows it.
    #[serde(default)]
    pub fault: Option<u64>,
    /// The node that holds the fault, for `clear_fault`, when two nodes
    /// hold a fault with the same id.
    #[serde(default)]
    pub node: Option<String>,
    /// Why, in the author's words. At most 1 KiB; secrets are redacted and
    /// control characters removed.
    pub reason: String,
    /// How long it may wait for approval: 300 to 86400 seconds; 3600 when
    /// absent.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// A key that makes a retry return the proposal already made.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// The kinds of change a request may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Apply `[app.*]` tables.
    Apply,
    /// Change an app's replica count.
    Scale,
    /// Roll an app back to its newest released spec that differs.
    Rollback,
    /// Stop an app.
    Stop,
    /// Delete an app.
    Delete,
    /// Replace every instance of an app, node by node.
    Restart,
    /// Cancel an app's rollout in progress.
    CancelDeploy,
    /// Clear an active Smoker fault on an app.
    ClearFault,
    /// Undo an applied proposal.
    Undo,
}

/// What the leader looked up for a request before building it: things the
/// council doesn't hold.
#[derive(Debug, Clone, Default)]
pub struct BuildContext {
    /// For a rollback the council has no released spec for, the target the
    /// nodes' deploy history gives
    /// (`crate::bun::cluster_view::rollback_target`).
    pub history_target: Option<AppSpec>,
    /// For a `clear_fault`, the fault, as the nodes listed it.
    pub fault: Option<FaultTarget>,
}

/// The longest idempotency key, in bytes.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// Build the proposal `request` asks for, by `author`, against `desired`
/// at `now_unix_ms`. Its id is 0 until the state machine numbers it.
///
/// `fill` gives each `apply` app its spec with policy defaults filled in,
/// as `relish apply` stores it.
pub fn build(
    desired: &DesiredState,
    request: &ProposalRequest,
    author: &Principal,
    now_unix_ms: u64,
    fill: impl Fn(&AppId, &AppSpec) -> AppSpec,
) -> Result<Proposal, ProposalRefusal> {
    build_with_history(desired, request, author, now_unix_ms, fill, None)
}

/// [`build`], with the rollback target the nodes' deploy history gives
/// (`crate::bun::cluster_view::rollback_target`), for when the council
/// has no released spec to go back to: after a crash loop, say. A person's
/// `relish rollback` falls back the same way, so both pick the same spec.
pub fn build_with_history(
    desired: &DesiredState,
    request: &ProposalRequest,
    author: &Principal,
    now_unix_ms: u64,
    fill: impl Fn(&AppId, &AppSpec) -> AppSpec,
    history_target: Option<&AppSpec>,
) -> Result<Proposal, ProposalRefusal> {
    let context = BuildContext {
        history_target: history_target.cloned(),
        fault: None,
    };
    build_with_context(desired, request, author, now_unix_ms, fill, &context)
}

/// [`build`], with what the leader looked up beyond the council
/// ([`BuildContext`]).
pub fn build_with_context(
    desired: &DesiredState,
    request: &ProposalRequest,
    author: &Principal,
    now_unix_ms: u64,
    fill: impl Fn(&AppId, &AppSpec) -> AppSpec,
    context: &BuildContext,
) -> Result<Proposal, ProposalRefusal> {
    let namespace = request
        .namespace
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let ttl_ms = match request.ttl_seconds {
        None => DEFAULT_PROPOSAL_TTL_MS,
        Some(seconds) => seconds.saturating_mul(1000),
    };
    if !(MIN_PROPOSAL_TTL_MS..=MAX_PROPOSAL_TTL_MS).contains(&ttl_ms) {
        return invalid("ttl_seconds must be between 300 and 86400");
    }
    if let Some(key) = &request.idempotency_key
        && (key.is_empty()
            || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
            || key.chars().any(char::is_control))
    {
        return invalid("an idempotency key is 1 to 128 bytes with no control characters");
    }
    if request.kind == RequestKind::ClearFault {
        let Some(fault) = context.fault.as_ref() else {
            return invalid("a clear_fault names an active fault in `fault`");
        };
        check_owned(desired, &fault.app)?;
        return Ok(clear_fault_proposal(
            desired,
            request,
            author,
            now_unix_ms,
            ttl_ms,
            fault,
        ));
    }
    let (kind, namespace, changes) = match request.kind {
        RequestKind::Apply => (
            ProposalKind::Apply,
            namespace.clone(),
            apply_changes(desired, request, &namespace, fill)?,
        ),
        RequestKind::Undo => {
            let Some(of) = request.of else {
                return invalid("an undo names the proposal it undoes in `of`");
            };
            let (namespace, changes) = undo_changes(desired, ProposalId(of), now_unix_ms)?;
            (
                ProposalKind::Undo { of: ProposalId(of) },
                namespace,
                changes,
            )
        }
        one_app => {
            let Some(name) = request.app.as_deref() else {
                return invalid("this kind names an app in `app`");
            };
            let app = AppId::new(name, &namespace);
            let (kind, change) = one_app_change(
                desired,
                one_app,
                &app,
                request.replicas,
                context.history_target.as_ref(),
                now_unix_ms,
            )?;
            (kind, namespace, vec![change])
        }
    };
    if changes
        .iter()
        .all(|change| change.before.revision == change.after.revision)
    {
        return invalid("the proposal changes nothing; every app is already like that");
    }
    let reversibility = reversibility_of(kind, &changes);
    let blast_radius = blast_radius(desired, &changes);
    let diff = field_diff(&changes);
    let summary = summary(kind, &changes, &blast_radius, reversibility);
    let undo = UndoPlan {
        command: None,
        effect: undo_effect(kind, &changes, reversibility),
    };
    Ok(Proposal {
        id: ProposalId(0),
        kind,
        namespace,
        author: author.clone(),
        reason: clean_reason(&request.reason),
        idempotency_key: request.idempotency_key.clone(),
        created_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl_ms),
        state: ProposalState::Pending,
        changes,
        summary,
        diff,
        blast_radius,
        reversibility,
        undo,
        findings: Vec::new(),
        compacted: false,
        fault: None,
    })
}

/// A `clear_fault` proposal: no spec changes, the fault as the leader
/// found it, and a blast radius of the faulted app on the fault's node.
fn clear_fault_proposal(
    desired: &DesiredState,
    request: &ProposalRequest,
    author: &Principal,
    now_unix_ms: u64,
    ttl_ms: u64,
    fault: &FaultTarget,
) -> Proposal {
    let app = &fault.app;
    let mut radius = BlastRadius {
        apps: vec![app.to_string()],
        nodes: vec![fault.node.clone()],
        ..BlastRadius::default()
    };
    if let Some(spec) = desired.apps.get(app) {
        if let Some(ingress) = &spec.ingress {
            radius.routes.push(format!(
                "{}{}",
                ingress.host,
                ingress.path.as_deref().unwrap_or("/")
            ));
        }
        if spec.port.is_some() {
            radius
                .services
                .push(format!("{}.{}", app.name, app.namespace));
        }
    }
    let scope = match &fault.instance {
        Some(instance) => format!("instance {instance} of {app}"),
        None => app.to_string(),
    };
    let mut summary = vec![
        "CAN'T BE UNDONE: think before approving.".to_string(),
        format!(
            "Clear fault {} ({}) on {scope}, held by node {}, injected by {}, {}s left when \
             proposed.",
            fault.id, fault.fault_type, fault.node, fault.injected_by, fault.remaining_secs
        ),
        format!(
            "Touches 1 app on node {}; it changes no spec, and the fault would expire on its \
             own.",
            fault.node
        ),
    ];
    let mut serves: Vec<String> = radius
        .routes
        .iter()
        .map(|route| format!("https://{route}"))
        .collect();
    serves.extend(
        radius
            .services
            .iter()
            .map(|service| format!("the service {service}")),
    );
    if !serves.is_empty() {
        summary.push(format!("Serves {}.", serves.join(" and ")));
    }
    Proposal {
        id: ProposalId(0),
        kind: ProposalKind::ClearFault,
        namespace: app.namespace.clone(),
        author: author.clone(),
        reason: clean_reason(&request.reason),
        idempotency_key: request.idempotency_key.clone(),
        created_at_unix_ms: now_unix_ms,
        expires_at_unix_ms: now_unix_ms.saturating_add(ttl_ms),
        state: ProposalState::Pending,
        changes: Vec::new(),
        summary,
        diff: vec![FieldChange {
            app: app.to_string(),
            field: format!("fault.{}", fault.id),
            from: Some(fault.fault_type.clone()),
            to: Some("cleared".to_string()),
        }],
        blast_radius: radius,
        reversibility: Reversibility::Irreversible,
        undo: UndoPlan {
            command: None,
            effect: "a cleared fault can't be put back; a person injects it again with `relish \
                     fault` if they still want it"
                .to_string(),
        },
        findings: Vec::new(),
        compacted: false,
        fault: Some(fault.clone()),
    }
}

/// Give a new proposal its number: the undo command, and the summary line
/// that names it. The state machine calls this as it stores the proposal.
pub fn number(proposal: &mut Proposal, id: ProposalId) {
    proposal.id = id;
    if proposal.reversibility == Reversibility::Irreversible {
        proposal.undo.command = None;
        return;
    }
    let command = format!("relish undo {id}");
    proposal.summary.push(format!(
        "Undo: `{command}` {}.",
        proposal.undo.effect.trim_end_matches('.')
    ));
    proposal.undo.command = Some(command);
}

/// The author's reason, made safe to store and show: control characters
/// become spaces, secrets are redacted, and it's cut to
/// [`MAX_REASON_BYTES`] on a character boundary.
pub fn clean_reason(reason: &str) -> String {
    let flat: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut clean = crate::redact::redact_secrets(flat.trim());
    if clean.len() > MAX_REASON_BYTES {
        let mut end = MAX_REASON_BYTES;
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        clean.truncate(end);
    }
    clean
}

fn invalid<T>(reason: &str) -> Result<T, ProposalRefusal> {
    Err(ProposalRefusal::Invalid {
        reason: reason.to_string(),
    })
}

fn refused<T>(rule: &str, message: String) -> Result<T, ProposalRefusal> {
    Err(ProposalRefusal::Guardrail {
        rule: rule.to_string(),
        message,
    })
}

/// The refusals every kind shares: an app GitOps or a controller owns.
fn check_owned(desired: &DesiredState, app: &AppId) -> Result<(), ProposalRefusal> {
    if desired.gitops_owned.apps.contains(app) {
        return refused(
            "gitops_owned",
            format!("{app} is managed by GitOps; propose a change to the repository"),
        );
    }
    if let Some(owner) = desired.apps.get(app).and_then(|spec| spec.owner.as_deref()) {
        return refused(
            "controller_owned",
            format!("{app} is managed by the controller {owner}, which is its only writer"),
        );
    }
    Ok(())
}

fn one_app_change(
    desired: &DesiredState,
    kind: RequestKind,
    app: &AppId,
    replicas: Option<u32>,
    history_target: Option<&AppSpec>,
    now_unix_ms: u64,
) -> Result<(ProposalKind, AppChange), ProposalRefusal> {
    let before = current_app(desired, app);
    let Some(spec) = before.spec.as_deref().cloned() else {
        return Err(ProposalRefusal::Invalid {
            reason: format!("there's no app {app}"),
        });
    };
    check_owned(desired, app)?;
    let (kind, after) = match kind {
        RequestKind::Scale => {
            let Some(replicas) = replicas else {
                return invalid("a scale names the new count in `replicas`");
            };
            if replicas == 0 {
                return refused(
                    "scale_to_zero",
                    format!("scaling {app} to zero is `stop`; propose that instead"),
                );
            }
            if spec.autoscale.is_some() {
                return refused(
                    "autoscaled",
                    format!("{app} is autoscaled; the autoscaler owns its replica count"),
                );
            }
            if spec.replicas == Replicas::DaemonSet {
                return refused(
                    "daemon",
                    format!("{app} runs one replica on every node and has no count to scale"),
                );
            }
            let mut after = spec;
            after.replicas = Replicas::Fixed(replicas);
            (
                ProposalKind::Scale,
                AppRevision::new(Some(after), before.stopped),
            )
        }
        RequestKind::Stop => {
            if before.stopped {
                return invalid(&format!("{app} is already stopped"));
            }
            (ProposalKind::Stop, AppRevision::new(Some(spec), true))
        }
        RequestKind::Delete => (ProposalKind::Delete, AppRevision::new(None, false)),
        RequestKind::Rollback => {
            let target = desired
                .rollouts
                .get(app)
                .and_then(|rollouts| {
                    crate::bun::cluster_view::released_rollback_target(rollouts, &spec)
                })
                .or_else(|| history_target.cloned());
            let Some(mut target) = target else {
                return invalid(&format!(
                    "{app} has no released spec that differs from the current one"
                ));
            };
            target.ordinals = None;
            target.owner = None;
            (
                ProposalKind::Rollback,
                AppRevision::new(Some(target), false),
            )
        }
        RequestKind::Restart => {
            if before.stopped {
                return invalid(&format!("{app} is stopped; nothing runs to restart"));
            }
            let mut after = spec;
            // Later than the stored stamp even if the leader's clock stepped
            // back, so the fingerprint always moves.
            let stamp = after
                .restarted_at
                .map_or(now_unix_ms, |at| now_unix_ms.max(at.saturating_add(1)));
            after.restarted_at = Some(stamp);
            (ProposalKind::Restart, AppRevision::new(Some(after), false))
        }
        RequestKind::CancelDeploy => {
            let target = rollout_in_progress(desired, app, &spec)?;
            (
                ProposalKind::CancelDeploy,
                AppRevision::new(Some(target), before.stopped),
            )
        }
        RequestKind::Apply | RequestKind::Undo | RequestKind::ClearFault => {
            return invalid("apply, undo and clear_fault don't name a single app");
        }
    };
    Ok((
        kind,
        AppChange {
            app: app.clone(),
            before,
            after,
        },
    ))
}

/// The spec cancelling `app`'s rollout goes back to: the newest one that
/// finished rolling out everywhere, at the current replica count. Refused
/// when the current spec is that one (nothing is rolling out) or when no
/// spec ever finished rolling out (there's nothing to go back to).
fn rollout_in_progress(
    desired: &DesiredState,
    app: &AppId,
    current: &AppSpec,
) -> Result<AppSpec, ProposalRefusal> {
    let Some(released) = desired
        .rollouts
        .get(app)
        .and_then(|rollouts| rollouts.last_released())
    else {
        return invalid(&format!(
            "{app} has no spec that finished rolling out to go back to; propose stop or delete \
             instead"
        ));
    };
    let fingerprint = crate::meat::deploy_types::rollout_fingerprint;
    if fingerprint(released) == fingerprint(current) {
        return invalid(&format!(
            "{app} has no rollout in progress: its current spec finished rolling out everywhere"
        ));
    }
    let mut target = released.clone();
    target.replicas = current.replicas;
    target.ordinals = None;
    target.owner = None;
    Ok(target)
}

fn apply_changes(
    desired: &DesiredState,
    request: &ProposalRequest,
    namespace: &str,
    fill: impl Fn(&AppId, &AppSpec) -> AppSpec,
) -> Result<Vec<AppChange>, ProposalRefusal> {
    let Some(text) = request.config.as_deref() else {
        return invalid("an apply carries the TOML in `config`");
    };
    let config = Config::parse(text).map_err(|error| ProposalRefusal::Invalid {
        reason: format!("the config doesn't parse: {error}"),
    })?;
    let others = [
        ("job", config.job.len()),
        ("namespace", config.namespace.len()),
        ("permission", config.permission.len()),
        ("policy", config.policy.len()),
        ("build", config.build.len()),
        ("template", config.template.len()),
        ("use", config.uses.len()),
    ];
    if let Some((table, _)) = others.iter().find(|(_, count)| *count > 0) {
        return refused(
            "apps_only",
            format!(
                "a proposal applies only [app.*] tables; [{table}.*] changes who may do what or \
                 the platform itself, so a person applies it"
            ),
        );
    }
    if config.app.is_empty() {
        return invalid("the config has no [app.*] tables");
    }
    if config.app.len() > MAX_APPS_PER_PROPOSAL {
        return Err(ProposalRefusal::Limit {
            reason: format!(
                "a proposal touches at most {MAX_APPS_PER_PROPOSAL} apps; a bigger change is a \
                 person's `relish apply`"
            ),
        });
    }
    config
        .validate()
        .map_err(|error| ProposalRefusal::Invalid {
            reason: format!("the config isn't valid: {error}"),
        })?;
    let mut changes = Vec::new();
    for (name, spec) in &config.app {
        let app_namespace = spec.namespace.as_deref().unwrap_or("default");
        if app_namespace != namespace {
            return invalid(&format!(
                "every app in a proposal is in namespace {namespace}, and {name} is in \
                 {app_namespace}"
            ));
        }
        let app = AppId::new(name, namespace);
        check_owned(desired, &app)?;
        if spec.owner.is_some() {
            return refused(
                "controller_owned",
                format!("{app} declares an owner; only a controller claims an app"),
            );
        }
        let before = current_app(desired, &app);
        let mut after = fill(&app, spec);
        // A file never carries the restart stamp, and `relish apply` keeps
        // the stored one, so a proposed apply does too: applying the same
        // file after a restart restarts nothing.
        if after.restarted_at.is_none() {
            after.restarted_at = before.spec.as_deref().and_then(|spec| spec.restarted_at);
        }
        changes.push(AppChange {
            app: app.clone(),
            before,
            after: AppRevision::new(Some(after), false),
        });
    }
    Ok(changes)
}

fn undo_changes(
    desired: &DesiredState,
    of: ProposalId,
    now_unix_ms: u64,
) -> Result<(String, Vec<AppChange>), ProposalRefusal> {
    let original = desired.proposals.undoable(of, now_unix_ms)?;
    // An undo starts from what the original left. If anyone has changed an
    // app since, putting `before` back would overwrite that change, so the
    // undo is stale from the start, as a person's direct undo is.
    let mut changes = Vec::with_capacity(original.changes.len());
    for change in &original.changes {
        if current_app(desired, &change.app).revision != change.after.revision {
            return Err(ProposalRefusal::Stale {
                app: change.app.to_string(),
                what: format!(
                    "after proposal {of} applied; undoing would overwrite that, so propose the \
                     change you want instead"
                ),
            });
        }
        changes.push(AppChange {
            app: change.app.clone(),
            before: change.after.clone(),
            after: change.after_undo(),
        });
    }
    Ok((original.namespace.clone(), changes))
}

impl AppChange {
    /// What undoing this change writes: the `before` spec and stopped flag.
    pub fn after_undo(&self) -> AppRevision {
        AppRevision::new(self.before.spec.as_deref().cloned(), self.before.stopped)
    }
}

/// Lossy when the change deletes an app with managed volumes (no `source`
/// on the host): undo brings back its spec, not their data. A restart
/// can't be undone: its instances have been replaced. Every other kind
/// here is exact in desired-state terms.
fn reversibility_of(kind: ProposalKind, changes: &[AppChange]) -> Reversibility {
    if matches!(kind, ProposalKind::Restart | ProposalKind::ClearFault) {
        return Reversibility::Irreversible;
    }
    let deletes_volumes = changes.iter().any(|change| {
        !change.after.exists()
            && change
                .before
                .spec
                .as_deref()
                .is_some_and(|spec| spec.volumes.iter().any(|volume| volume.source.is_none()))
    });
    if deletes_volumes {
        Reversibility::Lossy
    } else {
        Reversibility::Exact
    }
}

/// How many instances a revision runs: none when it's absent or stopped,
/// the count for a fixed one, `None` for a daemon (one per node).
fn running(revision: &AppRevision) -> Option<u32> {
    if !revision.exists() || revision.stopped {
        return Some(0);
    }
    revision.replicas()
}

/// What `changes` touch in `desired`: apps, the nodes they're placed on,
/// instances, routes and services. The leader computes it when the
/// proposal is made, and the state machine again when it's approved
/// ([`blast_radius_growth`]).
pub fn blast_radius(desired: &DesiredState, changes: &[AppChange]) -> BlastRadius {
    let mut radius = BlastRadius::default();
    let mut nodes = BTreeSet::new();
    let mut routes = BTreeSet::new();
    let mut services = BTreeSet::new();
    for change in changes {
        radius.apps.push(change.app.to_string());
        let placements = desired.scheduling.get(&change.app);
        for placement in placements.into_iter().flatten() {
            nodes.insert(placement.node_id.to_string());
        }
        for spec in [change.before.spec.as_deref(), change.after.spec.as_deref()]
            .into_iter()
            .flatten()
        {
            if let Some(ingress) = &spec.ingress {
                routes.insert(format!(
                    "{}{}",
                    ingress.host,
                    ingress.path.as_deref().unwrap_or("/")
                ));
            }
            if spec.port.is_some() {
                services.insert(format!("{}.{}", change.app.name, change.app.namespace));
            }
        }
        let (Some(before), Some(after)) = (running(&change.before), running(&change.after)) else {
            radius.unknown.push(format!(
                "{} runs one replica on every node; how many it touches depends on the nodes",
                change.app
            ));
            continue;
        };
        let new_version = match (change.before.spec.as_deref(), change.after.spec.as_deref()) {
            (Some(old), Some(new)) => {
                crate::meat::deploy_types::rollout_fingerprint(old)
                    != crate::meat::deploy_types::rollout_fingerprint(new)
            }
            _ => false,
        };
        if new_version {
            radius.instances_replaced += before.min(after);
        }
        radius.instances_started += after.saturating_sub(before);
        radius.instances_stopped += before.saturating_sub(after);
    }
    radius.nodes = nodes.into_iter().collect();
    radius.routes = routes.into_iter().collect();
    radius.services = services.into_iter().collect();
    radius
}

/// The field-by-field diff, through `relish diff`'s flattening, with every
/// value redacted and a secret's shown as `(secret)`.
fn field_diff(changes: &[AppChange]) -> Vec<FieldChange> {
    let mut diff = Vec::new();
    for change in changes {
        let app = change.app.to_string();
        let (before, after) = (change.before.spec.as_deref(), change.after.spec.as_deref());
        match (before, after) {
            (None, Some(_)) => diff.push(FieldChange {
                app: app.clone(),
                field: "app".to_string(),
                from: None,
                to: Some("created".to_string()),
            }),
            (Some(_), None) => diff.push(FieldChange {
                app: app.clone(),
                field: "app".to_string(),
                from: Some("present".to_string()),
                to: None,
            }),
            (Some(old), Some(new)) => {
                for field in crate::relish::diff::diff_app_specs(old, new) {
                    let secret = secret_field(old, &field.path) || secret_field(new, &field.path);
                    let shown = |value: Option<String>| {
                        value.map(|value| {
                            if secret {
                                "(secret)".to_string()
                            } else {
                                crate::redact::redact_secrets(&value)
                            }
                        })
                    };
                    diff.push(FieldChange {
                        app: app.clone(),
                        field: field.path,
                        from: shown(field.old),
                        to: shown(field.new),
                    });
                }
            }
            (None, None) => {}
        }
        if before.is_some() && after.is_some() && change.before.stopped != change.after.stopped {
            diff.push(FieldChange {
                app,
                field: "stopped".to_string(),
                from: Some(change.before.stopped.to_string()),
                to: Some(change.after.stopped.to_string()),
            });
        }
    }
    diff
}

/// Whether `path` is an environment variable whose value is a secret.
fn secret_field(spec: &AppSpec, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("env.") else {
        return false;
    };
    let key = rest.split('.').next().unwrap_or(rest);
    spec.env
        .get(key)
        .is_some_and(|value| !matches!(value, crate::config::types::EnvValue::Plain(_)))
}

/// How a summary names each end of a change between two specs, `before`
/// and `after`: by image when the images differ, otherwise by revision
/// (with the image, if there is one), so the two ends never read the same
/// (#1875).
fn ends_of(change: &AppChange) -> (String, String) {
    let image = |end: &AppRevision| end.spec.as_deref().and_then(|spec| spec.image.clone());
    let (before, after) = (image(&change.before), image(&change.after));
    if let (Some(before), Some(after)) = (&before, &after)
        && before != after
    {
        return (before.clone(), after.clone());
    }
    let named = |image: Option<String>, end: &AppRevision| {
        let short = super::view::short(&end.revision);
        match image {
            Some(image) => format!("{image} at revision {short}"),
            None => format!("revision {short}"),
        }
    };
    (named(before, &change.before), named(after, &change.after))
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

fn summary(
    kind: ProposalKind,
    changes: &[AppChange],
    radius: &BlastRadius,
    reversibility: Reversibility,
) -> Vec<String> {
    let mut lines = Vec::new();
    match reversibility {
        Reversibility::Lossy => lines.push(
            "UNDO IS PARTIAL: deleting an app loses the data on its volumes; undo brings back \
             the spec, not the data."
                .to_string(),
        ),
        Reversibility::Irreversible => {
            lines.push("CAN'T BE UNDONE: think before approving.".to_string());
        }
        Reversibility::Exact => {}
    }
    let first = &changes[0];
    lines.push(match kind {
        ProposalKind::Scale => format!(
            "Scale {} from {} to {} replicas.",
            first.app,
            first.before.replicas().unwrap_or(0),
            first.after.replicas().unwrap_or(0)
        ),
        ProposalKind::Stop => format!("Stop {}.", first.app),
        ProposalKind::Delete => format!(
            "Delete {}. Its instances' local state goes with it.",
            first.app
        ),
        ProposalKind::Rollback => {
            let (from, to) = ends_of(first);
            format!("Roll {} back from {from} to {to}.", first.app)
        }
        ProposalKind::Restart => format!(
            "Restart {}: replace its instances node by node, through its health checks, on \
             the same spec.",
            first.app
        ),
        ProposalKind::CancelDeploy => {
            let (rolling_out, released) = ends_of(first);
            format!(
                "Cancel the rollout of {} to {rolling_out}: put it back on {released}, the \
                 last spec that finished rolling out; nodes already on the new spec roll back.",
                first.app
            )
        }
        // `clear_fault_proposal` writes a fault's summary; it changes no app.
        ProposalKind::ClearFault => format!("Clear a fault on {}.", first.app),
        ProposalKind::Apply => {
            let described: Vec<String> = changes
                .iter()
                .map(|change| {
                    let verb = if change.before.exists() {
                        "change"
                    } else {
                        "create"
                    };
                    format!("{verb} {}", change.app)
                })
                .collect();
            format!(
                "Apply {}: {}.",
                plural(changes.len(), "app"),
                described.join(", ")
            )
        }
        ProposalKind::Undo { of } => format!(
            "Undo proposal {of}: put {} back as {} before it.",
            radius.apps.join(", "),
            if changes.len() == 1 {
                "it was"
            } else {
                "they were"
            }
        ),
    });
    let mut effects = Vec::new();
    if radius.instances_replaced > 0 {
        effects.push(format!(
            "replaces {}",
            plural(radius.instances_replaced as usize, "instance")
        ));
    }
    if radius.instances_started > 0 {
        effects.push(starts(radius));
    }
    if radius.instances_stopped > 0 {
        effects.push(format!(
            "stops {}",
            plural(radius.instances_stopped as usize, "instance")
        ));
    }
    let nodes = if radius.nodes.is_empty() {
        "no nodes yet".to_string()
    } else {
        format!(
            "{} ({})",
            plural(radius.nodes.len(), "node"),
            radius.nodes.join(", ")
        )
    };
    let mut touches = format!("Touches {} on {nodes}", plural(radius.apps.len(), "app"));
    if !effects.is_empty() {
        touches.push_str("; ");
        touches.push_str(&effects.join(", "));
    }
    touches.push('.');
    lines.push(touches);
    let mut serves = Vec::new();
    serves.extend(radius.routes.iter().map(|route| format!("https://{route}")));
    serves.extend(
        radius
            .services
            .iter()
            .map(|service| format!("the service {service}")),
    );
    if !serves.is_empty() {
        lines.push(format!("Serves {}.", serves.join(" and ")));
    }
    for unknown in &radius.unknown {
        lines.push(format!("Unknown: {unknown}."));
    }
    lines
}

/// "starts 2 instances", with what the scheduler's dry run said about
/// them.
fn starts(radius: &BlastRadius) -> String {
    let mut line = format!(
        "starts {}",
        plural(radius.instances_started as usize, "instance")
    );
    if !radius.new_nodes_forecast.is_empty() {
        line.push_str(&format!(
            ", forecast on {}",
            radius.new_nodes_forecast.join(", ")
        ));
    }
    match (radius.unplaced_forecast, radius.forecast_note.as_deref()) {
        (0, None) if radius.new_nodes_forecast.is_empty() => {
            line.push_str(" (the scheduler places them when it applies)");
        }
        (0, None) => {}
        (0, Some(note)) => line.push_str(&format!(
            " (no forecast: {note}; the scheduler places them when it applies)"
        )),
        (unplaced, note) => {
            line.push_str(&format!("; the scheduler can't place {unplaced}"));
            if let Some(note) = note {
                line.push_str(&format!(" ({note})"));
            }
        }
    }
    line
}

/// The scheduler's dry run for a proposal that starts instances: where
/// they'd go, and how many it couldn't place and why.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forecast {
    /// The node each placed instance would go to.
    pub nodes: Vec<String>,
    /// How many it couldn't place.
    pub unplaced: u32,
    /// Why not, or why there's no forecast at all.
    pub note: Option<String>,
}

/// Put the scheduler's `forecast` into a proposal the leader has built,
/// and write its summary again to say so. Called before it's numbered.
pub fn with_forecast(proposal: &mut Proposal, forecast: Forecast) {
    let mut nodes = forecast.nodes;
    nodes.sort();
    nodes.dedup();
    proposal.blast_radius.new_nodes_forecast = nodes;
    proposal.blast_radius.unplaced_forecast = forecast.unplaced;
    proposal.blast_radius.forecast_note = forecast.note;
    if !proposal.changes.is_empty() {
        proposal.summary = summary(
            proposal.kind,
            &proposal.changes,
            &proposal.blast_radius,
            proposal.reversibility,
        );
    }
}

/// How `proposal`'s blast radius has grown since it was made, judged
/// against `desired` now, or `None` while it hasn't: a node it didn't
/// touch, or more instances. The state machine fails an approval when it
/// has, and `relish status` calls such a proposal stale, in these words.
pub fn blast_radius_growth(desired: &DesiredState, proposal: &Proposal) -> Option<String> {
    if proposal.compacted || proposal.changes.is_empty() {
        return None;
    }
    let was = &proposal.blast_radius;
    let now = blast_radius(desired, &proposal.changes);
    let new_nodes: Vec<&str> = now
        .nodes
        .iter()
        .filter(|node| !was.nodes.contains(node))
        .map(String::as_str)
        .collect();
    if !new_nodes.is_empty() {
        return Some(format!(
            "its blast radius grew after it was made: it now touches {} too",
            new_nodes.join(", ")
        ));
    }
    if now.instances() > was.instances() {
        return Some(format!(
            "its blast radius grew after it was made: it now touches {} instances, not {}",
            now.instances(),
            was.instances()
        ));
    }
    None
}

fn undo_effect(kind: ProposalKind, changes: &[AppChange], reversibility: Reversibility) -> String {
    let first = &changes[0];
    let effect = match kind {
        ProposalKind::Scale => format!(
            "puts {} back to {} replicas",
            first.app.name,
            first.before.replicas().unwrap_or(0)
        ),
        ProposalKind::Stop => format!("starts {} again with the same spec", first.app.name),
        ProposalKind::Delete => format!("creates {} again from its spec", first.app.name),
        ProposalKind::Rollback => {
            format!("puts {} back on {}", first.app.name, ends_of(first).0)
        }
        ProposalKind::Apply => {
            let created: Vec<&str> = changes
                .iter()
                .filter(|change| !change.before.exists())
                .map(|change| change.app.name.as_str())
                .collect();
            if created.is_empty() {
                "puts every app back as it was".to_string()
            } else {
                format!(
                    "puts every app back as it was and deletes {}, which it created",
                    created.join(", ")
                )
            }
        }
        ProposalKind::Restart => {
            "a restart can't be undone: its instances have already been replaced".to_string()
        }
        ProposalKind::CancelDeploy => format!(
            "puts {} back on {}, which starts its rollout again",
            first.app.name,
            ends_of(first).0
        ),
        ProposalKind::ClearFault => "a cleared fault can't be put back".to_string(),
        ProposalKind::Undo { of } => format!("puts proposal {of}'s change back"),
    };
    match reversibility {
        Reversibility::Lossy => format!("{effect}; the data on its volumes doesn't come back"),
        _ => effect,
    }
}

/// Each `apply` app's spec with nothing filled in: what a caller with no
/// policies passes as `fill`.
pub fn as_declared(_app: &AppId, spec: &AppSpec) -> AppSpec {
    spec.clone()
}

/// Every app a request names before it's built, for authorising the
/// caller: the `app`, or each `[app.*]` in `config`. Empty for `undo`,
/// whose apps come from the original proposal, and for `clear_fault`,
/// whose app comes from the fault.
pub fn requested_apps(request: &ProposalRequest) -> Result<Vec<AppId>, ProposalRefusal> {
    let namespace = request.namespace.as_deref().unwrap_or("default");
    match request.kind {
        RequestKind::Undo | RequestKind::ClearFault => Ok(Vec::new()),
        RequestKind::Apply => {
            let Some(text) = request.config.as_deref() else {
                return invalid("an apply carries the TOML in `config`");
            };
            let config = Config::parse(text).map_err(|error| ProposalRefusal::Invalid {
                reason: format!("the config doesn't parse: {error}"),
            })?;
            Ok(config
                .app
                .iter()
                .map(|(name, spec)| {
                    AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
                })
                .collect())
        }
        _ => match request.app.as_deref() {
            Some(app) => Ok(vec![AppId::new(app, namespace)]),
            None => invalid("this kind names an app in `app`"),
        },
    }
}

#[cfg(test)]
#[path = "build_tests.rs"]
mod tests;
