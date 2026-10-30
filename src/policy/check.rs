//! The checks a policy must pass before it's committed.
//!
//! Every check here runs on the leader (and in `relish lint` and
//! `relish policy test`), never in the state machine: whether a policy
//! compiles depends on the `cel` crate's version, and the state machine
//! must give the same answer on every node for ever.

use std::collections::BTreeSet;

use cel::common::ast::{EntryExpr, Expr, IdedExpr};

use crate::config::{PolicySpec, PolicyTarget};

/// The longest `match` or `deny` expression a policy may hold, in bytes.
///
/// CEL has no loops of its own, but its macros (`all`, `exists`, `map`,
/// `filter`) iterate, and a macro inside another multiplies: evaluation
/// costs up to the expression's size times the input's size raised to the
/// nesting depth. The `cel` crate tracks no cost, so three caps bound it
/// instead: this one on the expression, [`MAX_COMPREHENSION_NESTING`] on the
/// exponent, and [`MAX_COLLECTION_ENTRIES`] on the input (#1431, chapter 18).
pub const MAX_EXPRESSION_BYTES: usize = 1024;

/// How deeply a policy may nest `all`, `exists`, `exists_one`, `map` and
/// `filter` (#1431). Two allows a rule that compares a list with itself
/// (no argument given twice); a third level would make the deployer's input size
/// cubed the cost.
pub const MAX_COMPREHENSION_NESTING: usize = 2;

/// The most entries any one list or map in a workload may have for a policy
/// to judge it (#1431). A workload with a bigger one isn't judged: every
/// policy that applies to it fails closed, naming the field, as a broken
/// expression does.
pub const MAX_COLLECTION_ENTRIES: usize = 1024;

/// The longest `message` a policy may hold, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 512;

/// The largest `default` table a policy may hold, in bytes of JSON.
pub const MAX_DEFAULT_BYTES: usize = 4096;

/// App fields a `default` can never fill, and why.
pub const UNFILLABLE_FIELDS: [(&str, &str); 13] = [
    ("name", "is the app's table name, not a field"),
    (
        "namespace",
        "decides which app it is, so a default can't move it",
    ),
    (
        "replicas",
        "always has a value (1 when the app leaves it out), so a default could never fill it",
    ),
    ("ordinals", "comes from the leader's placements"),
    // The apply path checks these against the caller's token (host-exec,
    // the runtime-knob gate) on the spec as declared. A default filled in
    // after that would hand every matching app what its caller may not
    // have, so a default can't set them.
    ("exec", GATED_BY_THE_CALLER),
    ("script", GATED_BY_THE_CALLER),
    ("volumes", GATED_BY_THE_CALLER),
    ("config_file", GATED_BY_THE_CALLER),
    ("init", GATED_BY_THE_CALLER),
    ("runtime", GATED_BY_THE_CALLER),
    // Only token X or an admin may declare `owner = X`, checked on the
    // declared spec before defaults fill it (#1125). A default that filled
    // it would hand an app to a controller its caller can't speak for.
    (
        "owner",
        "names the controller that manages the app, which only that controller's token or an admin may declare, so a default can't claim an app",
    ),
    (
        "sidecar",
        "would add a container to every matching app; declare sidecars on the app",
    ),
    (
        "provenance",
        "says which template made the app, and only template expansion writes it",
    ),
];

/// Every variable a policy expression can see. Nothing else: no other
/// workloads, no policies, permissions or namespaces, no clock. Which of
/// `app`, `job` and `workload` a policy sees depends on its `applies_to`
/// ([`workload_variables`]).
pub const POLICY_VARIABLES: [&str; 5] = ["app", "job", "workload", "caller", "source"];

/// The workload variables a policy that applies to `target` sees: `app`
/// for apps, `job` for jobs, and `workload`, the fields both share, for
/// either (#1119).
pub fn workload_variables(target: PolicyTarget) -> &'static [&'static str] {
    match target {
        PolicyTarget::Apps => &["app", "workload"],
        PolicyTarget::Jobs => &["job", "workload"],
        PolicyTarget::Both => &["workload"],
    }
}

/// The fields of `job`: every `[job.*]` key, plus the job's `name`
/// (#1119). `namespace` is always present, as for `app`. A test checks
/// this list against `JobSpec` itself.
pub const JOB_FIELDS: [&str; 20] = [
    "name",
    "runtime",
    "image",
    "command",
    "schedule",
    "run_before",
    "memory",
    "cpu",
    "pids",
    "env",
    "namespace",
    "exec",
    "script",
    "firewall",
    "egress",
    "max_attempts",
    "task_timeout_secs",
    "overlap",
    "replay_unknown",
    "provenance",
];

