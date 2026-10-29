//! Policies in the council: `[policy.*]` deny and warn rules (#1117).
//!
//! A policy is two CEL expressions: `match` picks the apps it applies to
//! and `deny` says when to refuse one. It can also carry a `default` table
//! of fields to fill in where an app leaves them out (#1118). This module
//! holds the one pure function that judges an app, [`judge_app`], with
//! [`evaluate`] running it over a config, and [`admit`], which wraps it
//! around the shared desired-state writes.
//!
//! Judging goes in a fixed order: every matching `default` in policy-name
//! order, then the built-in app validation on the filled spec, then every
//! `deny`. Nothing runs twice.
//!
//! The leader calls [`admit`] in `cluster_apply` and the Lettuce runner
//! calls [`evaluate_app`], right before their writes, so `relish apply` and
//! GitOps enforce the same rules. `relish policy test` and `relish apply
//! --dry-run` call [`evaluate`] on a laptop. Nothing here reads a clock, the
//! network or any state that isn't an argument.
//!
//! The state machine never evaluates a policy. Each app write the leader
//! proposes is wrapped in `RaftRequest::PolicyFenced`, stamped with the
//! policy generation it was evaluated against; the state machine compares
//! that number with its own and refuses the write if the policy set has
//! moved, so the apply evaluates again. Chapter 18 explains why.

pub mod audit;
mod check;
mod defaults;
pub mod report;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use cel::{Context, Env, Program, Value};

use crate::config::{AppSpec, Config, JobSpec, PolicyAction, PolicySpec, PolicyTarget};
use crate::council::types::RaftRequest;
use crate::meat::types::AppId;
use crate::sesame::types::ApiRole;

pub use check::{
    APP_FIELDS, CALLER_FIELDS, JOB_FIELDS, MAX_DEFAULT_BYTES, MAX_EXPRESSION_BYTES,
    MAX_MESSAGE_BYTES, POLICY_VARIABLES, PolicyError, PolicyField, UNFILLABLE_FIELDS,
    WORKLOAD_FIELDS, app_fields, check_policy, workload_variables,
};
pub use defaults::Filled;

/// What `GET /v1/policies` answers: every policy, by name, and the
/// generation the set is at.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PolicyListing {
    /// The policies in force, keyed by name.
    pub policies: std::collections::BTreeMap<String, crate::config::PolicySpec>,
    /// How many policy writes have applied.
    pub generation: u64,
    /// The policies GitOps wrote, which the next sync puts back if they're
    /// deleted while git still declares them.
    #[serde(default)]
    pub gitops_owned: std::collections::BTreeSet<String>,
}

/// Where a change came from, as a policy sees it in `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `relish apply`, or anything else that posts to `/v1/apply`, and every
    /// other route that submits a job (#1424).
    Apply,
    /// A GitOps sync.
    GitOps,
}

impl Source {
    /// The value of `source` in an expression.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Apply => "apply",
            Source::GitOps => "gitops",
        }
    }
}

/// Who asked for a change, as a policy sees it in `caller`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The token's name, or `gitops` for a sync.
    pub name: String,
    /// The token's role. `None` is the cluster itself (a GitOps sync), which
    /// a policy sees as the role `system`.
    pub role: Option<ApiRole>,
}

impl Caller {
    /// The caller a GitOps sync applies as.
    pub fn gitops() -> Self {
        Caller {
            name: "gitops".to_string(),
            role: None,
        }
    }

    /// A caller with a token.
    pub fn token(name: &str, role: ApiRole) -> Self {
        Caller {
            name: name.to_string(),
            role: Some(role),
        }
    }

    /// The caller `relish policy audit` judges committed workloads as:
    /// the cluster itself, named `audit` (#1119).
    pub fn audit() -> Self {
        Caller {
            name: "audit".to_string(),
            role: None,
        }
    }

    fn role_name(&self) -> String {
        self.role
            .map_or_else(|| "system".to_string(), |role| role.to_string())
    }
}

/// The kinds of workload a policy judges (#1119).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum WorkloadKind {
    /// An `[app.*]`.
    App,
    /// A `[job.*]`.
    Job,
}

