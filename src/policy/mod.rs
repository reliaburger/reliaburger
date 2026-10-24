//! Policies in the council: `[policy.*]` deny and warn rules (#1117).
//!
//! A policy is two CEL expressions: `match` picks the apps it applies to
//! and `deny` says when to refuse one. This module holds the one pure
//! function that evaluates them, [`evaluate`], and [`admit`], which wraps it
//! around the shared desired-state writes.
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

mod check;
pub mod report;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use cel::{Context, Env, Program, Value};

use crate::config::{AppSpec, Config, PolicyAction, PolicySpec};
use crate::council::types::RaftRequest;
use crate::meat::types::AppId;
use crate::sesame::types::ApiRole;

pub use check::{
    APP_FIELDS, CALLER_FIELDS, MAX_EXPRESSION_BYTES, MAX_MESSAGE_BYTES, POLICY_VARIABLES,
    PolicyError, PolicyField, app_fields, check_policy,
};

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
    /// `relish apply`, or anything else that posts to `/v1/apply`.
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

    fn role_name(&self) -> String {
        self.role
            .map_or_else(|| "system".to_string(), |role| role.to_string())
    }
}

/// One policy an app broke.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Finding {
    /// The policy's name.
    pub policy: String,
    /// Whether it refuses or warns.
    pub action: PolicyAction,
    /// The app that broke it.
    pub app: AppId,
    /// The policy's message, or why it couldn't be evaluated.
    pub message: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "policy {} {} app {}/{}: {}",
            self.policy,
            match self.action {
                PolicyAction::Deny => "denies",
                PolicyAction::Warn => "warns about",
            },
            self.app.namespace,
            self.app.name,
            self.message
        )
    }
}

/// Everything the policies said about some apps: what they refuse and
/// what they only warn about. Both lists are in app order, then policy
/// name order.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Verdict {
    /// Findings from `deny` policies, and from any policy that couldn't be
    /// evaluated: a broken deny policy fails closed.
    pub denials: Vec<Finding>,
    /// Findings from `warn` policies.
    pub warnings: Vec<Finding>,
}

impl Verdict {
    /// Whether nothing was refused.
    pub fn is_allowed(&self) -> bool {
        self.denials.is_empty()
    }

    fn extend(&mut self, other: Verdict) {
        self.denials.extend(other.denials);
        self.warnings.extend(other.warnings);
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
    /// The policy generation the app writes are fenced on: the committed
    /// one, plus one for each policy this config writes before its apps.
    pub generation: u64,
}

/// One compiled policy.
struct CompiledPolicy {
    name: String,
    action: PolicyAction,
    message: Option<String>,
    match_program: Option<Program>,
    deny_program: Program,
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
                .map(|expression| check::compile_checked(PolicyField::Match, expression))
                .transpose()
                .map_err(broken(name))?;
            let deny_program =
                check::compile_checked(PolicyField::Deny, &spec.deny).map_err(broken(name))?;
            policies.push(CompiledPolicy {
                name: name.clone(),
                action: spec.action,
                message: spec.message.clone(),
                match_program,
                deny_program,
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
    let mut verdict = Verdict::default();
    if set.is_empty() {
        return verdict;
    }
    let mut context = Context::with_env(Arc::clone(&set.environment));
    context.add_variable_from_value("app", app_value(app_id, spec));
    context.add_variable_from_value(
        "caller",
        HashMap::from([
            ("name".to_string(), Value::from(caller.name.clone())),
            ("role".to_string(), Value::from(caller.role_name())),
        ]),
    );
    context.add_variable_from_value("source", source.as_str());

    for policy in &set.policies {
        let broken = match decide(policy, &context) {
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
        let finding = Finding {
            policy: policy.name.clone(),
            action: policy.action,
            app: app_id.clone(),
            message,
        };
        match policy.action {
            PolicyAction::Deny => verdict.denials.push(finding),
            PolicyAction::Warn => verdict.warnings.push(finding),
        }
    }
    verdict
}

/// Whether `policy` applies to the app in `context` and its rule is
/// broken, or why that couldn't be decided.
fn decide(policy: &CompiledPolicy, context: &Context) -> Result<bool, String> {
    if let Some(program) = &policy.match_program
        && !run(PolicyField::Match, program, context)?
    {
        return Ok(false);
    }
    run(PolicyField::Deny, &policy.deny_program, context)
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

/// The app as a policy sees it: its spec as JSON, with `name` and the
/// resolved `namespace` added.
fn app_value(app_id: &AppId, spec: &AppSpec) -> Value {
    let mut json = serde_json::to_value(spec).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut json {
        map.insert("name".into(), app_id.name.clone().into());
        map.insert("namespace".into(), app_id.namespace.clone().into());
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

/// Evaluate `set` against every app in `config`, in name order.
pub fn evaluate(set: &PolicySet, config: &Config, caller: &Caller, source: Source) -> Verdict {
    let mut verdict = Verdict::default();
    for (name, spec) in &config.app {
        verdict.extend(evaluate_app(
            set,
            &app_id_of(name, spec),
            spec,
            caller,
            source,
        ));
    }
    verdict
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
/// The config's apps are evaluated under [`effective_policies`]. A denial
/// returns no writes at all. Otherwise every app write is wrapped in
/// `RaftRequest::PolicyFenced`, stamped with the generation desired state
/// will have once this config's policy writes, which come first, have
/// applied: each policy write moves it by one.
pub fn admit(
    committed: &BTreeMap<String, PolicySpec>,
    generation: u64,
    config: &Config,
    caller: &Caller,
    source: Source,
    writes: Vec<RaftRequest>,
) -> Result<Admitted, AdmitError> {
    let mut warnings = Vec::new();
    if !config.app.is_empty() {
        let set = PolicySet::compile(&effective_policies(committed, config))?;
        let verdict = evaluate(&set, config, caller, source);
        if !verdict.is_allowed() {
            return Err(AdmitError::Denied(Denied {
                denials: verdict.denials,
                warnings: verdict.warnings,
            }));
        }
        warnings = verdict.warnings;
    }
    let generation = generation.saturating_add(config.policy.len() as u64);
    Ok(Admitted {
        writes: fence(writes, generation),
        warnings,
        generation,
    })
}

/// Wrap every app write in `writes` in a policy fence at `generation`.
/// Other writes pass through unchanged: only apps are judged by policy.
pub fn fence(writes: Vec<RaftRequest>, generation: u64) -> Vec<RaftRequest> {
    writes
        .into_iter()
        .map(|write| match write {
            RaftRequest::AppSpec { .. } | RaftRequest::TestLeaseAppSpec { .. } => {
                RaftRequest::PolicyFenced {
                    generation,
                    change: Box::new(write),
                }
            }
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests;