/// The fields of `workload`: the ones apps and jobs share (#1119). A test
/// checks this is exactly where [`APP_FIELDS`] and [`JOB_FIELDS`] meet,
/// less `runtime`, which means an app's kernel knobs but a job's execution
/// state.
pub const WORKLOAD_FIELDS: [&str; 12] = [
    "name",
    "image",
    "command",
    "exec",
    "script",
    "memory",
    "cpu",
    "env",
    "firewall",
    "egress",
    "namespace",
    "provenance",
];
/// Why a default can't fill a field the apply path gates on the caller.
const GATED_BY_THE_CALLER: &str = "can need host access or a runtime knob, which the caller's token must allow, so a default can't grant it";

/// The fields of `app`: every `[app.*]` key, plus the app's `name`.
///
/// `namespace` is always present, resolved to `default` when the spec
/// doesn't name one. A test checks this list against `AppSpec` itself.
pub const APP_FIELDS: [&str; 36] = [
    "name",
    "image",
    "command",
    "args",
    "working_dir",
    "run_as_user",
    "run_as_group",
    "exec",
    "script",
    "replicas",
    "ordinals",
    "port",
    "health",
    "memory",
    "cpu",
    "gpu",
    "env",
    "config_file",
    "volumes",
    "init",
    "sidecar",
    "ingress",
    "placement",
    "deploy",
    "firewall",
    "egress",
    "autoscale",
    "metrics",
    "stop_signal",
    "stop_grace",
    "pre_stop",
    "pre_stop_timeout",
    "runtime",
    "namespace",
    "provenance",
    "owner",
];

/// The fields of `caller`.
pub const CALLER_FIELDS: [&str; 2] = ["name", "role"];

/// CEL's type names, which parse as identifiers (`type(x) == string`).
const TYPE_NAMES: [&str; 10] = [
    "bool",
    "int",
    "uint",
    "double",
    "string",
    "bytes",
    "list",
    "map",
    "null_type",
    "type",
];

/// Which expression of a policy a problem is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyField {
    /// The `match` expression.
    Match,
    /// The `deny` expression.
    Deny,
    /// The `default` table.
    Default,
}

impl std::fmt::Display for PolicyField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PolicyField::Match => "match",
            PolicyField::Deny => "deny",
            PolicyField::Default => "default",
        })
    }
}

/// Why a policy can't be committed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The policy's name isn't a DNS label.
    #[error(
        "name {name:?} must be a lowercase DNS label of 1–63 bytes, with letters or digits at both ends and only letters, digits or hyphens within"
    )]
    InvalidName {
        /// The name as written.
        name: String,
    },
    /// An expression is empty or only whitespace.
    #[error("{field} is empty")]
    Empty {
        /// The empty expression.
        field: PolicyField,
    },
    /// An expression is longer than [`MAX_EXPRESSION_BYTES`].
    #[error("{field} is {bytes} bytes, over the {limit}-byte limit")]
    TooLong {
        /// The long expression.
        field: PolicyField,
        /// Its length.
        bytes: usize,
        /// The cap.
        limit: usize,
    },
    /// The message is longer than [`MAX_MESSAGE_BYTES`].
    #[error("message is {bytes} bytes, over the {limit}-byte limit")]
    MessageTooLong {
        /// Its length.
        bytes: usize,
        /// The cap.
        limit: usize,
    },
    /// An expression isn't valid CEL.
    #[error("{field} isn't valid CEL: {error}")]
    Unparseable {
        /// The broken expression.
        field: PolicyField,
        /// The parser's complaint.
        error: String,
    },
    /// An expression names a variable a policy can't see, such as
    /// `policy`, `permission` or `namespace`, or the workload variable of
    /// a kind the policy doesn't apply to (`job` in an apps policy).
    #[error(
        "{field} uses {variable:?}, but a policy that applies to {target} sees only {visible}, caller and source; policies never match policies, permissions or namespaces",
        target = target.as_str(),
        visible = workload_variables(*target).join(", ")
    )]
    UnknownVariable {
        /// The expression.
        field: PolicyField,
        /// The variable it names.
        variable: String,
        /// What the policy applies to.
        target: PolicyTarget,
    },
    /// A `default` on a policy that doesn't apply to apps only (#1119).
    #[error("a default fills app fields, so its policy must apply to apps only, not {}", .target.as_str())]
    DefaultOutsideApps {
        /// What the policy applies to.
        target: PolicyTarget,
    },
    /// The policy has neither a `deny` nor a `default`, so it does nothing.
    #[error("a policy needs a deny expression, a default table, or both")]
    NothingToDo,
    /// A `default` names an app field that doesn't exist, or one a
    /// default can't fill (#1118).
    #[error("default sets {name}, which {reason}")]
    DefaultField {
        /// The field it names.
        name: String,
        /// Why it can't be filled.
        reason: String,
    },
    /// A `default` value doesn't fit its field: the wrong type, or a
    /// nested key the field doesn't have (#1118).
    #[error("default doesn't fit an app: {error}")]
    InvalidDefault {
        /// What serde said, naming the field.
        error: String,
    },
    /// An expression nests macros deeper than [`MAX_COMPREHENSION_NESTING`].
    #[error(
        "{field} nests all/exists/map/filter {depth} deep, over the limit of {limit}: each level multiplies the cost by the size of the input"
    )]
    TooDeep {
        /// The expression.
        field: PolicyField,
        /// How deep it nests.
        depth: usize,
        /// The cap.
        limit: usize,
    },
    /// An expression reads a field `app` or `caller` doesn't have.
    #[error("{field} reads {variable}.{name}, which doesn't exist")]
    UnknownField {
        /// The expression.
        field: PolicyField,
        /// `app` or `caller`.
        variable: String,
        /// The field it reads.
        name: String,
    },
}