impl WorkloadKind {
    /// The word used in output and as the expression variable.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadKind::App => "app",
            WorkloadKind::Job => "job",
        }
    }

    fn judged_by(self, target: PolicyTarget) -> bool {
        matches!(
            (self, target),
            (_, PolicyTarget::Both)
                | (WorkloadKind::App, PolicyTarget::Apps)
                | (WorkloadKind::Job, PolicyTarget::Jobs)
        )
    }
}

/// One policy a workload broke.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Finding {
    /// The policy's name.
    pub policy: String,
    /// Whether it refuses, warns or is only recorded.
    pub action: PolicyAction,
    /// Whether an app or a job broke it.
    pub kind: WorkloadKind,
    /// The workload that broke it.
    pub workload: AppId,
    /// The policy's message, or why it couldn't be evaluated.
    pub message: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "policy {} {} {} {}/{}: {}",
            self.policy,
            match self.action {
                PolicyAction::Deny => "denies",
                PolicyAction::Warn => "warns about",
                PolicyAction::Audit => "audits",
            },
            self.kind.as_str(),
            self.workload.namespace,
            self.workload.name,
            self.message
        )
    }
}

/// Everything the policies said about some workloads: what they refuse,
/// what they only warn about, and what they only record. Each list is in
/// workload order (apps, then jobs), then policy name order.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Verdict {
    /// Findings from `deny` policies, and from any policy that couldn't be
    /// evaluated: a broken deny policy fails closed.
    pub denials: Vec<Finding>,
    /// Findings from `warn` policies.
    pub warnings: Vec<Finding>,
    /// Findings from `audit` policies (#1119): recorded, never shown to
    /// the caller as a warning, never refusing.
    #[serde(default)]
    pub audits: Vec<Finding>,
}

impl Verdict {
    /// Whether nothing was refused.
    pub fn is_allowed(&self) -> bool {
        self.denials.is_empty()
    }

    /// Whether the policies said nothing at all.
    pub fn is_empty(&self) -> bool {
        self.denials.is_empty() && self.warnings.is_empty() && self.audits.is_empty()
    }

    fn extend(&mut self, other: Verdict) {
        self.denials.extend(other.denials);
        self.warnings.extend(other.warnings);
        self.audits.extend(other.audits);
    }

    fn push(&mut self, finding: Finding) {
        match finding.action {
            PolicyAction::Deny => self.denials.push(finding),
            PolicyAction::Warn => self.warnings.push(finding),
            PolicyAction::Audit => self.audits.push(finding),
        }
    }
}

/// A refused apply: every denial, named.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", denial_lines(.denials))]
pub struct Denied {
    /// What the `deny` policies refused.
    pub denials: Vec<Finding>,
    /// What the `warn` policies said, for the same apply.
    pub warnings: Vec<Finding>,
}

fn denial_lines(denials: &[Finding]) -> String {
    denials
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Why [`admit`] produced no writes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmitError {
    /// A policy refused an app.
    #[error("{0}")]
    Denied(Denied),
    /// A policy in force no longer compiles (a `cel` upgrade, say). The
    /// apply fails closed until an admin replaces or deletes it.
    #[error(
        "policy {name} can't be evaluated: {error}; an admin can replace it or run `relish policy delete {name}`"
    )]
    Broken {
        /// The policy's name.
        name: String,
        /// Why it doesn't compile.
        error: PolicyError,
    },
}

/// The writes an admitted apply makes, and what the policies warned about.
#[derive(Debug, Clone, PartialEq)]
pub struct Admitted {
    /// The writes, in order, each app write fenced on [`Self::generation`].
    pub writes: Vec<RaftRequest>,
    /// What the `warn` policies said.
    pub warnings: Vec<Finding>,
    /// What the `audit` policies recorded (#1119).
    pub audits: Vec<Finding>,
    /// The policy generation the app writes are fenced on: the committed
    /// one, plus one for each policy this config writes before its apps.
    pub generation: u64,
    /// Every field a policy default filled, in app order, then policy
    /// name order.
    pub filled: Vec<Filled>,
    /// The apps a default changed, by identity: the spec as declared and
    /// the spec to store.
    pub defaulted: BTreeMap<AppId, DefaultedApp>,
}

/// An app a policy default changed (#1118).
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultedApp {
    /// The spec as the config declared it. Stored beside the effective
    /// one, so GitOps compares git with what git said, not with what the
    /// policies added.
    pub declared: AppSpec,
    /// The spec with the defaults filled in: what runs.
    pub effective: AppSpec,
}

/// What the policies made of one app: the spec to store, the fields the
/// defaults filled in, and what the `deny` policies said.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgement {
    /// The spec with every matching default filled in.
    pub spec: AppSpec,
    /// The fields filled, in policy name order.
    pub filled: Vec<Filled>,
    /// The denials and warnings, judged on `spec`.
    pub verdict: Verdict,
}

/// What the policies made of a config's apps.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Evaluation {
    /// Each app with its defaults filled in, by its name in the config.
    #[serde(skip)]
    pub apps: BTreeMap<String, AppSpec>,
    /// Every field filled, in app order, then policy name order.
    pub filled: Vec<Filled>,
    /// Every denial and warning.
    #[serde(flatten)]
    pub verdict: Verdict,
}

impl Evaluation {
    /// The apps whose spec a default changed, by identity, with the spec
    /// `config` declared beside the effective one.
    pub fn defaulted(&self, config: &Config) -> BTreeMap<AppId, DefaultedApp> {
        self.apps
            .iter()
            .filter_map(|(name, effective)| {
                let declared = config.app.get(name)?;
                (declared != effective).then(|| {
                    (
                        app_id_of(name, declared),
                        DefaultedApp {
                            declared: declared.clone(),
                            effective: effective.clone(),
                        },
                    )
                })
            })
            .collect()
    }
}

/// One compiled policy.
struct CompiledPolicy {
    name: String,
    action: PolicyAction,
    target: PolicyTarget,
    message: Option<String>,
    match_program: Option<Program>,
    deny_program: Option<Program>,
    default: Option<serde_json::Map<String, serde_json::Value>>,
}

/// A set of policies compiled once, ready to evaluate against many apps.
pub struct PolicySet {
    environment: Arc<Env>,
    /// In name order, so findings come out the same way on every node.
    policies: Vec<CompiledPolicy>,
}