/// Check that `spec` can be committed under `name`: a valid name, short
/// expressions and message, valid CEL, only the variables and fields a
/// policy can see, and a `default` whose every field exists and has the
/// right type.
pub fn check_policy(name: &str, spec: &PolicySpec) -> Result<(), PolicyError> {
    if !crate::config::valid_workload_label(name) {
        return Err(PolicyError::InvalidName {
            name: name.to_string(),
        });
    }
    if let Some(message) = &spec.message
        && message.len() > MAX_MESSAGE_BYTES
    {
        return Err(PolicyError::MessageTooLong {
            bytes: message.len(),
            limit: MAX_MESSAGE_BYTES,
        });
    }
    if let Some(expression) = &spec.match_expression {
        compile_checked(PolicyField::Match, spec.applies_to, expression)?;
    }
    if let Some(deny) = &spec.deny {
        compile_checked(PolicyField::Deny, spec.applies_to, deny)?;
    }
    match &spec.default {
        Some(_) if !spec.applies_to.is_apps() => {
            return Err(PolicyError::DefaultOutsideApps {
                target: spec.applies_to,
            });
        }
        Some(default) => check_default(default)?,
        None if spec.deny.is_none() => return Err(PolicyError::NothingToDo),
        None => {}
    }
    Ok(())
}

/// Check a `default` table: not empty or over the cap, only fields an app
/// has and a default can fill, and values that fit them. The values are
/// filled into an app that sets nothing and read back through `AppSpec`,
/// so serde's own types decide what fits.
fn check_default(default: &serde_json::Map<String, serde_json::Value>) -> Result<(), PolicyError> {
    if default.is_empty() {
        return Err(PolicyError::Empty {
            field: PolicyField::Default,
        });
    }
    let bytes = serde_json::to_string(default).map_or(usize::MAX, |json| json.len());
    if bytes > MAX_DEFAULT_BYTES {
        return Err(PolicyError::TooLong {
            field: PolicyField::Default,
            bytes,
            limit: MAX_DEFAULT_BYTES,
        });
    }
    for name in default.keys() {
        if let Some((_, reason)) = UNFILLABLE_FIELDS.iter().find(|(field, _)| field == name) {
            return Err(PolicyError::DefaultField {
                name: name.clone(),
                reason: (*reason).to_string(),
            });
        }
        if !APP_FIELDS.contains(&name.as_str()) {
            return Err(PolicyError::DefaultField {
                name: name.clone(),
                reason: "isn't an app field".to_string(),
            });
        }
    }
    let invalid = |error: String| PolicyError::InvalidDefault { error };
    let bare: crate::config::AppSpec =
        serde_json::from_value(serde_json::Value::Object(serde_json::Map::new()))
            .map_err(|error| invalid(error.to_string()))?;
    let (filled, _) = super::defaults::apply_default(&bare, default).map_err(invalid)?;
    let kept = match serde_json::to_value(&filled) {
        Ok(serde_json::Value::Object(kept)) => kept,
        _ => return Err(invalid("the filled app didn't serialise".to_string())),
    };
    if let Some(path) = super::defaults::first_dropped(&kept, default, "") {
        return Err(invalid(format!("{path} isn't a field an app keeps")));
    }
    Ok(())
}

/// Compile one expression after checking its size, then check the names it
/// uses. Returns the program, so evaluation compiles each expression once.
pub(super) fn compile_checked(
    field: PolicyField,
    target: PolicyTarget,
    expression: &str,
) -> Result<cel::Program, PolicyError> {
    if expression.trim().is_empty() {
        return Err(PolicyError::Empty { field });
    }
    if expression.len() > MAX_EXPRESSION_BYTES {
        return Err(PolicyError::TooLong {
            field,
            bytes: expression.len(),
            limit: MAX_EXPRESSION_BYTES,
        });
    }
    let program =
        super::environment()
            .compile(expression)
            .map_err(|error| PolicyError::Unparseable {
                field,
                error: error.to_string(),
            })?;
    let mut walk = Walk {
        field,
        target,
        bound: Vec::new(),
        nesting: 0,
    };
    walk.check_names(program.expression())?;
    Ok(program)
}