impl std::fmt::Debug for PolicySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicySet")
            .field(
                "policies",
                &self.policies.iter().map(|p| &p.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The CEL environment policies compile and run in: the standard library
/// and the strings extension (`lowerAscii`, `split`, …), nothing else.
pub(crate) fn environment() -> Env {
    let mut env = Env::stdlib();
    // The extension only declares functions; declaring them on a fresh
    // standard environment can't collide, so a failure here would be a bug
    // in the crate, and the expressions using them just fail to evaluate.
    let _ = env.add_extension(cel::extensions::strings);
    env
}

impl PolicySet {
    /// Compile `specs`. Fails on the first policy that wouldn't pass
    /// [`check_policy`], naming it.
    pub fn compile(specs: &BTreeMap<String, PolicySpec>) -> Result<Self, AdmitError> {
        let broken = |name: &str| {
            let name = name.to_string();
            move |error| AdmitError::Broken { name, error }
        };
        let mut policies = Vec::with_capacity(specs.len());
        for (name, spec) in specs {
            check_policy(name, spec).map_err(broken(name))?;
            let match_program = spec
                .match_expression
                .as_deref()
                .map(|expression| {
                    check::compile_checked(PolicyField::Match, spec.applies_to, expression)
                })
                .transpose()
                .map_err(broken(name))?;
            let deny_program = spec
                .deny
                .as_deref()
                .map(|expression| {
                    check::compile_checked(PolicyField::Deny, spec.applies_to, expression)
                })
                .transpose()
                .map_err(broken(name))?;
            policies.push(CompiledPolicy {
                name: name.clone(),
                action: spec.action,
                target: spec.applies_to,
                message: spec.message.clone(),
                match_program,
                deny_program,
                default: spec.default.clone(),
            });
        }
        Ok(PolicySet {
            environment: Arc::new(environment()),
            policies,
        })
    }

    /// Whether the set holds no policies.
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }
}

/// Evaluate every policy in `set` against one app.
///
/// A policy whose `match` is absent or true, and whose `deny` is true,
/// produces a finding. An expression that fails to evaluate (it reads a
/// field the app doesn't set, or returns something other than a boolean)
/// produces one too, saying why: a `deny` policy fails closed.
pub fn evaluate_app(
    set: &PolicySet,
    app_id: &AppId,
    spec: &AppSpec,
    caller: &Caller,
    source: Source,
) -> Verdict {
    let value = workload_value(app_id, spec);
    evaluate_workload(set, WorkloadKind::App, app_id, value, caller, source)
}

/// Judge one job by every policy in `set` that applies to jobs (#1119).
/// Defaults never touch a job: a policy with a `default` applies to apps
/// only.
pub fn judge_job(
    set: &PolicySet,
    job_id: &AppId,
    spec: &JobSpec,
    caller: &Caller,
    source: Source,
) -> Verdict {
    let value = workload_value(job_id, spec);
    evaluate_workload(set, WorkloadKind::Job, job_id, value, caller, source)
}

/// Evaluate every `deny` in `set` that applies to `kind` against one
/// workload, seen as `value`.
fn evaluate_workload(
    set: &PolicySet,
    kind: WorkloadKind,
    id: &AppId,
    value: Value,
    caller: &Caller,
    source: Source,
) -> Verdict {
    let mut verdict = Verdict::default();
    if set.is_empty() {
        return verdict;
    }
    let context = policy_context(set, kind, value, caller, source);
    for policy in &set.policies {
        if !kind.judged_by(policy.target) {
            continue;
        }
        let Some(deny_program) = &policy.deny_program else {
            continue;
        };
        let broken = match decide(policy, deny_program, &context) {
            Ok(false) => continue,
            Ok(true) => None,
            Err(reason) => Some(reason),
        };
        let message = match broken {
            Some(reason) => format!("couldn't evaluate: {reason}"),
            None => policy
                .message
                .clone()
                .unwrap_or_else(|| format!("breaks policy {}", policy.name)),
        };
        verdict.push(Finding {
            policy: policy.name.clone(),
            action: policy.action,
            kind,
            workload: id.clone(),
            message,
        });
    }
    verdict
}

/// The variables an expression sees for one workload: `app` or `job` by
/// its kind, `workload` for either, `caller` and `source`.
fn policy_context(
    set: &PolicySet,
    kind: WorkloadKind,
    value: Value,
    caller: &Caller,
    source: Source,
) -> Context<'static, 'static> {
    let mut context = Context::with_env(Arc::clone(&set.environment));
    context.add_variable_from_value(kind.as_str(), value.clone());
    context.add_variable_from_value("workload", value);
    context.add_variable_from_value(
        "caller",
        HashMap::from([
            ("name".to_string(), Value::from(caller.name.clone())),
            ("role".to_string(), Value::from(caller.role_name())),
        ]),
    );
    context.add_variable_from_value("source", source.as_str());
    context
}

/// Whether `policy` applies to the app in `context` and its rule is
/// broken, or why that couldn't be decided.
fn decide(policy: &CompiledPolicy, deny: &Program, context: &Context) -> Result<bool, String> {
    if !matches(policy, context)? {
        return Ok(false);
    }
    run(PolicyField::Deny, deny, context)
}

/// Whether `policy` applies to the app in `context`: its `match` is
/// absent or true.
fn matches(policy: &CompiledPolicy, context: &Context) -> Result<bool, String> {
    match &policy.match_program {
        Some(program) => run(PolicyField::Match, program, context),
        None => Ok(true),
    }
}

/// Judge one app: fill in every matching policy's `default`, in policy
/// name order, then run the built-in app validation on the filled spec,
/// then every `deny` on it.
///
/// Each policy's `match` sees the spec as the policies before it left it.
/// A default fills only fields the spec leaves out, so a value the app
/// sets always survives. A default that can't be applied, or a `match`
/// that can't be evaluated, refuses the app: it fails closed, as a broken
/// `deny` does. So does a filled spec that fails validation, naming the
/// policies that filled it.
pub fn judge_app(
    set: &PolicySet,
    app_id: &AppId,
    declared: &AppSpec,
    caller: &Caller,
    source: Source,
) -> Judgement {
    let mut spec = declared.clone();
    let mut filled: Vec<Filled> = Vec::new();
    let mut verdict = Verdict::default();
    for policy in &set.policies {
        let Some(default) = &policy.default else {
            continue;
        };
        let value = workload_value(app_id, &spec);
        let context = policy_context(set, WorkloadKind::App, value, caller, source);
        let outcome = match matches(policy, &context) {
            Ok(true) => defaults::apply_default(&spec, default)
                .map(Some)
                .map_err(|reason| format!("its default couldn't be filled in: {reason}")),
            Ok(false) => Ok(None),
            Err(reason) => Err(format!("couldn't evaluate: {reason}")),
        };
        match outcome {
            Ok(Some((next, fields))) => {
                spec = next;
                filled.extend(fields.into_iter().map(|(field, value)| Filled {
                    policy: policy.name.clone(),
                    app: app_id.clone(),
                    field,
                    value,
                }));
            }
            Ok(None) => {}
            Err(message) => verdict.denials.push(Finding {
                policy: policy.name.clone(),
                action: PolicyAction::Deny,
                kind: WorkloadKind::App,
                workload: app_id.clone(),
                message,
            }),
        }
    }
    if !filled.is_empty()
        && let Err(error) = crate::config::validate_app_spec(&app_id.name, &spec)
    {
        let mut policies: Vec<&str> = filled.iter().map(|fill| fill.policy.as_str()).collect();
        policies.dedup();
        verdict.denials.push(Finding {
            policy: policies.join(","),
            action: PolicyAction::Deny,
            kind: WorkloadKind::App,
            workload: app_id.clone(),
            message: format!("the app isn't valid once its defaults are filled in: {error}"),
        });
    }
    verdict.extend(evaluate_app(set, app_id, &spec, caller, source));
    Judgement {
        spec,
        filled,
        verdict,
    }
}

fn run(field: PolicyField, program: &Program, context: &Context) -> Result<bool, String> {
    // The crate is young, and a panic here would take the leader's apply
    // task (or the whole GitOps loop) with it. Catching it turns a crate
    // bug into a refused apply, which an admin can see and fix.
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| program.execute(context)));
    match outcome {
        Ok(Ok(Value::Bool(value))) => Ok(value),
        Ok(Ok(other)) => Err(format!(
            "{field} returned {:?}, not a boolean",
            other.type_of()
        )),
        Ok(Err(error)) => Err(format!("{field}: {error}")),
        Err(_) => Err(format!("{field}: the evaluator panicked")),
    }
}

/// A workload as a policy sees it: its spec as JSON, with `name` and the
/// resolved `namespace` added.
fn workload_value(id: &AppId, spec: &impl serde::Serialize) -> Value {
    let mut json = serde_json::to_value(spec).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut json {
        map.insert("name".into(), id.name.clone().into());
        map.insert("namespace".into(), id.namespace.clone().into());
    }
    json_to_cel(json)
}

/// Turn JSON into a CEL value. Whole numbers become `int`, so
/// `app.port == 8080` compares like with like.
fn json_to_cel(json: serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(value) => Value::Bool(value),
        serde_json::Value::Number(number) => match (number.as_i64(), number.as_u64()) {
            (Some(value), _) => Value::Int(value),
            (None, Some(value)) => Value::UInt(value),
            (None, None) => Value::Float(number.as_f64().unwrap_or(f64::NAN)),
        },
        serde_json::Value::String(value) => Value::from(value),
        serde_json::Value::Array(items) => {
            Value::List(Arc::new(items.into_iter().map(json_to_cel).collect()))
        }
        serde_json::Value::Object(map) => Value::from(
            map.into_iter()
                .map(|(key, value)| (key, json_to_cel(value)))
                .collect::<HashMap<String, Value>>(),
        ),
    }
}

/// The identity an app in a config gets: its `namespace`, or `default`.
fn app_id_of(name: &str, spec: &AppSpec) -> AppId {
    AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
}

/// The identity a job in a config gets: its `namespace`, or `default`.
fn job_id_of(name: &str, spec: &JobSpec) -> AppId {
    AppId::new(name, spec.namespace.as_deref().unwrap_or("default"))
}

/// Judge every app, then every job, in `config` by `set`, in name order.
pub fn evaluate(set: &PolicySet, config: &Config, caller: &Caller, source: Source) -> Evaluation {
    let mut evaluation = Evaluation::default();
    for (name, spec) in &config.app {
        let judgement = judge_app(set, &app_id_of(name, spec), spec, caller, source);
        evaluation.apps.insert(name.clone(), judgement.spec);
        evaluation.filled.extend(judgement.filled);
        evaluation.verdict.extend(judgement.verdict);
    }
    for (name, spec) in &config.job {
        evaluation
            .verdict
            .extend(judge_job(set, &job_id_of(name, spec), spec, caller, source));
    }
    evaluation
}