/// The state of a walk over one expression: which expression it is, the
/// variables the enclosing comprehensions (`all`, `exists`, `map`,
/// `filter`) introduce, and how many of them enclose the current node.
struct Walk {
    field: PolicyField,
    target: PolicyTarget,
    bound: Vec<String>,
    nesting: usize,
}

impl Walk {
    /// Walk the expression, refusing a free variable a policy that applies
    /// to `target` can't see, a field `app`, `job`, `workload` or `caller`
    /// doesn't have, and macros nested deeper than
    /// [`MAX_COMPREHENSION_NESTING`].
    fn check_names(&mut self, expression: &IdedExpr) -> Result<(), PolicyError> {
        let field = self.field;
        match &expression.expr {
            Expr::Unspecified | Expr::Literal(_) => Ok(()),
            Expr::Ident(name) => check_variable(field, self.target, name, &self.bound),
            Expr::Select(select) => {
                // The variable first: `job.image` in an apps policy is the
                // wrong variable, not a missing field.
                self.check_names(&select.operand)?;
                if let Expr::Ident(variable) = &select.operand.expr
                    && !self.bound.contains(variable)
                {
                    check_field(field, variable, &select.field)?;
                }
                Ok(())
            }
            Expr::Call(call) => {
                if let Some(call_target) = &call.target {
                    self.check_names(call_target)?;
                }
                call.args
                    .iter()
                    .try_for_each(|argument| self.check_names(argument))
            }
            Expr::List(list) => list
                .elements
                .iter()
                .try_for_each(|element| self.check_names(element)),
            Expr::Map(map) => map
                .entries
                .iter()
                .try_for_each(|entry| self.check_entry(&entry.expr)),
            Expr::Struct(structure) => structure
                .entries
                .iter()
                .try_for_each(|entry| self.check_entry(&entry.expr)),
            Expr::Comprehension(comprehension) => {
                // The range and the accumulator's start are outside the
                // loop; everything else sees the loop's own variables, and
                // runs once per element.
                self.check_names(&comprehension.iter_range)?;
                self.check_names(&comprehension.accu_init)?;
                if self.nesting + 1 > MAX_COMPREHENSION_NESTING {
                    return Err(PolicyError::TooDeep {
                        field,
                        depth: self.nesting + 1,
                        limit: MAX_COMPREHENSION_NESTING,
                    });
                }
                let depth = self.bound.len();
                self.bound.push(comprehension.iter_var.clone());
                self.bound.extend(comprehension.iter_var2.iter().cloned());
                self.bound.push(comprehension.accu_var.clone());
                self.nesting += 1;
                let inner = [
                    &comprehension.loop_cond,
                    &comprehension.loop_step,
                    &comprehension.result,
                ]
                .into_iter()
                .try_for_each(|part| self.check_names(part));
                self.nesting -= 1;
                self.bound.truncate(depth);
                inner
            }
        }
    }

    fn check_entry(&mut self, entry: &EntryExpr) -> Result<(), PolicyError> {
        match entry {
            EntryExpr::StructField(structure) => self.check_names(&structure.value),
            EntryExpr::MapEntry(map) => {
                self.check_names(&map.key)?;
                self.check_names(&map.value)
            }
        }
    }
}

fn check_variable(
    field: PolicyField,
    target: PolicyTarget,
    name: &str,
    bound: &[String],
) -> Result<(), PolicyError> {
    if bound.iter().any(|variable| variable == name)
        || CALLER_VARIABLES.contains(&name)
        || workload_variables(target).contains(&name)
        || TYPE_NAMES.contains(&name)
    {
        return Ok(());
    }
    Err(PolicyError::UnknownVariable {
        field,
        variable: name.to_string(),
        target,
    })
}

/// The variables every policy sees, whatever it applies to.
const CALLER_VARIABLES: [&str; 2] = ["caller", "source"];

fn check_field(field: PolicyField, variable: &str, name: &str) -> Result<(), PolicyError> {
    let known: &[&str] = match variable {
        "app" => &APP_FIELDS,
        "job" => &JOB_FIELDS,
        "workload" => &WORKLOAD_FIELDS,
        "caller" => &CALLER_FIELDS,
        _ => return Ok(()),
    };
    if known.contains(&name) {
        return Ok(());
    }
    Err(PolicyError::UnknownField {
        field,
        variable: variable.to_string(),
        name: name.to_string(),
    })
}

/// The `app` fields as a set, for tests and the manual.
pub fn app_fields() -> BTreeSet<&'static str> {
    APP_FIELDS.into_iter().collect()
}