/// The policies an apply of `config` runs under: the committed ones, with
/// the config's own policies replacing any of the same name. A config that
/// declares a policy and an app is judged by the new policy.
pub fn effective_policies(
    committed: &BTreeMap<String, PolicySpec>,
    config: &Config,
) -> BTreeMap<String, PolicySpec> {
    let mut effective = committed.clone();
    effective.extend(
        config
            .policy
            .iter()
            .map(|(name, spec)| (name.clone(), spec.clone())),
    );
    effective
}

/// Decide whether `config` may be applied, and fence its writes.
///
/// `committed` and `generation` are the policies in force and their
/// generation, read together from desired state; `writes` are the config's
/// desired-state writes (`config_to_desired_writes`, or the leased ones).
/// The config's apps are judged under [`effective_policies`]. A denial
/// returns no writes at all. Otherwise each app write carries the app's
/// spec with its defaults filled in, and is wrapped in
/// `RaftRequest::PolicyFenced`, stamped with the generation desired state
/// will have once this config's policy writes, which come first, have
/// applied: each policy write moves it by one. A defaulted app's fence
/// also carries the spec as declared.
pub fn admit(
    committed: &BTreeMap<String, PolicySpec>,
    generation: u64,
    config: &Config,
    caller: &Caller,
    source: Source,
    writes: Vec<RaftRequest>,
) -> Result<Admitted, AdmitError> {
    let mut evaluation = Evaluation::default();
    if !config.app.is_empty() || !config.job.is_empty() {
        let set = PolicySet::compile(&effective_policies(committed, config))?;
        evaluation = evaluate(&set, config, caller, source);
        if !evaluation.verdict.is_allowed() {
            return Err(AdmitError::Denied(Denied {
                denials: evaluation.verdict.denials,
                warnings: evaluation.verdict.warnings,
            }));
        }
    }
    let generation = generation.saturating_add(config.policy.len() as u64);
    let defaulted = evaluation.defaulted(config);
    Ok(Admitted {
        writes: fence(
            with_defaults(writes, &defaulted),
            generation,
            &declared_apps(config),
        ),
        warnings: evaluation.verdict.warnings,
        audits: evaluation.verdict.audits,
        generation,
        filled: evaluation.filled,
        defaulted,
    })
}

/// `writes` with each defaulted app's spec replaced by its effective one.
fn with_defaults(
    writes: Vec<RaftRequest>,
    defaulted: &BTreeMap<AppId, DefaultedApp>,
) -> Vec<RaftRequest> {
    writes
        .into_iter()
        .map(|mut write| {
            if let RaftRequest::AppSpec { app_id, spec }
            | RaftRequest::TestLeaseAppSpec { app_id, spec, .. } = &mut write
                && let Some(app) = defaulted.get(app_id)
            {
                **spec = app.effective.clone();
            }
            write
        })
        .collect()
}

/// Wrap every app write in `writes` in a policy fence at `generation`.
/// Other writes pass through unchanged: only apps are judged by policy.
///
/// `declared` holds each app's spec as its config declared it. When the
/// spec a write stores differs from that, because a policy default filled
/// fields in or an image was bound to a digest, the fence carries the
/// declared spec, so the cluster keeps it beside the stored one (#1118).
pub fn fence(
    writes: Vec<RaftRequest>,
    generation: u64,
    declared: &BTreeMap<AppId, AppSpec>,
) -> Vec<RaftRequest> {
    writes
        .into_iter()
        .map(|write| match &write {
            RaftRequest::AppSpec { app_id, spec }
            | RaftRequest::TestLeaseAppSpec { app_id, spec, .. } => {
                let declared = declared
                    .get(app_id)
                    .filter(|declared| *declared != spec.as_ref())
                    .map(|declared| Box::new(declared.clone()));
                RaftRequest::PolicyFenced {
                    generation,
                    change: Box::new(write),
                    declared,
                }
            }
            _ => write,
        })
        .collect()
}

/// Each app in `config` by identity, as declared: what [`fence`] compares
/// a write's spec with.
pub fn declared_apps(config: &Config) -> BTreeMap<AppId, AppSpec> {
    config
        .app
        .iter()
        .map(|(name, spec)| (app_id_of(name, spec), spec.clone()))
        .collect()
}

#[cfg(test)]
mod tests;
